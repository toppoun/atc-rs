use crate::model::{Contest, Problem, Sample};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use tempfile::TempDir;

const METADATA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContestMetadata {
    version: u32,
    contest_id: String,
    problems: Vec<Problem>,
}

pub fn contest_path(root: &Path, contest_id: &str) -> io::Result<PathBuf> {
    validate_path_component(contest_id, "contest ID")?;
    Ok(root.join(contest_id))
}

pub fn save_metadata(destination: &Path, contest: &Contest) -> io::Result<()> {
    validate_contest_paths(contest)?;

    let atc_dir = destination.join(".atc");

    fs::create_dir_all(&atc_dir)?;

    let metadata = ContestMetadata {
        version: METADATA_VERSION,
        contest_id: contest.contest_id.clone(),
        problems: contest.problems.clone(),
    };

    let content = toml::to_string_pretty(&metadata).map_err(io::Error::other)?;

    fs::write(atc_dir.join("contest.toml"), content)?;

    Ok(())
}

pub fn load_metadata(destination: &Path) -> io::Result<Contest> {
    let path = destination.join(".atc").join("contest.toml");

    let content = fs::read_to_string(path)?;

    let metadata: ContestMetadata = toml::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    if metadata.version != METADATA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported contest metadata version: {} (expected {METADATA_VERSION})",
                metadata.version
            ),
        ));
    }

    Ok(Contest {
        contest_id: metadata.contest_id,
        problems: metadata.problems,
    })
}

pub fn save_samples(destination: &Path, problem: &Problem, samples: &[Sample]) -> io::Result<()> {
    if samples.is_empty() {
        return Ok(());
    }

    validate_path_component(&problem.index, "problem index")?;

    let test_dir = destination.join("tests").join(&problem.index);

    fs::create_dir_all(&test_dir)?;

    for (i, sample) in samples.iter().enumerate() {
        let number = i + 1;

        fs::write(test_dir.join(format!("sample-{number}.in")), &sample.input)?;

        fs::write(
            test_dir.join(format!("sample-{number}.out")),
            &sample.output,
        )?;
    }

    Ok(())
}

pub fn create_source_files(
    destination: &Path,
    problems: &[Problem],
    template: &str,
) -> io::Result<()> {
    for problem in problems {
        validate_path_component(&problem.index, "problem index")?;
        let path = destination.join(format!("{}.cpp", problem.index));

        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => file.write_all(template.as_bytes())?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

fn validate_path_component(value: &str, kind: &str) -> io::Result<()> {
    let mut components = Path::new(value).components();
    let is_single_normal_component = matches!(
        components.next(),
        Some(Component::Normal(component)) if component == OsStr::new(value)
    ) && components.next().is_none();

    if is_single_normal_component {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid {kind} for a file name: {value:?}"),
    ))
}

pub fn validate_contest_paths(contest: &Contest) -> io::Result<()> {
    validate_path_component(&contest.contest_id, "contest ID")?;
    for problem in &contest.problems {
        validate_path_component(&problem.index, "problem index")?;
    }
    Ok(())
}

pub fn validate_workspace_marker(destination: &Path) -> io::Result<()> {
    let marker = destination.join(".atc");

    if existing_real_directory(&marker, "workspace marker")? {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("workspace marker not found: {}", marker.display()),
    ))
}

pub fn validate_refresh_destination(cwd: &Path, contest_id: &str) -> std::io::Result<()> {
    validate_path_component(contest_id, "contest ID")?;
    validate_workspace_marker(cwd)?;

    let directory_name = cwd.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "current directory has no directory name",
        )
    })?;

    if directory_name != std::ffi::OsStr::new(contest_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("current directory is not {contest_id}: {}", cwd.display()),
        ));
    }

    Ok(())
}

