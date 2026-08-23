use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::model::Sample;

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
    WorkspaceInitialized {
        path: &'a Path,
    },
    WorkspaceAlreadyInitialized {
        path: &'a Path,
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

    TestCaseLayout {
        problem_index: &'a str,
        sample_cases: usize,
        stress_case: Option<&'a Sample>,
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
        elapsed: Duration,
    },

    TestCaseComparison {
        number: usize,
        expected: &'a str,
        actual: &'a str,
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
    StressFileCreated {
        path: &'a Path,
    },
    StressFileExists {
        path: &'a Path,
    },
    StressFilesAlreadyInitialized {
        problem_index: &'a str,
    },
    WatchStarted {
        destination: &'a Path,
    },

    WatchSourceChanged {
        source: &'a Path,
    },

    StressStarted {
        problem_index: &'a str,
        base_seed: u64,
        case_limit: Option<u64>,
    },

    StressProgress {
        problem_index: &'a str,
        case_number: u64,
        seed: u64,
        passed: u64,
        elapsed: Duration,
        cases_per_second: f64,
    },

    StressFailed {
        problem_index: &'a str,
        failure: &'a crate::stress::StressFailure,
        saved_to: &'a Path,
        elapsed: Duration,
    },

    StressFinished {
        problem_index: &'a str,
        cases: u64,
        elapsed: Duration,
    },

    StressCancelled {
        problem_index: &'a str,
        cases: u64,
        elapsed: Duration,
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
    problem_index: String,
    sample_cases: usize,
    total_cases: usize,
    cases: Vec<CaseDisplay>,
}

impl TestDisplayState {
    fn record_case(&mut self, case: CaseDisplay) {
        if self
            .cases
            .iter()
            .any(|existing| existing.number == case.number)
        {
            return;
        }

        self.cases.push(case);
    }

    fn record_stderr(&mut self, number: usize, stderr: &str) {
        if let Some(case) = self.cases.iter_mut().find(|case| case.number == number)
            && case.stderr.is_none()
        {
            case.stderr = Some(stderr.to_owned());
        }
    }
    fn record_comparison(&mut self, number: usize, expected: &str, actual: &str) {
        if let Some(case) = self.cases.iter_mut().find(|case| case.number == number)
            && case.expected.is_none()
            && case.actual.is_none()
        {
            case.expected = Some(expected.to_owned());
            case.actual = Some(actual.to_owned());
        }
    }
}

#[derive(Default)]
pub struct TerminalReporter {
    current_test: Option<TestDisplayState>,
    pending_test_layout: Option<(String, usize)>,
    stress_progress_visible: bool,
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
            Event::WorkspaceInitialized { path } => {
                eprintln!("Initialized atc workspace: {}", path.display());
            }
            Event::WorkspaceAlreadyInitialized { path } => {
                eprintln!("Workspace already initialized: {}", path.display());
            }
            Event::NoSamples { problem_index } => {
                self.current_test = None;
                println!("No samples for {problem_index}");
            }

            Event::CompileFailed { stderr } => {
                self.current_test = None;
                println!("Compile Error");
                if !stderr.is_empty() {
                    eprintln!("{stderr}");
                }
            }

            Event::CompileTimedOut { elapsed } => {
                self.current_test = None;
                println!("Compile Timed Out ({elapsed:.2?})");
            }

            Event::TestCaseAccepted { number, elapsed } => {
                if let Some(test) = &mut self.current_test {
                    test.record_case(CaseDisplay {
                        number,
                        verdict: DisplayVerdict::Ac,
                        elapsed,
                        expected: None,
                        actual: None,
                        stderr: None,
                    });
                }
            }

            Event::TestCaseWrongAnswer { number, elapsed } => {
                if let Some(test) = &mut self.current_test {
                    test.record_case(CaseDisplay {
                        number,
                        verdict: DisplayVerdict::Wa,
                        elapsed,
                        expected: None,
                        actual: None,
                        stderr: None,
                    });
                }
            }
            Event::TestCaseComparison {
                number,
                expected,
                actual,
            } => {
                if let Some(test) = &mut self.current_test {
                    test.record_comparison(number, expected, actual);
                }
            }
            Event::TestCaseRuntimeError { number, elapsed } => {
                if let Some(test) = &mut self.current_test {
                    test.record_case(CaseDisplay {
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
                    test.record_case(CaseDisplay {
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
                if let Some(test) = &mut self.current_test {
                    test.record_stderr(number, stderr);
                }
            }
            Event::SourceCreated { path } => {
                println!("Created {}", path.display());
            }
            Event::StressFileCreated { path } => {
                println!("Created {}", path.display());
            }
            Event::StressFileExists { path } => {
                println!("Exists {}", path.display());
            }
            Event::StressFilesAlreadyInitialized { problem_index } => {
                println!("Stress files already initialized for {problem_index}.");
            }
            Event::WatchStarted { destination } => {
                println!("Watching {}", destination.display());
            }

            Event::WatchSourceChanged { source } => {
                self.current_test = None;
                println!();
                println!(
                    "Changed {}",
                    source
                        .file_name()
                        .unwrap_or(source.as_os_str())
                        .to_string_lossy()
                );
            }
            Event::TestCaseLayout {
                problem_index,
                sample_cases,
                ..
            } => {
                self.pending_test_layout = Some((problem_index.to_owned(), sample_cases));
            }

            Event::TestRunStarted {
                problem_index,
                total_cases,
            } => {
                let sample_cases = self
                    .pending_test_layout
                    .take()
                    .filter(|(index, _)| index == problem_index)
                    .map(|(_, sample_cases)| sample_cases)
                    .unwrap_or(total_cases);
                self.current_test = Some(TestDisplayState {
                    problem_index: problem_index.to_owned(),
                    sample_cases,
                    total_cases,
                    cases: Vec::with_capacity(total_cases),
                });
            }

            Event::TestRunFinished {
                problem_index,
                accepted,
                total_cases,
            } => {
                if let Some(output) = self.finish_test_result(problem_index, accepted, total_cases)
                {
                    print!("{output}");
                }
            }

            Event::StressStarted {
                problem_index,
                base_seed,
                case_limit,
            } => {
                self.finish_stress_progress_line();
                match case_limit {
                    Some(limit) => {
                        println!("Stress {problem_index}  seed {base_seed}  max {limit} cases");
                    }
                    None => {
                        println!("Stress {problem_index}  seed {base_seed}  until failure");
                    }
                }
            }

            Event::StressProgress {
                problem_index,
                case_number,
                seed,
                passed,
                elapsed,
                cases_per_second,
            } => {
                eprint!(
                    "\rStress {problem_index}  passed {passed}  case {case_number}  seed {seed}  {cases_per_second:.1} cases/s  {elapsed:.1?}"
                );
                let _ = std::io::stderr().flush();
                self.stress_progress_visible = true;
            }

            Event::StressFailed {
                problem_index,
                failure,
                saved_to,
                elapsed,
            } => {
                self.finish_stress_progress_line();
                println!(
                    "Stress {problem_index}: {} at case {} (seed {}) after {elapsed:.2?}, candidate {candidate_elapsed:.2?}",
                    failure.kind.as_str(),
                    failure.case_number,
                    failure.seed,
                    candidate_elapsed = failure.elapsed
                );
                println!();
                println!("input:");
                print!("{}", print_section(&failure.input));

                println!();
                println!("expected:");
                print!("{}", print_section(&failure.expected));

                println!();
                println!("actual:");
                print!("{}", print_section(&failure.actual));

                if !failure.stderr.is_empty() {
                    println!();
                    println!("stderr:");
                    print!("{}", print_section(&failure.stderr));
                }

                println!();
                println!("Saved to {}", saved_to.display());
            }

            Event::StressFinished {
                problem_index,
                cases,
                elapsed,
            } => {
                self.finish_stress_progress_line();
                println!("Stress {problem_index}: {cases} cases passed ({elapsed:.2?})");
            }

            Event::StressCancelled {
                problem_index,
                cases,
                elapsed,
            } => {
                self.finish_stress_progress_line();
                println!("Stress {problem_index}: cancelled after {cases} cases ({elapsed:.2?})");
            }
        }
    }
}

fn case_label(number: usize, sample_cases: usize) -> String {
    if number <= sample_cases {
        format!("sample-{number}")
    } else {
        format!("stress-{}", number - sample_cases)
    }
}

fn print_section(text: &str) -> String {
    if text.is_empty() {
        return "<empty>\n".to_string();
    }

    let mut output = text.to_owned();

    if !text.ends_with('\n') {
        output.push('\n');
    }

    output
}

impl TerminalReporter {
    fn finish_stress_progress_line(&mut self) {
        if self.stress_progress_visible {
            eprintln!();
            self.stress_progress_visible = false;
        }
    }

    fn finish_test_result(
        &mut self,
        problem_index: &str,
        accepted: usize,
        total_cases: usize,
    ) -> Option<String> {
        let current = self.current_test.as_ref()?;

        if current.problem_index != problem_index || current.total_cases != total_cases {
            return None;
        }

        let test = self.current_test.take()?;
        let mut output = String::new();

        use std::fmt::Write as _;

        writeln!(output, "Test Results").expect("writing to String cannot fail");
        writeln!(output).expect("writing to String cannot fail");

        writeln!(output, "{:<12} {:<8} {:>10}", "Case", "Result", "Time")
            .expect("writing to String cannot fail");

        for case in &test.cases {
            writeln!(
                output,
                "{:<12} {:<8} {:>7.2} ms",
                case_label(case.number, test.sample_cases),
                case.verdict.as_str(),
                case.elapsed.as_secs_f64() * 1000.0,
            )
            .expect("writing to String cannot fail");
        }

        writeln!(output).expect("writing to String cannot fail");
        writeln!(output, "Result: {accepted}/{total_cases} AC")
            .expect("writing to String cannot fail");

        for case in &test.cases {
            let has_stderr = case
                .stderr
                .as_deref()
                .is_some_and(|stderr| !stderr.is_empty());

            let has_comparison = case.expected.is_some() || case.actual.is_some();

            let needs_detail =
                !matches!(case.verdict, DisplayVerdict::Ac) || has_comparison || has_stderr;

            if !needs_detail {
                continue;
            }

            writeln!(output).expect("writing to String cannot fail");
            writeln!(
                output,
                "=== {} {} ===",
                case_label(case.number, test.sample_cases),
                case.verdict.as_str()
            )
            .expect("writing to String cannot fail");

            if let Some(expected) = &case.expected {
                writeln!(output).expect("writing to String cannot fail");
                writeln!(output, "expected:").expect("writing to String cannot fail");
                output.push_str(&print_section(expected));
            }

            if let Some(actual) = &case.actual {
                writeln!(output).expect("writing to String cannot fail");
                writeln!(output, "actual:").expect("writing to String cannot fail");
                output.push_str(&print_section(actual));
            }

            if let Some(stderr) = &case.stderr
                && !stderr.is_empty()
            {
                writeln!(output).expect("writing to String cannot fail");
                writeln!(output, "stderr:").expect("writing to String cannot fail");
                output.push_str(&print_section(stderr));
            }
        }

        Some(output)
    }
}

#[cfg(test)]
pub struct NullReporter;

#[cfg(test)]
impl Reporter for NullReporter {
    fn report(&mut self, _: Event<'_>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const ELAPSED: Duration = Duration::from_micros(7_200);

    fn start(reporter: &mut TerminalReporter, problem_index: &str, total_cases: usize) {
        reporter.report(Event::TestRunStarted {
            problem_index,
            total_cases,
        });
    }

    #[test]
    fn labels_promoted_stress_case_separately_from_samples() {
        let mut reporter = TerminalReporter::default();
        let stress = Sample {
            input: "1\n".to_string(),
            output: "2\n".to_string(),
        };
        reporter.report(Event::TestCaseLayout {
            problem_index: "A",
            sample_cases: 2,
            stress_case: Some(&stress),
        });
        start(&mut reporter, "A", 3);
        for number in 1..=3 {
            reporter.report(Event::TestCaseAccepted {
                number,
                elapsed: ELAPSED,
            });
        }

        let output = reporter.finish_test_result("A", 3, 3).unwrap();
        assert!(output.contains("sample-1"));
        assert!(output.contains("sample-2"));
        assert!(output.contains("stress-1"));
        assert!(!output.contains("sample-3"));
    }

    #[test]
    fn renders_summary_and_only_required_case_details() {
        let mut reporter = TerminalReporter::default();
        start(&mut reporter, "A", 4);
        reporter.report(Event::TestCaseAccepted {
            number: 1,
            elapsed: ELAPSED,
        });
        reporter.report(Event::TestCaseWrongAnswer {
            number: 2,
            elapsed: Duration::from_micros(6_800),
        });
        reporter.report(Event::TestCaseComparison {
            number: 2,
            expected: "",
            actual: "",
        });
        reporter.report(Event::TestCaseStderr {
            number: 2,
            stderr: "warning  \n",
        });
        reporter.report(Event::TestCaseRuntimeError {
            number: 3,
            elapsed: Duration::from_millis(2),
        });
        reporter.report(Event::TestCaseStderr {
            number: 3,
            stderr: "runtime error",
        });
        reporter.report(Event::TestCaseTimedOut {
            number: 4,
            elapsed: Duration::from_secs(2),
        });
        reporter.report(Event::TestCaseStderr {
            number: 4,
            stderr: " \t\n",
        });

        let output = reporter.finish_test_result("A", 1, 4).unwrap();

        assert!(output.contains("sample-1     AC"));
        assert!(output.contains("sample-2     WA"));
        assert!(output.contains("sample-3     RE"));
        assert!(output.contains("sample-4     TLE"));
        assert!(output.contains("Result: 1/4 AC"));
        assert!(!output.contains("=== sample-1 AC ==="));
        assert!(output.contains(
            "=== sample-2 WA ===\n\nexpected:\n<empty>\n\nactual:\n<empty>\n\nstderr:\nwarning  \n"
        ));
        assert!(output.contains("=== sample-3 RE ===\n\nstderr:\nruntime error\n"));
        assert!(output.contains("=== sample-4 TLE ===\n\nstderr:\n \t\n"));
    }

    #[test]
    fn accepted_case_with_stderr_has_details_and_keeps_whitespace() {
        let mut reporter = TerminalReporter::default();
        start(&mut reporter, "A", 1);
        reporter.report(Event::TestCaseAccepted {
            number: 1,
            elapsed: ELAPSED,
        });
        reporter.report(Event::TestCaseStderr {
            number: 1,
            stderr: "  debug output  ",
        });

        let output = reporter.finish_test_result("A", 1, 1).unwrap();

        assert!(output.contains("=== sample-1 AC ===\n\nstderr:\n  debug output  \n"));
    }

    #[test]
    fn ignores_case_events_without_a_started_run() {
        let mut reporter = TerminalReporter::default();

        reporter.report(Event::TestCaseWrongAnswer {
            number: 1,
            elapsed: ELAPSED,
        });
        reporter.report(Event::TestCaseStderr {
            number: 1,
            stderr: "stderr",
        });

        assert!(reporter.current_test.is_none());
        assert!(reporter.finish_test_result("A", 0, 1).is_none());
    }

    #[test]
    fn mismatched_finish_does_not_consume_the_current_run() {
        let mut reporter = TerminalReporter::default();
        start(&mut reporter, "A", 1);
        reporter.report(Event::TestCaseAccepted {
            number: 1,
            elapsed: ELAPSED,
        });

        assert!(reporter.finish_test_result("B", 1, 1).is_none());
        assert!(reporter.finish_test_result("A", 1, 2).is_none());
        assert!(reporter.current_test.is_some());

        let output = reporter.finish_test_result("A", 1, 1).unwrap();
        assert!(output.contains("sample-1     AC"));
        assert!(reporter.current_test.is_none());
    }

    #[test]
    fn duplicate_case_and_stderr_events_are_idempotent() {
        let mut reporter = TerminalReporter::default();
        start(&mut reporter, "A", 1);

        for _ in 0..2 {
            reporter.report(Event::TestCaseAccepted {
                number: 1,
                elapsed: ELAPSED,
            });
        }
        reporter.report(Event::TestCaseStderr {
            number: 1,
            stderr: "first",
        });
        reporter.report(Event::TestCaseStderr {
            number: 1,
            stderr: "second",
        });

        let test = reporter.current_test.as_ref().unwrap();
        assert_eq!(test.cases.len(), 1);
        assert_eq!(test.cases[0].stderr.as_deref(), Some("first"));
    }

    #[test]
    fn new_boundaries_reset_incomplete_or_previous_state() {
        let mut reporter = TerminalReporter::default();
        start(&mut reporter, "A", 2);
        reporter.report(Event::TestCaseAccepted {
            number: 1,
            elapsed: ELAPSED,
        });

        start(&mut reporter, "B", 1);
        reporter.report(Event::TestCaseAccepted {
            number: 1,
            elapsed: ELAPSED,
        });
        let output = reporter.finish_test_result("B", 1, 1).unwrap();
        assert_eq!(output.matches("sample-1").count(), 1);

        start(&mut reporter, "C", 1);
        reporter.report(Event::NoSamples { problem_index: "D" });
        assert!(reporter.current_test.is_none());

        start(&mut reporter, "E", 1);
        reporter.report(Event::CompileFailed { stderr: "error" });
        assert!(reporter.current_test.is_none());

        start(&mut reporter, "F", 1);
        reporter.report(Event::CompileTimedOut { elapsed: ELAPSED });
        assert!(reporter.current_test.is_none());
    }

    #[test]
    fn print_section_preserves_content_and_supplies_only_a_missing_final_newline() {
        assert_eq!(print_section(""), "<empty>\n");
        assert_eq!(print_section("  text  "), "  text  \n");
        assert_eq!(print_section("line  \n\n"), "line  \n\n");
        assert_eq!(print_section(" \t\n"), " \t\n");
    }
}
