use super::*;
use crate::workspace;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitStatus};

use crate::attempt::{AttemptCancellation, AttemptOutcome, run_attempt};
use crate::ui::NullReporter;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn builtin_source_template(language: Language) -> Result<String, AppError> {
    Ok(builtin_template(language).to_owned())
}

fn create_directory_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_dir(target, link);

    match result {
        Ok(()) => true,
        #[cfg(windows)]
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1314) =>
        {
            false
        }
        Err(error) => panic!("failed to create directory symlink: {error}"),
    }
}

fn create_single_problem_fixture(root: &Path, contest_id: &str) {
    std::fs::create_dir_all(root.join("contests")).expect("contest fixtures should be created");
    std::fs::create_dir_all(root.join("problems")).expect("problem fixtures should be created");
    std::fs::write(
        root.join("contests").join(format!("{contest_id}.html")),
        format!(
            r#"<table><tbody>
                <tr><td><a href="/contests/{contest_id}/tasks/{contest_id}_a">A</a></td><td><a href="/contests/{contest_id}/tasks/{contest_id}_a">Problem A</a></td></tr>
            </tbody></table>"#
        ),
    )
    .expect("tasks fixture should be written");
    std::fs::write(
        root.join("problems").join(format!("{contest_id}_a.html")),
        r#"<div id="task-statement"><span class="lang-en">
            <div class="part"><section><h3>Sample Input 1</h3><pre>ABC349
</pre></section></div>
            <div class="part"><section><h3>Sample Output 1</h3><pre>Yes
</pre></section></div>
        </span></div>"#,
    )
    .expect("problem fixture should be written");
}

pub(super) fn available_cpp_compiler() -> Option<String> {
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
        task_id: "abc123_a".to_string(),
        url: "https://atcoder.jp/contests/abc123/tasks/abc123_a".to_string(),
        sample_count: 0,
    }
}

fn save_test_problem(destination: &Path, problem: &crate::model::Problem) {
    workspace::save_metadata(
        destination,
        &crate::model::Contest {
            contest_id: "abc123".into(),
            problems: vec![problem.clone()],
        },
    )
    .unwrap();
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
        stdout_is_utf8: true,
        stdout_truncated: false,
        stderr: stderr.to_string(),
        elapsed: Duration::from_millis(10),
    }
}

