use std::io;
use std::sync::mpsc::Sender;

use crate::ui::{Event, Reporter};

use super::message::{Message, RunId, StressEvent, TestEvent};

pub struct ChannelReporter {
    run_id: RunId,
    problem: usize,
    tx: Sender<Message>,
    send_error: Option<io::Error>,
}

impl ChannelReporter {
    pub fn new(run_id: RunId, problem: usize, tx: Sender<Message>) -> Self {
        Self {
            run_id,
            problem,
            tx,
            send_error: None,
        }
    }

    fn send_message(&mut self, message: Message) {
        if self.send_error.is_some() {
            return;
        }

        if self.tx.send(message).is_err() {
            self.send_error = Some(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TUI message receiver disconnected",
            ));
        }
    }

    fn send(&mut self, event: TestEvent) {
        self.send_message(Message::RunEvent {
            run_id: self.run_id,
            problem: self.problem,
            event,
        });
    }

    fn send_stress(&mut self, event: StressEvent) {
        self.send_message(Message::StressEvent {
            run_id: self.run_id,
            problem: self.problem,
            event,
        });
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        match self.send_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Reporter for ChannelReporter {
    fn report(&mut self, event: Event<'_>) {
        match event {
            Event::NoSamples { .. } => {
                self.send(TestEvent::NoSamples);
            }

            Event::CompileFailed { stderr } => {
                self.send(TestEvent::CompileFailed {
                    stderr: stderr.to_owned(),
                });
            }

            Event::CompileTimedOut { elapsed } => {
                self.send(TestEvent::CompileTimedOut { elapsed });
            }

            Event::TestCaseLayout {
                sample_cases,
                stress_case,
                ..
            } => {
                self.send(TestEvent::TestCaseLayout {
                    sample_cases,
                    stress_case: stress_case.cloned(),
                });
            }

            Event::TestRunStarted { total_cases, .. } => {
                self.send(TestEvent::TestRunStarted { total_cases });
            }

            Event::TestCaseAccepted { number, elapsed } => {
                self.send(TestEvent::TestCaseAccepted { number, elapsed });
            }

            Event::TestCaseWrongAnswer { number, elapsed } => {
                self.send(TestEvent::TestCaseWrongAnswer { number, elapsed });
            }
            Event::TestCaseComparison {
                number,
                input,
                expected,
                actual,
            } => {
                self.send(TestEvent::TestCaseComparison {
                    number,
                    input: input.to_owned(),
                    expected: expected.to_owned(),
                    actual: actual.to_owned(),
                });
            }

            Event::TestCaseRuntimeError { number, elapsed } => {
                self.send(TestEvent::TestCaseRuntimeError { number, elapsed });
            }

            Event::TestCaseTimedOut { number, elapsed } => {
                self.send(TestEvent::TestCaseTimedOut { number, elapsed });
            }

            Event::TestCaseStderr { number, stderr } => {
                self.send(TestEvent::TestCaseStderr {
                    number,
                    stderr: stderr.to_owned(),
                });
            }

            Event::TestRunFinished {
                accepted,
                total_cases,
                ..
            } => {
                self.send(TestEvent::TestRunFinished {
                    accepted,
                    total_cases,
                });
            }

            Event::StressStarted {
                base_seed,
                case_limit,
                ..
            } => {
                self.send_stress(StressEvent::Started {
                    base_seed,
                    case_limit,
                });
            }

            Event::StressProgress {
                case_number,
                seed,
                passed,
                elapsed,
                cases_per_second,
                ..
            } => {
                self.send_stress(StressEvent::Progress {
                    case_number,
                    seed,
                    passed,
                    elapsed,
                    cases_per_second,
                });
            }

            Event::StressFailed {
                failure,
                saved_to,
                elapsed,
                ..
            } => {
                self.send_stress(StressEvent::Failed {
                    kind: failure.kind,
                    case_number: failure.case_number,
                    base_seed: failure.base_seed,
                    seed: failure.seed,
                    input: failure.input.clone(),
                    expected: failure.expected.clone(),
                    actual: failure.actual.clone(),
                    stderr: failure.stderr.clone(),
                    candidate_elapsed: failure.elapsed,
                    elapsed,
                    saved_to: saved_to.to_path_buf(),
                });
            }

            Event::StressFinished { cases, elapsed, .. } => {
                self.send_stress(StressEvent::Finished { cases, elapsed });
            }

            Event::StressCancelled { cases, elapsed, .. } => {
                self.send_stress(StressEvent::Cancelled { cases, elapsed });
            }

            Event::DoctorReport { .. }
            | Event::ContestFetching { .. }
            | Event::ContestFetched { .. }
            | Event::ProblemFetching { .. }
            | Event::ProblemFetched { .. }
            | Event::ProblemFetchFailed { .. }
            | Event::WorkspaceCreated { .. }
            | Event::WorkspaceRefreshed { .. }
            | Event::WorkspaceRepaired { .. }
            | Event::WorkspaceInitialized { .. }
            | Event::WorkspaceAlreadyInitialized { .. }
            | Event::SourceCreated { .. }
            | Event::TemplateFileCreated { .. }
            | Event::TemplateFileExists { .. }
            | Event::ConfigFileCreated { .. }
            | Event::ConfigFileExists { .. }
            | Event::StressFileCreated { .. }
            | Event::StressFileExists { .. }
            | Event::StressFilesAlreadyInitialized { .. }
            | Event::WatchStarted { .. }
            | Event::WatchSourceChanged { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attempt::{AttemptCancellation, AttemptOutcome, run_attempt};
    use crate::error::AppError;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn converts_accepted_event_to_owned_run_message() {
        let (tx, rx) = mpsc::channel();

        let mut reporter = ChannelReporter::new(7, 2, tx);

        reporter.report(Event::TestCaseAccepted {
            number: 1,
            elapsed: Duration::from_millis(12),
        });

        let message = rx.recv().unwrap();

        match message {
            Message::RunEvent {
                run_id,
                problem,
                event: TestEvent::TestCaseAccepted { number, elapsed },
            } => {
                assert_eq!(run_id, 7);
                assert_eq!(problem, 2);
                assert_eq!(number, 1);
                assert_eq!(elapsed, Duration::from_millis(12));
            }

            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn ignores_cli_only_initialization_events() {
        let (tx, rx) = mpsc::channel();
        let mut reporter = ChannelReporter::new(1, 0, tx);
        let path = std::path::Path::new("templates/cpp.cpp");

        reporter.report(Event::TemplateFileCreated { path });
        reporter.report(Event::TemplateFileExists { path });
        reporter.report(Event::ConfigFileCreated { path });
        reporter.report(Event::ConfigFileExists { path });

        assert!(rx.try_recv().is_err());
        reporter.finish().unwrap();
    }

    #[test]
    fn owns_wrong_answer_text() {
        let (tx, rx) = mpsc::channel();

        let mut reporter = ChannelReporter::new(3, 0, tx);

        let input = String::from("1\n");
        let expected = String::from("Yes\n");
        let actual = String::from("No\n");

        reporter.report(Event::TestCaseWrongAnswer {
            number: 2,
            elapsed: Duration::from_millis(5),
        });
        reporter.report(Event::TestCaseComparison {
            number: 2,
            input: &input,
            expected: &expected,
            actual: &actual,
        });

        drop(input);
        drop(expected);
        drop(actual);

        let message = rx.recv().unwrap();

        match message {
            Message::RunEvent {
                event: TestEvent::TestCaseWrongAnswer { number, elapsed },
                ..
            } => {
                assert_eq!(number, 2);
                assert_eq!(elapsed, Duration::from_millis(5));
            }

            other => panic!("unexpected message: {other:?}"),
        }

        let message = rx.recv().unwrap();

        match message {
            Message::RunEvent {
                event:
                    TestEvent::TestCaseComparison {
                        number,
                        input,
                        expected,
                        actual,
                    },
                ..
            } => {
                assert_eq!(number, 2);
                assert_eq!(input, "1\n");
                assert_eq!(expected, "Yes\n");
                assert_eq!(actual, "No\n");
            }

            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn converts_stderr_event() {
        let (tx, rx) = mpsc::channel();

        let mut reporter = ChannelReporter::new(4, 1, tx);

        reporter.report(Event::TestCaseStderr {
            number: 1,
            stderr: "debug output\n",
        });

        match rx.recv().unwrap() {
            Message::RunEvent {
                run_id: 4,
                problem: 1,
                event: TestEvent::TestCaseStderr { number: 1, stderr },
            } => {
                assert_eq!(stderr, "debug output\n");
            }

            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn converts_run_boundaries_and_recoverable_terminal_events() {
        let (tx, rx) = mpsc::channel();
        let mut reporter = ChannelReporter::new(9, 1, tx);

        reporter.report(Event::TestRunStarted {
            problem_index: "B",
            total_cases: 2,
        });
        reporter.report(Event::TestRunFinished {
            problem_index: "B",
            accepted: 1,
            total_cases: 2,
        });
        reporter.report(Event::NoSamples { problem_index: "B" });
        reporter.report(Event::CompileFailed { stderr: "error" });
        reporter.report(Event::CompileTimedOut {
            elapsed: Duration::from_secs(3),
        });

        let messages: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            messages[0],
            Message::RunEvent {
                run_id: 9,
                problem: 1,
                event: TestEvent::TestRunStarted { total_cases: 2 },
            }
        ));
        assert!(matches!(
            messages[1],
            Message::RunEvent {
                run_id: 9,
                problem: 1,
                event: TestEvent::TestRunFinished {
                    accepted: 1,
                    total_cases: 2,
                },
            }
        ));
        assert!(matches!(
            messages[2],
            Message::RunEvent {
                event: TestEvent::NoSamples,
                ..
            }
        ));
        assert!(matches!(
            &messages[3],
            Message::RunEvent {
                event: TestEvent::CompileFailed { stderr },
                ..
            } if stderr == "error"
        ));
        assert!(matches!(
            messages[4],
            Message::RunEvent {
                event: TestEvent::CompileTimedOut { elapsed },
                ..
            } if elapsed == Duration::from_secs(3)
        ));
    }

    #[test]
    fn converts_stress_failure_to_owned_message() {
        let (tx, rx) = mpsc::channel();
        let mut reporter = ChannelReporter::new(11, 0, tx);
        let failure = crate::stress::StressFailure {
            kind: crate::stress::CandidateFailureKind::WrongAnswer,
            case_number: 14,
            base_seed: 100,
            seed: 113,
            input: "2\n1 2\n".to_string(),
            expected: "No\n".to_string(),
            actual: "Yes\n".to_string(),
            stderr: "debug\n".to_string(),
            elapsed: Duration::from_millis(7),
        };
        let saved_to = std::path::PathBuf::from(".atc/stress/A");

        reporter.report(Event::StressFailed {
            problem_index: "A",
            failure: &failure,
            saved_to: &saved_to,
            elapsed: Duration::from_millis(100),
        });

        drop(failure);

        match rx.recv().unwrap() {
            Message::StressEvent {
                run_id: 11,
                problem: 0,
                event:
                    StressEvent::Failed {
                        kind: crate::stress::CandidateFailureKind::WrongAnswer,
                        case_number: 14,
                        seed: 113,
                        input,
                        expected,
                        actual,
                        stderr,
                        saved_to,
                        ..
                    },
            } => {
                assert_eq!(input, "2\n1 2\n");
                assert_eq!(expected, "No\n");
                assert_eq!(actual, "Yes\n");
                assert_eq!(stderr, "debug\n");
                assert_eq!(saved_to, std::path::PathBuf::from(".atc/stress/A"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn disconnected_channel_can_be_normalized_as_an_attempt_failure() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let mut reporter = ChannelReporter::new(1, 0, tx);
        let cancellation = AttemptCancellation::new();

        let outcome = run_attempt(&cancellation, |_| {
            reporter.report(Event::NoSamples { problem_index: "A" });
            reporter.finish().map_err(AppError::from)
        });

        assert!(matches!(
            outcome,
            AttemptOutcome::Failed(AppError::Io(ref error))
                if error.kind() == io::ErrorKind::BrokenPipe
        ));
    }
}
