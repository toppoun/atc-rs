use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::AppError;

#[derive(Debug)]
pub(crate) enum AttemptOutcome {
    Completed,
    Cancelled,
    Failed(AppError),
}

#[derive(Debug, Default)]
pub(crate) struct AttemptCancellation {
    requested: AtomicBool,
    observed: AtomicBool,
}

impl AttemptCancellation {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    pub(crate) fn was_observed(&self) -> bool {
        self.observed.load(Ordering::Acquire)
    }

    fn poll(&self) -> bool {
        if !self.requested.load(Ordering::Acquire) {
            return false;
        }

        self.observed.store(true, Ordering::Release);
        true
    }
}

pub(crate) fn run_attempt(
    cancellation: &AttemptCancellation,
    run: impl FnOnce(&dyn Fn() -> bool) -> Result<(), AppError>,
) -> AttemptOutcome {
    let is_cancelled = || cancellation.poll();
    let result = run(&is_cancelled);

    match result {
        Ok(()) => AttemptOutcome::Completed,
        Err(error) if cancellation.was_observed() && app_error_is_clean_cancellation(&error) => {
            AttemptOutcome::Cancelled
        }
        Err(error) => AttemptOutcome::Failed(error),
    }
}

fn app_error_is_clean_cancellation(error: &AppError) -> bool {
    matches!(error, AppError::Io(error) if io_error_is_clean_cancellation(error))
}

#[derive(Debug)]
struct CleanCancellation;

impl fmt::Display for CleanCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("process execution cancelled")
    }
}

impl std::error::Error for CleanCancellation {}

pub(crate) fn clean_cancellation_io_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, CleanCancellation)
}

pub(crate) fn io_error_is_clean_cancellation(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<CleanCancellation>())
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn normal_return_is_completed() {
        let cancellation = AttemptCancellation::new();

        let outcome = run_attempt(&cancellation, |_| Ok(()));

        assert!(matches!(outcome, AttemptOutcome::Completed));
        assert!(!cancellation.is_requested());
        assert!(!cancellation.was_observed());
    }

    #[test]
    fn requested_but_unobserved_cancellation_does_not_override_natural_completion() {
        let cancellation = AttemptCancellation::new();
        cancellation.request();

        let outcome = run_attempt(&cancellation, |_| Ok(()));

        assert!(matches!(outcome, AttemptOutcome::Completed));
        assert!(cancellation.is_requested());
        assert!(!cancellation.was_observed());
    }

    #[test]
    fn observed_clean_runner_cancellation_is_cancelled() {
        let cancellation = AttemptCancellation::new();
        cancellation.request();

        let outcome = run_attempt(&cancellation, |is_cancelled| {
            runner::execute_with_cancel(
                Path::new("this-program-must-not-be-spawned"),
                &[],
                "",
                Duration::from_secs(1),
                is_cancelled,
            )
            .map(|_| ())
            .map_err(AppError::from)
        });

        assert!(matches!(outcome, AttemptOutcome::Cancelled));
        assert!(cancellation.was_observed());
    }

    #[test]
    fn ordinary_infrastructure_error_is_failed() {
        let cancellation = AttemptCancellation::new();

        let outcome = run_attempt(&cancellation, |_| {
            Err(io::Error::other("infrastructure failure").into())
        });

        assert!(matches!(
            outcome,
            AttemptOutcome::Failed(AppError::Io(ref error))
                if error.kind() == io::ErrorKind::Other
                    && error.to_string() == "infrastructure failure"
        ));
    }

    #[test]
    fn observed_but_unmarked_interrupted_error_is_failed() {
        let cancellation = AttemptCancellation::new();
        cancellation.request();

        let outcome = run_attempt(&cancellation, |is_cancelled| {
            assert!(is_cancelled());
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cleanup failed after cancellation",
            )
            .into())
        });

        assert!(matches!(
            outcome,
            AttemptOutcome::Failed(AppError::Io(ref error))
                if error.kind() == io::ErrorKind::Interrupted
        ));
    }

    #[test]
    fn clean_cancellation_marker_without_observation_is_failed() {
        let cancellation = AttemptCancellation::new();

        let outcome = run_attempt(&cancellation, |_| {
            Err(AppError::from(clean_cancellation_io_error()))
        });

        assert!(matches!(outcome, AttemptOutcome::Failed(_)));
        assert!(!cancellation.was_observed());
    }

    #[test]
    fn repeated_cancel_requests_are_idempotent() {
        let cancellation = AttemptCancellation::new();
        cancellation.request();
        cancellation.request();
        cancellation.request();

        let outcome = run_attempt(&cancellation, |is_cancelled| {
            assert!(is_cancelled());
            Err(AppError::from(clean_cancellation_io_error()))
        });

        assert!(matches!(outcome, AttemptOutcome::Cancelled));
        assert!(cancellation.is_requested());
        assert!(cancellation.was_observed());
    }

    #[test]
    fn attempt_state_and_outcome_are_send() {
        fn assert_send<T: Send>() {}

        assert_send::<AttemptCancellation>();
        assert_send::<AttemptOutcome>();
    }
}
