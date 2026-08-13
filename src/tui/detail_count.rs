use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::detail::DetailTextSource;
use super::detail_layout::{DetailCountCommand, DetailCountRequest, DetailCountResult};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(crate) struct DetailCountWorker {
    request_tx: Sender<DetailCountCommand>,
    result_rx: Option<Receiver<DetailCountResult>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DetailCountWorker {
    pub(crate) fn start() -> io::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let handle = thread::Builder::new()
            .name("atc-detail-count".to_string())
            .spawn(move || {
                worker_loop(
                    request_rx,
                    result_tx,
                    worker_shutdown,
                    |request, is_cancelled| count_request(request, is_cancelled),
                );
            })?;

        Ok(Self {
            request_tx,
            result_rx: Some(result_rx),
            shutdown,
            handle: Some(handle),
        })
    }

    pub(crate) fn request_sender(&self) -> Sender<DetailCountCommand> {
        self.request_tx.clone()
    }

    pub(crate) fn take_result_receiver(&mut self) -> Receiver<DetailCountResult> {
        self.result_rx
            .take()
            .expect("detail count result receiver may only be taken once")
    }

    pub(crate) fn request_stop(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.request_tx.send(DetailCountCommand::Shutdown);
    }

    pub(crate) fn stop_and_join(mut self) -> io::Result<()> {
        self.request_stop();
        self.join()
    }

    fn join(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };

        handle
            .join()
            .map_err(|_| io::Error::other("detail count worker thread panicked"))
    }

    #[cfg(test)]
    fn start_panicking() -> Self {
        let (request_tx, _request_rx) = mpsc::channel();
        let (_result_tx, result_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn(|| panic!("test detail count worker panic"));

        Self {
            request_tx,
            result_rx: Some(result_rx),
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for DetailCountWorker {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.join();
    }
}

fn count_request(
    request: DetailCountRequest,
    is_cancelled: &mut dyn FnMut() -> bool,
) -> Option<DetailCountResult> {
    let segment_lengths = (0..request.snapshot.segment_count())
        .map(|index| request.snapshot.segment_text(index).map_or(0, str::len))
        .collect::<Vec<_>>();

    if segment_lengths != request.identity.segment_lengths
        || request.line_index.chunk_count() != request.identity.chunk_count
    {
        return None;
    }

    let chunk_visual_lines = request.line_index.count_chunks(
        &request.snapshot,
        request.identity.layout_width,
        is_cancelled,
    )?;

    Some(DetailCountResult {
        identity: request.identity,
        chunk_visual_lines,
    })
}

fn worker_loop(
    request_rx: Receiver<DetailCountCommand>,
    result_tx: Sender<DetailCountResult>,
    shutdown: Arc<AtomicBool>,
    mut count: impl FnMut(DetailCountRequest, &mut dyn FnMut() -> bool) -> Option<DetailCountResult>,
) {
    let mut pending = None;

    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }

        let command = match pending.take() {
            Some(command) => command,
            None => match request_rx.recv_timeout(WORKER_POLL_INTERVAL) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        let command = drain_latest(&request_rx, command);

        match command {
            DetailCountCommand::Shutdown => return,
            DetailCountCommand::Cancel { generation } => {
                let _ = generation;
            }
            DetailCountCommand::Count(request) => {
                let mut replacement = None;
                let result = {
                    let mut is_cancelled = || {
                        if shutdown.load(Ordering::Acquire) {
                            return true;
                        }

                        if let Some(command) = try_drain_latest(&request_rx) {
                            replacement = Some(command);
                            return true;
                        }

                        false
                    };

                    count(request, &mut is_cancelled)
                };

                if replacement.is_none() {
                    replacement = try_drain_latest(&request_rx);
                }

                if let Some(command) = replacement {
                    pending = Some(command);
                    continue;
                }

                if shutdown.load(Ordering::Acquire) {
                    return;
                }

                if let Some(result) = result
                    && result_tx.send(result).is_err()
                {
                    return;
                }
            }
        }
    }
}

fn drain_latest(
    request_rx: &Receiver<DetailCountCommand>,
    initial: DetailCountCommand,
) -> DetailCountCommand {
    let mut latest = initial;

    loop {
        match request_rx.try_recv() {
            Ok(DetailCountCommand::Shutdown) => return DetailCountCommand::Shutdown,
            Ok(command) => latest = command,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return latest,
        }
    }
}

fn try_drain_latest(request_rx: &Receiver<DetailCountCommand>) -> Option<DetailCountCommand> {
    match request_rx.try_recv() {
        Ok(command) => Some(drain_latest(request_rx, command)),
        Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::detail::DetailDocument;
    use crate::tui::detail_layout::{
        DetailCountCommand, DetailCountRequest, DetailLayout, wrap_detail_document,
    };
    use std::sync::Mutex;

    fn request(raw: &Arc<String>, generation: u64, width: u16) -> DetailCountRequest {
        let segments = [raw];
        let document = DetailDocument::from_shared_segments(&segments);
        let mut layout = DetailLayout::default();
        layout.viewport(&document, generation, width, 20, 0);
        layout.stage_count_command(&document);

        let Some(DetailCountCommand::Count(mut request)) = layout.take_count_command() else {
            panic!("large test document must stage a count request");
        };
        request.identity.generation = generation;
        request
    }

    fn synthetic_result(request: DetailCountRequest) -> DetailCountResult {
        DetailCountResult {
            chunk_visual_lines: vec![1; request.identity.chunk_count],
            identity: request.identity,
        }
    }

    #[test]
    fn idle_worker_stops_and_joins() {
        DetailCountWorker::start().unwrap().stop_and_join().unwrap();
    }

    #[test]
    fn worker_panic_is_reported_by_join() {
        let error = DetailCountWorker::start_panicking()
            .stop_and_join()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("panicked"));
    }

    #[test]
    fn count_request_shares_raw_buffer_and_returns_only_chunk_counts() {
        let raw = Arc::new("line\n".repeat(100_000));
        let owners = Arc::strong_count(&raw);
        let request = request(&raw, 1, 80);

        assert!(request.snapshot.shares_buffer(&raw));
        assert_eq!(Arc::strong_count(&raw), owners + 1);

        let result = count_request(request, &mut || false).unwrap();
        assert_eq!(result.chunk_visual_lines.len(), result.identity.chunk_count);
        assert_eq!(result.chunk_visual_lines.iter().sum::<usize>(), 100_001);
        assert_eq!(Arc::strong_count(&raw), owners);
    }

    #[test]
    fn count_only_wrap_matches_the_ui_wrap_for_unicode_and_segment_boundaries() {
        let raw = Arc::new(
            [
                "ASCII words and spaces\n",
                "supercalifragilisticexpialidocious\n",
                "日本語 e\u{301} 👩‍💻 \u{200b}\n\n",
                "trailing\n",
            ]
            .concat()
            .repeat(3_000),
        );
        let request = request(&raw, 1, 13);
        let layout_width = request.identity.layout_width;
        let reference_height =
            wrap_detail_document(&request.snapshot, u16::try_from(layout_width).unwrap()).height();

        let result = count_request(request, &mut || false).unwrap();
        assert_eq!(
            result.chunk_visual_lines.iter().sum::<usize>(),
            reference_height
        );
    }

    #[test]
    fn waiting_requests_are_latest_wins() {
        let raw = Arc::new("line\n".repeat(3_000));
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let visited = Arc::new(Mutex::new(Vec::new()));
        let thread_visited = Arc::clone(&visited);
        let thread_shutdown = Arc::clone(&shutdown);

        for generation in 1..=3 {
            request_tx
                .send(DetailCountCommand::Count(request(&raw, generation, 80)))
                .unwrap();
        }

        let handle = thread::spawn(move || {
            worker_loop(request_rx, result_tx, thread_shutdown, move |request, _| {
                thread_visited
                    .lock()
                    .unwrap()
                    .push(request.identity.generation);
                Some(synthetic_result(request))
            });
        });

        let result = result_rx.recv().unwrap();
        assert_eq!(result.identity.generation, 3);
        assert_eq!(*visited.lock().unwrap(), [3]);

        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailCountCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn active_request_is_cancelled_by_newer_request() {
        let raw = Arc::new("line\n".repeat(3_000));
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = thread::spawn(move || {
            worker_loop(
                request_rx,
                result_tx,
                thread_shutdown,
                move |request, is_cancelled| {
                    if request.identity.generation == 1 {
                        started_tx.send(()).unwrap();
                        while !is_cancelled() {
                            thread::yield_now();
                        }
                        None
                    } else {
                        Some(synthetic_result(request))
                    }
                },
            );
        });

        request_tx
            .send(DetailCountCommand::Count(request(&raw, 1, 80)))
            .unwrap();
        started_rx.recv().unwrap();
        request_tx
            .send(DetailCountCommand::Count(request(&raw, 2, 80)))
            .unwrap();

        let result = result_rx.recv().unwrap();
        assert_eq!(result.identity.generation, 2);

        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailCountCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn cancel_without_replacement_stops_active_count_and_worker_remains_usable() {
        let raw = Arc::new("line\n".repeat(3_000));
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (cancelled_tx, cancelled_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = thread::spawn(move || {
            worker_loop(
                request_rx,
                result_tx,
                thread_shutdown,
                move |request, is_cancelled| {
                    if request.identity.generation == 1 {
                        started_tx.send(()).unwrap();
                        while !is_cancelled() {
                            thread::yield_now();
                        }
                        cancelled_tx.send(()).unwrap();
                        None
                    } else {
                        Some(synthetic_result(request))
                    }
                },
            );
        });

        request_tx
            .send(DetailCountCommand::Count(request(&raw, 1, 80)))
            .unwrap();
        started_rx.recv().unwrap();
        request_tx
            .send(DetailCountCommand::Cancel { generation: 2 })
            .unwrap();
        cancelled_rx.recv().unwrap();
        assert!(matches!(result_rx.try_recv(), Err(TryRecvError::Empty)));
        request_tx
            .send(DetailCountCommand::Count(request(&raw, 3, 80)))
            .unwrap();

        let result = result_rx.recv().unwrap();
        assert_eq!(result.identity.generation, 3);

        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailCountCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn persistent_worker_returns_a_real_count_result() {
        let raw = Arc::new("line\n".repeat(10_000));
        let mut worker = DetailCountWorker::start().unwrap();
        let request_tx = worker.request_sender();
        let result_rx = worker.take_result_receiver();

        request_tx
            .send(DetailCountCommand::Count(request(&raw, 9, 80)))
            .unwrap();
        let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        assert_eq!(result.identity.generation, 9);
        assert_eq!(result.chunk_visual_lines.iter().sum::<usize>(), 10_001);
        worker.stop_and_join().unwrap();
    }

    #[test]
    fn shutdown_cancels_an_active_count() {
        let raw = Arc::new("line\n".repeat(3_000));
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, _result_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = thread::spawn(move || {
            worker_loop(
                request_rx,
                result_tx,
                thread_shutdown,
                move |_request, is_cancelled| {
                    started_tx.send(()).unwrap();
                    while !is_cancelled() {
                        thread::yield_now();
                    }
                    None
                },
            );
        });

        request_tx
            .send(DetailCountCommand::Count(request(&raw, 1, 80)))
            .unwrap();
        started_rx.recv().unwrap();
        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailCountCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }
}
