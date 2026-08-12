use crate::atcoder;
use crate::config::Config;
use crate::error::AppError;
use crate::language::Language;
use crate::model::{Contest, Sample};
use crate::template::builtin_template;
use crate::ui::{Event, Reporter};
use crate::workspace;
use crate::workspace::validate_refresh_destination;
use std::path::Path;

struct FetchedContestData {
    contest: Contest,
    samples_by_problem: Vec<Option<Vec<Sample>>>,
}

pub fn new(contest_id: &str, reporter: &mut dyn Reporter) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::contest_path(&cwd, contest_id)?;

    if existing_contest_is_noop(&destination)? {
        return Ok(());
    }
    let config = Config::load()?;
    let language = config.defaults.language;

    let atcoder = if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)
    } else {
        atcoder::AtCoderClient::new()?
    };

    new_at(&destination, contest_id, language, &atcoder, reporter)
}

fn new_at(
    destination: &Path,
    contest_id: &str,
    language: Language,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    if existing_contest_is_noop(destination)? {
        return Ok(());
    }

    let template = builtin_template(language);

    let FetchedContestData {
        contest,
        samples_by_problem,
    } = fetch_contest_data(contest_id, atcoder, reporter)?;

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

    workspace::create_source_files(staging.path(), &contest.problems, language, template)?;
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
            reporter.report(Event::WorkspaceCreated { destination });
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

fn fetch_contest_data(
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<FetchedContestData, AppError> {
    reporter.report(Event::ContestFetching { contest_id });

    let contest = atcoder.fetch_contest(contest_id)?;

    reporter.report(Event::ContestFetched {
        contest_id: &contest.contest_id,
        problems: contest.problems.len(),
    });

    let total = contest.problems.len();
    let mut samples_by_problem = Vec::with_capacity(total);

    for (i, problem) in contest.problems.iter().enumerate() {
        reporter.report(Event::ProblemFetching {
            index: &problem.index,
            current: i + 1,
            total,
        });
        match atcoder.fetch_samples(problem) {
            Ok(samples) => {
                reporter.report(Event::ProblemFetched {
                    index: &problem.index,
                    samples: samples.len(),
                });
                samples_by_problem.push(Some(samples));
            }

            Err(err) => {
                let message = err.to_string();

                reporter.report(Event::ProblemFetchFailed {
                    index: &problem.index,
                    error: &message,
                });
                samples_by_problem.push(None);
            }
        }
    }
    Ok(FetchedContestData {
        contest,
        samples_by_problem,
    })
}

// ----- Refresh -----
pub fn refresh(contest: Option<String>, reporter: &mut dyn Reporter) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let contest_id = resolve_refresh_contest_id(&cwd, contest.as_deref())?;

    let atcoder = if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)
    } else {
        atcoder::AtCoderClient::new()?
    };

    refresh_at(&cwd, &contest_id, &atcoder, reporter)
}

fn resolve_refresh_contest_id(
    destination: &Path,
    specified_contest_id: Option<&str>,
) -> Result<String, AppError> {
    match specified_contest_id {
        Some(contest_id) => {
            validate_refresh_destination(destination, contest_id)?;
            Ok(contest_id.to_string())
        }
        None => {
            workspace::validate_workspace_marker(destination)?;
            Ok(workspace::load_metadata(destination)?.contest_id)
        }
    }
}

