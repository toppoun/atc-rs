use crate::error::AppError;
use crate::model::Contest;
use crate::workspace;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use crate::tui::message::Message;
use crate::watcher;

use super::watch_source::{build_watched_sources, resolve_watched_source};

fn start_watcher(destination: &Path, contest: &Contest) -> io::Result<mpsc::Receiver<Message>> {
    let watched_sources = build_watched_sources(destination, contest);

    let file_watcher = watcher::FileWatcher::new(destination)?;

    let (tx, rx) = mpsc::channel();

    thread::Builder::new()
        .name("atc-watch-fs".to_string())
        .spawn(move || {
            loop {
                let paths = match file_watcher.next_batch() {
                    Ok(paths) => paths,

                    Err(error) => {
                        let _ = tx.send(Message::WatcherFailed(error));
                        return;
                    }
                };

                for path in paths {
                    if !path.is_file() {
                        continue;
                    }

                    let Some(source) = resolve_watched_source(&watched_sources, &path) else {
                        continue;
                    };

                    let message = Message::SourceChanged {
                        problem: source.problem,
                        path,
                        language: source.language,
                    };

                    if tx.send(message).is_err() {
                        return;
                    }
                }
            }
        })?;

    Ok(rx)
}

pub(crate) fn watch_tui() -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    let (contest, sample_counts) = load_watch_input(&cwd)?;

    let message_rx = start_watcher(&cwd, &contest)?;

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            ratatui::restore();
            return Err(error.into());
        }
    };

    let result = crate::tui::run(&mut terminal, &contest, sample_counts, message_rx);

    let restore_result = ratatui::try_restore();

    result?;
    restore_result?;

    Ok(())
}

fn load_watch_input(destination: &Path) -> Result<(Contest, Vec<usize>), AppError> {
    workspace::validate_workspace_marker(destination)?;

    let contest = workspace::load_metadata(destination)?;
    workspace::validate_contest_paths(&contest)?;

    let sample_counts = contest
        .problems
        .iter()
        .map(|problem| {
            workspace::load_samples(destination, &problem.index).map(|samples| samples.len())
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok((contest, sample_counts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Problem, Sample};

    fn problem(index: &str) -> Problem {
        Problem {
            index: index.to_string(),
            title: format!("Problem {index}"),
            task_id: format!("contest_{}", index.to_ascii_lowercase()),
            url: format!("https://example.invalid/{index}"),
        }
    }

    #[test]
    fn loads_sample_counts_in_metadata_problem_order() {
        let temp = tempfile::tempdir().unwrap();
        let contest = Contest {
            contest_id: "contest".to_string(),
            problems: vec![problem("A"), problem("B"), problem("C")],
        };
        workspace::save_metadata(temp.path(), &contest).unwrap();
        workspace::save_samples(
            temp.path(),
            &contest.problems[0],
            &[
                Sample {
                    input: "1\n".to_string(),
                    output: "2\n".to_string(),
                },
                Sample {
                    input: "3\n".to_string(),
                    output: "4\n".to_string(),
                },
            ],
        )
        .unwrap();
        workspace::save_samples(
            temp.path(),
            &contest.problems[2],
            &[Sample {
                input: "5\n".to_string(),
                output: "6\n".to_string(),
            }],
        )
        .unwrap();

        let (loaded_contest, sample_counts) = load_watch_input(temp.path()).unwrap();

        assert_eq!(loaded_contest.contest_id, "contest");
        assert_eq!(
            loaded_contest
                .problems
                .iter()
                .map(|problem| problem.index.as_str())
                .collect::<Vec<_>>(),
            ["A", "B", "C"]
        );
        assert_eq!(sample_counts, [2, 0, 1]);
    }
}
