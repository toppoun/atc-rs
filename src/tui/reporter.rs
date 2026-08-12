use std::sync::mpsc::Sender;

use crate::ui::{Event, Reporter};

use super::message::{Message, RunId, TestEvent};

pub struct ChannelReporter {
    run_id: RunId,
    problem: usize,
    tx: Sender<Message>,
}

impl ChannelReporter {
    pub fn new(run_id: RunId, problem: usize, tx: Sender<Message>) -> Self {
        Self {
            run_id,
            problem,
            tx,
        }
    }

    fn send(&self, event: TestEvent) {
        let _ = self.tx.send(Message::RunEvent {
            run_id: self.run_id,
            problem: self.problem,
            event,
        });
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

            Event::TestRunStarted { total_cases, .. } => {
                self.send(TestEvent::TestRunStarted { total_cases });
            }

            Event::TestCaseAccepted { number, elapsed } => {
                self.send(TestEvent::TestCaseAccepted { number, elapsed });
            }

            Event::TestCaseWrongAnswer {
                number,
                expected,
                actual,
                elapsed,
            } => {
                self.send(TestEvent::TestCaseWrongAnswer {
                    number,
                    expected: expected.to_owned(),
                    actual: actual.to_owned(),
                    elapsed,
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

            Event::ContestFetching { .. }
            | Event::ContestFetched { .. }
            | Event::ProblemFetching { .. }
            | Event::ProblemFetched { .. }
            | Event::ProblemFetchFailed { .. }
            | Event::WorkspaceCreated { .. }
            | Event::WorkspaceRefreshed { .. }
            | Event::SourceCreated { .. }
            | Event::WatchStarted { .. }
            | Event::WatchSourceChanged { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn owns_wrong_answer_text() {
        let (tx, rx) = mpsc::channel();

        let mut reporter = ChannelReporter::new(3, 0, tx);

        let expected = String::from("Yes\n");
        let actual = String::from("No\n");

        reporter.report(Event::TestCaseWrongAnswer {
            number: 2,
            expected: &expected,
            actual: &actual,
            elapsed: Duration::from_millis(5),
        });

        drop(expected);
        drop(actual);

        let message = rx.recv().unwrap();

        match message {
            Message::RunEvent {
                event:
                    TestEvent::TestCaseWrongAnswer {
                        number,
                        expected,
                        actual,
                        ..
                    },
                ..
            } => {
                assert_eq!(number, 2);
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
}
