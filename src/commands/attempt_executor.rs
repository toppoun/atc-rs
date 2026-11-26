use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use crate::attempt::{AttemptCancellation, AttemptOutcome, run_attempt};
use crate::config::RunnerConfig;
use crate::error::AppError;
use crate::model::{Contest, Problem};
#[cfg(test)]
use crate::tui::message::RunScope;
use crate::tui::message::{
    Message, RunGeneration, RunId, RunKind, RunRequest, RunResults, RunTarget,
};
use crate::tui::reporter::ChannelReporter;
use crate::workspace::{self, ContestTaskSnapshot, ExplicitContestRoute};

use super::stress::build_request as build_stress_request;
use super::test::test_problem_with_cancel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AttemptCompletion {
    pub(super) run_id: RunId,
    pub(super) target: RunTarget,
}

struct RunBinding {
    metadata: ContestTaskSnapshot,
    route: Option<(PathBuf, ExplicitContestRoute)>,
}

pub(super) struct RunContext {
    destination: PathBuf,
    pub(super) generation: RunGeneration,
    problems: Vec<Problem>,
    runner_config: RunnerConfig,
    binding: Option<RunBinding>,
    stale: AtomicBool,
    workspace_watch: bool,
}

impl RunContext {
    pub(super) fn new(
        destination: PathBuf,
        contest: &Contest,
        runner_config: RunnerConfig,
        workspace_root: Option<&Path>,
    ) -> io::Result<Self> {
        let metadata = ContestTaskSnapshot::capture_contest(&destination, contest)?;
        let route = workspace_root
            .map(|root| {
                let route = ExplicitContestRoute::resolve(root, &contest.contest_id)?
                    .require_workspace()?;
                if workspace::contest_directory_identity(&destination)?
                    != workspace::contest_directory_identity(&route.destination)?
                {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "contest is outside its Workspace route",
                    ));
                }
                Ok((root.to_path_buf(), route))
            })
            .transpose()?;
        let context = Self {
            destination,
            generation: RunGeneration::new(&contest.contest_id),
            problems: contest.problems.clone(),
            runner_config,
            binding: Some(RunBinding { metadata, route }),
            stale: AtomicBool::new(false),
            workspace_watch: false,
        };
        context.revalidate()?;
        Ok(context)
    }

    pub(super) fn from_watch_route(
        root: &Path,
        route: ExplicitContestRoute,
        contest: &Contest,
        runner_config: RunnerConfig,
    ) -> io::Result<Self> {
        let destination = route.destination.clone();
        if &workspace::contest_directory_identity(&destination)? != route.destination_identity() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "contest changed during Workspace discovery",
            ));
        }
        let metadata = ContestTaskSnapshot::capture_contest(&destination, contest)?;
        Ok(Self {
            destination,
            generation: RunGeneration::new(&contest.contest_id),
            problems: contest.problems.clone(),
            runner_config,
            binding: Some(RunBinding {
                metadata,
                route: Some((root.to_path_buf(), route)),
            }),
            stale: AtomicBool::new(false),
            workspace_watch: true,
        })
    }

    pub(super) fn matches_watch_route(&self, route: &ExplicitContestRoute) -> bool {
        !self.is_stale()
            && self.binding.as_ref().is_some_and(|binding| {
                binding
                    .route
                    .as_ref()
                    .is_some_and(|(_, current)| current.same_owner(route))
                    && binding.metadata.revalidate(&self.destination).is_ok()
            })
    }

    pub(super) fn matches_view(&self, destination: &Path, contest: &Contest) -> io::Result<bool> {
        Ok(self.generation.scope().contest_id() == contest.contest_id
            && self.problems == contest.problems
            && workspace::contest_directory_identity(destination)?
                == workspace::contest_directory_identity(&self.destination)?)
    }

    #[cfg(test)]
    pub(super) fn fixture(
        destination: PathBuf,
        scope: RunScope,
        problems: Vec<Problem>,
        runner_config: RunnerConfig,
    ) -> Self {
        Self {
            destination,
            generation: RunGeneration::fixture(scope),
            problems,
            runner_config,
            binding: None,
            stale: AtomicBool::new(false),
            workspace_watch: false,
        }
    }

    pub(super) fn revalidate(&self) -> io::Result<()> {
        if let Some(binding) = &self.binding {
            if let Some((root, route)) = &binding.route {
                route.revalidate_workspace_owner(root, self.generation.scope().contest_id())?;
            }
            binding.metadata.revalidate(&self.destination)?;
        }
        Ok(())
    }

    pub(super) fn is_stale(&self) -> bool {
        self.stale.load(Ordering::Acquire)
    }

    pub(super) fn is_workspace_watch(&self) -> bool {
        self.workspace_watch
    }

    pub(super) fn source_stamp(
        &self,
        path: &Path,
    ) -> io::Result<Option<crate::safe_file::WatchedFileStamp>> {
        workspace::watched_source_stamp(path, self.watched_directory_identity())
    }

    pub(super) fn watched_source_exists(&self, path: &Path) -> io::Result<bool> {
        workspace::watched_source_exists(path, self.watched_directory_identity())
    }

    fn watched_directory_identity(&self) -> &workspace::PhysicalDirectoryIdentity {
        self.binding
            .as_ref()
            .and_then(|binding| binding.route.as_ref())
            .map(|(_, route)| route)
            .expect("Workspace watch source has a validated route")
            .destination_identity()
    }

    pub(super) fn sample_count(&self, task_id: &str) -> Option<usize> {
        #[cfg(test)]
        if self.binding.is_none() && self.problems.is_empty() {
            return Some(0);
        }
        self.problem(task_id).map(|problem| problem.sample_count)
    }

    fn problem(&self, task_id: &str) -> Option<&Problem> {
        self.problems
            .iter()
            .find(|problem| problem.task_id == task_id)
    }
}

