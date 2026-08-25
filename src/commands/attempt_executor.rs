use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use crate::attempt::{AttemptCancellation, AttemptOutcome, run_attempt};
use crate::config::RunnerConfig;
use crate::error::AppError;
use crate::model::Problem;
use crate::tui::message::{Message, RunId, RunKind, RunRequest};
use crate::tui::reporter::ChannelReporter;

use super::stress::build_request as build_stress_request;
use super::test::test_problem_with_cancel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AttemptCompletion {
    pub(super) run_id: RunId,
    pub(super) problem: usize,
}

pub(super) struct AttemptExecutor {
    destination: Arc<PathBuf>,
    contest_id: Arc<String>,
    problems: Arc<Vec<Problem>>,
    runner_config: Arc<RunnerConfig>,
    message_tx: Sender<Message>,
}

impl AttemptExecutor {
    pub(super) fn new(
        destination: PathBuf,
        contest_id: String,
        problems: Vec<Problem>,
        runner_config: RunnerConfig,
        message_tx: Sender<Message>,
    ) -> Self {
        Self {
            destination: Arc::new(destination),
            contest_id: Arc::new(contest_id),
            problems: Arc::new(problems),
            runner_config: Arc::new(runner_config),
            message_tx,
        }
    }

    pub(super) fn spawn(
        &self,
        request: RunRequest,
        completion_tx: Sender<AttemptCompletion>,
    ) -> io::Result<ActiveAttempt> {
        let destination = Arc::clone(&self.destination);
        let contest_id = Arc::clone(&self.contest_id);
        let problems = Arc::clone(&self.problems);
        let runner_config = Arc::clone(&self.runner_config);
        let message_tx = self.message_tx.clone();

        spawn_with(request, completion_tx, move |cancellation| {
            run_reported_attempt(
                request,
                message_tx,
                &cancellation,
                |reporter, is_cancelled| {
                    let problem = problems.get(request.problem).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid problem index: {}", request.problem),
                        )
                    })?;

                    match request.kind {
                        RunKind::Samples => test_problem_with_cancel(
                            &destination,
                            contest_id.as_str(),
                            problem,
                            request.language,
                            &runner_config,
                            request.debug,
                            reporter,
                            is_cancelled,
                        ),
                        RunKind::Stress { base_seed, count } => {
                            let stress_request = build_stress_request(
                                &destination,
                                contest_id.as_str(),
                                problem,
                                request.language,
                                &runner_config,
                                request.debug,
                                base_seed,
                                count,
                            )?;
                            match crate::stress::run(&stress_request, reporter, is_cancelled)? {
                                crate::stress::StressOutcome::Cancelled { .. } => {
                                    Err(crate::attempt::clean_cancellation_io_error().into())
                                }
                                crate::stress::StressOutcome::Completed { .. }
                                | crate::stress::StressOutcome::Failed { .. } => Ok(()),
                            }
                        }
                    }
                },
            )
        })
    }
}

pub(super) struct ActiveAttempt {
    request: RunRequest,
    cancellation: Arc<AttemptCancellation>,
    handle: Option<JoinHandle<AttemptOutcome>>,
}

impl ActiveAttempt {
    pub(super) fn request(&self) -> RunRequest {
        self.request
    }

    pub(super) fn request_cancel(&self) {
        self.cancellation.request();
    }

    pub(super) fn join(mut self) -> io::Result<AttemptOutcome> {
        self.join_inner()
    }

    fn join_inner(&mut self) -> io::Result<AttemptOutcome> {
        let Some(handle) = self.handle.take() else {
            return Err(io::Error::other("physical attempt was already joined"));
        };

        // After this join returns, the attempt thread has finished and can never send another
        // RunEvent. S4-B may emit RunRequeued or start the same run_id only beyond this boundary.
        handle
            .join()
            .map_err(|_| io::Error::other("physical attempt thread panicked"))
    }
}

impl Drop for ActiveAttempt {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.request_cancel();
            let _ = self.join_inner();
        }
    }
}

