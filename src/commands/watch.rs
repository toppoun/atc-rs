use super::test::test_problem;
use crate::config::{Config, RunnerConfig};
use crate::error::AppError;
use crate::language::Language;
use crate::model::{Contest, Problem};
use crate::ui::{Event, Reporter};
use crate::{watcher, workspace};
use std::path::Path;

use super::watch_source::{build_watched_sources, resolve_watched_source};

pub(crate) fn watch(
    cli_contest: Option<&str>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::resolve_contest_target(&cwd, cli_contest)?;

    workspace::validate_workspace_marker(&destination)?;

    let contest = workspace::load_metadata(&destination)?;
    workspace::validate_contest_paths(&contest)?;
    if let Some(contest_id) = cli_contest {
        workspace::validate_contest_identity(&contest, contest_id)?;
    }

    let config = Config::load()?;

    let file_watcher = watcher::FileWatcher::new(&destination)?;

    reporter.report(Event::WatchStarted {
        destination: &destination,
    });

    loop {
        let paths = file_watcher.next_batch()?;

        process_changed_paths(&destination, &contest, &config.runner, paths, reporter)?;
    }
}

fn process_changed_paths(
    destination: &Path,
    contest: &Contest,
    runner_config: &RunnerConfig,
    paths: Vec<std::path::PathBuf>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    process_changed_paths_with(
        destination,
        contest,
        runner_config,
        paths,
        reporter,
        |destination, problem, language, runner_config, debug, reporter| {
            test_problem(
                destination,
                &contest.contest_id,
                problem,
                language,
                runner_config,
                debug,
                reporter,
            )
        },
    )
}

