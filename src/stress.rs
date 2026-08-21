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
use crate::ui::{Event, Reporter};
use crate::workspace;

const FAILURE_FORMAT_VERSION: u32 = 1;
const FAILURE_GENERATION_FILES: [&str; 5] = [
    "failed.in",
    "actual.out",
    "expected.out",
    "stderr.txt",
    "meta.toml",
];
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn automatic_seed() -> io::Result<u64> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|error| {
        io::Error::other(format!("system clock is before UNIX epoch: {error}"))
    })?;

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
    ReferenceRuntimeError { seed: u64, stderr: String },
    ReferenceTimedOut { seed: u64, elapsed: Duration },
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
                write!(formatter, "candidate compilation timed out after {elapsed:.2?}")
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

                let result = match runner::compile_cpp_with_cancel(
                    &request.candidate_source,
                    &executable,
                    &request.cpp_compiler,
                    &request.cpp_flags,
                    request.compile_timeout,
                    &build_options,
                    is_cancelled,
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
            Self::Python { source } => runner::execute_python_with_cancel(
                source,
                input,
                &request.python,
                request.candidate_timeout,
                is_cancelled,
            ),
            Self::Cpp { executable, .. } => runner::execute_with_cancel(
                executable,
                &[],
                input,
                request.candidate_timeout,
                is_cancelled,
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

    let result = match process_step(runner::execute_with_cancel(
        Path::new(&request.python),
        &args,
        "",
        request.generator_timeout,
        is_cancelled,
    ))? {
        ProcessStep::Completed(result) => result,
        ProcessStep::Cancelled => return Ok(ProcessStep::Cancelled),
    };

    match result.outcome {
        ExecutionOutcome::Exited(status) if status.success() => {
            Ok(ProcessStep::Completed(result.stdout))
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
    let result = match process_step(runner::execute_python_with_cancel(
        &request.reference_source,
        input,
        &request.python,
        request.reference_timeout,
        is_cancelled,
    ))? {
        ProcessStep::Completed(result) => result,
        ProcessStep::Cancelled => return Ok(ProcessStep::Cancelled),
    };

    match result.outcome {
        ExecutionOutcome::Exited(status) if status.success() => {
            Ok(ProcessStep::Completed(result.stdout))
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
    let generation_lock = open_stress_lock(&stress_directory, &request.problem_index, true)?
        .expect("create was requested for the stress generation lock");

    let target = stress_root.join(&request.problem_index);
    let target_exists = existing_real_directory(&target, "stress failure directory")?;
    if target_exists {
        validate_failure_generation(&target, request)?;
    }

    if is_cancelled() {
        return Ok(PersistenceOutcome::Cancelled);
    }

    let staging = tempfile::Builder::new()
        .prefix(".stress-staging-")
        .tempdir_in(&stress_root)?;
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

    // Staging is harmless to discard. Once the old generation is moved, cancellation can no
    // longer win without either hiding a committed failure or requiring a destructive rollback.
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

    // destructive swap直前に再検査する。symlinkやfileへ差し替わっていたら止める。
    let target_still_exists = existing_real_directory(&target, "stress failure directory")?;
    if target_still_exists != target_exists {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "stress failure destination changed while preparing update: {}",
                target.display()
            ),
        ));
    }

    if target_exists {
        fs::rename(&target, &previous_generation)?;
        if let Err(error) = validate_failure_generation(&previous_generation, request) {
            return Err(rollback_previous_generation(
                staging,
                &target,
                &previous_generation,
                error,
            ));
        }
    }

    if let Err(error) = fs::rename(&new_generation, &target) {
        if target_exists {
            return Err(rollback_previous_generation(
                staging,
                &target,
                &previous_generation,
                error,
            ));
        }

        return Err(error);
    }

    Ok(PersistenceOutcome::Saved(target))
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

        if !FAILURE_GENERATION_FILES.contains(&name) || !entry.file_type()?.is_file() {
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

    if metadata.version != FAILURE_FORMAT_VERSION
        || metadata.contest != request.contest_id
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
        // Historical v1 RE/TLE generations did not include expected.out.
        "runtime-error" | "timed-out" => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown stress failure kind in {}", metadata_path.display()),
            ));
        }
    }

    Ok(())
}