pub(super) fn spawn_with(
    request: RunRequest,
    completion_tx: Sender<AttemptCompletion>,
    run: impl FnOnce(Arc<AttemptCancellation>) -> AttemptOutcome + Send + 'static,
) -> io::Result<ActiveAttempt> {
    let cancellation = Arc::new(AttemptCancellation::new());
    let thread_cancellation = Arc::clone(&cancellation);
    let completion = AttemptCompletion {
        run_id: request.run_id,
        problem: request.problem,
    };

    let handle = thread::Builder::new()
        .name(format!("atc-watch-attempt-{}", request.run_id))
        .spawn(move || {
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(thread_cancellation)));

            // The notification only says that an outcome is ready. JoinHandle remains the
            // authoritative owner of that outcome and joining establishes the no-late-event
            // boundary. It is sent for panics too, so the scheduler can always follow the same
            // completion -> identity check -> join ordering. A disconnected receiver must not
            // replace the attempt's outcome or panic payload.
            let _ = completion_tx.send(completion);

            match outcome {
                Ok(outcome) => outcome,
                Err(payload) => std::panic::resume_unwind(payload),
            }
        })?;

    Ok(ActiveAttempt {
        request,
        cancellation,
        handle: Some(handle),
    })
}

fn run_reported_attempt(
    request: RunRequest,
    message_tx: Sender<Message>,
    cancellation: &AttemptCancellation,
    run: impl FnOnce(&mut ChannelReporter, &dyn Fn() -> bool) -> Result<(), AppError>,
) -> AttemptOutcome {
    run_attempt(cancellation, |is_cancelled| {
        message_tx
            .send(Message::RunStarted {
                run_id: request.run_id,
                problem: request.problem,
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TUI message receiver disconnected before attempt start",
                )
            })?;

        let mut reporter =
            ChannelReporter::new(request.run_id, request.problem, message_tx.clone());
        let run_result = run(&mut reporter, is_cancelled);
        let reporter_result = reporter.finish().map_err(AppError::from);

        combine_run_and_reporter_results(run_result, reporter_result)
    })
}

