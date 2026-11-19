use super::{
    SampleFileKind, SampleLayoutError, create_directory_symlink, create_file_symlink,
    problem_with_sample_count, save_metadata, write_metadata_text, write_sample_pair,
};
use crate::model::Contest;
use crate::safe_file;
use crate::workspace::diagnosis::{
    BlockReason, ContestDiagnosis, MetadataIssue, MigrationReason, RepairReason,
    SampleVerification, diagnose_contest,
};
use std::fs;
use std::path::Path;

const CONTEST_ID: &str = "abc466";
const INVALID_METADATA: &str = "version = 1\ncontest_id = \"abc466\"\nproblems = []\n";

fn save_manifest(destination: &Path, sample_count: usize) -> Contest {
    let contest = Contest {
        contest_id: CONTEST_ID.into(),
        problems: vec![problem_with_sample_count("A", sample_count)],
    };
    save_metadata(destination, &contest).unwrap();
    contest
}

fn diagnose(destination: &Path) -> ContestDiagnosis {
    diagnose_contest(destination, CONTEST_ID)
}

#[test]
fn healthy_samples_are_only_structurally_verified_and_diagnosis_does_not_write() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join(CONTEST_ID);
    let contest = save_manifest(&destination, 1);
    write_sample_pair(&destination, "A", 1, "locally edited UTF-8\n");
    fs::write(destination.join("A.cpp"), "local source\n").unwrap();
    let before = safe_file::read_tree_snapshot(temp.path()).unwrap();

    assert_eq!(
        diagnose(&destination),
        ContestDiagnosis::Healthy {
            contest,
            samples: SampleVerification::StructureOnly,
        }
    );
    assert_eq!(safe_file::read_tree_snapshot(temp.path()).unwrap(), before);
}

#[test]
fn missing_directory_and_missing_metadata_do_not_create_entries() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join(CONTEST_ID);
    assert_eq!(diagnose(&destination), ContestDiagnosis::MissingDirectory);
    assert!(!destination.exists());

    fs::create_dir(&destination).unwrap();
    assert_eq!(
        diagnose(&destination),
        ContestDiagnosis::RepairRequired(RepairReason::Metadata(MetadataIssue::Missing))
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);

    fs::create_dir(destination.join(".atc")).unwrap();
    let before = safe_file::read_tree_snapshot(&destination).unwrap();
    assert_eq!(
        diagnose(&destination),
        ContestDiagnosis::RepairRequired(RepairReason::Metadata(MetadataIssue::Missing))
    );
    assert_eq!(safe_file::read_tree_snapshot(&destination).unwrap(), before);
}

#[test]
fn missing_tests_directories_matter_only_when_samples_are_expected() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path();
    let contest = save_manifest(destination, 0);
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Healthy {
            contest,
            samples: SampleVerification::StructureOnly,
        }
    );
    save_manifest(destination, 1);
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::RepairRequired(RepairReason::TestsDirectoryMissing)
    );
    fs::create_dir(destination.join("tests")).unwrap();
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::RepairRequired(RepairReason::ProblemTestsDirectoryMissing {
            index: "A".into(),
        })
    );
}

#[test]
fn distinguishes_missing_pairs_gaps_and_sample_count_mismatches() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path();
    save_manifest(destination, 2);
    write_sample_pair(destination, "A", 1, "sample");
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::RepairRequired(RepairReason::SampleCountMismatch {
            index: "A".into(),
            expected: 2,
            found: 1,
        })
    );
    write_sample_pair(destination, "A", 3, "sample");
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::RepairRequired(RepairReason::SampleLayout {
            index: "A".into(),
            issue: SampleLayoutError::NumberGap {
                expected: 2,
                found: 3,
            },
        })
    );
    write_sample_pair(destination, "A", 2, "sample");
    fs::remove_file(destination.join("tests/A/sample-2.out")).unwrap();
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::RepairRequired(RepairReason::SampleLayout {
            index: "A".into(),
            issue: SampleLayoutError::MissingFile {
                number: 2,
                kind: SampleFileKind::Output,
            },
        })
    );
    write_sample_pair(destination, "A", 2, "sample");
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::RepairRequired(RepairReason::SampleCountMismatch {
            index: "A".into(),
            expected: 2,
            found: 3,
        })
    );
    fs::write(destination.join("tests/A/sample-1.in"), [0xff]).unwrap();
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::RepairRequired(RepairReason::InvalidSamples { index: "A".into() })
    );
}

