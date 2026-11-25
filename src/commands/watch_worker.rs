use std::collections::{HashMap, HashSet};
use std::io;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::attempt::AttemptOutcome;
use crate::tui::message::{
    Message, RunGeneration, RunId, RunKind, RunRequest, RunResults, RunScope, RunTarget,
    RunWorkerCommand,
};
use crate::tui::run_results::RunResultStore;
#[cfg(test)]
use crate::{config::RunnerConfig, model::Problem};

use super::attempt_executor::{ActiveAttempt, AttemptCompletion, AttemptExecutor, RunContext};
use super::run_scheduler::{RequestArrival, RetiredActive, RunScheduler, StressCancellation};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_RUN_COMMANDS_PER_TICK: usize = 64;

pub(super) struct RunWorker {
    command_tx: Sender<RunWorkerCommand>,
    retire_tx: Sender<ScopeRetirement>,
    control: Arc<WorkerControl>,
    handle: Option<JoinHandle<io::Result<()>>>,
    results: RunResultStore,
}

struct RegisteredRun {
    context: Arc<RunContext>,
    results: RunResults,
}

type RunContexts = HashMap<RunScope, RegisteredRun>;

struct ScopeRetirement {
    scopes: HashSet<RunScope>,
    joined: Sender<()>,
}

struct WorkerControl {
    stopping: AtomicBool,
    lifecycle_gate: Mutex<RunContexts>,
}

impl WorkerControl {
    fn new(contexts: RunContexts) -> Self {
        Self {
            stopping: AtomicBool::new(false),
            lifecycle_gate: Mutex::new(contexts),
        }
    }

    fn begin_shutdown(&self) {
        let contexts = self
            .lifecycle_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.stopping.store(true, Ordering::Release);
        for entry in contexts.values() {
            entry.results.invalidate();
        }
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    fn while_running<T>(&self, action: impl FnOnce(&mut RunContexts) -> T) -> Option<T> {
        let mut contexts = self
            .lifecycle_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_stopping() {
            return None;
        }
        Some(action(&mut contexts))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestDrain {
    Open,
    Disconnected,
    Shutdown,
}

impl RunWorker {
    pub(super) fn start_workspace() -> io::Result<Self> {
        Self::launch(RunContexts::new(), |entry, request, completion| {
            AttemptExecutor::spawn(
                entry.context.clone(),
                entry.results.clone(),
                request,
                completion,
            )
        })
    }

    #[cfg(test)]
    pub fn start(
        destination: PathBuf,
        scope: RunScope,
        problems: Vec<Problem>,
        runner_config: RunnerConfig,
        message_tx: Sender<Message>,
    ) -> io::Result<Self> {
        let worker = Self::start_workspace()?;
        let context = RunContext::fixture(destination, scope, problems, runner_config);
        worker.register(context, message_tx.into())?;
        Ok(worker)
    }

    #[cfg(test)]
    fn start_with(
        scope: RunScope,
        message_tx: Sender<Message>,
        spawn_attempt: impl FnMut(RunRequest, Sender<AttemptCompletion>) -> io::Result<ActiveAttempt>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let mut spawn_attempt = spawn_attempt;
        Self::launch(
            Self::fixture_contexts(scope, message_tx),
            move |_, request, completion| spawn_attempt(request, completion),
        )
    }

    #[cfg(test)]
    fn fixture_contexts(scope: RunScope, message_tx: Sender<Message>) -> RunContexts {
        HashMap::from([(
            scope.clone(),
            RegisteredRun {
                context: Arc::new(RunContext::fixture(
                    PathBuf::new(),
                    scope,
                    Vec::new(),
                    RunnerConfig::default(),
                )),
                results: message_tx.into(),
            },
        )])
    }

    fn launch(
        contexts: RunContexts,
        spawn_attempt: impl FnMut(
            &RegisteredRun,
            RunRequest,
            Sender<AttemptCompletion>,
        ) -> io::Result<ActiveAttempt>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();
        let (retire_tx, retire_rx) = mpsc::channel();
        let control = Arc::new(WorkerControl::new(contexts));
        let thread_control = Arc::clone(&control);

        let handle = thread::Builder::new()
            .name("atc-watch-run".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    scheduler_loop(
                        command_rx,
                        completion_rx,
                        completion_tx,
                        Arc::clone(&thread_control),
                        retire_rx,
                        spawn_attempt,
                    )
                }));

                match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => {
                        report_worker_failure(&thread_control, &error);
                        Err(error)
                    }
                    Err(_) => {
                        let error = io::Error::other("run worker thread panicked");
                        report_worker_failure(&thread_control, &error);
                        Err(error)
                    }
                }
            })?;

        Ok(Self {
            command_tx,
            retire_tx,
            control,
            handle: Some(handle),
            results: RunResultStore::default(),
        })
    }

    #[cfg(test)]
    pub(super) fn start_with_test_attempt(
        scope: RunScope,
        message_tx: Sender<Message>,
        spawn_attempt: impl FnMut(RunRequest, Sender<AttemptCompletion>) -> io::Result<ActiveAttempt>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        Self::start_with(scope, message_tx, spawn_attempt)
    }

    pub fn sender(&self) -> Sender<RunWorkerCommand> {
        self.command_tx.clone()
    }

    pub(super) fn register_retained(
        &self,
        context: RunContext,
    ) -> io::Result<(RunGeneration, RunResults)> {
        self.register_shared(Arc::new(context))
    }

    pub(super) fn register_shared(
        &self,
        context: Arc<RunContext>,
    ) -> io::Result<(RunGeneration, RunResults)> {
        self.register_with(context, |scope| self.results.register(scope))
    }

    #[cfg(test)]
    pub(super) fn register(
        &self,
        context: RunContext,
        results: RunResults,
    ) -> io::Result<RunGeneration> {
        self.register_with(Arc::new(context), |_| results)
            .map(|(generation, _)| generation)
    }

    fn register_with(
        &self,
        context: Arc<RunContext>,
        make_results: impl FnOnce(&RunScope) -> RunResults,
    ) -> io::Result<(RunGeneration, RunResults)> {
        self.control
            .while_running(|contexts| {
                let generation = context.generation.clone();
                let scope = generation.scope();
                if contexts
                    .keys()
                    .any(|registered| registered.contest_id() == scope.contest_id())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "contest has a registered execution generation",
                    ));
                }
                let results = make_results(scope);
                contexts.insert(
                    scope.clone(),
                    RegisteredRun {
                        context,
                        results: results.clone(),
                    },
                );
                Ok((generation, results))
            })
            .ok_or_else(stopped_worker)?
    }

    pub(super) fn invalidate_and_join(&self, scope: &RunScope) -> io::Result<()> {
        self.invalidate_scopes_and_join(std::iter::once(scope.clone()).collect())
    }

    pub(super) fn invalidate_scopes_and_join(&self, scopes: HashSet<RunScope>) -> io::Result<()> {
        if scopes.is_empty() {
            return Ok(());
        }
        let (done, joined) = mpsc::channel();
        self.control
            .while_running(|contexts| {
                for scope in &scopes {
                    if let Some(entry) = contexts.remove(scope) {
                        entry.results.invalidate();
                    }
                }
                self.retire_tx
                    .send(ScopeRetirement {
                        scopes,
                        joined: done,
                    })
                    .map_err(|_| stopped_worker())
            })
            .ok_or_else(stopped_worker)??;
        joined.recv().map_err(|_| stopped_worker())
    }

    pub fn request_stop(&self) {
        self.control.begin_shutdown();
    }

    pub(super) fn is_stopping(&self) -> bool {
        self.control.is_stopping()
    }

    fn join(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };

        handle
            .join()
            .map_err(|_| io::Error::other("run worker thread panicked before reporting"))??;
        Ok(())
    }

    pub fn stop_and_join(mut self) -> io::Result<()> {
        self.request_stop();
        self.join()
    }
}

fn stopped_worker() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "workspace run worker is stopped")
}

impl Drop for RunWorker {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.join();
    }
}

