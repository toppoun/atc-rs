use crate::config::Config;
use crate::error::AppError;
use crate::model::Contest;
use crate::workspace;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::watch_worker::RunWorker;
use crate::tui::detail_analysis::DetailAnalysisWorker;
use crate::tui::message::Message;
use crate::watcher;

use super::watch_source::{build_watched_sources, resolve_watched_source};

const WATCHER_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug)]
struct WatchCleanupError {
    primary_name: Option<&'static str>,
    primary: io::Error,
    cleanup: Vec<(&'static str, io::Error)>,
}

impl fmt::Display for WatchCleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = self.primary_name {
            write!(formatter, "{name} failed: {}", self.primary)?;
        } else {
            write!(formatter, "{}", self.primary)?;
        }
        for (name, error) in &self.cleanup {
            write!(formatter, "; {name} also failed: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for WatchCleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.primary)
    }
}

fn with_cleanup_errors(
    primary_name: Option<&'static str>,
    primary: io::Error,
    cleanup: Vec<(&'static str, io::Error)>,
) -> io::Error {
    io::Error::new(
        primary.kind(),
        WatchCleanupError {
            primary_name,
            primary,
            cleanup,
        },
    )
}

fn combine_primary_and_cleanup_results<const N: usize>(
    primary: io::Result<()>,
    cleanup_results: [(&'static str, io::Result<()>); N],
) -> io::Result<()> {
    let cleanup_errors = cleanup_results
        .into_iter()
        .filter_map(|(name, result)| result.err().map(|error| (name, error)))
        .collect::<Vec<_>>();

    match primary {
        Err(primary) if cleanup_errors.is_empty() => Err(primary),
        Err(primary) => Err(with_cleanup_errors(None, primary, cleanup_errors)),
        Ok(()) => {
            let mut errors = cleanup_errors.into_iter();
            let Some((first_name, first_error)) = errors.next() else {
                return Ok(());
            };
            let Some(second) = errors.next() else {
                return Err(first_error);
            };

            let cleanup = std::iter::once(second).chain(errors).collect();
            Err(with_cleanup_errors(Some(first_name), first_error, cleanup))
        }
    }
}

struct WatcherThread {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl WatcherThread {
    fn request_stop(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    fn join(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };

        handle.join().map_err(|_| {
            io::Error::other("filesystem watcher thread panicked before reporting")
        })??;
        Ok(())
    }

    fn stop(mut self) -> io::Result<()> {
        self.request_stop();
        self.join()
    }
}

impl Drop for WatcherThread {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.join();
    }
}

fn start_watcher(
    destination: &Path,
    contest: &Contest,
    tx: mpsc::Sender<Message>,
) -> io::Result<WatcherThread> {
    let watched_sources = build_watched_sources(destination, contest);

    let file_watcher = watcher::FileWatcher::new(destination)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let panic_tx = tx.clone();

    let handle = thread::Builder::new()
        .name("atc-watch-fs".to_string())
        .spawn(move || {
            run_watcher_guarded(panic_tx, || {
                while !thread_shutdown.load(Ordering::Acquire) {
                    let paths = match file_watcher
                        .next_batch_timeout_with_cancel(WATCHER_POLL_INTERVAL, &|| {
                            thread_shutdown.load(Ordering::Acquire)
                        }) {
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
            })
        })?;

    Ok(WatcherThread {
        shutdown,
        handle: Some(handle),
    })
}

fn run_watcher_guarded(panic_tx: mpsc::Sender<Message>, run: impl FnOnce()) -> io::Result<()> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    if result.is_ok() {
        return Ok(());
    }

    let error = io::Error::other("filesystem watcher thread panicked");
    let _ = panic_tx.send(Message::WatcherFailed(io::Error::new(
        error.kind(),
        error.to_string(),
    )));
    Err(error)
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

pub(crate) fn watch_tui(cli_contest: Option<&str>) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::resolve_contest_target(&cwd, cli_contest)?;

    watch_tui_at(&destination, cli_contest)
}

pub(super) fn watch_tui_at(
    destination: &Path,
    expected_contest_id: Option<&str>,
) -> Result<(), AppError> {
    let (contest, sample_counts, stress_cases) =
        load_watch_input(destination, expected_contest_id)?;

    // workerが使うrunner設定。
    // thread開始前に読み込んでおく。
    let config = Config::load()?;

    // background → TUI の共有Message channel。
    let (message_tx, message_rx) = mpsc::channel();

    // filesystem watcherも、このchannelへ送る。
    let watcher_thread = start_watcher(destination, &contest, message_tx.clone())?;

    // run workerも、同じchannelへ結果を送る。
    let run_worker = match RunWorker::start(
        destination.to_path_buf(),
        contest.contest_id.clone(),
        contest.problems.clone(),
        config.runner,
        message_tx.clone(),
    ) {
        Ok(worker) => worker,

        Err(error) => {
            drop(message_rx);
            drop(message_tx);

            let watcher_result = watcher_thread.stop();

            watcher_result?;

            return Err(error.into());
        }
    };
    let run_tx = run_worker.sender();

    let mut detail_analysis_worker = match DetailAnalysisWorker::start() {
        Ok(worker) => worker,
        Err(error) => {
            drop(message_rx);
            drop(message_tx);

            run_worker.request_stop();
            watcher_thread.request_stop();

            let worker_result = run_worker.stop_and_join();
            let watcher_result = watcher_thread.stop();

            worker_result?;
            watcher_result?;

            return Err(error.into());
        }
    };
    let detail_analysis_tx = detail_analysis_worker.request_sender();
    let detail_analysis_rx = detail_analysis_worker.take_result_receiver();

    // watch_tui自身はMessageを送らない。
    //
    // Senderをここに残すとwatcher/workerが両方終了しても
    // Receiverから見るとchannelがconnectedのままになるのでdropする。
    drop(message_tx);

    let mut terminal = match crate::tui::TerminaSession::start() {
        Ok(terminal) => terminal,

        Err(error) => {
            drop(message_rx);

            run_worker.request_stop();
            watcher_thread.request_stop();
            detail_analysis_worker.request_stop();

            let worker_result = run_worker.stop_and_join();
            let watcher_result = watcher_thread.stop();
            let detail_analysis_result = detail_analysis_worker.stop_and_join();

            return combine_primary_and_cleanup_results(
                Err(error),
                [
                    ("run worker shutdown", worker_result),
                    ("filesystem watcher shutdown", watcher_result),
                    ("detail analysis worker shutdown", detail_analysis_result),
                ],
            )
            .map_err(AppError::from);
        }
    };

    let result = crate::tui::run(
        &mut terminal,
        &contest,
        sample_counts,
        stress_cases,
        message_rx,
        run_tx,
        detail_analysis_tx,
        detail_analysis_rx,
    );

    // sample/stress実行中だった場合、runnerまでcancelを先に伝える。
    run_worker.request_stop();
    watcher_thread.request_stop();
    detail_analysis_worker.request_stop();

    // joinが予想外に長引いてもterminalは先に復元する。
    let restore_result = terminal.restore();
    // TerminaのDropで元のplatform mode/code pageまで戻してからjoinする。
    drop(terminal);

    let worker_result = run_worker.stop_and_join();
    let watcher_result = watcher_thread.stop();
    let detail_analysis_result = detail_analysis_worker.stop_and_join();

    combine_primary_and_cleanup_results(
        result,
        [
            ("terminal restoration", restore_result),
            ("run worker shutdown", worker_result),
            ("filesystem watcher shutdown", watcher_result),
            ("detail analysis worker shutdown", detail_analysis_result),
        ],
    )
    .map_err(AppError::from)
}

fn load_watch_input(
    destination: &Path,
    expected_contest_id: Option<&str>,
) -> Result<(Contest, Vec<usize>, Vec<Option<crate::model::Sample>>), AppError> {
    workspace::validate_workspace_marker(destination)?;

    let contest = workspace::load_metadata(destination)?;
    workspace::validate_contest_paths(&contest)?;
    if let Some(contest_id) = expected_contest_id {
        workspace::validate_contest_identity(&contest, contest_id)?;
    }

    let sample_counts = contest
        .problems
        .iter()
        .map(|problem| {
            workspace::load_samples(destination, &problem.index).map(|samples| samples.len())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let stress_cases = contest
        .problems
        .iter()
        .map(|problem| {
            crate::stress::load_saved_case(destination, &contest.contest_id, &problem.index)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok((contest, sample_counts, stress_cases))
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
        let stress_a = temp.path().join(".atc").join("stress").join("A");
        std::fs::create_dir_all(&stress_a).unwrap();
        std::fs::write(stress_a.join("failed.in"), "7\n").unwrap();
        std::fs::write(stress_a.join("actual.out"), "9\n").unwrap();
        std::fs::write(stress_a.join("expected.out"), "8\n").unwrap();
        std::fs::write(
            stress_a.join("meta.toml"),
            "version = 1\ncontest = \"contest\"\nproblem = \"A\"\nkind = \"wrong-answer\"\ncase = 1\nbase_seed = 10\nseed = 10\n",
        )
        .unwrap();

        let (loaded_contest, sample_counts, stress_cases) =
            load_watch_input(temp.path(), None).unwrap();

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
        assert_eq!(
            stress_cases,
            [
                Some(Sample {
                    input: "7\n".to_string(),
                    output: "8\n".to_string(),
                }),
                None,
                None,
            ]
        );
    }

    #[test]
    fn requested_contest_id_must_match_loaded_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let contest = Contest {
            contest_id: "arc001".to_string(),
            problems: Vec::new(),
        };
        workspace::save_metadata(temp.path(), &contest).unwrap();

        let error = load_watch_input(temp.path(), Some("abc466"))
            .expect_err("resolved destination metadata must match the requested contest");

        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == io::ErrorKind::InvalidData
        ));
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
            Ok(())
        });
        let worker = WatcherThread {
            shutdown,
            handle: Some(handle),
        };

        worker.stop().unwrap();
    }

    #[test]
    fn watcher_thread_panic_is_an_error() {
        let (tx, rx) = mpsc::channel();
        let worker = WatcherThread {
            shutdown: Arc::new(AtomicBool::new(false)),
            handle: Some(thread::spawn(move || {
                run_watcher_guarded(tx, || panic!("watcher panic"))
            })),
        };

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::WatcherFailed(error) if error.to_string().contains("panicked")
        ));

        let error = worker.stop().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("panicked"));
    }