pub(super) struct AttemptExecutor;

impl AttemptExecutor {
    pub(super) fn spawn(
        context: Arc<RunContext>,
        results: RunResults,
        request: RunRequest,
        completion_tx: Sender<AttemptCompletion>,
    ) -> io::Result<ActiveAttempt> {
        spawn_with(request.clone(), completion_tx, move |cancellation| {
            run_reported_attempt(
                request.clone(),
                results.clone(),
                &cancellation,
                |reporter, is_cancelled| {
                    if &request.target.scope != context.generation.scope() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "run request does not match the registered contest generation",
                        )
                        .into());
                    }
                    let problem = context.problem(&request.target.task_id).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown task ID: {}", request.target.task_id),
                        )
                    })?;
                    if let Err(error) = context.revalidate() {
                        context.stale.store(true, Ordering::Release);
                        return Err(error.into());
                    }
                    if context.workspace_watch {
                        let source = workspace::source_file_path(
                            &context.destination,
                            &problem.index,
                            request.language,
                        )?;
                        if context.source_stamp(&source)?.is_none() {
                            return Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                format!("source file not found: {}", source.display()),
                            )
                            .into());
                        }
                    }

                    match &request.kind {
                        RunKind::UserInput(snapshot) => {
                            if snapshot.problem_index != problem.index {
                                return Err(io::Error::other(
                                    "User Input problem identity mismatch",
                                )
                                .into());
                            }
                            super::user_input_run::run(
                                &context.destination,
                                problem,
                                &request,
                                &context.runner_config,
                                &results,
                                is_cancelled,
                            )
                        }
                        RunKind::Samples => test_problem_with_cancel(
                            &context.destination,
                            context.generation.scope().contest_id(),
                            problem,
                            request.language,
                            &context.runner_config,
                            request.debug,
                            reporter,
                            is_cancelled,
                        ),
                        RunKind::Stress { base_seed, count } => {
                            let stress_request = build_stress_request(
                                &context.destination,
                                context.generation.scope().contest_id(),
                                problem,
                                request.language,
                                &context.runner_config,
                                request.debug,
                                *base_seed,
                                *count,
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
        self.request.clone()
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
        target: request.target.clone(),
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
    message_tx: impl Into<RunResults>,
    cancellation: &AttemptCancellation,
    run: impl FnOnce(&mut ChannelReporter, &dyn Fn() -> bool) -> Result<(), AppError>,
) -> AttemptOutcome {
    let message_tx = message_tx.into();
    run_attempt(cancellation, |is_cancelled| {
        message_tx
            .send(Message::RunStarted {
                run_id: request.run_id,
                target: request.target.clone(),
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "TUI message receiver disconnected before attempt start",
                )
            })?;

        let mut reporter = ChannelReporter::new(request.run_id, request.target, message_tx.clone());
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
pub(super) struct TestAttemptExecutor {
    context: Arc<RunContext>,
    results: RunResults,
}

#[cfg(test)]
impl TestAttemptExecutor {
    pub(super) fn new(
        destination: PathBuf,
        scope: RunScope,
        problems: Vec<Problem>,
        runner_config: RunnerConfig,
        results: Sender<Message>,
    ) -> Self {
        Self {
            context: Arc::new(RunContext::fixture(
                destination,
                scope,
                problems,
                runner_config,
            )),
            results: results.into(),
        }
    }

    pub(super) fn spawn(
        &self,
        request: RunRequest,
        completion: Sender<AttemptCompletion>,
    ) -> io::Result<ActiveAttempt> {
        AttemptExecutor::spawn(
            self.context.clone(),
            self.results.clone(),
            request,
            completion,
        )
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
            target: RunScope::fixture().target("abc123_a"),
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
            url: "https://atcoder.jp/contests/abc123/tasks/abc123_a".to_string(),
            sample_count: 0,
        }
    }

    fn executor_with_no_samples(
        message_tx: Sender<Message>,
    ) -> (tempfile::TempDir, TestAttemptExecutor) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("A.py"), "pass\n").unwrap();
        crate::workspace::save_metadata(
            temp.path(),
            &crate::model::Contest {
                contest_id: "abc123".into(),
                problems: vec![problem()],
            },
        )
        .unwrap();
        let executor = TestAttemptExecutor::new(
            temp.path().to_path_buf(),
            RunScope::fixture(),
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
                target: request().target,
            }
        );
        assert!(matches!(active.join().unwrap(), AttemptOutcome::Completed));

        let messages: Vec<_> = message_rx.try_iter().collect();
        assert!(matches!(
            messages.as_slice(),
            [
                Message::RunStarted {
                    run_id: 7,
                    target: started,
                },
                Message::RunEvent {
                    run_id: 7,
                    target: reported,
                    event: TestEvent::NoSamples
                }
            ] if started == &request().target && reported == started
        ));
        assert!(message_rx.try_recv().is_err());
    }

    #[test]
    fn reordered_metadata_still_runs_the_source_for_the_requested_task() {
        let (message_tx, message_rx) = mpsc::channel();
        let (temp, _) = executor_with_no_samples(message_tx.clone());
        let contest = crate::model::Contest {
            contest_id: "abc123".into(),
            problems: vec![
                Problem {
                    index: "B".into(),
                    task_id: "abc123_b".into(),
                    url: "https://atcoder.jp/contests/abc123/tasks/abc123_b".into(),
                    ..problem()
                },
                problem(),
            ],
        };
        crate::workspace::save_metadata(temp.path(), &contest).unwrap();
        let executor = TestAttemptExecutor::new(
            temp.path().to_path_buf(),
            RunScope::fixture(),
            contest.problems,
            RunnerConfig::default(),
            message_tx,
        );
        let (completion_tx, _completion_rx) = mpsc::channel();
        let requested = request();
        let outcome = executor
            .spawn(requested.clone(), completion_tx)
            .unwrap()
            .join()
            .unwrap();

        // Only A.py exists, even though A is the second item in the metadata.
        assert!(matches!(outcome, AttemptOutcome::Completed));
        assert!(matches!(
            message_rx.try_iter().collect::<Vec<_>>().as_slice(),
            [Message::RunStarted { .. }, Message::RunEvent { target, event: TestEvent::NoSamples, .. }]
                if target == &requested.target
        ));
    }

    #[test]
    fn unknown_task_and_other_session_are_rejected_before_source_execution() {
        let (message_tx, message_rx) = mpsc::channel();
        let (_temp, executor) = executor_with_no_samples(message_tx);
        for (target, reason) in [
            (
                RunScope::fixture().target("abc123_missing"),
                "unknown task ID",
            ),
            (RunScope::new("abc123").target("abc123_a"), "does not match"),
            (RunScope::new("abc456").target("abc123_a"), "does not match"),
        ] {
            let (completion_tx, completion_rx) = mpsc::channel();
            let requested = RunRequest {
                target,
                ..request()
            };
            let outcome = executor
                .spawn(requested.clone(), completion_tx)
                .unwrap()
                .join()
                .unwrap();
            assert!(
                matches!(outcome, AttemptOutcome::Failed(error) if error.to_string().contains(reason))
            );
            let completion = completion_rx.recv().unwrap();
            assert!(requested.matches(&completion.target, completion.run_id));
            assert!(matches!(
                message_rx.try_iter().collect::<Vec<_>>().as_slice(),
                [Message::RunStarted { target, .. }] if target == &requested.target
            ));
        }
    }

    #[test]
    fn tui_sample_attempt_rechecks_contest_recovery() {
        let (message_tx, message_rx) = mpsc::channel();
        let (temp, executor) = executor_with_no_samples(message_tx);
        let staging = temp.path().join(".atc-repair-interrupted");
        std::fs::create_dir(&staging).unwrap();
        crate::safe_file::write_recovery_intent(
            &staging,
            &crate::safe_file::RecoveryIdentity::new("contest", "abc123", None),
        )
        .unwrap();
        let (completion_tx, _completion_rx) = mpsc::channel();

        let outcome = executor
            .spawn(request(), completion_tx)
            .unwrap()
            .join()
            .unwrap();

        assert!(
            matches!(outcome, AttemptOutcome::Failed(error) if error.to_string().contains("requires recovery check"))
        );
        assert!(matches!(
            message_rx.try_iter().collect::<Vec<_>>().as_slice(),
            [Message::RunStarted { .. }]
        ));
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
