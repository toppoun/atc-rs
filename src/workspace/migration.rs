use super::diagnosis::{
    BlockReason, ContestDiagnosis, MetadataIssue, MigrationReason, RepairReason, diagnose_contest,
};
use super::{
    CapDir, MAX_CONTEST_METADATA_BYTES, PhysicalDirectoryIdentity, contest_staging_archive,
    entry_exists, lock_contest_update, metadata_contest_owner, missing_contest_directory,
    open_contest_destination_nofollow, open_managed_directory, opened_directory_identity,
    preserved_staging_error, read_contest_metadata_snapshot_in,
};
use crate::safe_file::{self, FileSnapshot, PreservedTempDir, TreeSnapshot};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) enum MigrationInspection {
    Healthy,
    Ready(Box<MigrationPlan>),
}

enum MigrationCause {
    Metadata(MetadataIssue),
    UnsupportedVersion(u32),
    UnknownTestsOwner(MetadataIssue),
    UnmanagedTests,
}

impl MigrationCause {
    fn description(&self) -> String {
        let metadata = |issue| match issue {
            MetadataIssue::Missing => "contest metadata is missing",
            MetadataIssue::Invalid => "contest metadata is invalid",
            MetadataIssue::Repairable => "contest metadata needs repair",
        };
        match self {
            Self::Metadata(issue) => metadata(*issue).into(),
            Self::UnsupportedVersion(version) => {
                format!("contest metadata version {version} is unsupported")
            }
            Self::UnknownTestsOwner(issue) => {
                format!(
                    "{} and existing tests have unconfirmed ownership",
                    metadata(*issue)
                )
            }
            Self::UnmanagedTests => "tests contain entries outside atc management".into(),
        }
    }
}

enum MigrationRefusal {
    MissingDirectory,
    Diagnosis(BlockReason),
    SamplesOnly,
}

impl MigrationRefusal {
    fn into_error(self, destination: &Path, contest_id: &str) -> io::Error {
        let (kind, message) = match self {
            Self::MissingDirectory => return missing_contest_directory(destination),
            Self::Diagnosis(reason) => reason.error_details(),
            Self::SamplesOnly => (
                io::ErrorKind::InvalidData,
                "contest metadata is current and only samples need repair; run `atc refresh`"
                    .into(),
            ),
        };
        io::Error::new(
            kind,
            format!(
                "migration refused for {contest_id} at {}: {message}",
                destination.display()
            ),
        )
    }
}

fn migration_cause(
    diagnosis: &ContestDiagnosis,
) -> Result<Option<MigrationCause>, MigrationRefusal> {
    let cause = match diagnosis {
        ContestDiagnosis::Healthy { .. } => return Ok(None),
        ContestDiagnosis::MissingDirectory => return Err(MigrationRefusal::MissingDirectory),
        ContestDiagnosis::MigrationRequired(MigrationReason::UnsupportedVersion(version)) => {
            MigrationCause::UnsupportedVersion(*version)
        }
        ContestDiagnosis::RepairRequired(RepairReason::Metadata(issue)) => {
            MigrationCause::Metadata(*issue)
        }
        ContestDiagnosis::RepairRequired(_) => return Err(MigrationRefusal::SamplesOnly),
        ContestDiagnosis::Blocked(BlockReason::UnsafeMetadata) => {
            // The old problem definitions will not be used. The top-level owner
            // still has to match in the independently parsed metadata snapshot.
            MigrationCause::Metadata(MetadataIssue::Invalid)
        }
        ContestDiagnosis::Blocked(BlockReason::TestsOwnershipUnknown(issue)) => {
            MigrationCause::UnknownTestsOwner(*issue)
        }
        ContestDiagnosis::Blocked(BlockReason::UnownedTestsEntry { .. }) => {
            MigrationCause::UnmanagedTests
        }
        ContestDiagnosis::Blocked(reason) => {
            return Err(MigrationRefusal::Diagnosis(reason.clone()));
        }
    };
    Ok(Some(cause))
}

struct LiveDirectories {
    root: CapDir,
    marker: Option<CapDir>,
}

