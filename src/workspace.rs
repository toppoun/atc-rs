use crate::model::{Contest, Problem, Sample};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

#[derive(Serialize, Deserialize)]
struct ContestMetadata {
    version: u32,
    contest_id: String,
    problems: Vec<Problem>,
}

pub fn create_contest_dir(destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    Ok(())
}

pub fn save_metadata(destination: &Path, contest: &Contest) -> io::Result<()> {
    let atc_dir = destination.join(".atc");

    fs::create_dir_all(&atc_dir)?;

    let metadata = ContestMetadata {
        version: 1,
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

    let metadata: ContestMetadata = toml::from_str(&content).map_err(io::Error::other)?;

    Ok(Contest {
        contest_id: metadata.contest_id,
        problems: metadata.problems,
    })
}

pub fn save_samples(destination: &Path, problem: &Problem, samples: &[Sample]) -> io::Result<()> {
    if samples.is_empty() {
        return Ok(());
    }

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
        let path = destination.join(format!("{}.cpp", problem.index));

        if path.exists() {
            continue;
        }

        fs::write(path, template)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
