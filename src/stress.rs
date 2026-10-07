use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir as CapDir, OpenOptions as CapOpenOptions};
use serde::{Deserialize, Serialize};

use crate::attempt::io_error_is_clean_cancellation;
use crate::comparator::{self, ComparisonResult};
use crate::error::AppError;
use crate::language::Language;
use crate::model::Sample;
use crate::runner::{self, ExecutionOutcome};
use crate::safe_file;
use crate::ui::{Event, Reporter};
use crate::workspace;

const FAILURE_FORMAT_VERSION: u32 = 2;
const LEGACY_FAILURE_FORMAT_VERSION: u32 = 1;
const FAILURE_GENERATION_FILES: [&str; 5] = [
    "failed.in",
    "actual.out",
    "expected.out",
    "stderr.txt",
    "meta.toml",
];
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct UnscopedStagingError(String);

impl fmt::Display for UnscopedStagingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for UnscopedStagingError {}

pub(crate) fn is_unscoped_load_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.is::<UnscopedStagingError>())
}

fn unscoped_staging_error(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, UnscopedStagingError(message))
}

pub(crate) fn automatic_seed() -> io::Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| io::Error::other(format!("system clock is before UNIX epoch: {error}")))?;

    u64::try_from(elapsed.as_nanos()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "current time does not fit in a u64 stress seed",
        )
    })
}

#[derive(Debug, Clone)]
pub(crate) struct StressRequest {
    pub(crate) destination: PathBuf,
    pub(crate) contest_id: String,
    pub(crate) problem_index: String,
    pub(crate) candidate_source: PathBuf,
    pub(crate) candidate_language: Language,
    pub(crate) generator_source: PathBuf,
    pub(crate) reference_source: PathBuf,
    pub(crate) python: String,
    pub(crate) cpp_compiler: String,
    pub(crate) cpp_flags: Vec<String>,
    pub(crate) candidate_timeout: Duration,
    pub(crate) generator_timeout: Duration,
    pub(crate) reference_timeout: Duration,
    pub(crate) compile_timeout: Duration,
    pub(crate) base_seed: u64,
    pub(crate) count: Option<NonZeroU64>,
    pub(crate) debug: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateFailureKind {
    WrongAnswer,
    RuntimeError,
    TimedOut,
}

impl CandidateFailureKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::WrongAnswer => "WA",
            Self::RuntimeError => "RE",
            Self::TimedOut => "TLE",
        }
    }

    fn metadata_name(self) -> &'static str {
        match self {
            Self::WrongAnswer => "wrong-answer",
            Self::RuntimeError => "runtime-error",
            Self::TimedOut => "timed-out",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StressFailure {
    pub(crate) kind: CandidateFailureKind,
    pub(crate) case_number: u64,
    pub(crate) base_seed: u64,
    pub(crate) seed: u64,
    pub(crate) input: String,
    pub(crate) expected: String,
    pub(crate) actual: String,
    pub(crate) stderr: String,
    pub(crate) elapsed: Duration,
}

#[derive(Debug, Clone)]
pub(crate) enum StressOutcome {
    Completed {
        cases: u64,
        elapsed: Duration,
    },
    Failed {
        failure: StressFailure,
        saved_to: PathBuf,
        elapsed: Duration,
    },
    Cancelled {
        cases: u64,
        elapsed: Duration,
    },
}

#[derive(Debug)]
pub enum StressError {
    CandidateCompileFailed { stderr: String },
    CandidateCompileTimedOut { elapsed: Duration },
    GeneratorRuntimeError { seed: u64, stderr: String },
    GeneratorTimedOut { seed: u64, elapsed: Duration },
    GeneratorOutputLimit { seed: u64 },
    GeneratorInvalidUtf8 { seed: u64 },
    ReferenceRuntimeError { seed: u64, stderr: String },
    ReferenceTimedOut { seed: u64, elapsed: Duration },
    ReferenceOutputLimit { seed: u64 },
    ReferenceInvalidUtf8 { seed: u64 },
    SeedOverflow { base_seed: u64, case_number: u64 },
}

impl fmt::Display for StressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CandidateCompileFailed { stderr } => {
                write!(formatter, "candidate compilation failed")?;
                if !stderr.is_empty() {
                    write!(formatter, ":\n{stderr}")?;
                }
                Ok(())
            }
            Self::CandidateCompileTimedOut { elapsed } => {
                write!(
                    formatter,
                    "candidate compilation timed out after {elapsed:.2?}"
                )
            }
            Self::GeneratorRuntimeError { seed, stderr } => {
                write!(formatter, "generator failed for seed {seed}")?;
                if !stderr.is_empty() {
                    write!(formatter, ":\n{stderr}")?;
                }
                Ok(())
            }
            Self::GeneratorTimedOut { seed, elapsed } => {
                write!(
                    formatter,
                    "generator timed out for seed {seed} after {elapsed:.2?}"
                )
            }
            Self::GeneratorInvalidUtf8 { seed } => {
                write!(formatter, "generator emitted invalid UTF-8 for seed {seed}")
            }
            Self::GeneratorOutputLimit { seed } => {
                write!(
                    formatter,
                    "generator output exceeded the capture limit for seed {seed}"
                )
            }
            Self::ReferenceRuntimeError { seed, stderr } => {
                write!(formatter, "reference program failed for seed {seed}")?;
                if !stderr.is_empty() {
                    write!(formatter, ":\n{stderr}")?;
                }
                Ok(())
            }
            Self::ReferenceTimedOut { seed, elapsed } => {
                write!(
                    formatter,
                    "reference program timed out for seed {seed} after {elapsed:.2?}"
                )
            }
            Self::ReferenceInvalidUtf8 { seed } => {
                write!(
                    formatter,
                    "reference program emitted invalid UTF-8 for seed {seed}"
                )
            }
            Self::ReferenceOutputLimit { seed } => write!(
                formatter,
                "reference program output exceeded the capture limit for seed {seed}"
            ),
            Self::SeedOverflow {
                base_seed,
                case_number,
            } => write!(
                formatter,
                "stress seed overflow: base seed {base_seed}, case {case_number}"
            ),
        }
    }
}

impl std::error::Error for StressError {}