#[derive(PartialEq, Eq)]
struct DirectoryIdentities {
    root: PhysicalDirectoryIdentity,
    marker: Option<PhysicalDirectoryIdentity>,
}

impl LiveDirectories {
    fn open(destination: &Path) -> io::Result<Self> {
        let root = open_contest_destination_nofollow(destination.components().as_path())?;
        let marker =
            open_managed_directory(&root, ".atc", &destination.join(".atc"), "workspace marker")?;
        Ok(Self { root, marker })
    }

    fn identities(&self, destination: &Path) -> io::Result<DirectoryIdentities> {
        Ok(DirectoryIdentities {
            root: opened_directory_identity(&self.root, destination)?,
            marker: self
                .marker
                .as_ref()
                .map(|marker| opened_directory_identity(marker, &destination.join(".atc")))
                .transpose()?,
        })
    }

    fn metadata(&self, destination: &Path) -> io::Result<Option<FileSnapshot>> {
        self.marker.as_ref().map_or(Ok(None), |marker| {
            read_contest_metadata_snapshot_in(marker, &destination.join(".atc"))
        })
    }
}

pub(crate) struct MigrationPlan {
    destination: PathBuf,
    contest_id: String,
    cause: MigrationCause,
    diagnosis: ContestDiagnosis,
    directories: DirectoryIdentities,
    metadata: Option<FileSnapshot>,
    tests: Option<TreeSnapshot>,
}

pub(crate) fn inspect(destination: PathBuf, contest_id: String) -> io::Result<MigrationInspection> {
    let diagnosis = diagnose_contest(&destination, &contest_id);
    let Some(cause) = migration_cause(&diagnosis)
        .map_err(|reason| reason.into_error(&destination, &contest_id))?
    else {
        return Ok(MigrationInspection::Healthy);
    };

    let directories = LiveDirectories::open(&destination)?;
    let identities = directories.identities(&destination)?;
    let metadata = directories.metadata(&destination)?;
    if let Some(metadata) = &metadata {
        let owner = std::str::from_utf8(&metadata.bytes)
            .ok()
            .and_then(metadata_contest_owner);
        let reason = match owner {
            Some(actual) if actual == contest_id => None,
            Some(actual) => Some(BlockReason::ContestIdMismatch { actual }),
            None => Some(BlockReason::MetadataOwnershipUnknown),
        };
        if let Some(reason) = reason {
            return Err(MigrationRefusal::Diagnosis(reason).into_error(&destination, &contest_id));
        }
    }

    safe_file::regular_file_exists_at(
        &directories.root,
        OsStr::new(".atc-update.lock"),
        &destination.join(".atc-update.lock"),
    )?;
    let tests = snapshot_tests(&directories.root, &destination, "tests")?;
    let current = LiveDirectories::open(&destination)?;
    if diagnose_contest(&destination, &contest_id) != diagnosis
        || current.identities(&destination)? != identities
        || current.metadata(&destination)? != metadata
    {
        return Err(changed_local_data());
    }

    Ok(MigrationInspection::Ready(Box::new(MigrationPlan {
        destination,
        contest_id,
        cause,
        diagnosis,
        directories: identities,
        metadata,
        tests,
    })))
}

fn snapshot_tests(
    root: &CapDir,
    destination: &Path,
    name: &str,
) -> io::Result<Option<TreeSnapshot>> {
    let path = destination.join(name);
    let snapshot = open_managed_directory(root, name, &path, "tests directory")?
        .map(|tests| safe_file::read_tree_snapshot_in(&tests, &path))
        .transpose();
    snapshot.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot safely back up {}: {error}", path.display()),
        )
    })
}

fn changed_local_data() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "local contest data changed during migration; inspect it before retrying",
    )
}