fn process_changed_paths_with(
    destination: &Path,
    contest: &Contest,
    runner_config: &RunnerConfig,
    paths: Vec<std::path::PathBuf>,
    reporter: &mut dyn Reporter,
    mut run_test: impl FnMut(
        &Path,
        &Problem,
        Language,
        &RunnerConfig,
        bool,
        &mut dyn Reporter,
    ) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let watched_sources = build_watched_sources(destination, contest);

    for path in paths {
        if !path.is_file() {
            continue;
        }

        let Some(source) = resolve_watched_source(&watched_sources, &path) else {
            continue;
        };

        let problem = &contest.problems[source.problem];

        reporter.report(Event::WatchSourceChanged { source: &path });

        run_test(
            destination,
            problem,
            source.language,
            runner_config,
            false,
            reporter,
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Problem;

    fn problem(index: &str) -> Problem {
        Problem {
            index: index.to_string(),
            title: format!("Problem {index}"),
            task_id: format!("contest_{}", index.to_ascii_lowercase()),
            url: format!("https://example.invalid/{index}"),
        }
    }

    fn contest(problems: Vec<Problem>) -> Contest {
        Contest {
            contest_id: "contest".to_string(),
            problems,
        }
    }

    #[derive(Default)]
    struct RecordingReporter {
        events: Vec<String>,
    }

    impl Reporter for RecordingReporter {
        fn report(&mut self, event: Event<'_>) {
            match event {
                Event::WatchStarted { destination } => {
                    self.events
                        .push(format!("started:{}", destination.display()));
                }
                Event::WatchSourceChanged { source } => {
                    self.events.push(format!("changed:{}", source.display()));
                }
                Event::NoSamples { problem_index } => {
                    self.events.push(format!("no-samples:{problem_index}"));
                }
                Event::TestCaseLayout { .. } => {}
                Event::TestRunStarted {
                    problem_index,
                    total_cases,
                } => {
                    self.events
                        .push(format!("test-started:{problem_index}:{total_cases}"));
                }
                Event::TestCaseAccepted { number, .. } => {
                    self.events.push(format!("case-accepted:{number}"));
                }
                Event::TestRunFinished {
                    problem_index,
                    accepted,
                    total_cases,
                } => {
                    self.events.push(format!(
                        "test-finished:{problem_index}:{accepted}:{total_cases}"
                    ));
                }
                _ => panic!("unexpected event while testing watch"),
            }
        }
    }

    #[test]
    fn processes_each_concrete_source_in_a_batch_with_its_language() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("workspace with spaces");
        std::fs::create_dir(&destination).unwrap();
        let cpp = destination.join("A.cpp");
        let python = destination.join("A.py");
        std::fs::write(&cpp, "cpp source").unwrap();
        std::fs::write(&python, "python source").unwrap();
        let contest = contest(vec![problem("A")]);
        let mut reporter = RecordingReporter::default();

        reporter.report(Event::WatchStarted {
            destination: &destination,
        });
        process_changed_paths(
            &destination,
            &contest,
            &RunnerConfig::default(),
            vec![cpp.clone(), python.clone()],
            &mut reporter,
        )
        .unwrap();

        assert_eq!(
            reporter.events,
            [
                format!("started:{}", destination.display()),
                format!("changed:{}", cpp.display()),
                "no-samples:A".to_string(),
                format!("changed:{}", python.display()),
                "no-samples:A".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_missing_removed_sources_directories_and_unrelated_files() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path();
        let missing_source = destination.join("A.cpp");
        let source_directory = destination.join("B.py");
        let unrelated = destination.join("B_brute.py");
        std::fs::create_dir(&source_directory).unwrap();
        std::fs::write(&unrelated, "helper").unwrap();
        let contest = contest(vec![problem("A"), problem("B")]);
        let mut reporter = RecordingReporter::default();

        process_changed_paths(
            destination,
            &contest,
            &RunnerConfig::default(),
            vec![missing_source, source_directory, unrelated],
            &mut reporter,
        )
        .unwrap();

        assert!(reporter.events.is_empty());
    }

    #[test]
    fn changed_header_precedes_the_shared_test_run_events() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path();
        let source = destination.join("A.py");
        std::fs::write(&source, "source").unwrap();
        let contest = contest(vec![problem("A")]);
        let mut reporter = RecordingReporter::default();

        process_changed_paths_with(
            destination,
            &contest,
            &RunnerConfig::default(),
            vec![source.clone()],
            &mut reporter,
            |_, problem, language, _, debug, reporter| {
                assert_eq!(problem.index, "A");
                assert_eq!(language, Language::Python);
                assert!(!debug);
                reporter.report(Event::TestRunStarted {
                    problem_index: &problem.index,
                    total_cases: 1,
                });
                reporter.report(Event::TestCaseAccepted {
                    number: 1,
                    elapsed: std::time::Duration::from_millis(1),
                });
                reporter.report(Event::TestRunFinished {
                    problem_index: &problem.index,
                    accepted: 1,
                    total_cases: 1,
                });
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            reporter.events,
            [
                format!("changed:{}", source.display()),
                "test-started:A:1".to_string(),
                "case-accepted:1".to_string(),
                "test-finished:A:1:1".to_string(),
            ]
        );
    }

    #[test]
    fn resolves_only_exact_watched_source_paths() {
        let destination = Path::new("workspace");

        let contest = Contest {
            contest_id: "contest".to_string(),
            problems: vec![problem("A"), problem("D")],
        };

        let sources = build_watched_sources(destination, &contest);

        let cpp = resolve_watched_source(&sources, &destination.join("A.cpp")).unwrap();

        assert_eq!(cpp.problem, 0);
        assert_eq!(cpp.language, Language::Cpp);

        let python = resolve_watched_source(&sources, &destination.join("A.py")).unwrap();

        assert_eq!(python.problem, 0);
        assert_eq!(python.language, Language::Python);

        assert!(resolve_watched_source(&sources, &destination.join("D_brute.py")).is_none());

        assert!(
            resolve_watched_source(&sources, &destination.join("tempCodeRunnerFile.py")).is_none()
        );

        assert!(resolve_watched_source(&sources, &destination.join("A.rs")).is_none());

        assert!(
            resolve_watched_source(&sources, &destination.join("nested").join("A.cpp")).is_none()
        );
    }
}
