use std::io;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::attempt::AttemptOutcome;
use crate::config::RunnerConfig;
use crate::model::Problem;
use crate::tui::message::{Message, RunRequest};

use super::attempt_executor::{ActiveAttempt, AttemptCompletion, AttemptExecutor};
use super::run_scheduler::{RequestArrival, RetiredActive, RunScheduler};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_RUN_REQUESTS_PER_TICK: usize = 64;

pub(super) struct TestWorker {
    request_tx: Sender<RunRequest>,
    control: Arc<WorkerControl>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

#[derive(Debug, Default)]
struct WorkerControl {
    stopping: AtomicBool,
    lifecycle_gate: Mutex<()>,
}

impl WorkerControl {
    fn begin_shutdown(&self) {
        let _gate = self
            .lifecycle_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.stopping.store(true, Ordering::Release);
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    fn while_running<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let _gate = self
            .lifecycle_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_stopping() {
            return None;
        }
        Some(action())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestDrain {
    Open,
    Disconnected,
    Shutdown,
}

impl TestWorker {
    pub fn start(
        destination: PathBuf,
        problems: Vec<Problem>,
        runner_config: RunnerConfig,
        message_tx: Sender<Message>,
    ) -> io::Result<Self> {
        let executor =
            AttemptExecutor::new(destination, problems, runner_config, message_tx.clone());

        Self::start_with(message_tx, move |request, completion_tx| {
            executor.spawn(request, completion_tx)
        })
    }

    fn start_with(
        message_tx: Sender<Message>,
        spawn_attempt: impl FnMut(RunRequest, Sender<AttemptCompletion>) -> io::Result<ActiveAttempt>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();
        let control = Arc::new(WorkerControl::default());
        let thread_control = Arc::clone(&control);
        let failure_tx = message_tx.clone();

        let handle = thread::Builder::new()
            .name("atc-watch-test".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    scheduler_loop(
                        request_rx,
                        completion_rx,
                        completion_tx,
                        thread_control,
                        message_tx,
                        spawn_attempt,
                    )
                }));

                match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => {
                        report_worker_failure(&failure_tx, &error);
                        Err(error)
                    }
                    Err(_) => {
                        let error = io::Error::other("test worker thread panicked");
                        report_worker_failure(&failure_tx, &error);
                        Err(error)
                    }
                }
            })?;

        Ok(Self {
            request_tx,
            control,
            handle: Some(handle),
        })
    }

    pub fn sender(&self) -> Sender<RunRequest> {
        self.request_tx.clone()
    }

    pub fn request_stop(&self) {
        self.control.begin_shutdown();
    }

    fn join(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };

        handle
            .join()
            .map_err(|_| io::Error::other("test worker thread panicked before reporting"))??;
        Ok(())
    }

    pub fn stop_and_join(mut self) -> io::Result<()> {
        self.request_stop();
        self.join()
    }
}

impl Drop for TestWorker {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.join();
    }
}

fn scheduler_loop(
    request_rx: Receiver<RunRequest>,
    completion_rx: Receiver<AttemptCompletion>,
    completion_tx: Sender<AttemptCompletion>,
    control: Arc<WorkerControl>,
    message_tx: Sender<Message>,
    mut spawn_attempt: impl FnMut(RunRequest, Sender<AttemptCompletion>) -> io::Result<ActiveAttempt>,
) -> io::Result<()> {
    let mut scheduler = RunScheduler::default();
    let mut active = None;

    let run_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scheduler_loop_inner(
            &request_rx,
            &completion_rx,
            &completion_tx,
            &control,
            &message_tx,
            &mut spawn_attempt,
            &mut scheduler,
            &mut active,
        )
    }))
    .unwrap_or_else(|_| Err(io::Error::other("test worker scheduler loop panicked")));
    let cleanup_result = cleanup_active(&mut scheduler, active.take());

    combine_loop_and_cleanup_results(run_result, cleanup_result)
}