pub(crate) fn run(
    request: &StressRequest,
    reporter: &mut dyn Reporter,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<StressOutcome, AppError> {
    let started = Instant::now();

    if is_cancelled() {
        let outcome = StressOutcome::Cancelled {
            cases: 0,
            elapsed: started.elapsed(),
        };
        report_terminal_outcome(request, &outcome, reporter);
        return Ok(outcome);
    }

    let prepared = match PreparedCandidate::prepare(request, is_cancelled)? {
        PrepareOutcome::Ready(candidate) => candidate,
        PrepareOutcome::Cancelled => {
            let outcome = StressOutcome::Cancelled {
                cases: 0,
                elapsed: started.elapsed(),
            };
            report_terminal_outcome(request, &outcome, reporter);
            return Ok(outcome);
        }
    };

    reporter.report(Event::StressStarted {
        problem_index: &request.problem_index,
        base_seed: request.base_seed,
        case_limit: request.count.map(NonZeroU64::get),
    });

    let loop_started = Instant::now();
    let mut passed = 0_u64;
    let mut case_number = 1_u64;
    let mut last_progress = loop_started;

    loop {
        if let Some(limit) = request.count
            && passed >= limit.get()
        {
            let outcome = StressOutcome::Completed {
                cases: passed,
                elapsed: loop_started.elapsed(),
            };
            report_terminal_outcome(request, &outcome, reporter);
            return Ok(outcome);
        }

        if is_cancelled() {
            let outcome = StressOutcome::Cancelled {
                cases: passed,
                elapsed: loop_started.elapsed(),
            };
            report_terminal_outcome(request, &outcome, reporter);
            return Ok(outcome);
        }

        let seed = seed_for_case(request.base_seed, case_number)?;

        let input = match run_generator(request, seed, is_cancelled)? {
            ProcessStep::Completed(input) => input,
            ProcessStep::Cancelled => {
                let outcome = StressOutcome::Cancelled {
                    cases: passed,
                    elapsed: loop_started.elapsed(),
                };
                report_terminal_outcome(request, &outcome, reporter);
                return Ok(outcome);
            }
        };

        let candidate = match prepared.execute(request, &input, is_cancelled)? {
            ProcessStep::Completed(result) => result,
            ProcessStep::Cancelled => {
                let outcome = StressOutcome::Cancelled {
                    cases: passed,
                    elapsed: loop_started.elapsed(),
                };
                report_terminal_outcome(request, &outcome, reporter);
                return Ok(outcome);
            }
        };

        let candidate_failure = candidate_failure_kind(&candidate);

        let expected = match run_reference(request, &input, seed, is_cancelled)? {
            ProcessStep::Completed(expected) => expected,
            ProcessStep::Cancelled => {
                let outcome = StressOutcome::Cancelled {
                    cases: passed,
                    elapsed: loop_started.elapsed(),
                };
                report_terminal_outcome(request, &outcome, reporter);
                return Ok(outcome);
            }
        };

        let failure_kind = candidate_failure.or_else(|| {
            matches!(
                comparator::compare(&expected, &candidate.stdout),
                ComparisonResult::WrongAnswer
            )
            .then_some(CandidateFailureKind::WrongAnswer)
        });

        if let Some(kind) = failure_kind {
            let failure = StressFailure {
                kind,
                case_number,
                base_seed: request.base_seed,
                seed,
                input,
                expected,
                actual: candidate.stdout,
                stderr: candidate.stderr,
                elapsed: candidate.elapsed,
            };

            return finish_failure(
                request,
                failure,
                passed,
                loop_started,
                reporter,
                is_cancelled,
            );
        }

        passed = passed.checked_add(1).ok_or_else(|| {
            AppError::from(StressError::SeedOverflow {
                base_seed: request.base_seed,
                case_number,
            })
        })?;

        let now = Instant::now();
        if now.duration_since(last_progress) >= PROGRESS_INTERVAL {
            let elapsed = loop_started.elapsed();
            reporter.report(Event::StressProgress {
                problem_index: &request.problem_index,
                case_number,
                seed,
                passed,
                elapsed,
                cases_per_second: rate(passed, elapsed),
            });
            last_progress = now;
        }

        if let Some(limit) = request.count
            && passed >= limit.get()
        {
            let outcome = StressOutcome::Completed {
                cases: passed,
                elapsed: loop_started.elapsed(),
            };
            report_terminal_outcome(request, &outcome, reporter);
            return Ok(outcome);
        }

        case_number = case_number.checked_add(1).ok_or_else(|| {
            AppError::from(StressError::SeedOverflow {
                base_seed: request.base_seed,
                case_number: u64::MAX,
            })
        })?;
    }
}

fn finish_failure(
    request: &StressRequest,
    failure: StressFailure,
    passed: u64,
    loop_started: Instant,
    reporter: &mut dyn Reporter,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<StressOutcome, AppError> {
    if is_cancelled() {
        let outcome = StressOutcome::Cancelled {
            cases: passed,
            elapsed: loop_started.elapsed(),
        };
        report_terminal_outcome(request, &outcome, reporter);
        return Ok(outcome);
    }

    let saved_to = match persist_failure(request, &failure, is_cancelled)? {
        PersistenceOutcome::Saved(path) => path,
        PersistenceOutcome::Cancelled => {
            let outcome = StressOutcome::Cancelled {
                cases: passed,
                elapsed: loop_started.elapsed(),
            };
            report_terminal_outcome(request, &outcome, reporter);
            return Ok(outcome);
        }
    };

    let outcome = StressOutcome::Failed {
        failure,
        saved_to,
        elapsed: loop_started.elapsed(),
    };
    report_terminal_outcome(request, &outcome, reporter);
    Ok(outcome)
}

fn report_terminal_outcome(
    request: &StressRequest,
    outcome: &StressOutcome,
    reporter: &mut dyn Reporter,
) {
    match outcome {
        StressOutcome::Completed { cases, elapsed } => {
            reporter.report(Event::StressFinished {
                problem_index: &request.problem_index,
                cases: *cases,
                elapsed: *elapsed,
            });
        }
        StressOutcome::Failed {
            failure,
            saved_to,
            elapsed,
        } => {
            reporter.report(Event::StressFailed {
                problem_index: &request.problem_index,
                failure,
                saved_to,
                elapsed: *elapsed,
            });
        }
        StressOutcome::Cancelled { cases, elapsed } => {
            reporter.report(Event::StressCancelled {
                problem_index: &request.problem_index,
                cases: *cases,
                elapsed: *elapsed,
            });
        }
    }
}

fn seed_for_case(base_seed: u64, case_number: u64) -> Result<u64, AppError> {
    let offset = case_number.checked_sub(1).ok_or_else(|| {
        AppError::from(StressError::SeedOverflow {
            base_seed,
            case_number,
        })
    })?;

    base_seed.checked_add(offset).ok_or_else(|| {
        AppError::from(StressError::SeedOverflow {
            base_seed,
            case_number,
        })
    })
}

fn rate(cases: u64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds == 0.0 {
        0.0
    } else {
        cases as f64 / seconds
    }
}

enum PrepareOutcome {
    Ready(PreparedCandidate),
    Cancelled,
}

enum PreparedCandidate {
    Python {
        source: PathBuf,
    },
    Cpp {
        _build_dir: tempfile::TempDir,
        executable: PathBuf,
    },
}

impl PreparedCandidate {
    fn prepare(
        request: &StressRequest,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<PrepareOutcome, AppError> {
        match request.candidate_language {
            Language::Python => Ok(PrepareOutcome::Ready(Self::Python {
                source: request.candidate_source.clone(),
            })),
            Language::Cpp => {
                let build_dir = tempfile::tempdir()?;
                let executable = build_dir.path().join(format!(
                    "{}{}",
                    request.problem_index,
                    std::env::consts::EXE_SUFFIX
                ));

                let build_options = if request.debug {
                    runner::BuildOptions {
                        debug_include_dir: Some(crate::debug::materialize_debug_header()?),
                    }
                } else {
                    runner::BuildOptions::default()
                };

                let result = match runner::compile_cpp_with_cancel_in(
                    &request.candidate_source,
                    &executable,
                    &request.cpp_compiler,
                    &request.cpp_flags,
                    request.compile_timeout,
                    &build_options,
                    is_cancelled,
                    &request.destination,
                ) {
                    Ok(result) => result,
                    Err(error) if io_error_is_clean_cancellation(&error) => {
                        return Ok(PrepareOutcome::Cancelled);
                    }
                    Err(error) => return Err(error.into()),
                };

                match result.outcome {
                    ExecutionOutcome::Exited(status) if status.success() => {}
                    ExecutionOutcome::Exited(_) => {
                        return Err(StressError::CandidateCompileFailed {
                            stderr: result.stderr,
                        }
                        .into());
                    }
                    ExecutionOutcome::TimedOut => {
                        return Err(StressError::CandidateCompileTimedOut {
                            elapsed: result.elapsed,
                        }
                        .into());
                    }
                }

                Ok(PrepareOutcome::Ready(Self::Cpp {
                    _build_dir: build_dir,
                    executable,
                }))
            }
        }
    }

    fn execute(
        &self,
        request: &StressRequest,
        input: &str,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ProcessStep<runner::ExecutionResult>, AppError> {
        let result = match self {
            Self::Python { source } => runner::execute_python_with_cancel_in(
                source,
                input,
                &request.python,
                request.candidate_timeout,
                is_cancelled,
                &request.destination,
            ),
            Self::Cpp { executable, .. } => runner::execute_with_cancel_in(
                executable,
                &[],
                input,
                request.candidate_timeout,
                is_cancelled,
                &request.destination,
            ),
        };

        process_step(result)
    }
}

enum ProcessStep<T> {
    Completed(T),
    Cancelled,
}

fn process_step<T>(result: io::Result<T>) -> Result<ProcessStep<T>, AppError> {
    match result {
        Ok(value) => Ok(ProcessStep::Completed(value)),
        Err(error) if io_error_is_clean_cancellation(&error) => Ok(ProcessStep::Cancelled),
        Err(error) => Err(error.into()),
    }
}

fn run_generator(
    request: &StressRequest,
    seed: u64,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<ProcessStep<String>, AppError> {
    let args = vec![
        request.generator_source.as_os_str().to_owned(),
        OsString::from(seed.to_string()),
    ];

    let result = match process_step(runner::execute_with_cancel_in(
        Path::new(&request.python),
        &args,
        "",
        request.generator_timeout,
        is_cancelled,
        &request.destination,
    ))? {
        ProcessStep::Completed(result) => result,
        ProcessStep::Cancelled => return Ok(ProcessStep::Cancelled),
    };

    match result.outcome {
        ExecutionOutcome::Exited(status) if status.success() && result.stdout_truncated => {
            Err(StressError::GeneratorOutputLimit { seed }.into())
        }
        ExecutionOutcome::Exited(status) if status.success() && result.stdout_is_utf8 => {
            Ok(ProcessStep::Completed(result.stdout))
        }
        ExecutionOutcome::Exited(status) if status.success() => {
            Err(StressError::GeneratorInvalidUtf8 { seed }.into())
        }
        ExecutionOutcome::Exited(_) => Err(StressError::GeneratorRuntimeError {
            seed,
            stderr: result.stderr,
        }
        .into()),
        ExecutionOutcome::TimedOut => Err(StressError::GeneratorTimedOut {
            seed,
            elapsed: result.elapsed,
        }
        .into()),
    }
}

fn run_reference(
    request: &StressRequest,
    input: &str,
    seed: u64,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<ProcessStep<String>, AppError> {
    let result = match process_step(runner::execute_python_with_cancel_in(
        &request.reference_source,
        input,
        &request.python,
        request.reference_timeout,
        is_cancelled,
        &request.destination,
    ))? {
        ProcessStep::Completed(result) => result,
        ProcessStep::Cancelled => return Ok(ProcessStep::Cancelled),
    };

    match result.outcome {
        ExecutionOutcome::Exited(status) if status.success() && result.stdout_truncated => {
            Err(StressError::ReferenceOutputLimit { seed }.into())
        }
        ExecutionOutcome::Exited(status) if status.success() && result.stdout_is_utf8 => {
            Ok(ProcessStep::Completed(result.stdout))
        }
        ExecutionOutcome::Exited(status) if status.success() => {
            Err(StressError::ReferenceInvalidUtf8 { seed }.into())
        }
        ExecutionOutcome::Exited(_) => Err(StressError::ReferenceRuntimeError {
            seed,
            stderr: result.stderr,
        }
        .into()),
        ExecutionOutcome::TimedOut => Err(StressError::ReferenceTimedOut {
            seed,
            elapsed: result.elapsed,
        }
        .into()),
    }
}

fn candidate_failure_kind(result: &runner::ExecutionResult) -> Option<CandidateFailureKind> {
    match &result.outcome {
        ExecutionOutcome::TimedOut => Some(CandidateFailureKind::TimedOut),
        ExecutionOutcome::Exited(status) if !status.success() => {
            Some(CandidateFailureKind::RuntimeError)
        }
        ExecutionOutcome::Exited(_) if result.stdout_truncated || !result.stdout_is_utf8 => {
            Some(CandidateFailureKind::WrongAnswer)
        }
        ExecutionOutcome::Exited(_) => None,
    }
}

#[derive(Serialize)]
struct FailureMetadata<'a> {
    version: u32,
    contest: &'a str,
    problem: &'a str,
    kind: &'a str,
    case: u64,
    base_seed: u64,
    seed: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFailureMetadata {
    version: u32,
    contest: String,
    problem: String,
    kind: String,
    case: u64,
    base_seed: u64,
    seed: u64,
}

#[derive(Debug)]
enum PersistenceOutcome {
    Saved(PathBuf),
    Cancelled,
}

fn persist_failure(
    request: &StressRequest,
    failure: &StressFailure,
    is_cancelled: &dyn Fn() -> bool,
) -> io::Result<PersistenceOutcome> {
    persist_failure_with_hooks(request, failure, is_cancelled, || {}, || {})
}

fn persist_failure_with_hooks(
    request: &StressRequest,
    failure: &StressFailure,
    is_cancelled: &dyn Fn() -> bool,
    before_install: impl FnOnce(),
    before_recovery_cleanup: impl FnOnce(),
) -> io::Result<PersistenceOutcome> {
    persist_failure_with_cleanup_hook(
        request,
        failure,
        is_cancelled,
        before_install,
        before_recovery_cleanup,
        || {},
    )
}

fn persist_failure_with_cleanup_hook(
    request: &StressRequest,
    failure: &StressFailure,
    is_cancelled: &dyn Fn() -> bool,
    before_install: impl FnOnce(),
    before_recovery_cleanup: impl FnOnce(),
    before_staging_cleanup: impl FnOnce(),
) -> io::Result<PersistenceOutcome> {
    workspace::validate_problem_index(&request.problem_index)?;
    workspace::validate_workspace_marker(&request.destination)?;

    let stress_root = request.destination.join(".atc").join("stress");
    ensure_real_directory(&stress_root, "stress directory")?;
    let stress_directory = open_stress_directory(&request.destination)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("stress directory disappeared: {}", stress_root.display()),
        )
    })?;
    let generation_lock = open_stress_lock(&stress_directory, &request.problem_index)?;

    if is_cancelled() {
        return Ok(PersistenceOutcome::Cancelled);
    }

    loop {
        if is_cancelled() {
            return Ok(PersistenceOutcome::Cancelled);
        }

        match generation_lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
    }
    ensure_no_interrupted_generation(
        &stress_directory,
        &request.contest_id,
        &request.problem_index,
    )?;
    let target = stress_root.join(&request.problem_index);
    let target_exists = existing_real_directory(&target, "stress failure directory")?;
    let previous_snapshot = if target_exists {
        validate_failure_generation(&target, request)?;
        Some(safe_file::read_tree_snapshot(&target)?)
    } else {
        None
    };

    let staging_prefix = format!(
        ".stress-staging-{}-{}-",
        request.problem_index.len(),
        request.problem_index
    );
    let staging = safe_file::PreservedTempDir::new(
        tempfile::Builder::new()
            .prefix(&staging_prefix)
            .tempdir_in(&stress_root)?,
    );
    let new_generation = staging.path().join("new");
    let previous_generation = staging.path().join("previous");
    fs::create_dir(&new_generation)?;

    write_new_file(&new_generation.join("failed.in"), &failure.input)?;
    write_new_file(&new_generation.join("actual.out"), &failure.actual)?;

    write_new_file(&new_generation.join("expected.out"), &failure.expected)?;

    if !failure.stderr.is_empty() {
        write_new_file(&new_generation.join("stderr.txt"), &failure.stderr)?;
    }

    let metadata = FailureMetadata {
        version: FAILURE_FORMAT_VERSION,
        contest: &request.contest_id,
        problem: &request.problem_index,
        kind: failure.kind.metadata_name(),
        case: failure.case_number,
        base_seed: failure.base_seed,
        seed: failure.seed,
    };
    let metadata = toml::to_string_pretty(&metadata).map_err(io::Error::other)?;
    write_new_file(&new_generation.join("meta.toml"), &metadata)?;
    sync_directory(&new_generation)?;

    let recovery_identity = safe_file::RecoveryIdentity::new(
        "stress",
        &request.contest_id,
        Some(&request.problem_index),
    );
    safe_file::write_recovery_intent(staging.path(), &recovery_identity)?;

    // Once the old generation is moved, cancellation can no longer win without either hiding a
    // committed failure or requiring a destructive rollback.
    if is_cancelled() {
        complete_stress_recovery(staging, &recovery_identity, "aborted")?;
        return Ok(PersistenceOutcome::Cancelled);
    }

    // destructive swap直前に再検査する。symlinkやfileへ差し替わっていたら止める。
    let target_still_exists = existing_real_directory(&target, "stress failure directory")?;
    if target_still_exists != target_exists {
        let error = io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "stress failure destination changed while preparing update: {}",
                target.display()
            ),
        );
        return Err(abort_stress_recovery(staging, &recovery_identity, error));
    }

    if target_exists {
        safe_file::rename_noclobber(&target, &previous_generation)?;
        if let Err(error) =
            validate_failure_generation(&previous_generation, request).and_then(|()| {
                validate_previous_failure_snapshot(
                    &previous_generation,
                    previous_snapshot.as_ref().unwrap(),
                )
            })
        {
            return Err(rollback_previous_generation(
                staging,
                &target,
                &previous_generation,
                &recovery_identity,
                error,
            ));
        }
    }

    before_install();

    if let Err(error) = safe_file::rename_noclobber(&new_generation, &target) {
        if target_exists {
            return Err(rollback_previous_generation(
                staging,
                &target,
                &previous_generation,
                &recovery_identity,
                error,
            ));
        }

        return Err(abort_stress_recovery(staging, &recovery_identity, error));
    }

    before_recovery_cleanup();
    if target_exists
        && let Err(error) =
            validate_failure_generation(&previous_generation, request).and_then(|()| {
                validate_previous_failure_snapshot(
                    &previous_generation,
                    previous_snapshot.as_ref().unwrap(),
                )
            })
    {
        let kind = error.kind();
        let recovery_path = staging.keep();
        return Err(io::Error::new(
            kind,
            format!(
                "installed new stress failure {}, but the previous generation changed before cleanup: {error}; recovery data kept at {}",
                target.display(),
                recovery_path.display()
            ),
        ));
    }

    before_staging_cleanup();
    let staging_path = staging.path().to_path_buf();
    if let Err(error) = complete_stress_staging(staging, request, target_exists, &recovery_identity)
    {
        return Err(io::Error::new(
            error.kind(),
            format!(
                "stress failure installed at {}, but recovery completion failed at {}: {error}; recovery check required before another save",
                target.display(),
                staging_path.display()
            ),
        ));
    }

    Ok(PersistenceOutcome::Saved(target))
}

fn validate_previous_failure_snapshot(
    path: &Path,
    expected: &safe_file::TreeSnapshot,
) -> io::Result<()> {
    if &safe_file::read_tree_snapshot(path)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "previous stress generation changed during update: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn complete_stress_staging(
    staging: safe_file::PreservedTempDir,
    request: &StressRequest,
    had_previous: bool,
    recovery_identity: &safe_file::RecoveryIdentity,
) -> io::Result<()> {
    let root = staging.path();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_name() != safe_file::RECOVERY_INTENT
            && (entry.file_name() != "previous" || !had_previous)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "stress staging contains an unowned entry: {}",
                    entry.path().display()
                ),
            ));
        }
    }
    if had_previous {
        let previous = root.join("previous");
        if !existing_real_directory(&previous, "previous stress generation")? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "previous stress generation is missing: {}",
                    previous.display()
                ),
            ));
        }
        validate_failure_generation(&previous, request)?;
    }
    complete_stress_recovery(staging, recovery_identity, "recovery").map(|_| ())
}

