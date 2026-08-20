use std::io;
use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use super::resolve_language;
use super::test::{find_problem, validate_debug_language};
use crate::config::{Config, RunnerConfig};
use crate::error::AppError;
use crate::language::Language;
use crate::model::Problem;
use crate::stress::{self, StressRequest};
use crate::ui::Reporter;
use crate::workspace;

const DEFAULT_STRESS_COUNT: NonZeroU64 = NonZeroU64::new(100).unwrap();

pub(crate) fn stress(
    problem_index: &str,
    cli_contest: Option<&str>,
    cli_language: Option<Language>,
    debug: bool,
    count: Option<NonZeroU64>,
    forever: bool,
    seed: Option<u64>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::resolve_contest_target(&cwd, cli_contest)?;

    workspace::validate_workspace_marker(&destination)?;

    let contest = workspace::load_metadata(&destination)?;
    workspace::validate_contest_paths(&contest)?;
    if let Some(contest_id) = cli_contest {
        workspace::validate_contest_identity(&contest, contest_id)?;
    }

    let problem = find_problem(&contest, problem_index)?;
    let config = Config::load()?;
    let language = resolve_language(cli_language, &config);

    let base_seed = match seed {
        Some(seed) => seed,
        None => stress::automatic_seed()?,
    };
    let count = if forever {
        None
    } else {
        Some(count.unwrap_or(DEFAULT_STRESS_COUNT))
    };

    let request = build_request(
        &destination,
        &contest.contest_id,
        problem,
        language,
        &config.runner,
        debug,
        base_seed,
        count,
    )?;

    let _ = stress::run(&request, reporter, &|| false)?;
    Ok(())
}

pub(super) fn build_request(
    destination: &Path,
    contest_id: &str,
    problem: &Problem,
    language: Language,
    runner: &RunnerConfig,
    debug: bool,
    base_seed: u64,
    count: Option<NonZeroU64>,
) -> Result<StressRequest, AppError> {
    validate_debug_language(language, debug)?;

    let candidate_source = destination.join(format!("{}.{}", problem.index, language.extension()));
    let generator_source = destination.join(format!("{}_gen.py", problem.index));
    let reference_source = destination.join(format!("{}_brute.py", problem.index));

    require_file(&candidate_source, "candidate source")?;
    require_file(&generator_source, "stress generator")?;
    require_file(&reference_source, "stress reference")?;

    let timeout = duration_from_seconds(runner.timeout_seconds, "runner.timeout_seconds")?;
    let compile_timeout = duration_from_seconds(
        runner.compile_timeout_seconds,
        "runner.compile_timeout_seconds",
    )?;

    Ok(StressRequest {
        destination: destination.to_path_buf(),
        contest_id: contest_id.to_string(),
        problem_index: problem.index.clone(),
        candidate_source,
        candidate_language: language,
        generator_source,
        reference_source,
        python: runner.python.clone(),
        cpp_compiler: runner.cpp_compiler.clone(),
        cpp_flags: runner.cpp_flags.clone(),
        candidate_timeout: timeout,
        generator_timeout: timeout,
        reference_timeout: timeout,
        compile_timeout,
        base_seed,
        count,
        debug,
    })
}

fn require_file(path: &Path, kind: &str) -> io::Result<()> {
    if path.is_file() {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{kind} not found: {}", path.display()),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_count_is_finite_and_nonzero() {
        assert_eq!(DEFAULT_STRESS_COUNT.get(), 100);
    }

    #[test]
    fn missing_required_file_is_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let error = require_file(&temp.path().join("missing.py"), "stress generator").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("stress generator not found"));
    }
}