fn scheduler_loop(
    command_rx: Receiver<RunWorkerCommand>,
    completion_rx: Receiver<AttemptCompletion>,
    completion_tx: Sender<AttemptCompletion>,
    control: Arc<WorkerControl>,
    retire_rx: Receiver<ScopeRetirement>,
    mut spawn_attempt: impl FnMut(
        &RegisteredRun,
        RunRequest,
        Sender<AttemptCompletion>,
    ) -> io::Result<ActiveAttempt>,
) -> io::Result<()> {
    let mut scheduler = RunScheduler::default();
    let mut active = None;

    let run_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scheduler_loop_inner(
            &command_rx,
            &completion_rx,
            &completion_tx,
            &control,
            &retire_rx,
            &mut spawn_attempt,
            &mut scheduler,
            &mut active,
        )
    }))
    .unwrap_or_else(|_| Err(io::Error::other("run worker scheduler loop panicked")));
    let cleanup_result = cleanup_active(&mut scheduler, active.take());

    combine_loop_and_cleanup_results(run_result, cleanup_result)
}

#[allow(clippy::too_many_arguments)]
fn scheduler_loop_inner(
    command_rx: &Receiver<RunWorkerCommand>,
    completion_rx: &Receiver<AttemptCompletion>,
    completion_tx: &Sender<AttemptCompletion>,
    control: &WorkerControl,
    retire_rx: &Receiver<ScopeRetirement>,
    spawn_attempt: &mut impl FnMut(
        &RegisteredRun,
        RunRequest,
        Sender<AttemptCompletion>,
    ) -> io::Result<ActiveAttempt>,
    scheduler: &mut RunScheduler,
    active: &mut Option<ActiveAttempt>,
) -> io::Result<()> {
    loop {
        if control.is_stopping() {
            return Ok(());
        }

        for retirement in retire_rx.try_iter() {
            if scheduler
                .active_request()
                .is_some_and(|request| retirement.scopes.contains(&request.target.scope))
            {
                cleanup_active(scheduler, active.take())?;
            }
            scheduler.discard_scopes(&retirement.scopes);
            let _ = retirement.joined.send(());
        }

        if process_ready_completion(completion_rx, scheduler, active, control)? {
            continue;
        }

        match drain_commands(command_rx, scheduler, active.as_ref(), control)? {
            RequestDrain::Open => {}
            RequestDrain::Disconnected | RequestDrain::Shutdown => return Ok(()),
        }

        // Requests are bounded above, and completion is checked both before and after the
        // batch. A request flood therefore cannot starve an already-finished attempt.
        if process_ready_completion(completion_rx, scheduler, active, control)? {
            continue;
        }

        if control.is_stopping() {
            return Ok(());
        }

        if active.is_none() {
            let spawned = control.while_running(|contexts| {
                let next = scheduler
                    .start_next_with_retirement(|request| cancel_result(contexts, request));
                next.map(|request| {
                    let attempt = contexts.get(&request.target.scope).and_then(|entry| {
                        let mut spawn =
                            || spawn_attempt(entry, request.clone(), completion_tx.clone());
                        match &request.kind {
                            RunKind::UserInput(snapshot) => snapshot
                                .start_gate
                                .start_if_current(snapshot.source_revision, spawn),
                            _ => Some(spawn()),
                        }
                    });
                    (request, attempt)
                })
            });

            match spawned {
                None => return Ok(()),
                Some(None) => {}
                Some(Some((request, None))) => {
                    // Removal won after start_next selected this logical request, but before
                    // any physical attempt was spawned. Retire its admitted state as well.
                    control.while_running(|contexts| cancel_result(contexts, &request));
                    let _retired = retire_matching(scheduler, request)?;
                    continue;
                }
                Some(Some((_request, Some(Ok(attempt))))) => {
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
                Some(Some((request, Some(Err(error))))) => {
                    let _retired = retire_matching(scheduler, request.clone())?;
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "failed to spawn physical attempt {:?}/{}: {error}",
                            request.target, request.run_id
                        ),
                    ));
                }
            }
        }

        match command_rx.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(command) => {
                let Some(result) = control.while_running(|contexts| {
                    process_command(contexts, scheduler, active.as_ref(), command)
                }) else {
                    return Ok(());
                };
                result?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn drain_commands(
    command_rx: &Receiver<RunWorkerCommand>,
    scheduler: &mut RunScheduler,
    active: Option<&ActiveAttempt>,
    control: &WorkerControl,
) -> io::Result<RequestDrain> {
    for _ in 0..MAX_RUN_COMMANDS_PER_TICK {
        match command_rx.try_recv() {
            Ok(command) => {
                let Some(result) = control.while_running(|contexts| {
                    process_command(contexts, scheduler, active, command)
                }) else {
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

fn process_command(
    contexts: &RunContexts,
    scheduler: &mut RunScheduler,
    active: Option<&ActiveAttempt>,
    command: RunWorkerCommand,
) -> io::Result<()> {
    let Some(entry) = contexts.get(&command.target().scope) else {
        return Ok(());
    };
    match command {
        RunWorkerCommand::Run(request) => {
            let Some(sample_count) = entry.context.sample_count(&request.target.task_id) else {
                return Ok(());
            };
            if entry.context.is_stale() {
                entry.results.admitted(&request, sample_count);
                return entry.results.send(Message::RunFailed {
                    run_id: request.run_id,
                    target: request.target,
                    error: "contest data or Workspace route changed; reopen the contest and retry"
                        .into(),
                });
            }

            let previous_foreground = scheduler
                .foreground_request()
                .filter(|request| !request.kind.preserve_on_preemption())
                .cloned();
            let previous_active = scheduler
                .active_request()
                .filter(|request| !request.kind.preserve_on_preemption());
            let RequestArrival::Accepted { cancel_active } =
                scheduler.request_arrived(request.clone())
            else {
                return Ok(());
            };
            for displaced in [previous_foreground, previous_active].iter().flatten() {
                cancel_result(contexts, displaced);
            }
            entry.results.admitted(&request, sample_count);
            if cancel_active {
                let active = active.ok_or_else(|| {
                    io::Error::other("scheduler requested cancellation without a physical attempt")
                })?;
                ensure_identity(scheduler, active)?;
                active.request_cancel();
            }
            Ok(())
        }
        RunWorkerCommand::CancelStress { target, run_id } => {
            if process_stress_cancellation(scheduler, active, &target, run_id)? {
                entry.results.cancel(&target, run_id);
            }
            Ok(())
        }
        RunWorkerCommand::RetireUserInputRuns {
            target,
            before_source_revision,
        } => {
            if entry.context.is_workspace_watch() {
                let running = scheduler.active_request();
                let stress = [scheduler.foreground_request(), running.as_ref()]
                    .into_iter()
                    .flatten()
                    .find(|request| {
                        request.target == target && matches!(request.kind, RunKind::Stress { .. })
                    })
                    .map(|request| request.run_id);
                if let Some(run_id) = stress
                    && process_stress_cancellation(scheduler, active, &target, run_id)?
                {
                    entry.results.cancel(&target, run_id);
                }
            }
            let waiting = scheduler
                .foreground_request()
                .filter(|request| {
                    request.target == target
                        && matches!(&request.kind, RunKind::UserInput(snapshot)
                    if snapshot.source_revision < before_source_revision)
                })
                .cloned();
            scheduler.retire_user_input_runs(&target, before_source_revision);
            entry.results.source_removed(&target);
            if let Some(waiting) = waiting {
                entry.results.cancelled(&waiting);
            }
            Ok(())
        }
    }
}

fn cancel_result(contexts: &RunContexts, request: &RunRequest) {
    if let Some(entry) = contexts.get(&request.target.scope) {
        entry.results.cancelled(request);
    }
}

fn process_stress_cancellation(
    scheduler: &mut RunScheduler,
    active: Option<&ActiveAttempt>,
    target: &RunTarget,
    run_id: RunId,
) -> io::Result<bool> {
    match scheduler.cancel_stress(target, run_id) {
        StressCancellation::Ignored => Ok(false),
        StressCancellation::Accepted {
            cancel_active: false,
        } => Ok(true),
        StressCancellation::Accepted {
            cancel_active: true,
        } => {
            let active = active.ok_or_else(|| {
                io::Error::other(
                    "scheduler requested stress cancellation without a physical attempt",
                )
            })?;
            ensure_identity(scheduler, active)?;
            active.request_cancel();
            Ok(true)
        }
    }
}

fn process_ready_completion(
    completion_rx: &Receiver<AttemptCompletion>,
    scheduler: &mut RunScheduler,
    active: &mut Option<ActiveAttempt>,
    control: &WorkerControl,
) -> io::Result<bool> {
    match completion_rx.try_recv() {
        Ok(completion) => {
            if control.while_running(|contexts| contexts.contains_key(&completion.target.scope))
                == Some(true)
            {
                complete_attempt(completion, scheduler, active, control)?;
            }
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
    control: &WorkerControl,
) -> io::Result<()> {
    let physical = active.as_ref().ok_or_else(|| {
        io::Error::other(format!(
            "completion arrived without a physical attempt: {:?}/{}",
            completion.target, completion.run_id
        ))
    })?;
    let request = physical.request();
    validate_completion(&completion, &request)?;
    ensure_identity(scheduler, physical)?;

    // Join is authoritative and is the no-late-event boundary. Logical messages and the next
    // spawn are deliberately impossible before this call returns.
    let physical = active.take().expect("active was checked above");
    let outcome = physical.join()?;
    finish_joined_attempt(request, outcome, scheduler, control)
}

fn finish_joined_attempt(
    request: RunRequest,
    outcome: AttemptOutcome,
    scheduler: &mut RunScheduler,
    control: &WorkerControl,
) -> io::Result<()> {
    let retired = retire_matching(scheduler, request.clone())?;

    // Shutdown and logical outcome publication are serialized by the same lifecycle gate.
    // Whichever obtains it first is the linearization point. Once shutdown wins, no terminal
    // message or requeue can be published and no next attempt can be spawned.
    let Some(result) = control.while_running(|contexts| {
        let Some(entry) = contexts.get(&request.target.scope) else {
            return Ok(());
        };
        if entry.context.is_stale() {
            entry.results.discard_completed();
            for discarded in scheduler.discard_scope(&request.target.scope) {
                entry.results.send(Message::RunFailed {
                    run_id: discarded.run_id,
                    target: discarded.target,
                    error: "contest data or Workspace route changed; reopen the contest and retry"
                        .into(),
                })?;
            }
        }
        match outcome {
            AttemptOutcome::Completed if retired.is_latest() => {
                entry.results.send(Message::RunCompleted {
                    run_id: request.run_id,
                    target: request.target,
                })
            }
            AttemptOutcome::Failed(error) if retired.is_latest() => {
                entry.results.send(Message::RunFailed {
                    run_id: request.run_id,
                    target: request.target,
                    error: error.to_string(),
                })
            }
            AttemptOutcome::Cancelled if !entry.context.is_stale() => {
                if scheduler.requeue_retired(retired) {
                    entry.results.send(Message::RunRequeued {
                        run_id: request.run_id,
                        target: request.target,
                    })?;
                } else {
                    entry.results.cancelled(&request);
                }
                Ok(())
            }
            AttemptOutcome::Completed | AttemptOutcome::Failed(_) | AttemptOutcome::Cancelled => {
                Ok(())
            }
        }
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
            "run worker failed: {run_error}; active attempt cleanup also failed: {cleanup_error}"
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
            "logical/physical attempt identity mismatch: logical={:?}/{}, physical={:?}/{}",
            logical.target, logical.run_id, physical.target, physical.run_id
        )));
    }

    Ok(())
}

fn validate_completion(completion: &AttemptCompletion, request: &RunRequest) -> io::Result<()> {
    if request.matches(&completion.target, completion.run_id) {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "attempt completion identity mismatch: completion={:?}/{}, physical={:?}/{}",
        completion.target, completion.run_id, request.target, request.run_id
    )))
}

fn retire_matching(scheduler: &mut RunScheduler, request: RunRequest) -> io::Result<RetiredActive> {
    let retired = scheduler.retire_active().ok_or_else(|| {
        io::Error::other("physical attempt finished without logical active state")
    })?;

    if retired.request() != request {
        return Err(io::Error::other(format!(
            "retired/physical attempt identity mismatch: retired={:?}/{}, physical={:?}/{}",
            retired.request().target,
            retired.request().run_id,
            request.target,
            request.run_id
        )));
    }

    Ok(retired)
}

fn report_worker_failure(control: &WorkerControl, error: &io::Error) {
    control.while_running(|contexts| {
        control.stopping.store(true, Ordering::Release);
        for entry in contexts.values() {
            let _ = entry.results.send(Message::WorkerFailed(io::Error::new(
                error.kind(),
                error.to_string(),
            )));
            entry.results.invalidate();
        }
    });
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
        worker: RunWorker,
        request_tx: Sender<RunWorkerCommand>,
        spawned_rx: Receiver<SpawnedAttempt>,
        message_rx: Receiver<Message>,
        active_count: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl FakeWorker {
        fn start() -> Self {
            let (message_tx, message_rx) = mpsc::channel();
            Self::launch(
                RunWorker::fixture_contexts(RunScope::fixture(), message_tx),
                message_rx,
            )
        }

        fn launch(contexts: RunContexts, message_rx: Receiver<Message>) -> Self {
            let (spawned_tx, spawned_rx) = mpsc::channel();
            let active_count = Arc::new(AtomicUsize::new(0));
            let max_active = Arc::new(AtomicUsize::new(0));
            let closure_active = Arc::clone(&active_count);
            let closure_max = Arc::clone(&max_active);

            let worker = RunWorker::launch(contexts, move |entry, request, completion_tx| {
                let messages = entry.results.clone();
                let spawned_tx = spawned_tx.clone();
                let active_count = Arc::clone(&closure_active);
                let max_active = Arc::clone(&closure_max);
                let (finish_tx, finish_rx) = mpsc::channel();

                spawn_with(request.clone(), completion_tx, move |cancellation| {
                    let current = active_count.fetch_add(1, Ordering::AcqRel) + 1;
                    max_active.fetch_max(current, Ordering::AcqRel);
                    let _guard = ActiveCountGuard(active_count);

                    messages
                        .send(Message::RunStarted {
                            run_id: request.run_id,
                            target: request.target.clone(),
                        })
                        .unwrap();
                    spawned_tx
                        .send(SpawnedAttempt {
                            request: request.clone(),
                            cancellation: Arc::clone(&cancellation),
                            finish_tx,
                        })
                        .unwrap();

                    let finish = finish_rx.recv().unwrap();
                    messages
                        .send(Message::RunEvent {
                            run_id: request.run_id,
                            target: request.target.clone(),
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
            send_run(&self.request_tx, request(problem, run_id));
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
            target: RunTarget::fixture(problem),
            language: Language::Cpp,
            debug: false,
            kind: crate::tui::message::RunKind::Samples,
        }
    }

    fn scoped_sample(scope: &RunScope, task: &str, run_id: u64) -> RunRequest {
        RunRequest {
            target: scope.target(task),
            ..request(0, run_id)
        }
    }

    fn register_fake(worker: &RunWorker, scope: &RunScope, results: RunResults) {
        let context = RunContext::fixture(
            PathBuf::new(),
            scope.clone(),
            Vec::new(),
            RunnerConfig::default(),
        );
        assert_eq!(worker.register(context, results).unwrap().scope(), scope);
    }

    fn register_retained_fixture(
        worker: &RunWorker,
        contest_id: &str,
    ) -> (crate::model::Contest, RunGeneration, RunResults) {
        let contest = crate::model::Contest {
            contest_id: contest_id.to_owned(),
            problems: vec![Problem {
                index: "A".into(),
                title: "A".into(),
                task_id: "same_task".into(),
                url: String::new(),
                sample_count: 1,
            }],
        };
        let context = RunContext::fixture(
            PathBuf::new(),
            RunScope::new(contest_id),
            contest.problems.clone(),
            RunnerConfig::default(),
        );
        let (generation, results) = worker.register_retained(context).unwrap();
        (contest, generation, results)
    }

    fn retained_view(
        fixture: &(crate::model::Contest, RunGeneration, RunResults),
    ) -> (crate::tui::app::WatchApp, Receiver<Message>) {
        use crate::tui::app::{PersistedUserInputState, UserInputState, WatchApp};
        let (contest, generation, results) = fixture;
        let (tx, rx) = mpsc::channel();
        let mut app = WatchApp::new_with_run_generation(
            contest,
            generation.clone(),
            vec![1],
            vec![None],
            vec![UserInputState::loaded(vec![PersistedUserInputState {
                id: 7,
                content: "old stdin".into(),
            }])],
        )
        .unwrap();
        app.attach_run_view(results.attach(tx).unwrap()).unwrap();
        (app, rx)
    }

    #[test]
    fn hidden_contest_restores_queued_running_progress_and_completed_results() {
        use crate::tui::app::{CaseVerdict, RunPhase};
        let (_tx, rx) = mpsc::channel();
        let fake = FakeWorker::launch(RunContexts::new(), rx);
        let a = register_retained_fixture(&fake.worker, "abc483");
        let b = register_retained_fixture(&fake.worker, "abc480");
        let first = scoped_sample(a.1.scope(), "same_task", 1);
        let hidden = RunRequest {
            language: Language::Python,
            ..scoped_sample(b.1.scope(), "same_task", 1)
        };
        send_run(&fake.request_tx, first.clone());
        let active = fake.spawned();
        send_run(&fake.request_tx, hidden.clone());
        wait_cancel_requested(&active.cancellation);
        let (queued, _) = retained_view(&b);
        assert_eq!(
            queued.current_problem().unwrap().run.phase,
            RunPhase::Queued
        );
        drop(queued);
        active.finish(Finish::Cancelled);

        let active = fake.spawned();
        assert_eq!(active.request, hidden);
        let (requeued, _) = retained_view(&a);
        assert_eq!(
            requeued.current_problem().unwrap().run.phase,
            RunPhase::Queued
        );
        assert_eq!(
            requeued.current_problem().unwrap().run.id,
            Some(first.run_id)
        );
        drop(requeued);
        let (running, _) = retained_view(&b);
        assert_eq!(
            running.current_problem().unwrap().run.phase,
            RunPhase::Running
        );
        drop(running);

        let event = |event| {
            b.2.send(Message::RunEvent {
                target: hidden.target.clone(),
                run_id: hidden.run_id,
                event,
            })
            .unwrap()
        };
        event(TestEvent::TestCaseLayout {
            sample_cases: 1,
            stress_case: None,
        });
        event(TestEvent::TestRunStarted { total_cases: 1 });
        event(TestEvent::TestCaseAccepted {
            number: 1,
            elapsed: Duration::from_millis(4),
        });
        event(TestEvent::TestCaseComparison {
            number: 1,
            input: "stdin".into(),
            expected: "abc480".into(),
            actual: "abc480".into(),
        });
        let (progress, _) = retained_view(&b);
        assert_eq!(
            progress.current_problem().unwrap().run.cases[0].verdict,
            CaseVerdict::Accepted
        );
        drop(progress);
        event(TestEvent::TestRunFinished {
            accepted: 1,
            total_cases: 1,
        });
        active.finish(Finish::Completed);

        let reattempt = fake.spawned();
        let actual = reattempt.request.clone();
        reattempt.finish(Finish::Completed);
        assert_eq!(actual, first);
        let (mut restored, _) = retained_view(&b);
        let result = &restored.current_problem().unwrap().run;
        assert_eq!(result.phase, RunPhase::Finished);
        assert_eq!(result.accepted, 1);
        assert_eq!(
            result.cases[0].actual.as_deref().map(String::as_str),
            Some("abc480")
        );
        assert!(!restored.apply_execution_updates());
        fake.stop();
    }

    #[test]
    fn hidden_one_shot_attempts_are_cancelled_by_cross_contest_preemption() {
        use crate::tui::app::{StressPhase, UserInputSelection};
        use crate::tui::message::UserInputRunStatus;
        for kind in [stress_request(0, 1).kind, user_input_request(0, 1, 0).kind] {
            let (_tx, rx) = mpsc::channel();
            let fake = FakeWorker::launch(RunContexts::new(), rx);
            let first = register_retained_fixture(&fake.worker, "abc483");
            let other = register_retained_fixture(&fake.worker, "abc480");
            let is_stress = matches!(kind, RunKind::Stress { .. });
            let kind = match kind {
                RunKind::UserInput(snapshot) => {
                    RunKind::UserInput(Arc::new(crate::tui::message::UserInputRunSnapshot {
                        problem_index: "A".into(),
                        ..(*snapshot).clone()
                    }))
                }
                kind => kind,
            };
            let request = RunRequest {
                kind,
                ..scoped_sample(first.1.scope(), "same_task", 1)
            };
            let next = scoped_sample(other.1.scope(), "same_task", 1);
            send_run(&fake.request_tx, request);
            let active = fake.spawned();
            send_run(&fake.request_tx, next.clone());
            wait_cancel_requested(&active.cancellation);
            active.finish(Finish::Cancelled);
            let active = fake.spawned();
            let actual = active.request.clone();
            active.finish(Finish::Completed);
            assert_eq!(actual, next);

            let (app, _) = retained_view(&first);
            let problem = app.current_problem().unwrap();
            if is_stress {
                assert_eq!(problem.stress.phase, StressPhase::Cancelled);
            } else {
                let run = problem
                    .user_inputs
                    .ready()
                    .unwrap()
                    .last_run(UserInputSelection::Persisted(7))
                    .unwrap();
                assert_eq!(run.status, UserInputRunStatus::Cancelled);
            }
            assert!(fake.spawned_rx.try_recv().is_err());
            fake.stop();
        }
    }

    #[test]
    fn workspace_executor_uses_each_registered_contest_and_rejects_changed_metadata() {
        use crate::{
            model::{Contest, Sample},
            workspace,
        };

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
        let python = ["python3", "python"]
            .into_iter()
            .find(|name| {
                std::process::Command::new(name)
                    .arg("--version")
                    .output()
                    .is_ok_and(|output| output.status.success())
            })
            .expect("runner integration tests require Python");
        let config = RunnerConfig {
            python: python.into(),
            ..RunnerConfig::default()
        };
        let worker = RunWorker::start_workspace().unwrap();
        let sender = worker.sender();
        let register = |contest_id: &str| {
            let destination = root.path().join(contest_id);
            let task_id = format!("{contest_id}_a");
            let contest = Contest {
                contest_id: contest_id.into(),
                problems: vec![Problem {
                    index: "A".into(),
                    title: "Problem A".into(),
                    url: format!("https://atcoder.jp/contests/{contest_id}/tasks/{task_id}"),
                    task_id,
                    sample_count: 1,
                }],
            };
            workspace::save_metadata(&destination, &contest).unwrap();
            workspace::save_samples(
                &destination,
                &contest.problems[0],
                &[Sample {
                    input: String::new(),
                    output: format!("{contest_id}\n"),
                }],
            )
            .unwrap();
            std::fs::write(destination.join("A.py"), format!("print('{contest_id}')\n")).unwrap();
            let (tx, rx) = mpsc::channel();
            let generation = worker
                .register(
                    RunContext::new(
                        destination.clone(),
                        &contest,
                        config.clone(),
                        Some(root.path()),
                    )
                    .unwrap(),
                    tx.into(),
                )
                .unwrap();
            let request = RunRequest {
                language: Language::Python,
                debug: true,
                ..scoped_sample(generation.scope(), &contest.problems[0].task_id, 1)
            };
            (request, rx, contest, destination)
        };

        let (a, a_rx, mut contest, destination) = register("abc483");
        let (b, b_rx, _, b_destination) = register("abc480");
        let assert_correct_output = |request: &RunRequest, rx: &Receiver<Message>| {
            send_run(&sender, request.clone());
            let events = messages_until(rx, |message| match message {
                Message::RunCompleted { target, run_id } => request.matches(target, *run_id),
                Message::RunFailed { error, .. } => panic!("registered contest failed: {error}"),
                _ => false,
            });
            assert!(events.iter().any(|message| matches!(message,
                Message::RunEvent { target, run_id, event: TestEvent::TestCaseComparison { actual, .. } }
                if request.matches(target, *run_id) && actual.trim() == request.target.scope.contest_id()
            )));
        };
        assert_correct_output(&a, &a_rx);
        assert_correct_output(&b, &b_rx);

        // An unrelated mapping must leave both effective routes intact.
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\n[[paths]]\npattern = '^arc'\npath = 'arc'\n",
        )
        .unwrap();
        assert_correct_output(
            &RunRequest {
                run_id: 2,
                ..b.clone()
            },
            &b_rx,
        );

        contest.problems[0].title = "Replaced metadata".into();
        workspace::save_metadata(&destination, &contest).unwrap();
        std::fs::write(
            destination.join("A.py"),
            "open('unexpected.txt', 'w').close()\n",
        )
        .unwrap();
        for run_id in 2..=3 {
            let stale = RunRequest {
                run_id,
                ..a.clone()
            };
            send_run(&sender, stale.clone());
            let events = messages_until(&a_rx, |message| {
                matches!(message,
                    Message::RunFailed { target, run_id, .. } if stale.matches(target, *run_id)
                )
            });
            assert!(
                !events
                    .iter()
                    .any(|message| matches!(message, Message::RunEvent { .. }))
            );
        }
        assert!(!destination.join("unexpected.txt").exists());
        assert_correct_output(
            &RunRequest {
                run_id: 3,
                ..b.clone()
            },
            &b_rx,
        );

        let moved = root.path().join("moved");
        std::fs::create_dir(&moved).unwrap();
        std::fs::rename(&b_destination, moved.join("abc480")).unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\n[[paths]]\npattern = '^abc480$'\npath = 'moved'\n",
        )
        .unwrap();
        send_run(&sender, RunRequest { run_id: 4, ..b });
        messages_until(&b_rx, |message| {
            matches!(message,
                Message::RunFailed { run_id: 4, error, .. } if error.contains("Workspace ownership changed")
            )
        });
        worker.stop_and_join().unwrap();
    }

    #[test]
    fn workspace_prefers_latest_then_fifo_and_requeues_after_view_detaches() {
        let (a_tx, a_rx) = mpsc::channel();
        let a_results = RunResults::from(a_tx);
        let fake = FakeWorker::launch(RunContexts::new(), a_rx);
        let (other_tx, other_rx) = mpsc::channel();
        let a_scope = RunScope::new("abc483");
        let b_scope = RunScope::new("abc480");
        let c_scope = RunScope::new("abc478");
        register_fake(&fake.worker, &a_scope, a_results.clone());
        register_fake(&fake.worker, &b_scope, other_tx.clone().into());
        register_fake(&fake.worker, &c_scope, other_tx.into());
        let a = scoped_sample(&a_scope, "a", 1);
        let b = scoped_sample(&b_scope, "a", 1);
        let c = scoped_sample(&c_scope, "a", 1);

        send_run(&fake.request_tx, a.clone());
        let active = fake.spawned();
        assert_eq!(active.request, a);
        assert!(
            matches!(fake.message_rx.recv().unwrap(), Message::RunStarted { target, .. } if target == a.target)
        );
        a_results.invalidate();
        send_run(&fake.request_tx, b.clone());
        wait_cancel_requested(&active.cancellation);
        send_run(&fake.request_tx, c.clone());
        active.finish(Finish::Cancelled);

        for expected in [&c, &b, &a] {
            let spawn = fake.spawned();
            let actual = spawn.request.clone();
            spawn.finish(Finish::Completed);
            assert_eq!(&actual, expected);
        }
        assert!(fake.message_rx.try_recv().is_err());
        let delivered = messages_until(
            &other_rx,
            |message| matches!(message, Message::RunCompleted { target, .. } if *target == b.target),
        );
        assert!(delivered.iter().any(|message| {
            matches!(message, Message::RunCompleted { target, .. } if *target == c.target)
        }));
        assert_eq!(fake.max_active.load(Ordering::Acquire), 1);
        fake.stop();
    }

    #[test]
    fn scope_retirement_joins_matching_attempt_and_rejects_old_commands_and_completion() {
        let fake = FakeWorker::start();
        let old_scope = RunScope::fixture();
        let other_scope = RunScope::new("abc480");
        let (other_tx, other_rx) = mpsc::channel();
        register_fake(&fake.worker, &other_scope, other_tx.into());
        fake.send(0, 1);
        let active = fake.spawned();
        let cancellation = Arc::clone(&active.cancellation);
        // Hold the physical attempt while retirement runs. Its completion will arrive after removal.
        let (joined_tx, joined_rx) = mpsc::channel();
        thread::scope(|threads| {
            let worker = &fake.worker;
            let old_scope = &old_scope;
            threads.spawn(move || {
                joined_tx
                    .send(worker.invalidate_and_join(old_scope))
                    .unwrap()
            });
            wait_cancel_requested(&cancellation);
            let early_ack = joined_rx.try_recv();
            send_run(&fake.request_tx, request(1, 2));
            send_run(&fake.request_tx, scoped_sample(&other_scope, "a", 1));
            active.finish(Finish::Cancelled);
            assert!(matches!(early_ack, Err(TryRecvError::Empty)));
            joined_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
        });

        let other = fake.spawned();
        let actual = other.request.clone();
        other.finish(Finish::Completed);
        assert_eq!(actual, scoped_sample(&other_scope, "a", 1));
        messages_until(&other_rx, |message| {
            matches!(message, Message::RunCompleted { .. })
        });
        assert_no_logical_message(&fake.message_rx, 1);
        assert_no_logical_message(&fake.message_rx, 2);

        let new_scope = RunScope::new(old_scope.contest_id());
        let (new_tx, new_rx) = mpsc::channel();
        register_fake(&fake.worker, &new_scope, new_tx.into());
        let fresh = RunRequest {
            target: new_scope.target("abc123_0"),
            ..stress_request(0, 1)
        };
        send_run(&fake.request_tx, fresh.clone());
        let fresh_attempt = fake.spawned();
        fake.request_tx
            .send(RunWorkerCommand::CancelStress {
                target: request(0, 1).target,
                run_id: 1,
            })
            .unwrap();
        let actual = fresh_attempt.request.clone();
        let cancellation = fresh_attempt.cancellation.clone();
        fresh_attempt.finish(Finish::Completed);
        assert_eq!(actual, fresh);
        messages_until(
            &new_rx,
            |message| matches!(message, Message::RunCompleted { target, run_id: 1 } if *target == fresh.target),
        );
        assert!(!cancellation.is_requested());
        assert!(fake.spawned_rx.try_recv().is_err());
        fake.stop();
    }

    #[test]
    fn workspace_shutdown_joins_active_and_drops_other_contests_work() {
        let fake = FakeWorker::start();
        let other_scope = RunScope::new("abc480");
        let (other_tx, other_rx) = mpsc::channel();
        register_fake(&fake.worker, &other_scope, other_tx.into());
        fake.send(0, 1);
        let active = fake.spawned();
        send_run(&fake.request_tx, scoped_sample(&other_scope, "a", 1));
        wait_cancel_requested(&active.cancellation);

        fake.worker.request_stop();
        let new_scope = RunScope::new("abc478");
        let new_context = RunContext::fixture(
            PathBuf::new(),
            new_scope,
            Vec::new(),
            RunnerConfig::default(),
        );
        assert_eq!(
            fake.worker
                .register(new_context, RunResults::default())
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        active.finish(Finish::Cancelled);
        let FakeWorker {
            worker,
            request_tx,
            spawned_rx,
            message_rx,
            active_count,
            ..
        } = fake;
        worker.stop_and_join().unwrap();
        assert_eq!(active_count.load(Ordering::Acquire), 0);
        assert!(spawned_rx.try_recv().is_err());
        assert!(other_rx.try_recv().is_err());
        assert_no_logical_message(&message_rx, 1);
        assert!(
            request_tx
                .send(RunWorkerCommand::Run(request(0, 2)))
                .is_err()
        );
    }

    fn start_fixture_worker(
        message_tx: Sender<Message>,
        spawn_attempt: impl FnMut(RunRequest, Sender<AttemptCompletion>) -> io::Result<ActiveAttempt>
        + Send
        + 'static,
    ) -> io::Result<RunWorker> {
        RunWorker::start_with(RunScope::fixture(), message_tx, spawn_attempt)
    }

    fn stress_request(problem: usize, run_id: u64) -> RunRequest {
        RunRequest {
            kind: crate::tui::message::RunKind::Stress {
                base_seed: 42,
                count: None,
            },
            ..request(problem, run_id)
        }
    }

    fn user_input_request(problem: usize, run_id: u64, source_revision: u64) -> RunRequest {
        use crate::tui::message::{UserInputRunSnapshot, UserInputRunTarget};
        RunRequest {
            kind: RunKind::UserInput(Arc::new(UserInputRunSnapshot {
                problem_index: format!("P{problem}"),
                target: UserInputRunTarget::Persisted(7),
                input: Arc::from("old stdin"),
                source_revision,
                start_gate: Default::default(),
            })),
            ..request(problem, run_id)
        }
    }

    fn retire_user_inputs(
        sender: &Sender<RunWorkerCommand>,
        problem: usize,
        before_source_revision: u64,
    ) {
        sender
            .send(RunWorkerCommand::RetireUserInputRuns {
                target: RunTarget::fixture(problem),
                before_source_revision,
            })
            .unwrap();
    }

    #[test]
    fn source_removal_retires_worker_sent_waiting_user_input_and_preserves_sample_requeue() {
        // Exercise the command independently of the shared gate. X remains physically alive
        // until the test releases it, proving A was handed off but could not already start.
        let fake = FakeWorker::start();
        fake.send(1, 1);
        let x = fake.spawned();
        send_run(&fake.request_tx, user_input_request(0, 2, 1));
        wait_cancel_requested(&x.cancellation);
        retire_user_inputs(&fake.request_tx, 0, 2);
        x.finish(Finish::Cancelled);

        let resumed = fake.spawned();
        let actual = resumed.request.clone();
        resumed.finish(Finish::Completed);
        assert_eq!(actual, request(1, 1), "obsolete User Input must not spawn");
        messages_until(&fake.message_rx, |message| {
            matches!(
                message,
                Message::RunCompleted { target, run_id: 1 } if target == &RunTarget::fixture(1)
            )
        });
        assert!(fake.spawned_rx.try_recv().is_err());
        assert_eq!(fake.max_active.load(Ordering::Acquire), 1);
        fake.stop();
    }

    #[test]
    fn source_removal_gate_blocks_waiting_input_even_before_retire_command_delivery() {
        let fake = FakeWorker::start();
        fake.send(1, 1);
        let x = fake.spawned();
        let old = user_input_request(0, 2, 1);
        send_run(&fake.request_tx, old.clone());
        wait_cancel_requested(&x.cancellation);
        let RunKind::UserInput(snapshot) = old.kind else {
            unreachable!()
        };
        snapshot.start_gate.retire_before(2); // The synchronous SourceRemoved boundary.
        // Deliberately withhold the command until after X finishes and scheduling resumes.
        x.finish(Finish::Cancelled);
        let resumed = fake.spawned();
        let actual = resumed.request.clone();
        resumed.finish(Finish::Completed);
        assert_eq!(
            actual,
            request(1, 1),
            "channel latency must not admit the old input"
        );
        messages_until(&fake.message_rx, |message| {
            matches!(
                message,
                Message::RunCompleted { target, run_id: 1 } if target == &RunTarget::fixture(1)
            )
        });
        retire_user_inputs(&fake.request_tx, 0, 2);
        assert!(fake.spawned_rx.try_recv().is_err());
        fake.stop();
    }

    #[test]
    fn source_removal_recreate_and_delayed_retirement_preserve_new_user_input() {
        for delayed in [false, true] {
            let fake = FakeWorker::start();
            fake.send(2, 1);
            let x = fake.spawned();
            send_run(&fake.request_tx, user_input_request(0, 2, 1));
            wait_cancel_requested(&x.cancellation);
            retire_user_inputs(&fake.request_tx, 0, 2);
            let fresh = user_input_request(0, 3, 3);
            send_run(&fake.request_tx, fresh.clone());
            if delayed {
                retire_user_inputs(&fake.request_tx, 0, 2);
            }
            x.finish(Finish::Cancelled);
            let started = fake.spawned();
            let actual = started.request.clone();
            started.finish(Finish::Completed);
            assert_eq!(actual, fresh);
            let resumed = fake.spawned();
            let actual = resumed.request.clone();
            resumed.finish(Finish::Completed);
            assert_eq!(actual, request(2, 1));
            messages_until(&fake.message_rx, |message| {
                matches!(
                    message,
                    Message::RunCompleted { target, run_id: 1 } if target == &RunTarget::fixture(2)
                )
            });
            assert!(fake.spawned_rx.try_recv().is_err());
            assert_eq!(fake.max_active.load(Ordering::Acquire), 1);
            fake.stop();
        }
    }

    #[test]
    fn source_removal_of_a_does_not_retire_waiting_user_input_for_b() {
        let fake = FakeWorker::start();
        fake.send(2, 1);
        let x = fake.spawned();
        send_run(&fake.request_tx, user_input_request(0, 2, 1));
        wait_cancel_requested(&x.cancellation);
        let b = user_input_request(1, 3, 1);
        send_run(&fake.request_tx, b.clone());
        retire_user_inputs(&fake.request_tx, 0, 2);
        x.finish(Finish::Cancelled);
        let started = fake.spawned();
        let actual = started.request.clone();
        started.finish(Finish::Completed);
        assert_eq!(actual, b);
        let resumed = fake.spawned();
        let actual = resumed.request.clone();
        resumed.finish(Finish::Completed);
        assert_eq!(actual, request(2, 1));
        messages_until(&fake.message_rx, |message| {
            matches!(
                message,
                Message::RunCompleted { target, run_id: 1 } if target == &RunTarget::fixture(2)
            )
        });
        assert!(fake.spawned_rx.try_recv().is_err());
        fake.stop();
    }

    #[test]
    fn user_input_start_gate_preserves_normal_admission_and_latest_wins() {
        let fake = FakeWorker::start();
        let first = user_input_request(0, 1, 1);
        send_run(&fake.request_tx, first.clone());
        let active = fake.spawned();
        assert_eq!(active.request, first);
        send_run(&fake.request_tx, user_input_request(0, 2, 1));
        wait_cancel_requested(&active.cancellation);
        let latest = user_input_request(0, 3, 1);
        send_run(&fake.request_tx, latest.clone());
        active.finish(Finish::Cancelled);
        let started = fake.spawned();
        let actual = started.request.clone();
        started.finish(Finish::Completed);
        assert_eq!(actual, latest);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 3, .. })
        });
        assert!(fake.spawned_rx.try_recv().is_err());
        assert_eq!(fake.max_active.load(Ordering::Acquire), 1);
        fake.stop();
    }

    #[test]
    fn source_removal_gate_covers_commands_beyond_the_worker_batch_limit() {
        let (command_tx, command_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let (_retire_tx, retire_rx) = mpsc::channel();
        let control = Arc::new(WorkerControl::new(RunWorker::fixture_contexts(
            RunScope::fixture(),
            message_tx,
        )));
        let thread_control = Arc::clone(&control);
        let old = user_input_request(0, 1, 1);
        let RunKind::UserInput(snapshot) = &old.kind else {
            unreachable!()
        };
        snapshot.start_gate.retire_before(2);
        send_run(&command_tx, old);
        for _ in 1..MAX_RUN_COMMANDS_PER_TICK {
            command_tx
                .send(RunWorkerCommand::CancelStress {
                    target: RunTarget::fixture(9),
                    run_id: 0,
                })
                .unwrap();
        }
        retire_user_inputs(&command_tx, 0, 2);
        let fresh = user_input_request(0, 2, 3);
        send_run(&command_tx, fresh.clone());
        // Preload the real command queue so retirement is deterministically beyond the cap.
        let worker = thread::spawn(move || {
            scheduler_loop(
                command_rx,
                completion_rx,
                completion_tx,
                thread_control,
                retire_rx,
                move |_, request, completion| {
                    spawned_tx.send(request.clone()).unwrap();
                    spawn_with(request, completion, |cancellation| {
                        run_attempt(&cancellation, |_| Ok(()))
                    })
                },
            )
        });
        messages_until(&message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 2, .. })
        });
        control.begin_shutdown();
        worker.join().unwrap().unwrap();
        assert_eq!(spawned_rx.try_iter().collect::<Vec<_>>(), [fresh]);
    }

    fn send_run(sender: &Sender<RunWorkerCommand>, request: RunRequest) {
        sender.send(RunWorkerCommand::Run(request)).unwrap();
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
            .position(|message| {
                matches!(message, Message::RunRequeued { run_id: 5, target }
                    if target == &RunTarget::fixture(0))
            })
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
    fn old_session_commands_cannot_preempt_or_cancel_a_reused_run_id() {
        let active_request = stress_request(0, 1);
        let scope = active_request.target.scope.clone();
        let old_target = RunScope::new(scope.contest_id()).target(&active_request.target.task_id);
        let mut scheduler = RunScheduler::default();
        scheduler.request_arrived(active_request.clone());
        assert_eq!(scheduler.start_next(), Some(active_request.clone()));

        let (cancellation_tx, cancellation_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();
        let active = spawn_with(
            active_request.clone(),
            completion_tx.clone(),
            move |cancellation| {
                cancellation_tx.send(cancellation).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                AttemptOutcome::Completed
            },
        )
        .unwrap();
        let mut active = Some(active);
        let cancellation = cancellation_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let (command_tx, command_rx) = mpsc::channel();
        let (message_tx, message_rx) = mpsc::channel();
        let control = WorkerControl::new(RunWorker::fixture_contexts(scope, message_tx));
        command_tx
            .send(RunWorkerCommand::CancelStress {
                target: old_target.clone(),
                run_id: active_request.run_id,
            })
            .unwrap();
        command_tx
            .send(RunWorkerCommand::Run(RunRequest {
                target: old_target.clone(),
                ..request(0, 2)
            }))
            .unwrap();
        assert_eq!(
            drain_commands(&command_rx, &mut scheduler, active.as_ref(), &control).unwrap(),
            RequestDrain::Open
        );
        assert!(!cancellation.is_requested());
        assert_eq!(scheduler.active_request(), Some(active_request.clone()));
        let old_completion = AttemptCompletion {
            target: old_target,
            run_id: active_request.run_id,
        };
        assert!(validate_completion(&old_completion, &active_request).is_err());
        completion_tx.send(old_completion).unwrap();
        assert!(
            process_ready_completion(&completion_rx, &mut scheduler, &mut active, &control,)
                .unwrap()
        );
        assert!(message_rx.try_recv().is_err());
        assert_eq!(active.as_ref().unwrap().request(), active_request);
        assert!(!cancellation.is_requested());

        command_tx
            .send(RunWorkerCommand::CancelStress {
                target: active_request.target.clone(),
                run_id: active_request.run_id,
            })
            .unwrap();
        drain_commands(&command_rx, &mut scheduler, active.as_ref(), &control).unwrap();
        assert!(cancellation.is_requested());
        release_tx.send(()).unwrap();
        let completion = completion_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        validate_completion(&completion, &active_request).unwrap();
        active.unwrap().join().unwrap();
        assert!(!scheduler.retire_active().unwrap().is_latest());
        assert!(scheduler.start_next().is_none());
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
    fn latest_stress_failure_is_published_without_requeue() {
        let fake = FakeWorker::start();
        let stress = stress_request(0, 1);
        send_run(&fake.request_tx, stress.clone());
        let active = fake.spawned();
        assert_eq!(active.request, stress);

        active.finish(Finish::Failed);

        let messages = messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunFailed { run_id: 1, .. })
        });
        assert!(
            messages
                .iter()
                .all(|message| { !matches!(message, Message::RunRequeued { run_id: 1, .. }) })
        );
        assert!(fake.spawned_rx.try_recv().is_err());
        fake.stop();
    }

    #[test]
    fn explicit_active_stress_cancellation_uses_attempt_token_and_allows_restart() {
        let fake = FakeWorker::start();
        let first_request = stress_request(0, 10);
        send_run(&fake.request_tx, first_request.clone());
        let first = fake.spawned();
        assert_eq!(first.request, first_request);

        fake.request_tx
            .send(RunWorkerCommand::CancelStress {
                target: RunTarget::fixture(0),
                run_id: 10,
            })
            .unwrap();
        wait_cancel_requested(&first.cancellation);

        let second_request = stress_request(0, 11);
        send_run(&fake.request_tx, second_request.clone());
        first.finish(Finish::Cancelled);
        let second = fake.spawned();
        assert_eq!(second.request, second_request);
        assert!(!second.cancellation.is_requested());
        second.finish(Finish::Completed);

        let messages = messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 11, .. })
        });
        assert!(messages.iter().all(|message| {
            !matches!(
                message,
                Message::RunCompleted { run_id: 10, .. }
                    | Message::RunFailed { run_id: 10, .. }
                    | Message::RunRequeued { run_id: 10, .. }
            )
        }));
        fake.stop();
    }

    #[test]
    fn newer_stress_then_old_stop_preserves_new_physical_attempt() {
        let fake = FakeWorker::start();
        let old_request = stress_request(0, 10);
        send_run(&fake.request_tx, old_request.clone());
        let old = fake.spawned();

        let new_request = stress_request(0, 11);
        send_run(&fake.request_tx, new_request.clone());
        wait_cancel_requested(&old.cancellation);
        fake.request_tx
            .send(RunWorkerCommand::CancelStress {
                target: old_request.target.clone(),
                run_id: old_request.run_id,
            })
            .unwrap();
        old.finish(Finish::Cancelled);

        let new = fake.spawned();
        assert_eq!(new.request, new_request);
        assert!(!new.cancellation.is_requested());
        new.finish(Finish::Completed);
        messages_until(&fake.message_rx, |message| {
            matches!(message, Message::RunCompleted { run_id: 11, .. })
        });
        fake.stop();
    }

    #[test]
    fn stale_stress_cancellation_does_not_touch_newer_physical_attempt() {
        let mut scheduler = RunScheduler::default();
        let current = stress_request(0, 11);
        scheduler.request_arrived(current.clone());
        assert_eq!(scheduler.start_next(), Some(current.clone()));

        let (completion_tx, completion_rx) = mpsc::channel();
        let (cancellation_tx, cancellation_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let active = spawn_with(current, completion_tx, move |cancellation| {
            cancellation_tx.send(Arc::clone(&cancellation)).unwrap();
            release_rx.recv().unwrap();
            run_attempt(&cancellation, |_| Ok(()))
        })
        .unwrap();
        let cancellation = cancellation_rx.recv().unwrap();

        process_stress_cancellation(&mut scheduler, Some(&active), &RunTarget::fixture(0), 10)
            .unwrap();
        assert!(!cancellation.is_requested());

        release_tx.send(()).unwrap();
        completion_rx.recv().unwrap();
        assert!(matches!(active.join().unwrap(), AttemptOutcome::Completed));
        let retired = scheduler.retire_active().unwrap();
        assert!(retired.is_latest());
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
        let first_request = first_pending.request.clone();
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
        for run_id in 1..=(MAX_RUN_COMMANDS_PER_TICK as u64 + 5) {
            send_run(&tx, request(run_id as usize, run_id));
        }
        let mut scheduler = RunScheduler::default();
        let control = WorkerControl::new(RunWorker::fixture_contexts(
            RunScope::fixture(),
            mpsc::channel().0,
        ));

        assert_eq!(
            drain_commands(&rx, &mut scheduler, None, &control).unwrap(),
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
        let worker = start_fixture_worker(message_tx, move |request, completion_tx| {
            let spawned_tx = spawned_tx.clone();
            let release_rx = release_rx.take().unwrap();
            spawn_with(request.clone(), completion_tx, move |cancellation| {
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
        send_run(&request_tx, request(0, 1));
        let (spawned, cancellation) = spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(spawned, request(0, 1));
        for problem in 1..=100 {
            send_run(&request_tx, request(problem, problem as u64 + 1));
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

        let worker = start_fixture_worker(message_tx, move |request, completion_tx| {
            let is_first = std::mem::replace(&mut first, false);
            let release_rx = is_first.then(|| first_release.take().unwrap());
            let spawned_tx = spawned_tx.clone();
            let active_count = Arc::clone(&closure_active);
            let max_active = Arc::clone(&closure_max);
            let attempt_messages = attempt_messages.clone();

            spawn_with(request.clone(), completion_tx, move |cancellation| {
                let current = active_count.fetch_add(1, Ordering::AcqRel) + 1;
                max_active.fetch_max(current, Ordering::AcqRel);
                let _guard = ActiveCountGuard(active_count);
                attempt_messages
                    .send(Message::RunStarted {
                        run_id: request.run_id,
                        target: request.target.clone(),
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
        send_run(&request_tx, request(0, 1));
        let (_, first_cancellation) = spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        send_run(&request_tx, request(1, 2));
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
        let control = WorkerControl::new(RunWorker::fixture_contexts(
            RunScope::fixture(),
            mpsc::channel().0,
        ));
        let called = AtomicBool::new(false);

        control.begin_shutdown();

        assert!(
            control
                .while_running(|_| called.store(true, Ordering::Release))
                .is_none()
        );
        assert!(!called.load(Ordering::Acquire));
    }

    #[test]
    fn attempt_panic_is_worker_failure_and_does_not_start_next() {
        let (message_tx, message_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let mut first = true;
        let worker = start_fixture_worker(message_tx, move |request, completion_tx| {
            spawned_tx.send(request.clone()).unwrap();
            if first {
                first = false;
                spawn_with(request.clone(), completion_tx, |_| panic!("fake panic"))
            } else {
                spawn_with(request.clone(), completion_tx, |cancellation| {
                    run_attempt(&cancellation, |_| Ok(()))
                })
            }
        })
        .unwrap();
        let request_tx = worker.sender();
        send_run(&request_tx, request(0, 1));
        send_run(&request_tx, request(1, 2));
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
        let worker = start_fixture_worker(message_tx, move |request, completion_tx| {
            if fail_first {
                fail_first = false;
                return Err(io::Error::other("fake spawn failure"));
            }

            let spawned_tx = spawned_tx.clone();
            spawn_with(request.clone(), completion_tx, move |cancellation| {
                spawned_tx.send(request.clone()).unwrap();
                run_attempt(&cancellation, |_| Ok(()))
            })
        })
        .unwrap();
        let request_tx = worker.sender();
        send_run(&request_tx, request(0, 1));
        assert!(matches!(
            message_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::WorkerFailed(error) if error.to_string().contains("fake spawn failure")
        ));

        let _ = request_tx.send(RunWorkerCommand::Run(request(1, 2)));
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
        let (_retire_tx, retire_rx) = mpsc::channel();
        let control = Arc::new(WorkerControl::new(RunWorker::fixture_contexts(
            RunScope::fixture(),
            message_tx,
        )));

        let handle = thread::spawn({
            let control = Arc::clone(&control);
            move || {
                scheduler_loop(
                    request_rx,
                    completion_rx,
                    completion_tx,
                    control,
                    retire_rx,
                    move |_, request, completion_tx| {
                        let spawned_tx = spawned_tx.clone();
                        let finished_tx = finished_tx.clone();
                        let release_rx = release_rx.take().unwrap();
                        spawn_with(request.clone(), completion_tx, move |cancellation| {
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

        send_run(&request_tx, request(0, 1));
        let (spawned, cancellation) = spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(spawned, request(0, 1));
        send_run(&request_tx, request(1, 2));
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
        let mut worker = start_fixture_worker(message_tx, move |request, completion_tx| {
            let attempt_messages = attempt_messages.clone();
            let finished_tx = finished_tx.clone();
            spawn_with(request.clone(), completion_tx, move |cancellation| {
                let outcome = run_attempt(&cancellation, |_| {
                    attempt_messages
                        .send(Message::RunStarted {
                            run_id: request.run_id,
                            target: request.target.clone(),
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
        send_run(&worker.sender(), request(0, 1));

        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let error = worker.join().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn scheduler_helper_panic_is_reported_and_stops_without_next_spawn() {
        let (message_tx, message_rx) = mpsc::channel();
        let worker =
            start_fixture_worker(message_tx, |_, _| panic!("fake scheduler helper panic")).unwrap();
        let request_tx = worker.sender();
        send_run(&request_tx, request(0, 1));
        let _ = request_tx.send(RunWorkerCommand::Run(request(1, 2)));

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
        let worker = start_fixture_worker(message_tx, move |_, completion_tx| {
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
        send_run(&worker.sender(), request(0, 1));

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
        let control = WorkerControl::new(RunContexts::new());

        let error = complete_attempt(
            AttemptCompletion {
                target: RunTarget::fixture(7),
                run_id: 8,
            },
            &mut scheduler,
            &mut active,
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
                target: RunTarget::fixture(0),
                run_id: 1,
            },
            &mut scheduler,
            &mut no_active,
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
        let control = WorkerControl::new(RunContexts::new());

        let error =
            process_ready_completion(&disconnected_rx, &mut scheduler, &mut active, &control)
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        cleanup_active(&mut scheduler, active.take()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn thousand_rapid_requests_keep_one_physical_attempt_and_only_complete_latest_runs() {
        verify_request_flood(&[RunScope::fixture()]);
    }

    #[test]
    fn workspace_request_flood_keeps_one_physical_attempt() {
        verify_request_flood(&[
            RunScope::fixture(),
            RunScope::new("abc480"),
            RunScope::new("abc478"),
        ]);
    }

    fn verify_request_flood(scopes: &[RunScope]) {
        let (message_tx, message_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let allow_completion = Arc::new(AtomicBool::new(false));
        let active_count = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let closure_allow = Arc::clone(&allow_completion);
        let closure_active = Arc::clone(&active_count);
        let closure_max = Arc::clone(&max_active);
        let attempt_messages = message_tx.clone();

        let contexts = scopes
            .iter()
            .flat_map(|scope| RunWorker::fixture_contexts(scope.clone(), message_tx.clone()))
            .collect();
        let worker = RunWorker::launch(contexts, move |_, request, completion_tx| {
            let spawned_tx = spawned_tx.clone();
            let allow_completion = Arc::clone(&closure_allow);
            let active_count = Arc::clone(&closure_active);
            let max_active = Arc::clone(&closure_max);
            let attempt_messages = attempt_messages.clone();
            spawn_with(request.clone(), completion_tx, move |cancellation| {
                let current = active_count.fetch_add(1, Ordering::AcqRel) + 1;
                max_active.fetch_max(current, Ordering::AcqRel);
                let _guard = ActiveCountGuard(active_count);
                attempt_messages
                    .send(Message::RunStarted {
                        run_id: request.run_id,
                        target: request.target.clone(),
                    })
                    .unwrap();
                spawned_tx.send(request.clone()).unwrap();

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
        send_run(&request_tx, scoped_sample(&scopes[0], "task_0", 1));
        spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let mut latest = std::collections::HashMap::new();
        for run_id in 2..=1_001 {
            let problem = (run_id as usize) % 3;
            let request = RunRequest {
                run_id,
                target: scopes[((run_id as usize) / 3) % scopes.len()]
                    .target(&format!("task_{problem}")),
                language: if run_id % 5 == 0 {
                    Language::Python
                } else {
                    Language::Cpp
                },
                debug: run_id % 2 == 0 && run_id % 5 != 0,
                kind: crate::tui::message::RunKind::Samples,
            };
            latest.insert(request.target.clone(), request.clone());
            send_run(&request_tx, request);
        }
        let sentinel = RunRequest {
            run_id: 1_002,
            target: RunTarget::fixture(3),
            language: Language::Cpp,
            debug: true,
            kind: crate::tui::message::RunKind::Samples,
        };
        latest.insert(sentinel.target.clone(), sentinel.clone());
        send_run(&request_tx, sentinel.clone());

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
            let mut worker = start_fixture_worker(message_tx, move |request, completion_tx| {
                let spawned_tx = spawned_tx.clone();
                let active_count = Arc::clone(&closure_active);
                let release_cancelled_attempt = Arc::clone(&closure_release);
                spawn_with(request.clone(), completion_tx, move |cancellation| {
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
            send_run(&request_tx, request(0, iteration * 10 + 1));
            let (_, cancellation) = spawned_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            send_run(&request_tx, request(1, iteration * 10 + 2));
            send_run(&request_tx, request(2, iteration * 10 + 3));
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