fn complete_stress_recovery(
    staging: safe_file::PreservedTempDir,
    identity: &safe_file::RecoveryIdentity,
    state: &str,
) -> io::Result<PathBuf> {
    let root = staging.path();
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stress staging name is not Unicode",
            )
        })?;
    let suffix = name.strip_prefix(".stress-staging-").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "unexpected stress staging name")
    })?;
    let completed = root.with_file_name(format!(".stress-{state}-{suffix}"));
    safe_file::complete_recovery(staging, &completed, identity)
}

fn abort_stress_recovery(
    staging: safe_file::PreservedTempDir,
    identity: &safe_file::RecoveryIdentity,
    original: io::Error,
) -> io::Error {
    let kind = original.kind();
    match complete_stress_recovery(staging, identity, "aborted") {
        Ok(path) => io::Error::new(
            kind,
            format!(
                "{original}; attempted generation kept at {}",
                path.display()
            ),
        ),
        Err(error) => io::Error::new(
            kind,
            format!("{original}; recovery completion also failed: {error}"),
        ),
    }
}

fn validate_failure_generation(path: &Path, request: &StressRequest) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "stress failure generation contains a non-Unicode entry: {}",
                    entry.path().display()
                ),
            )
        })?;

        if stress_entry_is_reparse(&entry.path())?
            || !FAILURE_GENERATION_FILES.contains(&name)
            || !entry.file_type()?.is_file()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "stress failure generation contains an unowned entry: {}",
                    entry.path().display()
                ),
            ));
        }
    }

    for required in ["failed.in", "actual.out", "meta.toml"] {
        if !existing_regular_file(&path.join(required), "stress failure file")? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "stress failure generation is missing {required}: {}",
                    path.display()
                ),
            ));
        }
    }

    let metadata_path = path.join("meta.toml");
    let metadata: StoredFailureMetadata = toml::from_str(&fs::read_to_string(&metadata_path)?)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid stress metadata {}: {error}",
                    metadata_path.display()
                ),
            )
        })?;

    if !matches!(
        metadata.version,
        LEGACY_FAILURE_FORMAT_VERSION | FAILURE_FORMAT_VERSION
    ) || metadata.contest != request.contest_id
        || metadata.problem != request.problem_index
        || metadata.case == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "stress failure metadata does not own generation {}",
                path.display()
            ),
        ));
    }

    let expected_seed = metadata
        .case
        .checked_sub(1)
        .and_then(|offset| metadata.base_seed.checked_add(offset));
    if expected_seed != Some(metadata.seed) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "stress failure metadata has inconsistent seed mapping: {}",
                metadata_path.display()
            ),
        ));
    }

    match metadata.kind.as_str() {
        "wrong-answer" => {
            if !existing_regular_file(&path.join("expected.out"), "stress expected output")? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "wrong-answer stress generation is missing expected.out: {}",
                        path.display()
                    ),
                ));
            }
        }
        // Historical v1 RE/TLE generations did not include expected.out. New generations do.
        "runtime-error" | "timed-out" if metadata.version == LEGACY_FAILURE_FORMAT_VERSION => {}
        "runtime-error" | "timed-out" => {
            if !existing_regular_file(&path.join("expected.out"), "stress expected output")? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "stress generation is missing expected.out: {}",
                        path.display()
                    ),
                ));
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown stress failure kind in {}", metadata_path.display()),
            ));
        }
    }

    // Use the same snapshot and content checks as ordinary tests before moving the old
    // generation. A legacy RE/TLE without expected.out remains valid but cannot be replayed.
    let generation = CapDir::open_ambient_dir(path, ambient_authority())?;
    let snapshot = open_saved_case_snapshot(&generation)?;
    read_saved_case_snapshot(snapshot, &request.contest_id, &request.problem_index)?;

    Ok(())
}

fn valid_failure_metadata(
    metadata: &StoredFailureMetadata,
    contest_id: &str,
    problem_index: &str,
) -> bool {
    matches!(
        metadata.version,
        LEGACY_FAILURE_FORMAT_VERSION | FAILURE_FORMAT_VERSION
    ) && metadata.contest == contest_id
        && metadata.problem == problem_index
        && metadata.case != 0
        && metadata
            .case
            .checked_sub(1)
            .and_then(|offset| metadata.base_seed.checked_add(offset))
            == Some(metadata.seed)
        && matches!(
            metadata.kind.as_str(),
            "wrong-answer" | "runtime-error" | "timed-out"
        )
}

fn rollback_previous_generation(
    staging: safe_file::PreservedTempDir,
    target: &Path,
    previous_generation: &Path,
    identity: &safe_file::RecoveryIdentity,
    original: io::Error,
) -> io::Error {
    if let Err(rollback_error) = safe_file::rename_noclobber(previous_generation, target) {
        let kind = original.kind();
        let recovery_path = staging.keep();
        return io::Error::new(
            kind,
            format!(
                "failed to replace stress failure {}: {original}; rollback also failed: {rollback_error}; recovery data kept at {}",
                target.display(),
                recovery_path.display()
            ),
        );
    }

    abort_stress_recovery(staging, identity, original)
}

pub(crate) fn load_saved_case(
    destination: &Path,
    contest_id: &str,
    problem_index: &str,
) -> io::Result<Option<Sample>> {
    load_saved_case_with_hook(destination, contest_id, problem_index, || {})
}