fn combine_run_and_reporter_results(
    run_result: Result<(), AppError>,
    reporter_result: Result<(), AppError>,
) -> Result<(), AppError> {
    match (run_result, reporter_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        // Neither failure is discarded. Combining them into an unmarked infrastructure error is
        // also intentional: even a clean run cancellation is Failed if reporter cleanup failed.
        (Err(run_error), Err(reporter_error)) => Err(io::Error::other(format!(
            "run attempt failed: {run_error}; reporter also failed: {reporter_error}"
        ))
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attempt::{clean_cancellation_io_error, io_error_is_clean_cancellation};
    use crate::language::Language;
    use crate::runner::{self, ExecutionCheckpoint};
    use crate::tui::message::TestEvent;
    use crate::ui::{Event, Reporter};
    use std::ffi::OsString;
    use std::sync::{Mutex, mpsc};
    use std::time::{Duration, Instant};

    fn request() -> RunRequest {
        RunRequest {
            run_id: 7,
            problem: 0,
            language: Language::Python,
            debug: false,
            kind: RunKind::Samples,
        }
    }

    fn problem() -> Problem {
        Problem {
            index: "A".to_string(),
            title: "Problem A".to_string(),
            task_id: "abc123_a".to_string(),
            url: "https://example.invalid/a".to_string(),
            sample_count: 0,
        }
    }

    fn executor_with_no_samples(
        message_tx: Sender<Message>,
    ) -> (tempfile::TempDir, AttemptExecutor) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("A.py"), "pass\n").unwrap();
        let executor = AttemptExecutor::new(
            temp.path().to_path_buf(),
            "abc123".to_string(),
            vec![problem()],
            RunnerConfig::default(),
            message_tx,
        );
        (temp, executor)
    }

    #[test]
    fn normal_attempt_orders_started_before_events_then_completes_and_joins() {
        let (message_tx, message_rx) = mpsc::channel();
        let (_temp, executor) = executor_with_no_samples(message_tx);
        let (completion_tx, completion_rx) = mpsc::channel();

        let active = executor.spawn(request(), completion_tx).unwrap();
        assert_eq!(active.request(), request());
        assert_eq!(
            completion_rx.recv().unwrap(),
            AttemptCompletion {
                run_id: 7,
                problem: 0,
            }
        );
        assert!(matches!(active.join().unwrap(), AttemptOutcome::Completed));

        let messages: Vec<_> = message_rx.try_iter().collect();
        assert!(matches!(
            messages.as_slice(),
            [
                Message::RunStarted {
                    run_id: 7,
                    problem: 0
                },
                Message::RunEvent {
                    run_id: 7,
                    problem: 0,
                    event: TestEvent::NoSamples
                }
            ]
        ));
        assert!(message_rx.try_recv().is_err());
    }

    #[test]
    fn external_repeated_cancellation_is_observed_and_returns_cancelled() {
        let (completion_tx, completion_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();

        let active = spawn_with(request(), completion_tx, move |cancellation| {
            run_attempt(&cancellation, |is_cancelled| {
                ready_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                assert!(is_cancelled());
                Err(AppError::from(clean_cancellation_io_error()))
            })
        })
        .unwrap();

        ready_rx.recv().unwrap();
        active.request_cancel();
        active.request_cancel();
        active.request_cancel();
        continue_tx.send(()).unwrap();
        assert_eq!(completion_rx.recv().unwrap().run_id, 7);
        assert!(matches!(active.join().unwrap(), AttemptOutcome::Cancelled));
    }

    #[test]
    fn requested_but_unobserved_cancel_does_not_override_natural_completion() {
        let (completion_tx, completion_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();

        let active = spawn_with(request(), completion_tx, move |cancellation| {
            run_attempt(&cancellation, |_| {
                ready_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                Ok(())
            })
        })
        .unwrap();

        ready_rx.recv().unwrap();
        active.request_cancel();
        continue_tx.send(()).unwrap();
        completion_rx.recv().unwrap();
        assert!(matches!(active.join().unwrap(), AttemptOutcome::Completed));
    }

    #[test]
    fn reporter_disconnect_after_started_is_an_attempt_failure() {
        let (message_tx, message_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();

        let active = spawn_with(request(), completion_tx, move |cancellation| {
            run_reported_attempt(request(), message_tx, &cancellation, move |reporter, _| {
                ready_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                reporter.report(Event::NoSamples { problem_index: "A" });
                Ok(())
            })
        })
        .unwrap();

        ready_rx.recv().unwrap();
        assert!(matches!(
            message_rx.recv().unwrap(),
            Message::RunStarted { .. }
        ));
        drop(message_rx);
        continue_tx.send(()).unwrap();
        completion_rx.recv().unwrap();

        assert!(matches!(
            active.join().unwrap(),
            AttemptOutcome::Failed(AppError::Io(ref error))
                if error.kind() == io::ErrorKind::BrokenPipe
        ));
    }

    #[test]
    fn simultaneous_test_and_reporter_errors_are_both_retained_and_failed() {
        let (message_tx, message_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let (completion_tx, completion_rx) = mpsc::channel();

        let active = spawn_with(request(), completion_tx, move |cancellation| {
            run_reported_attempt(request(), message_tx, &cancellation, move |reporter, _| {
                ready_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                reporter.report(Event::NoSamples { problem_index: "A" });
                Err(io::Error::new(io::ErrorKind::NotFound, "test input missing").into())
            })
        })
        .unwrap();

        ready_rx.recv().unwrap();
        message_rx.recv().unwrap();
        drop(message_rx);
        continue_tx.send(()).unwrap();
        completion_rx.recv().unwrap();

        assert!(matches!(
            active.join().unwrap(),
            AttemptOutcome::Failed(AppError::Io(ref error))
                if error.to_string().contains("test input missing")
                    && error.to_string().contains("reporter also failed")
        ));
    }

    #[test]
    fn completion_receiver_disconnect_does_not_prevent_joining_outcome() {
        let (completion_tx, completion_rx) = mpsc::channel();
        drop(completion_rx);

        let active = spawn_with(request(), completion_tx, |cancellation| {
            run_attempt(&cancellation, |_| Ok(()))
        })
        .unwrap();

        assert!(matches!(active.join().unwrap(), AttemptOutcome::Completed));
    }

    #[test]
    fn dropping_without_explicit_join_requests_cancel_and_waits_for_thread() {
        let (completion_tx, _completion_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();

        let active = spawn_with(request(), completion_tx, move |cancellation| {
            ready_tx.send(()).unwrap();
            while !cancellation.is_requested() {
                thread::yield_now();
            }
            let outcome = run_attempt(&cancellation, |is_cancelled| {
                assert!(is_cancelled());
                Err(AppError::from(clean_cancellation_io_error()))
            });
            finished_tx.send(()).unwrap();
            outcome
        })
        .unwrap();

        ready_rx.recv().unwrap();
        drop(active);
        finished_rx.recv().unwrap();
    }

    #[test]
    fn thread_panic_is_reported_by_join_instead_of_completed() {
        let (completion_tx, completion_rx) = mpsc::channel();
        let active = spawn_with(request(), completion_tx, |_| panic!("attempt panic")).unwrap();

        assert_eq!(completion_rx.recv().unwrap().run_id, request().run_id);

        let error = active.join().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("panicked"));
    }

    #[test]
    fn clean_cancellation_plus_reporter_failure_is_failed() {
        let test_error = AppError::from(clean_cancellation_io_error());
        let reporter_error = AppError::from(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "reporter disconnected",
        ));

        let error =
            combine_run_and_reporter_results(Err(test_error), Err(reporter_error)).unwrap_err();

        assert!(matches!(error, AppError::Io(ref error) if
            !io_error_is_clean_cancellation(error)
                && error.to_string().contains("reporter disconnected")));
    }

    #[test]
    fn attempt_types_do_not_encode_scheduler_policy() {
        fn assert_send<T: Send>() {}

        assert_send::<ActiveAttempt>();
        assert_send::<AttemptCompletion>();
    }

    #[derive(Debug)]
    struct CancelMeasurement {
        observe: Duration,
        reap: Duration,
        readers: Duration,
        join: Duration,
    }

    fn runner_helper_args(name: &str) -> Vec<OsString> {
        vec![
            OsString::from("--exact"),
            OsString::from(format!("runner::tests::{name}")),
            OsString::from("--ignored"),
            OsString::from("--nocapture"),
        ]
    }

    fn cancel_continuous_output_attempt(
        helper: &'static str,
        output_time: Duration,
    ) -> CancelMeasurement {
        let (completion_tx, completion_rx) = mpsc::channel();
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let checkpoints = Arc::new(Mutex::new(Vec::new()));
        let attempt_checkpoints = Arc::clone(&checkpoints);

        let active = spawn_with(request(), completion_tx, move |cancellation| {
            run_attempt(&cancellation, |is_cancelled| {
                runner::execute_with_cancel_observer(
                    &std::env::current_exe().unwrap(),
                    &runner_helper_args(helper),
                    "",
                    Duration::from_secs(30),
                    is_cancelled,
                    &|checkpoint| {
                        attempt_checkpoints
                            .lock()
                            .unwrap()
                            .push((checkpoint, Instant::now()));
                        if checkpoint == ExecutionCheckpoint::ChildSpawned {
                            let _ = spawned_tx.send(());
                        }
                    },
                )
                .map(|_| ())
                .map_err(AppError::from)
            })
        })
        .unwrap();

        spawned_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread::sleep(output_time);
        let cancel_requested = Instant::now();
        active.request_cancel();
        completion_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let outcome = active.join().unwrap();
        let joined = Instant::now();
        assert!(matches!(outcome, AttemptOutcome::Cancelled));

        let checkpoints = checkpoints.lock().unwrap();
        let at = |target| {
            checkpoints
                .iter()
                .find_map(|(checkpoint, at)| (*checkpoint == target).then_some(*at))
                .unwrap()
        };
        let observed = at(ExecutionCheckpoint::CancelObserved);
        let reaped = at(ExecutionCheckpoint::ChildReaped);
        let readers = at(ExecutionCheckpoint::PipeThreadsJoined);
        assert!(cancel_requested <= observed);
        assert!(observed <= reaped);
        assert!(reaped <= readers);
        assert!(readers <= joined);

        CancelMeasurement {
            observe: observed.duration_since(cancel_requested),
            reap: reaped.duration_since(cancel_requested),
            readers: readers.duration_since(cancel_requested),
            join: joined.duration_since(cancel_requested),
        }
    }

    #[test]
    fn continuous_stdout_and_stderr_cancel_and_join_without_hanging() {
        for helper in [
            "continuous_stdout_helper",
            "continuous_stderr_helper",
            "continuous_stdout_stderr_helper",
        ] {
            let measurement = cancel_continuous_output_attempt(helper, Duration::from_millis(20));
            assert!(measurement.join < Duration::from_secs(5));
        }
    }

    #[test]
    #[ignore = "manual cancellation latency measurement"]
    fn measure_continuous_output_cancel_to_attempt_join() {
        for helper in [
            "continuous_stdout_helper",
            "continuous_stderr_helper",
            "continuous_stdout_stderr_helper",
        ] {
            for iteration in 1..=5 {
                let measurement =
                    cancel_continuous_output_attempt(helper, Duration::from_millis(50));
                eprintln!(
                    "{helper} #{iteration}: observe={:?}, reap={:?}, readers={:?}, join={:?}",
                    measurement.observe, measurement.reap, measurement.readers, measurement.join
                );
            }
        }
    }
}