#[test]
fn invalid_metadata_can_require_repair_when_the_root_owner_is_known() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path();
    write_metadata_text(destination, INVALID_METADATA);
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::RepairRequired(RepairReason::Metadata(MetadataIssue::Invalid))
    );
    write_metadata_text(destination, &INVALID_METADATA.replace(CONTEST_ID, "arc001"));
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Blocked(BlockReason::ContestIdMismatch {
            actual: "arc001".into(),
        })
    );
}

#[test]
fn unsupported_versions_require_separate_handling_after_checking_the_owner() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path();
    for version in [0, 99] {
        write_metadata_text(
            destination,
            &format!("version = {version}\ncontest_id = \"{CONTEST_ID}\"\nlegacy = true\n"),
        );
        assert_eq!(
            diagnose(destination),
            ContestDiagnosis::MigrationRequired(MigrationReason::UnsupportedVersion(version))
        );
    }
    write_metadata_text(destination, "version = 99\ncontest_id = \"arc001\"\n");
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Blocked(BlockReason::ContestIdMismatch {
            actual: "arc001".into(),
        })
    );
}

#[test]
fn unreadable_or_ambiguous_metadata_cannot_establish_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path();
    for body in [
        "version = ???",
        "version = 1\nproblems = []\n",
        "version = 99\nproblems = []\n",
        "version = 1\ncontest_id = \"abc466\"\ncontest_id = \"arc001\"\n",
    ] {
        write_metadata_text(destination, body);
        assert_eq!(
            diagnose(destination),
            ContestDiagnosis::Blocked(BlockReason::MetadataOwnershipUnknown)
        );
    }
    fs::write(destination.join(".atc/contest.toml"), [0xff]).unwrap();
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Blocked(BlockReason::MetadataOwnershipUnknown)
    );
}

#[test]
fn validated_manifest_with_a_different_id_blocks_even_without_tests() {
    let temp = tempfile::tempdir().unwrap();
    save_manifest(temp.path(), 0);
    assert_eq!(
        diagnose_contest(temp.path(), "arc001"),
        ContestDiagnosis::Blocked(BlockReason::ContestIdMismatch {
            actual: CONTEST_ID.into(),
        })
    );
}

#[test]
fn sample_shape_does_not_establish_tests_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path();
    write_sample_pair(destination, "A", 1, "sample");
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Blocked(BlockReason::TestsOwnershipUnknown(MetadataIssue::Missing))
    );
    write_metadata_text(destination, INVALID_METADATA);
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Blocked(BlockReason::TestsOwnershipUnknown(MetadataIssue::Invalid))
    );
    save_manifest(destination, 1);
    let path = destination.join(".atc/contest.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    let repairable = manifest.replace("/tasks/abc466_a", "/tasks/abc466_b");
    fs::write(&path, &repairable).unwrap();
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Blocked(BlockReason::TestsOwnershipUnknown(
            MetadataIssue::Repairable
        ))
    );
    fs::remove_dir_all(destination.join("tests")).unwrap();
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::RepairRequired(RepairReason::Metadata(MetadataIssue::Repairable))
    );

    fs::write(
        &path,
        manifest.replace("https://atcoder.jp", "https://example.invalid"),
    )
    .unwrap();
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Blocked(BlockReason::UnsafeMetadata)
    );
}

#[test]
fn extra_unowned_test_entries_block_even_when_samples_match_the_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path();
    save_manifest(destination, 1);
    write_sample_pair(destination, "A", 1, "sample");
    for name in ["tests/memo.txt", "tests/A/memo.txt", "tests/A/sample-01.in"] {
        let path = destination.join(name);
        fs::write(&path, "keep").unwrap();
        let before = safe_file::read_tree_snapshot(destination).unwrap();
        assert_eq!(
            diagnose(destination),
            ContestDiagnosis::Blocked(BlockReason::UnownedTestsEntry { path: path.clone() })
        );
        assert_eq!(safe_file::read_tree_snapshot(destination).unwrap(), before);
        fs::remove_file(&path).unwrap();
    }
}