    #[test]
    fn terminal_operation_error_remains_primary_when_cleanup_also_fails() {
        let error = combine_primary_and_cleanup_results(
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input failed",
            )),
            [
                (
                    "terminal restoration",
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "mouse reset failed",
                    )),
                ),
                ("worker shutdown", Ok(())),
            ],
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(error.to_string().starts_with("terminal input failed;"));
        assert!(
            error
                .to_string()
                .contains("terminal restoration also failed")
        );
        assert!(error.to_string().contains("mouse reset failed"));
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            "terminal input failed"
        );
    }

    #[test]
    fn cleanup_failure_is_returned_after_successful_terminal_run() {
        let error = combine_primary_and_cleanup_results(
            Ok(()),
            [
                ("terminal restoration", Ok(())),
                (
                    "worker shutdown",
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "worker did not stop",
                    )),
                ),
            ],
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "worker did not stop");
    }

    #[test]
    fn multiple_cleanup_failures_are_all_reported_in_execution_order() {
        let error = combine_primary_and_cleanup_results(
            Ok(()),
            [
                (
                    "terminal restoration",
                    Err(io::Error::other("cursor restore failed")),
                ),
                (
                    "worker shutdown",
                    Err(io::Error::other("worker join failed")),
                ),
            ],
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "terminal restoration failed: cursor restore failed; worker shutdown also failed: worker join failed"
        );
    }
}
