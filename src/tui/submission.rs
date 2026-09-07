use std::collections::HashMap;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use crate::atcoder::AtCoderClient;
use crate::atcoder::submission_tracking::SubmissionStatus;
use crate::commands::submit::{
    PreparedSubmit, SubmissionCompletion, SubmissionEvent, SubmitPlan,
    execute_prepared_with_client, prepare_submit,
};
use crate::error::AppError;

const MAX_EVENTS_PER_TICK: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SubmissionKey {
    pub(crate) contest_id: String,
    pub(crate) task_id: String,
}

impl SubmissionKey {
    pub(crate) fn new(contest_id: impl Into<String>, task_id: impl Into<String>) -> Self {
        Self {
            contest_id: contest_id.into(),
            task_id: task_id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiSubmissionState {
    Accepted,
    Status(SubmissionStatus),
    TrackingUnavailable,
}

impl TuiSubmissionState {
    fn user_visible(self) -> Self {
        match self {
            Self::Accepted => Self::Status(SubmissionStatus::WaitingForJudge),
            state => state,
        }
    }

    pub(super) fn label(self) -> String {
        match self {
            Self::Accepted | Self::Status(SubmissionStatus::WaitingForJudge) => "WJ".to_string(),
            Self::TrackingUnavailable => "Untracked".to_string(),
            Self::Status(SubmissionStatus::WaitingForRejudge) => "WR".to_string(),
            Self::Status(SubmissionStatus::Judging) => "Judging".to_string(),
            Self::Status(SubmissionStatus::JudgingProgress {
                judged,
                total,
                provisional,
            }) => provisional.map_or_else(
                || format!("{judged}/{total}"),
                |verdict| format!("{judged}/{total} {verdict}"),
            ),
            Self::Status(SubmissionStatus::Finished(verdict)) => verdict.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UserVisibleSubmissionState {
    Current(TuiSubmissionState),
    Attempt(TuiSubmissionAttemptState),
}

impl UserVisibleSubmissionState {
    fn label(self) -> String {
        match self {
            Self::Current(current) => current.label(),
            Self::Attempt(attempt) => attempt.label().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiSubmissionAttemptState {
    Submitting,
    Unknown,
}

impl TuiSubmissionAttemptState {
    fn label(self) -> &'static str {
        match self {
            Self::Submitting => "Submitting",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubmissionDisplayState {
    pub(crate) current: Option<TuiSubmissionState>,
    pub(crate) attempt: Option<TuiSubmissionAttemptState>,
}

impl SubmissionDisplayState {
    pub(super) fn effective(self) -> Option<UserVisibleSubmissionState> {
        self.attempt
            .map(UserVisibleSubmissionState::Attempt)
            .or_else(|| {
                self.current
                    .map(|current| UserVisibleSubmissionState::Current(current.user_visible()))
            })
    }

    pub(super) fn summary_label(self) -> String {
        self.compact_label()
    }

    pub(super) fn compact_label(self) -> String {
        self.effective()
            .map_or_else(String::new, UserVisibleSubmissionState::label)
    }

    pub(super) fn header_label(self, problem_label: &str) -> String {
        self.effective().map_or_else(String::new, |state| {
            format!("SUB {problem_label} {}", state.label())
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LatestSubmissionDisplay {
    pub(crate) key: SubmissionKey,
    pub(crate) problem_index: String,
    pub(crate) state: SubmissionDisplayState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubmissionHeaderState {
    pub(super) problem_label: String,
    pub(super) state: SubmissionDisplayState,
}

impl SubmissionHeaderState {
    pub(super) fn label(&self) -> String {
        self.state.header_label(&self.problem_label)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SubmissionViewState {
    pub(super) latest: Option<SubmissionHeaderState>,
    pub(super) problems: Vec<Option<SubmissionDisplayState>>,
}

#[derive(Debug, Default)]
struct SubmissionRecord {
    problem_index: Option<String>,
    generation: Option<u64>,
    state: Option<TuiSubmissionState>,
    current_activity_seq: Option<u64>,
    pending_generation: Option<u64>,
    discovering: bool,
    unknown_generation: Option<u64>,
    attempt_activity_seq: Option<u64>,
    last_failure: Option<(u64, String)>,
}

impl SubmissionRecord {
    fn display_state(&self) -> Option<SubmissionDisplayState> {
        let attempt = if self.unknown_generation.is_some() {
            Some(TuiSubmissionAttemptState::Unknown)
        } else if self.pending_generation.is_some() {
            Some(TuiSubmissionAttemptState::Submitting)
        } else {
            None
        };
        if self.state.is_none() && attempt.is_none() {
            None
        } else {
            Some(SubmissionDisplayState {
                current: self.state,
                attempt,
            })
        }
    }

    fn user_visible_state(&self) -> Option<UserVisibleSubmissionState> {
        self.display_state()
            .and_then(SubmissionDisplayState::effective)
    }

    fn user_visible_activity_seq(&self) -> Option<u64> {
        match self.user_visible_state()? {
            UserVisibleSubmissionState::Current(_) => self.current_activity_seq,
            UserVisibleSubmissionState::Attempt(_) => self.attempt_activity_seq,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AttemptResolution {
    Pending,
    Accepted,
    Failed(String),
    Unknown,
}

#[derive(Debug)]
enum WorkerEventKind {
    Submission(SubmissionEvent),
    Failed(String),
    Unknown,
    CancelledBeforeSubmit,
    WorkerPanicked,
}

#[derive(Debug)]
struct WorkerEvent {
    key: SubmissionKey,
    generation: u64,
    kind: WorkerEventKind,
}

struct SubmissionWorker {
    key: SubmissionKey,
    generation: u64,
    cancellation: Arc<SubmissionCancellation>,
    progress: Arc<WorkerProgress>,
    handle: Option<JoinHandle<()>>,
}

impl SubmissionWorker {
    fn request_cancel(&self) {
        self.cancellation.request_cancel();
    }

    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn join(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| io::Error::other("submission worker thread panicked"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum WorkerProgressPhase {
    PreAccepted = 0,
    AcceptedKnown = 1,
    FinishedKnown = 2,
}

struct WorkerProgress {
    phase: AtomicU8,
}

impl Default for WorkerProgress {
    fn default() -> Self {
        Self {
            phase: AtomicU8::new(WorkerProgressPhase::PreAccepted as u8),
        }
    }
}

impl WorkerProgress {
    fn observe_before_send(&self, event: SubmissionEvent) {
        let phase = match event {
            SubmissionEvent::Accepted => WorkerProgressPhase::AcceptedKnown,
            SubmissionEvent::Status {
                status: SubmissionStatus::Finished(_),
                ..
            } => WorkerProgressPhase::FinishedKnown,
            SubmissionEvent::TrackingStarted { .. }
            | SubmissionEvent::Status { .. }
            | SubmissionEvent::TrackingUnavailable { .. } => return,
        };
        // fetch_max makes the proof monotonic even if a test executor emits unusual ordering.
        // Release pairs with the fallback's Acquire load; join additionally synchronizes thread
        // completion before the handle-side read.
        self.phase.fetch_max(phase as u8, Ordering::Release);
    }

    fn phase(&self) -> WorkerProgressPhase {
        match self.phase.load(Ordering::Acquire) {
            0 => WorkerProgressPhase::PreAccepted,
            1 => WorkerProgressPhase::AcceptedKnown,
            _ => WorkerProgressPhase::FinishedKnown,
        }
    }
}

const CANCEL_REQUESTED: u8 = 1;
const POST_STARTED: u8 = 2;

#[derive(Default)]
struct SubmissionCancellation {
    state: AtomicU8,
}

impl SubmissionCancellation {
    fn request_cancel(&self) {
        self.state.fetch_or(CANCEL_REQUESTED, Ordering::AcqRel);
    }

    fn should_continue(&self) -> bool {
        self.state.load(Ordering::Acquire) & CANCEL_REQUESTED == 0
    }

    fn try_begin_post(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & (CANCEL_REQUESTED | POST_STARTED) != 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state | POST_STARTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => state = observed,
            }
        }
    }

    #[cfg(test)]
    fn post_started(&self) -> bool {
        self.state.load(Ordering::Acquire) & POST_STARTED != 0
    }
}

trait SubmissionExecutor: Send + Sync {
    fn execute(
        &self,
        prepared: PreparedSubmit,
        emit: &mut dyn FnMut(SubmissionEvent) -> bool,
        cancellation: &SubmissionCancellation,
    ) -> Result<SubmissionCompletion, AppError>;
}

#[derive(Default)]
struct LazySharedAtCoderClient {
    client: Mutex<Option<Arc<AtCoderClient>>>,
}

impl LazySharedAtCoderClient {
    fn get(&self) -> Result<Arc<AtCoderClient>, AppError> {
        let mut client = self
            .client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = client.as_ref() {
            return Ok(Arc::clone(client));
        }
        let initialized = Arc::new(AtCoderClient::new()?);
        *client = Some(Arc::clone(&initialized));
        Ok(initialized)
    }
}

struct AtCoderSubmissionExecutor {
    client: Arc<LazySharedAtCoderClient>,
}

impl SubmissionExecutor for AtCoderSubmissionExecutor {
    fn execute(
        &self,
        prepared: PreparedSubmit,
        emit: &mut dyn FnMut(SubmissionEvent) -> bool,
        cancellation: &SubmissionCancellation,
    ) -> Result<SubmissionCompletion, AppError> {
        if !cancellation.should_continue() {
            return Ok(SubmissionCompletion::CancelledBeforeSubmit);
        }
        let atcoder = self.client.get()?;
        if !cancellation.should_continue() {
            return Ok(SubmissionCompletion::CancelledBeforeSubmit);
        }
        execute_prepared_with_client(
            prepared,
            &atcoder,
            emit,
            &|| cancellation.should_continue(),
            &|| cancellation.try_begin_post(),
        )
    }
}

pub(crate) struct SubmissionHub {
    records: HashMap<SubmissionKey, SubmissionRecord>,
    next_generation: u64,
    next_activity_seq: u64,
    #[cfg(test)]
    client: Arc<LazySharedAtCoderClient>,
    executor: Arc<dyn SubmissionExecutor>,
    event_tx: mpsc::Sender<WorkerEvent>,
    event_rx: mpsc::Receiver<WorkerEvent>,
    workers: Vec<SubmissionWorker>,
    stopping: bool,
    #[cfg(test)]
    join_panic_fallback_count: usize,
}

impl Default for SubmissionHub {
    fn default() -> Self {
        Self::new()
    }
}

impl SubmissionHub {
    pub(crate) fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let client = Arc::new(LazySharedAtCoderClient::default());
        Self {
            records: HashMap::new(),
            next_generation: 1,
            next_activity_seq: 1,
            #[cfg(test)]
            client: Arc::clone(&client),
            executor: Arc::new(AtCoderSubmissionExecutor { client }),
            event_tx,
            event_rx,
            workers: Vec::new(),
            stopping: false,
            #[cfg(test)]
            join_panic_fallback_count: 0,
        }
    }

    #[cfg(test)]
    fn with_executor(executor: Arc<dyn SubmissionExecutor>) -> Self {
        let mut hub = Self::new();
        hub.executor = executor;
        hub
    }

    pub(crate) fn state(&self, key: &SubmissionKey) -> Option<SubmissionDisplayState> {
        self.records.get(key)?.display_state()
    }

    pub(crate) fn latest_activity(&self) -> Option<LatestSubmissionDisplay> {
        self.records
            .iter()
            .filter_map(|(key, record)| {
                let problem_index = record.problem_index.as_ref()?;
                let state = record.display_state()?;
                let activity_seq = record.user_visible_activity_seq()?;
                Some((activity_seq, key, problem_index, state))
            })
            .max_by_key(|(activity_seq, _, _, _)| *activity_seq)
            .map(|(_, key, problem_index, state)| LatestSubmissionDisplay {
                key: key.clone(),
                problem_index: problem_index.clone(),
                state,
            })
    }

    fn take_activity_seq(&mut self) -> u64 {
        let activity_seq = self.next_activity_seq;
        self.next_activity_seq = self
            .next_activity_seq
            .checked_add(1)
            .expect("submission activity sequence space is exhausted");
        activity_seq
    }

    pub(crate) fn ensure_start_allowed(&self, key: &SubmissionKey) -> Result<(), &'static str> {
        let Some(record) = self.records.get(key) else {
            return Ok(());
        };
        if record.pending_generation.is_some() || record.discovering {
            return Err("Submission is already being started for this problem.");
        }
        if record.unknown_generation.is_some() {
            return Err("Submission outcome is unknown.\nCheck My Submissions before retrying.");
        }
        Ok(())
    }

    pub(crate) fn start(
        &mut self,
        key: SubmissionKey,
        problem_index: String,
        plan: SubmitPlan,
    ) -> Result<u64, String> {
        if self.stopping {
            return Err("Submission is unavailable while the TUI is stopping.".to_string());
        }
        self.ensure_start_allowed(&key).map_err(str::to_owned)?;

        // This synchronous read is the Enter-time ownership boundary. The worker receives only
        // the owned bytes and never rereads the source from disk.
        let prepared = prepare_submit(plan).map_err(submit_start_error_message)?;
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or_else(|| "submission generation space is exhausted".to_string())?;

        let cancellation = Arc::new(SubmissionCancellation::default());
        let progress = Arc::new(WorkerProgress::default());
        let event_tx = self.event_tx.clone();
        let executor = Arc::clone(&self.executor);
        let thread_key = key.clone();
        let thread_cancellation = Arc::clone(&cancellation);
        let thread_progress = Arc::clone(&progress);
        let spawn = thread::Builder::new()
            .name(format!("atc-submit-{generation}"))
            .spawn(move || {
                run_worker(
                    thread_key,
                    generation,
                    prepared,
                    executor,
                    thread_cancellation,
                    thread_progress,
                    event_tx,
                );
            });
        let handle = match spawn {
            Ok(handle) => handle,
            Err(error) => return Err(format!("failed to start submission worker: {error}")),
        };
        let activity_seq = self.take_activity_seq();
        let record = self.records.entry(key.clone()).or_default();
        record.problem_index = Some(problem_index);
        record.pending_generation = Some(generation);
        record.attempt_activity_seq = Some(activity_seq);
        self.workers.push(SubmissionWorker {
            key,
            generation,
            cancellation,
            progress,
            handle: Some(handle),
        });
        Ok(generation)
    }

    pub(crate) fn handle_events(&mut self) -> bool {
        let mut changed = self.drain_events();
        changed |= self.reap_finished();
        changed
    }

    fn drain_events(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..MAX_EVENTS_PER_TICK {
            match self.event_rx.try_recv() {
                Ok(event) => changed |= self.apply_event(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        changed
    }

    pub(super) fn attempt_resolution(
        &self,
        key: &SubmissionKey,
        generation: u64,
    ) -> Option<AttemptResolution> {
        let record = self.records.get(key)?;
        if record.pending_generation == Some(generation) {
            return Some(AttemptResolution::Pending);
        }
        if record.unknown_generation == Some(generation) {
            return Some(AttemptResolution::Unknown);
        }
        if record.generation == Some(generation) {
            return Some(AttemptResolution::Accepted);
        }
        record
            .last_failure
            .as_ref()
            .filter(|(failed_generation, _)| *failed_generation == generation)
            .map(|(_, message)| AttemptResolution::Failed(message.clone()))
    }

    fn apply_event(&mut self, event: WorkerEvent) -> bool {
        match event.kind {
            WorkerEventKind::Submission(SubmissionEvent::Accepted) => {
                if !self.records.get(&event.key).is_some_and(|record| {
                    record.pending_generation == Some(event.generation)
                        && record.unknown_generation.is_none()
                }) {
                    return false;
                }
                let (old_generation, user_visible_changed) = {
                    let record = self
                        .records
                        .get_mut(&event.key)
                        .expect("a validated pending submission record must exist");
                    let visible_before = record.user_visible_state();
                    let old_generation = record.generation;
                    record.pending_generation = None;
                    record.attempt_activity_seq = None;
                    record.generation = Some(event.generation);
                    record.state = Some(TuiSubmissionState::Accepted);
                    record.discovering = true;
                    record.last_failure = None;
                    (
                        old_generation,
                        visible_before != record.user_visible_state(),
                    )
                };
                if user_visible_changed {
                    let activity_seq = self.take_activity_seq();
                    self.records
                        .get_mut(&event.key)
                        .expect("an accepted submission record must still exist")
                        .current_activity_seq = Some(activity_seq);
                }
                if let Some(old_generation) = old_generation
                    && old_generation != event.generation
                {
                    self.cancel_worker(&event.key, old_generation);
                }
                true
            }
            WorkerEventKind::Submission(SubmissionEvent::TrackingStarted { .. }) => self
                .update_current(
                    event.key,
                    event.generation,
                    TuiSubmissionState::Accepted,
                    true,
                ),
            WorkerEventKind::Submission(SubmissionEvent::Status { status, .. }) => self
                .update_current(
                    event.key,
                    event.generation,
                    TuiSubmissionState::Status(status),
                    true,
                ),
            WorkerEventKind::Submission(SubmissionEvent::TrackingUnavailable { .. }) => self
                .update_current(
                    event.key,
                    event.generation,
                    TuiSubmissionState::TrackingUnavailable,
                    true,
                ),
            WorkerEventKind::Failed(message) => {
                let Some(record) = self.records.get_mut(&event.key) else {
                    return false;
                };
                if record.pending_generation != Some(event.generation) {
                    return false;
                }
                record.pending_generation = None;
                record.attempt_activity_seq = None;
                record.last_failure = Some((event.generation, message));
                true
            }
            WorkerEventKind::Unknown => {
                if !self
                    .records
                    .get(&event.key)
                    .is_some_and(|record| record.pending_generation == Some(event.generation))
                {
                    return false;
                }
                let user_visible_changed = {
                    let record = self
                        .records
                        .get_mut(&event.key)
                        .expect("a validated pending submission record must exist");
                    let visible_before = record.user_visible_state();
                    record.pending_generation = None;
                    record.unknown_generation = Some(event.generation);
                    visible_before != record.user_visible_state()
                };
                if user_visible_changed {
                    let activity_seq = self.take_activity_seq();
                    self.records
                        .get_mut(&event.key)
                        .expect("an unknown submission record must still exist")
                        .attempt_activity_seq = Some(activity_seq);
                }
                true
            }
            WorkerEventKind::CancelledBeforeSubmit => {
                let Some(record) = self.records.get_mut(&event.key) else {
                    return false;
                };
                if record.pending_generation != Some(event.generation) {
                    return false;
                }
                record.pending_generation = None;
                record.attempt_activity_seq = None;
                true
            }
            WorkerEventKind::WorkerPanicked => {
                self.handle_worker_panic(&event.key, event.generation)
            }
        }
    }

    fn update_current(
        &mut self,
        key: SubmissionKey,
        generation: u64,
        state: TuiSubmissionState,
        discovery_finished: bool,
    ) -> bool {
        let (changed, user_visible_changed) = {
            let Some(record) = self.records.get_mut(&key) else {
                return false;
            };
            if record.generation != Some(generation) {
                return false;
            }
            let visible_before = record.user_visible_state();
            let state_changed = record.state != Some(state);
            let changed = state_changed || (discovery_finished && record.discovering);
            record.state = Some(state);
            if discovery_finished {
                record.discovering = false;
            }
            (changed, visible_before != record.user_visible_state())
        };
        if user_visible_changed {
            let activity_seq = self.take_activity_seq();
            self.records
                .get_mut(&key)
                .expect("an updated submission record must still exist")
                .current_activity_seq = Some(activity_seq);
        }
        changed
    }

    fn cancel_worker(&self, key: &SubmissionKey, generation: u64) {
        if let Some(worker) = self
            .workers
            .iter()
            .find(|worker| worker.key == *key && worker.generation == generation)
        {
            worker.request_cancel();
        }
    }

    fn reap_finished(&mut self) -> bool {
        let mut changed = false;
        let mut index = 0;
        while index < self.workers.len() {
            if self.workers[index].is_finished() {
                let mut worker = self.workers.swap_remove(index);
                let key = worker.key.clone();
                let generation = worker.generation;
                if worker.join().is_err() {
                    #[cfg(test)]
                    {
                        self.join_panic_fallback_count += 1;
                    }
                    // Normal executor panics are contained by run_worker and reported as an
                    // ordered WorkerPanicked event. A join error therefore means containment
                    // itself failed; classify conservatively so the attempt never remains stuck.
                    changed |=
                        self.handle_worker_join_panic(&key, generation, worker.progress.phase());
                }
            } else {
                index += 1;
            }
        }
        changed
    }

    fn handle_worker_panic(&mut self, key: &SubmissionKey, generation: u64) -> bool {
        let Some(record) = self.records.get(key) else {
            return false;
        };
        if record.pending_generation == Some(generation) {
            let user_visible_changed = {
                let record = self
                    .records
                    .get_mut(key)
                    .expect("a validated pending submission record must exist");
                let visible_before = record.user_visible_state();
                record.pending_generation = None;
                record.unknown_generation = Some(generation);
                record.last_failure = Some((
                    generation,
                    "Submission worker stopped unexpectedly; the submission outcome is unknown."
                        .to_string(),
                ));
                visible_before != record.user_visible_state()
            };
            if user_visible_changed {
                let activity_seq = self.take_activity_seq();
                self.records
                    .get_mut(key)
                    .expect("an unknown submission record must still exist")
                    .attempt_activity_seq = Some(activity_seq);
            }
            return true;
        }
        if record.generation != Some(generation) {
            return false;
        }
        if matches!(
            record.state,
            Some(TuiSubmissionState::Status(SubmissionStatus::Finished(_)))
        ) {
            self.records
                .get_mut(key)
                .expect("an existing final submission record must remain present")
                .last_failure = Some((
                generation,
                "Submission worker stopped unexpectedly after reporting a final status."
                    .to_string(),
            ));
            return false;
        }
        let (changed, user_visible_changed) = {
            let record = self
                .records
                .get_mut(key)
                .expect("an existing submission record must remain present");
            let visible_before = record.user_visible_state();
            let changed =
                record.state != Some(TuiSubmissionState::TrackingUnavailable) || record.discovering;
            record.state = Some(TuiSubmissionState::TrackingUnavailable);
            record.discovering = false;
            record.last_failure = Some((
                generation,
                "Submission tracking worker stopped unexpectedly.".to_string(),
            ));
            (changed, visible_before != record.user_visible_state())
        };
        if user_visible_changed {
            let activity_seq = self.take_activity_seq();
            self.records
                .get_mut(key)
                .expect("an updated submission record must still exist")
                .current_activity_seq = Some(activity_seq);
        }
        changed
    }

    fn handle_worker_join_panic(
        &mut self,
        key: &SubmissionKey,
        generation: u64,
        progress: WorkerProgressPhase,
    ) -> bool {
        let Some(record) = self.records.get(key) else {
            return false;
        };
        if record.generation == Some(generation)
            && matches!(
                record.state,
                Some(TuiSubmissionState::Status(SubmissionStatus::Finished(_)))
            )
        {
            self.records
                .get_mut(key)
                .expect("an existing final submission record must remain present")
                .last_failure = Some((
                generation,
                "Submission worker stopped unexpectedly after reporting a final status."
                    .to_string(),
            ));
            return false;
        }
        if record.pending_generation != Some(generation) && record.generation != Some(generation) {
            // A newer accepted generation already replaced this worker. Its outer failure
            // must not resurrect a stale submission or create an unrelated Unknown lock.
            return false;
        }

        match progress {
            WorkerProgressPhase::PreAccepted => {
                let user_visible_changed = {
                    let record = self
                        .records
                        .get_mut(key)
                        .expect("a validated submission record must remain present");
                    let visible_before = record.user_visible_state();
                    if record.pending_generation == Some(generation) {
                        record.pending_generation = None;
                    }
                    record.unknown_generation = Some(generation);
                    record.last_failure = Some((
                        generation,
                        "Submission worker stopped unexpectedly; the submission outcome is unknown."
                            .to_string(),
                    ));
                    visible_before != record.user_visible_state()
                };
                if user_visible_changed {
                    let activity_seq = self.take_activity_seq();
                    self.records
                        .get_mut(key)
                        .expect("an unknown submission record must still exist")
                        .attempt_activity_seq = Some(activity_seq);
                }
                true
            }
            WorkerProgressPhase::AcceptedKnown | WorkerProgressPhase::FinishedKnown => {
                let target_attempt_removed = record.unknown_generation == Some(generation)
                    || (record.unknown_generation.is_none()
                        && record.pending_generation == Some(generation));
                let (old_generation, user_visible_changed) = {
                    let record = self
                        .records
                        .get_mut(key)
                        .expect("a validated submission record must remain present");
                    let visible_before = record.user_visible_state();
                    let old_generation = record.generation;
                    if record.pending_generation == Some(generation) {
                        record.pending_generation = None;
                    }
                    if target_attempt_removed {
                        record.attempt_activity_seq = None;
                    }
                    if record.unknown_generation == Some(generation) {
                        record.unknown_generation = None;
                    }
                    record.generation = Some(generation);
                    record.state = Some(TuiSubmissionState::TrackingUnavailable);
                    record.discovering = false;
                    record.last_failure = Some((
                        generation,
                        "Submission tracking worker stopped unexpectedly.".to_string(),
                    ));
                    (
                        old_generation,
                        visible_before != record.user_visible_state(),
                    )
                };
                if user_visible_changed {
                    let activity_seq = self.take_activity_seq();
                    self.records
                        .get_mut(key)
                        .expect("an untracked submission record must still exist")
                        .current_activity_seq = Some(activity_seq);
                }
                if let Some(old_generation) = old_generation
                    && old_generation != generation
                {
                    self.cancel_worker(key, old_generation);
                }
                true
            }
        }
    }

    pub(crate) fn request_stop(&mut self) {
        self.stopping = true;
        for worker in &self.workers {
            worker.request_cancel();
        }
    }

    pub(crate) fn shutdown(mut self) -> io::Result<()> {
        self.request_stop();
        let mut first_error = None;
        for worker in &mut self.workers {
            if let Err(error) = worker.join()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for SubmissionHub {
    fn drop(&mut self) {
        self.request_stop();
        for worker in &mut self.workers {
            let _ = worker.join();
        }
    }
}

fn run_worker(
    key: SubmissionKey,
    generation: u64,
    prepared: PreparedSubmit,
    executor: Arc<dyn SubmissionExecutor>,
    cancellation: Arc<SubmissionCancellation>,
    progress: Arc<WorkerProgress>,
    event_tx: mpsc::Sender<WorkerEvent>,
) {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        run_worker_inner(
            &key,
            generation,
            prepared,
            executor,
            &cancellation,
            &progress,
            &event_tx,
        );
    }));
    if result.is_err() {
        // std::sync::mpsc preserves the order of messages sent through this Sender. Reusing the
        // worker's exact Sender instance makes this terminal event an ordering barrier after all
        // Accepted/Status/Finished events emitted by this worker, regardless of other producers.
        let _ = send_worker_event(&event_tx, &key, generation, WorkerEventKind::WorkerPanicked);
    }
}

fn run_worker_inner(
    key: &SubmissionKey,
    generation: u64,
    prepared: PreparedSubmit,
    executor: Arc<dyn SubmissionExecutor>,
    cancellation: &SubmissionCancellation,
    progress: &WorkerProgress,
    event_tx: &mpsc::Sender<WorkerEvent>,
) {
    let send = |kind| send_worker_event(event_tx, key, generation, kind);
    if !cancellation.should_continue() {
        let _ = send(WorkerEventKind::CancelledBeforeSubmit);
        return;
    }
    let result = executor.execute(
        prepared,
        &mut |event| {
            // AcceptedKnown is published only when the shared orchestration reports remote
            // acceptance, and is visible before its channel event can be queued.
            progress.observe_before_send(event);
            send(WorkerEventKind::Submission(event)) && cancellation.should_continue()
        },
        cancellation,
    );
    match result {
        Ok(SubmissionCompletion::Accepted) => {}
        Ok(SubmissionCompletion::UnknownSubmissionOutcome) => {
            let _ = send(WorkerEventKind::Unknown);
        }
        Ok(SubmissionCompletion::CancelledBeforeSubmit) => {
            let _ = send(WorkerEventKind::CancelledBeforeSubmit);
        }
        Err(error) => {
            let _ = send(WorkerEventKind::Failed(error.to_string()));
        }
    }
}

fn send_worker_event(
    event_tx: &mpsc::Sender<WorkerEvent>,
    key: &SubmissionKey,
    generation: u64,
    kind: WorkerEventKind,
) -> bool {
    event_tx
        .send(WorkerEvent {
            key: key.clone(),
            generation,
            kind,
        })
        .is_ok()
}

fn submit_start_error_message(error: AppError) -> String {
    match &error {
        AppError::Io(source) if source.kind() == io::ErrorKind::NotFound => {
            "Source file no longer exists.".to_string()
        }
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atcoder::submission_tracking::{SubmissionId, Verdict};
    use crate::language::{Language, PythonRuntime};
    use crate::model::{Contest, Problem};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    type TestRun = Box<
        dyn FnOnce(
                PreparedSubmit,
                &mut dyn FnMut(SubmissionEvent) -> bool,
                &SubmissionCancellation,
            ) -> Result<SubmissionCompletion, AppError>
            + Send,
    >;

    struct TestExecutor {
        runs: Mutex<HashMap<SubmissionKey, VecDeque<TestRun>>>,
        calls: AtomicUsize,
    }

    impl TestExecutor {
        fn for_key(key: SubmissionKey, runs: Vec<TestRun>) -> Arc<Self> {
            Self::with_keyed_runs(vec![(key, runs)])
        }

        fn with_keyed_runs(runs: Vec<(SubmissionKey, Vec<TestRun>)>) -> Arc<Self> {
            let runs = runs
                .into_iter()
                .map(|(key, runs)| (key, runs.into()))
                .collect();
            Arc::new(Self {
                runs: Mutex::new(runs),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl SubmissionExecutor for TestExecutor {
        fn execute(
            &self,
            prepared: PreparedSubmit,
            emit: &mut dyn FnMut(SubmissionEvent) -> bool,
            cancellation: &SubmissionCancellation,
        ) -> Result<SubmissionCompletion, AppError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let (contest_id, task_id) = prepared.test_submission_identity();
            let key = SubmissionKey::new(contest_id, task_id);
            let run = self
                .runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&key)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| panic!("test executor received an unexpected worker: {key:?}"));
            run(prepared, emit, cancellation)
        }
    }

    fn key() -> SubmissionKey {
        SubmissionKey::new("adt_easy_20260826_1", "abc430_a")
    }

    fn event(key: &SubmissionKey, generation: u64, kind: WorkerEventKind) -> WorkerEvent {
        WorkerEvent {
            key: key.clone(),
            generation,
            kind,
        }
    }

    fn display_current(state: TuiSubmissionState) -> Option<SubmissionDisplayState> {
        Some(SubmissionDisplayState {
            current: Some(state),
            attempt: None,
        })
    }

    fn display_attempt(
        current: Option<TuiSubmissionState>,
        attempt: TuiSubmissionAttemptState,
    ) -> Option<SubmissionDisplayState> {
        Some(SubmissionDisplayState {
            current,
            attempt: Some(attempt),
        })
    }

    fn seed_current_activity(
        hub: &mut SubmissionHub,
        key: &SubmissionKey,
        problem_index: &str,
        generation: u64,
        state: TuiSubmissionState,
    ) {
        let activity_seq = hub.take_activity_seq();
        hub.records.insert(
            key.clone(),
            SubmissionRecord {
                problem_index: Some(problem_index.to_string()),
                generation: Some(generation),
                state: Some(state),
                current_activity_seq: Some(activity_seq),
                ..SubmissionRecord::default()
            },
        );
    }

    fn seed_attempt_activity(
        hub: &mut SubmissionHub,
        key: &SubmissionKey,
        problem_index: &str,
        generation: u64,
    ) {
        let activity_seq = hub.take_activity_seq();
        let record = hub.records.entry(key.clone()).or_default();
        record.problem_index = Some(problem_index.to_string());
        record.pending_generation = Some(generation);
        record.attempt_activity_seq = Some(activity_seq);
    }

    fn latest_label(hub: &SubmissionHub) -> Option<String> {
        hub.latest_activity()
            .map(|latest| latest.state.header_label(&latest.problem_index))
    }

    fn start_test_submission(
        hub: &mut SubmissionHub,
        key: &SubmissionKey,
        problem_index: &str,
    ) -> u64 {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(format!("{problem_index}.cpp"));
        std::fs::write(&path, "int main() {}\r\n").unwrap();
        let plan = SubmitPlan::for_selected_source(
            key.contest_id.clone(),
            key.task_id.clone(),
            problem_index.to_string(),
            path,
            Language::Cpp,
            PythonRuntime::CPython,
        );
        hub.start(key.clone(), problem_index.to_string(), plan)
            .unwrap()
    }

    fn wait_for_hub(
        hub: &mut SubmissionHub,
        description: &str,
        predicate: impl Fn(&SubmissionHub) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            hub.handle_events();
            if predicate(hub) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}; workers={}, records={:?}",
                hub.workers.len(),
                hub.records
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_for_worker_thread_exit(hub: &SubmissionHub, generation: u64, description: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if hub
                .workers
                .iter()
                .any(|worker| worker.generation == generation && worker.is_finished())
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn accepted_then_waiting_worker(ready: mpsc::Sender<()>, stopped: Arc<AtomicUsize>) -> TestRun {
        Box::new(move |_, emit, cancellation| {
            assert!(cancellation.try_begin_post());
            assert!(emit(SubmissionEvent::Accepted));
            assert!(emit(SubmissionEvent::Status {
                submission_id: SubmissionId::for_test(1),
                status: SubmissionStatus::WaitingForJudge,
            }));
            ready.send(()).unwrap();
            while cancellation.should_continue() {
                thread::sleep(Duration::from_millis(2));
            }
            stopped.fetch_add(1, Ordering::AcqRel);
            let _ = emit(SubmissionEvent::Status {
                submission_id: SubmissionId::for_test(1),
                status: SubmissionStatus::Finished(Verdict::TimeLimitExceeded),
            });
            Ok(SubmissionCompletion::Accepted)
        })
    }

    #[test]
    fn production_worker_quit_before_post_gate_never_attempts_post() {
        let (at_gate_tx, at_gate_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let post_count = Arc::new(AtomicUsize::new(0));
        let worker_posts = Arc::clone(&post_count);
        let executor = TestExecutor::for_key(
            key(),
            vec![Box::new(move |_, _, cancellation| {
                at_gate_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                if !cancellation.try_begin_post() {
                    return Ok(SubmissionCompletion::CancelledBeforeSubmit);
                }
                worker_posts.fetch_add(1, Ordering::AcqRel);
                Ok(SubmissionCompletion::Accepted)
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        let generation = start_test_submission(&mut hub, &key, "A");
        at_gate_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        hub.request_stop();
        release_tx.send(()).unwrap();
        wait_for_hub(&mut hub, "cancelled worker", |hub| hub.workers.is_empty());

        assert_eq!(post_count.load(Ordering::Acquire), 0);
        assert_eq!(
            hub.attempt_resolution(&key, generation),
            None,
            "pre-POST cancellation is neither Accepted nor Unknown"
        );
        assert_eq!(hub.state(&key), None);
    }

    #[test]
    fn production_worker_cancel_during_retry_style_wait_exits_promptly_without_post() {
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let post_count = Arc::new(AtomicUsize::new(0));
        let worker_posts = Arc::clone(&post_count);
        let executor = TestExecutor::for_key(
            key(),
            vec![Box::new(move |_, _, cancellation| {
                waiting_tx.send(()).unwrap();
                while cancellation.should_continue() {
                    thread::sleep(Duration::from_millis(2));
                }
                if cancellation.try_begin_post() {
                    worker_posts.fetch_add(1, Ordering::AcqRel);
                }
                Ok(SubmissionCompletion::CancelledBeforeSubmit)
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        start_test_submission(&mut hub, &key, "A");
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let started = Instant::now();
        hub.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(post_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn production_workers_replace_only_after_s2_accepted_and_ignore_late_s1_event() {
        let (s1_ready_tx, s1_ready_rx) = mpsc::channel();
        let s1_stopped = Arc::new(AtomicUsize::new(0));
        let (s2_gate_tx, s2_gate_rx) = mpsc::channel();
        let (s2_release_tx, s2_release_rx) = mpsc::channel();
        let executor = TestExecutor::for_key(
            key(),
            vec![
                accepted_then_waiting_worker(s1_ready_tx, Arc::clone(&s1_stopped)),
                Box::new(move |_, emit, cancellation| {
                    s2_gate_tx.send(()).unwrap();
                    s2_release_rx.recv().unwrap();
                    assert!(cancellation.try_begin_post());
                    assert!(emit(SubmissionEvent::Accepted));
                    let _ = emit(SubmissionEvent::Status {
                        submission_id: SubmissionId::for_test(2),
                        status: SubmissionStatus::WaitingForJudge,
                    });
                    Ok(SubmissionCompletion::Accepted)
                }),
            ],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        let s1 = start_test_submission(&mut hub, &key, "A");
        s1_ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_for_hub(&mut hub, "S1 WJ", |hub| {
            hub.records.get(&key).is_some_and(|record| {
                record.generation == Some(s1)
                    && record.state
                        == Some(TuiSubmissionState::Status(
                            SubmissionStatus::WaitingForJudge,
                        ))
            })
        });

        let s2 = start_test_submission(&mut hub, &key, "A");
        s2_gate_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        hub.handle_events();
        assert_eq!(
            hub.state(&key),
            display_attempt(
                Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                TuiSubmissionAttemptState::Submitting,
            )
        );
        assert!(
            hub.workers
                .iter()
                .find(|worker| worker.generation == s1)
                .unwrap()
                .cancellation
                .should_continue(),
            "S1 must remain live before S2 Accepted"
        );

        s2_release_tx.send(()).unwrap();
        wait_for_hub(&mut hub, "S2 WJ", |hub| {
            hub.records.get(&key).is_some_and(|record| {
                record.generation == Some(s2)
                    && record.state
                        == Some(TuiSubmissionState::Status(
                            SubmissionStatus::WaitingForJudge,
                        ))
            })
        });
        wait_for_hub(&mut hub, "S1 cancellation", |_| {
            s1_stopped.load(Ordering::Acquire) == 1
        });
        hub.handle_events();

        assert_eq!(
            hub.state(&key),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge
            ))
        );
        assert_eq!(hub.records[&key].generation, Some(s2));
        hub.request_stop();
    }

    #[test]
    fn production_worker_pre_submit_failure_preserves_s1() {
        let (s1_ready_tx, s1_ready_rx) = mpsc::channel();
        let s1_stopped = Arc::new(AtomicUsize::new(0));
        let executor = TestExecutor::for_key(
            key(),
            vec![
                accepted_then_waiting_worker(s1_ready_tx, Arc::clone(&s1_stopped)),
                Box::new(|_, _, cancellation| {
                    assert!(!cancellation.post_started());
                    Err(io::Error::other("submit page failed before POST").into())
                }),
            ],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        let s1 = start_test_submission(&mut hub, &key, "A");
        s1_ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_for_hub(&mut hub, "S1 WJ", |hub| {
            hub.records
                .get(&key)
                .is_some_and(|record| record.generation == Some(s1) && !record.discovering)
        });
        let s2 = start_test_submission(&mut hub, &key, "A");
        wait_for_hub(&mut hub, "S2 failure", |hub| {
            matches!(
                hub.attempt_resolution(&key, s2),
                Some(AttemptResolution::Failed(_))
            )
        });

        assert_eq!(hub.records[&key].generation, Some(s1));
        assert_eq!(
            hub.state(&key),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge
            ))
        );
        assert_eq!(s1_stopped.load(Ordering::Acquire), 0);
        hub.request_stop();
    }

    #[test]
    fn production_worker_rejection_posts_once_and_preserves_s1() {
        let (s1_ready_tx, s1_ready_rx) = mpsc::channel();
        let s1_stopped = Arc::new(AtomicUsize::new(0));
        let post_count = Arc::new(AtomicUsize::new(0));
        let worker_posts = Arc::clone(&post_count);
        let executor = TestExecutor::for_key(
            key(),
            vec![
                accepted_then_waiting_worker(s1_ready_tx, Arc::clone(&s1_stopped)),
                Box::new(move |_, _, cancellation| {
                    assert!(cancellation.try_begin_post());
                    worker_posts.fetch_add(1, Ordering::AcqRel);
                    Err(crate::atcoder::submit::SubmitError::SubmissionRejected.into())
                }),
            ],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        let s1 = start_test_submission(&mut hub, &key, "A");
        s1_ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_for_hub(&mut hub, "S1 WJ", |hub| {
            hub.records
                .get(&key)
                .is_some_and(|record| record.generation == Some(s1) && !record.discovering)
        });
        let s2 = start_test_submission(&mut hub, &key, "A");
        wait_for_hub(&mut hub, "S2 rejection", |hub| {
            matches!(
                hub.attempt_resolution(&key, s2),
                Some(AttemptResolution::Failed(_))
            )
        });

        assert_eq!(post_count.load(Ordering::Acquire), 1);
        assert_eq!(hub.records[&key].generation, Some(s1));
        assert_eq!(s1_stopped.load(Ordering::Acquire), 0);
        hub.request_stop();
    }

    #[test]
    fn production_worker_unknown_keeps_s1_visible_and_locks_resubmit() {
        let (s1_ready_tx, s1_ready_rx) = mpsc::channel();
        let s1_stopped = Arc::new(AtomicUsize::new(0));
        let post_count = Arc::new(AtomicUsize::new(0));
        let worker_posts = Arc::clone(&post_count);
        let executor = TestExecutor::for_key(
            key(),
            vec![
                accepted_then_waiting_worker(s1_ready_tx, Arc::clone(&s1_stopped)),
                Box::new(move |_, _, cancellation| {
                    assert!(cancellation.try_begin_post());
                    worker_posts.fetch_add(1, Ordering::AcqRel);
                    Ok(SubmissionCompletion::UnknownSubmissionOutcome)
                }),
            ],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        let s1 = start_test_submission(&mut hub, &key, "A");
        s1_ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_for_hub(&mut hub, "S1 WJ", |hub| {
            hub.records
                .get(&key)
                .is_some_and(|record| record.generation == Some(s1) && !record.discovering)
        });
        let s2 = start_test_submission(&mut hub, &key, "A");
        wait_for_hub(&mut hub, "S2 Unknown", |hub| {
            hub.attempt_resolution(&key, s2) == Some(AttemptResolution::Unknown)
        });

        assert_eq!(post_count.load(Ordering::Acquire), 1);
        assert_eq!(s1_stopped.load(Ordering::Acquire), 0);
        assert_eq!(
            hub.state(&key),
            display_attempt(
                Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                TuiSubmissionAttemptState::Unknown,
            )
        );
        assert!(
            hub.ensure_start_allowed(&key)
                .unwrap_err()
                .contains("unknown")
        );
        let temp = tempfile::tempdir().unwrap();
        let plan = SubmitPlan::for_selected_source(
            key.contest_id.clone(),
            key.task_id.clone(),
            "A".to_string(),
            temp.path().join("unused.cpp"),
            Language::Cpp,
            PythonRuntime::CPython,
        );
        assert!(hub.start(key.clone(), "A".to_string(), plan).is_err());
        assert_eq!(post_count.load(Ordering::Acquire), 1);
        hub.request_stop();
    }

    #[test]
    fn worker_panic_before_accepted_becomes_new_unknown_without_stuck_pending() {
        let executor = TestExecutor::for_key(
            key(),
            vec![Box::new(|_, _, _| {
                panic!("intentional test panic before Accepted")
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        hub.records.insert(
            key.clone(),
            SubmissionRecord {
                generation: Some(41),
                state: Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                    Verdict::Accepted,
                ))),
                ..SubmissionRecord::default()
            },
        );
        let generation = start_test_submission(&mut hub, &key, "A");

        wait_for_hub(&mut hub, "panic classification", |hub| {
            hub.workers.is_empty()
                && hub.attempt_resolution(&key, generation) == Some(AttemptResolution::Unknown)
        });

        assert_eq!(hub.records[&key].pending_generation, None);
        assert_eq!(
            hub.state(&key),
            display_attempt(
                Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                    Verdict::Accepted,
                ))),
                TuiSubmissionAttemptState::Unknown,
            )
        );
        assert!(hub.ensure_start_allowed(&key).is_err());
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Unknown"));
    }

    #[test]
    fn worker_panic_after_accepted_becomes_untracked() {
        let executor = TestExecutor::for_key(
            key(),
            vec![Box::new(|_, emit, cancellation| {
                assert!(cancellation.try_begin_post());
                assert!(emit(SubmissionEvent::Accepted));
                panic!("intentional test panic after Accepted")
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        let generation = start_test_submission(&mut hub, &key, "A");

        wait_for_hub(&mut hub, "accepted panic classification", |hub| {
            hub.workers.is_empty()
                && hub.records.get(&key).is_some_and(|record| {
                    record.generation == Some(generation)
                        && record.state == Some(TuiSubmissionState::TrackingUnavailable)
                })
        });

        assert_eq!(
            hub.state(&key),
            display_current(TuiSubmissionState::TrackingUnavailable)
        );
        assert!(hub.ensure_start_allowed(&key).is_ok());
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Untracked"));
    }

    #[test]
    fn worker_panic_after_finished_does_not_overwrite_final_status() {
        let executor = TestExecutor::for_key(
            key(),
            vec![Box::new(|_, emit, cancellation| {
                assert!(cancellation.try_begin_post());
                assert!(emit(SubmissionEvent::Accepted));
                assert!(emit(SubmissionEvent::Status {
                    submission_id: SubmissionId::for_test(7),
                    status: SubmissionStatus::Finished(Verdict::Accepted),
                }));
                panic!("intentional test panic after Finished")
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        start_test_submission(&mut hub, &key, "A");

        wait_for_hub(&mut hub, "finished panic preservation", |hub| {
            hub.workers.is_empty()
                && hub.records.get(&key).is_some_and(|record| {
                    record.state
                        == Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                            Verdict::Accepted,
                        )))
                })
        });

        assert_eq!(
            hub.state(&key),
            display_current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                Verdict::Accepted
            )))
        );
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A AC"));
    }

    fn enqueue_unrelated_backlog(hub: &SubmissionHub, count: usize) {
        let unrelated = SubmissionKey::new("abc999", "abc999_z");
        for generation in 0..count as u64 {
            hub.event_tx
                .send(event(
                    &unrelated,
                    generation,
                    WorkerEventKind::CancelledBeforeSubmit,
                ))
                .unwrap();
        }
    }

    struct PanicAgainOnDrop;

    impl Drop for PanicAgainOnDrop {
        fn drop(&mut self) {
            panic!("intentional outer worker-wrapper panic")
        }
    }

    fn panic_then_repanic_outside_catch() -> ! {
        panic::panic_any(PanicAgainOnDrop)
    }

    fn drain_large_test_backlog(hub: &mut SubmissionHub) {
        for _ in 0..8 {
            hub.handle_events();
        }
    }

    #[test]
    fn ordered_panic_event_preserves_accepted_fact_after_large_global_backlog() {
        let target = key();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let executor = TestExecutor::for_key(
            target.clone(),
            vec![Box::new(move |_, emit, cancellation| {
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                assert!(cancellation.try_begin_post());
                assert!(emit(SubmissionEvent::Accepted));
                panic!("intentional panic after Accepted behind backlog")
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let generation = start_test_submission(&mut hub, &target, "A");
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        enqueue_unrelated_backlog(&hub, 1024);
        release_tx.send(()).unwrap();

        wait_for_hub(&mut hub, "ordered panic after Accepted backlog", |hub| {
            hub.workers.is_empty()
                && hub.records.get(&target).is_some_and(|record| {
                    record.generation == Some(generation)
                        && record.state == Some(TuiSubmissionState::TrackingUnavailable)
                })
        });

        assert_eq!(hub.records[&target].unknown_generation, None);
        assert_eq!(
            hub.state(&target),
            display_current(TuiSubmissionState::TrackingUnavailable)
        );
    }

    #[test]
    fn ordered_panic_event_preserves_finished_fact_after_large_global_backlog() {
        let target = key();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let executor = TestExecutor::for_key(
            target.clone(),
            vec![Box::new(move |_, emit, cancellation| {
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                assert!(cancellation.try_begin_post());
                assert!(emit(SubmissionEvent::Accepted));
                assert!(emit(SubmissionEvent::Status {
                    submission_id: SubmissionId::for_test(70),
                    status: SubmissionStatus::Finished(Verdict::Accepted),
                }));
                panic!("intentional panic after Finished behind backlog")
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let generation = start_test_submission(&mut hub, &target, "A");
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        enqueue_unrelated_backlog(&hub, 1024);
        release_tx.send(()).unwrap();

        wait_for_hub(&mut hub, "ordered panic after Finished backlog", |hub| {
            hub.workers.is_empty()
                && hub.records.get(&target).is_some_and(|record| {
                    record.generation == Some(generation)
                        && record.state
                            == Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                                Verdict::Accepted,
                            )))
                })
        });

        assert_eq!(hub.records[&target].unknown_generation, None);
        assert_eq!(
            hub.state(&target),
            display_current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                Verdict::Accepted,
            )))
        );
    }

    #[test]
    fn join_fallback_uses_accepted_progress_before_draining_large_global_backlog() {
        let target = key();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let executor = TestExecutor::for_key(
            target.clone(),
            vec![Box::new(move |_, emit, cancellation| {
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                assert!(cancellation.try_begin_post());
                assert!(emit(SubmissionEvent::Accepted));
                panic_then_repanic_outside_catch()
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let generation = start_test_submission(&mut hub, &target, "A");
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        enqueue_unrelated_backlog(&hub, 1024);
        release_tx.send(()).unwrap();
        wait_for_worker_thread_exit(&hub, generation, "outer panic after Accepted");

        // This tick can drain only 256 unrelated events before reaping the failed JoinHandle.
        // Accepted and WorkerPanicked are still behind the backlog, so Untracked can only come
        // from the per-worker progress proof rather than channel state.
        hub.handle_events();
        assert!(hub.workers.is_empty());
        assert_eq!(hub.records[&target].unknown_generation, None);
        assert_eq!(
            hub.state(&target),
            display_current(TuiSubmissionState::TrackingUnavailable)
        );
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Untracked"));

        drain_large_test_backlog(&mut hub);
        assert_eq!(hub.records[&target].unknown_generation, None);
        assert_eq!(
            hub.state(&target),
            display_current(TuiSubmissionState::TrackingUnavailable)
        );
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Untracked"));
    }

    #[test]
    fn join_fallback_allows_queued_finished_status_after_large_global_backlog() {
        let target = key();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let executor = TestExecutor::for_key(
            target.clone(),
            vec![Box::new(move |_, emit, cancellation| {
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                assert!(cancellation.try_begin_post());
                assert!(emit(SubmissionEvent::Accepted));
                assert!(emit(SubmissionEvent::Status {
                    submission_id: SubmissionId::for_test(71),
                    status: SubmissionStatus::Finished(Verdict::Accepted),
                }));
                panic_then_repanic_outside_catch()
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let generation = start_test_submission(&mut hub, &target, "A");
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        enqueue_unrelated_backlog(&hub, 1024);
        release_tx.send(()).unwrap();
        wait_for_worker_thread_exit(&hub, generation, "outer panic after Finished");

        hub.handle_events();
        assert!(hub.workers.is_empty());
        assert_eq!(hub.records[&target].unknown_generation, None);
        assert_eq!(
            hub.state(&target),
            display_current(TuiSubmissionState::TrackingUnavailable)
        );
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Untracked"));

        drain_large_test_backlog(&mut hub);
        assert_eq!(hub.records[&target].unknown_generation, None);
        assert_eq!(
            hub.state(&target),
            display_current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                Verdict::Accepted,
            )))
        );
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A AC"));
    }

    #[test]
    fn join_fallback_keeps_preaccepted_outer_panic_unknown_and_locked() {
        let target = key();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let executor = TestExecutor::for_key(
            target.clone(),
            vec![Box::new(move |_, _, cancellation| {
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                assert!(!cancellation.post_started());
                panic_then_repanic_outside_catch()
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        hub.records.insert(
            target.clone(),
            SubmissionRecord {
                generation: Some(1),
                state: Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                ..SubmissionRecord::default()
            },
        );
        hub.next_generation = 2;
        let generation = start_test_submission(&mut hub, &target, "A");
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        enqueue_unrelated_backlog(&hub, 1024);
        release_tx.send(()).unwrap();
        wait_for_worker_thread_exit(&hub, generation, "outer panic before Accepted");

        hub.handle_events();
        assert!(hub.workers.is_empty());
        assert_eq!(
            hub.attempt_resolution(&target, generation),
            Some(AttemptResolution::Unknown)
        );
        assert_eq!(
            hub.state(&target),
            display_attempt(
                Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                TuiSubmissionAttemptState::Unknown,
            )
        );
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Unknown"));
        assert!(hub.ensure_start_allowed(&target).is_err());

        drain_large_test_backlog(&mut hub);
        assert_eq!(
            hub.state(&target),
            display_attempt(
                Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                TuiSubmissionAttemptState::Unknown,
            )
        );
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Unknown"));
        assert!(hub.ensure_start_allowed(&target).is_err());
    }

    #[test]
    fn outer_lifetime_hub_receives_worker_event_while_another_contest_is_selected() {
        let (release_tx, release_rx) = mpsc::channel();
        let abc473 = SubmissionKey::new("abc473", "abc473_c");
        let abc474 = SubmissionKey::new("abc474", "abc474_a");
        let executor = TestExecutor::for_key(
            abc473.clone(),
            vec![Box::new(move |_, emit, cancellation| {
                assert!(cancellation.try_begin_post());
                assert!(emit(SubmissionEvent::Accepted));
                release_rx.recv().unwrap();
                let _ = emit(SubmissionEvent::Status {
                    submission_id: SubmissionId::for_test(9),
                    status: SubmissionStatus::Finished(Verdict::Accepted),
                });
                Ok(SubmissionCompletion::Accepted)
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        start_test_submission(&mut hub, &abc473, "C");
        wait_for_hub(&mut hub, "abc473 Accepted", |hub| {
            hub.records
                .get(&abc473)
                .is_some_and(|record| record.generation.is_some())
        });

        assert_eq!(hub.state(&abc474), None);
        release_tx.send(()).unwrap();
        wait_for_hub(&mut hub, "abc473 AC while switched", |hub| {
            hub.state(&abc473)
                == display_current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                    Verdict::Accepted,
                )))
        });

        assert_eq!(hub.state(&abc474), None);
        assert_eq!(
            hub.state(&abc473),
            display_current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                Verdict::Accepted
            )))
        );
    }

    #[test]
    fn multiple_problem_workers_share_executor_and_keep_events_key_isolated() {
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let keys = [
            SubmissionKey::new("abc500", "abc500_a"),
            SubmissionKey::new("abc500", "abc500_b"),
            SubmissionKey::new("abc500", "abc500_c"),
        ];
        let mut runs = Vec::new();
        for (key, (id, status)) in keys.iter().cloned().zip([
            (11, SubmissionStatus::WaitingForJudge),
            (12, SubmissionStatus::WaitingForRejudge),
            (13, SubmissionStatus::Finished(Verdict::WrongAnswer)),
        ]) {
            let calls = Arc::clone(&calls);
            let barrier = Arc::clone(&barrier);
            runs.push((
                key,
                vec![Box::new(
                    move |_,
                          emit: &mut dyn FnMut(SubmissionEvent) -> bool,
                          cancellation: &SubmissionCancellation| {
                        calls.fetch_add(1, Ordering::AcqRel);
                        barrier.wait();
                        assert!(cancellation.try_begin_post());
                        assert!(emit(SubmissionEvent::Accepted));
                        let _ = emit(SubmissionEvent::Status {
                            submission_id: SubmissionId::for_test(id),
                            status,
                        });
                        Ok(SubmissionCompletion::Accepted)
                    },
                ) as TestRun],
            ));
        }
        let executor = TestExecutor::with_keyed_runs(runs);
        let mut hub =
            SubmissionHub::with_executor(Arc::clone(&executor) as Arc<dyn SubmissionExecutor>);
        for (index, key) in keys.iter().enumerate() {
            start_test_submission(&mut hub, key, ["A", "B", "C"][index]);
        }
        barrier.wait();
        wait_for_hub(&mut hub, "three workers", |hub| hub.workers.is_empty());

        assert_eq!(executor.calls.load(Ordering::Acquire), 3);
        assert_eq!(calls.load(Ordering::Acquire), 3);
        assert_eq!(
            hub.state(&keys[0]),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge
            ))
        );
        assert_eq!(
            hub.state(&keys[1]),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForRejudge
            ))
        );
        assert_eq!(
            hub.state(&keys[2]),
            display_current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                Verdict::WrongAnswer
            )))
        );
    }

    #[test]
    fn worker_post_gate_allows_at_most_one_physical_attempt() {
        let post_count = Arc::new(AtomicUsize::new(0));
        let worker_posts = Arc::clone(&post_count);
        let executor = TestExecutor::for_key(
            key(),
            vec![Box::new(move |_, emit, cancellation| {
                if cancellation.try_begin_post() {
                    worker_posts.fetch_add(1, Ordering::AcqRel);
                }
                if cancellation.try_begin_post() {
                    worker_posts.fetch_add(1, Ordering::AcqRel);
                }
                assert!(cancellation.post_started());
                assert!(emit(SubmissionEvent::Accepted));
                Ok(SubmissionCompletion::Accepted)
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        let key = key();
        start_test_submission(&mut hub, &key, "A");
        wait_for_hub(&mut hub, "single physical attempt", |hub| {
            hub.workers.is_empty()
        });

        assert_eq!(post_count.load(Ordering::Acquire), 1);
    }

    #[test]
    fn worker_progress_is_monotonic_from_accepted_through_finished() {
        let progress = WorkerProgress::default();
        assert_eq!(progress.phase(), WorkerProgressPhase::PreAccepted);

        progress.observe_before_send(SubmissionEvent::Accepted);
        assert_eq!(progress.phase(), WorkerProgressPhase::AcceptedKnown);
        progress.observe_before_send(SubmissionEvent::Status {
            submission_id: SubmissionId::for_test(72),
            status: SubmissionStatus::Finished(Verdict::Accepted),
        });
        assert_eq!(progress.phase(), WorkerProgressPhase::FinishedKnown);

        progress.observe_before_send(SubmissionEvent::Accepted);
        assert_eq!(progress.phase(), WorkerProgressPhase::FinishedKnown);
    }

    #[test]
    fn state_is_keyed_by_exact_contest_and_stable_task_identity() {
        let mut hub = SubmissionHub::new();
        let adt = key();
        let abc = SubmissionKey::new("abc430", "abc430_a");
        hub.records.insert(
            adt.clone(),
            SubmissionRecord {
                generation: Some(1),
                state: Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                ..SubmissionRecord::default()
            },
        );

        assert_eq!(
            hub.state(&adt),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge
            ))
        );
        assert_eq!(hub.state(&abc), None);
    }

    #[test]
    fn creating_the_memory_only_hub_does_not_initialize_an_atcoder_client() {
        let hub = SubmissionHub::new();
        assert!(
            hub.client
                .client
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        );
        assert!(hub.records.is_empty());
    }

    #[test]
    fn replacement_waits_for_accepted_and_ignores_old_generation_afterward() {
        let mut hub = SubmissionHub::new();
        let key = key();
        let old_cancellation = Arc::new(SubmissionCancellation::default());
        hub.workers.push(SubmissionWorker {
            key: key.clone(),
            generation: 4,
            cancellation: Arc::clone(&old_cancellation),
            progress: Arc::new(WorkerProgress::default()),
            handle: Some(thread::spawn(|| {})),
        });
        hub.records.insert(
            key.clone(),
            SubmissionRecord {
                generation: Some(4),
                state: Some(TuiSubmissionState::Status(
                    SubmissionStatus::JudgingProgress {
                        judged: 18,
                        total: 50,
                        provisional: Some(Verdict::TimeLimitExceeded),
                    },
                )),
                pending_generation: Some(5),
                ..SubmissionRecord::default()
            },
        );

        assert!(hub.apply_event(event(
            &key,
            5,
            WorkerEventKind::Failed("pre-submit failure".to_string())
        )));
        assert!(old_cancellation.should_continue());
        // A pre-submit failure leaves generation 4 current. A later retry can use generation 5.
        hub.records.get_mut(&key).unwrap().pending_generation = Some(5);
        assert!(hub.apply_event(event(
            &key,
            5,
            WorkerEventKind::Submission(SubmissionEvent::Accepted)
        )));
        assert!(!old_cancellation.should_continue());
        assert!(hub.ensure_start_allowed(&key).is_err());
        assert!(hub.apply_event(event(
            &key,
            5,
            WorkerEventKind::Submission(SubmissionEvent::Status {
                submission_id: SubmissionId::for_test(2),
                status: SubmissionStatus::WaitingForJudge,
            })
        )));
        assert!(hub.ensure_start_allowed(&key).is_ok());
        assert!(!hub.apply_event(event(
            &key,
            4,
            WorkerEventKind::Submission(SubmissionEvent::Status {
                submission_id: SubmissionId::for_test(1),
                status: SubmissionStatus::Finished(Verdict::TimeLimitExceeded),
            })
        )));
        assert_eq!(
            hub.state(&key),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge
            ))
        );
    }

    #[test]
    fn stale_join_fallback_cannot_replace_a_newer_generation() {
        let mut hub = SubmissionHub::new();
        let key = key();
        hub.next_activity_seq = 23;
        hub.records.insert(
            key.clone(),
            SubmissionRecord {
                problem_index: Some("A".to_string()),
                generation: Some(5),
                state: Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                current_activity_seq: Some(17),
                pending_generation: Some(6),
                attempt_activity_seq: Some(22),
                ..SubmissionRecord::default()
            },
        );
        let latest_before = latest_label(&hub);

        assert!(!hub.handle_worker_join_panic(&key, 4, WorkerProgressPhase::FinishedKnown,));
        assert_eq!(hub.records[&key].generation, Some(5));
        assert_eq!(hub.records[&key].unknown_generation, None);
        assert_eq!(hub.records[&key].current_activity_seq, Some(17));
        assert_eq!(hub.records[&key].pending_generation, Some(6));
        assert_eq!(hub.records[&key].attempt_activity_seq, Some(22));
        assert_eq!(hub.next_activity_seq, 23);
        assert_eq!(latest_label(&hub), latest_before);
        assert_eq!(
            hub.state(&key),
            display_attempt(
                Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                TuiSubmissionAttemptState::Submitting,
            )
        );
    }

    #[test]
    fn unchanged_join_fallback_does_not_consume_activity_sequence() {
        for progress in [
            WorkerProgressPhase::AcceptedKnown,
            WorkerProgressPhase::FinishedKnown,
        ] {
            let mut hub = SubmissionHub::new();
            let key = key();
            hub.next_activity_seq = 23;
            hub.records.insert(
                key.clone(),
                SubmissionRecord {
                    problem_index: Some("A".to_string()),
                    generation: Some(5),
                    state: Some(TuiSubmissionState::TrackingUnavailable),
                    current_activity_seq: Some(17),
                    ..SubmissionRecord::default()
                },
            );

            assert!(hub.handle_worker_join_panic(&key, 5, progress));
            assert_eq!(hub.records[&key].current_activity_seq, Some(17));
            assert_eq!(hub.next_activity_seq, 23);
            assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Untracked"));
        }
    }

    #[test]
    fn accepted_join_fallback_keeps_the_problem_latest_when_its_attempt_disappears() {
        for progress in [
            WorkerProgressPhase::AcceptedKnown,
            WorkerProgressPhase::FinishedKnown,
        ] {
            let mut hub = SubmissionHub::new();
            let a = SubmissionKey::new("abc474", "abc474_a");
            let b = SubmissionKey::new("abc474", "abc474_b");
            hub.next_activity_seq = 12;
            hub.records.insert(
                b.clone(),
                SubmissionRecord {
                    problem_index: Some("B".to_string()),
                    generation: Some(1),
                    state: Some(TuiSubmissionState::TrackingUnavailable),
                    current_activity_seq: Some(5),
                    pending_generation: Some(2),
                    attempt_activity_seq: Some(11),
                    ..SubmissionRecord::default()
                },
            );
            hub.records.insert(
                a.clone(),
                SubmissionRecord {
                    problem_index: Some("A".to_string()),
                    generation: Some(1),
                    state: Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                        Verdict::Accepted,
                    ))),
                    current_activity_seq: Some(10),
                    ..SubmissionRecord::default()
                },
            );

            assert_eq!(latest_label(&hub).as_deref(), Some("SUB B Submitting"));
            assert!(hub.handle_worker_join_panic(&b, 2, progress));
            assert_eq!(hub.records[&b].generation, Some(2));
            assert_eq!(hub.records[&b].current_activity_seq, Some(12));
            assert_eq!(hub.records[&b].pending_generation, None);
            assert_eq!(hub.records[&b].attempt_activity_seq, None);
            assert_eq!(hub.records[&a].current_activity_seq, Some(10));
            assert_eq!(hub.next_activity_seq, 13);
            assert_eq!(latest_label(&hub).as_deref(), Some("SUB B Untracked"));
        }
    }

    #[test]
    fn join_fallback_hidden_by_a_different_generations_attempt_is_not_activity() {
        let mut hub = SubmissionHub::new();
        let key = key();
        hub.next_activity_seq = 23;
        hub.records.insert(
            key.clone(),
            SubmissionRecord {
                problem_index: Some("A".to_string()),
                generation: Some(5),
                state: Some(TuiSubmissionState::Accepted),
                current_activity_seq: Some(17),
                pending_generation: Some(6),
                attempt_activity_seq: Some(22),
                ..SubmissionRecord::default()
            },
        );

        assert!(hub.handle_worker_join_panic(&key, 5, WorkerProgressPhase::AcceptedKnown));
        assert_eq!(hub.records[&key].current_activity_seq, Some(17));
        assert_eq!(hub.records[&key].pending_generation, Some(6));
        assert_eq!(hub.records[&key].attempt_activity_seq, Some(22));
        assert_eq!(hub.next_activity_seq, 23);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Submitting"));
    }

    #[test]
    fn visible_join_fallback_change_consumes_exactly_one_activity_sequence() {
        let mut hub = SubmissionHub::new();
        let key = key();
        hub.next_activity_seq = 23;
        hub.records.insert(
            key.clone(),
            SubmissionRecord {
                problem_index: Some("A".to_string()),
                generation: Some(5),
                state: Some(TuiSubmissionState::Accepted),
                current_activity_seq: Some(17),
                ..SubmissionRecord::default()
            },
        );

        assert!(hub.handle_worker_join_panic(&key, 5, WorkerProgressPhase::AcceptedKnown));
        assert_eq!(hub.records[&key].current_activity_seq, Some(23));
        assert_eq!(hub.next_activity_seq, 24);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Untracked"));
    }

    #[test]
    fn production_stale_outer_panic_fallback_cannot_replace_a_newer_generation() {
        let target = key();
        let (s1_ready_tx, s1_ready_rx) = mpsc::channel();
        let (s1_panic_release_tx, s1_panic_release_rx) = mpsc::channel();
        let executor = TestExecutor::for_key(
            target.clone(),
            vec![
                Box::new(move |_, emit, cancellation| {
                    assert!(cancellation.try_begin_post());
                    assert!(emit(SubmissionEvent::Accepted));
                    assert!(emit(SubmissionEvent::Status {
                        submission_id: SubmissionId::for_test(73),
                        status: SubmissionStatus::WaitingForRejudge,
                    }));
                    s1_ready_tx.send(()).unwrap();
                    s1_panic_release_rx.recv().unwrap();

                    assert!(
                        !cancellation.should_continue(),
                        "S2 Accepted must cancel the stale S1 worker"
                    );
                    assert!(
                        !emit(SubmissionEvent::Accepted),
                        "the stale event is queued, but cancellation makes the callback stop"
                    );
                    panic_then_repanic_outside_catch()
                }),
                Box::new(|_, emit, cancellation| {
                    assert!(cancellation.try_begin_post());
                    assert!(emit(SubmissionEvent::Accepted));
                    assert!(emit(SubmissionEvent::Status {
                        submission_id: SubmissionId::for_test(74),
                        status: SubmissionStatus::WaitingForJudge,
                    }));
                    Ok(SubmissionCompletion::Accepted)
                }),
            ],
        );
        let mut hub = SubmissionHub::with_executor(executor);

        let s1 = start_test_submission(&mut hub, &target, "A");
        s1_ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_for_hub(&mut hub, "S1 WR", |hub| {
            hub.records.get(&target).is_some_and(|record| {
                record.generation == Some(s1)
                    && record.state
                        == Some(TuiSubmissionState::Status(
                            SubmissionStatus::WaitingForRejudge,
                        ))
            })
        });

        let s2 = start_test_submission(&mut hub, &target, "A");
        wait_for_hub(&mut hub, "S2 WJ", |hub| {
            hub.records.get(&target).is_some_and(|record| {
                record.generation == Some(s2)
                    && record.state
                        == Some(TuiSubmissionState::Status(
                            SubmissionStatus::WaitingForJudge,
                        ))
            })
        });
        assert_ne!(s1, s2);

        enqueue_unrelated_backlog(&hub, 1024);
        s1_panic_release_tx.send(()).unwrap();
        wait_for_worker_thread_exit(&hub, s1, "stale S1 outer panic");

        // A single production tick drains only the first 256 unrelated events, then reaps the
        // actual failed JoinHandle. S1's late Accepted and ordered WorkerPanicked events remain
        // behind the backlog while the join fallback sees that S1 is already stale.
        hub.handle_events();
        assert_eq!(hub.join_panic_fallback_count, 1);
        assert!(hub.workers.is_empty());
        assert_eq!(hub.records[&target].generation, Some(s2));
        assert_eq!(hub.records[&target].pending_generation, None);
        assert_eq!(hub.records[&target].unknown_generation, None);
        assert_eq!(
            hub.state(&target),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge,
            ))
        );

        drain_large_test_backlog(&mut hub);
        assert_eq!(hub.join_panic_fallback_count, 1);
        assert_eq!(hub.records[&target].generation, Some(s2));
        assert_eq!(hub.records[&target].pending_generation, None);
        assert_eq!(hub.records[&target].unknown_generation, None);
        assert_eq!(
            hub.state(&target),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge,
            ))
        );
    }

    #[test]
    fn failure_and_unknown_do_not_become_accepted() {
        let mut hub = SubmissionHub::new();
        let key = key();
        hub.records.insert(
            key.clone(),
            SubmissionRecord {
                generation: Some(1),
                state: Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge,
                )),
                pending_generation: Some(2),
                ..SubmissionRecord::default()
            },
        );
        assert!(hub.apply_event(event(
            &key,
            2,
            WorkerEventKind::Failed("rejected".to_string())
        )));
        assert_eq!(hub.records[&key].generation, Some(1));
        assert_eq!(
            hub.state(&key),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge
            ))
        );

        hub.records.get_mut(&key).unwrap().pending_generation = Some(3);
        let old_cancellation = Arc::new(SubmissionCancellation::default());
        hub.workers.push(SubmissionWorker {
            key: key.clone(),
            generation: 1,
            cancellation: Arc::clone(&old_cancellation),
            progress: Arc::new(WorkerProgress::default()),
            handle: Some(thread::spawn(|| {})),
        });
        assert!(hub.apply_event(event(&key, 3, WorkerEventKind::Unknown)));
        assert_eq!(hub.records[&key].generation, Some(1));
        assert_eq!(
            hub.state(&key),
            display_attempt(
                Some(TuiSubmissionState::Status(
                    SubmissionStatus::WaitingForJudge
                )),
                TuiSubmissionAttemptState::Unknown,
            )
        );
        assert!(old_cancellation.should_continue());
        assert!(hub.apply_event(event(
            &key,
            1,
            WorkerEventKind::Submission(SubmissionEvent::Status {
                submission_id: SubmissionId::for_test(1),
                status: SubmissionStatus::Finished(Verdict::Accepted),
            })
        )));
        assert_eq!(
            hub.records[&key].state,
            Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                Verdict::Accepted
            )))
        );
        assert_eq!(
            hub.state(&key),
            display_attempt(
                Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                    Verdict::Accepted
                ))),
                TuiSubmissionAttemptState::Unknown,
            )
        );
        assert!(
            hub.ensure_start_allowed(&key)
                .unwrap_err()
                .contains("unknown")
        );
    }

    #[test]
    fn replacement_source_read_failure_does_not_cancel_or_hide_the_old_tracker() {
        let temp = tempfile::tempdir().unwrap();
        let mut hub = SubmissionHub::new();
        let key = key();
        let old_cancellation = Arc::new(SubmissionCancellation::default());
        hub.workers.push(SubmissionWorker {
            key: key.clone(),
            generation: 1,
            cancellation: Arc::clone(&old_cancellation),
            progress: Arc::new(WorkerProgress::default()),
            handle: Some(thread::spawn(|| {})),
        });
        seed_current_activity(
            &mut hub,
            &key,
            "A",
            1,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
        );
        let next_activity_seq = hub.next_activity_seq;
        let plan = SubmitPlan::for_selected_source(
            key.contest_id.clone(),
            key.task_id.clone(),
            "A".to_string(),
            temp.path().join("missing.cpp"),
            Language::Cpp,
            PythonRuntime::CPython,
        );

        assert!(hub.start(key.clone(), "A".to_string(), plan).is_err());
        assert!(old_cancellation.should_continue());
        assert_eq!(hub.next_activity_seq, next_activity_seq);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A WJ"));
        assert_eq!(
            hub.state(&key),
            display_current(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge
            ))
        );
    }

    #[test]
    fn tracking_failure_keeps_the_accepted_generation_and_allows_resubmit() {
        let mut hub = SubmissionHub::new();
        let key = key();
        hub.records.insert(
            key.clone(),
            SubmissionRecord {
                generation: Some(8),
                state: Some(TuiSubmissionState::Accepted),
                discovering: true,
                ..SubmissionRecord::default()
            },
        );

        assert!(hub.apply_event(event(
            &key,
            8,
            WorkerEventKind::Submission(SubmissionEvent::TrackingUnavailable {
                submission_id: None,
            })
        )));
        assert_eq!(
            hub.state(&key),
            display_current(TuiSubmissionState::TrackingUnavailable)
        );
        assert_eq!(hub.records[&key].generation, Some(8));
        assert!(hub.ensure_start_allowed(&key).is_ok());
    }

    #[test]
    fn resubmit_gate_blocks_only_starting_and_unknown_states() {
        let key = key();
        for state in [
            None,
            Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                Verdict::Accepted,
            ))),
            Some(TuiSubmissionState::TrackingUnavailable),
            Some(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForJudge,
            )),
            Some(TuiSubmissionState::Status(
                SubmissionStatus::WaitingForRejudge,
            )),
            Some(TuiSubmissionState::Status(SubmissionStatus::Judging)),
            Some(TuiSubmissionState::Status(
                SubmissionStatus::JudgingProgress {
                    judged: 18,
                    total: 50,
                    provisional: Some(Verdict::TimeLimitExceeded),
                },
            )),
        ] {
            let mut hub = SubmissionHub::new();
            if let Some(state) = state {
                hub.records.insert(
                    key.clone(),
                    SubmissionRecord {
                        generation: Some(1),
                        state: Some(state),
                        ..SubmissionRecord::default()
                    },
                );
            }
            assert!(hub.ensure_start_allowed(&key).is_ok(), "state={state:?}");
        }

        let mut submitting = SubmissionHub::new();
        submitting.records.insert(
            key.clone(),
            SubmissionRecord {
                pending_generation: Some(2),
                ..SubmissionRecord::default()
            },
        );
        assert!(
            submitting
                .ensure_start_allowed(&key)
                .unwrap_err()
                .contains("already")
        );

        let mut discovering = SubmissionHub::new();
        discovering.records.insert(
            key.clone(),
            SubmissionRecord {
                generation: Some(2),
                state: Some(TuiSubmissionState::Accepted),
                discovering: true,
                ..SubmissionRecord::default()
            },
        );
        assert!(
            discovering
                .ensure_start_allowed(&key)
                .unwrap_err()
                .contains("already")
        );

        let mut unknown = SubmissionHub::new();
        unknown.records.insert(
            key.clone(),
            SubmissionRecord {
                unknown_generation: Some(3),
                ..SubmissionRecord::default()
            },
        );
        assert!(
            unknown
                .ensure_start_allowed(&key)
                .unwrap_err()
                .contains("unknown")
        );
    }

    #[test]
    fn problem_and_contest_view_changes_keep_records_and_tracking_alive() {
        let mut hub = SubmissionHub::new();
        let abc473_c = SubmissionKey::new("abc473", "abc473_c");
        let abc474_a = SubmissionKey::new("abc474", "abc474_a");
        let tracking_cancellation = Arc::new(SubmissionCancellation::default());
        hub.workers.push(SubmissionWorker {
            key: abc473_c.clone(),
            generation: 1,
            cancellation: Arc::clone(&tracking_cancellation),
            progress: Arc::new(WorkerProgress::default()),
            handle: Some(thread::spawn(|| {})),
        });
        hub.records.insert(
            abc473_c.clone(),
            SubmissionRecord {
                generation: Some(1),
                state: Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                    Verdict::Accepted,
                ))),
                ..SubmissionRecord::default()
            },
        );

        // Selecting another problem or rebuilding a WatchApp for another contest only changes
        // which key is queried. The outer-loop hub and its worker remain untouched.
        assert_eq!(hub.state(&abc474_a), None);
        assert!(tracking_cancellation.should_continue());
        assert_eq!(
            hub.state(&abc473_c),
            display_current(TuiSubmissionState::Status(SubmissionStatus::Finished(
                Verdict::Accepted
            )))
        );
        assert!(tracking_cancellation.should_continue());
        assert_eq!(SubmissionHub::new().state(&abc473_c), None);
    }

    #[test]
    fn whole_tui_stop_cancels_every_worker() {
        let mut hub = SubmissionHub::new();
        let first = Arc::new(SubmissionCancellation::default());
        let second = Arc::new(SubmissionCancellation::default());
        for (generation, cancellation) in [(1, &first), (2, &second)] {
            hub.workers.push(SubmissionWorker {
                key: key(),
                generation,
                cancellation: Arc::clone(cancellation),
                progress: Arc::new(WorkerProgress::default()),
                handle: Some(thread::spawn(|| {})),
            });
        }

        hub.request_stop();

        assert!(!first.should_continue());
        assert!(!second.should_continue());
    }

    #[test]
    fn latest_activity_follows_the_last_visible_status_change_and_has_no_ttl() {
        let mut hub = SubmissionHub::new();
        let a = SubmissionKey::new("abc474", "abc474_a");
        let b = SubmissionKey::new("abc474", "abc474_b");
        seed_current_activity(&mut hub, &a, "A", 1, TuiSubmissionState::Accepted);
        assert!(hub.update_current(
            a.clone(),
            1,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
            true,
        ));
        seed_current_activity(&mut hub, &b, "B", 2, TuiSubmissionState::Accepted);
        assert!(hub.update_current(
            b,
            2,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
            true,
        ));
        assert!(hub.update_current(
            a,
            1,
            TuiSubmissionState::Status(SubmissionStatus::JudgingProgress {
                judged: 14,
                total: 50,
                provisional: None,
            }),
            true,
        ));

        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A 14/50"));
        for _ in 0..10 {
            assert!(!hub.handle_events());
            assert_eq!(latest_label(&hub).as_deref(), Some("SUB A 14/50"));
        }
    }

    #[test]
    fn successful_start_is_a_new_activity_after_the_worker_exists() {
        let a = SubmissionKey::new("abc474", "abc474_a");
        let b = SubmissionKey::new("abc474", "abc474_b");
        let (entered_tx, entered_rx) = mpsc::channel();
        let executor = TestExecutor::for_key(
            b.clone(),
            vec![Box::new(move |_, _, cancellation| {
                entered_tx.send(()).unwrap();
                while cancellation.should_continue() {
                    thread::sleep(Duration::from_millis(2));
                }
                Ok(SubmissionCompletion::CancelledBeforeSubmit)
            })],
        );
        let mut hub = SubmissionHub::with_executor(executor);
        seed_current_activity(
            &mut hub,
            &a,
            "A",
            1,
            TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::Accepted)),
        );

        let generation = start_test_submission(&mut hub, &b, "B");
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB B Submitting"));
        assert_eq!(hub.records[&b].pending_generation, Some(generation));
        assert!(hub.records[&b].attempt_activity_seq.is_some());

        hub.request_stop();
    }

    #[test]
    fn a_new_attempt_becomes_latest_and_failed_or_cancelled_attempts_fall_back() {
        let a = SubmissionKey::new("abc474", "abc474_a");
        let b = SubmissionKey::new("abc474", "abc474_b");

        for terminal in [
            WorkerEventKind::Failed("rejected".to_string()),
            WorkerEventKind::CancelledBeforeSubmit,
        ] {
            let mut hub = SubmissionHub::new();
            seed_current_activity(
                &mut hub,
                &a,
                "A",
                1,
                TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::Accepted)),
            );
            seed_attempt_activity(&mut hub, &b, "B", 2);
            assert_eq!(latest_label(&hub).as_deref(), Some("SUB B Submitting"));

            assert!(hub.apply_event(event(&b, 2, terminal)));
            assert_eq!(hub.records[&b].attempt_activity_seq, None);
            assert_eq!(latest_label(&hub).as_deref(), Some("SUB A AC"));
        }
    }

    #[test]
    fn failed_or_cancelled_attempt_never_promotes_the_same_problems_older_current_activity() {
        let a = SubmissionKey::new("abc474", "abc474_a");
        let b = SubmissionKey::new("abc474", "abc474_b");

        for terminal in [
            WorkerEventKind::Failed("rejected".to_string()),
            WorkerEventKind::CancelledBeforeSubmit,
        ] {
            let mut hub = SubmissionHub::new();
            seed_current_activity(
                &mut hub,
                &b,
                "B",
                1,
                TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::WrongAnswer)),
            );
            seed_current_activity(
                &mut hub,
                &a,
                "A",
                2,
                TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::Accepted)),
            );
            let b_current_seq = hub.records[&b].current_activity_seq;
            seed_attempt_activity(&mut hub, &b, "B", 3);
            let next_activity_seq = hub.next_activity_seq;
            assert_eq!(latest_label(&hub).as_deref(), Some("SUB B Submitting"));

            assert!(hub.apply_event(event(&b, 3, terminal)));
            assert_eq!(hub.records[&b].current_activity_seq, b_current_seq);
            assert_eq!(hub.records[&b].attempt_activity_seq, None);
            assert_eq!(hub.next_activity_seq, next_activity_seq);
            assert_eq!(latest_label(&hub).as_deref(), Some("SUB A AC"));
        }
    }

    #[test]
    fn unknown_and_untracked_are_persistent_visible_activities() {
        let mut unknown = SubmissionHub::new();
        let a = SubmissionKey::new("abc474", "abc474_a");
        seed_attempt_activity(&mut unknown, &a, "A", 1);
        let submitting_seq = unknown.records[&a].attempt_activity_seq;
        assert!(unknown.apply_event(event(&a, 1, WorkerEventKind::Unknown)));
        assert!(unknown.records[&a].attempt_activity_seq > submitting_seq);
        assert_eq!(latest_label(&unknown).as_deref(), Some("SUB A Unknown"));
        assert!(!unknown.handle_events());
        assert_eq!(latest_label(&unknown).as_deref(), Some("SUB A Unknown"));

        let mut untracked = SubmissionHub::new();
        seed_current_activity(&mut untracked, &a, "A", 2, TuiSubmissionState::Accepted);
        untracked.records.get_mut(&a).unwrap().discovering = true;
        assert!(untracked.apply_event(event(
            &a,
            2,
            WorkerEventKind::Submission(SubmissionEvent::TrackingUnavailable {
                submission_id: None,
            }),
        )));
        assert_eq!(latest_label(&untracked).as_deref(), Some("SUB A Untracked"));
        assert!(!untracked.handle_events());
        assert_eq!(latest_label(&untracked).as_deref(), Some("SUB A Untracked"));
    }

    #[test]
    fn duplicate_visible_status_and_tracking_started_do_not_steal_latest_activity() {
        let mut hub = SubmissionHub::new();
        let a = SubmissionKey::new("abc474", "abc474_a");
        let b = SubmissionKey::new("abc474", "abc474_b");
        seed_current_activity(
            &mut hub,
            &a,
            "A",
            1,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
        );
        seed_current_activity(
            &mut hub,
            &b,
            "B",
            2,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
        );
        let next_activity_seq = hub.next_activity_seq;

        assert!(!hub.apply_event(event(
            &a,
            1,
            WorkerEventKind::Submission(SubmissionEvent::Status {
                submission_id: SubmissionId::for_test(1),
                status: SubmissionStatus::WaitingForJudge,
            }),
        )));
        assert_eq!(hub.next_activity_seq, next_activity_seq);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB B WJ"));

        hub.records.get_mut(&a).unwrap().state = Some(TuiSubmissionState::Accepted);
        hub.records.get_mut(&a).unwrap().discovering = true;
        let a_activity_seq = hub.records[&a].current_activity_seq;
        assert!(hub.apply_event(event(
            &a,
            1,
            WorkerEventKind::Submission(SubmissionEvent::TrackingStarted {
                submission_id: SubmissionId::for_test(1),
            }),
        )));
        assert_eq!(hub.records[&a].current_activity_seq, a_activity_seq);
        assert_eq!(hub.next_activity_seq, next_activity_seq);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB B WJ"));
    }

    #[test]
    fn accepted_to_waiting_for_judge_does_not_create_duplicate_visible_activity() {
        let mut hub = SubmissionHub::new();
        let a = SubmissionKey::new("abc474", "abc474_a");
        let b = SubmissionKey::new("abc474", "abc474_b");
        seed_current_activity(&mut hub, &a, "A", 1, TuiSubmissionState::Accepted);
        seed_attempt_activity(&mut hub, &b, "B", 2);
        let a_activity_seq = hub.records[&a].current_activity_seq;
        let next_activity_seq = hub.next_activity_seq;

        assert_eq!(latest_label(&hub).as_deref(), Some("SUB B Submitting"));
        assert!(hub.apply_event(event(
            &a,
            1,
            WorkerEventKind::Submission(SubmissionEvent::Status {
                submission_id: SubmissionId::for_test(1),
                status: SubmissionStatus::WaitingForJudge,
            }),
        )));

        assert_eq!(hub.records[&a].current_activity_seq, a_activity_seq);
        assert_eq!(hub.next_activity_seq, next_activity_seq);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB B Submitting"));
    }

    #[test]
    fn hidden_current_progress_and_final_status_do_not_steal_latest_activity() {
        let mut hub = SubmissionHub::new();
        let a = SubmissionKey::new("abc474", "abc474_a");
        let b = SubmissionKey::new("abc474", "abc474_b");
        seed_current_activity(
            &mut hub,
            &a,
            "A",
            1,
            TuiSubmissionState::Status(SubmissionStatus::JudgingProgress {
                judged: 14,
                total: 72,
                provisional: None,
            }),
        );
        seed_attempt_activity(&mut hub, &a, "A", 2);
        seed_current_activity(
            &mut hub,
            &b,
            "B",
            3,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
        );
        let a_activity_seq = hub.records[&a].current_activity_seq;
        let next_activity_seq = hub.next_activity_seq;

        for status in [
            SubmissionStatus::JudgingProgress {
                judged: 15,
                total: 72,
                provisional: None,
            },
            SubmissionStatus::JudgingProgress {
                judged: 16,
                total: 72,
                provisional: None,
            },
            SubmissionStatus::Finished(Verdict::Accepted),
        ] {
            assert!(hub.apply_event(event(
                &a,
                1,
                WorkerEventKind::Submission(SubmissionEvent::Status {
                    submission_id: SubmissionId::for_test(1),
                    status,
                }),
            )));
            assert_eq!(hub.records[&a].current_activity_seq, a_activity_seq);
            assert_eq!(hub.next_activity_seq, next_activity_seq);
            assert_eq!(latest_label(&hub).as_deref(), Some("SUB B WJ"));
            assert_eq!(hub.state(&a).unwrap().header_label("A"), "SUB A Submitting");
        }
    }

    #[test]
    fn accepted_s2_changes_submitting_to_wj_and_becomes_latest_activity() {
        let mut hub = SubmissionHub::new();
        let a = SubmissionKey::new("abc474", "abc474_a");
        let b = SubmissionKey::new("abc474", "abc474_b");
        seed_current_activity(
            &mut hub,
            &a,
            "A",
            1,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
        );
        seed_attempt_activity(&mut hub, &a, "A", 2);
        seed_current_activity(
            &mut hub,
            &b,
            "B",
            3,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
        );
        let next_activity_seq = hub.next_activity_seq;
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB B WJ"));

        assert!(hub.apply_event(event(
            &a,
            2,
            WorkerEventKind::Submission(SubmissionEvent::Accepted),
        )));

        assert_eq!(
            hub.records[&a].current_activity_seq,
            Some(next_activity_seq)
        );
        assert_eq!(hub.next_activity_seq, next_activity_seq + 1);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A WJ"));
    }

    #[test]
    fn stale_generation_events_cannot_change_activity_or_latest_selection() {
        let mut hub = SubmissionHub::new();
        let a = SubmissionKey::new("abc474", "abc474_a");
        let b = SubmissionKey::new("abc474", "abc474_b");
        seed_current_activity(
            &mut hub,
            &a,
            "A",
            2,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForRejudge),
        );
        seed_current_activity(
            &mut hub,
            &b,
            "B",
            3,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
        );
        let a_state = hub.records[&a].state;
        let a_activity_seq = hub.records[&a].current_activity_seq;
        let next_activity_seq = hub.next_activity_seq;

        assert!(!hub.apply_event(event(
            &a,
            1,
            WorkerEventKind::Submission(SubmissionEvent::Status {
                submission_id: SubmissionId::for_test(1),
                status: SubmissionStatus::Finished(Verdict::Accepted),
            }),
        )));
        assert!(!hub.apply_event(event(&a, 1, WorkerEventKind::WorkerPanicked,)));
        assert_eq!(hub.records[&a].state, a_state);
        assert_eq!(hub.records[&a].current_activity_seq, a_activity_seq);
        assert_eq!(hub.next_activity_seq, next_activity_seq);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB B WJ"));
    }

    #[test]
    fn s1_progress_is_hidden_while_s2_attempt_is_visible() {
        let mut hub = SubmissionHub::new();
        let a = SubmissionKey::new("abc474", "abc474_a");
        seed_current_activity(
            &mut hub,
            &a,
            "A",
            1,
            TuiSubmissionState::Status(SubmissionStatus::JudgingProgress {
                judged: 14,
                total: 50,
                provisional: None,
            }),
        );
        seed_attempt_activity(&mut hub, &a, "A", 2);
        let attempt_activity_seq = hub.records[&a].attempt_activity_seq;
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Submitting"));

        assert!(hub.apply_event(event(
            &a,
            1,
            WorkerEventKind::Submission(SubmissionEvent::Status {
                submission_id: SubmissionId::for_test(1),
                status: SubmissionStatus::JudgingProgress {
                    judged: 15,
                    total: 50,
                    provisional: None,
                },
            }),
        )));
        assert!(hub.records[&a].current_activity_seq < attempt_activity_seq);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A Submitting"));

        assert!(hub.apply_event(event(
            &a,
            2,
            WorkerEventKind::Submission(SubmissionEvent::Accepted),
        )));
        assert_eq!(hub.records[&a].attempt_activity_seq, None);
        assert_eq!(latest_label(&hub).as_deref(), Some("SUB A WJ"));
    }

    #[test]
    fn view_snapshot_keeps_latest_independent_of_selection_and_formats_cross_contest() {
        let contest = |contest_id: &str| Contest {
            contest_id: contest_id.to_string(),
            problems: ["A", "B"]
                .into_iter()
                .map(|index| Problem {
                    index: index.to_string(),
                    title: format!("Problem {index}"),
                    task_id: format!("{contest_id}_{}", index.to_ascii_lowercase()),
                    url: format!("https://example.invalid/{index}"),
                    sample_count: 1,
                })
                .collect(),
        };
        let abc474 = contest("abc474");
        let mut app = super::super::app::WatchApp::new(&abc474, vec![1, 1]).unwrap();
        let mut hub = SubmissionHub::new();
        let a = SubmissionKey::new("abc474", "abc474_a");
        seed_current_activity(
            &mut hub,
            &a,
            "A",
            1,
            TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
        );

        let initial = super::super::submission_view_state(&app, &hub);
        assert_eq!(initial.latest.unwrap().label(), "SUB A WJ");
        app.toggle_problem_status_mode();
        assert!(app.next_problem());
        assert_eq!(
            app.problem_status_mode(),
            super::super::app::ProblemStatusMode::Submissions
        );
        let after_problem_switch = super::super::submission_view_state(&app, &hub);
        assert_eq!(after_problem_switch.latest.unwrap().label(), "SUB A WJ");
        assert!(after_problem_switch.problems[0].is_some());
        assert!(after_problem_switch.problems[1].is_none());

        let abc475 = contest("abc475");
        let other_contest = super::super::app::WatchApp::new(&abc475, vec![1, 1]).unwrap();
        assert!(hub.update_current(
            a,
            1,
            TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::Accepted)),
            true,
        ));
        assert_eq!(
            app.problem_status_mode(),
            super::super::app::ProblemStatusMode::Submissions
        );
        let cross_contest = super::super::submission_view_state(&other_contest, &hub);
        assert_eq!(cross_contest.latest.unwrap().label(), "SUB abc474/A AC");
        assert!(cross_contest.problems.iter().all(Option::is_none));

        seed_attempt_activity(&mut hub, &SubmissionKey::new("abc474", "abc474_a"), "A", 2);
        let cross_contest_attempt = super::super::submission_view_state(&other_contest, &hub);
        assert_eq!(
            cross_contest_attempt.latest.unwrap().label(),
            "SUB abc474/A Submitting"
        );
    }

    #[test]
    fn every_status_has_the_requested_header_label() {
        let cases = [
            (TuiSubmissionState::Accepted, "WJ"),
            (
                TuiSubmissionState::Status(SubmissionStatus::WaitingForJudge),
                "WJ",
            ),
            (
                TuiSubmissionState::Status(SubmissionStatus::WaitingForRejudge),
                "WR",
            ),
            (
                TuiSubmissionState::Status(SubmissionStatus::JudgingProgress {
                    judged: 14,
                    total: 50,
                    provisional: None,
                }),
                "14/50",
            ),
            (
                TuiSubmissionState::Status(SubmissionStatus::JudgingProgress {
                    judged: 14,
                    total: 50,
                    provisional: Some(Verdict::WrongAnswer),
                }),
                "14/50 WA",
            ),
            (
                TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::Accepted)),
                "AC",
            ),
            (
                TuiSubmissionState::Status(SubmissionStatus::Finished(Verdict::WrongAnswer)),
                "WA",
            ),
            (TuiSubmissionState::TrackingUnavailable, "Untracked"),
        ];
        for (state, expected) in cases {
            assert_eq!(state.label(), expected);
        }
    }

    #[test]
    fn effective_user_visible_projection_is_shared_by_all_labels() {
        let cases = [
            (
                SubmissionDisplayState {
                    current: None,
                    attempt: Some(TuiSubmissionAttemptState::Submitting),
                },
                "Submitting",
            ),
            (
                SubmissionDisplayState {
                    current: Some(TuiSubmissionState::Accepted),
                    attempt: None,
                },
                "WJ",
            ),
            (
                SubmissionDisplayState {
                    current: Some(TuiSubmissionState::Status(
                        SubmissionStatus::WaitingForJudge,
                    )),
                    attempt: None,
                },
                "WJ",
            ),
            (
                SubmissionDisplayState {
                    current: Some(TuiSubmissionState::Status(
                        SubmissionStatus::WaitingForRejudge,
                    )),
                    attempt: None,
                },
                "WR",
            ),
            (
                SubmissionDisplayState {
                    current: Some(TuiSubmissionState::Status(
                        SubmissionStatus::WaitingForJudge,
                    )),
                    attempt: Some(TuiSubmissionAttemptState::Submitting),
                },
                "Submitting",
            ),
            (
                SubmissionDisplayState {
                    current: Some(TuiSubmissionState::Status(
                        SubmissionStatus::JudgingProgress {
                            judged: 14,
                            total: 50,
                            provisional: None,
                        },
                    )),
                    attempt: Some(TuiSubmissionAttemptState::Submitting),
                },
                "Submitting",
            ),
            (
                SubmissionDisplayState {
                    current: Some(TuiSubmissionState::Status(SubmissionStatus::Finished(
                        Verdict::Accepted,
                    ))),
                    attempt: Some(TuiSubmissionAttemptState::Unknown),
                },
                "Unknown",
            ),
        ];

        for (state, expected) in cases {
            assert_eq!(state.compact_label(), expected);
            assert_eq!(state.summary_label(), expected);
            let header = state.header_label("A");
            assert_eq!(header, format!("SUB A {expected}"));
            assert!(!header.contains("NEW"));
            assert!(!header.contains("Accepted"));
            assert!(!header.contains('·'));
        }
    }
}
