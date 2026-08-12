use super::test::test_problem;
use crate::config::{Config, RunnerConfig};
use crate::error::AppError;
use crate::language::Language;
use crate::model::{Contest, Problem};
use crate::ui::{Event, Reporter};
use crate::{watcher, workspace};
use std::path::Path;

pub(crate) fn watch(reporter: &mut dyn Reporter) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    workspace::validate_workspace_marker(&cwd)?;

    let contest = workspace::load_metadata(&cwd)?;
    workspace::validate_contest_paths(&contest)?;

    let config = Config::load()?;

    let file_watcher = watcher::FileWatcher::new(&cwd)?;

    reporter.report(Event::WatchStarted { destination: &cwd });

    loop {
        let paths = file_watcher.next_batch()?;

        process_changed_paths(&cwd, &contest, &config.runner, paths, reporter)?;
    }
}

fn process_changed_paths(
    destination: &Path,
    contest: &Contest,
    runner_config: &RunnerConfig,
    paths: Vec<std::path::PathBuf>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    for path in paths {
        // remove途中など、一時的に存在しない場合は無視
        if !path.is_file() {
            continue;
        }

        let Some((problem, language)) = resolve_watched_source(destination, contest, &path) else {
            continue;
        };

        reporter.report(Event::WatchSourceChanged { source: &path });

        test_problem(
            destination,
            problem,
            language,
            runner_config,
            false,
            reporter,
        )?;
    }

    Ok(())
}

fn resolve_watched_source<'a>(
    destination: &Path,
    contest: &'a Contest,
    path: &Path,
) -> Option<(&'a Problem, Language)> {
    for problem in &contest.problems {
        for language in [Language::Cpp, Language::Python] {
            let expected = destination.join(format!("{}.{}", problem.index, language.extension()));

            if path == expected {
                return Some((problem, language));
            }
        }
    }

    None
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
                _ => panic!("unexpected event while testing watch"),
            }
        }
    }

    #[test]
    fn resolves_exact_cpp_and_python_sources_from_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("workspace root with spaces");
        std::fs::create_dir(&destination).unwrap();
        let contest = contest(vec![problem("A"), problem("D")]);

        let (cpp_problem, cpp_language) =
            resolve_watched_source(&destination, &contest, &destination.join("A.cpp")).unwrap();
        assert_eq!(cpp_problem.index, "A");
        assert_eq!(cpp_language, Language::Cpp);

        let (python_problem, python_language) =
            resolve_watched_source(&destination, &contest, &destination.join("A.py")).unwrap();
        assert_eq!(python_problem.index, "A");
        assert_eq!(python_language, Language::Python);

        assert!(
            resolve_watched_source(&destination, &contest, &destination.join("D_brute.py"))
                .is_none()
        );
        assert!(
            resolve_watched_source(
                &destination,
                &contest,
                &destination.join("tempCodeRunnerFile.py")
            )
            .is_none()
        );
        assert!(
            resolve_watched_source(&destination, &contest, &destination.join("A.rs")).is_none()
        );
        assert!(
            resolve_watched_source(
                &destination,
                &contest,
                &destination.join("nested").join("A.cpp"),
            )
            .is_none()
        );
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
}