fn old_contest(contest_id: &str) -> Contest {
    let task_id = format!("{contest_id}_old");
    Contest {
        contest_id: contest_id.to_string(),
        problems: vec![crate::model::Problem {
            index: "OLD".to_string(),
            title: "Old problem".to_string(),
            url: format!("https://atcoder.jp/contests/{contest_id}/tasks/{task_id}"),
            task_id,
            sample_count: 0,
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
            Event::DoctorReport { .. } => "doctor-report".to_owned(),
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
            Event::WorkspaceRefreshSkipped { contest_id } => {
                format!("refresh-skipped:{contest_id}")
            }
            Event::WorkspaceRepaired { destination } => {
                format!("repaired:{}", destination.display())
            }
            Event::WorkspaceInitialized { path } => {
                format!("workspace-initialized:{}", path.display())
            }
            Event::WorkspaceAlreadyInitialized { path } => {
                format!("workspace-already-initialized:{}", path.display())
            }
            Event::NoSamples { problem_index } => {
                format!("no-samples:{problem_index}")
            }
            Event::SavedStressCaseSkipped {
                problem_index,
                reason,
            } => format!("stress-skipped:{problem_index}:{reason}"),

            Event::CompileDiagnostics {
                problem_index,
                stderr,
            } => format!("compile-diagnostics:{problem_index}:{stderr}"),

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
            Event::TemplateFileCreated { path } => {
                format!("template-file-created:{}", path.display())
            }
            Event::TemplateFileExists { path } => {
                format!("template-file-exists:{}", path.display())
            }
            Event::ConfigFileCreated { path } => {
                format!("config-file-created:{}", path.display())
            }
            Event::ConfigFileExists { path } => {
                format!("config-file-exists:{}", path.display())
            }
            Event::StressFileCreated { path } => {
                format!("stress-file-created:{}", path.display())
            }
            Event::StressFileExists { path } => {
                format!("stress-file-exists:{}", path.display())
            }
            Event::StressFilesAlreadyInitialized { problem_index } => {
                format!("stress-files-already-initialized:{problem_index}")
            }
            Event::WatchStarted { destination } => {
                format!("watch-started:{}", destination.display())
            }

            Event::WatchSourceChanged { source } => {
                format!("watch-source-changed:{}", source.display())
            }
            Event::WatchTestFailed {
                problem_index,
                error,
            } => {
                format!("watch-test-failed:{problem_index}:{error}")
            }
            Event::TestCaseLayout { .. } => return,
            Event::TestRunStarted {
                problem_index,
                total_cases,
            } => {
                format!("test-started:{problem_index}:{total_cases}")
            }

            Event::TestRunFinished {
                problem_index,
                accepted,
                total_cases,
            } => {
                format!("test-finished:{problem_index}:{accepted}:{total_cases}")
            }
            Event::TestCaseComparison {
                number,
                input,
                expected,
                actual,
            } => {
                format!(
                    "case-comparison:{number}:input={input:?}:expected={expected:?}:actual={actual:?}"
                )
            }
            Event::StressStarted {
                problem_index,
                base_seed,
                case_limit,
            } => {
                format!("stress-started:{problem_index}:{base_seed}:{case_limit:?}")
            }
            Event::StressProgress {
                problem_index,
                case_number,
                seed,
                passed,
                ..
            } => {
                format!("stress-progress:{problem_index}:{case_number}:{seed}:{passed}")
            }
            Event::StressFailed {
                problem_index,
                failure,
                ..
            } => {
                format!(
                    "stress-failed:{problem_index}:{}:{}:{}",
                    failure.kind.as_str(),
                    failure.case_number,
                    failure.seed
                )
            }
            Event::StressFinished {
                problem_index,
                cases,
                ..
            } => {
                format!("stress-finished:{problem_index}:{cases}")
            }
            Event::StressCancelled {
                problem_index,
                cases,
                ..
            } => {
                format!("stress-cancelled:{problem_index}:{cases}")
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
            url: "https://atcoder.jp/contests/abc466/tasks/abc466_a".to_string(),
            sample_count: 0,
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
                task_id: "abc466_duplicate".to_string(),
                url: "https://atcoder.jp/contests/abc466/tasks/abc466_duplicate".to_string(),
                sample_count: 0,
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
    let templates_dir = temp.path().join("missing-templates");
    let mut reporter = RecordingReporter::default();

    create_at(
        temp.path(),
        "A",
        None,
        &Config::default(),
        |language| resolve_source_template_in(&templates_dir, language),
        &mut reporter,
    )
    .unwrap();

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
    let templates_dir = temp.path().join("missing-templates");
    let mut config = Config::default();
    config.defaults.language = Language::Python;
    let mut reporter = RecordingReporter::default();

    create_at(
        temp.path(),
        "A",
        None,
        &config,
        |language| resolve_source_template_in(&templates_dir, language),
        &mut reporter,
    )
    .unwrap();

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
        builtin_source_template,
        &mut reporter,
    )
    .unwrap();

    assert!(temp.path().join("A.cpp").is_file());
    assert!(!temp.path().join("A.py").exists());
}

#[test]
fn create_uses_the_selected_override_and_ignores_a_broken_unselected_override() {
    let temp = tempfile::tempdir().unwrap();
    let templates_dir = temp.path().join("templates");
    std::fs::create_dir(&templates_dir).unwrap();
    std::fs::create_dir(templates_dir.join("cpp.cpp")).unwrap();
    std::fs::write(templates_dir.join("python.py"), "print('custom create')\n").unwrap();
    let mut reporter = RecordingReporter::default();

    create_at(
        temp.path(),
        "A",
        Some(Language::Python),
        &Config::default(),
        |language| resolve_source_template_in(&templates_dir, language),
        &mut reporter,
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(temp.path().join("A.py")).unwrap(),
        "print('custom create')\n"
    );
    assert!(!temp.path().join("A.cpp").exists());
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
        builtin_source_template,
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

    let error = create_at(
        temp.path(),
        "A",
        None,
        &Config::default(),
        builtin_source_template,
        &mut reporter,
    )
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
        let error = create_at(
            &destination,
            &name,
            None,
            &Config::default(),
            builtin_source_template,
            &mut reporter,
        )
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
    save_test_problem(temp.path(), &problem);
    std::fs::write(temp.path().join("A.cpp"), "source").unwrap();
    let runner_config = RunnerConfig {
        cpp_compiler: "definitely-not-a-real-compiler".to_string(),
        ..RunnerConfig::default()
    };
    let mut reporter = RecordingReporter::default();

    test_problem(
        temp.path(),
        "abc123",
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
    save_test_problem(temp.path(), &problem);
    std::fs::write(temp.path().join("A.cpp"), "source").unwrap();
    let runner_config = RunnerConfig {
        cpp_compiler: "definitely-not-a-real-compiler".to_string(),
        ..RunnerConfig::default()
    };
    let mut reporter = RecordingReporter::default();

    test_problem_with_debug_header(
        temp.path(),
        "abc123",
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
    let mut problem = test_problem_model();
    problem.sample_count = 1;
    save_test_problem(temp.path(), &problem);
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
        compile_timeout_seconds: 60.0,
        ..RunnerConfig::default()
    };
    let mut reporter = RecordingReporter::default();

    test_problem_with_debug_header(
        temp.path(),
        "abc123",
        &problem,
        Language::Cpp,
        &runner_config,
        false,
        &mut reporter,
        || panic!("normal build must not materialize the debug header"),
    )
    .unwrap();

    assert_eq!(
        reporter.events,
        ["test-started:A:1", "case-accepted:1", "test-finished:A:1:1",]
    );
}

#[test]
fn debug_cpp_build_resolves_embedded_header_and_reports_debug_stderr_after_ac() {
    let Some(compiler) = available_cpp_compiler() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let mut problem = test_problem_model();
    problem.sample_count = 1;
    save_test_problem(temp.path(), &problem);
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
            input: "sample input\n".to_string(),
            output: "7\n".to_string(),
        }],
    )
    .unwrap();
    let runner_config = RunnerConfig {
        cpp_compiler: compiler,
        compile_timeout_seconds: 60.0,
        ..RunnerConfig::default()
    };
    let cache_dir = temp.path().join("cache root with spaces");
    let mut reporter = RecordingReporter::default();

    test_problem_with_debug_header(
        temp.path(),
        "abc123",
        &problem,
        Language::Cpp,
        &runner_config,
        true,
        &mut reporter,
        || Ok(crate::debug::materialize_debug_header_in(&cache_dir)?),
    )
    .unwrap();

    assert_eq!(reporter.events[0], "test-started:A:1");
    assert_eq!(reporter.events[1], "case-accepted:1");

    assert!(reporter.events[2].starts_with("case-comparison:1:"));
    assert!(reporter.events[2].contains(r#"input="sample input\n""#));
    assert!(reporter.events[2].contains("expected="));
    assert!(reporter.events[2].contains("actual="));

    assert!(reporter.events[3].starts_with("case-stderr:1:"));
    assert!(reporter.events[3].contains("x = 7"));

    assert_eq!(reporter.events[4], "test-finished:A:1:1");
}

#[test]
fn debug_header_materialization_error_is_fatal_before_compiler_start() {
    let temp = tempfile::tempdir().unwrap();
    let mut problem = test_problem_model();
    problem.sample_count = 1;
    save_test_problem(temp.path(), &problem);
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
        "abc123",
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
    save_test_problem(temp.path(), &problem);
    std::fs::write(temp.path().join("A.cpp"), "source").unwrap();
    let mut reporter = RecordingReporter::default();

    let error = test_problem(
        temp.path(),
        "abc123",
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
fn successful_compile_reports_only_nonempty_stderr_without_rewriting_it() {
    let mut reporter = RecordingReporter::default();
    let stderr = "  warning: comparison [-Wsign-compare]\r\n\t ^\r\n\r\n";

    for stderr in ["", stderr] {
        assert!(report_compile_result(
            "A",
            &execution(ExecutionOutcome::Exited(exit_status(0)), "", stderr),
            &mut reporter,
        ));
    }

    assert_eq!(reporter.events, [format!("compile-diagnostics:A:{stderr}")]);
}

#[test]
fn compile_failure_and_compile_timeout_are_distinct_recoverable_events() {
    let mut reporter = RecordingReporter::default();

    assert!(!report_compile_result(
        "A",
        &execution(ExecutionOutcome::Exited(exit_status(1)), "", "error"),
        &mut reporter,
    ));
    assert!(!report_compile_result(
        "A",
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

    let cancellation = AttemptCancellation::new();
    let outcome = run_attempt(&cancellation, |_| {
        run_test_cases(
            "A",
            &samples,
            None,
            false,
            |_| Ok(results.pop_front().unwrap()),
            &mut reporter,
        )
    });

    assert!(matches!(outcome, AttemptOutcome::Completed));
    assert!(results.is_empty());
    assert_eq!(
        reporter.events,
        [
            "test-started:A:4",
            "case-wrong-answer:1",
            r#"case-comparison:1:input="":expected="expected\n":actual="wrong\n""#,
            "case-stderr:1:wa stderr",
            "case-runtime-error:2",
            "case-stderr:2:runtime",
            "case-timed-out:3",
            "case-stderr:3:tle stderr",
            "case-accepted:4",
            "case-stderr:4:ac stderr",
            "test-finished:A:1:4",
        ]
    );
}

#[test]
fn saved_stress_case_runs_after_official_samples() {
    let samples = vec![
        Sample {
            input: "sample-1\n".to_string(),
            output: "ok\n".to_string(),
        },
        Sample {
            input: "sample-2\n".to_string(),
            output: "ok\n".to_string(),
        },
    ];
    let stress = Sample {
        input: "stress\n".to_string(),
        output: "ok\n".to_string(),
    };
    let mut executed = Vec::new();
    let mut reporter = RecordingReporter::default();

    run_test_cases(
        "A",
        &samples,
        Some(&stress),
        false,
        |sample| {
            executed.push(sample.input.clone());
            Ok(execution(
                ExecutionOutcome::Exited(exit_status(0)),
                "ok\n",
                "",
            ))
        },
        &mut reporter,
    )
    .unwrap();

    assert_eq!(executed, ["sample-1\n", "sample-2\n", "stress\n"]);
    assert_eq!(
        reporter.events,
        [
            "test-started:A:3",
            "case-accepted:1",
            "case-accepted:2",
            "case-accepted:3",
            "test-finished:A:3:3",
        ]
    );
}

#[test]
fn test_run_boundaries_and_accepted_counts_are_independent_between_runs() {
    let samples = vec![
        Sample {
            input: String::new(),
            output: "expected\n".to_string(),
        },
        Sample {
            input: String::new(),
            output: "expected\n".to_string(),
        },
        Sample {
            input: String::new(),
            output: "expected\n".to_string(),
        },
    ];
    let mut results = VecDeque::from([
        execution(ExecutionOutcome::Exited(exit_status(0)), "expected\n", ""),
        execution(ExecutionOutcome::Exited(exit_status(0)), "wrong\n", "wa"),
        execution(
            ExecutionOutcome::Exited(exit_status(0)),
            "expected\n",
            "debug",
        ),
    ]);
    let mut reporter = RecordingReporter::default();

    run_test_cases(
        "A",
        &samples,
        None,
        false,
        |_| Ok(results.pop_front().unwrap()),
        &mut reporter,
    )
    .unwrap();
    run_test_cases(
        "B",
        &samples[..1],
        None,
        false,
        |_| {
            Ok(execution(
                ExecutionOutcome::Exited(exit_status(0)),
                "expected\n",
                "",
            ))
        },
        &mut reporter,
    )
    .unwrap();

    assert_eq!(
        reporter.events,
        [
            "test-started:A:3",
            "case-accepted:1",
            "case-wrong-answer:2",
            r#"case-comparison:2:input="":expected="expected\n":actual="wrong\n""#,
            "case-stderr:2:wa",
            "case-accepted:3",
            "case-stderr:3:debug",
            "test-finished:A:2:3",
            "test-started:B:1",
            "case-accepted:1",
            "test-finished:B:1:1",
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

    report_case_result(1, &sample, &result, false, &mut reporter);

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
    let templates_dir = temp.path().join("missing-templates");
    let template = resolve_source_template_in(&templates_dir, Language::Cpp).unwrap();
    let client = atcoder::AtCoderClient::fixture(fixture_root());

    new_at(
        &destination,
        "abc466",
        Language::Cpp,
        &template,
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
fn new_at_creates_a_missing_mapped_parent_directory() {
    let mut reporter = NullReporter;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc").join("abc466");
    let client = atcoder::AtCoderClient::fixture(fixture_root());

    new_at(
        &destination,
        "abc466",
        Language::Cpp,
        builtin_template(Language::Cpp),
        &client,
        &mut reporter,
    )
    .unwrap();

    assert_eq!(
        workspace::load_metadata(&destination).unwrap().contest_id,
        "abc466"
    );
}

#[test]
fn new_at_reports_a_racing_directory_without_replacing_it() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = RecordingReporter::default();

    let error = new_at_with_install_hook(
        &destination,
        "abc466",
        Language::Cpp,
        builtin_template(Language::Cpp),
        &client,
        &mut reporter,
        || {
            std::fs::create_dir(&destination).unwrap();
            std::fs::write(destination.join("competitor.txt"), "keep me\n").unwrap();
        },
    )
    .expect_err("a destination created during staging must be reported as a race");

    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == io::ErrorKind::AlreadyExists
    ));
    assert_eq!(
        std::fs::read_to_string(destination.join("competitor.txt")).unwrap(),
        "keep me\n"
    );
    assert!(
        !reporter
            .events
            .iter()
            .any(|event| event.starts_with("workspace-created:"))
    );
    assert!(std::fs::read_dir(temp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".atc-new-")
    }));
}

#[test]
fn invalid_utf8_candidate_output_cannot_be_accepted_as_replacement_text() {
    let sample = Sample {
        input: String::new(),
        output: "\u{fffd}\n".to_string(),
    };
    let mut result = execution(ExecutionOutcome::Exited(exit_status(0)), "\u{fffd}\n", "");
    result.stdout_is_utf8 = false;
    let mut reporter = RecordingReporter::default();

    let verdict = report_case_result(1, &sample, &result, false, &mut reporter);

    assert!(matches!(verdict, test::CaseVerdict::WrongAnswer));
    assert!(
        reporter
            .events
            .iter()
            .any(|event| event == "case-wrong-answer:1")
    );
}

#[test]
fn workspace_new_flow_creates_a_nested_mapped_parent_hierarchy() {
    let mut reporter = NullReporter;
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join(".atc-workspace.toml"),
        concat!(
            "version = 1\n",
            "[[paths]]\n",
            "pattern = \"^abc[0-9]+$\"\n",
            "path = \"AtCoder/Contests/ABC\"\n",
        ),
    )
    .unwrap();
    let destination = workspace::resolve_contest_path(temp.path(), "abc466").unwrap();
    let client = atcoder::AtCoderClient::fixture(fixture_root());

    new_at_in_workspace(
        temp.path(),
        &destination,
        "abc466",
        Language::Cpp,
        builtin_template(Language::Cpp),
        &client,
        &mut reporter,
    )
    .unwrap();

    assert_eq!(
        destination,
        temp.path()
            .join("AtCoder")
            .join("Contests")
            .join("ABC")
            .join("abc466")
    );
    assert_eq!(
        workspace::load_metadata(&destination).unwrap().contest_id,
        "abc466"
    );
}

#[test]
fn truncated_candidate_output_cannot_be_accepted_as_displayed_prefix() {
    let sample = Sample {
        input: String::new(),
        output: "prefix\n".to_string(),
    };
    let mut result = execution(ExecutionOutcome::Exited(exit_status(0)), "prefix\n", "");
    result.stdout_truncated = true;
    let mut reporter = RecordingReporter::default();

    let verdict = report_case_result(1, &sample, &result, false, &mut reporter);

    assert!(matches!(verdict, test::CaseVerdict::WrongAnswer));
    assert!(
        reporter
            .events
            .iter()
            .any(|event| event == "case-wrong-answer:1")
    );
}

#[test]
fn new_at_rejects_a_symlinked_contest_destination() {
    let temp = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    if !create_directory_symlink(external.path(), &destination) {
        return;
    }
    let client = atcoder::AtCoderClient::fixture(temp.path().join("missing-fixtures"));
    let mut reporter = NullReporter;

    let error = new_at(
        &destination,
        "abc466",
        Language::Cpp,
        builtin_template(Language::Cpp),
        &client,
        &mut reporter,
    )
    .expect_err("a symlinked destination must fail before fixture access");

    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == io::ErrorKind::InvalidInput
    ));
    assert!(std::fs::read_dir(external.path()).unwrap().next().is_none());
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
        builtin_template(Language::Python),
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
fn new_flow_uses_the_cli_selected_override_and_ignores_a_broken_unselected_override() {
    let mut reporter = NullReporter;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc500");
    let templates_dir = temp.path().join("templates");
    std::fs::create_dir(&templates_dir).unwrap();
    std::fs::create_dir(templates_dir.join("cpp.cpp")).unwrap();
    std::fs::write(templates_dir.join("python.py"), "print('custom new')\n").unwrap();
    let prepared = prepare_new(
        &destination,
        Some(Language::Python),
        || Ok(Config::default()),
        |language| resolve_source_template_in(&templates_dir, language),
    )
    .unwrap()
    .expect("missing destination should be prepared");
    let fixtures = temp.path().join("fixtures");
    create_single_problem_fixture(&fixtures, "abc500");
    let client = atcoder::AtCoderClient::fixture(&fixtures);

    new_at(
        &destination,
        "abc500",
        prepared.0,
        &prepared.1,
        &client,
        &mut reporter,
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(destination.join("A.py")).unwrap(),
        "print('custom new')\n"
    );
    assert!(!destination.join("A.cpp").exists());
}

#[test]
fn new_selected_broken_override_errors_during_local_preparation() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc500");
    let templates_dir = temp.path().join("templates");
    std::fs::create_dir(&templates_dir).unwrap();
    std::fs::create_dir(templates_dir.join("cpp.cpp")).unwrap();

    let error = prepare_new(
        &destination,
        None,
        || Ok(Config::default()),
        |language| resolve_source_template_in(&templates_dir, language),
    )
    .expect_err("broken selected override must fail before a client is created");

    assert!(error.to_string().contains("regular file"));
    assert!(!destination.exists());
}

#[test]
fn new_existing_destination_short_circuits_before_config_and_template_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc500");
    std::fs::create_dir(&destination).unwrap();

    let prepared = prepare_new(
        &destination,
        None,
        || -> Result<Config, AppError> { panic!("config must not be loaded") },
        |_| -> Result<String, AppError> { panic!("template must not be resolved") },
    )
    .unwrap();

    assert!(prepared.is_none());
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
        builtin_template(Language::Cpp),
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
        builtin_template(Language::Cpp),
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
    std::fs::write(stale_tests.join("sample-99.in"), "stale")
        .expect("stale input should be written");
    std::fs::write(stale_tests.join("sample-99.out"), "stale")
        .expect("stale output should be written");

    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = RecordingReporter::default();
    refresh_at(&destination, "abc466", false, &client, &mut reporter)
        .expect("fixture refresh should succeed");

    assert_eq!(
        std::fs::read_to_string(destination.join("A.cpp")).expect("source should remain readable"),
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
    assert_eq!(
        contest
            .problems
            .iter()
            .map(|problem| (problem.index.as_str(), problem.sample_count))
            .collect::<Vec<_>>(),
        [
            ("A", 3),
            ("B", 2),
            ("C", 0),
            ("D", 2),
            ("E", 3),
            ("F", 1),
            ("G", 3)
        ]
    );
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
fn prepared_refresh_owns_staging_without_mutating_live_data() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    create_workspace(&destination, "abc466");
    let old_tests = destination.join("tests/OLD");
    std::fs::create_dir_all(&old_tests).unwrap();
    std::fs::write(old_tests.join("sample-1.in"), "old input\n").unwrap();
    std::fs::write(old_tests.join("sample-1.out"), "old output\n").unwrap();
    let metadata_before = std::fs::read(destination.join(".atc/contest.toml")).unwrap();
    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = RecordingReporter::default();

    let prepared = prepare_refresh(&destination, "abc466", false, &client, &mut reporter)
        .expect("fixture refresh preparation should succeed");
    let staging = prepared.staging_path().to_path_buf();

    assert!(staging.is_dir());
    assert_eq!(
        std::fs::read(destination.join(".atc/contest.toml")).unwrap(),
        metadata_before
    );
    assert_eq!(
        std::fs::read_to_string(old_tests.join("sample-1.in")).unwrap(),
        "old input\n"
    );
    assert!(!destination.join("tests/A").exists());
    assert!(
        !reporter
            .events
            .iter()
            .any(|event| event.starts_with("refreshed:"))
    );

    drop(prepared);

    assert!(
        staging.is_dir(),
        "prepared data remains for recovery review"
    );
    assert_eq!(
        std::fs::read(destination.join(".atc/contest.toml")).unwrap(),
        metadata_before
    );
    assert_eq!(
        std::fs::read_to_string(old_tests.join("sample-1.out")).unwrap(),
        "old output\n"
    );
}

#[test]
fn applying_prepared_refresh_installs_data_and_reports_completion() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    create_workspace(&destination, "abc466");
    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = RecordingReporter::default();

    let prepared = prepare_refresh(&destination, "abc466", false, &client, &mut reporter)
        .expect("fixture refresh preparation should succeed");
    assert!(
        !reporter
            .events
            .iter()
            .any(|event| event.starts_with("refreshed:"))
    );

    apply_refresh(prepared, &mut reporter).expect("prepared refresh should apply");

    assert_eq!(
        workspace::load_metadata(&destination)
            .unwrap()
            .problems
            .len(),
        7
    );
    assert!(destination.join("tests/A/sample-1.in").is_file());
    assert_eq!(
        reporter
            .events
            .iter()
            .filter(|event| event.starts_with("refreshed:"))
            .count(),
        1
    );
}

#[test]
fn prepared_refresh_rejects_destination_metadata_changes_before_apply() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    create_workspace(&destination, "abc466");
    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = RecordingReporter::default();
    let prepared = prepare_refresh(&destination, "abc466", false, &client, &mut reporter)
        .expect("fixture refresh preparation should succeed");
    let changed = old_contest("arc001");
    workspace::save_metadata(&destination, &changed).unwrap();
    let changed_bytes = std::fs::read(destination.join(".atc/contest.toml")).unwrap();

    let error = apply_refresh(prepared, &mut reporter)
        .expect_err("changed destination metadata must invalidate prepared data");

    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == io::ErrorKind::Interrupted
    ));
    assert_eq!(
        std::fs::read(destination.join(".atc/contest.toml")).unwrap(),
        changed_bytes
    );
    assert!(!destination.join("tests/A").exists());
    assert!(
        !reporter
            .events
            .iter()
            .any(|event| event.starts_with("refreshed:"))
    );
}

