use crate::atcoder;
use crate::comparator::{self, ComparisonResult};
use crate::config::{Config, RunnerConfig};
use crate::error::AppError;
use crate::language::Language;
use crate::model::{Contest, Sample};
use crate::runner::{self, ExecutionOutcome};
use crate::template::builtin_template;
use crate::ui::{Event, Reporter};
use crate::workspace;
use crate::workspace::validate_refresh_destination;
use std::io;
use std::path::Path;
use std::time::Duration;

struct FetchedContestData {
    contest: Contest,
    samples_by_problem: Vec<Option<Vec<Sample>>>,
}

pub fn new(
    contest_id: &str,
    cli_language: Option<Language>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::contest_path(&cwd, contest_id)?;

    if existing_contest_is_noop(&destination)? {
        return Ok(());
    }
    let config = Config::load()?;
    let language = resolve_language(cli_language, &config);

    let atcoder = if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)
    } else {
        atcoder::AtCoderClient::new()?
    };

    new_at(&destination, contest_id, language, &atcoder, reporter)
}

fn new_at(
    destination: &Path,
    contest_id: &str,
    language: Language,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    if existing_contest_is_noop(destination)? {
        return Ok(());
    }

    let template = builtin_template(language);

    let FetchedContestData {
        contest,
        samples_by_problem,
    } = fetch_contest_data(contest_id, atcoder, reporter)?;

    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "contest destination has no parent directory: {}",
                destination.display()
            ),
        )
    })?;
    let staging = tempfile::Builder::new()
        .prefix(".atc-new-")
        .tempdir_in(parent)?;

    workspace::create_source_files(staging.path(), &contest.problems, language, template)?;
    for (problem, samples) in contest.problems.iter().zip(samples_by_problem) {
        if let Some(samples) = samples {
            workspace::save_samples(staging.path(), problem, &samples)?;
        }
    }
    workspace::save_metadata(staging.path(), &contest)?;

    // Another process may have created the contest while fixtures/HTTP were read.
    if existing_contest_is_noop(destination)? {
        return Ok(());
    }

    match std::fs::rename(staging.path(), destination) {
        Ok(()) => {
            drop(staging.keep());
            reporter.report(Event::WorkspaceCreated { destination });
            Ok(())
        }
        Err(_) if destination.is_dir() => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn existing_contest_is_noop(destination: &Path) -> std::io::Result<bool> {
    if !destination.try_exists()? {
        return Ok(false);
    }

    if destination.is_dir() {
        return Ok(true);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "contest destination exists but is not a directory: {}",
            destination.display()
        ),
    ))
}

fn fetch_contest_data(
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<FetchedContestData, AppError> {
    reporter.report(Event::ContestFetching { contest_id });

    let contest = atcoder.fetch_contest(contest_id)?;

    reporter.report(Event::ContestFetched {
        contest_id: &contest.contest_id,
        problems: contest.problems.len(),
    });

    let total = contest.problems.len();
    let mut samples_by_problem = Vec::with_capacity(total);

    for (i, problem) in contest.problems.iter().enumerate() {
        reporter.report(Event::ProblemFetching {
            index: &problem.index,
            current: i + 1,
            total,
        });
        match atcoder.fetch_samples(problem) {
            Ok(samples) => {
                reporter.report(Event::ProblemFetched {
                    index: &problem.index,
                    samples: samples.len(),
                });
                samples_by_problem.push(Some(samples));
            }

            Err(err) => {
                let message = err.to_string();

                reporter.report(Event::ProblemFetchFailed {
                    index: &problem.index,
                    error: &message,
                });
                samples_by_problem.push(None);
            }
        }
    }
    Ok(FetchedContestData {
        contest,
        samples_by_problem,
    })
}

// ----- Refresh -----
pub fn refresh(contest: Option<String>, reporter: &mut dyn Reporter) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let contest_id = resolve_refresh_contest_id(&cwd, contest.as_deref())?;

    let atcoder = if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)
    } else {
        atcoder::AtCoderClient::new()?
    };

    refresh_at(&cwd, &contest_id, &atcoder, reporter)
}

fn resolve_refresh_contest_id(
    destination: &Path,
    specified_contest_id: Option<&str>,
) -> Result<String, AppError> {
    match specified_contest_id {
        Some(contest_id) => {
            validate_refresh_destination(destination, contest_id)?;
            Ok(contest_id.to_string())
        }
        None => {
            workspace::validate_workspace_marker(destination)?;
            Ok(workspace::load_metadata(destination)?.contest_id)
        }
    }
}