pub(crate) enum SavedCaseLoad {
    Absent,
    Replay(Sample),
    AmbiguousV1WithoutExpected { kind: &'static str },
}

pub(crate) fn load_saved_case_status(
    destination: &Path,
    contest_id: &str,
    problem_index: &str,
) -> io::Result<SavedCaseLoad> {
    load_saved_case_status_with_hook(destination, contest_id, problem_index, || {})
}

fn load_saved_case_with_hook(
    destination: &Path,
    contest_id: &str,
    problem_index: &str,
    before_shared_lock: impl FnOnce(),
) -> io::Result<Option<Sample>> {
    match load_saved_case_status_with_hook(
        destination,
        contest_id,
        problem_index,
        before_shared_lock,
    )? {
        SavedCaseLoad::Replay(sample) => Ok(Some(sample)),
        SavedCaseLoad::Absent | SavedCaseLoad::AmbiguousV1WithoutExpected { .. } => Ok(None),
    }
}

fn load_saved_case_status_with_hook(
    destination: &Path,
    contest_id: &str,
    problem_index: &str,
    before_shared_lock: impl FnOnce(),
) -> io::Result<SavedCaseLoad> {
    workspace::validate_problem_index(problem_index)?;

    let Some(stress_directory) = open_stress_directory(destination)? else {
        return Ok(SavedCaseLoad::Absent);
    };

    let generation_lock = open_stress_lock(&stress_directory, problem_index)?;
    before_shared_lock();
    generation_lock.lock_shared()?;

    ensure_no_interrupted_generation(&stress_directory, contest_id, problem_index)?;

    let Some(generation) = open_optional_directory(&stress_directory, problem_index)? else {
        return Ok(SavedCaseLoad::Absent);
    };

    let snapshot = open_saved_case_snapshot(&generation)?;

    // Keep the shared lock through the reads. On Windows, no-follow file handles intentionally
    // block a directory rename, so releasing the lock before closing them would make the writer's
    // generation swap fail instead of merely waiting for this coherent read to finish.
    drop(generation);
    let result = read_saved_case_snapshot(snapshot, contest_id, problem_index);
    drop(generation_lock);
    result
}

fn ensure_no_interrupted_generation(
    stress_directory: &CapDir,
    contest_id: &str,
    problem_index: &str,
) -> io::Result<()> {
    let owned_recovery_prefix = format!(
        ".stress-recovery-{}-{}-",
        problem_index.len(),
        problem_index
    );
    let owned_aborted_prefix =
        format!(".stress-aborted-{}-{}-", problem_index.len(), problem_index);
    for entry in stress_directory.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            if name.to_string_lossy().starts_with(".stress-staging-") {
                return Err(unscoped_staging_error(
                    "stress staging has a non-Unicode name; recovery check required".to_string(),
                ));
            }
            continue;
        };
        if name.starts_with(&owned_recovery_prefix) || name.starts_with(&owned_aborted_prefix) {
            let expected =
                safe_file::RecoveryIdentity::new("stress", contest_id, Some(problem_index));
            let archive = stress_directory.open_dir_nofollow(name)?;
            let mut intent =
                open_regular_file(&archive, safe_file::RECOVERY_INTENT)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "stress recovery intent is missing",
                    )
                })?;
            let mut complete = open_regular_file(&archive, safe_file::RECOVERY_COMPLETE)?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "stress recovery completion is missing",
                    )
                })?;
            let mut intent_bytes = Vec::new();
            let mut complete_bytes = Vec::new();
            intent.read_to_end(&mut intent_bytes)?;
            complete.read_to_end(&mut complete_bytes)?;
            if safe_file::parse_completed_recovery(&intent_bytes, &complete_bytes)? != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("stress recovery area requires review: .atc/stress/{name}"),
                ));
            }
            continue;
        }
        if !name.starts_with(".stress-staging-") {
            continue;
        }
        let (requires_review, unscoped) = match current_staging_problem(name) {
            Ok(Some(owner)) if owner == problem_index => (true, false),
            Ok(Some(owner)) => {
                // A writer of another problem can still be creating meta.toml. Do not inspect
                // that partial file without its lock; an idle stage must match its name.
                let lock = open_stress_lock(stress_directory, owner)?;
                match lock.try_lock_shared() {
                    Ok(()) => (
                        current_staging_metadata_conflicts(
                            stress_directory,
                            name,
                            contest_id,
                            owner,
                        ),
                        true,
                    ),
                    Err(fs::TryLockError::WouldBlock) => (false, false),
                    Err(fs::TryLockError::Error(error)) => return Err(error),
                }
            }
            Ok(None) => (
                legacy_staging_requires_review(stress_directory, name, contest_id, problem_index),
                true,
            ),
            Err(()) => (true, true),
        };
        if requires_review {
            let message = format!(
                "saved stress case for problem {problem_index} requires recovery check: .atc/stress/{name}"
            );
            return Err(if unscoped {
                unscoped_staging_error(message)
            } else {
                io::Error::new(io::ErrorKind::InvalidData, message)
            });
        }
    }
    Ok(())
}

fn current_staging_problem(name: &str) -> Result<Option<&str>, ()> {
    let suffix = name.strip_prefix(".stress-staging-").ok_or(())?;
    if !suffix.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return Ok(None);
    }
    let (length_text, remainder) = suffix.split_once('-').ok_or(())?;
    let length = length_text.parse::<usize>().map_err(|_| ())?;
    if length == 0 || length.to_string() != length_text {
        return Err(());
    }
    let problem = remainder.get(..length).ok_or(())?;
    let random = remainder
        .get(length..)
        .and_then(|tail| tail.strip_prefix('-'))
        .ok_or(())?;
    if random.is_empty()
        || !random.bytes().all(|byte| byte.is_ascii_alphanumeric())
        || workspace::validate_problem_index(problem).is_err()
    {
        return Err(());
    }
    Ok(Some(problem))
}

fn current_staging_metadata_conflicts(
    stress_directory: &CapDir,
    name: &str,
    contest_id: &str,
    named_problem: &str,
) -> bool {
    let Ok(staging) = stress_directory.open_dir_nofollow(name) else {
        return true;
    };
    for generation_name in ["previous", "new"] {
        let generation = match open_optional_directory(&staging, generation_name) {
            Ok(Some(generation)) => generation,
            Ok(None) => continue,
            Err(_) => return true,
        };
        let mut metadata_file = match open_regular_file(&generation, "meta.toml") {
            Ok(Some(file)) => file,
            Ok(None) => continue,
            Err(_) => return true,
        };
        let mut metadata_text = String::new();
        if metadata_file.read_to_string(&mut metadata_text).is_err() {
            continue;
        }
        let Ok(metadata) = toml::from_str::<toml::Value>(&metadata_text) else {
            continue;
        };
        if metadata
            .get("contest")
            .and_then(toml::Value::as_str)
            .is_some_and(|id| id != contest_id)
            || metadata
                .get("problem")
                .and_then(toml::Value::as_str)
                .is_some_and(|index| index != named_problem)
        {
            return true;
        }
    }
    false
}

fn legacy_staging_requires_review(
    stress_directory: &CapDir,
    name: &str,
    contest_id: &str,
    problem_index: &str,
) -> bool {
    let Ok(staging) = stress_directory.open_dir_nofollow(name) else {
        return true;
    };
    let mut known_owner: Option<(String, String)> = None;
    for generation_name in ["previous", "new"] {
        let generation = match open_optional_directory(&staging, generation_name) {
            Ok(Some(generation)) => generation,
            Ok(None) => continue,
            Err(_) => return true,
        };
        let Ok(Some(mut metadata_file)) = open_regular_file(&generation, "meta.toml") else {
            return true;
        };
        let mut metadata_text = String::new();
        if metadata_file.read_to_string(&mut metadata_text).is_err() {
            return true;
        }
        let Ok(metadata) = toml::from_str::<StoredFailureMetadata>(&metadata_text) else {
            return true;
        };
        if !valid_failure_metadata(&metadata, &metadata.contest, &metadata.problem)
            || workspace::validate_problem_index(&metadata.problem).is_err()
        {
            return true;
        }
        let owner = (metadata.contest, metadata.problem);
        if known_owner.as_ref().is_some_and(|known| known != &owner) {
            return true;
        }
        known_owner = Some(owner);
    }
    match known_owner {
        Some((owner_contest, owner_problem)) => {
            owner_contest == contest_id && owner_problem == problem_index
        }
        None => true,
    }
}

struct SavedCaseSnapshot {
    metadata: fs::File,
    actual: fs::File,
    input: fs::File,
    expected: Option<fs::File>,
    stderr: Option<fs::File>,
}

fn open_saved_case_snapshot(generation: &CapDir) -> io::Result<SavedCaseSnapshot> {
    validate_saved_generation_entries(generation)?;

    let metadata = require_saved_case_file(generation, "meta.toml")?;
    let actual = require_saved_case_file(generation, "actual.out")?;
    let input = require_saved_case_file(generation, "failed.in")?;
    // Historical v1 RE/TLE generations did not have expected.out. They remain valid
    // stored failures, but cannot be replayed as ordinary test cases.
    let expected = open_regular_file(generation, "expected.out")?;
    let stderr = open_regular_file(generation, "stderr.txt").map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot open saved stress stderr.txt: {error}"),
        )
    })?;

    Ok(SavedCaseSnapshot {
        metadata,
        actual,
        input,
        expected,
        stderr,
    })
}

fn require_saved_case_file(generation: &CapDir, name: &str) -> io::Result<fs::File> {
    open_regular_file(generation, name)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("saved stress case is missing {name}"),
        )
    })
}

fn validate_saved_generation_entries(generation: &CapDir) -> io::Result<()> {
    for entry in generation.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "saved stress case contains a non-Unicode entry",
            ));
        };

        if !FAILURE_GENERATION_FILES.contains(&name) || !entry.file_type()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("saved stress case contains an unowned entry: {name}"),
            ));
        }
    }

    Ok(())
}

fn read_saved_case_snapshot(
    mut snapshot: SavedCaseSnapshot,
    contest_id: &str,
    problem_index: &str,
) -> io::Result<SavedCaseLoad> {
    let metadata_text = read_saved_case_utf8(&mut snapshot.metadata, "meta.toml")?;
    let metadata = toml::from_str::<StoredFailureMetadata>(&metadata_text).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid saved stress metadata: {error}"),
        )
    })?;
    if !valid_failure_metadata(&metadata, contest_id, problem_index) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "saved stress metadata does not match the current contest and problem",
        ));
    }

    let input = read_saved_case_utf8(&mut snapshot.input, "failed.in")?;
    read_saved_case_bytes(&mut snapshot.actual, "actual.out")?;
    if let Some(stderr) = snapshot.stderr.as_mut() {
        read_saved_case_bytes(stderr, "stderr.txt")?;
    }

    let Some(expected_file) = snapshot.expected.as_mut() else {
        let kind = match (metadata.version, metadata.kind.as_str()) {
            (LEGACY_FAILURE_FORMAT_VERSION, "runtime-error") => "runtime-error",
            (LEGACY_FAILURE_FORMAT_VERSION, "timed-out") => "timed-out",
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "saved stress case is missing expected.out",
                ));
            }
        };
        return Ok(SavedCaseLoad::AmbiguousV1WithoutExpected { kind });
    };
    let expected = read_saved_case_utf8(expected_file, "expected.out")?;

    Ok(SavedCaseLoad::Replay(Sample {
        input,
        output: expected,
    }))
}