pub fn replace_refresh_data(destination: &Path, staging: TempDir) -> io::Result<()> {
    validate_workspace_marker(destination)?;

    let staging_root = staging.path().to_path_buf();
    let destination_tests = destination.join("tests");
    let staged_tests = staging_root.join("tests");
    let backup_tests = staging_root.join("previous-tests");
    let destination_metadata = destination.join(".atc").join("contest.toml");
    let staged_metadata = staging_root.join(".atc").join("contest.toml");
    let backup_metadata = staging_root.join("previous-contest.toml");

    let had_destination_tests = existing_real_directory(&destination_tests, "existing tests path")?;
    let has_staged_tests = existing_real_directory(&staged_tests, "staged tests path")?;
    let had_destination_metadata =
        existing_regular_file(&destination_metadata, "existing metadata path")?;
    if !existing_regular_file(&staged_metadata, "staged metadata path")? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("staged metadata not found: {}", staged_metadata.display()),
        ));
    }

    if had_destination_tests {
        fs::rename(&destination_tests, &backup_tests)?;
    }

    if has_staged_tests && let Err(error) = fs::rename(&staged_tests, &destination_tests) {
        let rollback_errors = rollback_tests(
            &destination_tests,
            &staged_tests,
            &backup_tests,
            false,
            had_destination_tests,
        );
        return Err(refresh_update_error(staging, error, rollback_errors));
    }

    if had_destination_metadata
        && let Err(error) = fs::rename(&destination_metadata, &backup_metadata)
    {
        let rollback_errors = rollback_tests(
            &destination_tests,
            &staged_tests,
            &backup_tests,
            has_staged_tests,
            had_destination_tests,
        );
        return Err(refresh_update_error(staging, error, rollback_errors));
    }

    if let Err(error) = fs::rename(&staged_metadata, &destination_metadata) {
        let mut rollback_errors = Vec::new();
        if had_destination_metadata
            && let Err(rollback_error) = fs::rename(&backup_metadata, &destination_metadata)
        {
            rollback_errors.push(format!(
                "failed to restore metadata {}: {rollback_error}",
                destination_metadata.display()
            ));
        }
        rollback_errors.extend(rollback_tests(
            &destination_tests,
            &staged_tests,
            &backup_tests,
            has_staged_tests,
            had_destination_tests,
        ));
        return Err(refresh_update_error(staging, error, rollback_errors));
    }

    Ok(())
}

fn existing_real_directory(path: &Path, kind: &str) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a real directory: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn existing_regular_file(path: &Path, kind: &str) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a regular file: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn rollback_tests(
    destination_tests: &Path,
    staged_tests: &Path,
    backup_tests: &Path,
    new_tests_were_moved: bool,
    old_tests_were_moved: bool,
) -> Vec<String> {
    let mut errors = Vec::new();

    if new_tests_were_moved && let Err(error) = fs::rename(destination_tests, staged_tests) {
        errors.push(format!(
            "failed to move new tests out of {}: {error}",
            destination_tests.display()
        ));
    }
    if old_tests_were_moved && let Err(error) = fs::rename(backup_tests, destination_tests) {
        errors.push(format!(
            "failed to restore previous tests {}: {error}",
            destination_tests.display()
        ));
    }

    errors
}