fn refresh_at(
    destination: &Path,
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    workspace::validate_workspace_marker(destination)?;

    let FetchedContestData {
        contest,
        samples_by_problem,
    } = fetch_contest_data(contest_id, atcoder, reporter)?;
    workspace::validate_contest_paths(&contest)?;

    let staging = tempfile::Builder::new()
        .prefix(".atc-refresh-")
        .tempdir_in(destination)?;

    for (problem, samples) in contest.problems.iter().zip(samples_by_problem) {
        if let Some(samples) = samples {
            workspace::save_samples(staging.path(), problem, &samples)?;
        }
    }
    workspace::save_metadata(staging.path(), &contest)?;

    workspace::replace_refresh_data(destination, staging)?;
    reporter.report(Event::WorkspaceRefreshed { destination });

    Ok(())
}

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

fn report_case_result(
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

fn run_test_cases(
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

fn report_compile_result(result: &runner::ExecutionResult, reporter: &mut dyn Reporter) -> bool {
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

fn find_problem<'a>(
    contest: &'a Contest,
    problem_index: &str,
) -> io::Result<&'a crate::model::Problem> {
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

fn resolve_language(cli_language: Option<Language>, config: &Config) -> Language {
    cli_language.unwrap_or(config.defaults.language)
}

fn validate_debug_language(language: Language, debug: bool) -> io::Result<()> {
    if debug && language != Language::Cpp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--debug is only supported for C++",
        ));
    }

    Ok(())
}

