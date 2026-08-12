use std::path::Path;
use std::time::Duration;

pub enum Event<'a> {
    ContestFetching {
        contest_id: &'a str,
    },
    ContestFetched {
        contest_id: &'a str,
        problems: usize,
    },
    ProblemFetching {
        index: &'a str,
        current: usize,
        total: usize,
    },
    ProblemFetched {
        index: &'a str,
        samples: usize,
    },
    ProblemFetchFailed {
        index: &'a str,
        error: &'a str,
    },
    WorkspaceCreated {
        destination: &'a Path,
    },
    WorkspaceRefreshed {
        destination: &'a Path,
    },
    NoSamples {
        problem_index: &'a str,
    },

    CompileFailed {
        stderr: &'a str,
    },

    CompileTimedOut {
        elapsed: Duration,
    },

    TestCaseAccepted {
        number: usize,
        elapsed: Duration,
    },

    TestCaseWrongAnswer {
        number: usize,
        expected: &'a str,
        actual: &'a str,
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
        stderr: &'a str,
    },
    SourceCreated {
        path: &'a Path,
    },
    WatchStarted {
        destination: &'a Path,
    },

    WatchSourceChanged {
        source: &'a Path,
    },
}

pub trait Reporter {
    fn report(&mut self, event: Event<'_>);
}

pub struct TerminalReporter;

impl Reporter for TerminalReporter {
    fn report(&mut self, event: Event<'_>) {
        match event {
            Event::ContestFetching { contest_id } => {
                eprintln!("Fetching contest {contest_id}...");
            }

            Event::ContestFetched {
                contest_id,
                problems,
            } => {
                eprintln!("Found {problems} problems in {contest_id}");
            }

            Event::ProblemFetching {
                index,
                current,
                total,
            } => {
                eprintln!("[{current}/{total}] Fetching {index}...");
            }

            Event::ProblemFetched { index, samples } => {
                eprintln!("  {index}: {samples} samples");
            }

            Event::ProblemFetchFailed { index, error } => {
                eprintln!("  warning: {index}: {error}");
            }

            Event::WorkspaceCreated { destination } => {
                eprintln!("Created {}", destination.display());
            }

            Event::WorkspaceRefreshed { destination } => {
                eprintln!("Refreshed {}", destination.display());
            }
            Event::NoSamples { problem_index } => {
                println!("No samples for {problem_index}");
            }

            Event::CompileFailed { stderr } => {
                println!("Compile Error");
                if !stderr.is_empty() {
                    eprintln!("{stderr}");
                }
            }

            Event::CompileTimedOut { elapsed } => {
                println!("Compile Timed Out ({elapsed:.2?})");
            }

            Event::TestCaseAccepted { number, elapsed } => {
                println!("Sample {number}: AC ({elapsed:.2?})");
            }

            Event::TestCaseWrongAnswer {
                number,
                expected,
                actual,
                elapsed,
            } => {
                println!("Sample {number}: WA ({elapsed:.2?})");
                println!("expected:");
                println!("{expected}");
                println!("actual:");
                println!("{actual}");
            }

            Event::TestCaseRuntimeError { number, elapsed } => {
                println!("Sample {number}: RE ({elapsed:.2?})");
            }

            Event::TestCaseTimedOut { number, elapsed } => {
                println!("Sample {number}: TLE ({elapsed:.2?})");
            }
            Event::TestCaseStderr { number, stderr } => {
                eprintln!("Sample {number} stderr:");
                eprint!("{stderr}");
            }
            Event::SourceCreated { path } => {
                println!("Created {}", path.display());
            }
            Event::WatchStarted { destination } => {
                println!("Watching {}", destination.display());
            }

            Event::WatchSourceChanged { source } => {
                println!();
                println!(
                    "Changed {}",
                    source
                        .file_name()
                        .unwrap_or(source.as_os_str())
                        .to_string_lossy()
                );
            }
        }
    }
}

#[cfg(test)]
pub struct NullReporter;

#[cfg(test)]
impl Reporter for NullReporter {
    fn report(&mut self, _: Event<'_>) {}
}
