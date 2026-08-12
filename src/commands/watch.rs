use super::test::test_problem;
use crate::config::Config;
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

        for path in paths {
            // remove途中など、一時的に存在しない場合は無視
            if !path.is_file() {
                continue;
            }

            let Some((problem, language)) = resolve_watched_source(&cwd, &contest, &path) else {
                continue;
            };

            reporter.report(Event::WatchSourceChanged { source: &path });

            test_problem(&cwd, problem, language, &config.runner, false, reporter)?;
        }
    }
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