#[allow(clippy::too_many_arguments)]
fn scheduler_loop_inner(
    request_rx: &Receiver<RunRequest>,
    completion_rx: &Receiver<AttemptCompletion>,
    completion_tx: &Sender<AttemptCompletion>,
    control: &WorkerControl,
    message_tx: &Sender<Message>,
    spawn_attempt: &mut impl FnMut(RunRequest, Sender<AttemptCompletion>) -> io::Result<ActiveAttempt>,
    scheduler: &mut RunScheduler,
    active: &mut Option<ActiveAttempt>,
) -> io::Result<()> {
    loop {
        if control.is_stopping() {
            return Ok(());
        }

        if process_ready_completion(completion_rx, scheduler, active, message_tx, control)? {
            continue;
        }

        match drain_requests(request_rx, scheduler, active.as_ref(), control)? {
            RequestDrain::Open => {}
            RequestDrain::Disconnected | RequestDrain::Shutdown => return Ok(()),
        }

        // Requests are bounded above, and completion is checked both before and after the
        // batch. A request flood therefore cannot starve an already-finished attempt.
        if process_ready_completion(completion_rx, scheduler, active, message_tx, control)? {
            continue;
        }

        if control.is_stopping() {
            return Ok(());
        }

        if active.is_none() {
            let spawned = control.while_running(|| {
                scheduler
                    .start_next()
                    .map(|request| (request, spawn_attempt(request, completion_tx.clone())))
            });

            match spawned {
                None => return Ok(()),
                Some(None) => {}
                Some(Some((_request, Ok(attempt)))) => {
                    if let Err(identity_error) = ensure_identity(scheduler, &attempt) {
                        attempt.request_cancel();
                        let cleanup_result = attempt.join().map(|_| ());
                        return combine_loop_and_cleanup_results(
                            Err(identity_error),
                            cleanup_result,
                        );
                    }
                    *active = Some(attempt);
                    continue;
                }
                Some(Some((request, Err(error)))) => {
                    let _retired = retire_matching(scheduler, request)?;
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "failed to spawn physical attempt {}/{}: {error}",
                            request.problem, request.run_id
                        ),
                    ));
                }
            }
        }

        match request_rx.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(request) => {
                let Some(result) =
                    control.while_running(|| process_request(scheduler, active.as_ref(), request))
                else {
                    return Ok(());
                };
                result?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn drain_requests(
    request_rx: &Receiver<RunRequest>,
    scheduler: &mut RunScheduler,
    active: Option<&ActiveAttempt>,
    control: &WorkerControl,
) -> io::Result<RequestDrain> {
    for _ in 0..MAX_RUN_REQUESTS_PER_TICK {
        match request_rx.try_recv() {
            Ok(request) => {
                let Some(result) =
                    control.while_running(|| process_request(scheduler, active, request))
                else {
                    return Ok(RequestDrain::Shutdown);
                };
                result?;
            }
            Err(TryRecvError::Empty) => return Ok(RequestDrain::Open),
            Err(TryRecvError::Disconnected) => return Ok(RequestDrain::Disconnected),
        }
    }

    Ok(RequestDrain::Open)
}

fn process_request(
    scheduler: &mut RunScheduler,
    active: Option<&ActiveAttempt>,
    request: RunRequest,
) -> io::Result<()> {
    match scheduler.request_arrived(request) {
        RequestArrival::IgnoredStale => Ok(()),
        RequestArrival::Accepted {
            cancel_active: false,
        } => Ok(()),
        RequestArrival::Accepted {
            cancel_active: true,
        } => {
            let active = active.ok_or_else(|| {
                io::Error::other("scheduler requested cancellation without a physical attempt")
            })?;
            ensure_identity(scheduler, active)?;
            active.request_cancel();
            Ok(())
        }
    }
}

fn process_ready_completion(
    completion_rx: &Receiver<AttemptCompletion>,
    scheduler: &mut RunScheduler,
    active: &mut Option<ActiveAttempt>,
    message_tx: &Sender<Message>,
    control: &WorkerControl,
) -> io::Result<bool> {
    match completion_rx.try_recv() {
        Ok(completion) => {
            complete_attempt(completion, scheduler, active, message_tx, control)?;
            Ok(true)
        }
        Err(TryRecvError::Disconnected) => {
            if active.is_some() {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "physical attempt completion channel disconnected",
                ))
            } else {
                Ok(false)
            }
        }
        Err(TryRecvError::Empty) => Ok(false),
    }
}

fn complete_attempt(
    completion: AttemptCompletion,
    scheduler: &mut RunScheduler,
    active: &mut Option<ActiveAttempt>,
    message_tx: &Sender<Message>,
    control: &WorkerControl,
) -> io::Result<()> {
    let physical = active.as_ref().ok_or_else(|| {
        io::Error::other(format!(
            "completion arrived without a physical attempt: {}/{}",
            completion.problem, completion.run_id
        ))
    })?;
    let request = physical.request();
    validate_completion(completion, request)?;
    ensure_identity(scheduler, physical)?;

    // Join is authoritative and is the no-late-event boundary. Logical messages and the next
    // spawn are deliberately impossible before this call returns.
    let physical = active.take().expect("active was checked above");
    let outcome = physical.join()?;
    finish_joined_attempt(request, outcome, scheduler, message_tx, control)
}

fn finish_joined_attempt(
    request: RunRequest,
    outcome: AttemptOutcome,
    scheduler: &mut RunScheduler,
    message_tx: &Sender<Message>,
    control: &WorkerControl,
) -> io::Result<()> {
    let retired = retire_matching(scheduler, request)?;

    // Shutdown and logical outcome publication are serialized by the same lifecycle gate.
    // Whichever obtains it first is the linearization point. Once shutdown wins, no terminal
    // message or requeue can be published and no next attempt can be spawned.
    let Some(result) = control.while_running(|| match outcome {
        AttemptOutcome::Completed if retired.is_latest() => send_message(
            message_tx,
            Message::RunCompleted {
                run_id: request.run_id,
                problem: request.problem,
            },
        ),
        AttemptOutcome::Failed(error) if retired.is_latest() => send_message(
            message_tx,
            Message::RunFailed {
                run_id: request.run_id,
                problem: request.problem,
                error: error.to_string(),
            },
        ),
        AttemptOutcome::Cancelled => {
            if scheduler.requeue_retired(retired) {
                send_message(
                    message_tx,
                    Message::RunRequeued {
                        run_id: request.run_id,
                        problem: request.problem,
                    },
                )?;
            }
            Ok(())
        }
        AttemptOutcome::Completed | AttemptOutcome::Failed(_) => Ok(()),
    }) else {
        return Ok(());
    };
    result
}

fn cleanup_active(scheduler: &mut RunScheduler, active: Option<ActiveAttempt>) -> io::Result<()> {
    if let Some(active) = active {
        let identity_result = ensure_identity(scheduler, &active);
        active.request_cancel();
        let request = active.request();
        let join_result = active.join().map(|_| ());
        combine_loop_and_cleanup_results(identity_result, join_result)?;
        let _retired = retire_matching(scheduler, request)?;
    }

    // Foreground and pending logical requests are intentionally dropped with the scheduler.
    // Shutdown cancellation never produces RunRequeued or a terminal run message.
    Ok(())
}

fn combine_loop_and_cleanup_results(
    run_result: io::Result<()>,
    cleanup_result: io::Result<()>,
) -> io::Result<()> {
    match (run_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(run_error), Err(cleanup_error)) => Err(io::Error::other(format!(
            "test worker failed: {run_error}; active attempt cleanup also failed: {cleanup_error}"
        ))),
    }
}

