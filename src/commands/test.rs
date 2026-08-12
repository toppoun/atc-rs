use super::resolve_language;
use crate::comparator::{self, ComparisonResult};
use crate::config::{Config, RunnerConfig};
use crate::error::AppError;
use crate::language::Language;
use crate::model::{Contest, Problem, Sample};
use crate::runner::{self, ExecutionOutcome};
use crate::ui::{Event, Reporter};
use crate::workspace;
use std::io;
use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
enum CaseVerdict {
    Accepted,
    WrongAnswer,
    RuntimeError,
    TimedOut,
}

fn judge_case(sample: &Sample, result: &runner::ExecutionResult) -> CaseVerdict {
    match &result.outcome {
        ExecutionOutcome::TimedOut => CaseVerdict::TimedOut,

        ExecutionOutcome::Exited(status) if !status.success() => CaseVerdict::RuntimeError,

        ExecutionOutcome::Exited(_) => match comparator::compare(&sample.output, &result.stdout) {
            ComparisonResult::Accepted => CaseVerdict::Accepted,
            ComparisonResult::WrongAnswer => CaseVerdict::WrongAnswer,
        },
    }
}

pub(super) fn report_case_result(
    number: usize,
    sample: &Sample,
    result: &runner::ExecutionResult,
    reporter: &mut dyn Reporter,
) {
    match judge_case(sample, result) {
        CaseVerdict::Accepted => {
            reporter.report(Event::TestCaseAccepted {
                number,
                elapsed: result.elapsed,
            });
        }

        CaseVerdict::WrongAnswer => {
            reporter.report(Event::TestCaseWrongAnswer {
                number,
                expected: &sample.output,
                actual: &result.stdout,
                elapsed: result.elapsed,
            });
        }

        CaseVerdict::RuntimeError => {
            reporter.report(Event::TestCaseRuntimeError {
                number,
                elapsed: result.elapsed,
            });
        }

        CaseVerdict::TimedOut => {
            reporter.report(Event::TestCaseTimedOut {
                number,
                elapsed: result.elapsed,
            });
        }
    }
    if !result.stderr.is_empty() {
        reporter.report(Event::TestCaseStderr {
            number,
            stderr: &result.stderr,
        });
    }
}

pub(super) fn run_test_cases(
    samples: &[Sample],
    mut execute_case: impl FnMut(&Sample) -> io::Result<runner::ExecutionResult>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    for (index, sample) in samples.iter().enumerate() {
        let result = execute_case(sample)?;
        report_case_result(index + 1, sample, &result, reporter);
    }

    Ok(())
}

pub(super) fn report_compile_result(
    result: &runner::ExecutionResult,
    reporter: &mut dyn Reporter,
) -> bool {
    match &result.outcome {
        ExecutionOutcome::Exited(status) if status.success() => true,
        ExecutionOutcome::Exited(_) => {
            reporter.report(Event::CompileFailed {
                stderr: &result.stderr,
            });
            false
        }
        ExecutionOutcome::TimedOut => {
            reporter.report(Event::CompileTimedOut {
                elapsed: result.elapsed,
            });
            false
        }
    }
}

pub(super) fn find_problem<'a>(
    contest: &'a Contest,
    problem_index: &str,
) -> io::Result<&'a Problem> {
    let mut matches = contest
        .problems
        .iter()
        .filter(|problem| problem.index.eq_ignore_ascii_case(problem_index));
    let problem = matches.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("problem not found in this contest: {problem_index}"),
        )
    })?;

    if matches.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ambiguous problem index in contest metadata: {problem_index}"),
        ));
    }

    Ok(problem)
}

pub(super) fn validate_debug_language(language: Language, debug: bool) -> io::Result<()> {
    if debug && language != Language::Cpp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--debug is only supported for C++",
        ));
    }

    Ok(())
}

pub(crate) fn test(
    problem_index: &str,
    cli_language: Option<Language>,
    debug: bool,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    // contest directoryとして正しいか
    workspace::validate_workspace_marker(&cwd)?;

    // このcontestに本当にそのproblemがあるか
    let contest = workspace::load_metadata(&cwd)?;
    workspace::validate_contest_paths(&contest)?;

    let problem = find_problem(&contest, problem_index)?;

    let config = Config::load()?;
    let language = resolve_language(cli_language, &config);
    validate_debug_language(language, debug)?;
    test_problem(&cwd, problem, language, &config.runner, debug, reporter)
}

pub(super) fn test_problem(
    destination: &Path,
    problem: &Problem,
    language: Language,
    runner_config: &RunnerConfig,
    debug: bool,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    test_problem_with_debug_header(
        destination,
        problem,
        language,
        runner_config,
        debug,
        reporter,
        crate::debug::materialize_debug_header,
    )
}

pub(super) fn test_problem_with_debug_header(
    destination: &Path,
    problem: &Problem,
    language: Language,
    runner_config: &RunnerConfig,
    debug: bool,
    reporter: &mut dyn Reporter,
    materialize_debug_header: impl FnOnce() -> Result<std::path::PathBuf, AppError>,
) -> Result<(), AppError> {
    let source = destination.join(format!("{}.{}", problem.index, language.extension()));

    if !source.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("source file not found: {}", source.display()),
        )
        .into());
    }

    let samples = workspace::load_samples(destination, &problem.index)?;
    if samples.is_empty() {
        reporter.report(Event::NoSamples {
            problem_index: &problem.index,
        });
        return Ok(());
    }

    let timeout = duration_from_seconds(runner_config.timeout_seconds, "runner.timeout_seconds")?;
    let compile_timeout = duration_from_seconds(
        runner_config.compile_timeout_seconds,
        "runner.compile_timeout_seconds",
    )?;

    match language {
        Language::Python => {
            run_test_cases(
                &samples,
                |sample| {
                    runner::execute_python(&source, &sample.input, &runner_config.python, timeout)
                },
                reporter,
            )?;
        }

        Language::Cpp => {
            let build_dir = tempfile::tempdir()?;

            let output =
                build_dir
                    .path()
                    .join(format!("{}{}", problem.index, std::env::consts::EXE_SUFFIX));
            let build_options = if debug {
                runner::BuildOptions {
                    debug_include_dir: Some(materialize_debug_header()?),
                }
            } else {
                runner::BuildOptions::default()
            };
            let compile_result = runner::compile_cpp(
                &source,
                &output,
                &runner_config.cpp_compiler,
                &runner_config.cpp_flags,
                compile_timeout,
                &build_options,
            )?;

            if !report_compile_result(&compile_result, reporter) {
                return Ok(());
            }

            run_test_cases(
                &samples,
                |sample| runner::execute(&output, &[], &sample.input, timeout),
                reporter,
            )?;
        }
    }

    Ok(())
}

fn duration_from_seconds(seconds: f64, name: &str) -> io::Result<Duration> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} must be a positive finite duration"),
        ));
    }

    let duration = Duration::try_from_secs_f64(seconds).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {name}: {error}"),
        )
    })?;

    if duration.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} is too small to represent as a positive duration"),
        ));
    }

    Ok(duration)
}