#[test]
fn rejects_contest_and_managed_directory_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let actual = temp.path().join("actual");
    save_manifest(&actual, 1);
    write_sample_pair(&actual, "A", 1, "sample");
    let link = temp.path().join(CONTEST_ID);
    if !create_directory_symlink(&actual, &link) {
        return;
    }
    for target in [&link, &link.join(""), &link.join(".")] {
        assert_eq!(
            diagnose(target),
            ContestDiagnosis::Blocked(BlockReason::UnsafeEntry {
                path: target.to_path_buf(),
            })
        );
    }

    let destination = temp.path().join("managed");
    for (name, target) in [
        (".atc", actual.join(".atc")),
        ("tests", actual.join("tests")),
        ("tests/A", actual.join("tests/A")),
    ] {
        let entry = destination.join(name);
        if name == ".atc" {
            fs::create_dir(&destination).unwrap();
        } else {
            save_manifest(&destination, 1);
        }
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        assert!(create_directory_symlink(&target, &entry));
        assert_eq!(
            diagnose(&destination),
            ContestDiagnosis::Blocked(BlockReason::UnsafeEntry {
                path: entry.clone()
            })
        );
        #[cfg(unix)]
        fs::remove_file(&entry).unwrap();
        #[cfg(windows)]
        fs::remove_dir(&entry).unwrap();
    }
}

#[test]
fn rejects_metadata_and_sample_file_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join(CONTEST_ID);
    save_manifest(&destination, 1);
    let path = destination.join(".atc/contest.toml");
    let target = temp.path().join("metadata.toml");
    fs::rename(&path, &target).unwrap();
    if !create_file_symlink(&target, &path) {
        return;
    }
    assert_eq!(
        diagnose(&destination),
        ContestDiagnosis::Blocked(BlockReason::UnsafeEntry { path: path.clone() })
    );
    fs::remove_file(&path).unwrap();
    fs::rename(&target, &path).unwrap();

    write_sample_pair(&destination, "A", 1, "sample");
    let path = destination.join("tests/A/sample-1.in");
    fs::remove_file(&path).unwrap();
    assert!(create_file_symlink(&target, &path));
    assert_eq!(
        diagnose(&destination),
        ContestDiagnosis::Blocked(BlockReason::UnsafeEntry { path })
    );
}

#[test]
fn recovery_pending_blocks_but_preparation_and_finalized_recovery_allow_reads() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path();
    let contest = save_manifest(destination, 0);
    let staging = tempfile::Builder::new()
        .prefix(".atc-repair-")
        .tempdir_in(destination)
        .unwrap();
    let healthy = ContestDiagnosis::Healthy {
        contest,
        samples: SampleVerification::StructureOnly,
    };
    assert_eq!(diagnose(destination), healthy);

    let identity = safe_file::RecoveryIdentity::new("contest", CONTEST_ID, None);
    safe_file::write_recovery_intent(staging.path(), &identity).unwrap();
    let before = safe_file::read_tree_snapshot(destination).unwrap();
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Blocked(BlockReason::RecoveryRequired {
            path: staging.path().to_path_buf(),
        })
    );
    assert_eq!(safe_file::read_tree_snapshot(destination).unwrap(), before);

    let archive = destination.join(".atc-recovery-repair-test");
    safe_file::complete_recovery(
        safe_file::PreservedTempDir::new(staging),
        &archive,
        &identity,
    )
    .unwrap();
    assert_eq!(diagnose(destination), healthy);

    fs::remove_file(archive.join(safe_file::RECOVERY_COMPLETE)).unwrap();
    assert_eq!(
        diagnose(destination),
        ContestDiagnosis::Blocked(BlockReason::RecoveryRequired { path: archive })
    );
}