#[test]
fn prepared_refresh_rejects_a_replaced_destination_with_identical_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    create_workspace(&destination, "abc466");
    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = RecordingReporter::default();
    let prepared = prepare_refresh(&destination, "abc466", false, &client, &mut reporter)
        .expect("fixture refresh preparation should succeed");
    let staging_name = prepared
        .staging_path()
        .file_name()
        .expect("staging directory should have a name")
        .to_owned();

    let displaced = temp.path().join("displaced-abc466");
    std::fs::rename(&destination, &displaced).unwrap();
    std::fs::create_dir(&destination).unwrap();
    std::fs::create_dir(destination.join(".atc")).unwrap();
    std::fs::copy(
        displaced.join(".atc/contest.toml"),
        destination.join(".atc/contest.toml"),
    )
    .unwrap();
    let replacement_tests = destination.join("tests/OLD");
    std::fs::create_dir_all(&replacement_tests).unwrap();
    std::fs::write(replacement_tests.join("sample-1.in"), "replacement input\n").unwrap();
    std::fs::write(
        replacement_tests.join("sample-1.out"),
        "replacement output\n",
    )
    .unwrap();

    // Keep the prepared staging reachable at its owned path. This proves the
    // rejection comes from physical destination identity, not a missing stage.
    std::fs::rename(
        displaced.join(&staging_name),
        destination.join(&staging_name),
    )
    .unwrap();
    let replacement_metadata = std::fs::read(destination.join(".atc/contest.toml")).unwrap();

    let error = apply_refresh(prepared, &mut reporter)
        .expect_err("a physically replaced destination must invalidate prepared data");

    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == io::ErrorKind::Interrupted
    ));
    assert_eq!(
        std::fs::read(destination.join(".atc/contest.toml")).unwrap(),
        replacement_metadata
    );
    assert_eq!(
        std::fs::read_to_string(replacement_tests.join("sample-1.in")).unwrap(),
        "replacement input\n"
    );
    assert!(!destination.join("tests/A").exists());
    assert!(destination.join(staging_name).is_dir());
    assert!(displaced.join(".atc/contest.toml").is_file());
    assert!(
        !reporter
            .events
            .iter()
            .any(|event| event.starts_with("refreshed:"))
    );
}

