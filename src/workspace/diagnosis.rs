use super::{
    CapDir, ContestMetadataHealth, ContestRecoveryError, MAX_CONTEST_METADATA_BYTES,
    SampleLayoutError, UnownedTestEntry, contest_directory_exists, find_unowned_test_entry,
    inspect_contest_metadata_text, inspect_contest_recovery_state, metadata_contest_owner,
    open_contest_destination_nofollow, open_managed_directory, problem_indices,
    validate_path_component, visit_problem_sample_pairs,
};
use crate::model::Contest;
use crate::safe_file;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ContestDiagnosis {
    Healthy {
        contest: Contest,
        samples: SampleVerification,
    },
    RepairRequired(RepairReason),
    MigrationRequired(MigrationReason),
    Blocked(BlockReason),
    MissingDirectory,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SampleVerification {
    /// File names, pairs, counts and UTF-8 were checked against the local manifest.
    /// The sample text was not compared with AtCoder.
    StructureOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataIssue {
    Missing,
    Invalid,
    Repairable,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RepairReason {
    Metadata(MetadataIssue),
    TestsDirectoryMissing,
    ProblemTestsDirectoryMissing {
        index: String,
    },
    SampleLayout {
        index: String,
        issue: SampleLayoutError,
    },
    SampleCountMismatch {
        index: String,
        expected: usize,
        found: usize,
    },
    InvalidSamples {
        index: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MigrationReason {
    /// The schema needs separate handling before the contest can be refreshed.
    UnsupportedVersion(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlockReason {
    InvalidContestId,
    ContestIdMismatch { actual: String },
    MetadataOwnershipUnknown,
    UnsafeMetadata,
    TestsOwnershipUnknown(MetadataIssue),
    UnownedTestsEntry { path: PathBuf },
    UnsafeEntry { path: PathBuf },
    RecoveryRequired { path: PathBuf },
    Filesystem { path: PathBuf, kind: io::ErrorKind },
}

impl BlockReason {
    pub(crate) fn error_details(&self) -> (io::ErrorKind, String) {
        match self {
            Self::InvalidContestId => (io::ErrorKind::InvalidInput, "invalid contest ID".into()),
            Self::ContestIdMismatch { actual } => (
                io::ErrorKind::InvalidData,
                format!("contest ID mismatch: metadata contains {actual:?}"),
            ),
            Self::MetadataOwnershipUnknown => (
                io::ErrorKind::InvalidData,
                "cannot confirm ownership of existing contest metadata".into(),
            ),
            Self::UnsafeMetadata => (io::ErrorKind::InvalidData, "unsafe contest metadata".into()),
            Self::TestsOwnershipUnknown(_) => (
                io::ErrorKind::InvalidData,
                "cannot confirm ownership of existing tests".into(),
            ),
            Self::UnownedTestsEntry { path } => (
                io::ErrorKind::InvalidData,
                format!("unowned entry in tests: {}", path.display()),
            ),
            Self::UnsafeEntry { path } => (
                io::ErrorKind::InvalidInput,
                format!("unsafe filesystem entry: {}", path.display()),
            ),
            Self::RecoveryRequired { path } => (
                io::ErrorKind::InvalidData,
                format!("contest update requires recovery check: {}", path.display()),
            ),
            Self::Filesystem { path, kind } => {
                (*kind, format!("cannot inspect {}: {kind}", path.display()))
            }
        }
    }
}

/// Inspect an already-resolved contest destination. Enclosing workspace routing is the
/// caller's responsibility. This read-only scan does not lock or authorize a later write.
pub(crate) fn diagnose_contest(destination: &Path, contest_id: &str) -> ContestDiagnosis {
    inspect(destination, contest_id, true).unwrap_or_else(ContestDiagnosis::Blocked)
}

pub(crate) fn diagnose_watch_contest(destination: &Path, contest_id: &str) -> ContestDiagnosis {
    inspect(destination, contest_id, false).unwrap_or_else(ContestDiagnosis::Blocked)
}

fn inspect(
    destination: &Path,
    contest_id: &str,
    require_owned_tests: bool,
) -> Result<ContestDiagnosis, BlockReason> {
    use ContestDiagnosis::{Healthy, MigrationRequired, RepairRequired};

    validate_path_component(contest_id, "contest ID").map_err(|_| BlockReason::InvalidContestId)?;
    // Strip trailing separators and redundant trailing dots for the no-follow open. A
    // separator after a symlink can otherwise make the OS resolve it as an ancestor.
    let open_path = destination.components().as_path();
    if !contest_directory_exists(open_path).map_err(|error| entry_error(destination, error))? {
        return Ok(ContestDiagnosis::MissingDirectory);
    }
    let directory = open_contest_destination_nofollow(open_path)
        .map_err(|error| entry_error(destination, error))?;
    inspect_contest_recovery_state(destination, &directory).map_err(|error| match error {
        ContestRecoveryError::Area { path, .. } => BlockReason::RecoveryRequired { path },
        ContestRecoveryError::Scan(source) => entry_error(destination, source),
    })?;

    let tests_path = destination.join("tests");
    let open_tests = || open_directory(&directory, "tests", &tests_path, "tests directory");
    let (contest, metadata_issue) = match inspect_metadata(destination, &directory, contest_id)? {
        ContestMetadataHealth::Healthy(contest) => (contest, None),
        ContestMetadataHealth::Repairable(manifest) => {
            (manifest.contest, Some(MetadataIssue::Repairable))
        }
        health @ (ContestMetadataHealth::Missing | ContestMetadataHealth::Invalid) => {
            let issue = if matches!(health, ContestMetadataHealth::Missing) {
                MetadataIssue::Missing
            } else {
                MetadataIssue::Invalid
            };
            if open_tests()?.is_some() {
                return Err(BlockReason::TestsOwnershipUnknown(issue));
            }
            return Ok(RepairRequired(RepairReason::Metadata(issue)));
        }
        ContestMetadataHealth::UnsupportedVersion(version) => {
            return Ok(MigrationRequired(MigrationReason::UnsupportedVersion(
                version,
            )));
        }
    };

    let tests = open_tests()?;
    if require_owned_tests && let Some(tests) = &tests {
        let unowned = find_unowned_test_entry(&tests_path, tests, &problem_indices(&contest))
            .map_err(|error| entry_error(&tests_path, error))?;
        match unowned {
            Some(UnownedTestEntry::Unsafe(path)) => return Err(BlockReason::UnsafeEntry { path }),
            Some(UnownedTestEntry::Unrecognized(path)) => {
                return Err(BlockReason::UnownedTestsEntry { path });
            }
            None => {}
        }
        if let Some(issue) = metadata_issue
            && tests
                .entries()
                .and_then(|mut entries| entries.next().transpose())
                .map_err(|error| entry_error(&tests_path, error))?
                .is_some()
        {
            // A repairable manifest requires corroboration with the official task list.
            return Err(BlockReason::TestsOwnershipUnknown(issue));
        }
    }

    if let Some(issue) = metadata_issue {
        return Ok(RepairRequired(RepairReason::Metadata(issue)));
    }
    if let Some(reason) = inspect_samples(&tests_path, tests.as_ref(), &contest)? {
        return Ok(RepairRequired(reason));
    }
    Ok(Healthy {
        contest,
        samples: SampleVerification::StructureOnly,
    })
}

fn inspect_metadata(
    destination: &Path,
    directory: &CapDir,
    contest_id: &str,
) -> Result<ContestMetadataHealth, BlockReason> {
    let marker_path = destination.join(".atc");
    let Some(marker) = open_directory(directory, ".atc", &marker_path, "workspace marker")? else {
        return Ok(ContestMetadataHealth::Missing);
    };
    let name = OsStr::new("contest.toml");
    let path = marker_path.join(name);
    if !safe_file::regular_file_exists_at(&marker, name, &path)
        .map_err(|error| entry_error(&path, error))?
    {
        return Ok(ContestMetadataHealth::Missing);
    }
    let content =
        safe_file::read_bounded_regular_text_at(&marker, name, &path, MAX_CONTEST_METADATA_BYTES)
            .map_err(|error| match error.kind() {
            io::ErrorKind::InvalidData => BlockReason::MetadataOwnershipUnknown,
            _ => entry_error(&path, error),
        })?;
    let health =
        inspect_contest_metadata_text(&content).map_err(|_| BlockReason::UnsafeMetadata)?;
    let actual = match &health {
        ContestMetadataHealth::Healthy(contest) => contest.contest_id.clone(),
        ContestMetadataHealth::Repairable(manifest) => manifest.contest.contest_id.clone(),
        _ => metadata_contest_owner(&content).ok_or(BlockReason::MetadataOwnershipUnknown)?,
    };
    if contest_id != actual {
        return Err(BlockReason::ContestIdMismatch { actual });
    }
    Ok(health)
}

fn inspect_samples(
    tests_path: &Path,
    tests: Option<&CapDir>,
    contest: &Contest,
) -> Result<Option<RepairReason>, BlockReason> {
    let Some(tests) = tests else {
        return Ok(contest
            .problems
            .iter()
            .any(|problem| problem.sample_count != 0)
            .then_some(RepairReason::TestsDirectoryMissing));
    };
    for problem in &contest.problems {
        let index = &problem.index;
        let path = tests_path.join(index);
        let Some(directory) = open_directory(tests, index, &path, "problem tests directory")?
        else {
            if problem.sample_count != 0 {
                return Ok(Some(RepairReason::ProblemTestsDirectoryMissing {
                    index: index.clone(),
                }));
            }
            continue;
        };
        let count = visit_problem_sample_pairs(&path, &directory, |input, output| {
            safe_file::validate_regular_utf8_at(&directory, input, &path.join(input))?;
            safe_file::validate_regular_utf8_at(&directory, output, &path.join(output))
        });
        match count {
            Ok(found) if found == problem.sample_count => {}
            Ok(found) => {
                return Ok(Some(RepairReason::SampleCountMismatch {
                    index: index.clone(),
                    expected: problem.sample_count,
                    found,
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let reason = match error
                    .get_ref()
                    .and_then(|cause| cause.downcast_ref::<SampleLayoutError>())
                {
                    Some(issue) => RepairReason::SampleLayout {
                        index: index.clone(),
                        issue: *issue,
                    },
                    None => RepairReason::InvalidSamples {
                        index: index.clone(),
                    },
                };
                return Ok(Some(reason));
            }
            Err(error) => return Err(entry_error(&path, error)),
        }
    }
    Ok(None)
}

fn open_directory(
    parent: &CapDir,
    name: &str,
    path: &Path,
    kind: &str,
) -> Result<Option<CapDir>, BlockReason> {
    open_managed_directory(parent, name, path, kind).map_err(|error| entry_error(path, error))
}

fn entry_error(path: &Path, error: io::Error) -> BlockReason {
    if error.kind() == io::ErrorKind::InvalidInput {
        BlockReason::UnsafeEntry {
            path: path.to_path_buf(),
        }
    } else {
        BlockReason::Filesystem {
            path: path.to_path_buf(),
            kind: error.kind(),
        }
    }
}