fn refresh_update_error(
    staging: TempDir,
    original: io::Error,
    rollback_errors: Vec<String>,
) -> io::Error {
    if rollback_errors.is_empty() {
        return original;
    }

    let kind = original.kind();
    let recovery_path = staging.keep();
    io::Error::new(
        kind,
        format!(
            "refresh update failed: {original}; rollback also failed: {}; recovery data kept at {}",
            rollback_errors.join("; "),
            recovery_path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(index: &str) -> Problem {
        Problem {
            index: index.to_string(),
            title: format!("Problem {index}"),
            task_id: format!("abc466_{}", index.to_ascii_lowercase()),
            url: format!(
                "https://atcoder.jp/contests/abc466/tasks/abc466_{}",
                index.to_ascii_lowercase()
            ),
        }
    }

    #[test]
    fn save_and_load_metadata() {
        let temp = tempfile::tempdir().unwrap();

        let contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![
                Problem {
                    index: "A".to_string(),
                    title: "Compromise".to_string(),
                    task_id: "abc466_a".to_string(),
                    url: "https://atcoder.jp/contests/abc466/tasks/abc466_a".to_string(),
                },
                Problem {
                    index: "B".to_string(),
                    title: "Representative Balls".to_string(),
                    task_id: "abc466_b".to_string(),
                    url: "https://atcoder.jp/contests/abc466/tasks/abc466_b".to_string(),
                },
            ],
        };

        save_metadata(temp.path(), &contest).unwrap();

        let loaded = load_metadata(temp.path()).unwrap();

        assert_eq!(loaded, contest);
    }

    #[test]
    fn saves_samples_with_expected_names_and_contents() {
        let temp = tempfile::tempdir().unwrap();
        let samples = vec![
            Sample {
                input: "1 2\n".to_string(),
                output: "3\n".to_string(),
            },
            Sample {
                input: "4 5\n".to_string(),
                output: "9\n".to_string(),
            },
        ];

        save_samples(temp.path(), &problem("A"), &samples).unwrap();

        let test_dir = temp.path().join("tests").join("A");
        assert_eq!(
            fs::read_to_string(test_dir.join("sample-1.in")).unwrap(),
            "1 2\n"
        );
        assert_eq!(
            fs::read_to_string(test_dir.join("sample-1.out")).unwrap(),
            "3\n"
        );
        assert_eq!(
            fs::read_to_string(test_dir.join("sample-2.in")).unwrap(),
            "4 5\n"
        );
        assert_eq!(
            fs::read_to_string(test_dir.join("sample-2.out")).unwrap(),
            "9\n"
        );
    }

    #[test]
    fn empty_samples_do_not_create_tests_directory() {
        let temp = tempfile::tempdir().unwrap();

        save_samples(temp.path(), &problem("A"), &[]).unwrap();

        assert!(!temp.path().join("tests").exists());
    }

    #[test]
    fn creates_a_source_file_for_each_problem() {
        let temp = tempfile::tempdir().unwrap();
        let problems = vec![problem("A"), problem("B"), problem("C")];

        create_source_files(temp.path(), &problems, "template").unwrap();

        for problem in problems {
            assert_eq!(
                fs::read_to_string(temp.path().join(format!("{}.cpp", problem.index))).unwrap(),
                "template"
            );
        }
    }

    #[test]
    fn existing_source_file_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("A.cpp");
        fs::write(&source, "user source").unwrap();

        create_source_files(temp.path(), &[problem("A")], "template").unwrap();

        assert_eq!(fs::read_to_string(source).unwrap(), "user source");
    }

    #[test]
    fn rejects_problem_index_that_escapes_destination() {
        let temp = tempfile::tempdir().unwrap();

        let error = create_source_files(temp.path(), &[problem("../outside")], "template")
            .expect_err("unsafe problem index should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!temp.path().join("outside.cpp").exists());
    }

    #[test]
    fn rejects_unknown_metadata_version() {
        let temp = tempfile::tempdir().unwrap();
        let atc_dir = temp.path().join(".atc");
        fs::create_dir(&atc_dir).unwrap();
        fs::write(
            atc_dir.join("contest.toml"),
            "version = 2\ncontest_id = \"abc466\"\nproblems = []\n",
        )
        .unwrap();

        let error = load_metadata(temp.path()).expect_err("unknown version should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_contest_id_that_escapes_root() {
        let temp = tempfile::tempdir().unwrap();

        let error = contest_path(temp.path(), "../outside")
            .expect_err("unsafe contest ID should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn malformed_metadata_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let atc_dir = temp.path().join(".atc");
        fs::create_dir(&atc_dir).unwrap();

        fs::write(atc_dir.join("contest.toml"), "version = ???").unwrap();

        let error = load_metadata(temp.path()).expect_err("malformed metadata should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn missing_required_metadata_field_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let atc_dir = temp.path().join(".atc");
        fs::create_dir(&atc_dir).unwrap();

        fs::write(atc_dir.join("contest.toml"), "version = 1\nproblems = []\n").unwrap();

        let error = load_metadata(temp.path()).expect_err("missing required field should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unknown_metadata_field_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let atc_dir = temp.path().join(".atc");
        fs::create_dir(&atc_dir).unwrap();

        fs::write(
            atc_dir.join("contest.toml"),
            concat!(
                "version = 1\n",
                "contest_id = \"abc466\"\n",
                "problems = []\n",
                "unexpected = true\n",
            ),
        )
        .unwrap();

        let error = load_metadata(temp.path()).expect_err("unknown field should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn refresh_replacement_rejects_tests_file_without_changing_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir(&destination).unwrap();
        let old_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("OLD")],
        };
        save_metadata(&destination, &old_contest).unwrap();
        fs::write(destination.join("tests"), "user file").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        let new_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("NEW")],
        };
        save_metadata(staging.path(), &new_contest).unwrap();

        let error = replace_refresh_data(&destination, staging)
            .expect_err("a tests file must not be deleted as a directory");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            fs::read_to_string(destination.join("tests")).unwrap(),
            "user file"
        );
        assert_eq!(load_metadata(&destination).unwrap(), old_contest);
    }
}
