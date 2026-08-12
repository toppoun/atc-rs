use std::io;
use std::path::PathBuf;

use crate::language::Language;
use std::time::Duration;

pub type RunId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunRequest {
    pub run_id: RunId,
    pub problem: usize,
    pub language: Language,
    pub debug: bool,
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

    TestRunStarted {
        total_cases: usize,
    },

    TestCaseAccepted {
        number: usize,
        elapsed: Duration,
    },

    TestCaseWrongAnswer {
        number: usize,
        expected: String,
        actual: String,
        elapsed: Duration,
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

    RunEvent {
        run_id: RunId,
        problem: usize,
        event: TestEvent,
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