fn ensure_identity(scheduler: &RunScheduler, active: &ActiveAttempt) -> io::Result<()> {
    let physical = active.request();
    let logical = scheduler.active_request().ok_or_else(|| {
        io::Error::other("physical attempt exists without a logical active request")
    })?;

    if logical != physical {
        return Err(io::Error::other(format!(
            "logical/physical attempt identity mismatch: logical={}/{}, physical={}/{}",
            logical.problem, logical.run_id, physical.problem, physical.run_id
        )));
    }

    Ok(())
}

fn validate_completion(completion: AttemptCompletion, request: RunRequest) -> io::Result<()> {
    if completion.problem == request.problem && completion.run_id == request.run_id {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "attempt completion identity mismatch: completion={}/{}, physical={}/{}",
        completion.problem, completion.run_id, request.problem, request.run_id
    )))
}

fn retire_matching(scheduler: &mut RunScheduler, request: RunRequest) -> io::Result<RetiredActive> {
    let retired = scheduler.retire_active().ok_or_else(|| {
        io::Error::other("physical attempt finished without logical active state")
    })?;

    if retired.request() != request {
        return Err(io::Error::other(format!(
            "retired/physical attempt identity mismatch: retired={}/{}, physical={}/{}",
            retired.request().problem,
            retired.request().run_id,
            request.problem,
            request.run_id
        )));
    }

    Ok(retired)
}

fn send_message(message_tx: &Sender<Message>, message: Message) -> io::Result<()> {
    message_tx.send(message).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "TUI message receiver disconnected from test worker",
        )
    })
}