pub fn test(
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

fn test_problem(
    destination: &Path,
    problem: &crate::model::Problem,
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

fn test_problem_with_debug_header(
    destination: &Path,
    problem: &crate::model::Problem,
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

pub fn create(
    name: &str,
    specified_language: Option<Language>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let config = Config::load()?;
    let cwd = std::env::current_dir()?;

    create_at(&cwd, name, specified_language, &config, reporter)
}

fn create_at(
    destination: &Path,
    name: &str,
    specified_language: Option<Language>,
    config: &Config,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let language = resolve_language(specified_language, config);
    let template = builtin_template(language);

    let path = workspace::create_source_file(destination, name, language, template)?;

    reporter.report(Event::SourceCreated { path: &path });

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::process::{Command as ProcessCommand, ExitStatus};

    use crate::ui::NullReporter;

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    fn available_cpp_compiler() -> Option<String> {
        ["g++", "clang++"].into_iter().find_map(|compiler| {
            ProcessCommand::new(compiler)
                .arg("--version")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|_| compiler.to_string())
        })
    }

    fn test_problem_model() -> crate::model::Problem {
        crate::model::Problem {
            index: "A".to_string(),
            title: "Problem A".to_string(),
            task_id: "abc466_a".to_string(),
            url: "https://example.invalid/a".to_string(),
        }
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }

    fn execution(outcome: ExecutionOutcome, stdout: &str, stderr: &str) -> runner::ExecutionResult {
        runner::ExecutionResult {
            outcome,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            elapsed: Duration::from_millis(10),
        }
    }

    fn old_contest(contest_id: &str) -> Contest {
        Contest {
            contest_id: contest_id.to_string(),
            problems: vec![crate::model::Problem {
                index: "OLD".to_string(),
                title: "Old problem".to_string(),
                task_id: "old_problem".to_string(),
                url: "https://atcoder.jp/contests/old/tasks/old_problem".to_string(),
            }],
        }
    }

    fn create_workspace(destination: &Path, contest_id: &str) {
        std::fs::create_dir(destination).expect("workspace directory should be created");
        workspace::save_metadata(destination, &old_contest(contest_id))
            .expect("old metadata should be written");
    }

    #[derive(Default)]
    struct RecordingReporter {
        events: Vec<String>,
    }

    impl Reporter for RecordingReporter {
        fn report(&mut self, event: Event<'_>) {
            let event = match event {
                Event::ContestFetching { contest_id } => format!("contest-fetching:{contest_id}"),
                Event::ContestFetched {
                    contest_id,
                    problems,
                } => format!("contest-fetched:{contest_id}:{problems}"),
                Event::ProblemFetching { index, .. } => format!("problem-fetching:{index}"),
                Event::ProblemFetched { index, samples } => {
                    format!("problem-fetched:{index}:{samples}")
                }
                Event::ProblemFetchFailed { index, .. } => format!("problem-failed:{index}"),
                Event::WorkspaceCreated { destination } => {
                    format!("created:{}", destination.display())
                }
                Event::WorkspaceRefreshed { destination } => {
                    format!("refreshed:{}", destination.display())
                }
                Event::NoSamples { problem_index } => {
                    format!("no-samples:{problem_index}")
                }

                Event::CompileFailed { .. } => "compile-failed".to_string(),

                Event::CompileTimedOut { .. } => "compile-timed-out".to_string(),

                Event::TestCaseAccepted { number, .. } => {
                    format!("case-accepted:{number}")
                }

                Event::TestCaseWrongAnswer { number, .. } => {
                    format!("case-wrong-answer:{number}")
                }

                Event::TestCaseRuntimeError { number, .. } => {
                    format!("case-runtime-error:{number}")
                }

                Event::TestCaseTimedOut { number, .. } => {
                    format!("case-timed-out:{number}")
                }
                Event::TestCaseStderr { number, stderr } => {
                    format!("case-stderr:{number}:{stderr}")
                }
                Event::SourceCreated { path } => {
                    format!("source-created:{}", path.display())
                }
            };
            self.events.push(event);
        }
    }

    #[test]
    fn problem_lookup_is_ascii_case_insensitive_and_rejects_ambiguity() {
        let contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![crate::model::Problem {
                index: "A".to_string(),
                title: "Problem A".to_string(),
                task_id: "abc466_a".to_string(),
                url: "https://example.invalid/a".to_string(),
            }],
        };

        assert_eq!(find_problem(&contest, "a").unwrap().index, "A");

        let ambiguous = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![
                contest.problems[0].clone(),
                crate::model::Problem {
                    index: "a".to_string(),
                    title: "Duplicate".to_string(),
                    task_id: "duplicate".to_string(),
                    url: "https://example.invalid/duplicate".to_string(),
                },
            ],
        };
        let error = find_problem(&ambiguous, "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn language_resolution_uses_cli_then_config_then_builtin_default() {
        let mut config = Config::default();
        config.defaults.language = Language::Python;

        assert_eq!(
            resolve_language(Some(Language::Cpp), &config),
            Language::Cpp
        );
        assert_eq!(resolve_language(None, &config), Language::Python);

        config.defaults.language = Language::Cpp;
        assert_eq!(
            resolve_language(Some(Language::Python), &config),
            Language::Python
        );
        assert_eq!(resolve_language(None, &Config::default()), Language::Cpp);
    }

    #[test]
    fn create_cpp_source_outside_a_workspace_uses_the_builtin_default() {
        let temp = tempfile::tempdir().unwrap();
        let mut reporter = RecordingReporter::default();

        create_at(temp.path(), "A", None, &Config::default(), &mut reporter).unwrap();

        let source = temp.path().join("A.cpp");
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            builtin_template(Language::Cpp)
        );
        assert!(!temp.path().join("A.py").exists());
        assert!(!temp.path().join(".atc").exists());
        assert!(!temp.path().join("tests").exists());
        assert_eq!(
            reporter.events,
            [format!("source-created:{}", source.display())]
        );
    }

    #[test]
    fn create_python_source_uses_config_and_the_builtin_template() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.defaults.language = Language::Python;
        let mut reporter = RecordingReporter::default();

        create_at(temp.path(), "A", None, &config, &mut reporter).unwrap();

        assert_eq!(
            std::fs::read_to_string(temp.path().join("A.py")).unwrap(),
            builtin_template(Language::Python)
        );
        assert!(!temp.path().join("A.cpp").exists());
    }

    #[test]
    fn create_cli_language_overrides_config_language() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.defaults.language = Language::Python;
        let mut reporter = RecordingReporter::default();

        create_at(
            temp.path(),
            "A",
            Some(Language::Cpp),
            &config,
            &mut reporter,
        )
        .unwrap();

        assert!(temp.path().join("A.cpp").is_file());
        assert!(!temp.path().join("A.py").exists());
    }

    #[test]
    fn create_does_not_read_or_modify_workspace_data() {
        let temp = tempfile::tempdir().unwrap();
        let atc_dir = temp.path().join(".atc");
        let tests_dir = temp.path().join("tests");
        std::fs::create_dir(&atc_dir).unwrap();
        std::fs::create_dir(&tests_dir).unwrap();
        let metadata = atc_dir.join("contest.toml");
        let sample = tests_dir.join("local.txt");
        std::fs::write(&metadata, "malformed metadata").unwrap();
        std::fs::write(&sample, "local test data").unwrap();
        let mut reporter = RecordingReporter::default();

        create_at(
            temp.path(),
            "not-in-metadata",
            None,
            &Config::default(),
            &mut reporter,
        )
        .unwrap();

        assert!(temp.path().join("not-in-metadata.cpp").is_file());
        assert_eq!(
            std::fs::read_to_string(metadata).unwrap(),
            "malformed metadata"
        );
        assert_eq!(std::fs::read_to_string(sample).unwrap(), "local test data");
    }

    #[test]
    fn create_preserves_an_existing_source_and_reports_only_success() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("A.cpp");
        std::fs::write(&source, "user source").unwrap();
        let mut reporter = RecordingReporter::default();

        let error = create_at(temp.path(), "A", None, &Config::default(), &mut reporter)
            .expect_err("an existing source must not be overwritten");

        assert!(matches!(
            error,
            AppError::Io(ref error) if error.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(std::fs::read_to_string(source).unwrap(), "user source");
        assert!(reporter.events.is_empty());
    }

    #[test]
    fn create_rejects_unsafe_names_without_writing_outside_the_destination() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("cwd");
        std::fs::create_dir(&destination).unwrap();
        let absolute = temp.path().join("absolute");
        let unsafe_names = vec![
            "../outside".to_string(),
            "nested/name".to_string(),
            "nested\\name".to_string(),
            absolute
                .to_str()
                .expect("temporary path should be UTF-8")
                .to_string(),
        ];
        let mut reporter = RecordingReporter::default();

        for name in unsafe_names {
            let error = create_at(&destination, &name, None, &Config::default(), &mut reporter)
                .expect_err("an unsafe name must be rejected");
            assert!(matches!(
                error,
                AppError::Io(ref error) if error.kind() == io::ErrorKind::InvalidInput
            ));
        }

        assert!(!temp.path().join("outside.cpp").exists());
        assert!(!absolute.with_extension("cpp").exists());
        assert!(!destination.join("nested").exists());
        assert!(reporter.events.is_empty());
    }

    #[test]
    fn python_debug_is_rejected_by_commands_policy() {
        let error = validate_debug_language(Language::Python, true).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("only supported for C++"));
        assert!(validate_debug_language(Language::Cpp, true).is_ok());
        assert!(validate_debug_language(Language::Python, false).is_ok());
    }

    #[test]
    fn no_samples_is_reported_without_starting_a_runner() {
        let temp = tempfile::tempdir().unwrap();
        let problem = test_problem_model();
        std::fs::write(temp.path().join("A.cpp"), "source").unwrap();
        let runner_config = RunnerConfig {
            cpp_compiler: "definitely-not-a-real-compiler".to_string(),
            ..RunnerConfig::default()
        };
        let mut reporter = RecordingReporter::default();

        test_problem(
            temp.path(),
            &problem,
            Language::Cpp,
            &runner_config,
            false,
            &mut reporter,
        )
        .unwrap();

        assert_eq!(reporter.events, ["no-samples:A"]);
    }

    #[test]
    fn no_samples_in_debug_mode_does_not_materialize_header_or_start_compiler() {
        let temp = tempfile::tempdir().unwrap();
        let problem = test_problem_model();
        std::fs::write(temp.path().join("A.cpp"), "source").unwrap();
        let runner_config = RunnerConfig {
            cpp_compiler: "definitely-not-a-real-compiler".to_string(),
            ..RunnerConfig::default()
        };
        let mut reporter = RecordingReporter::default();

        test_problem_with_debug_header(
            temp.path(),
            &problem,
            Language::Cpp,
            &runner_config,
            true,
            &mut reporter,
            || panic!("debug header must not be materialized without samples"),
        )
        .unwrap();

        assert_eq!(reporter.events, ["no-samples:A"]);
    }

    #[test]
    fn normal_cpp_build_neither_materializes_nor_requires_debug_header() {
        let Some(compiler) = available_cpp_compiler() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let problem = test_problem_model();
        std::fs::write(
            temp.path().join("A.cpp"),
            concat!(
                "#ifdef LOCAL\n",
                "#error LOCAL must not be defined in a normal build\n",
                "#endif\n",
                "#include <iostream>\n",
                "int main() { std::cout << 7 << '\\n'; }\n",
            ),
        )
        .unwrap();
        workspace::save_samples(
            temp.path(),
            &problem,
            &[Sample {
                input: String::new(),
                output: "7\n".to_string(),
            }],
        )
        .unwrap();
        let runner_config = RunnerConfig {
            cpp_compiler: compiler,
            ..RunnerConfig::default()
        };
        let mut reporter = RecordingReporter::default();

        test_problem_with_debug_header(
            temp.path(),
            &problem,
            Language::Cpp,
            &runner_config,
            false,
            &mut reporter,
            || panic!("normal build must not materialize the debug header"),
        )
        .unwrap();

        assert_eq!(reporter.events, ["case-accepted:1"]);
    }

    #[test]
    fn debug_cpp_build_resolves_embedded_header_and_reports_debug_stderr_after_ac() {
        let Some(compiler) = available_cpp_compiler() else {
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let problem = test_problem_model();
        std::fs::write(
            temp.path().join("A.cpp"),
            concat!(
                "#ifndef LOCAL\n",
                "#error LOCAL must be defined in a debug build\n",
                "#endif\n",
                "#include <atc/debug.hpp>\n",
                "#include <iostream>\n",
                "int main() { int x = 7; std::cout << x << '\\n'; debug(x); }\n",
            ),
        )
        .unwrap();
        workspace::save_samples(
            temp.path(),
            &problem,
            &[Sample {
                input: String::new(),
                output: "7\n".to_string(),
            }],
        )
        .unwrap();
        let runner_config = RunnerConfig {
            cpp_compiler: compiler,
            ..RunnerConfig::default()
        };
        let cache_dir = temp.path().join("cache root with spaces");
        let mut reporter = RecordingReporter::default();

        test_problem_with_debug_header(
            temp.path(),
            &problem,
            Language::Cpp,
            &runner_config,
            true,
            &mut reporter,
            || Ok(crate::debug::materialize_debug_header_in(&cache_dir)?),
        )
        .unwrap();

        assert_eq!(reporter.events[0], "case-accepted:1");
        assert!(reporter.events[1].starts_with("case-stderr:1:"));
        assert!(reporter.events[1].contains("x = 7"));
    }

    #[test]
    fn debug_header_materialization_error_is_fatal_before_compiler_start() {
        let temp = tempfile::tempdir().unwrap();
        let problem = test_problem_model();
        std::fs::write(temp.path().join("A.cpp"), "source").unwrap();
        workspace::save_samples(
            temp.path(),
            &problem,
            &[Sample {
                input: String::new(),
                output: String::new(),
            }],
        )
        .unwrap();
        let runner_config = RunnerConfig {
            cpp_compiler: "definitely-not-a-real-compiler".to_string(),
            ..RunnerConfig::default()
        };
        let mut reporter = RecordingReporter::default();

        let error = test_problem_with_debug_header(
            temp.path(),
            &problem,
            Language::Cpp,
            &runner_config,
            true,
            &mut reporter,
            || Err(io::Error::new(io::ErrorKind::PermissionDenied, "cache is read-only").into()),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            AppError::Io(ref error) if error.kind() == io::ErrorKind::PermissionDenied
        ));
        assert!(reporter.events.is_empty());
    }

    #[test]
    fn resolved_language_does_not_fall_back_to_an_existing_other_source() {
        let temp = tempfile::tempdir().unwrap();
        let problem = test_problem_model();
        std::fs::write(temp.path().join("A.cpp"), "source").unwrap();
        let mut reporter = RecordingReporter::default();

        let error = test_problem(
            temp.path(),
            &problem,
            Language::Python,
            &RunnerConfig::default(),
            false,
            &mut reporter,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            AppError::Io(ref error) if error.kind() == io::ErrorKind::NotFound
        ));
        assert!(reporter.events.is_empty());
    }

    #[test]
    fn compile_failure_and_compile_timeout_are_distinct_recoverable_events() {
        let mut reporter = RecordingReporter::default();

        assert!(!report_compile_result(
            &execution(ExecutionOutcome::Exited(exit_status(1)), "", "error"),
            &mut reporter,
        ));
        assert!(!report_compile_result(
            &execution(ExecutionOutcome::TimedOut, "", ""),
            &mut reporter,
        ));

        assert_eq!(reporter.events, ["compile-failed", "compile-timed-out"]);
    }

    #[test]
    fn recoverable_case_results_do_not_stop_later_samples() {
        let samples: Vec<_> = (0..4)
            .map(|_| Sample {
                input: String::new(),
                output: "expected\n".to_string(),
            })
            .collect();
        let mut results = VecDeque::from([
            execution(
                ExecutionOutcome::Exited(exit_status(0)),
                "wrong\n",
                "wa stderr",
            ),
            execution(ExecutionOutcome::Exited(exit_status(1)), "", "runtime"),
            execution(ExecutionOutcome::TimedOut, "", "tle stderr"),
            execution(
                ExecutionOutcome::Exited(exit_status(0)),
                "expected\n",
                "ac stderr",
            ),
        ]);
        let mut reporter = RecordingReporter::default();

        run_test_cases(
            &samples,
            |_| Ok(results.pop_front().unwrap()),
            &mut reporter,
        )
        .unwrap();

        assert!(results.is_empty());
        assert_eq!(
            reporter.events,
            [
                "case-wrong-answer:1",
                "case-stderr:1:wa stderr",
                "case-runtime-error:2",
                "case-stderr:2:runtime",
                "case-timed-out:3",
                "case-stderr:3:tle stderr",
                "case-accepted:4",
                "case-stderr:4:ac stderr",
            ]
        );
    }

    #[test]
    fn stderr_does_not_change_an_accepted_stdout_verdict() {
        let sample = Sample {
            input: String::new(),
            output: "answer\n".to_string(),
        };
        let result = execution(
            ExecutionOutcome::Exited(exit_status(0)),
            "answer\n",
            "debug output\n",
        );
        let mut reporter = RecordingReporter::default();

        report_case_result(1, &sample, &result, &mut reporter);

        assert_eq!(
            reporter.events,
            ["case-accepted:1", "case-stderr:1:debug output\n"]
        );
    }

    #[test]
    fn new_flow_runs_entirely_from_fixtures() {
        let mut reporter = NullReporter;
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        let client = atcoder::AtCoderClient::fixture(fixture_root());

        new_at(
            &destination,
            "abc466",
            Language::Cpp,
            &client,
            &mut reporter,
        )
        .expect("fixture new flow should succeed");

        let contest =
            workspace::load_metadata(&destination).expect("created metadata should be readable");
        assert_eq!(contest.contest_id, "abc466");
        assert_eq!(contest.problems.len(), 7);

        for problem in &contest.problems {
            assert_eq!(
                std::fs::read_to_string(destination.join(format!("{}.cpp", problem.index)))
                    .expect("C++ source should be readable"),
                builtin_template(Language::Cpp)
            );
            assert!(!destination.join(format!("{}.py", problem.index)).exists());

            let test_dir = destination.join("tests").join(&problem.index);

            if problem.index == "C" {
                assert!(!test_dir.exists());
            } else {
                assert!(test_dir.join("sample-1.in").is_file());

                assert!(test_dir.join("sample-1.out").is_file());
            }
        }
    }

    #[test]
    fn new_flow_creates_python_sources_from_the_builtin_template() {
        let mut reporter = NullReporter;
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        let client = atcoder::AtCoderClient::fixture(fixture_root());

        new_at(
            &destination,
            "abc466",
            Language::Python,
            &client,
            &mut reporter,
        )
        .expect("fixture new flow should succeed");

        let contest =
            workspace::load_metadata(&destination).expect("created metadata should be readable");
        for problem in &contest.problems {
            assert_eq!(
                std::fs::read_to_string(destination.join(format!("{}.py", problem.index)))
                    .expect("Python source should be readable"),
                builtin_template(Language::Python)
            );
            assert!(!destination.join(format!("{}.cpp", problem.index)).exists());
        }
    }

    #[test]
    fn existing_contest_is_a_noop_before_fixture_access() {
        let mut reporter = NullReporter;
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("contest directory should be created");
        std::fs::write(destination.join("A.cpp"), "user source")
            .expect("existing source should be written");
        let client = atcoder::AtCoderClient::fixture(temp.path().join("missing-fixtures"));

        new_at(
            &destination,
            "abc466",
            Language::Cpp,
            &client,
            &mut reporter,
        )
        .expect("existing contest should be a no-op");

        assert_eq!(
            std::fs::read_to_string(destination.join("A.cpp"))
                .expect("existing source should remain readable"),
            "user source"
        );
    }

    #[test]
    fn failed_workspace_build_does_not_leave_partial_contest() {
        let mut reporter = NullReporter;
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let fixture_root = temp.path().join("fixtures");
        let contests = fixture_root.join("contests");
        std::fs::create_dir_all(&contests).expect("fixture directory should be created");
        std::fs::write(
            contests.join("broken.html"),
            r#"<table><tbody><tr>
                <td><a href="/contests/broken/tasks/broken_a">../outside</a></td>
                <td><a href="/contests/broken/tasks/broken_a">Broken</a></td>
            </tr></tbody></table>"#,
        )
        .expect("fixture should be written");
        let destination = temp.path().join("broken");
        let client = atcoder::AtCoderClient::fixture(&fixture_root);

        let error = new_at(
            &destination,
            "broken",
            Language::Cpp,
            &client,
            &mut reporter,
        )
        .expect_err("unsafe workspace path should fail");

        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(!destination.exists());
        assert!(
            std::fs::read_dir(temp.path())
                .expect("temporary root should be readable")
                .all(|entry| !entry
                    .expect("directory entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".atc-new-"))
        );
    }

    #[test]
    fn refresh_rebuilds_metadata_and_tests_without_touching_sources() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        create_workspace(&destination, "abc466");
        std::fs::write(destination.join("A.cpp"), "user source").expect("source should be written");
        std::fs::write(destination.join("LOCAL.cpp"), "local source")
            .expect("local source should be written");
        let stale_tests = destination.join("tests").join("C");
        std::fs::create_dir_all(&stale_tests).expect("stale tests should be created");
        std::fs::write(stale_tests.join("old.in"), "stale").expect("stale test should be written");

        let client = atcoder::AtCoderClient::fixture(fixture_root());
        let mut reporter = RecordingReporter::default();
        refresh_at(&destination, "abc466", &client, &mut reporter)
            .expect("fixture refresh should succeed");

        assert_eq!(
            std::fs::read_to_string(destination.join("A.cpp"))
                .expect("source should remain readable"),
            "user source"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("LOCAL.cpp"))
                .expect("local source should remain readable"),
            "local source"
        );
        assert!(!destination.join("tests").join("C").exists());
        assert!(
            destination
                .join("tests")
                .join("A")
                .join("sample-1.in")
                .is_file()
        );

        let contest =
            workspace::load_metadata(&destination).expect("refreshed metadata should be readable");
        assert_eq!(contest.contest_id, "abc466");
        assert_eq!(contest.problems.len(), 7);
        assert!(
            reporter
                .events
                .contains(&format!("refreshed:{}", destination.display()))
        );
        assert!(
            std::fs::read_dir(&destination)
                .expect("workspace should be readable")
                .all(|entry| !entry
                    .expect("directory entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".atc-refresh-"))
        );
    }

    #[test]
    fn refresh_sample_failure_is_recoverable_and_removes_old_problem_tests() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("mini");
        create_workspace(&destination, "mini");
        let old_b_tests = destination.join("tests").join("B");
        std::fs::create_dir_all(&old_b_tests).expect("old tests should be created");
        std::fs::write(old_b_tests.join("old.in"), "old").expect("old test should be written");

        let fixtures = temp.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("contests"))
            .expect("contest fixtures should be created");
        std::fs::create_dir_all(fixtures.join("problems"))
            .expect("problem fixtures should be created");
        std::fs::write(
            fixtures.join("contests").join("mini.html"),
            r#"<table><tbody>
                <tr><td><a href="/contests/mini/tasks/mini_a">A</a></td><td><a href="/contests/mini/tasks/mini_a">A</a></td></tr>
                <tr><td><a href="/contests/mini/tasks/mini_b">B</a></td><td><a href="/contests/mini/tasks/mini_b">B</a></td></tr>
            </tbody></table>"#,
        )
        .expect("tasks fixture should be written");
        std::fs::write(
            fixtures.join("problems").join("mini_a.html"),
            r#"<div id="task-statement"><span class="lang-en">
                <div class="part"><section><h3>Sample Input 1</h3><pre>1\n</pre></section></div>
                <div class="part"><section><h3>Sample Output 1</h3><pre>2\n</pre></section></div>
            </span></div>"#,
        )
        .expect("problem fixture should be written");

        let client = atcoder::AtCoderClient::fixture(&fixtures);
        let mut reporter = RecordingReporter::default();
        refresh_at(&destination, "mini", &client, &mut reporter)
            .expect("partial sample failure should be recoverable");

        assert!(destination.join("tests").join("A").is_dir());
        assert!(!destination.join("tests").join("B").exists());
        assert!(
            reporter
                .events
                .iter()
                .any(|event| event == "problem-failed:B")
        );
    }

    #[test]
    fn refresh_without_override_does_not_infer_id_from_directory_name() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("directory should be created");
        std::fs::create_dir(destination.join(".atc")).expect("marker should be created");
        std::fs::write(
            destination.join(".atc").join("contest.toml"),
            "version = ???",
        )
        .expect("broken metadata should be written");

        let error = resolve_refresh_contest_id(&destination, None)
            .expect_err("broken metadata must not fall back to the directory name");

        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn refresh_override_requires_workspace_marker() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("directory should be created");

        let error = resolve_refresh_contest_id(&destination, Some("abc466"))
            .expect_err("an arbitrary matching directory must be rejected");

        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn refresh_override_recovers_missing_metadata_inside_workspace() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("directory should be created");
        std::fs::create_dir(destination.join(".atc")).expect("marker should be created");

        let contest_id = resolve_refresh_contest_id(&destination, Some("abc466"))
            .expect("override should not require contest.toml");

        assert_eq!(contest_id, "abc466");

        let client = atcoder::AtCoderClient::fixture(fixture_root());
        let mut reporter = NullReporter;
        refresh_at(&destination, &contest_id, &client, &mut reporter)
            .expect("override refresh should reconstruct missing metadata");
        assert_eq!(
            workspace::load_metadata(&destination)
                .expect("reconstructed metadata should load")
                .contest_id,
            "abc466"
        );
    }

    #[test]
    fn refresh_override_recovers_malformed_metadata_inside_workspace() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).expect("directory should be created");
        std::fs::create_dir(destination.join(".atc")).expect("marker should be created");
        std::fs::write(
            destination.join(".atc").join("contest.toml"),
            "version = ???",
        )
        .expect("malformed metadata should be written");

        let contest_id = resolve_refresh_contest_id(&destination, Some("abc466"))
            .expect("override should ignore malformed contest.toml");
        let client = atcoder::AtCoderClient::fixture(fixture_root());
        let mut reporter = NullReporter;
        refresh_at(&destination, &contest_id, &client, &mut reporter)
            .expect("override refresh should replace malformed metadata");

        assert_eq!(
            workspace::load_metadata(&destination)
                .expect("reconstructed metadata should load")
                .contest_id,
            "abc466"
        );
    }

    #[test]
    fn refresh_override_rejects_wrong_directory_name() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("other");
        std::fs::create_dir(&destination).expect("directory should be created");
        std::fs::create_dir(destination.join(".atc")).expect("marker should be created");

        let error = resolve_refresh_contest_id(&destination, Some("abc466"))
            .expect_err("override must match the directory name");

        assert!(matches!(error, AppError::Io(_)));
    }

    #[test]
    fn invalid_fetched_problem_path_preserves_old_refresh_data() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let destination = temp.path().join("broken");
        create_workspace(&destination, "broken");
        let old_tests = destination.join("tests").join("OLD");
        std::fs::create_dir_all(&old_tests).expect("old tests should be created");
        std::fs::write(old_tests.join("old.in"), "old").expect("old test should be written");
        let old_metadata = std::fs::read_to_string(destination.join(".atc").join("contest.toml"))
            .expect("old metadata should be readable");

        let fixtures = temp.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("contests"))
            .expect("fixture directory should be created");
        std::fs::write(
            fixtures.join("contests").join("broken.html"),
            r#"<table><tbody><tr>
                <td><a href="/contests/broken/tasks/broken_a">../outside</a></td>
                <td><a href="/contests/broken/tasks/broken_a">Broken</a></td>
            </tr></tbody></table>"#,
        )
        .expect("fixture should be written");
        let client = atcoder::AtCoderClient::fixture(&fixtures);
        let mut reporter = NullReporter;

        refresh_at(&destination, "broken", &client, &mut reporter)
            .expect_err("unsafe fetched path should fail");

        assert_eq!(
            std::fs::read_to_string(destination.join(".atc").join("contest.toml"))
                .expect("old metadata should remain readable"),
            old_metadata
        );
        assert_eq!(
            std::fs::read_to_string(old_tests.join("old.in"))
                .expect("old tests should remain readable"),
            "old"
        );
    }
}
