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
    let watched_sources = build_watched_sources(destination, contest)?;

    for path in paths {
        if !path.is_file() {
            continue;
        }

        let Some(source) = resolve_watched_source(&watched_sources, &path) else {
            continue;
        };

        let problem = &contest.problems[source.problem];

        reporter.report(Event::WatchSourceChanged { source: &path });

        if let Err(error) = run_test(
            destination,
            problem,
            source.language,
            runner_config,
            false,
            reporter,
        ) {
            if !matches!(&error, AppError::Io(error) if crate::stress::is_problem_load_error(error))
            {
                return Err(error);
            }
            reporter.report(Event::WatchTestFailed {
                problem_index: &problem.index,
                error: &error.to_string(),
            });
        }
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
            url: format!(
                "https://atcoder.jp/contests/contest/tasks/contest_{}",
                index.to_ascii_lowercase()
            ),
            sample_count: 0,
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
                Event::WatchTestFailed {
                    problem_index,
                    error,
                } => {
                    self.events.push(format!("failed:{problem_index}:{error}"));
                }
                Event::NoSamples { problem_index } => {
                    self.events.push(format!("no-samples:{problem_index}"));
                }
                Event::SavedStressCaseSkipped {
                    problem_index,
                    reason,
                } => self
                    .events
                    .push(format!("stress-skipped:{problem_index}:{reason}")),
                Event::CompileDiagnostics {
                    problem_index,
                    stderr,
                } => self
                    .events
                    .push(format!("compiler:{problem_index}:{stderr}")),
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
        workspace::save_metadata(&destination, &contest).unwrap();
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
    fn changed_source_reports_missing_official_samples_before_running() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("A.cpp");
        std::fs::write(&source, "not valid C++").unwrap();
        let mut problem = problem("A");
        problem.sample_count = 3;
        let contest = contest(vec![problem]);
        workspace::save_metadata(temp.path(), &contest).unwrap();
        let mut reporter = RecordingReporter::default();

        let error = process_changed_paths(
            temp.path(),
            &contest,
            &RunnerConfig {
                cpp_compiler: "definitely-not-a-real-compiler".to_string(),
                ..RunnerConfig::default()
            },
            vec![source.clone()],
            &mut reporter,
        )
        .unwrap_err();

        assert!(error.to_string().contains("local samples"));
        assert_eq!(reporter.events, [format!("changed:{}", source.display())]);
    }

    #[test]
    fn watch_rechecks_contest_recovery_after_startup() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path();
        let source = destination.join("A.py");
        std::fs::write(&source, "pass\n").unwrap();
        workspace::save_metadata(destination, &contest(vec![problem("A")])).unwrap();
        let contest = workspace::load_metadata(destination).unwrap();
        let staging = destination.join(".atc-refresh-interrupted");
        std::fs::create_dir(&staging).unwrap();
        crate::safe_file::write_recovery_intent(
            &staging,
            &crate::safe_file::RecoveryIdentity::new("contest", "contest", None),
        )
        .unwrap();
        let mut reporter = RecordingReporter::default();

        let error = process_changed_paths(
            destination,
            &contest,
            &RunnerConfig::default(),
            vec![source.clone()],
            &mut reporter,
        )
        .unwrap_err();

        assert!(error.to_string().contains("requires recovery check"));
        assert_eq!(reporter.events, [format!("changed:{}", source.display())]);
    }

    #[test]
    fn watch_continues_to_another_problem_after_a_broken_stress_case() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path();
        let a = destination.join("A.py");
        let b = destination.join("B.py");
        std::fs::write(&a, "pass\n").unwrap();
        std::fs::write(&b, "print(input())\n").unwrap();
        let mut problem_b = problem("B");
        problem_b.sample_count = 1;
        workspace::save_samples(
            destination,
            &problem_b,
            &[crate::model::Sample {
                input: "42\n".into(),
                output: "42\n".into(),
            }],
        )
        .unwrap();
        let contest = contest(vec![problem("A"), problem_b]);
        workspace::save_metadata(destination, &contest).unwrap();
        std::fs::create_dir_all(destination.join(".atc/stress/A")).unwrap();
        let mut reporter = RecordingReporter::default();

        process_changed_paths(
            destination,
            &contest,
            &RunnerConfig {
                python: ["python3", "python"]
                    .into_iter()
                    .find(|python| {
                        std::process::Command::new(python)
                            .arg("--version")
                            .output()
                            .is_ok_and(|output| output.status.success())
                    })
                    .expect("watch test requires Python")
                    .into(),
                ..RunnerConfig::default()
            },
            vec![a.clone(), b.clone()],
            &mut reporter,
        )
        .unwrap();

        assert_eq!(reporter.events[0], format!("changed:{}", a.display()));
        assert!(reporter.events[1].contains(".atc/stress/A"));
        assert!(reporter.events[1].contains("meta.toml"));
        assert_eq!(reporter.events[2], format!("changed:{}", b.display()));
        assert_eq!(
            reporter.events[3..],
            ["test-started:B:1", "case-accepted:1", "test-finished:B:1:1"]
        );
    }

    #[test]
    fn changed_source_reports_unreplayable_legacy_stress_case_without_counting_it() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("A.py");
        std::fs::write(&source, "print(input())\n").unwrap();
        let saved = temp.path().join(".atc/stress/A");
        std::fs::create_dir_all(&saved).unwrap();
        std::fs::write(saved.join("failed.in"), "42\n").unwrap();
        std::fs::write(saved.join("actual.out"), "runtime error\n").unwrap();
        std::fs::write(
            saved.join("meta.toml"),
            concat!(
                "version = 1\ncontest = \"contest\"\nproblem = \"A\"\n",
                "kind = \"runtime-error\"\ncase = 1\nbase_seed = 10\nseed = 10\n"
            ),
        )
        .unwrap();
        let contest = contest(vec![problem("A")]);
        workspace::save_metadata(temp.path(), &contest).unwrap();
        let mut reporter = RecordingReporter::default();

        process_changed_paths(
            temp.path(),
            &contest,
            &RunnerConfig::default(),
            vec![source.clone()],
            &mut reporter,
        )
        .unwrap();
        assert_eq!(reporter.events.len(), 3);
        assert_eq!(reporter.events[0], format!("changed:{}", source.display()));
        assert!(reporter.events[1].contains("stress-skipped:A:v1 runtime-error"));
        assert!(reporter.events[1].contains("unverified"));
        assert_eq!(reporter.events[2], "no-samples:A");
        assert!(!saved.join("expected.out").exists());
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
    fn watch_reports_diagnostics_only_for_the_recompile_that_produced_them() {
        let Some(cpp_compiler) = crate::commands::tests::available_cpp_compiler() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path();
        let source = destination.join("A.cpp");
        let mut problem = problem("A");
        problem.sample_count = 1;
        workspace::save_samples(
            destination,
            &problem,
            &[crate::model::Sample {
                input: String::new(),
                output: String::new(),
            }],
        )
        .unwrap();
        let contest = contest(vec![problem]);
        workspace::save_metadata(destination, &contest).unwrap();
        let runner = RunnerConfig {
            cpp_compiler,
            compile_timeout_seconds: 60.0,
            timeout_seconds: 15.0,
            ..RunnerConfig::default()
        };
        let mut reporter = RecordingReporter::default();

        std::fs::write(
            &source,
            "#pragma message(\"atc watch diagnostic\")\nint main() {}\n",
        )
        .unwrap();
        process_changed_paths(
            destination,
            &contest,
            &runner,
            vec![source.clone()],
            &mut reporter,
        )
        .unwrap();
        let events = &reporter.events;
        assert_eq!(events[0], format!("changed:{}", source.display()));
        assert!(
            events[1].starts_with("compiler:A:") && events[1].contains("atc watch diagnostic"),
            "{events:?}"
        );
        assert_eq!(
            events[2..],
            ["test-started:A:1", "case-accepted:1", "test-finished:A:1:1"]
        );

        reporter.events.clear();
        std::fs::write(&source, "int main() {}\n").unwrap();
        process_changed_paths(
            destination,
            &contest,
            &runner,
            vec![source.clone()],
            &mut reporter,
        )
        .unwrap();
        assert_eq!(reporter.events[0], format!("changed:{}", source.display()));
        assert_eq!(
            reporter.events[1..],
            ["test-started:A:1", "case-accepted:1", "test-finished:A:1:1"]
        );
    }

    #[test]
    fn resolves_only_exact_watched_source_paths() {
        let destination = Path::new("workspace");

        let contest = Contest {
            contest_id: "contest".to_string(),
            problems: vec![problem("A"), problem("D")],
        };

        let sources = build_watched_sources(destination, &contest).unwrap();

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