fn report_worker_failure(message_tx: &Sender<Message>, error: &io::Error) {
    let _ = message_tx.send(Message::WorkerFailed(io::Error::new(
        error.kind(),
        error.to_string(),
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attempt::{AttemptCancellation, clean_cancellation_io_error, run_attempt};
    use crate::commands::attempt_executor::spawn_with;
    use crate::error::AppError;
    use crate::language::Language;
    use crate::tui::message::TestEvent;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    #[derive(Debug, Clone, Copy)]
    enum Finish {
        Completed,
        Cancelled,
        Failed,
    }

    struct SpawnedAttempt {
        request: RunRequest,
        cancellation: Arc<AttemptCancellation>,
        finish_tx: Sender<Finish>,
    }

    impl SpawnedAttempt {
        fn finish(self, finish: Finish) {
            self.finish_tx.send(finish).unwrap();
        }
    }

    struct FakeWorker {
        worker: TestWorker,
        request_tx: Sender<RunRequest>,
        spawned_rx: Receiver<SpawnedAttempt>,
        message_rx: Receiver<Message>,
        active_count: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl FakeWorker {
        fn start() -> Self {
            let (message_tx, message_rx) = mpsc::channel();
            let (spawned_tx, spawned_rx) = mpsc::channel();
            let active_count = Arc::new(AtomicUsize::new(0));
            let max_active = Arc::new(AtomicUsize::new(0));
            let closure_messages = message_tx.clone();
            let closure_active = Arc::clone(&active_count);
            let closure_max = Arc::clone(&max_active);

            let worker = TestWorker::start_with(message_tx, move |request, completion_tx| {
                let messages = closure_messages.clone();
                let spawned_tx = spawned_tx.clone();
                let active_count = Arc::clone(&closure_active);
                let max_active = Arc::clone(&closure_max);
                let (finish_tx, finish_rx) = mpsc::channel();

                spawn_with(request, completion_tx, move |cancellation| {
                    let current = active_count.fetch_add(1, Ordering::AcqRel) + 1;
                    max_active.fetch_max(current, Ordering::AcqRel);
                    let _guard = ActiveCountGuard(active_count);

                    messages
                        .send(Message::RunStarted {
                            run_id: request.run_id,
                            problem: request.problem,
                        })
                        .unwrap();
                    spawned_tx
                        .send(SpawnedAttempt {
                            request,
                            cancellation: Arc::clone(&cancellation),
                            finish_tx,
                        })
                        .unwrap();

                    let finish = finish_rx.recv().unwrap();
                    messages
                        .send(Message::RunEvent {
                            run_id: request.run_id,
                            problem: request.problem,
                            event: TestEvent::NoSamples,
                        })
                        .unwrap();

                    match finish {
                        Finish::Completed => run_attempt(&cancellation, |_| Ok(())),
                        Finish::Cancelled => run_attempt(&cancellation, |is_cancelled| {
                            assert!(is_cancelled());
                            Err(AppError::from(clean_cancellation_io_error()))
                        }),
                        Finish::Failed => run_attempt(&cancellation, |_| {
                            Err(io::Error::other("fake attempt failed").into())
                        }),
                    }
                })
            })
            .unwrap();
            let request_tx = worker.sender();

            Self {
                worker,
                request_tx,
                spawned_rx,
                message_rx,
                active_count,
                max_active,
            }
        }

        fn send(&self, problem: usize, run_id: u64) {
            self.request_tx.send(request(problem, run_id)).unwrap();
        }

        fn spawned(&self) -> SpawnedAttempt {
            self.spawned_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
        }

        fn stop(self) {
            self.worker.stop_and_join().unwrap();
            assert_eq!(self.active_count.load(Ordering::Acquire), 0);
        }
    }

    struct ActiveCountGuard(Arc<AtomicUsize>);

    impl Drop for ActiveCountGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn request(problem: usize, run_id: u64) -> RunRequest {
        RunRequest {
            run_id,
            problem,
            language: Language::Cpp,
            debug: false,
        }
    }

    fn wait_cancel_requested(cancellation: &AttemptCancellation) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !cancellation.is_requested() {
            assert!(Instant::now() < deadline, "attempt was not cancelled");
            thread::yield_now();
        }
    }

    fn messages_until(
        receiver: &Receiver<Message>,
        predicate: impl Fn(&Message) -> bool,
    ) -> Vec<Message> {
        let mut messages = Vec::new();
        loop {
            let message = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            let done = predicate(&message);
            messages.push(message);
            if done {
                return messages;
            }
        }
    }

    fn assert_no_logical_message(receiver: &Receiver<Message>, run_id: u64) {
        for message in receiver.try_iter() {
            match message {
                Message::RunCompleted { run_id: id, .. }
                | Message::RunFailed { run_id: id, .. }
                | Message::RunRequeued { run_id: id, .. }
                    if id == run_id =>
                {
                    panic!("unexpected logical message for run {run_id}")
                }
                _ => {}
            }
        }
    }

    #[test]
    fn same_problem_cancelled_replacement_is_discarded_and_latest_starts() {
        let fake = FakeWorker::start();
        fake.send(0, 1);
        let first = fake.spawned();
        fake.send(0, 2);
        wait_cancel_requested(&first.cancellation);
        first.finish(Finish::Cancelled);

        let second = fake.spawned();
        assert_eq!(second.request, request(0, 2));
        assert_no_logical_message(&fake.message_rx, 1);
        second.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 2, .. })
        });
        assert_eq!(fake.max_active.load(Ordering::Acquire), 1);
        fake.stop();
    }

    #[test]
    fn obsolete_natural_completion_is_silently_discarded() {
        let fake = FakeWorker::start();
        fake.send(0, 1);
        let first = fake.spawned();
        fake.send(0, 2);
        wait_cancel_requested(&first.cancellation);
        first.finish(Finish::Completed);

        let second = fake.spawned();
        assert_eq!(second.request, request(0, 2));
        assert_no_logical_message(&fake.message_rx, 1);
        second.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 2, .. })
        });
        fake.stop();
    }

    #[test]
    fn obsolete_failure_is_silently_discarded() {
        let fake = FakeWorker::start();
        fake.send(0, 1);
        let first = fake.spawned();
        fake.send(0, 2);
        wait_cancel_requested(&first.cancellation);
        first.finish(Finish::Failed);

        let second = fake.spawned();
        assert_eq!(second.request, request(0, 2));
        assert_no_logical_message(&fake.message_rx, 1);
        second.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 2, .. })
        });
        fake.stop();
    }

    #[test]
    fn different_problem_cancel_requeues_after_join_and_reruns_same_id() {
        let fake = FakeWorker::start();
        fake.send(0, 5);
        let first_a = fake.spawned();
        fake.send(1, 6);
        wait_cancel_requested(&first_a.cancellation);
        first_a.finish(Finish::Cancelled);

        let b = fake.spawned();
        assert_eq!(b.request, request(1, 6));
        let before_b = messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunStarted { run_id: 6, .. })
        });
        let old_event = before_b
            .iter()
            .position(|message| matches!(message, Message::RunEvent { run_id: 5, .. }))
            .unwrap();
        let requeued = before_b
            .iter()
            .position(|message| matches!(message, Message::RunRequeued { run_id: 5, .. }))
            .unwrap();
        let b_started = before_b
            .iter()
            .position(|message| matches!(message, Message::RunStarted { run_id: 6, .. }))
            .unwrap();
        assert!(old_event < requeued && requeued < b_started);

        b.finish(Finish::Completed);
        let second_a = fake.spawned();
        assert_eq!(second_a.request, request(0, 5));
        second_a.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 5, .. })
        });
        assert_eq!(fake.max_active.load(Ordering::Acquire), 1);
        fake.stop();
    }

    #[test]
    fn different_problem_natural_completion_is_terminal_not_requeued() {
        let fake = FakeWorker::start();
        fake.send(0, 1);
        let a = fake.spawned();
        fake.send(1, 2);
        wait_cancel_requested(&a.cancellation);
        a.finish(Finish::Completed);

        let b = fake.spawned();
        assert_eq!(b.request, request(1, 2));
        let messages = messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 1, .. })
        });
        assert!(
            messages
                .iter()
                .all(|message| { !matches!(message, Message::RunRequeued { run_id: 1, .. }) })
        );
        b.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 2, .. })
        });
        assert!(fake.spawned_rx.try_recv().is_err());
        fake.stop();
    }

    #[test]
    fn failed_active_is_terminal_and_not_requeued() {
        let fake = FakeWorker::start();
        fake.send(0, 1);
        let a = fake.spawned();
        fake.send(1, 2);
        wait_cancel_requested(&a.cancellation);
        a.finish(Finish::Failed);

        let b = fake.spawned();
        assert_eq!(b.request, request(1, 2));
        let messages = messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunFailed { run_id: 1, .. })
        });
        assert!(
            messages
                .iter()
                .all(|message| { !matches!(message, Message::RunRequeued { run_id: 1, .. }) })
        );
        b.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 2, .. })
        });
        fake.stop();
    }

    #[test]
    fn requests_during_cancellation_update_foreground_and_fifo_tail() {
        let fake = FakeWorker::start();
        fake.send(0, 1);
        let a = fake.spawned();
        fake.send(1, 2);
        wait_cancel_requested(&a.cancellation);
        fake.send(2, 3);
        a.finish(Finish::Cancelled);

        let c = fake.spawned();
        assert_eq!(c.request, request(2, 3));
        c.finish(Finish::Completed);
        let b = fake.spawned();
        assert_eq!(b.request, request(1, 2));
        b.finish(Finish::Completed);
        let a_again = fake.spawned();
        assert_eq!(a_again.request, request(0, 1));
        a_again.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 1, .. })
        });
        fake.stop();
    }

    #[test]
    fn same_problem_arrival_during_cancellation_prevents_old_requeue() {
        let fake = FakeWorker::start();
        fake.send(0, 1);
        let a = fake.spawned();
        fake.send(1, 2);
        wait_cancel_requested(&a.cancellation);
        fake.send(0, 3);
        a.finish(Finish::Cancelled);

        let latest_a = fake.spawned();
        assert_eq!(latest_a.request, request(0, 3));
        assert_no_logical_message(&fake.message_rx, 1);
        latest_a.finish(Finish::Completed);
        let b = fake.spawned();
        assert_eq!(b.request, request(1, 2));
        b.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 2, .. })
        });
        fake.stop();
    }

    #[test]
    fn pending_problem_update_runs_only_the_latest_request() {
        let fake = FakeWorker::start();
        fake.send(0, 1);
        let a = fake.spawned();

        // B1 is displaced by C1 and then replaced by B2. Request and completion
        // notifications use different channels, so A may be requeued immediately
        // before or after C is adopted. That cross-channel order is intentionally
        // unspecified; the invariant here is that B1 never starts and C/A each run.
        fake.send(1, 2);
        wait_cancel_requested(&a.cancellation);
        fake.send(2, 3);
        fake.send(1, 4);
        a.finish(Finish::Cancelled);

        let latest_b = fake.spawned();
        assert_eq!(latest_b.request, request(1, 4));
        latest_b.finish(Finish::Completed);
        let first_pending = fake.spawned();
        let first_request = first_pending.request;
        assert!(first_request == request(0, 1) || first_request == request(2, 3));
        first_pending.finish(Finish::Completed);

        let second_pending = fake.spawned();
        let expected_second = if first_request == request(0, 1) {
            request(2, 3)
        } else {
            assert_eq!(first_request, request(2, 3));
            request(0, 1)
        };
        assert_eq!(second_pending.request, expected_second);
        second_pending.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 1, .. })
        });

        assert!(
            fake.message_rx
                .try_iter()
                .all(|message| !matches!(message, Message::RunStarted { run_id: 2, .. }))
        );
        fake.stop();
    }

    #[test]
    fn stale_and_duplicate_requests_never_start() {
        let fake = FakeWorker::start();
        fake.send(0, 3);
        let latest = fake.spawned();
        fake.send(0, 2);
        fake.send(0, 3);
        latest.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 3, .. })
        });
        assert!(fake.spawned_rx.try_recv().is_err());
        fake.stop();
    }

    #[test]
    fn bounded_request_drain_leaves_excess_for_the_next_tick() {
        let (tx, rx) = mpsc::channel();
        for run_id in 1..=(MAX_RUN_REQUESTS_PER_TICK as u64 + 5) {
            tx.send(request(run_id as usize, run_id)).unwrap();
        }
        let mut scheduler = RunScheduler::default();
        let control = WorkerControl::default();

        assert_eq!(
            drain_requests(&rx, &mut scheduler, None, &control).unwrap(),
            RequestDrain::Open
        );
        assert_eq!(rx.try_iter().count(), 5);
    }

    #[test]
    fn idle_shutdown_does_not_require_a_request() {
        let fake = FakeWorker::start();
        fake.stop();
    }

    #[test]
    fn active_shutdown_cancels_joins_and_never_starts_pending() {
        let (message_tx, message_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut release_rx = Some(release_rx);
        let worker = TestWorker::start_with(message_tx, move |request, completion_tx| {
            let spawned_tx = spawned_tx.clone();
            let release_rx = release_rx.take().unwrap();
            spawn_with(request, completion_tx, move |cancellation| {
                spawned_tx
                    .send((request, Arc::clone(&cancellation)))
                    .unwrap();
                while !cancellation.is_requested() {
                    thread::yield_now();
                }
                release_rx.recv().unwrap();
                run_attempt(&cancellation, |is_cancelled| {
                    assert!(is_cancelled());
                    Err(AppError::from(clean_cancellation_io_error()))
                })
            })
        })
        .unwrap();
        let request_tx = worker.sender();
        request_tx.send(request(0, 1)).unwrap();
        let (spawned, cancellation) = spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(spawned, request(0, 1));
        for problem in 1..=100 {
            request_tx
                .send(request(problem, problem as u64 + 1))
                .unwrap();
        }
        wait_cancel_requested(&cancellation);

        worker.request_stop();
        release_tx.send(()).unwrap();
        worker.stop_and_join().unwrap();

        assert!(spawned_rx.try_recv().is_err());
        assert!(message_rx.try_iter().all(|message| {
            !matches!(
                message,
                Message::RunRequeued { .. }
                    | Message::RunCompleted { .. }
                    | Message::RunFailed { .. }
            )
        }));
    }

    #[test]
    fn shutdown_immediately_after_requeue_completion_joins_any_started_next_attempt() {
        let (message_tx, message_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut first_release = Some(release_rx);
        let active_count = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let closure_active = Arc::clone(&active_count);
        let closure_max = Arc::clone(&max_active);
        let attempt_messages = message_tx.clone();
        let mut first = true;

        let worker = TestWorker::start_with(message_tx, move |request, completion_tx| {
            let is_first = std::mem::replace(&mut first, false);
            let release_rx = is_first.then(|| first_release.take().unwrap());
            let spawned_tx = spawned_tx.clone();
            let active_count = Arc::clone(&closure_active);
            let max_active = Arc::clone(&closure_max);
            let attempt_messages = attempt_messages.clone();

            spawn_with(request, completion_tx, move |cancellation| {
                let current = active_count.fetch_add(1, Ordering::AcqRel) + 1;
                max_active.fetch_max(current, Ordering::AcqRel);
                let _guard = ActiveCountGuard(active_count);
                attempt_messages
                    .send(Message::RunStarted {
                        run_id: request.run_id,
                        problem: request.problem,
                    })
                    .unwrap();
                spawned_tx
                    .send((request, Arc::clone(&cancellation)))
                    .unwrap();

                if let Some(release_rx) = release_rx {
                    release_rx.recv().unwrap();
                } else {
                    while !cancellation.is_requested() {
                        thread::yield_now();
                    }
                }

                run_attempt(&cancellation, |is_cancelled| {
                    assert!(is_cancelled());
                    Err(AppError::from(clean_cancellation_io_error()))
                })
            })
        })
        .unwrap();
        let request_tx = worker.sender();
        request_tx.send(request(0, 1)).unwrap();
        let (_, first_cancellation) = spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        request_tx.send(request(1, 2)).unwrap();
        wait_cancel_requested(&first_cancellation);
        release_tx.send(()).unwrap();

        messages_until(&message_rx, |message| {
            matches!(message, Message::RunRequeued { run_id: 1, .. })
        });
        worker.request_stop();
        worker.stop_and_join().unwrap();

        assert_eq!(active_count.load(Ordering::Acquire), 0);
        assert_eq!(max_active.load(Ordering::Acquire), 1);
        assert!(message_rx.try_iter().all(|message| {
            !matches!(
                message,
                Message::RunRequeued { .. }
                    | Message::RunCompleted { .. }
                    | Message::RunFailed { .. }
            )
        }));
    }

    #[test]
    fn shutdown_gate_rejects_requests_spawns_and_outcomes_after_linearization() {
        let control = WorkerControl::default();
        let called = AtomicBool::new(false);

        control.begin_shutdown();

        assert!(
            control
                .while_running(|| called.store(true, Ordering::Release))
                .is_none()
        );
        assert!(!called.load(Ordering::Acquire));
    }

    #[test]
    fn attempt_panic_is_worker_failure_and_does_not_start_next() {
        let (message_tx, message_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let mut first = true;
        let worker = TestWorker::start_with(message_tx, move |request, completion_tx| {
            spawned_tx.send(request).unwrap();
            if first {
                first = false;
                spawn_with(request, completion_tx, |_| panic!("fake panic"))
            } else {
                spawn_with(request, completion_tx, |cancellation| {
                    run_attempt(&cancellation, |_| Ok(()))
                })
            }
        })
        .unwrap();
        let request_tx = worker.sender();
        request_tx.send(request(0, 1)).unwrap();
        request_tx.send(request(1, 2)).unwrap();
        spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert!(matches!(
            message_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::WorkerFailed(error) if error.to_string().contains("panicked")
        ));
        assert!(spawned_rx.try_recv().is_err());
        assert!(
            worker
                .stop_and_join()
                .unwrap_err()
                .to_string()
                .contains("panicked")
        );
    }

    #[test]
    fn attempt_spawn_failure_stops_the_worker_without_starting_next() {
        let (message_tx, message_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let mut fail_first = true;
        let worker = TestWorker::start_with(message_tx, move |request, completion_tx| {
            if fail_first {
                fail_first = false;
                return Err(io::Error::other("fake spawn failure"));
            }

            let spawned_tx = spawned_tx.clone();
            spawn_with(request, completion_tx, move |cancellation| {
                spawned_tx.send(request).unwrap();
                run_attempt(&cancellation, |_| Ok(()))
            })
        })
        .unwrap();
        let request_tx = worker.sender();
        request_tx.send(request(0, 1)).unwrap();
        assert!(matches!(
            message_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::WorkerFailed(error) if error.to_string().contains("fake spawn failure")
        ));

        let _ = request_tx.send(request(1, 2));
        assert!(spawned_rx.try_recv().is_err());
        assert!(
            worker
                .stop_and_join()
                .unwrap_err()
                .to_string()
                .contains("fake spawn failure")
        );
    }

    #[test]
    fn request_channel_disconnect_is_shutdown_and_cancels_the_active_attempt() {
        let (request_tx, request_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut release_rx = Some(release_rx);
        let control = Arc::new(WorkerControl::default());

        let handle = thread::spawn({
            let control = Arc::clone(&control);
            move || {
                scheduler_loop(
                    request_rx,
                    completion_rx,
                    completion_tx,
                    control,
                    message_tx,
                    move |request, completion_tx| {
                        let spawned_tx = spawned_tx.clone();
                        let finished_tx = finished_tx.clone();
                        let release_rx = release_rx.take().unwrap();
                        spawn_with(request, completion_tx, move |cancellation| {
                            spawned_tx
                                .send((request, Arc::clone(&cancellation)))
                                .unwrap();
                            while !cancellation.is_requested() {
                                thread::yield_now();
                            }
                            release_rx.recv().unwrap();
                            let outcome = run_attempt(&cancellation, |is_cancelled| {
                                assert!(is_cancelled());
                                Err(AppError::from(clean_cancellation_io_error()))
                            });
                            finished_tx.send(()).unwrap();
                            outcome
                        })
                    },
                )
            }
        });

        request_tx.send(request(0, 1)).unwrap();
        let (spawned, cancellation) = spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(spawned, request(0, 1));
        request_tx.send(request(1, 2)).unwrap();
        wait_cancel_requested(&cancellation);
        drop(request_tx);
        release_tx.send(()).unwrap();

        handle.join().unwrap().unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(message_rx.try_iter().all(|message| {
            !matches!(
                message,
                Message::RunRequeued { .. }
                    | Message::RunCompleted { .. }
                    | Message::RunFailed { .. }
            )
        }));
    }

    #[test]
    fn message_channel_disconnect_joins_attempt_and_stops_worker() {
        let (message_tx, message_rx) = mpsc::channel();
        drop(message_rx);
        let (finished_tx, finished_rx) = mpsc::channel();
        let attempt_messages = message_tx.clone();
        let mut worker = TestWorker::start_with(message_tx, move |request, completion_tx| {
            let attempt_messages = attempt_messages.clone();
            let finished_tx = finished_tx.clone();
            spawn_with(request, completion_tx, move |cancellation| {
                let outcome = run_attempt(&cancellation, |_| {
                    attempt_messages
                        .send(Message::RunStarted {
                            run_id: request.run_id,
                            problem: request.problem,
                        })
                        .map_err(|_| {
                            AppError::from(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "fake message receiver disconnected",
                            ))
                        })
                });
                finished_tx.send(()).unwrap();
                outcome
            })
        })
        .unwrap();
        worker.sender().send(request(0, 1)).unwrap();

        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let error = worker.join().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn scheduler_helper_panic_is_reported_and_stops_without_next_spawn() {
        let (message_tx, message_rx) = mpsc::channel();
        let worker =
            TestWorker::start_with(message_tx, |_, _| panic!("fake scheduler helper panic"))
                .unwrap();
        let request_tx = worker.sender();
        request_tx.send(request(0, 1)).unwrap();
        let _ = request_tx.send(request(1, 2));

        assert!(matches!(
            message_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::WorkerFailed(error)
                if error.to_string().contains("scheduler loop panicked")
        ));
        assert!(
            worker
                .stop_and_join()
                .unwrap_err()
                .to_string()
                .contains("scheduler loop panicked")
        );
    }

    #[test]
    fn spawned_identity_mismatch_explicitly_cancels_and_joins_attempt() {
        let (message_tx, message_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = TestWorker::start_with(message_tx, move |_, completion_tx| {
            let physical = request(1, 99);
            let finished_tx = finished_tx.clone();
            spawn_with(physical, completion_tx, move |cancellation| {
                while !cancellation.is_requested() {
                    thread::yield_now();
                }
                let outcome = run_attempt(&cancellation, |is_cancelled| {
                    assert!(is_cancelled());
                    Err(AppError::from(clean_cancellation_io_error()))
                });
                finished_tx.send(()).unwrap();
                outcome
            })
        })
        .unwrap();
        worker.sender().send(request(0, 1)).unwrap();

        assert!(matches!(
            message_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::WorkerFailed(error) if error.to_string().contains("identity mismatch")
        ));
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            worker
                .stop_and_join()
                .unwrap_err()
                .to_string()
                .contains("identity mismatch")
        );
    }

    #[test]
    fn wrong_or_unexpected_completion_is_rejected_before_joining_outcome() {
        let mut scheduler = RunScheduler::default();
        scheduler.request_arrived(request(0, 1));
        let logical = scheduler.start_next().unwrap();
        let (completion_tx, _completion_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let physical = spawn_with(logical, completion_tx, move |cancellation| {
            while !cancellation.is_requested() {
                thread::yield_now();
            }
            let outcome = run_attempt(&cancellation, |is_cancelled| {
                assert!(is_cancelled());
                Err(AppError::from(clean_cancellation_io_error()))
            });
            finished_tx.send(()).unwrap();
            outcome
        })
        .unwrap();
        let mut active = Some(physical);
        let (message_tx, _message_rx) = mpsc::channel();
        let control = WorkerControl::default();

        let error = complete_attempt(
            AttemptCompletion {
                problem: 7,
                run_id: 8,
            },
            &mut scheduler,
            &mut active,
            &message_tx,
            &control,
        )
        .unwrap_err();
        assert!(error.to_string().contains("completion identity mismatch"));
        assert!(active.is_some());
        cleanup_active(&mut scheduler, active.take()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let mut no_active = None;
        let error = complete_attempt(
            AttemptCompletion {
                problem: 0,
                run_id: 1,
            },
            &mut scheduler,
            &mut no_active,
            &message_tx,
            &control,
        )
        .unwrap_err();
        assert!(error.to_string().contains("without a physical attempt"));
    }

    #[test]
    fn disconnected_completion_receiver_is_an_error_when_active_is_present() {
        let mut scheduler = RunScheduler::default();
        scheduler.request_arrived(request(0, 1));
        let logical = scheduler.start_next().unwrap();
        let (attempt_completion_tx, _attempt_completion_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let physical = spawn_with(logical, attempt_completion_tx, move |cancellation| {
            while !cancellation.is_requested() {
                thread::yield_now();
            }
            let outcome = run_attempt(&cancellation, |is_cancelled| {
                assert!(is_cancelled());
                Err(AppError::from(clean_cancellation_io_error()))
            });
            finished_tx.send(()).unwrap();
            outcome
        })
        .unwrap();
        let mut active = Some(physical);
        let (disconnected_tx, disconnected_rx) = mpsc::channel();
        drop(disconnected_tx);
        let (message_tx, _message_rx) = mpsc::channel();
        let control = WorkerControl::default();

        let error = process_ready_completion(
            &disconnected_rx,
            &mut scheduler,
            &mut active,
            &message_tx,
            &control,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        cleanup_active(&mut scheduler, active.take()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn thousand_rapid_requests_keep_one_physical_attempt_and_only_complete_latest_runs() {
        let (message_tx, message_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let allow_completion = Arc::new(AtomicBool::new(false));
        let active_count = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let closure_allow = Arc::clone(&allow_completion);
        let closure_active = Arc::clone(&active_count);
        let closure_max = Arc::clone(&max_active);
        let attempt_messages = message_tx.clone();

        let worker = TestWorker::start_with(message_tx, move |request, completion_tx| {
            let spawned_tx = spawned_tx.clone();
            let allow_completion = Arc::clone(&closure_allow);
            let active_count = Arc::clone(&closure_active);
            let max_active = Arc::clone(&closure_max);
            let attempt_messages = attempt_messages.clone();
            spawn_with(request, completion_tx, move |cancellation| {
                let current = active_count.fetch_add(1, Ordering::AcqRel) + 1;
                max_active.fetch_max(current, Ordering::AcqRel);
                let _guard = ActiveCountGuard(active_count);
                attempt_messages
                    .send(Message::RunStarted {
                        run_id: request.run_id,
                        problem: request.problem,
                    })
                    .unwrap();
                spawned_tx.send(request).unwrap();

                while !cancellation.is_requested() && !allow_completion.load(Ordering::Acquire) {
                    thread::yield_now();
                }

                if cancellation.is_requested() {
                    run_attempt(&cancellation, |is_cancelled| {
                        assert!(is_cancelled());
                        Err(AppError::from(clean_cancellation_io_error()))
                    })
                } else {
                    run_attempt(&cancellation, |_| Ok(()))
                }
            })
        })
        .unwrap();
        let request_tx = worker.sender();
        request_tx.send(request(0, 1)).unwrap();
        spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let mut latest = std::collections::HashMap::new();
        for run_id in 2..=1_001 {
            let problem = (run_id as usize) % 3;
            let request = RunRequest {
                run_id,
                problem,
                language: if run_id % 5 == 0 {
                    Language::Python
                } else {
                    Language::Cpp
                },
                debug: run_id % 2 == 0 && run_id % 5 != 0,
            };
            latest.insert(problem, request);
            request_tx.send(request).unwrap();
        }
        let sentinel = RunRequest {
            run_id: 1_002,
            problem: 3,
            language: Language::Cpp,
            debug: true,
        };
        latest.insert(sentinel.problem, sentinel);
        request_tx.send(sentinel).unwrap();

        loop {
            let spawned = spawned_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if spawned == sentinel {
                break;
            }
        }
        allow_completion.store(true, Ordering::Release);

        let expected: std::collections::HashSet<_> =
            latest.values().map(|request| request.run_id).collect();
        let mut completed = std::collections::HashSet::new();
        while completed.len() < expected.len() {
            match message_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Message::RunCompleted { run_id, .. } => {
                    assert!(expected.contains(&run_id));
                    assert!(completed.insert(run_id));
                }
                Message::RunFailed { run_id, error, .. } => {
                    panic!("unexpected failure for {run_id}: {error}")
                }
                _ => {}
            }
        }

        assert_eq!(completed, expected);
        worker.stop_and_join().unwrap();
        assert_eq!(active_count.load(Ordering::Acquire), 0);
        assert_eq!(max_active.load(Ordering::Acquire), 1);
    }

    #[test]
    fn repeated_preemption_outcomes_and_worker_lifecycles_do_not_leak_attempts() {
        for iteration in 0..30 {
            let fake = FakeWorker::start();
            let a_run = iteration * 10 + 1;
            let b_run = iteration * 10 + 2;
            fake.send(0, a_run);
            let a = fake.spawned();
            fake.send(1, b_run);
            wait_cancel_requested(&a.cancellation);

            match iteration % 3 {
                0 => {
                    a.finish(Finish::Cancelled);
                    let b = fake.spawned();
                    assert_eq!(b.request, request(1, b_run));
                    b.finish(Finish::Completed);
                    let a_again = fake.spawned();
                    assert_eq!(a_again.request, request(0, a_run));
                    a_again.finish(Finish::Completed);
                    messages_until(
                        &fake.message_rx,
                        |message| matches!(message, Message::RunCompleted { run_id, .. } if *run_id == a_run),
                    );
                }
                1 => {
                    a.finish(Finish::Completed);
                    let b = fake.spawned();
                    b.finish(Finish::Completed);
                    messages_until(
                        &fake.message_rx,
                        |message| matches!(message, Message::RunCompleted { run_id, .. } if *run_id == b_run),
                    );
                }
                _ => {
                    a.finish(Finish::Failed);
                    let b = fake.spawned();
                    b.finish(Finish::Completed);
                    messages_until(
                        &fake.message_rx,
                        |message| matches!(message, Message::RunCompleted { run_id, .. } if *run_id == b_run),
                    );
                }
            }

            assert_eq!(fake.max_active.load(Ordering::Acquire), 1);
            fake.stop();
        }
    }

    #[test]
    fn repeated_shutdown_during_preemption_never_starts_pending_work() {
        for iteration in 0..30 {
            let (message_tx, message_rx) = mpsc::channel();
            let (spawned_tx, spawned_rx) = mpsc::channel();
            let active_count = Arc::new(AtomicUsize::new(0));
            let release_cancelled_attempt = Arc::new(AtomicBool::new(false));
            let closure_active = Arc::clone(&active_count);
            let closure_release = Arc::clone(&release_cancelled_attempt);
            let mut worker = TestWorker::start_with(message_tx, move |request, completion_tx| {
                let spawned_tx = spawned_tx.clone();
                let active_count = Arc::clone(&closure_active);
                let release_cancelled_attempt = Arc::clone(&closure_release);
                spawn_with(request, completion_tx, move |cancellation| {
                    active_count.fetch_add(1, Ordering::AcqRel);
                    let _guard = ActiveCountGuard(active_count);
                    spawned_tx
                        .send((request, Arc::clone(&cancellation)))
                        .unwrap();
                    while !cancellation.is_requested() {
                        thread::yield_now();
                    }
                    while !release_cancelled_attempt.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                    run_attempt(&cancellation, |is_cancelled| {
                        assert!(is_cancelled());
                        Err(AppError::from(clean_cancellation_io_error()))
                    })
                })
            })
            .unwrap();
            let request_tx = worker.sender();
            request_tx.send(request(0, iteration * 10 + 1)).unwrap();
            let (_, cancellation) = spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            request_tx.send(request(1, iteration * 10 + 2)).unwrap();
            request_tx.send(request(2, iteration * 10 + 3)).unwrap();
            wait_cancel_requested(&cancellation);

            worker.request_stop();
            release_cancelled_attempt.store(true, Ordering::Release);
            worker.join().unwrap();

            assert_eq!(active_count.load(Ordering::Acquire), 0);
            assert!(spawned_rx.try_recv().is_err());
            assert!(message_rx.try_iter().all(|message| {
                !matches!(
                    message,
                    Message::RunRequeued { .. }
                        | Message::RunCompleted { .. }
                        | Message::RunFailed { .. }
                )
            }));
        }
    }
}