#[test]
fn refresh_sample_failure_is_fatal_and_preserves_old_metadata_and_tests() {
    let temp = tempfile::tempdir().expect("temporary directory should be created");
    let destination = temp.path().join("mini");
    let mut contest = old_contest("mini");
    contest.problems[0] = crate::model::Problem {
        index: "B".into(),
        title: "B".into(),
        task_id: "mini_b".into(),
        url: "https://atcoder.jp/contests/mini/tasks/mini_b".into(),
        sample_count: 2,
    };
    workspace::save_metadata(&destination, &contest).unwrap();
    let old_b_tests = destination.join("tests").join("B");
    std::fs::create_dir_all(&old_b_tests).expect("old tests should be created");
    std::fs::write(old_b_tests.join("sample-1.in"), "old").expect("old input should be written");
    std::fs::write(old_b_tests.join("sample-1.out"), "old").expect("old output should be written");
    let old_metadata = std::fs::read(destination.join(".atc/contest.toml")).unwrap();
    let before = crate::safe_file::read_tree_snapshot(&destination).unwrap();

    let fixtures = temp.path().join("fixtures");
    std::fs::create_dir_all(fixtures.join("contests")).expect("contest fixtures should be created");
    std::fs::create_dir_all(fixtures.join("problems")).expect("problem fixtures should be created");
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

    let mut reporter = RecordingReporter::default();
    let error = refresh_from(
        &destination,
        None,
        false,
        || Ok(atcoder::AtCoderClient::fixture(&fixtures)),
        &mut reporter,
    )
    .expect_err("a partial remote snapshot must fail");

    assert!(matches!(error, AppError::AtCoder(_)));
    assert_eq!(
        crate::safe_file::read_tree_snapshot(&destination).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read(destination.join(".atc/contest.toml")).unwrap(),
        old_metadata
    );
    assert!(!destination.join("tests").join("A").exists());
    assert_eq!(
        std::fs::read_to_string(old_b_tests.join("sample-1.in")).unwrap(),
        "old"
    );
    assert_eq!(
        std::fs::read_to_string(old_b_tests.join("sample-1.out")).unwrap(),
        "old"
    );
    assert!(
        reporter
            .events
            .iter()
            .any(|event| event == "problem-failed:B")
    );
    assert!(
        !reporter
            .events
            .iter()
            .any(|event| event.starts_with("refreshed:"))
    );
}