fn refresh_at(
    destination: &Path,
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    workspace::validate_workspace_marker(destination)?;

    let FetchedContestData {
        contest,
        samples_by_problem,
    } = fetch_contest_data(contest_id, atcoder, reporter)?;
    workspace::validate_contest_paths(&contest)?;

    let staging = tempfile::Builder::new()
        .prefix(".atc-refresh-")
        .tempdir_in(destination)?;

    for (problem, samples) in contest.problems.iter().zip(samples_by_problem) {
        if let Some(samples) = samples {
            workspace::save_samples(staging.path(), problem, &samples)?;
        }
    }
    workspace::save_metadata(staging.path(), &contest)?;

    workspace::replace_refresh_data(destination, staging)?;
    reporter.report(Event::WorkspaceRefreshed { destination });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace;
    use std::path::PathBuf;

    use crate::ui::NullReporter;

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    fn old_contest(contest_id: &str) -> Contest {
        Contest {
            contest_id: contest_id.to_string(),
            problems: vec![crate::model::Problem {
                index: "OLD".to_string(),
                title: "Old problem".to_string(),
                task_id: "old_problem".to_string(),
                url: "https://atcoder.jp/contests/old/tasks/old_problem".to_string(),
            }],
        }
    }

    fn create_workspace(destination: &Path, contest_id: &str) {
        std::fs::create_dir(destination).expect("workspace directory should be created");
        workspace::save_metadata(destination, &old_contest(contest_id))
            .expect("old metadata should be written");
    }

    #[derive(Default)]
    struct RecordingReporter {
        events: Vec<String>,
    }

    impl Reporter for RecordingReporter {
        fn report(&mut self, event: Event<'_>) {
            let event = match event {
                Event::ContestFetching { contest_id } => format!("contest-fetching:{contest_id}"),
                Event::ContestFetched {
                    contest_id,
                    problems,
                } => format!("contest-fetched:{contest_id}:{problems}"),
                Event::ProblemFetching { index, .. } => format!("problem-fetching:{index}"),
                Event::ProblemFetched { index, samples } => {
                    format!("problem-fetched:{index}:{samples}")
                }
                Event::ProblemFetchFailed { index, .. } => format!("problem-failed:{index}"),
                Event::WorkspaceCreated { destination } => {
                    format!("created:{}", destination.display())
                }
                Event::WorkspaceRefreshed { destination } => {
                    format!("refreshed:{}", destination.display())
                }
            };
            self.events.push(event);
        }
    }

    #[test]
    fn new_flow_runs_entirely_from_fixtures() {
        let mut reporter = NullReporter;
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        let client = atcoder::AtCoderClient::fixture(fixture_root());

        new_at(
            &destination,
            "abc466",
            Language::Cpp,
            &client,
            &mut reporter,
        )
        .expect("fixture new flow should succeed");

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
        let mut reporter = NullReporter;
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("contest directory should be created");
        std::fs::write(destination.join("A.cpp"), "user source")
            .expect("existing source should be written");
        let client = atcoder::AtCoderClient::fixture(temp.path().join("missing-fixtures"));

        new_at(
            &destination,
            "abc466",
            Language::Cpp,
            &client,
            &mut reporter,
        )
        .expect("existing contest should be a no-op");

        assert_eq!(
            std::fs::read_to_string(destination.join("A.cpp"))
                .expect("existing source should remain readable"),
            "user source"
        );
    }

    #[test]
    fn failed_workspace_build_does_not_leave_partial_contest() {
        let mut reporter = NullReporter;
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

        let error = new_at(
            &destination,
            "broken",
            Language::Cpp,
            &client,
            &mut reporter,
        )
        .expect_err("unsafe workspace path should fail");

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

    #[test]
    fn refresh_rebuilds_metadata_and_tests_without_touching_sources() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        create_workspace(&destination, "abc466");
        std::fs::write(destination.join("A.cpp"), "user source").expect("source should be written");
        std::fs::write(destination.join("LOCAL.cpp"), "local source")
            .expect("local source should be written");
        let stale_tests = destination.join("tests").join("C");
        std::fs::create_dir_all(&stale_tests).expect("stale tests should be created");
        std::fs::write(stale_tests.join("old.in"), "stale").expect("stale test should be written");

        let client = atcoder::AtCoderClient::fixture(fixture_root());
        let mut reporter = RecordingReporter::default();
        refresh_at(&destination, "abc466", &client, &mut reporter)
            .expect("fixture refresh should succeed");

        assert_eq!(
            std::fs::read_to_string(destination.join("A.cpp"))
                .expect("source should remain readable"),
            "user source"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("LOCAL.cpp"))
                .expect("local source should remain readable"),
            "local source"
        );
        assert!(!destination.join("tests").join("C").exists());
        assert!(
            destination
                .join("tests")
                .join("A")
                .join("sample-1.in")
                .is_file()
        );

        let contest =
            workspace::load_metadata(&destination).expect("refreshed metadata should be readable");
        assert_eq!(contest.contest_id, "abc466");
        assert_eq!(contest.problems.len(), 7);
        assert!(
            reporter
                .events
                .contains(&format!("refreshed:{}", destination.display()))
        );
        assert!(
            std::fs::read_dir(&destination)
                .expect("workspace should be readable")
                .all(|entry| !entry
                    .expect("directory entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".atc-refresh-"))
        );
    }

    #[test]
    fn refresh_sample_failure_is_recoverable_and_removes_old_problem_tests() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("mini");
        create_workspace(&destination, "mini");
        let old_b_tests = destination.join("tests").join("B");
        std::fs::create_dir_all(&old_b_tests).expect("old tests should be created");
        std::fs::write(old_b_tests.join("old.in"), "old").expect("old test should be written");

        let fixtures = temp.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("contests"))
            .expect("contest fixtures should be created");
        std::fs::create_dir_all(fixtures.join("problems"))
            .expect("problem fixtures should be created");
        std::fs::write(
            fixtures.join("contests").join("mini.html"),
            r#"<table><tbody>
                <tr><td><a href="/contests/mini/tasks/mini_a">A</a></td><td><a href="/contests/mini/tasks/mini_a">A</a></td></tr>
                <tr><td><a href="/contests/mini/tasks/mini_b">B</a></td><td><a href="/contests/mini/tasks/mini_b">B</a></td></tr>
            </tbody></table>"#,
        )
        .expect("tasks fixture should be written");
        std::fs::write(
            fixtures.join("problems").join("mini_a.html"),
            r#"<div id="task-statement"><span class="lang-en">
                <div class="part"><section><h3>Sample Input 1</h3><pre>1\n</pre></section></div>
                <div class="part"><section><h3>Sample Output 1</h3><pre>2\n</pre></section></div>
            </span></div>"#,
        )
        .expect("problem fixture should be written");

        let client = atcoder::AtCoderClient::fixture(&fixtures);
        let mut reporter = RecordingReporter::default();
        refresh_at(&destination, "mini", &client, &mut reporter)
            .expect("partial sample failure should be recoverable");

        assert!(destination.join("tests").join("A").is_dir());
        assert!(!destination.join("tests").join("B").exists());
        assert!(
            reporter
                .events
                .iter()
                .any(|event| event == "problem-failed:B")
        );
    }

    #[test]
    fn refresh_without_override_does_not_infer_id_from_directory_name() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("directory should be created");
        std::fs::create_dir(destination.join(".atc")).expect("marker should be created");
        std::fs::write(
            destination.join(".atc").join("contest.toml"),
            "version = ???",
        )
        .expect("broken metadata should be written");

        let error = resolve_refresh_contest_id(&destination, None)
            .expect_err("broken metadata must not fall back to the directory name");

        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn refresh_override_requires_workspace_marker() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("directory should be created");

        let error = resolve_refresh_contest_id(&destination, Some("abc466"))
            .expect_err("an arbitrary matching directory must be rejected");

        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn refresh_override_recovers_missing_metadata_inside_workspace() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("directory should be created");
        std::fs::create_dir(destination.join(".atc")).expect("marker should be created");

        let contest_id = resolve_refresh_contest_id(&destination, Some("abc466"))
            .expect("override should not require contest.toml");

        assert_eq!(contest_id, "abc466");

        let client = atcoder::AtCoderClient::fixture(fixture_root());
        let mut reporter = NullReporter;
        refresh_at(&destination, &contest_id, &client, &mut reporter)
            .expect("override refresh should reconstruct missing metadata");
        assert_eq!(
            workspace::load_metadata(&destination)
                .expect("reconstructed metadata should load")
                .contest_id,
            "abc466"
        );
    }

    #[test]
    fn refresh_override_recovers_malformed_metadata_inside_workspace() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("directory should be created");
        std::fs::create_dir(destination.join(".atc")).expect("marker should be created");
        std::fs::write(
            destination.join(".atc").join("contest.toml"),
            "version = ???",
        )
        .expect("malformed metadata should be written");

        let contest_id = resolve_refresh_contest_id(&destination, Some("abc466"))
            .expect("override should ignore malformed contest.toml");
        let client = atcoder::AtCoderClient::fixture(fixture_root());
        let mut reporter = NullReporter;
        refresh_at(&destination, &contest_id, &client, &mut reporter)
            .expect("override refresh should replace malformed metadata");

        assert_eq!(
            workspace::load_metadata(&destination)
                .expect("reconstructed metadata should load")
                .contest_id,
            "abc466"
        );
    }

    #[test]
    fn refresh_override_rejects_wrong_directory_name() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("other");
        std::fs::create_dir(&destination).expect("directory should be created");
        std::fs::create_dir(destination.join(".atc")).expect("marker should be created");

        let error = resolve_refresh_contest_id(&destination, Some("abc466"))
            .expect_err("override must match the directory name");

        assert!(matches!(error, AppError::Io(_)));
    }

    #[test]
    fn invalid_fetched_problem_path_preserves_old_refresh_data() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("broken");
        create_workspace(&destination, "broken");
        let old_tests = destination.join("tests").join("OLD");
        std::fs::create_dir_all(&old_tests).expect("old tests should be created");
        std::fs::write(old_tests.join("old.in"), "old").expect("old test should be written");
        let old_metadata = std::fs::read_to_string(destination.join(".atc").join("contest.toml"))
            .expect("old metadata should be readable");

        let fixtures = temp.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("contests"))
            .expect("fixture directory should be created");
        std::fs::write(
            fixtures.join("contests").join("broken.html"),
            r#"<table><tbody><tr>
                <td><a href="/contests/broken/tasks/broken_a">../outside</a></td>
                <td><a href="/contests/broken/tasks/broken_a">Broken</a></td>
            </tr></tbody></table>"#,
        )
        .expect("fixture should be written");
        let client = atcoder::AtCoderClient::fixture(&fixtures);
        let mut reporter = NullReporter;

        refresh_at(&destination, "broken", &client, &mut reporter)
            .expect_err("unsafe fetched path should fail");

        assert_eq!(
            std::fs::read_to_string(destination.join(".atc").join("contest.toml"))
                .expect("old metadata should remain readable"),
            old_metadata
        );
        assert_eq!(
            std::fs::read_to_string(old_tests.join("old.in"))
                .expect("old tests should remain readable"),
            "old"
        );
    }
}
