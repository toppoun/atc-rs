use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::language::Language;
use crate::model::Sample;
use crate::stress::CandidateFailureKind;

pub type RunId = u64;

// In-flight requests retain the token, so a live generation cannot be reused.
#[derive(Debug, Clone)]
struct GenerationToken(Arc<()>);

impl PartialEq for GenerationToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for GenerationToken {}

impl Hash for GenerationToken {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunScope {
    contest_id: Arc<str>,
    generation: GenerationToken,
}

impl RunScope {
    pub(crate) fn new(contest_id: &str) -> Self {
        Self {
            contest_id: Arc::from(contest_id),
            generation: GenerationToken(Arc::new(())),
        }
    }

    pub(crate) fn contest_id(&self) -> &str {
        &self.contest_id
    }

    pub(crate) fn target(&self, task_id: &str) -> RunTarget {
        RunTarget {
            scope: self.clone(),
            task_id: Arc::from(task_id),
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        static SCOPE: std::sync::OnceLock<RunScope> = std::sync::OnceLock::new();
        SCOPE.get_or_init(|| Self::new("abc123")).clone()
    }
}

// Shared by the registered context and each display model built for it.
#[derive(Debug, Clone)]
pub(crate) struct RunGeneration {
    scope: RunScope,
    state: Arc<RunGenerationState>,
}

#[derive(Debug)]
struct RunGenerationState {
    next_run_id: AtomicU64,
    source_gates: Mutex<HashMap<String, UserInputRunStartGate>>,
}

impl RunGeneration {
    pub(crate) fn new(contest_id: &str) -> Self {
        Self::with_scope(RunScope::new(contest_id))
    }

    pub(crate) fn scope(&self) -> &RunScope {
        &self.scope
    }

    #[cfg(test)]
    pub(crate) fn fixture(scope: RunScope) -> Self {
        Self::with_scope(scope)
    }

    fn with_scope(scope: RunScope) -> Self {
        Self {
            scope,
            state: Arc::new(RunGenerationState {
                next_run_id: AtomicU64::new(1),
                source_gates: Mutex::default(),
            }),
        }
    }

    pub(super) fn next_run_id(&self) -> RunId {
        self.state
            .next_run_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("run ID space exhausted")
    }

    pub(super) fn source_gate(&self, task_id: &str) -> UserInputRunStartGate {
        self.state
            .source_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(task_id.to_owned())
            .or_default()
            .clone()
    }
}

// The logical problem is (contest_id, task_id). The scope ties its queued work,
// source revisions and events to a particular registered metadata generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunTarget {
    pub(crate) scope: RunScope,
    pub(crate) task_id: Arc<str>,
}

#[cfg(test)]
impl RunTarget {
    pub(crate) fn fixture(problem: usize) -> Self {
        RunScope::fixture().target(&format!("abc123_{problem}"))
    }