#[test]
fn create_problem_fetch_failure_installs_no_destination() {
    let temp = tempfile::tempdir().unwrap();
    let fixtures = temp.path().join("fixtures");
    std::fs::create_dir_all(fixtures.join("contests")).unwrap();
    std::fs::create_dir_all(fixtures.join("problems")).unwrap();
    std::fs::write(
        fixtures.join("contests/mini.html"),
        r#"<table><tbody>
            <tr><td><a href="/contests/mini/tasks/mini_a">A</a></td><td><a href="/contests/mini/tasks/mini_a">A</a></td></tr>
            <tr><td><a href="/contests/mini/tasks/mini_b">B</a></td><td><a href="/contests/mini/tasks/mini_b">B</a></td></tr>
        </tbody></table>"#,
    )
    .unwrap();
    std::fs::write(
        fixtures.join("problems/mini_a.html"),
        r#"<div id="task-statement"><span class="lang-en">
            <div class="part"><section><h3>Sample Input 1</h3><pre>1
</pre></section></div>
            <div class="part"><section><h3>Sample Output 1</h3><pre>2
</pre></section></div>
        </span></div>"#,
    )
    .unwrap();
    let destination = temp.path().join("mini");
    let client = atcoder::AtCoderClient::fixture(&fixtures);
    let mut reporter = RecordingReporter::default();

    let error = new_at(
        &destination,
        "mini",
        Language::Cpp,
        builtin_template(Language::Cpp),
        &client,
        &mut reporter,
    )
    .expect_err("incomplete authoritative fetch must prevent creation");

    assert!(matches!(error, AppError::AtCoder(_)));
    assert!(!destination.exists());
    assert!(
        reporter
            .events
            .iter()
            .any(|event| event == "problem-failed:B")
    );
    assert!(
        !reporter
            .events
            .iter()
            .any(|event| event.starts_with("created:"))
    );
}