fn read_saved_case_bytes(file: &mut fs::File, name: &str) -> io::Result<()> {
    io::copy(file, &mut io::sink()).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot read saved stress {name}: {error}"),
        )
    })?;
    Ok(())
}

fn read_saved_case_utf8(file: &mut fs::File, name: &str) -> io::Result<String> {
    let mut content = String::new();
    file.read_to_string(&mut content).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot read saved stress {name}: {error}"),
        )
    })?;
    Ok(content)
}

fn open_stress_directory(destination: &Path) -> io::Result<Option<CapDir>> {
    let workspace = CapDir::open_ambient_dir(destination, ambient_authority())?;
    let Some(marker) = open_optional_directory(&workspace, ".atc")? else {
        return Ok(None);
    };

    open_optional_directory(&marker, "stress")
}

fn open_optional_directory(parent: &CapDir, name: &str) -> io::Result<Option<CapDir>> {
    match parent.open_dir_nofollow(name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_regular_file(directory: &CapDir, name: &str) -> io::Result<Option<fs::File>> {
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);

    let file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stress generation entry is not a regular file: {name}"),
        ));
    }

    Ok(Some(file.into_std()))
}

fn open_stress_lock(stress_directory: &CapDir, problem_index: &str) -> io::Result<fs::File> {
    let name = format!(".{problem_index}.lock");
    let mut options = CapOpenOptions::new();
    // Readers and writers must both open the same persistent lock file. Never remove it:
    // recreating the name could leave concurrent processes locking different file objects.
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .follow(FollowSymlinks::No)
        .nonblock(true);

    let file = stress_directory.open_with(&name, &options)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stress generation lock is not a regular file: {name}"),
        ));
    }

    Ok(file.into_std())
}

fn ensure_real_directory(path: &Path, kind: &str) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if stress_metadata_is_reparse(&metadata) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is a reparse point: {}", path.display()),
        )),
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a real directory: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(path),
        Err(error) => Err(error),
    }
}

fn existing_real_directory(path: &Path, kind: &str) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if stress_metadata_is_reparse(&metadata) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is a reparse point: {}", path.display()),
        )),
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a real directory: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn existing_regular_file(path: &Path, kind: &str) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if stress_metadata_is_reparse(&metadata) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is a reparse point: {}", path.display()),
        )),
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a real file: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn stress_entry_is_reparse(path: &Path) -> io::Result<bool> {
    Ok(stress_metadata_is_reparse(&fs::symlink_metadata(path)?))
}

#[cfg(not(windows))]
fn stress_entry_is_reparse(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(windows)]
fn stress_metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn stress_metadata_is_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