fn valid_failure_metadata(
    metadata: &StoredFailureMetadata,
    contest_id: &str,
    problem_index: &str,
) -> bool {
    metadata.version == FAILURE_FORMAT_VERSION
        && metadata.contest == contest_id
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
    staging: tempfile::TempDir,
    target: &Path,
    previous_generation: &Path,
    original: io::Error,
) -> io::Error {
    if let Err(rollback_error) = fs::rename(previous_generation, target) {
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

    original
}

pub(crate) fn load_saved_case(
    destination: &Path,
    contest_id: &str,
    problem_index: &str,
) -> io::Result<Option<Sample>> {
    load_saved_case_with_hook(destination, contest_id, problem_index, || {})
}

fn load_saved_case_with_hook(
    destination: &Path,
    contest_id: &str,
    problem_index: &str,
    before_shared_lock: impl FnOnce(),
) -> io::Result<Option<Sample>> {
    workspace::validate_problem_index(problem_index)?;

    let Some(stress_directory) = open_stress_directory(destination)? else {
        return Ok(None);
    };

    let generation_lock = open_stress_lock(&stress_directory, problem_index, false)?;
    before_shared_lock();
    if let Some(lock) = generation_lock.as_ref() {
        lock.lock_shared()?;
    }

    let Some(generation) = open_optional_directory(&stress_directory, problem_index)? else {
        return Ok(None);
    };

    let Some(snapshot) = open_saved_case_snapshot(&generation)? else {
        return Ok(None);
    };

    // Keep the shared lock through the reads. On Windows, no-follow file handles intentionally
    // block a directory rename, so releasing the lock before closing them would make the writer's
    // generation swap fail instead of merely waiting for this coherent read to finish.
    drop(generation);
    let result = read_saved_case_snapshot(snapshot, contest_id, problem_index);
    drop(generation_lock);
    result
}

struct SavedCaseSnapshot {
    metadata: fs::File,
    _actual: fs::File,
    input: fs::File,
    expected: fs::File,
}

fn open_saved_case_snapshot(generation: &CapDir) -> io::Result<Option<SavedCaseSnapshot>> {
    if !saved_generation_entries_are_owned(generation)? {
        return Ok(None);
    }

    let Some(metadata) = open_regular_file(generation, "meta.toml")? else {
        return Ok(None);
    };
    let Some(actual) = open_regular_file(generation, "actual.out")? else {
        return Ok(None);
    };
    let Some(input) = open_regular_file(generation, "failed.in")? else {
        return Ok(None);
    };
    let Some(expected) = open_regular_file(generation, "expected.out")? else {
        // Historical v1 RE/TLE generations did not have expected.out and cannot be promoted.
        return Ok(None);
    };

    Ok(Some(SavedCaseSnapshot {
        metadata,
        _actual: actual,
        input,
        expected,
    }))
}

fn saved_generation_entries_are_owned(generation: &CapDir) -> io::Result<bool> {
    for entry in generation.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Ok(false);
        };

        if !FAILURE_GENERATION_FILES.contains(&name) || !entry.file_type()?.is_file() {
            return Ok(false);
        }
    }

    Ok(true)
}

fn read_saved_case_snapshot(
    mut snapshot: SavedCaseSnapshot,
    contest_id: &str,
    problem_index: &str,
) -> io::Result<Option<Sample>> {
    let Some(metadata_text) = read_utf8_or_ignore(&mut snapshot.metadata)? else {
        return Ok(None);
    };
    let Ok(metadata) = toml::from_str::<StoredFailureMetadata>(&metadata_text) else {
        return Ok(None);
    };
    if !valid_failure_metadata(&metadata, contest_id, problem_index) {
        return Ok(None);
    }

    let Some(input) = read_utf8_or_ignore(&mut snapshot.input)? else {
        return Ok(None);
    };
    let Some(expected) = read_utf8_or_ignore(&mut snapshot.expected)? else {
        return Ok(None);
    };

    Ok(Some(Sample {
        input,
        output: expected,
    }))
}

fn read_utf8_or_ignore(file: &mut fs::File) -> io::Result<Option<String>> {
    let mut content = String::new();
    match file.read_to_string(&mut content) {
        Ok(_) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(None),
        Err(error) => Err(error),
    }
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

fn open_stress_lock(
    stress_directory: &CapDir,
    problem_index: &str,
    create: bool,
) -> io::Result<Option<fs::File>> {
    let name = format!(".{problem_index}.lock");
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    if create {
        options.write(true).create(true).truncate(false);
    }

    let file = match stress_directory.open_with(&name, &options) {
        Ok(file) => file,
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stress generation lock is not a regular file: {name}"),
        ));
    }

    Ok(Some(file.into_std()))
}

fn ensure_real_directory(path: &Path, kind: &str) -> io::Result<()> {
    match fs::symlink_metadata(path) {
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
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a real file: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
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
        assert!(metadata.contains("kind = \"runtime-error\""));
        assert!(metadata.contains("seed = 11"));
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

        assert_eq!(load_saved_case(temp.path(), "abc123", "A").unwrap(), None);

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

        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A").unwrap(),
            None
        );
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
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "wrong-answer",
            "1 2\n",
            Some("3\n"),
        );

        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A").unwrap(),
            Some(Sample {
                input: "1 2\n".to_string(),
                output: "3\n".to_string(),
            })
        );

        fs::remove_dir_all(&target).unwrap();
        write_v1_generation(
            &target,
            "abc123",
            "A",
            "runtime-error",
            "4\n",
            None,
        );
        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A").unwrap(),
            None
        );
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

        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A").unwrap(),
            None
        );

        fs::write(
            target.join("meta.toml"),
            "version = 1\ncontest = \"abc123\"\nproblem = \"B\"\nkind = \"wrong-answer\"\ncase = 1\nbase_seed = 10\nseed = 10\n",
        )
        .unwrap();
        assert_eq!(
            load_saved_case(temp.path(), "abc123", "A").unwrap(),
            None
        );
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
        let writer_lock = open_stress_lock(&stress_directory, "A", true)
            .unwrap()
            .unwrap();
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
}