#[test]
fn strict_fetch_builds_authoritative_abc466_sample_counts() {
    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = NullReporter;

    let fetched = fetch_contest_data("abc466", &client, &mut reporter).unwrap();

    assert_eq!(
        fetched
            .contest
            .problems
            .iter()
            .map(|problem| (problem.index.as_str(), problem.sample_count))
            .collect::<Vec<_>>(),
        [
            ("A", 3),
            ("B", 2),
            ("C", 0),
            ("D", 2),
            ("E", 3),
            ("F", 1),
            ("G", 3)
        ]
    );
    assert_eq!(
        fetched
            .samples_by_problem
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [3, 2, 0, 2, 3, 1, 3]
    );
}

fn fixture_refresh_client() -> Result<atcoder::AtCoderClient, AppError> {
    Ok(atcoder::AtCoderClient::fixture(fixture_root()))
}

fn unexpected_refresh_client() -> Result<atcoder::AtCoderClient, AppError> {
    panic!("local refresh decision must not initialize an AtCoder client");
}

fn refresh_manifest(sample_count: usize) -> Contest {
    Contest {
        contest_id: "abc466".into(),
        problems: vec![crate::model::Problem {
            index: "A".into(),
            title: "A".into(),
            task_id: "abc466_a".into(),
            url: "https://atcoder.jp/contests/abc466/tasks/abc466_a".into(),
            sample_count,
        }],
    }
}

fn assert_refresh_downloaded_fixture(reporter: &RecordingReporter) {
    assert_eq!(
        reporter
            .events
            .iter()
            .filter(|event| event.starts_with("contest-fetching:"))
            .count(),
        1
    );
    assert_eq!(
        reporter
            .events
            .iter()
            .filter(|event| event.starts_with("problem-fetched:"))
            .count(),
        7
    );
}

#[test]
fn healthy_refresh_skips_client_and_all_writes_but_force_downloads() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    let contest = refresh_manifest(1);
    workspace::save_metadata(&destination, &contest).unwrap();
    let sample = Sample {
        input: "local input\n".into(),
        output: "local output\n".into(),
    };
    workspace::save_samples(&destination, &contest.problems[0], &[sample]).unwrap();
    std::fs::write(destination.join("A.cpp"), "user source").unwrap();
    std::fs::write(destination.join(".atc/notes.txt"), "user data").unwrap();
    let before = crate::safe_file::read_tree_snapshot(&destination).unwrap();
    let mut reporter = RecordingReporter::default();

    refresh_from(
        &destination,
        None,
        false,
        unexpected_refresh_client,
        &mut reporter,
    )
    .unwrap();

    assert_eq!(reporter.events, ["refresh-skipped:abc466"]);
    assert_eq!(
        crate::safe_file::read_tree_snapshot(&destination).unwrap(),
        before
    );

    let mut reporter = RecordingReporter::default();
    refresh_from(
        &destination,
        None,
        true,
        fixture_refresh_client,
        &mut reporter,
    )
    .unwrap();
    assert_refresh_downloaded_fixture(&reporter);
    assert_eq!(
        workspace::load_metadata(&destination).unwrap().problems[0].sample_count,
        3
    );
    assert_eq!(
        std::fs::read(destination.join("A.cpp")).unwrap(),
        b"user source"
    );
    assert_eq!(
        std::fs::read(destination.join(".atc/notes.txt")).unwrap(),
        b"user data"
    );
    let recovery = std::fs::read_dir(&destination)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|entry| {
            entry
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".atc-recovery-refresh-")
        })
        .expect("forced refresh should retain previous data");
    assert!(recovery.join(crate::safe_file::RECOVERY_COMPLETE).is_file());
    assert_eq!(
        std::fs::read(recovery.join("previous-tests/A/sample-1.in")).unwrap(),
        b"local input\n"
    );

    // Completed recovery archives remain readable and need no network on the next refresh.
    let mut reporter = RecordingReporter::default();
    refresh_from(
        &destination,
        None,
        false,
        unexpected_refresh_client,
        &mut reporter,
    )
    .unwrap();
    assert_eq!(reporter.events, ["refresh-skipped:abc466"]);
}

