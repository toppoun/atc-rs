use crate::model::{Contest, Problem, Sample};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

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

// Local-only commands will use this once they are implemented.
#[allow(dead_code)]
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
}
