use std::io;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, Sender},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::config::RunnerConfig;
use crate::model::Problem;
use crate::tui::message::{Message, RunRequest};
use crate::tui::reporter::ChannelReporter;

use super::test::test_problem_with_cancel;

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(super) struct TestWorker {
    request_tx: Sender<RunRequest>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

fn worker_loop(
    request_rx: Receiver<RunRequest>,
    shutdown: Arc<AtomicBool>,
    mut handle_run: impl FnMut(RunRequest, &AtomicBool) + Send + 'static,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        match request_rx.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(mut request) => {
                // 実行開始前にqueueへ溜まっているrequestを全部読む。
                // 古いrequestは捨てて、最新の1件だけ実行する。
                while let Ok(newer_request) = request_rx.try_recv() {
                    request = newer_request;
                }

                handle_run(request, &shutdown);
            }

            Err(RecvTimeoutError::Timeout) => {}

            Err(RecvTimeoutError::Disconnected) => {
                return;
            }
        }
    }
}

impl TestWorker {
    pub fn start(
        destination: PathBuf,
        problems: Vec<Problem>,
        runner_config: RunnerConfig,
        message_tx: Sender<Message>,
    ) -> io::Result<Self> {
        Self::start_with(move |request, shutdown| {
            let Some(problem) = problems.get(request.problem) else {
                let _ = message_tx.send(Message::RunFailed {
                    run_id: request.run_id,
                    problem: request.problem,
                    error: format!("invalid problem index: {}", request.problem),
                });

                return;
            };

            if message_tx
                .send(Message::RunStarted {
                    run_id: request.run_id,
                    problem: request.problem,
                })
                .is_err()
            {
                return;
            }

            let mut reporter =
                ChannelReporter::new(request.run_id, request.problem, message_tx.clone());

            let result = test_problem_with_cancel(
                &destination,
                problem,
                request.language,
                &runner_config,
                request.debug,
                &mut reporter,
                &|| shutdown.load(Ordering::Relaxed),
            );

            match result {
                Ok(()) => {
                    let _ = message_tx.send(Message::RunCompleted {
                        run_id: request.run_id,
                        problem: request.problem,
                    });
                }

                Err(_error) if shutdown.load(Ordering::Relaxed) => {
                    // TUI終了によるキャンセル。
                    // 終了時なのでRunFailedは送らない。
                }

                Err(error) => {
                    let _ = message_tx.send(Message::RunFailed {
                        run_id: request.run_id,
                        problem: request.problem,
                        error: error.to_string(),
                    });
                }
            }
        })
    }

    fn start_with(
        handle_run: impl FnMut(RunRequest, &AtomicBool) + Send + 'static,
    ) -> io::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel();

        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_shutdown = Arc::clone(&shutdown);

        let handle = thread::Builder::new()
            .name("atc-watch-test".to_string())
            .spawn(move || {
                worker_loop(request_rx, thread_shutdown, handle_run);
            })?;

        Ok(Self {
            request_tx,
            shutdown,
            handle: Some(handle),
        })
    }

    pub fn sender(&self) -> Sender<RunRequest> {
        self.request_tx.clone()
    }

    pub fn stop_and_join(mut self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);

        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| io::Error::other("test worker thread panicked"))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;
    use std::sync::mpsc;

    #[test]
    fn worker_receives_run_request() {
        let (seen_tx, seen_rx) = mpsc::channel();

        let worker = TestWorker::start_with(move |request, _shutdown| {
            let _ = seen_tx.send(request);
        })
        .unwrap();

        let run_tx = worker.sender();

        run_tx
            .send(RunRequest {
                run_id: 7,
                problem: 2,
                language: Language::Cpp,
                debug: true,
            })
            .unwrap();

        let received = seen_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert_eq!(received.run_id, 7);
        assert_eq!(received.problem, 2);
        assert_eq!(received.language, Language::Cpp);
        assert!(received.debug);

        worker.stop_and_join().unwrap();
    }

    #[test]
    fn idle_worker_stops_without_waiting_for_a_request() {
        let worker = TestWorker::start_with(|_, _| {}).unwrap();

        worker.stop_and_join().unwrap();
    }
    #[test]
    fn worker_keeps_only_latest_queued_request() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (seen_tx, seen_rx) = mpsc::channel();

        let mut first = true;

        let worker = TestWorker::start_with(move |request, _shutdown| {
            if first {
                first = false;

                started_tx.send(()).unwrap();

                // 最初のrunをここで止めて、
                // その間に複数requestをqueueへ積ませる。
                release_rx.recv().unwrap();
            }

            seen_tx.send(request).unwrap();
        })
        .unwrap();

        let run_tx = worker.sender();

        run_tx
            .send(RunRequest {
                run_id: 1,
                problem: 0,
                language: Language::Cpp,
                debug: false,
            })
            .unwrap();

        // #1がworkerに入ったことを確認
        started_rx.recv().unwrap();

        for run_id in 2..=5 {
            run_tx
                .send(RunRequest {
                    run_id,
                    problem: 0,
                    language: Language::Cpp,
                    debug: false,
                })
                .unwrap();
        }

        // #1を再開
        release_tx.send(()).unwrap();

        let first_seen = seen_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let latest_seen = seen_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert_eq!(first_seen.run_id, 1);
        assert_eq!(latest_seen.run_id, 5);

        assert!(seen_rx.recv_timeout(Duration::from_millis(100)).is_err());

        worker.stop_and_join().unwrap();
    }
}
