use crate::error::AppError;
use crate::model::Contest;
use crate::workspace;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::tui::message::Message;
use crate::watcher;

use super::watch_source::{build_watched_sources, resolve_watched_source};

const WATCHER_POLL_INTERVAL: Duration = Duration::from_millis(20);

struct WatcherThread {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl WatcherThread {
    fn stop(mut self) -> io::Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::Release);
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };

        handle
            .join()
            .map_err(|_| io::Error::other("filesystem watcher thread panicked"))
    }
}

impl Drop for WatcherThread {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn start_watcher(
    destination: &Path,
    contest: &Contest,
) -> io::Result<(mpsc::Receiver<Message>, WatcherThread)> {
    let watched_sources = build_watched_sources(destination, contest);

    let file_watcher = watcher::FileWatcher::new(destination)?;

    let (tx, rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);

    let handle = thread::Builder::new()
        .name("atc-watch-fs".to_string())
        .spawn(move || {
            while !thread_shutdown.load(Ordering::Acquire) {
                let paths = match file_watcher.next_batch_timeout(WATCHER_POLL_INTERVAL) {
                    Ok(Some(paths)) => paths,
                    Ok(None) => continue,

                    Err(error) => {
                        let _ = tx.send(Message::WatcherFailed(error));
                        return;
                    }
                };

                if !send_source_changes(paths, &watched_sources, &tx) {
                    return;
                }
            }
        })?;

    Ok((
        rx,
        WatcherThread {
            shutdown,
            handle: Some(handle),
        },
    ))
}

fn send_source_changes(
    paths: Vec<std::path::PathBuf>,
    watched_sources: &[super::watch_source::WatchedSource],
    tx: &mpsc::Sender<Message>,
) -> bool {
    for path in paths {
        if !path.is_file() {
            continue;
        }

        let Some(source) = resolve_watched_source(watched_sources, &path) else {
            continue;
        };

        let message = Message::SourceChanged {
            problem: source.problem,
            path,
            language: source.language,
        };

        if tx.send(message).is_err() {
            return false;
        }
    }

    true
}

pub(crate) fn watch_tui() -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    let (contest, sample_counts) = load_watch_input(&cwd)?;

    let (message_rx, watcher_thread) = start_watcher(&cwd, &contest)?;

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            drop(message_rx);
            let watcher_result = watcher_thread.stop();
            ratatui::restore();
            watcher_result?;
            return Err(error.into());
        }
    };

    let result = crate::tui::run(&mut terminal, &contest, sample_counts, message_rx);
    let watcher_result = watcher_thread.stop();
    let restore_result = ratatui::try_restore();

    result?;
    watcher_result?;
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

    #[test]
    fn sends_only_existing_exact_sources_and_keeps_metadata_positions() {
        let temp = tempfile::tempdir().unwrap();
        let contest = Contest {
            contest_id: "contest".to_string(),
            problems: vec![problem("B"), problem("A")],
        };
        let watched_sources = build_watched_sources(temp.path(), &contest);
        let b_cpp = temp.path().join("B.cpp");
        let a_py = temp.path().join("A.py");
        let helper = temp.path().join("A_brute.py");
        let nested = temp.path().join("nested").join("A.cpp");
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        for path in [&b_cpp, &a_py, &helper, &nested] {
            std::fs::write(path, "source").unwrap();
        }
        let (tx, rx) = mpsc::channel();

        assert!(send_source_changes(
            vec![helper, nested, b_cpp.clone(), a_py.clone()],
            &watched_sources,
            &tx,
        ));

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            Message::SourceChanged { problem: 0, path, language: crate::language::Language::Cpp }
                if path == &b_cpp
        ));
        assert!(matches!(
            &messages[1],
            Message::SourceChanged { problem: 1, path, language: crate::language::Language::Python }
                if path == &a_py
        ));
    }

    #[test]
    fn prebuilt_mapping_recognizes_a_source_created_later() {
        let temp = tempfile::tempdir().unwrap();
        let contest = Contest {
            contest_id: "contest".to_string(),
            problems: vec![problem("A")],
        };
        let watched_sources = build_watched_sources(temp.path(), &contest);
        let source = temp.path().join("A.py");
        assert!(!source.exists());
        std::fs::write(&source, "source").unwrap();
        let (tx, rx) = mpsc::channel();

        assert!(send_source_changes(
            vec![source.clone()],
            &watched_sources,
            &tx,
        ));

        assert!(matches!(
            rx.try_recv().unwrap(),
            Message::SourceChanged { problem: 0, path, language: crate::language::Language::Python }
                if path == source
        ));
    }

    #[test]
    fn watcher_thread_stop_joins_the_thread() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Acquire) {
                thread::yield_now();
            }
        });
        let worker = WatcherThread {
            shutdown,
            handle: Some(handle),
        };

        worker.stop().unwrap();
    }

    #[test]
    fn watcher_thread_panic_is_an_error() {
        let worker = WatcherThread {
            shutdown: Arc::new(AtomicBool::new(false)),
            handle: Some(thread::spawn(|| panic!("watcher panic"))),
        };

        let error = worker.stop().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("panicked"));
    }
}