#[test]
fn ordinary_refresh_repairs_samples_even_when_the_official_count_changed() {
    use crate::workspace::diagnosis::{ContestDiagnosis, diagnose_contest};

    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    workspace::save_metadata(&destination, &refresh_manifest(2)).unwrap();
    assert!(matches!(
        diagnose_contest(&destination, "abc466"),
        ContestDiagnosis::RepairRequired(_)
    ));
    let mut reporter = RecordingReporter::default();

    refresh_from(
        &destination,
        None,
        false,
        fixture_refresh_client,
        &mut reporter,
    )
    .unwrap();

    assert_refresh_downloaded_fixture(&reporter);
    let ContestDiagnosis::Healthy { contest, .. } = diagnose_contest(&destination, "abc466") else {
        panic!("refreshed contest should be healthy");
    };
    assert_eq!(contest.problems[0].sample_count, 3);
    assert!(
        reporter
            .events
            .contains(&format!("refreshed:{}", destination.display()))
    );
}

#[test]
fn ordinary_refresh_uses_known_owner_from_an_invalid_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    std::fs::create_dir_all(destination.join(".atc")).unwrap();
    std::fs::write(
        destination.join(".atc/contest.toml"),
        "version = 1\ncontest_id = 'abc466'\nproblems = []\n",
    )
    .unwrap();

    refresh_from(
        &destination,
        None,
        false,
        fixture_refresh_client,
        &mut NullReporter,
    )
    .unwrap();
    assert_eq!(
        workspace::load_metadata(&destination)
            .unwrap()
            .problems
            .len(),
        7
    );
}

#[test]
fn refresh_refusals_leave_the_tree_unchanged_and_do_not_initialize_the_client() {
    for scenario in ["migration", "unowned", "recovery", "wrong-id", "missing"] {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        let expected_reason = match scenario {
            "migration" => {
                std::fs::create_dir_all(destination.join(".atc")).unwrap();
                std::fs::write(
                    destination.join(".atc/contest.toml"),
                    "version = 99\ncontest_id = 'abc466'\n",
                )
                .unwrap();
                "inspect with `atc migrate --dry-run`"
            }
            "unowned" => {
                workspace::save_metadata(&destination, &refresh_manifest(0)).unwrap();
                std::fs::create_dir(destination.join("tests")).unwrap();
                std::fs::write(destination.join("tests/notes.txt"), "user data").unwrap();
                "unowned entry in tests"
            }
            "recovery" => {
                workspace::save_metadata(&destination, &refresh_manifest(0)).unwrap();
                let staging = destination.join(".atc-refresh-interrupted");
                std::fs::create_dir(&staging).unwrap();
                std::fs::write(staging.join("previous-contest.toml"), "previous data").unwrap();
                "contest update requires recovery check"
            }
            "wrong-id" => {
                workspace::save_metadata(&destination, &old_contest("arc001")).unwrap();
                "contest ID mismatch"
            }
            "missing" => "contest directory not found",
            _ => unreachable!(),
        };
        let before = crate::safe_file::read_tree_snapshot(temp.path()).unwrap();
        for force in [false, true] {
            let error = refresh_from(
                temp.path(),
                Some("abc466"),
                force,
                unexpected_refresh_client,
                &mut NullReporter,
            )
            .expect_err("local diagnosis should refuse refresh");
            assert!(
                error.to_string().contains(expected_reason),
                "{scenario}: {error}"
            );
            assert_eq!(
                crate::safe_file::read_tree_snapshot(temp.path()).unwrap(),
                before
            );
        }
    }
}

#[test]
fn cli_refresh_aborts_before_staging_if_metadata_or_diagnosis_changes_during_fetch() {
    struct ChangeDuringFetch<F>(Option<F>);
    impl<F: FnOnce()> Reporter for ChangeDuringFetch<F> {
        fn report(&mut self, event: Event<'_>) {
            if matches!(event, Event::ProblemFetched { index: "G", .. }) {
                self.0.take().unwrap()();
            }
        }
    }

    for change_metadata in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        let contest = refresh_manifest(1);
        workspace::save_metadata(&destination, &contest).unwrap();
        let metadata = destination.join(".atc/contest.toml");
        let original = std::fs::read_to_string(&metadata).unwrap();
        let changed = format!("# concurrent edit\n{original}");
        let mut reporter = ChangeDuringFetch(Some(|| {
            if change_metadata {
                // Keep the diagnosis identical while changing the metadata bytes.
                std::fs::write(&metadata, &changed).unwrap();
            } else {
                let sample = Sample {
                    input: "concurrent input\n".into(),
                    output: "concurrent output\n".into(),
                };
                workspace::save_samples(&destination, &contest.problems[0], &[sample]).unwrap();
            }
        }));

        let error = refresh_from(
            &destination,
            None,
            false,
            fixture_refresh_client,
            &mut reporter,
        )
        .expect_err("a changed contest must invalidate refresh preparation");

        assert!(reporter.0.is_none());
        let files = std::fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        if change_metadata {
            assert!(error.to_string().contains("contest metadata changed"));
            assert_eq!(std::fs::read_to_string(&metadata).unwrap(), changed);
            assert_eq!(
                files.len(),
                1,
                "no staging or locks should have been created"
            );
        } else {
            assert!(
                error
                    .to_string()
                    .contains("local contest diagnosis changed")
            );
            assert_eq!(std::fs::read_to_string(&metadata).unwrap(), original);
            assert_eq!(
                std::fs::read(destination.join("tests/A/sample-1.in")).unwrap(),
                b"concurrent input\n"
            );
            assert_eq!(
                files.len(),
                2,
                "only the concurrent sample change is allowed"
            );
        }
    }
}

#[test]
fn refresh_without_override_does_not_infer_id_from_directory_name() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    std::fs::create_dir_all(destination.join(".atc")).unwrap();
    let metadata = destination.join(".atc/contest.toml");

    let error = refresh_from(
        &destination,
        None,
        false,
        unexpected_refresh_client,
        &mut NullReporter,
    )
    .expect_err("ordinary refresh needs a known contest ID");
    assert!(error.to_string().contains("atc refresh -c"));

    std::fs::write(&metadata, "version = ???").unwrap();
    let error = refresh_from(
        &destination,
        None,
        false,
        unexpected_refresh_client,
        &mut NullReporter,
    )
    .expect_err("broken metadata must not fall back to the directory name");
    assert!(matches!(error, AppError::Io(source) if source.kind() == io::ErrorKind::InvalidData));
    assert_eq!(std::fs::read(&metadata).unwrap(), b"version = ???");
}

#[test]
fn refresh_override_requires_workspace_marker() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    std::fs::create_dir(&destination).unwrap();

    let error = refresh_from(
        temp.path(),
        Some("abc466"),
        false,
        unexpected_refresh_client,
        &mut NullReporter,
    )
    .expect_err("an arbitrary matching directory must be rejected");
    assert!(error.to_string().contains("workspace marker not found"));
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
}

