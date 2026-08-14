use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::detail::DetailTextSource;
use super::detail_layout::{
    DetailAnalysisCommand, DetailAnalysisResult, DetailCountRequest, DetailCountResult,
    DetailStructureRequest, DetailStructureResult, build_document_structure_cancellable,
};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(crate) struct DetailAnalysisWorker {
    request_tx: Sender<DetailAnalysisCommand>,
    result_rx: Option<Receiver<DetailAnalysisResult>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DetailAnalysisWorker {
    pub(crate) fn start() -> io::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let handle = thread::Builder::new()
            .name("atc-detail-analysis".to_string())
            .spawn(move || {
                worker_loop(
                    request_rx,
                    result_tx,
                    worker_shutdown,
                    |request, is_cancelled| structure_request(request, is_cancelled),
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

    pub(crate) fn request_sender(&self) -> Sender<DetailAnalysisCommand> {
        self.request_tx.clone()
    }

    pub(crate) fn take_result_receiver(&mut self) -> Receiver<DetailAnalysisResult> {
        self.result_rx
            .take()
            .expect("detail analysis result receiver may only be taken once")
    }

    pub(crate) fn request_stop(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.request_tx.send(DetailAnalysisCommand::Shutdown);
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
            .map_err(|_| io::Error::other("detail analysis worker thread panicked"))
    }

    #[cfg(test)]
    fn start_panicking() -> Self {
        let (request_tx, _request_rx) = mpsc::channel();
        let (_result_tx, result_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = thread::spawn(|| panic!("test detail analysis worker panic"));

        Self {
            request_tx,
            result_rx: Some(result_rx),
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for DetailAnalysisWorker {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.join();
    }
}

fn structure_request(
    request: DetailStructureRequest,
    is_cancelled: &mut dyn FnMut() -> bool,
) -> Option<DetailStructureResult> {
    let segment_lengths = (0..request.snapshot.segment_count())
        .map(|index| request.snapshot.segment_text(index).map_or(0, str::len))
        .collect::<Vec<_>>();
    if segment_lengths != request.identity.segment_lengths {
        return None;
    }

    let structure = build_document_structure_cancellable(&request.snapshot, is_cancelled)?;
    Some(DetailStructureResult {
        identity: request.identity,
        structure,
    })
}

fn count_request(
    request: DetailCountRequest,
    is_cancelled: &mut dyn FnMut() -> bool,
) -> Option<DetailCountResult> {
    let segment_lengths = (0..request.snapshot.segment_count())
        .map(|index| request.snapshot.segment_text(index).map_or(0, str::len))
        .collect::<Vec<_>>();

    if segment_lengths != request.identity.segment_lengths
        || request.structure.chunk_count() != request.identity.chunk_count
    {
        return None;
    }

    let count = request.structure.count_chunks(
        &request.snapshot,
        request.identity.layout_width,
        request.anchor,
        is_cancelled,
    )?;

    Some(DetailCountResult {
        identity: request.identity,
        chunk_visual_lines: count.chunk_visual_lines,
        anchor_visual_row: count.anchor_visual_row,
    })
}

fn worker_loop(
    request_rx: Receiver<DetailAnalysisCommand>,
    result_tx: Sender<DetailAnalysisResult>,
    shutdown: Arc<AtomicBool>,
    mut build: impl FnMut(
        DetailStructureRequest,
        &mut dyn FnMut() -> bool,
    ) -> Option<DetailStructureResult>,
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
            DetailAnalysisCommand::Shutdown => return,
            DetailAnalysisCommand::Cancel { layout_generation } => {
                let _ = layout_generation;
            }
            DetailAnalysisCommand::BuildStructure(request) => {
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

                    build(request, &mut is_cancelled).map(DetailAnalysisResult::StructureReady)
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
            DetailAnalysisCommand::Count(request) => {
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

                    count(request, &mut is_cancelled).map(DetailAnalysisResult::Count)
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
    request_rx: &Receiver<DetailAnalysisCommand>,
    initial: DetailAnalysisCommand,
) -> DetailAnalysisCommand {
    let mut latest = initial;

    loop {
        match request_rx.try_recv() {
            Ok(DetailAnalysisCommand::Shutdown) => return DetailAnalysisCommand::Shutdown,
            Ok(command) => latest = command,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return latest,
        }
    }
}

fn try_drain_latest(request_rx: &Receiver<DetailAnalysisCommand>) -> Option<DetailAnalysisCommand> {
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
        ContentAnchor, DetailAnalysisCommand, DetailAnalysisResult, DetailCountRequest,
        DetailLayout, RawOffset, wrap_detail_document,
    };
    use std::sync::Mutex;

    fn request(raw: &Arc<String>, generation: u64, width: u16) -> DetailCountRequest {
        let segments = [raw];
        let document = DetailDocument::from_shared_segments(&segments);
        let mut layout = DetailLayout::default();
        layout.viewport(&document, generation, width, 20, 0);
        layout.complete_structure_for_test(&document);
        layout.stage_analysis_command(&document);

        let Some(DetailAnalysisCommand::Count(mut request)) = layout.take_analysis_command() else {
            panic!("large test document must stage a count request");
        };
        request.identity.layout_generation = generation;
        request
    }

    fn build_request(raw: &Arc<String>, generation: u64) -> DetailStructureRequest {
        let segments = [raw];
        let document = DetailDocument::from_shared_segments(&segments);
        let mut layout = DetailLayout::default();
        layout.viewport(&document, generation, 80, 20, 0);
        layout.stage_analysis_command(&document);

        let Some(DetailAnalysisCommand::BuildStructure(mut request)) =
            layout.take_analysis_command()
        else {
            panic!("large test document must stage a structure request");
        };
        request.identity.generation = generation;
        request
    }

    fn synthetic_result(request: DetailCountRequest) -> DetailCountResult {
        DetailCountResult {
            chunk_visual_lines: vec![1; request.identity.chunk_count],
            anchor_visual_row: None,
            identity: request.identity,
        }
    }

    #[test]
    fn count_request_resolves_an_anchor_in_the_same_background_pass() {
        let raw = Arc::new("abcdefghij".repeat(10_000));
        let mut request = request(&raw, 1, 6);
        assert_eq!(request.identity.layout_width, 5);
        request.anchor = Some(ContentAnchor {
            unit_index: 0,
            raw_position: RawOffset(5),
        });

        let result = count_request(request, &mut || false).unwrap();

        assert_eq!(result.anchor_visual_row, Some(1));
        assert_eq!(result.chunk_visual_lines, [20_000]);
    }

    #[test]
    fn idle_worker_stops_and_joins() {
        DetailAnalysisWorker::start()
            .unwrap()
            .stop_and_join()
            .unwrap();
    }

    #[test]
    fn worker_panic_is_reported_by_join() {
        let error = DetailAnalysisWorker::start_panicking()
            .stop_and_join()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("panicked"));
    }

    #[test]
    fn count_request_shares_raw_buffer_and_returns_only_count_metadata() {
        let raw = Arc::new("line\n".repeat(100_000));
        let owners = Arc::strong_count(&raw);
        let request = request(&raw, 1, 80);

        assert!(request.snapshot.shares_buffer(&raw));
        assert_eq!(Arc::strong_count(&raw), owners + 1);

        let result = count_request(request, &mut || false).unwrap();
        assert_eq!(result.chunk_visual_lines.len(), result.identity.chunk_count);
        assert_eq!(result.chunk_visual_lines.iter().sum::<usize>(), 100_001);
        assert_eq!(result.anchor_visual_row, None);
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
    fn giant_single_line_background_count_matches_the_eager_reference() {
        let raw =
            Arc::new("word 日本語 e\u{301} 👩‍💻 \u{200b} abcdefghijklmnopqrstuvwxyz ".repeat(4_000));
        let owners = Arc::strong_count(&raw);
        let request = request(&raw, 1, 24);
        let layout_width = request.identity.layout_width;
        let reference_height =
            wrap_detail_document(&request.snapshot, u16::try_from(layout_width).unwrap()).height();

        assert_eq!(request.identity.chunk_count, 1);
        assert!(request.snapshot.shares_buffer(&raw));
        let result = count_request(request, &mut || false).unwrap();

        assert_eq!(result.chunk_visual_lines, [reference_height]);
        assert_eq!(Arc::strong_count(&raw), owners);
    }

    #[test]
    fn mixed_normal_and_giant_sparse_units_match_the_eager_reference() {
        let normal = "normal 日本語 e\u{301} 👩‍💻\n\n".repeat(600);
        let giant = "giant-token ".repeat(8_000);
        let tail = "tail\n".repeat(600);
        let raw = Arc::new(format!("{normal}{giant}\n{tail}"));
        let request = request(&raw, 1, 23);
        let layout_width = request.identity.layout_width;
        let reference_height =
            wrap_detail_document(&request.snapshot, u16::try_from(layout_width).unwrap()).height();

        assert!(request.identity.chunk_count > 3);
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
                .send(DetailAnalysisCommand::Count(request(&raw, generation, 80)))
                .unwrap();
        }

        let handle = thread::spawn(move || {
            worker_loop(
                request_rx,
                result_tx,
                thread_shutdown,
                |_, _| panic!("count test must not build structure"),
                move |request, _| {
                    thread_visited
                        .lock()
                        .unwrap()
                        .push(request.identity.layout_generation);
                    Some(synthetic_result(request))
                },
            );
        });

        let result = result_rx.recv().unwrap();
        let DetailAnalysisResult::Count(result) = result else {
            panic!("expected count result");
        };
        assert_eq!(result.identity.layout_generation, 3);
        assert_eq!(*visited.lock().unwrap(), [3]);

        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailAnalysisCommand::Shutdown).unwrap();
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
                |_, _| panic!("count test must not build structure"),
                move |request, is_cancelled| {
                    if request.identity.layout_generation == 1 {
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
            .send(DetailAnalysisCommand::Count(request(&raw, 1, 80)))
            .unwrap();
        started_rx.recv().unwrap();
        request_tx
            .send(DetailAnalysisCommand::Count(request(&raw, 2, 80)))
            .unwrap();

        let result = result_rx.recv().unwrap();
        let DetailAnalysisResult::Count(result) = result else {
            panic!("expected count result");
        };
        assert_eq!(result.identity.layout_generation, 2);

        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailAnalysisCommand::Shutdown).unwrap();
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
                |_, _| panic!("count test must not build structure"),
                move |request, is_cancelled| {
                    if request.identity.layout_generation == 1 {
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
            .send(DetailAnalysisCommand::Count(request(&raw, 1, 80)))
            .unwrap();
        started_rx.recv().unwrap();
        request_tx
            .send(DetailAnalysisCommand::Cancel {
                layout_generation: 2,
            })
            .unwrap();
        cancelled_rx.recv().unwrap();
        assert!(matches!(result_rx.try_recv(), Err(TryRecvError::Empty)));
        request_tx
            .send(DetailAnalysisCommand::Count(request(&raw, 3, 80)))
            .unwrap();

        let result = result_rx.recv().unwrap();
        let DetailAnalysisResult::Count(result) = result else {
            panic!("expected count result");
        };
        assert_eq!(result.identity.layout_generation, 3);

        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailAnalysisCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn persistent_worker_returns_a_real_count_result() {
        let raw = Arc::new("line\n".repeat(10_000));
        let mut worker = DetailAnalysisWorker::start().unwrap();
        let request_tx = worker.request_sender();
        let result_rx = worker.take_result_receiver();

        request_tx
            .send(DetailAnalysisCommand::Count(request(&raw, 9, 80)))
            .unwrap();
        let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let DetailAnalysisResult::Count(result) = result else {
            panic!("expected count result");
        };
        assert_eq!(result.identity.layout_generation, 9);
        assert_eq!(result.chunk_visual_lines.iter().sum::<usize>(), 10_001);
        worker.stop_and_join().unwrap();
    }

    #[test]
    fn persistent_worker_builds_a_complete_sparse_structure_from_shared_raw_text() {
        let raw = Arc::new("line\n".repeat(100_000));
        let owners = Arc::strong_count(&raw);
        let request = build_request(&raw, 17);
        assert!(request.snapshot.shares_buffer(&raw));
        assert_eq!(Arc::strong_count(&raw), owners + 1);

        let mut worker = DetailAnalysisWorker::start().unwrap();
        let request_tx = worker.request_sender();
        let result_rx = worker.take_result_receiver();
        request_tx
            .send(DetailAnalysisCommand::BuildStructure(request))
            .unwrap();

        let DetailAnalysisResult::StructureReady(result) =
            result_rx.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("expected structure result");
        };
        assert_eq!(result.identity.generation, 17);
        assert!(result.structure.is_complete());
        assert_eq!(result.structure.len(), 100_001);
        assert!(result.structure.chunk_count() < 500);
        assert_eq!(Arc::strong_count(&raw), owners);
        worker.stop_and_join().unwrap();
    }

    #[test]
    fn newer_document_cancels_an_active_structure_build() {
        let raw_a = Arc::new("line-a\n".repeat(20_000));
        let raw_b = Arc::new("line-b\n".repeat(20_000));
        let request_a = build_request(&raw_a, 1);
        let request_b = build_request(&raw_b, 2);
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
                        structure_request(request, &mut || false)
                    }
                },
                |_, _| panic!("structure test must not count"),
            );
        });

        request_tx
            .send(DetailAnalysisCommand::BuildStructure(request_a))
            .unwrap();
        started_rx.recv().unwrap();
        request_tx
            .send(DetailAnalysisCommand::BuildStructure(request_b))
            .unwrap();
        cancelled_rx.recv().unwrap();

        let DetailAnalysisResult::StructureReady(result) = result_rx.recv().unwrap() else {
            panic!("expected latest structure result");
        };
        assert_eq!(result.identity.generation, 2);

        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailAnalysisCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn background_structure_scan_polls_cancellation_for_newline_and_giant_documents() {
        for raw in [
            Arc::new("short-line\n".repeat(200_000)),
            Arc::new("giant-token-without-newline".repeat(100_000)),
        ] {
            let request = build_request(&raw, 1);
            let mut polls = 0usize;
            let result = structure_request(request, &mut || {
                polls = polls.saturating_add(1);
                polls >= 3
            });
            assert!(result.is_none());
            assert_eq!(polls, 3);
        }
    }

    #[test]
    fn shutdown_cancels_an_active_structure_build() {
        let raw = Arc::new("giant-no-newline".repeat(100_000));
        let request = build_request(&raw, 1);
        let (request_tx, request_rx) = mpsc::channel();
        let (result_tx, _result_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (cancelled_tx, cancelled_rx) = mpsc::channel();
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
                    cancelled_tx.send(()).unwrap();
                    None
                },
                |_, _| panic!("structure test must not count"),
            );
        });

        request_tx
            .send(DetailAnalysisCommand::BuildStructure(request))
            .unwrap();
        started_rx.recv().unwrap();
        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailAnalysisCommand::Shutdown).unwrap();
        cancelled_rx.recv().unwrap();
        handle.join().unwrap();
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
                |_, _| panic!("count test must not build structure"),
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
            .send(DetailAnalysisCommand::Count(request(&raw, 1, 80)))
            .unwrap();
        started_rx.recv().unwrap();
        shutdown.store(true, Ordering::Release);
        request_tx.send(DetailAnalysisCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }
}