    pub(crate) fn fixture_index(&self) -> usize {
        self.task_id
            .strip_prefix("abc123_")
            .unwrap()
            .parse()
            .unwrap()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunKind {
    Samples,
    UserInput(Arc<UserInputRunSnapshot>),
    Stress {
        base_seed: u64,
        count: Option<NonZeroU64>,
    },
}

impl RunKind {
    pub(crate) fn preserve_on_preemption(&self) -> bool {
        matches!(self, Self::Samples)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub run_id: RunId,
    pub target: RunTarget,
    pub language: Language,
    pub debug: bool,
    pub kind: RunKind,
}

impl RunRequest {
    pub(crate) fn matches(&self, target: &RunTarget, run_id: RunId) -> bool {
        self.target == *target && self.run_id == run_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunWorkerCommand {
    Run(RunRequest),
    CancelStress {
        target: RunTarget,
        run_id: RunId,
    },
    RetireUserInputRuns {
        target: RunTarget,
        before_source_revision: u64,
    },
}

impl RunWorkerCommand {
    pub(crate) fn target(&self) -> &RunTarget {
        match self {
            Self::Run(request) => &request.target,
            Self::CancelStress { target, .. } | Self::RetireUserInputRuns { target, .. } => target,
        }
    }
}

/// One execution scope's result route. With no view attached, events can be discarded.
#[derive(Clone, Default)]
pub(crate) struct RunResults(Arc<Mutex<Option<Sender<Message>>>>);

impl From<Sender<Message>> for RunResults {
    fn from(sender: Sender<Message>) -> Self {
        Self(Arc::new(Mutex::new(Some(sender))))
    }
}

impl RunResults {
    pub(crate) fn detach(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub(crate) fn send(&self, message: Message) -> io::Result<()> {
        if let Some(sender) = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            sender.send(message).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TUI message receiver disconnected from run worker",
                )
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserInputRunTarget {
    Persisted(u64),
    Draft(u64),
}

// Shared only by requests for one task in one execution generation. Removal and physical
// attempt admission use the same lock, including when the worker command is still queued.
#[derive(Debug, Clone, Default)]
pub struct UserInputRunStartGate(Arc<Mutex<SourceRevisions>>);

#[derive(Debug, Default)]
struct SourceRevisions {
    current: u64,
    retired_before: u64,
}

impl UserInputRunStartGate {
    pub(super) fn advance(&self, removed: bool) -> u64 {
        let mut revisions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        revisions.current = revisions
            .current
            .checked_add(1)
            .expect("source revision space exhausted");
        if removed {
            revisions.retired_before = revisions.current;
        }
        revisions.current
    }

    #[cfg(test)]
    pub(crate) fn retire_before(&self, source_revision: u64) {
        let mut revisions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        revisions.retired_before = revisions.retired_before.max(source_revision);
        revisions.current = revisions.current.max(source_revision);
    }

    pub(crate) fn is_retired(&self, source_revision: u64) -> bool {
        source_revision
            < self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retired_before
    }

    pub(crate) fn start_if_current<T>(
        &self,
        source_revision: u64,
        start: impl FnOnce() -> T,
    ) -> Option<T> {
        let revisions = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if source_revision < revisions.retired_before {
            return None;
        }
        // Keep the guard until spawning has finished; a check followed by an unlocked spawn
        // would allow removal to win in between and still start the obsolete request.
        let started = start();
        drop(revisions);
        Some(started)
    }
}

#[derive(Debug, Clone)]
pub struct UserInputRunSnapshot {
    pub problem_index: String,
    pub target: UserInputRunTarget,
    // Exact stdin from Run or Save; source execution uses the normal live path.
    pub input: Arc<str>,
    // Reject completions after a source-change notification, without capturing source bytes.
    pub source_revision: u64,
    pub start_gate: UserInputRunStartGate,
}

// Admission control is mutable runtime state, not part of the immutable result identity.
impl PartialEq for UserInputRunSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.problem_index == other.problem_index
            && self.target == other.target
            && self.input == other.input
            && self.source_revision == other.source_revision
    }
}

impl Eq for UserInputRunSnapshot {}

#[cfg(test)]
mod user_input_start_gate_tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn removal_and_physical_spawn_share_one_admission_boundary() {
        let gate = UserInputRunStartGate::default();
        let worker_gate = gate.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_gate.start_if_current(1, || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                "started"
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // The source removal lock cannot be acquired while physical spawn is in progress.
        let locked_through_spawn = gate.0.try_lock().is_err();
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), Some("started"));
        assert!(locked_through_spawn);
        gate.retire_before(2);
        gate.retire_before(1); // Delayed removal cannot lower the boundary.
        assert!(
            gate.start_if_current(1, || panic!("retired request spawned"))
                .is_none()
        );
        assert_eq!(gate.start_if_current(3, || "recreated"), Some("recreated"));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserInputRunStatus {
    Queued,
    Compiling,
    Running,
    Finished,
    RuntimeError,
    TimedOut,
    CompileError,
    CompileTimedOut,
    Cancelled,
    Failed,
}

impl UserInputRunStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Compiling | Self::Running)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Compiling => "Compiling",
            Self::Running => "Running",
            Self::Finished => "Finished",
            Self::RuntimeError => "Runtime Error",
            Self::TimedOut => "Time Limit Exceeded",
            Self::CompileError => "Compile Error",
            Self::CompileTimedOut => "Compile Timed Out",
            Self::Cancelled => "Cancelled",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInputRunResult {
    pub status: UserInputRunStatus,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub enum UserInputRunEvent {
    Running,
    Finished(UserInputRunResult),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestEvent {
    NoSamples,
    SavedStressCaseSkipped {
        reason: String,
    },

    CompileFailed {
        stderr: String,
    },

    CompileTimedOut {
        elapsed: Duration,
    },

    TestCaseLayout {
        sample_cases: usize,
        stress_case: Option<Sample>,
    },

    TestRunStarted {
        total_cases: usize,
    },

    TestCaseAccepted {
        number: usize,
        elapsed: Duration,
    },
    TestCaseWrongAnswer {
        number: usize,
        elapsed: Duration,
    },

    TestCaseComparison {
        number: usize,
        input: String,
        expected: String,
        actual: String,
    },

    TestCaseRuntimeError {
        number: usize,
        elapsed: Duration,
    },

    TestCaseTimedOut {
        number: usize,
        elapsed: Duration,
    },

    TestCaseStderr {
        number: usize,
        stderr: String,
    },

    TestRunFinished {
        accepted: usize,
        total_cases: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum StressEvent {
    Started {
        base_seed: u64,
        case_limit: Option<u64>,
    },
    Progress {
        case_number: u64,
        seed: u64,
        passed: u64,
        elapsed: Duration,
        cases_per_second: f64,
    },
    Failed {
        kind: CandidateFailureKind,
        case_number: u64,
        base_seed: u64,
        seed: u64,
        input: String,
        expected: String,
        actual: String,
        stderr: String,
        candidate_elapsed: Duration,
        elapsed: Duration,
        saved_to: PathBuf,
    },
    Finished {
        cases: u64,
        elapsed: Duration,
    },
    Cancelled {
        cases: u64,
        elapsed: Duration,
    },
}

#[derive(Debug)]
pub enum Message {
    UserInputRunEvent {
        run_id: RunId,
        target: RunTarget,
        snapshot: Arc<UserInputRunSnapshot>,
        event: UserInputRunEvent,
    },
    SourceChanged {
        problem: usize,
        path: PathBuf,
        language: Language,
    },
    SourceRemoved {
        problem: usize,
        path: PathBuf,
        language: Language,
    },

    WatcherFailed(io::Error),

    WorkerFailed(io::Error),

    RunStarted {
        run_id: RunId,
        target: RunTarget,
    },

    // The sender must emit this only after the old physical attempt has stopped sending events
    // and its thread has been joined. The same logical run_id is then safe to start again.
    RunRequeued {
        run_id: RunId,
        target: RunTarget,
    },

    CompileDiagnostics {
        run_id: RunId,
        target: RunTarget,
        stderr: String,
    },

    RunEvent {
        run_id: RunId,
        target: RunTarget,
        event: TestEvent,
    },

    StressEvent {
        run_id: RunId,
        target: RunTarget,
        event: StressEvent,
    },

    RunCompleted {
        run_id: RunId,
        target: RunTarget,
    },

    RunFailed {
        run_id: RunId,
        target: RunTarget,
        error: String,
    },
}