#[test]
fn forced_refresh_refuses_markerless_tests_without_touching_sources() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    let stale_tests = destination.join("tests/A");
    std::fs::create_dir_all(&stale_tests).unwrap();
    std::fs::write(stale_tests.join("sample-99.in"), "stale").unwrap();
    std::fs::write(stale_tests.join("sample-99.out"), "stale").unwrap();
    std::fs::write(destination.join("A.cpp"), "user source").unwrap();
    let before = crate::safe_file::read_tree_snapshot(&destination).unwrap();

    let error = refresh_from(
        &destination,
        None,
        true,
        unexpected_refresh_client,
        &mut NullReporter,
    )
    .expect_err("force must not claim tests without an old manifest");
    assert!(
        error
            .to_string()
            .contains("cannot confirm ownership of existing tests")
    );
    assert_eq!(
        crate::safe_file::read_tree_snapshot(&destination).unwrap(),
        before
    );
}

#[test]
fn forced_refresh_reconstructs_a_markerless_workspace_without_existing_tests() {
    for marker in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).unwrap();
        if marker {
            std::fs::create_dir(destination.join(".atc")).unwrap();
            std::fs::write(
                destination.join(".atc/config.toml"),
                b"language = \"cpp\"\n",
            )
            .unwrap();
        }
        std::fs::write(destination.join("A.cpp"), "user source").unwrap();
        let mut reporter = RecordingReporter::default();

        refresh_from(
            &destination,
            None,
            true,
            fixture_refresh_client,
            &mut reporter,
        )
        .expect("force can reconstruct a contest without existing metadata or tests");

        assert_eq!(
            workspace::load_metadata(&destination).unwrap().contest_id,
            "abc466"
        );
        assert!(destination.join("tests/A/sample-1.in").is_file());
        assert_eq!(
            std::fs::read(destination.join("A.cpp")).unwrap(),
            b"user source"
        );
        if marker {
            assert_eq!(
                std::fs::read(destination.join(".atc/config.toml")).unwrap(),
                b"language = \"cpp\"\n"
            );
        }
        assert!(
            reporter
                .events
                .contains(&format!("refreshed:{}", destination.display()))
        );
    }
}

#[test]
fn refresh_override_preserves_malformed_metadata_inside_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    std::fs::create_dir_all(destination.join(".atc")).unwrap();
    let metadata = destination.join(".atc/contest.toml");
    std::fs::write(&metadata, "version = ???").unwrap();
    let before = crate::safe_file::read_tree_snapshot(&destination).unwrap();

    for force in [false, true] {
        let error = refresh_from(
            temp.path(),
            Some("abc466"),
            force,
            unexpected_refresh_client,
            &mut NullReporter,
        )
        .expect_err("unknown metadata ownership must block refresh");
        assert!(error.to_string().contains("cannot confirm ownership"));
        assert_eq!(
            crate::safe_file::read_tree_snapshot(&destination).unwrap(),
            before
        );
    }
}

#[test]
fn forced_refresh_of_unknown_directory_is_a_fetch_error_without_partial_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("not-a-contest");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("main.cpp"), "user source").unwrap();
    let before = crate::safe_file::read_tree_snapshot(&destination).unwrap();
    let mut reporter = RecordingReporter::default();

    let error = refresh_from(
        &destination,
        None,
        true,
        fixture_refresh_client,
        &mut reporter,
    )
    .expect_err("a missing contest fixture must be a fetch error");

    assert!(matches!(error, AppError::AtCoder(_)));
    assert_eq!(reporter.events, ["contest-fetching:not-a-contest"]);
    assert_eq!(
        crate::safe_file::read_tree_snapshot(&destination).unwrap(),
        before
    );
}

#[test]
fn forced_refresh_refuses_unsafe_tests_without_leaving_a_workspace_marker() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("A.cpp"), "user source").unwrap();
    std::fs::write(destination.join("tests"), "user-owned file").unwrap();
    let before = crate::safe_file::read_tree_snapshot(&destination).unwrap();

    let error = refresh_from(
        &destination,
        None,
        true,
        unexpected_refresh_client,
        &mut NullReporter,
    )
    .expect_err("an unsafe existing tests path must fail");

    assert!(matches!(error, AppError::Io(source) if source.kind() == io::ErrorKind::InvalidInput));
    assert_eq!(
        crate::safe_file::read_tree_snapshot(&destination).unwrap(),
        before
    );
}

#[test]
fn refresh_override_recovers_missing_metadata_inside_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    std::fs::create_dir_all(destination.join(".atc")).unwrap();

    refresh_from(
        temp.path(),
        Some("abc466"),
        false,
        fixture_refresh_client,
        &mut NullReporter,
    )
    .expect("explicit ID should allow reconstruction of missing metadata");

    assert_eq!(
        workspace::load_metadata(&destination).unwrap().contest_id,
        "abc466"
    );
    assert!(destination.join("tests/A/sample-1.in").is_file());
}

#[test]
fn refresh_rejects_a_directory_name_that_differs_from_the_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("other");
    create_workspace(&destination, "abc466");
    let before = crate::safe_file::read_tree_snapshot(&destination).unwrap();

    for force in [false, true] {
        refresh_from(
            &destination,
            None,
            force,
            unexpected_refresh_client,
            &mut NullReporter,
        )
        .expect_err("the directory name and manifest ID must agree");
        assert_eq!(
            crate::safe_file::read_tree_snapshot(&destination).unwrap(),
            before
        );
    }
}

#[test]
fn forced_refresh_rejects_unsafe_contest_ids_and_invalid_markers() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    std::fs::create_dir(&destination).unwrap();

    let error = refresh_from(
        temp.path(),
        Some("../abc466"),
        true,
        unexpected_refresh_client,
        &mut NullReporter,
    )
    .expect_err("force must not bypass contest path validation");
    assert!(matches!(error, AppError::Io(source) if source.kind() == io::ErrorKind::InvalidInput));

    std::fs::write(destination.join(".atc"), "not a directory").unwrap();
    let error = refresh_from(
        temp.path(),
        Some("abc466"),
        true,
        unexpected_refresh_client,
        &mut NullReporter,
    )
    .expect_err("force must only permit a missing marker");
    assert!(matches!(error, AppError::Io(source) if source.kind() == io::ErrorKind::InvalidInput));
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

    refresh_at(&destination, "broken", false, &client, &mut reporter)
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
