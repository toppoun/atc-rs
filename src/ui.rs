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

    TestRunStarted {
        problem_index: &'a str,
        total_cases: usize,
    },

    TestRunFinished {
        problem_index: &'a str,
        accepted: usize,
        total_cases: usize,
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

#[derive(Debug, Clone, Copy)]
enum DisplayVerdict {
    Ac,
    Wa,
    Re,
    Tle,
}

impl DisplayVerdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ac => "AC",
            Self::Wa => "WA",
            Self::Re => "RE",
            Self::Tle => "TLE",
        }
    }
}

#[derive(Debug)]
struct CaseDisplay {
    number: usize,
    verdict: DisplayVerdict,
    elapsed: Duration,
    expected: Option<String>,
    actual: Option<String>,
    stderr: Option<String>,
}

#[derive(Debug)]
struct TestDisplayState {
    cases: Vec<CaseDisplay>,
}

#[derive(Default)]
pub struct TerminalReporter {
    current_test: Option<TestDisplayState>,
}

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
                if let Some(test) = &mut self.current_test {
                    test.cases.push(CaseDisplay {
                        number,
                        verdict: DisplayVerdict::Ac,
                        elapsed,
                        expected: None,
                        actual: None,
                        stderr: None,
                    });
                }
            }

            Event::TestCaseWrongAnswer {
                number,
                expected,
                actual,
                elapsed,
            } => {
                if let Some(test) = &mut self.current_test {
                    test.cases.push(CaseDisplay {
                        number,
                        verdict: DisplayVerdict::Wa,
                        elapsed,
                        expected: Some(expected.to_owned()),
                        actual: Some(actual.to_owned()),
                        stderr: None,
                    });
                }
            }
            Event::TestCaseRuntimeError { number, elapsed } => {
                if let Some(test) = &mut self.current_test {
                    test.cases.push(CaseDisplay {
                        number,
                        verdict: DisplayVerdict::Re,
                        elapsed,
                        expected: None,
                        actual: None,
                        stderr: None,
                    });
                }
            }

            Event::TestCaseTimedOut { number, elapsed } => {
                if let Some(test) = &mut self.current_test {
                    test.cases.push(CaseDisplay {
                        number,
                        verdict: DisplayVerdict::Tle,
                        elapsed,
                        expected: None,
                        actual: None,
                        stderr: None,
                    });
                }
            }

            Event::TestCaseStderr { number, stderr } => {
                if let Some(test) = &mut self.current_test
                    && let Some(case) = test.cases.iter_mut().find(|case| case.number == number)
                {
                    case.stderr = Some(stderr.to_owned());
                }
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
            Event::TestRunStarted {
                problem_index: _,
                total_cases,
            } => {
                self.current_test = Some(TestDisplayState {
                    cases: Vec::with_capacity(total_cases),
                });
            }

            Event::TestRunFinished {
                accepted,
                total_cases,
                ..
            } => {
                self.print_test_result(accepted, total_cases);
            }
        }
    }
}

fn print_section(text: &str) {
    if text.is_empty() {
        println!("<empty>");
        return;
    }

    print!("{text}");

    if !text.ends_with('\n') {
        println!();
    }
}

impl TerminalReporter {
    fn print_test_result(&mut self, accepted: usize, total_cases: usize) {
        let Some(test) = self.current_test.take() else {
            return;
        };

        println!("Test Results");
        println!();

        println!("{:<12} {:<8} {:>10}", "Case", "Result", "Time");

        for case in &test.cases {
            println!(
                "{:<12} {:<8} {:>7.2} ms",
                format!("sample-{}", case.number),
                case.verdict.as_str(),
                case.elapsed.as_secs_f64() * 1000.0,
            );
        }

        println!();
        println!("Result: {accepted}/{total_cases} AC");

        for case in &test.cases {
            let has_stderr = case
                .stderr
                .as_deref()
                .is_some_and(|stderr| !stderr.is_empty());

            let needs_detail = !matches!(case.verdict, DisplayVerdict::Ac) || has_stderr;

            if !needs_detail {
                continue;
            }

            println!();
            println!("=== sample-{} {} ===", case.number, case.verdict.as_str());

            if let Some(expected) = &case.expected {
                println!();
                println!("expected:");
                print_section(expected);
            }

            if let Some(actual) = &case.actual {
                println!();
                println!("actual:");
                print_section(actual);
            }

            if let Some(stderr) = &case.stderr
                && !stderr.is_empty()
            {
                println!();
                println!("stderr:");
                print_section(stderr);
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
