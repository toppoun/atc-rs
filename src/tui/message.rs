use std::io;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::time::Duration;

use crate::language::Language;
use crate::model::Sample;
use crate::stress::CandidateFailureKind;

pub type RunId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Samples,
    Stress {
        base_seed: u64,
        count: Option<NonZeroU64>,
    },
}

impl RunKind {
    pub(crate) fn preserve_on_preemption(self) -> bool {
        matches!(self, Self::Samples)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunRequest {
    pub run_id: RunId,
    pub problem: usize,
    pub language: Language,
    pub debug: bool,
    pub kind: RunKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunWorkerCommand {
    Run(RunRequest),
    CancelStress { problem: usize, run_id: RunId },
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
    SourceChanged {
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
