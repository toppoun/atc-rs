use crate::atcoder;
use crate::error::AppError;
use crate::workspace;
use std::path::Path;

const TEMPLATE: &str = r#"#include <bits/stdc++.h>
using namespace std;

#ifdef LOCAL
#include <atc/debug.hpp>
#else
#define debug(...) ((void)0)
#endif

using ll = long long;

// 1. 見方・状態:
// 
// 2. 答えに必要な情報:
// 
// 3. 捨てる情報と根拠:
// 
// 4. 初期化・更新・判定・計算量:
// 


int main() {
    ios::sync_with_stdio(false);
    cin.tie(nullptr);

    return 0;
}
"#;

pub fn new(contest_id: &str) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::contest_path(&cwd, contest_id)?;

    if existing_contest_is_noop(&destination)? {
        return Ok(());
    }

    let atcoder = if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)
    } else {
        atcoder::AtCoderClient::new()?
    };

    new_at(&destination, contest_id, &atcoder)
}

fn new_at(
    destination: &Path,
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
) -> Result<(), AppError> {
    if existing_contest_is_noop(destination)? {
        return Ok(());
    }

    let contest = atcoder.fetch_contest(contest_id)?;
    let mut samples_by_problem = Vec::with_capacity(contest.problems.len());

    for problem in &contest.problems {
        match atcoder.fetch_samples(problem) {
            Ok(samples) => samples_by_problem.push(Some(samples)),

            Err(err) => {
                eprintln!(
                    "[WARN] failed to fetch samples for {}: {err}",
                    problem.index
                );
                samples_by_problem.push(None);
            }
        }
    }

    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "contest destination has no parent directory: {}",
                destination.display()
            ),
        )
    })?;
    let staging = tempfile::Builder::new()
        .prefix(".atc-new-")
        .tempdir_in(parent)?;

    workspace::create_source_files(staging.path(), &contest.problems, TEMPLATE)?;
    for (problem, samples) in contest.problems.iter().zip(samples_by_problem) {
        if let Some(samples) = samples {
            workspace::save_samples(staging.path(), problem, &samples)?;
        }
    }
    workspace::save_metadata(staging.path(), &contest)?;

    // Another process may have created the contest while fixtures/HTTP were read.
    if existing_contest_is_noop(destination)? {
        return Ok(());
    }

    match std::fs::rename(staging.path(), destination) {
        Ok(()) => {
            drop(staging.keep());
            Ok(())
        }
        Err(_) if destination.is_dir() => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn existing_contest_is_noop(destination: &Path) -> std::io::Result<bool> {
    if !destination.try_exists()? {
        return Ok(false);
    }

    if destination.is_dir() {
        return Ok(true);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "contest destination exists but is not a directory: {}",
            destination.display()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace;
    use std::path::PathBuf;

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    #[test]
    fn new_flow_runs_entirely_from_fixtures() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        let client = atcoder::AtCoderClient::fixture(fixture_root());

        new_at(&destination, "abc466", &client).expect("fixture new flow should succeed");

        let contest =
            workspace::load_metadata(&destination).expect("created metadata should be readable");
        assert_eq!(contest.contest_id, "abc466");
        assert_eq!(contest.problems.len(), 7);

        for problem in &contest.problems {
            let test_dir = destination.join("tests").join(&problem.index);

            if problem.index == "C" {
                assert!(!test_dir.exists());
            } else {
                assert!(test_dir.join("sample-1.in").is_file());

                assert!(test_dir.join("sample-1.out").is_file());
            }
        }
    }

    #[test]
    fn existing_contest_is_a_noop_before_fixture_access() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("contest directory should be created");
        std::fs::write(destination.join("A.cpp"), "user source")
            .expect("existing source should be written");
        let client = atcoder::AtCoderClient::fixture(temp.path().join("missing-fixtures"));

        new_at(&destination, "abc466", &client).expect("existing contest should be a no-op");

        assert_eq!(
            std::fs::read_to_string(destination.join("A.cpp"))
                .expect("existing source should remain readable"),
            "user source"
        );
    }

    #[test]
    fn failed_workspace_build_does_not_leave_partial_contest() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let fixture_root = temp.path().join("fixtures");
        let contests = fixture_root.join("contests");
        std::fs::create_dir_all(&contests).expect("fixture directory should be created");
        std::fs::write(
            contests.join("broken.html"),
            r#"<table><tbody><tr>
                <td><a href="/contests/broken/tasks/broken_a">../outside</a></td>
                <td><a href="/contests/broken/tasks/broken_a">Broken</a></td>
            </tr></tbody></table>"#,
        )
        .expect("fixture should be written");
        let destination = temp.path().join("broken");
        let client = atcoder::AtCoderClient::fixture(&fixture_root);

        let error =
            new_at(&destination, "broken", &client).expect_err("unsafe workspace path should fail");

        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(!destination.exists());
        assert!(
            std::fs::read_dir(temp.path())
                .expect("temporary root should be readable")
                .all(|entry| !entry
                    .expect("directory entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".atc-new-"))
        );
    }
}