fn write_new_file(path: &Path, content: &str) -> io::Result<()> {
    use std::io::Write as _;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // std does not expose a portable way to flush a Windows directory handle. Each generation
    // file is still flushed individually before the same-volume directory rename.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn request(destination: &Path, base_seed: u64) -> StressRequest {
        StressRequest {
            destination: destination.to_path_buf(),
            contest_id: "abc123".to_string(),
            problem_index: "A".to_string(),
            candidate_source: destination.join("A.cpp"),
            candidate_language: Language::Cpp,
            generator_source: destination.join("A_gen.py"),
            reference_source: destination.join("A_brute.py"),
            python: "python".to_string(),
            cpp_compiler: "g++".to_string(),
            cpp_flags: Vec::new(),
            candidate_timeout: Duration::from_secs(1),
            generator_timeout: Duration::from_secs(1),
            reference_timeout: Duration::from_secs(1),
            compile_timeout: Duration::from_secs(1),
            base_seed,
            count: NonZeroU64::new(100),
            debug: false,
        }
    }

    fn persisted_path(result: io::Result<PersistenceOutcome>) -> PathBuf {
        match result.unwrap() {
            PersistenceOutcome::Saved(path) => path,
            PersistenceOutcome::Cancelled => panic!("persistence unexpectedly cancelled"),
        }
    }

    fn write_v1_generation(
        target: &Path,
        contest: &str,
        problem: &str,
        kind: &str,
        input: &str,
        expected: Option<&str>,
    ) {
        fs::create_dir_all(target).unwrap();
        fs::write(target.join("failed.in"), input).unwrap();
        fs::write(target.join("actual.out"), "old actual\n").unwrap();
        if let Some(expected) = expected {
            fs::write(target.join("expected.out"), expected).unwrap();
        }
        fs::write(
            target.join("meta.toml"),
            format!(
                "version = 1\ncontest = {contest:?}\nproblem = {problem:?}\nkind = {kind:?}\ncase = 1\nbase_seed = 10\nseed = 10\n"
            ),
        )
        .unwrap();
    }

    #[derive(Default)]
    struct TerminalReporter {
        terminal: Vec<&'static str>,
    }

    impl Reporter for TerminalReporter {
        fn report(&mut self, event: Event<'_>) {
            match event {
                Event::StressFailed { .. } => self.terminal.push("failed"),
                Event::StressCancelled { .. } => self.terminal.push("cancelled"),
                Event::StressFinished { .. } => self.terminal.push("finished"),
                _ => {}
            }
        }
    }

    #[test]
    fn seed_is_base_plus_one_origin_case_offset() {
        assert_eq!(seed_for_case(100, 1).unwrap(), 100);
        assert_eq!(seed_for_case(100, 2).unwrap(), 101);
        assert_eq!(seed_for_case(100, 1000).unwrap(), 1099);
    }

    #[test]
    fn seed_overflow_is_an_explicit_stress_error() {
        let error = seed_for_case(u64::MAX, 2).unwrap_err();

        assert!(matches!(
            error,
            AppError::Stress(StressError::SeedOverflow {
                base_seed: u64::MAX,
                case_number: 2,
            })
        ));
    }

    #[test]
    fn save_refuses_unreadable_previous_case_before_moving_it() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let target = temp.path().join(".atc/stress/A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "old\n",
            Some("answer\n"),
        );
        let original = vec![0xff, 0xfe, b'\n'];
        fs::write(target.join("failed.in"), &original).unwrap();
        assert!(load_saved_case(temp.path(), "abc123", "A").is_err());

        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new\n".to_string(),
            expected: "new answer\n".to_string(),
            actual: "actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let error = persist_failure(&request(temp.path(), 10), &failure, &|| false).unwrap_err();
        assert!(error.to_string().contains("failed.in"));
        assert_eq!(fs::read(target.join("failed.in")).unwrap(), original);
        assert_eq!(
            fs::read_to_string(target.join("expected.out")).unwrap(),
            "answer\n"
        );
    }

    #[test]
    fn replacing_failure_generation_does_not_leave_stale_optional_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();

        let request = request(temp.path(), 10);

        let wa = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "1\n".to_string(),
            expected: "2\n".to_string(),
            actual: "3\n".to_string(),
            stderr: "debug\n".to_string(),
            elapsed: Duration::from_millis(5),
        };

        let target = persisted_path(persist_failure(&request, &wa, &|| false));
        assert_eq!(
            fs::read_to_string(target.join("expected.out")).unwrap(),
            "2\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("stderr.txt")).unwrap(),
            "debug\n"
        );

        let re = StressFailure {
            kind: CandidateFailureKind::RuntimeError,
            case_number: 2,
            base_seed: 10,
            seed: 11,
            input: "4\n".to_string(),
            expected: "5\n".to_string(),
            actual: String::new(),
            stderr: String::new(),
            elapsed: Duration::from_millis(7),
        };

        let target = persisted_path(persist_failure(&request, &re, &|| false));
        assert_eq!(
            fs::read_to_string(target.join("expected.out")).unwrap(),
            "5\n"
        );
        assert!(!target.join("stderr.txt").exists());
        assert_eq!(fs::read_to_string(target.join("failed.in")).unwrap(), "4\n");

        let metadata = fs::read_to_string(target.join("meta.toml")).unwrap();
        assert!(metadata.contains("version = 2"));
        assert!(metadata.contains("kind = \"runtime-error\""));
        assert!(metadata.contains("seed = 11"));
        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A").unwrap(),
            Some(Sample {
                input: "4\n".to_string(),
                output: "5\n".to_string(),
            })
        );
        assert!(
            fs::read_dir(target.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".stress-staging-"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn unreadable_diagnostic_file_refuses_replacement() {
        use std::os::windows::fs::OpenOptionsExt;

        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let target = temp.path().join(".atc/stress/A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "old\n",
            Some("answer\n"),
        );
        let diagnostic = target.join("stderr.txt");
        fs::write(&diagnostic, [0xff, 0x00, 0xfe]).unwrap();
        // Permit rename, but deny another reader while this handle is alive.
        let blocker = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x2 | 0x4)
            .open(&diagnostic)
            .unwrap();

        let request = request(temp.path(), 10);
        let validation_error = validate_failure_generation(&target, &request).unwrap_err();
        assert!(
            validation_error.to_string().contains("stderr.txt"),
            "{validation_error}"
        );

        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new\n".to_string(),
            expected: "new answer\n".to_string(),
            actual: "wrong\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let error = persist_failure(&request, &failure, &|| false).unwrap_err();
        assert!(error.to_string().contains("stderr.txt"), "{error}");
        drop(blocker);
        assert_eq!(fs::read(&diagnostic).unwrap(), [0xff, 0x00, 0xfe]);
        assert_eq!(
            fs::read_to_string(target.join("failed.in")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn binary_diagnostics_remain_valid_and_are_kept_in_previous_generation() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let target = temp.path().join(".atc/stress/A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "old\n",
            Some("answer\n"),
        );
        fs::write(target.join("actual.out"), [0xff, 0x00]).unwrap();
        fs::write(target.join("stderr.txt"), [0xfe, 0x00]).unwrap();
        let request = request(temp.path(), 10);
        validate_failure_generation(&target, &request).unwrap();
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new\n".to_string(),
            expected: "new answer\n".to_string(),
            actual: "wrong\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        persisted_path(persist_failure(&request, &failure, &|| false));
        let recovery = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".stress-recovery-")
                    && path.join("previous/failed.in").exists()
            })
            .unwrap();
        assert_eq!(
            fs::read(recovery.join("previous/actual.out")).unwrap(),
            [0xff, 0x00]
        );
        assert_eq!(
            fs::read(recovery.join("previous/stderr.txt")).unwrap(),
            [0xfe, 0x00]
        );
        assert!(
            load_saved_case(temp.path(), "abc123", "A")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn valid_external_edit_of_previous_generation_aborts_completion() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let target = temp.path().join(".atc/stress/A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "old\n",
            Some("answer\n"),
        );
        let request = request(temp.path(), 10);
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new\n".to_string(),
            expected: "new answer\n".to_string(),
            actual: "wrong\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let stress_root = target.parent().unwrap();
        let changed = std::cell::RefCell::new(None);
        let result = persist_failure_with_hooks(
            &request,
            &failure,
            &|| false,
            || {},
            || {
                let staging = fs::read_dir(stress_root)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".stress-staging-")
                    })
                    .unwrap();
                let input = staging.join("previous/failed.in");
                fs::write(&input, "externally edited\n").unwrap();
                *changed.borrow_mut() = Some(input);
            },
        );
        assert!(result.is_err(), "external edit must stop completion");
        assert_eq!(
            fs::read_to_string(changed.into_inner().unwrap()).unwrap(),
            "externally edited\n"
        );
    }

    #[test]
    fn same_name_replacement_of_previous_file_is_detected_by_identity() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let target = temp.path().join(".atc/stress/A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "old\n",
            Some("answer\n"),
        );
        let moved_original = temp.path().join("original-failed.in");
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new\n".to_string(),
            expected: "new answer\n".to_string(),
            actual: "wrong\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let stress_root = target.parent().unwrap();
        let result = persist_failure_with_hooks(
            &request(temp.path(), 10),
            &failure,
            &|| false,
            || {},
            || {
                let staging = fs::read_dir(stress_root)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".stress-staging-")
                    })
                    .unwrap();
                let input = staging.join("previous/failed.in");
                fs::rename(&input, &moved_original).unwrap();
                fs::write(&input, "old\n").unwrap();
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&moved_original).unwrap(), "old\n");
    }

    #[test]
    fn completed_stress_update_retains_previous_generation_without_blocking_load() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let request = request(temp.path(), 10);
        let first = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "old input\n".to_string(),
            expected: "old expected\n".to_string(),
            actual: "old actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        persisted_path(persist_failure(&request, &first, &|| false));
        let second = StressFailure {
            input: "new input\n".to_string(),
            expected: "new expected\n".to_string(),
            actual: "new actual\n".to_string(),
            ..first
        };
        persisted_path(persist_failure(&request, &second, &|| false));

        let stress_root = temp.path().join(".atc/stress");
        let recovery = fs::read_dir(&stress_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".stress-recovery-1-A-")
                    && path.join("previous/failed.in").is_file()
            })
            .expect("completed update must retain its previous generation");
        assert_eq!(
            fs::read_to_string(recovery.join("previous/failed.in")).unwrap(),
            "old input\n"
        );
        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A")
                .unwrap()
                .unwrap()
                .input,
            "new input\n"
        );
    }

    #[test]
    fn failed_stress_completion_record_keeps_both_generations_for_review() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let request = request(temp.path(), 10);
        let first = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "old\n".to_string(),
            expected: "old expected\n".to_string(),
            actual: "old actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        persisted_path(persist_failure(&request, &first, &|| false));
        let second = StressFailure {
            input: "new\n".to_string(),
            ..first
        };
        let root = temp.path().join(".atc/stress");
        let error = persist_failure_with_cleanup_hook(
            &request,
            &second,
            &|| false,
            || {},
            || {},
            || {
                let staging = fs::read_dir(&root)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".stress-staging-1-A-")
                    })
                    .unwrap();
                fs::write(staging.join("recovery-complete.toml"), "partial\n").unwrap();
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("recovery completion failed"));
        let staging = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".stress-staging-1-A-")
            })
            .unwrap();
        assert_eq!(
            fs::read_to_string(staging.join("previous/failed.in")).unwrap(),
            "old\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("A/failed.in")).unwrap(),
            "new\n"
        );
        assert!(load_saved_case(temp.path(), "abc123", "A").is_err());
    }

    #[test]
    fn cancellation_before_persistence_commit_preserves_the_previous_generation() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let request = request(temp.path(), 10);

        let previous = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "old input\n".to_string(),
            expected: "old expected\n".to_string(),
            actual: "old actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let target = persisted_path(persist_failure(&request, &previous, &|| false));

        let replacement = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 2,
            base_seed: 10,
            seed: 11,
            input: "new input\n".to_string(),
            expected: "new expected\n".to_string(),
            actual: "new actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let checks = Cell::new(0_u8);
        let is_cancelled = || {
            let check = checks.get();
            checks.set(check + 1);
            check >= 2
        };
        let mut reporter = TerminalReporter::default();

        let outcome = finish_failure(
            &request,
            replacement,
            1,
            Instant::now(),
            &mut reporter,
            &is_cancelled,
        )
        .unwrap();

        assert!(matches!(outcome, StressOutcome::Cancelled { cases: 1, .. }));
        assert_eq!(reporter.terminal, ["cancelled"]);
        assert_eq!(
            fs::read_to_string(target.join("failed.in")).unwrap(),
            "old input\n"
        );
    }

    #[test]
    fn persistence_refuses_to_delete_an_unowned_target_directory() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join(".atc").join("stress").join("A");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("important.txt"), "keep me\n").unwrap();

        let request = request(temp.path(), 10);
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "1\n".to_string(),
            expected: "2\n".to_string(),
            actual: "3\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };

        let error = persist_failure(&request, &failure, &|| false).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(target.join("important.txt")).unwrap(),
            "keep me\n"
        );
        assert!(!target.join("failed.in").exists());
    }

    #[test]
    fn persistence_destination_race_preserves_the_competing_generation() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let request = request(temp.path(), 10);
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new input\n".to_string(),
            expected: "new expected\n".to_string(),
            actual: "new actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let target = temp.path().join(".atc").join("stress").join("A");

        let error = persist_failure_with_hooks(
            &request,
            &failure,
            &|| false,
            || {
                fs::create_dir(&target).unwrap();
                fs::write(target.join("competitor.txt"), "keep me\n").unwrap();
            },
            || {},
        )
        .expect_err("a target created immediately before install must win the race");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(target.join("competitor.txt")).unwrap(),
            "keep me\n"
        );
        assert!(
            fs::read_dir(temp.path().join(".atc").join("stress"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".stress-staging-"))
        );
    }

    #[test]
    fn persistence_rollback_race_preserves_competitor_and_both_generations() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let request = request(temp.path(), 10);
        let previous = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "old input\n".to_string(),
            expected: "old expected\n".to_string(),
            actual: "old actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let target = persisted_path(persist_failure(&request, &previous, &|| false));
        let replacement = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 2,
            base_seed: 10,
            seed: 11,
            input: "new input\n".to_string(),
            expected: "new expected\n".to_string(),
            actual: "new actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };

        let error = persist_failure_with_hooks(
            &request,
            &replacement,
            &|| false,
            || {
                fs::create_dir(&target).unwrap();
                fs::write(target.join("competitor.txt"), "keep me\n").unwrap();
            },
            || {},
        )
        .expect_err("a competing target must block both install and rollback");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("rollback also failed"));
        assert!(error.to_string().contains("recovery data kept at"));
        assert_eq!(
            fs::read_to_string(target.join("competitor.txt")).unwrap(),
            "keep me\n"
        );
        let stress_root = temp.path().join(".atc").join("stress");
        let recovery = fs::read_dir(&stress_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".stress-staging-")
            })
            .expect("failed rollback must retain stress staging");
        assert_eq!(
            fs::read_to_string(recovery.join("previous").join("failed.in")).unwrap(),
            "old input\n"
        );
        assert_eq!(
            fs::read_to_string(recovery.join("new").join("failed.in")).unwrap(),
            "new input\n"
        );
    }

    #[test]
    fn persistence_final_reinspection_keeps_a_late_changed_previous_generation() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let request = request(temp.path(), 10);
        let previous = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "old input\n".to_string(),
            expected: "old expected\n".to_string(),
            actual: "old actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let target = persisted_path(persist_failure(&request, &previous, &|| false));
        let replacement = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 2,
            base_seed: 10,
            seed: 11,
            input: "new input\n".to_string(),
            expected: "new expected\n".to_string(),
            actual: "new actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let stress_root = temp.path().join(".atc").join("stress");

        let error = persist_failure_with_hooks(
            &request,
            &replacement,
            &|| false,
            || {},
            || {
                let recovery = fs::read_dir(&stress_root)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".stress-staging-")
                    })
                    .unwrap();
                fs::write(recovery.join("previous").join("important.txt"), "keep me\n").unwrap();
            },
        )
        .expect_err("a changed previous generation must be retained instead of cleaned");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("recovery data kept at"));
        assert_eq!(
            fs::read_to_string(target.join("failed.in")).unwrap(),
            "new input\n"
        );
        let recovery = fs::read_dir(&stress_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".stress-staging-")
            })
            .expect("late mutation must retain stress staging");
        assert_eq!(
            fs::read_to_string(recovery.join("previous").join("failed.in")).unwrap(),
            "old input\n"
        );
        assert_eq!(
            fs::read_to_string(recovery.join("previous").join("important.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[cfg(windows)]
    #[test]
    fn installed_failure_with_blocked_staging_cleanup_is_not_reported_as_saved() {
        use std::os::windows::fs::OpenOptionsExt;

        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let request = request(temp.path(), 10);
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new input\n".to_string(),
            expected: "new expected\n".to_string(),
            actual: "wrong\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let stress_root = temp.path().join(".atc/stress");
        let blocked_file = std::cell::RefCell::new(None);
        let result = persist_failure_with_hooks(
            &request,
            &failure,
            &|| false,
            || {},
            || {
                let staging = fs::read_dir(&stress_root)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".stress-staging-")
                    })
                    .unwrap();
                let blocker = staging.join("cleanup-blocker");
                fs::write(&blocker, "keep until return\n").unwrap();
                *blocked_file.borrow_mut() = Some(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(0)
                        .open(&blocker)
                        .unwrap(),
                );
            },
        );
        let error = result.expect_err("installed case with failed cleanup needs recovery review");
        assert!(error.to_string().contains("cleanup"));
        assert!(error.to_string().contains("installed"));
        assert!(error.to_string().contains(".stress-staging-"));
        assert_eq!(
            fs::read_to_string(stress_root.join("A/failed.in")).unwrap(),
            "new input\n"
        );
        assert!(fs::read_dir(&stress_root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".stress-staging-")
        }));
        assert!(load_saved_case(temp.path(), "abc123", "A").is_err());
        assert!(persist_failure(&request, &failure, &|| false).is_err());
        assert_eq!(
            fs::read_to_string(stress_root.join("A/failed.in")).unwrap(),
            "new input\n"
        );
        drop(blocked_file.take());
    }

    #[test]
    fn staging_cleanup_preserves_a_late_unowned_file() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let request = request(temp.path(), 10);
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new input\n".to_string(),
            expected: "new expected\n".to_string(),
            actual: "wrong\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let stress_root = temp.path().join(".atc/stress");
        let inserted = std::cell::RefCell::new(None);
        let result = persist_failure_with_cleanup_hook(
            &request,
            &failure,
            &|| false,
            || {},
            || {},
            || {
                let staging = fs::read_dir(&stress_root)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".stress-staging-")
                    })
                    .unwrap();
                let path = staging.join("user-note.txt");
                fs::write(&path, "keep me\n").unwrap();
                *inserted.borrow_mut() = Some(path);
            },
        );
        assert!(
            result.is_err(),
            "unknown staging content must prevent success"
        );
        let path = inserted.into_inner().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep me\n");
        assert_eq!(
            fs::read_to_string(stress_root.join("A/failed.in")).unwrap(),
            "new input\n"
        );
    }

    #[test]
    fn loader_and_writer_both_reject_a_generation_with_unowned_entries() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join(".atc").join("stress").join("A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "old input\n",
            Some("old expected\n"),
        );
        fs::write(target.join("important.txt"), "user data\n").unwrap();

        let error = load_saved_case(temp.path(), "abc123", "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unowned entry"));

        let request = request(temp.path(), 10);
        let replacement = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new input\n".to_string(),
            expected: "new expected\n".to_string(),
            actual: "new actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };

        let error = persist_failure(&request, &replacement, &|| false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(target.join("important.txt")).unwrap(),
            "user data\n"
        );
    }

    #[test]
    fn unusable_saved_generations_return_errors_without_modifying_files() {
        for damaged in ["input", "actual", "expected", "metadata", "unowned"] {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join(".atc/stress/A");
            write_v1_generation(
                &target,
                "abc123",
                "A",
                "wrong-answer",
                "input\n",
                Some("expected\n"),
            );
            match damaged {
                "input" => fs::remove_file(target.join("failed.in")).unwrap(),
                "actual" => fs::remove_file(target.join("actual.out")).unwrap(),
                "expected" => fs::remove_file(target.join("expected.out")).unwrap(),
                "metadata" => fs::write(target.join("meta.toml"), "invalid TOML = [").unwrap(),
                "unowned" => fs::write(target.join("important.txt"), "user data\n").unwrap(),
                _ => unreachable!(),
            }
            let before = fs::read_dir(&target)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<std::collections::BTreeMap<_, _>>();

            let error = load_saved_case(temp.path(), "abc123", "A").unwrap_err();
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidData,
                "{damaged}: {error}"
            );
            let after = fs::read_dir(&target)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(after, before, "{damaged}");
        }
    }

    #[test]
    fn absent_saved_case_is_distinct_from_a_valid_case_and_unknown_staging() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(load_saved_case(temp.path(), "abc123", "A").unwrap(), None);

        let target = temp.path().join(".atc/stress/A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "input\n",
            Some("expected\n"),
        );
        fs::write(target.join("stderr.txt"), "diagnostic\n").unwrap();
        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A").unwrap(),
            Some(Sample {
                input: "input\n".to_string(),
                output: "expected\n".to_string()
            })
        );
        let unknown = target.parent().unwrap().join(".stress-staging-old");
        fs::create_dir(&unknown).unwrap();
        let error = load_saved_case(temp.path(), "abc123", "A").unwrap_err();
        assert!(error.to_string().contains("recovery"));
        assert!(unknown.is_dir());
    }

    #[test]
    fn interrupted_replacement_with_previous_generation_is_not_absence() {
        let temp = tempfile::tempdir().unwrap();
        let stress_root = temp.path().join(".atc/stress");
        let target = stress_root.join("A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "old input\n",
            Some("old expected\n"),
        );
        let staging = stress_root.join(".stress-staging-interrupted");
        fs::create_dir(&staging).unwrap();
        fs::rename(&target, staging.join("previous")).unwrap();
        write_v1_generation(
            &stress_root.join("B"),
            "abc123",
            "B",
            "wrong-answer",
            "B input\n",
            Some("B expected\n"),
        );

        let error = load_saved_case(temp.path(), "abc123", "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("recovery"));
        assert_eq!(
            fs::read_to_string(staging.join("previous/failed.in")).unwrap(),
            "old input\n"
        );
        assert!(!target.exists());
        assert_eq!(
            load_saved_case(temp.path(), "abc123", "B").unwrap(),
            Some(Sample {
                input: "B input\n".to_string(),
                output: "B expected\n".to_string(),
            })
        );
    }

    #[test]
    fn unfinished_current_staging_only_blocks_its_named_problem() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join(".atc/stress/.stress-staging-1-B-empty");
        fs::create_dir_all(&staging).unwrap();

        assert_eq!(load_saved_case(temp.path(), "abc123", "A").unwrap(), None);
        let error = load_saved_case(temp.path(), "abc123", "B").unwrap_err();
        assert!(error.to_string().contains("recovery"));
        assert!(staging.is_dir());
    }

    #[test]
    fn malformed_current_staging_names_and_metadata_mismatch_fail_closed() {
        for name in [
            ".stress-staging-01-B-random",
            ".stress-staging-1-B-",
            ".stress-staging-1-BC-random",
            ".stress-staging-2-B-random",
        ] {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir_all(temp.path().join(".atc/stress").join(name)).unwrap();
            assert!(
                load_saved_case(temp.path(), "abc123", "A").is_err(),
                "{name}"
            );
        }

        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join(".atc/stress/.stress-staging-1-B-mismatch");
        write_v1_generation(
            &staging.join("new"),
            "abc123",
            "A",
            "wrong-answer",
            "old\n",
            Some("expected\n"),
        );
        assert!(load_saved_case(temp.path(), "abc123", "A").is_err());
        assert!(load_saved_case(temp.path(), "abc123", "B").is_err());
        assert!(staging.join("new/failed.in").is_file());

        for metadata in ["problem = \"A\"\n", "contest = \"other\"\n"] {
            let temp = tempfile::tempdir().unwrap();
            let staging = temp.path().join(".atc/stress/.stress-staging-1-B-partial");
            fs::create_dir_all(staging.join("new")).unwrap();
            fs::write(staging.join("new/meta.toml"), metadata).unwrap();
            assert!(load_saved_case(temp.path(), "abc123", "A").is_err());
            assert_eq!(
                fs::read_to_string(staging.join("new/meta.toml")).unwrap(),
                metadata
            );
        }
    }

    #[test]
    fn current_staging_owner_uses_exact_length_and_separator() {
        assert_eq!(
            current_staging_problem(".stress-staging-1-A-random"),
            Ok(Some("A"))
        );
        assert_eq!(
            current_staging_problem(".stress-staging-2-AA-random"),
            Ok(Some("AA"))
        );
        assert_eq!(
            current_staging_problem(".stress-staging-3-A-B-random"),
            Ok(Some("A-B"))
        );
        assert_eq!(
            current_staging_problem(".stress-staging-2-Å-random"),
            Ok(Some("Å"))
        );
        assert_eq!(
            current_staging_problem(".stress-staging-1-AA-random"),
            Err(())
        );
        assert_eq!(
            current_staging_problem(".stress-staging-2-A-random"),
            Err(())
        );
    }

    #[test]
    fn reading_a_does_not_wait_for_an_active_b_save() {
        let temp = tempfile::tempdir().unwrap();
        let stress_root = temp.path().join(".atc/stress");
        fs::create_dir_all(&stress_root).unwrap();
        let stress_directory = open_stress_directory(temp.path()).unwrap().unwrap();
        let writer_lock = open_stress_lock(&stress_directory, "B").unwrap();
        writer_lock.lock().unwrap();
        let staging = stress_root.join(".stress-staging-1-B-active");
        fs::create_dir(&staging).unwrap();
        // This can be a partially written metadata file while B owns the lock.
        fs::create_dir(staging.join("new")).unwrap();
        fs::write(staging.join("new/meta.toml"), "version =").unwrap();

        let started = std::time::Instant::now();
        assert_eq!(load_saved_case(temp.path(), "abc123", "A").unwrap(), None);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer_lock);
        assert_eq!(load_saved_case(temp.path(), "abc123", "A").unwrap(), None);
        let error = load_saved_case(temp.path(), "abc123", "B").unwrap_err();
        assert!(error.to_string().contains("recovery"));
        assert_eq!(
            fs::read_to_string(staging.join("new/meta.toml")).unwrap(),
            "version ="
        );
    }

    #[test]
    fn interrupted_first_install_and_post_install_cleanup_require_review() {
        for installed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let stress_root = temp.path().join(".atc/stress");
            fs::create_dir_all(&stress_root).unwrap();
            let staging = stress_root.join(if installed {
                ".stress-staging-legacy-interrupted"
            } else {
                ".stress-staging-1-A-interrupted"
            });
            fs::create_dir(&staging).unwrap();
            if installed {
                write_v1_generation(
                    &staging.join("previous"),
                    "abc123",
                    "A",
                    "wrong-answer",
                    "old\n",
                    Some("old expected\n"),
                );
                write_v1_generation(
                    &stress_root.join("A"),
                    "abc123",
                    "A",
                    "wrong-answer",
                    "new\n",
                    Some("new expected\n"),
                );
            } else {
                write_v1_generation(
                    &staging.join("new"),
                    "abc123",
                    "A",
                    "wrong-answer",
                    "new\n",
                    Some("new expected\n"),
                );
            }

            let error = load_saved_case(temp.path(), "abc123", "A").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("recovery"));
            assert_eq!(load_saved_case(temp.path(), "abc123", "B").unwrap(), None);
            let preserved = if installed {
                staging.join("previous/failed.in")
            } else {
                staging.join("new/failed.in")
            };
            assert_eq!(
                fs::read_to_string(preserved).unwrap(),
                if installed { "old\n" } else { "new\n" }
            );
        }
    }

    #[test]
    fn interrupted_replacement_blocks_a_new_stress_save_without_touching_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let stress_root = temp.path().join(".atc/stress");
        let target = stress_root.join("A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "old\n",
            Some("old expected\n"),
        );
        let staging = stress_root.join(".stress-staging-interrupted");
        fs::create_dir(&staging).unwrap();
        fs::rename(&target, staging.join("previous")).unwrap();
        let replacement = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "replacement\n".to_string(),
            expected: "replacement expected\n".to_string(),
            actual: "wrong\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };

        let error =
            persist_failure(&request(temp.path(), 10), &replacement, &|| false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("recovery"));
        assert_eq!(
            fs::read_to_string(staging.join("previous/failed.in")).unwrap(),
            "old\n"
        );
        assert!(!target.exists());
    }

    #[test]
    fn metadata_preserves_the_full_u64_seed_range() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let request = request(temp.path(), u64::MAX);
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: u64::MAX,
            seed: u64::MAX,
            input: "1\n".to_string(),
            expected: "2\n".to_string(),
            actual: "3\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };

        let target = persisted_path(persist_failure(&request, &failure, &|| false));
        validate_failure_generation(&target, &request).unwrap();

        let metadata = fs::read_to_string(target.join("meta.toml")).unwrap();
        assert!(metadata.contains("base_seed = 18446744073709551615"));
        assert!(metadata.contains("seed = 18446744073709551615"));
    }

    #[test]
    fn partial_generation_is_not_promoted() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join(".atc").join("stress").join("A");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("failed.in"), "1 2\n").unwrap();
        fs::write(target.join("expected.out"), "3\n").unwrap();

        let error = load_saved_case(temp.path(), "abc123", "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("missing meta.toml"));
    }

    #[test]
    fn saved_case_problem_index_cannot_escape_the_stress_directory() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".atc").join("stress")).unwrap();

        let error = load_saved_case(temp.path(), "abc123", "../outside").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn old_v1_wrong_answer_is_promoted_but_old_re_without_expected_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join(".atc").join("stress").join("A");
        write_v1_generation(&target, "abc123", "A", "wrong-answer", "1 2\n", Some("3\n"));

        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A").unwrap(),
            Some(Sample {
                input: "1 2\n".to_string(),
                output: "3\n".to_string(),
            })
        );

        fs::remove_dir_all(&target).unwrap();
        write_v1_generation(&target, "abc123", "A", "runtime-error", "4\n", None);
        assert_eq!(load_saved_case(temp.path(), "abc123", "A").unwrap(), None);
    }

    #[test]
    fn only_legacy_v1_runtime_failures_may_omit_expected_output() {
        for kind in ["runtime-error", "timed-out"] {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join(".atc/stress/A");
            write_v1_generation(&target, "abc123", "A", kind, "input\n", None);
            assert!(matches!(
                load_saved_case_status(temp.path(), "abc123", "A").unwrap(),
                SavedCaseLoad::AmbiguousV1WithoutExpected { kind: loaded_kind } if loaded_kind == kind
            ));
            assert!(!target.join("expected.out").exists());
        }

        for (version, kind) in [
            (1, "wrong-answer"),
            (2, "wrong-answer"),
            (2, "runtime-error"),
            (2, "timed-out"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join(".atc/stress/A");
            write_v1_generation(&target, "abc123", "A", kind, "input\n", None);
            if version == 2 {
                let metadata = fs::read_to_string(target.join("meta.toml")).unwrap();
                fs::write(
                    target.join("meta.toml"),
                    metadata.replace("version = 1", "version = 2"),
                )
                .unwrap();
            }
            let before = fs::read(target.join("meta.toml")).unwrap();
            let error = load_saved_case_status(temp.path(), "abc123", "A")
                .err()
                .expect("missing expected output must be rejected");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("expected.out"));
            assert_eq!(fs::read(target.join("meta.toml")).unwrap(), before);
        }
    }

    #[test]
    fn v1_wrong_answer_and_all_v2_failure_kinds_replay_with_expected_output() {
        for (version, kind) in [
            (1, "wrong-answer"),
            (2, "wrong-answer"),
            (2, "runtime-error"),
            (2, "timed-out"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join(".atc/stress/A");
            write_v1_generation(&target, "abc123", "A", kind, "input\n", Some("expected\n"));
            if version == 2 {
                let metadata = fs::read_to_string(target.join("meta.toml")).unwrap();
                fs::write(
                    target.join("meta.toml"),
                    metadata.replace("version = 1", "version = 2"),
                )
                .unwrap();
            }
            assert!(matches!(
                load_saved_case_status(temp.path(), "abc123", "A").unwrap(),
                SavedCaseLoad::Replay(Sample { input, output })
                    if input == "input\n" && output == "expected\n"
            ));
        }
    }

    #[test]
    fn malformed_or_unknown_metadata_cannot_be_treated_as_legacy_runtime_failure() {
        for replacement in ["version = 9", "version = 0", "version = \"1\""] {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join(".atc/stress/A");
            write_v1_generation(&target, "abc123", "A", "runtime-error", "input\n", None);
            let metadata = fs::read_to_string(target.join("meta.toml")).unwrap();
            fs::write(
                target.join("meta.toml"),
                metadata.replace("version = 1", replacement),
            )
            .unwrap();
            assert!(load_saved_case_status(temp.path(), "abc123", "A").is_err());
        }

        for version in [1, 2] {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join(".atc/stress/A");
            write_v1_generation(&target, "abc123", "A", "runtime-error", "input\n", None);
            let metadata = fs::read_to_string(target.join("meta.toml")).unwrap();
            let metadata = metadata
                .replace("version = 1", &format!("version = {version}"))
                .replace("\nseed = 10", "\nseed = 11");
            fs::write(target.join("meta.toml"), &metadata).unwrap();
            let error = load_saved_case_status(temp.path(), "abc123", "A")
                .err()
                .expect("inconsistent metadata must be rejected");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(!target.join("expected.out").exists());
            assert_eq!(
                fs::read_to_string(target.join("meta.toml")).unwrap(),
                metadata
            );
        }
    }

    #[test]
    fn replacing_legacy_v1_runtime_failure_keeps_its_unreplayable_generation() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let target = temp.path().join(".atc/stress/A");
        write_v1_generation(&target, "abc123", "A", "runtime-error", "old\n", None);
        let old_metadata = fs::read(target.join("meta.toml")).unwrap();
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "new\n".to_string(),
            expected: "answer\n".to_string(),
            actual: "wrong\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        persisted_path(persist_failure(
            &request(temp.path(), 10),
            &failure,
            &|| false,
        ));
        let recovery = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".stress-recovery-")
                    && path.join("previous/meta.toml").exists()
            })
            .unwrap();
        assert_eq!(
            fs::read(recovery.join("previous/meta.toml")).unwrap(),
            old_metadata
        );
        assert_eq!(
            fs::read_to_string(recovery.join("previous/failed.in")).unwrap(),
            "old\n"
        );
        assert!(!recovery.join("previous/expected.out").exists());
    }

    #[test]
    fn saved_case_metadata_must_match_the_current_contest_and_problem() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join(".atc").join("stress").join("A");
        write_v1_generation(
            &target,
            "other-contest",
            "A",
            "wrong-answer",
            "1\n",
            Some("2\n"),
        );

        let error = load_saved_case(temp.path(), "abc123", "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("does not match"));

        fs::write(
            target.join("meta.toml"),
            "version = 1\ncontest = \"abc123\"\nproblem = \"B\"\nkind = \"wrong-answer\"\ncase = 1\nbase_seed = 10\nseed = 10\n",
        )
        .unwrap();
        let error = load_saved_case(temp.path(), "abc123", "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn loader_waits_for_generation_replacement_and_reads_only_the_new_generation() {
        let temp = tempfile::tempdir().unwrap();
        let stress_root = temp.path().join(".atc").join("stress");
        let target = stress_root.join("A");
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "old input\n",
            Some("old expected\n"),
        );

        let stress_directory = open_stress_directory(temp.path()).unwrap().unwrap();
        let writer_lock = open_stress_lock(&stress_directory, "A").unwrap();
        writer_lock.lock().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let loader = scope.spawn(|| {
                load_saved_case_with_hook(temp.path(), "abc123", "A", || {
                    ready_tx.send(()).unwrap();
                })
                .unwrap()
            });
            ready_rx.recv().unwrap();

            let previous = stress_root.join("previous");
            fs::rename(&target, &previous).unwrap();
            write_v1_generation(
                &target,
                "abc123",
                "A",
                "wrong-answer",
                "new input\n",
                Some("new expected\n"),
            );
            drop(writer_lock);

            assert_eq!(
                loader.join().unwrap(),
                Some(Sample {
                    input: "new input\n".to_string(),
                    output: "new expected\n".to_string(),
                })
            );
        });
    }

    #[test]
    fn first_save_lock_creation_cannot_expose_active_staging_to_loader() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().to_path_buf();
        let stress_root = destination.join(".atc/stress");
        fs::create_dir_all(&stress_root).unwrap();
        assert!(!stress_root.join(".A.lock").exists());
        let writer = std::cell::RefCell::new(None);

        let loaded = load_saved_case_with_hook(&destination, "abc123", "A", || {
            let writer_destination = destination.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let handle = std::thread::spawn(move || {
                let stress_root = writer_destination.join(".atc/stress");
                let stress_directory = open_stress_directory(&writer_destination).unwrap().unwrap();
                let lock = open_stress_lock(&stress_directory, "A").unwrap();
                lock.lock().unwrap();
                let staging = stress_root.join(".stress-staging-1-A-active");
                fs::create_dir(&staging).unwrap();
                ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(200));
                write_v1_generation(
                    &stress_root.join("A"),
                    "abc123",
                    "A",
                    "wrong-answer",
                    "new input\n",
                    Some("new expected\n"),
                );
                fs::remove_dir(&staging).unwrap();
                drop(lock);
            });
            ready_rx.recv().unwrap();
            *writer.borrow_mut() = Some(handle);
        });
        writer.into_inner().unwrap().join().unwrap();
        assert_eq!(
            loaded.unwrap(),
            Some(Sample {
                input: "new input\n".to_string(),
                output: "new expected\n".to_string(),
            })
        );
        assert!(stress_root.join(".A.lock").exists());
    }

    #[test]
    fn lock_open_failure_never_falls_back_to_unlocked_read_or_write() {
        let temp = tempfile::tempdir().unwrap();
        let stress_root = temp.path().join(".atc/stress");
        fs::create_dir_all(stress_root.join(".A.lock")).unwrap();
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "input\n".to_string(),
            expected: "expected\n".to_string(),
            actual: "actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };

        assert!(load_saved_case(temp.path(), "abc123", "A").is_err());
        assert!(persist_failure(&request(temp.path(), 10), &failure, &|| false).is_err());
        assert!(!stress_root.join("A").exists());
        assert_eq!(load_saved_case(temp.path(), "abc123", "B").unwrap(), None);
        assert!(stress_root.join(".A.lock").is_dir());
    }

    #[test]
    fn same_problem_saves_serialize_while_another_problem_can_save() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".atc")).unwrap();
        let first_request = request(temp.path(), 10);
        let second_request = first_request.clone();
        let mut other_request = first_request.clone();
        other_request.problem_index = "B".to_string();
        let failure = StressFailure {
            kind: CandidateFailureKind::WrongAnswer,
            case_number: 1,
            base_seed: 10,
            seed: 10,
            input: "first\n".to_string(),
            expected: "expected\n".to_string(),
            actual: "actual\n".to_string(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        };
        let mut second_failure = failure.clone();
        second_failure.input = "second\n".to_string();
        let mut other_failure = failure.clone();
        other_failure.input = "other\n".to_string();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let first = scope.spawn(move || {
                persist_failure_with_hooks(
                    &first_request,
                    &failure,
                    &|| false,
                    || {
                        ready_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    },
                    || {},
                )
                .unwrap()
            });
            ready_rx.recv().unwrap();
            let second = scope.spawn(move || {
                persist_failure(&second_request, &second_failure, &|| false).unwrap()
            });
            let other = persisted_path(persist_failure(&other_request, &other_failure, &|| false));
            assert_eq!(
                fs::read_to_string(other.join("failed.in")).unwrap(),
                "other\n"
            );
            release_tx.send(()).unwrap();
            assert!(matches!(
                first.join().unwrap(),
                PersistenceOutcome::Saved(_)
            ));
            assert!(matches!(
                second.join().unwrap(),
                PersistenceOutcome::Saved(_)
            ));
        });

        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A")
                .unwrap()
                .unwrap()
                .input,
            "second\n"
        );
        assert_eq!(
            load_saved_case(temp.path(), "abc123", "B")
                .unwrap()
                .unwrap()
                .input,
            "other\n"
        );
    }
}