impl MigrationPlan {
    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) fn contest_id(&self) -> &str {
        &self.contest_id
    }

    pub(crate) fn write_summary(&self, output: &mut dyn io::Write) -> io::Result<()> {
        writeln!(
            output,
            "Migrate {} at {}",
            self.contest_id,
            self.destination.display()
        )?;
        writeln!(output, "Reason: {}", self.cause.description())?;
        writeln!(
            output,
            "Install official samples and .atc/contest.toml. Preserve source files and other .atc data."
        )?;
        writeln!(output, "Recovery directory: .atc-recovery-migrate-*")?;
        if self.tests.is_some() {
            writeln!(output, "  tests/ -> previous-tests/ (all existing entries)")?;
        }
        if self.metadata.is_some() {
            writeln!(output, "  .atc/contest.toml -> previous-contest.toml")?;
        }
        Ok(())
    }

    fn open_unchanged(&self) -> io::Result<LiveDirectories> {
        let live = LiveDirectories::open(&self.destination)?;
        if live.identities(&self.destination)? != self.directories {
            return Err(changed_local_data());
        }
        Ok(live)
    }

    pub(crate) fn revalidate(&self) -> io::Result<()> {
        let live = self.open_unchanged()?;
        if diagnose_contest(&self.destination, &self.contest_id) != self.diagnosis
            || live.metadata(&self.destination)? != self.metadata
            || snapshot_tests(&live.root, &self.destination, "tests")? != self.tests
        {
            return Err(changed_local_data());
        }
        self.open_unchanged()?;
        Ok(())
    }

    pub(crate) fn revalidate_directories(&self) -> io::Result<()> {
        self.open_unchanged().map(|_| ())
    }

    pub(crate) fn apply(self, staging: PreservedTempDir) -> io::Result<PathBuf> {
        self.apply_with_hooks(staging, |_| {}, |_| {})
    }

    fn apply_with_hooks(
        self,
        staging: PreservedTempDir,
        before_metadata_backup: impl FnOnce(&Path),
        before_metadata_install: impl FnOnce(&Path),
    ) -> io::Result<PathBuf> {
        let destination = &self.destination;
        let staging_root = staging.path().to_path_buf();
        let archive = contest_staging_archive(&staging_root, ".atc-recovery-")?;
        let identity = safe_file::RecoveryIdentity::new("contest", &self.contest_id, None);
        let _lock = lock_contest_update(destination, false)
            .map_err(|error| preserved_staging_error(&staging_root, error))?;
        let live = self
            .open_unchanged()
            .map_err(|error| preserved_staging_error(&staging_root, error))?;
        let staged_root = self
            .install(
                &live,
                &staging_root,
                &identity,
                before_metadata_backup,
                before_metadata_install,
            )
            .map_err(|error| preserved_staging_error(&staging_root, error))?;

        let backup = safe_file::complete_recovery(staging, &archive, &identity)?;
        let backup_root = open_contest_destination_nofollow(&backup)
            .and_then(|directory| opened_directory_identity(&directory, &backup));
        if !matches!(backup_root, Ok(actual) if actual == staged_root)
            || !matches!(
                diagnose_contest(destination, &self.contest_id),
                ContestDiagnosis::Healthy { .. }
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "migration installed new data but local verification failed; backup kept at {}",
                    backup.display()
                ),
            ));
        }
        Ok(backup)
    }

    fn install(
        &self,
        live: &LiveDirectories,
        staging_root: &Path,
        identity: &safe_file::RecoveryIdentity,
        before_metadata_backup: impl FnOnce(&Path),
        before_metadata_install: impl FnOnce(&Path),
    ) -> io::Result<PhysicalDirectoryIdentity> {
        let destination = &self.destination;
        let mut staged = LiveDirectories::open(staging_root)?;
        let staged_identities = staged.identities(staging_root)?;
        if !matches!(
            diagnose_contest(staging_root, &self.contest_id),
            ContestDiagnosis::Healthy { .. }
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged migration data failed local verification",
            ));
        }
        let staged_metadata = staged
            .metadata(staging_root)?
            .ok_or_else(changed_local_data)?;
        let staged_tests = snapshot_tests(&staged.root, staging_root, "tests")?;
        let verify_staging = || {
            if LiveDirectories::open(staging_root)?.identities(staging_root)? != staged_identities {
                return Err(changed_local_data());
            }
            Ok(())
        };

        let destination_tests = destination.join("tests");
        let destination_metadata = destination.join(".atc/contest.toml");
        let backup_tests = staging_root.join("previous-tests");
        let backup_metadata = staging_root.join("previous-contest.toml");
        safe_file::require_absent(&backup_tests)?;
        safe_file::require_absent(&backup_metadata)?;
        verify_staging()?;
        self.revalidate()?;
        safe_file::write_recovery_intent_at(&staged.root, identity)?;

        if self.tests.is_some() {
            safe_file::rename_noclobber_at(
                &live.root,
                &destination_tests,
                &staged.root,
                &backup_tests,
            )?;
        }
        before_metadata_backup(staging_root);
        self.open_unchanged()?;
        verify_staging()?;
        if live.metadata(destination)? != self.metadata {
            return Err(changed_local_data());
        }
        if let Some(marker) = live.marker.as_ref().filter(|_| self.metadata.is_some())
            && let Err(error) = safe_file::rename_noclobber_at(
                marker,
                &destination_metadata,
                &staged.root,
                &backup_metadata,
            )
        {
            // The second backup failed. Restore the verified tests only if
            // their former name is still free. Keep recovery data on failure.
            if self.tests.is_some() {
                let restore = || {
                    self.open_unchanged()?;
                    verify_staging()?;
                    if snapshot_tests(&staged.root, staging_root, "previous-tests")? != self.tests {
                        return Err(changed_local_data());
                    }
                    safe_file::rename_noclobber_at(
                        &staged.root,
                        &backup_tests,
                        &live.root,
                        &destination_tests,
                    )
                };
                if let Err(rollback) = restore() {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("{error}. Tests could not be restored: {rollback}"),
                    ));
                }
            }
            return Err(error);
        }

        verify_staging()?;
        self.open_unchanged()?;
        self.verify_backups(&staged.root, staging_root)?;
        if entry_exists(&live.root, "tests")?
            || live.metadata(destination)?.is_some()
            || staged.metadata(staging_root)?.as_ref() != Some(&staged_metadata)
            || snapshot_tests(&staged.root, staging_root, "tests")? != staged_tests
        {
            return Err(changed_local_data());
        }

        if staged_tests.is_some() {
            safe_file::rename_noclobber_at(
                &staged.root,
                &staging_root.join("tests"),
                &live.root,
                &destination_tests,
            )?;
        }

        before_metadata_install(staging_root);
        self.open_unchanged()?;
        verify_staging()?;
        if staged.metadata(staging_root)?.as_ref() != Some(&staged_metadata) {
            return Err(changed_local_data());
        }
        let staged_marker_path = staging_root.join(".atc");
        let staged_marker = staged.marker.take().ok_or_else(changed_local_data)?;
        let expected_marker = if let Some(marker) = &live.marker {
            safe_file::rename_noclobber_at(
                &staged_marker,
                &staged_marker_path.join("contest.toml"),
                marker,
                &destination_metadata,
            )?;
            &self.directories.marker
        } else {
            for entry in staged_marker.entries()? {
                if entry?.file_name() != "contest.toml" {
                    return Err(changed_local_data());
                }
            }
            // Windows directory handles opened without delete sharing must be
            // released before the directory itself can be renamed.
            drop(staged_marker);
            safe_file::rename_noclobber_at(
                &staged.root,
                &staged_marker_path,
                &live.root,
                &destination.join(".atc"),
            )?;
            &staged_identities.marker
        };

        let installed = LiveDirectories::open(destination)?;
        let installed_identities = installed.identities(destination)?;
        if installed_identities.root != self.directories.root
            || &installed_identities.marker != expected_marker
            || installed.metadata(destination)?.as_ref() != Some(&staged_metadata)
            || snapshot_tests(&installed.root, destination, "tests")? != staged_tests
            || LiveDirectories::open(destination)?.identities(destination)? != installed_identities
        {
            return Err(changed_local_data());
        }
        self.verify_backups(&staged.root, staging_root)?;
        let current_staging = LiveDirectories::open(staging_root)?.identities(staging_root)?;
        if current_staging.root != staged_identities.root {
            return Err(changed_local_data());
        }
        Ok(staged_identities.root)
    }

    fn verify_backups(&self, staging: &CapDir, root: &Path) -> io::Result<()> {
        let name = "previous-contest.toml";
        let actual_metadata = if entry_exists(staging, name)? {
            Some(safe_file::read_bounded_regular_file_snapshot_at(
                staging,
                OsStr::new(name),
                &root.join(name),
                MAX_CONTEST_METADATA_BYTES,
            )?)
        } else {
            None
        };
        if actual_metadata != self.metadata
            || snapshot_tests(staging, root, "previous-tests")? != self.tests
        {
            return Err(changed_local_data());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Contest, Problem, Sample};
    use std::fs;

    const OLD_METADATA: &str = "version = 99\ncontest_id = 'abc466'\n";

    fn replacement(destination: &Path) -> (Box<MigrationPlan>, PreservedTempDir) {
        fs::create_dir_all(destination.join(".atc")).unwrap();
        fs::write(destination.join(".atc/contest.toml"), OLD_METADATA).unwrap();
        fs::create_dir_all(destination.join("tests/legacy")).unwrap();
        fs::write(destination.join("tests/legacy/custom"), b"old test").unwrap();
        let MigrationInspection::Ready(plan) =
            inspect(destination.to_path_buf(), "abc466".into()).unwrap()
        else {
            panic!("old metadata should be eligible for migration");
        };
        let staging = PreservedTempDir::new(
            tempfile::Builder::new()
                .prefix(".atc-migrate-")
                .tempdir_in(destination)
                .unwrap(),
        );
        let problem = Problem {
            index: "A".into(),
            title: "Official fixture".into(),
            url: "https://atcoder.jp/contests/abc466/tasks/abc466_a".into(),
            task_id: "abc466_a".into(),
            sample_count: 1,
        };
        crate::workspace::save_samples(
            staging.path(),
            &problem,
            &[Sample {
                input: "1\n".into(),
                output: "2\n".into(),
            }],
        )
        .unwrap();
        crate::workspace::save_metadata(
            staging.path(),
            &Contest {
                contest_id: "abc466".into(),
                problems: vec![problem],
            },
        )
        .unwrap();
        (plan, staging)
    }

    #[test]
    fn failure_to_back_up_metadata_prevents_installing_either_new_file() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        let (plan, staging) = replacement(&destination);
        let recover = staging.path().to_path_buf();
        let error = plan
            .apply_with_hooks(
                staging,
                |root| {
                    fs::write(root.join("previous-contest.toml"), b"competitor").unwrap();
                },
                |_| panic!("installation must wait for both backups"),
            )
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(destination.join(".atc/contest.toml")).unwrap(),
            OLD_METADATA.as_bytes()
        );
        assert_eq!(
            fs::read(destination.join("tests/legacy/custom")).unwrap(),
            b"old test"
        );
        assert!(!destination.join("tests/A").exists());
        assert_eq!(
            fs::read(recover.join("previous-contest.toml")).unwrap(),
            b"competitor"
        );
        assert!(recover.join("tests/A/sample-1.in").is_file());
    }

    #[test]
    fn concurrent_install_conflict_keeps_both_backups_and_blocks_further_updates() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        let (plan, staging) = replacement(&destination);
        let recover = staging.path().to_path_buf();
        let error = plan
            .apply_with_hooks(
                staging,
                |_| {},
                |_| {
                    assert!(destination.join("tests/A/sample-1.in").is_file());
                    fs::write(destination.join(".atc/contest.toml"), b"concurrent change").unwrap();
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(destination.join(".atc/contest.toml")).unwrap(),
            b"concurrent change"
        );
        assert_eq!(
            fs::read(destination.join("tests/A/sample-1.in")).unwrap(),
            b"1\n"
        );
        assert_eq!(
            fs::read(recover.join("previous-tests/legacy/custom")).unwrap(),
            b"old test"
        );
        assert_eq!(
            fs::read(recover.join("previous-contest.toml")).unwrap(),
            OLD_METADATA.as_bytes()
        );
        assert!(matches!(
            diagnose_contest(&destination, "abc466"),
            ContestDiagnosis::Blocked(BlockReason::RecoveryRequired { path }) if path == recover
        ));
    }
}
