use std::io;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::language::Language;
use crate::model::Sample;
use crate::stress::CandidateFailureKind;

pub type RunId = u64;

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
    pub problem: usize,
    pub language: Language,
    pub debug: bool,
    pub kind: RunKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunWorkerCommand {
    Run(RunRequest),
    CancelStress {
        problem: usize,
        run_id: RunId,
    },
    RetireUserInputRuns {
        problem: usize,
        before_source_revision: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserInputRunTarget {
    Persisted(u64),
    Draft(u64),
}

// Shared only by requests for one problem in one contest session. Removal and physical
// attempt admission use the same lock, including when the worker command is still queued.
#[derive(Debug, Clone, Default)]
pub struct UserInputRunStartGate(Arc<Mutex<u64>>);

impl UserInputRunStartGate {
    pub(crate) fn retire_before(&self, source_revision: u64) {
        let mut minimum = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *minimum = (*minimum).max(source_revision);
    }

    pub(crate) fn is_retired(&self, source_revision: u64) -> bool {
        source_revision
            < *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn start_if_current<T>(
        &self,
        source_revision: u64,
        start: impl FnOnce() -> T,
    ) -> Option<T> {
        let minimum = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if source_revision < *minimum {
            return None;
        }
        // Keep the guard until spawning has finished; a check followed by an unlocked spawn
        // would allow removal to win in between and still start the obsolete request.
        let started = start();
        drop(minimum);
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
        problem: usize,
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
        problem: usize,
    },

    // The sender must emit this only after the old physical attempt has stopped sending events
    // and its thread has been joined. The same logical run_id is then safe to start again.
    RunRequeued {
        run_id: RunId,
        problem: usize,
    },

    RunEvent {
        run_id: RunId,
        problem: usize,
        event: TestEvent,
    },

    StressEvent {
        run_id: RunId,
        problem: usize,
        event: StressEvent,
    },

    RunCompleted {
        run_id: RunId,
        problem: usize,
    },

    RunFailed {
        run_id: RunId,
        problem: usize,
        error: String,
    },
}
