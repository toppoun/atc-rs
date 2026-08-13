use crate::config::Config;
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

use super::watch_worker::TestWorker;
use crate::tui::detail_count::DetailCountWorker;
use crate::tui::message::Message;
use crate::watcher;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};

use super::watch_source::{build_watched_sources, resolve_watched_source};

const WATCHER_POLL_INTERVAL: Duration = Duration::from_millis(20);

fn enable_mouse_capture() -> io::Result<()> {
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnableMouseCapture)
}

fn disable_mouse_capture() -> io::Result<()> {
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, DisableMouseCapture)
}

struct MouseCaptureGuard {
    enabled: bool,
}

impl MouseCaptureGuard {
    fn enable() -> io::Result<Self> {
        if let Err(error) = enable_mouse_capture() {
            // The terminal write may have partially succeeded before returning an error.
            let _ = disable_mouse_capture();
            return Err(error);
        }

        Ok(Self { enabled: true })
    }

    fn disable(&mut self) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let result = disable_mouse_capture();
        if result.is_ok() {
            self.enabled = false;
        }
        result
    }
}

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = disable_mouse_capture();
            self.enabled = false;
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

pub(crate) fn watch_tui() -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    let (contest, sample_counts) = load_watch_input(&cwd)?;

    // workerが使うrunner設定。
    // thread開始前に読み込んでおく。
    let config = Config::load()?;

    // background → TUI の共有Message channel。
    let (message_tx, message_rx) = mpsc::channel();

    // filesystem watcherも、このchannelへ送る。
    let watcher_thread = start_watcher(&cwd, &contest, message_tx.clone())?;

    // test workerも、同じchannelへ結果を送る。
    let test_worker = match TestWorker::start(
        cwd.clone(),
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
    let run_tx = test_worker.sender();

    let mut detail_count_worker = match DetailCountWorker::start() {
        Ok(worker) => worker,
        Err(error) => {
            drop(message_rx);
            drop(message_tx);

            test_worker.request_stop();
            watcher_thread.request_stop();

            let worker_result = test_worker.stop_and_join();
            let watcher_result = watcher_thread.stop();

            worker_result?;
            watcher_result?;

            return Err(error.into());
        }
    };
    let detail_count_tx = detail_count_worker.request_sender();
    let detail_count_rx = detail_count_worker.take_result_receiver();

    // watch_tui自身はMessageを送らない。
    //
    // Senderをここに残すとwatcher/workerが両方終了しても
    // Receiverから見るとchannelがconnectedのままになるのでdropする。
    drop(message_tx);

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,

        Err(error) => {
            drop(message_rx);

            test_worker.request_stop();
            watcher_thread.request_stop();
            detail_count_worker.request_stop();

            ratatui::restore();

            let worker_result = test_worker.stop_and_join();
            let watcher_result = watcher_thread.stop();
            let detail_count_result = detail_count_worker.stop_and_join();

            worker_result?;
            watcher_result?;
            detail_count_result?;

            return Err(error.into());
        }
    };

    let mut mouse_capture = match MouseCaptureGuard::enable() {
        Ok(mouse_capture) => mouse_capture,

        Err(error) => {
            drop(message_rx);

            test_worker.request_stop();
            watcher_thread.request_stop();
            detail_count_worker.request_stop();

            ratatui::restore();

            let worker_result = test_worker.stop_and_join();
            let watcher_result = watcher_thread.stop();
            let detail_count_result = detail_count_worker.stop_and_join();

            worker_result?;
            watcher_result?;
            detail_count_result?;

            return Err(error.into());
        }
    };

    let result = crate::tui::run(
        &mut terminal,
        &contest,
        sample_counts,
        message_rx,
        run_tx,
        detail_count_tx,
        detail_count_rx,
    );

    // test実行中だった場合、runnerまでcancelを先に伝える。
    test_worker.request_stop();
    watcher_thread.request_stop();
    detail_count_worker.request_stop();

    // Mouse Captureもterminal状態の一部なので、restore前に戻す。
    // 失敗してもcleanupは最後まで続行する。
    let mouse_result = mouse_capture.disable();

    // joinが予想外に長引いてもterminalは先に復元する。
    let restore_result = ratatui::try_restore();

    let worker_result = test_worker.stop_and_join();
    let watcher_result = watcher_thread.stop();
    let detail_count_result = detail_count_worker.stop_and_join();

    result?;
    worker_result?;
    watcher_result?;
    detail_count_result?;
    restore_result?;
    mouse_result?;

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
}
