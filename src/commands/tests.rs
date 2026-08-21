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
            Event::WatchStarted { destination } => {
                format!("watch-started:{}", destination.display())
            }

            Event::WatchSourceChanged { source } => {
                format!("watch-source-changed:{}", source.display())
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
                expected,
                actual,
            } => {
                format!("case-comparison:{number}:expected={expected:?}:actual={actual:?}")
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
    assert!(reporter.events[2].contains("expected="));
    assert!(reporter.events[2].contains("actual="));

    assert!(reporter.events[3].starts_with("case-stderr:1:"));
    assert!(reporter.events[3].contains("x = 7"));

    assert_eq!(reporter.events[4], "test-finished:A:1:1");
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
            r#"case-comparison:1:expected="expected\n":actual="wrong\n""#,
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
            r#"case-comparison:2:expected="expected\n":actual="wrong\n""#,
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
fn new_at_creates_a_missing_mapped_parent_directory() {
    let mut reporter = NullReporter;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc").join("abc466");
    let client = atcoder::AtCoderClient::fixture(fixture_root());

    new_at(
        &destination,
        "abc466",
        Language::Cpp,
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

    let client = atcoder::AtCoderClient::fixture(&fixtures);
    let mut reporter = RecordingReporter::default();
    refresh_at(&destination, "mini", false, &client, &mut reporter)
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

    let error = resolve_refresh_contest_id(&destination, None, false)
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

    let error = resolve_refresh_contest_id(&destination, Some("abc466"), false)
        .expect_err("an arbitrary matching directory must be rejected");

    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidInput
    ));

    assert_eq!(
        resolve_refresh_contest_id(&destination, None, true)
            .expect("force should infer the contest ID from the directory name"),
        "abc466"
    );
}

#[test]
fn forced_refresh_reconstructs_a_markerless_workspace_without_touching_sources() {
    let temp = tempfile::tempdir().expect("temporary directory should be created");
    let destination = temp.path().join("abc350");
    std::fs::create_dir(&destination).expect("old contest directory should be created");
    std::fs::write(destination.join("A.cpp"), "user source").expect("source should be written");
    let stale_tests = destination.join("tests").join("OLD");
    std::fs::create_dir_all(&stale_tests).expect("stale tests should be created");
    std::fs::write(stale_tests.join("old.in"), "stale").expect("stale sample should be written");

    let fixtures = temp.path().join("fixtures");
    create_single_problem_fixture(&fixtures, "abc350");

    let contest_id = resolve_refresh_contest_id(&destination, None, true)
        .expect("force should infer a missing workspace from its directory name");
    let client = atcoder::AtCoderClient::fixture(&fixtures);
    let mut reporter = RecordingReporter::default();

    refresh_at(&destination, &contest_id, true, &client, &mut reporter)
        .expect("forced refresh should succeed from fixtures");

    assert!(destination.join(".atc").is_dir());
    assert_eq!(
        workspace::load_metadata(&destination)
            .expect("new metadata should load")
            .contest_id,
        "abc350"
    );
    assert!(
        destination
            .join("tests")
            .join("A")
            .join("sample-1.in")
            .is_file()
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("tests").join("A").join("sample-1.in"))
            .expect("sample input should remain readable"),
        "ABC349\n"
    );
    assert!(!destination.join("tests").join("OLD").exists());
    assert_eq!(
        std::fs::read_to_string(destination.join("A.cpp")).expect("source should remain readable"),
        "user source"
    );
    assert!(
        reporter
            .events
            .contains(&format!("refreshed:{}", destination.display()))
    );
}

#[test]
fn forced_refresh_rebuilds_versionless_metadata_using_the_directory_name() {
    let temp = tempfile::tempdir().expect("temporary directory should be created");
    let destination = temp.path().join("abc350");
    std::fs::create_dir(&destination).expect("old contest directory should be created");
    std::fs::create_dir(destination.join(".atc")).expect("old marker should be created");
    std::fs::write(
        destination.join(".atc").join("config.toml"),
        "language = \"cpp\"\n",
    )
    .expect("versionless legacy metadata should be written");
    std::fs::write(destination.join("A.py"), "user source").expect("source should be written");
    let fixtures = temp.path().join("fixtures");
    create_single_problem_fixture(&fixtures, "abc350");

    let error = resolve_refresh_contest_id(&destination, None, false)
        .expect_err("normal refresh must reject versionless metadata");
    assert!(matches!(error, AppError::Io(_)));

    let contest_id = resolve_refresh_contest_id(&destination, None, true)
        .expect("force must not read the old metadata");
    assert_eq!(contest_id, "abc350");

    let client = atcoder::AtCoderClient::fixture(&fixtures);
    let mut reporter = NullReporter;
    refresh_at(&destination, &contest_id, true, &client, &mut reporter)
        .expect("forced refresh should replace versionless metadata");

    assert_eq!(
        workspace::load_metadata(&destination)
            .expect("rebuilt metadata should load")
            .contest_id,
        "abc350"
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("A.py")).expect("source should remain readable"),
        "user source"
    );
}

#[test]
fn forced_refresh_with_override_uses_the_cli_id_instead_of_malformed_metadata() {
    let temp = tempfile::tempdir().expect("temporary directory should be created");
    let destination = temp.path().join("abc350");
    std::fs::create_dir(&destination).expect("old contest directory should be created");
    std::fs::create_dir(destination.join(".atc")).expect("old marker should be created");
    std::fs::write(
        destination.join(".atc").join("contest.toml"),
        "version = ???",
    )
    .expect("malformed metadata should be written");
    let fixtures = temp.path().join("fixtures");
    create_single_problem_fixture(&fixtures, "abc350");

    let contest_id = resolve_refresh_contest_id(&destination, Some("abc350"), true)
        .expect("force should use the CLI contest ID without reading metadata");
    assert_eq!(contest_id, "abc350");

    let client = atcoder::AtCoderClient::fixture(&fixtures);
    let mut reporter = NullReporter;
    refresh_at(&destination, &contest_id, true, &client, &mut reporter)
        .expect("forced refresh should replace malformed metadata");

    assert_eq!(
        workspace::load_metadata(&destination)
            .expect("rebuilt metadata should load")
            .contest_id,
        "abc350"
    );
}

#[test]
fn forced_refresh_of_unknown_directory_is_a_fetch_error_without_partial_workspace() {
    let temp = tempfile::tempdir().expect("temporary directory should be created");
    let destination = temp.path().join("not-a-contest");
    std::fs::create_dir(&destination).expect("legacy directory should be created");
    std::fs::write(destination.join("main.cpp"), "user source").expect("source should be written");
    let fixtures = temp.path().join("fixtures");
    std::fs::create_dir_all(fixtures.join("contests"))
        .expect("empty contest fixture directory should be created");
    let contest_id = resolve_refresh_contest_id(&destination, None, true)
        .expect("force should infer the directory name before fetching");
    assert_eq!(contest_id, "not-a-contest");

    let client = atcoder::AtCoderClient::fixture(&fixtures);
    let mut reporter = RecordingReporter::default();
    let error = refresh_at(&destination, &contest_id, true, &client, &mut reporter)
        .expect_err("a missing contest fixture must be a fetch error");

    assert!(matches!(error, AppError::AtCoder(_)));
    assert!(!destination.join(".atc").exists());
    assert!(!destination.join("tests").exists());
    assert_eq!(
        std::fs::read_to_string(destination.join("main.cpp"))
            .expect("source should remain readable"),
        "user source"
    );
    assert!(
        std::fs::read_dir(&destination)
            .expect("legacy directory should remain readable")
            .all(|entry| !entry
                .expect("directory entry should be readable")
                .file_name()
                .to_string_lossy()
                .starts_with(".atc-refresh-"))
    );
    assert!(
        !reporter
            .events
            .iter()
            .any(|event| event.starts_with("refreshed:"))
    );
}

#[test]
fn failed_forced_refresh_does_not_leave_a_workspace_marker() {
    let temp = tempfile::tempdir().expect("temporary directory should be created");
    let destination = temp.path().join("abc466");
    std::fs::create_dir(&destination).expect("old contest directory should be created");
    std::fs::write(destination.join("A.cpp"), "user source").expect("source should be written");
    std::fs::write(destination.join("tests"), "user-owned file")
        .expect("conflicting tests file should be written");
    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = RecordingReporter::default();

    let error = refresh_at(&destination, "abc466", true, &client, &mut reporter)
        .expect_err("an unsafe existing tests path must fail");

    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidInput
    ));
    assert!(!destination.join(".atc").exists());
    assert_eq!(
        std::fs::read_to_string(destination.join("tests"))
            .expect("existing tests file should remain readable"),
        "user-owned file"
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("A.cpp")).expect("source should remain readable"),
        "user source"
    );
    assert!(
        !reporter
            .events
            .iter()
            .any(|event| event.starts_with("refreshed:"))
    );
}

#[test]
fn refresh_override_recovers_missing_metadata_inside_workspace() {
    let temp = tempfile::tempdir().expect("temporary directory should be created");
    let destination = temp.path().join("abc466");
    std::fs::create_dir(&destination).expect("directory should be created");
    std::fs::create_dir(destination.join(".atc")).expect("marker should be created");

    let contest_id = resolve_refresh_contest_id(&destination, Some("abc466"), false)
        .expect("override should not require contest.toml");

    assert_eq!(contest_id, "abc466");

    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = NullReporter;
    refresh_at(&destination, &contest_id, false, &client, &mut reporter)
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

    let contest_id = resolve_refresh_contest_id(&destination, Some("abc466"), false)
        .expect("override should ignore malformed contest.toml");
    let client = atcoder::AtCoderClient::fixture(fixture_root());
    let mut reporter = NullReporter;
    refresh_at(&destination, &contest_id, false, &client, &mut reporter)
        .expect("override refresh should replace malformed metadata");

    assert_eq!(
        workspace::load_metadata(&destination)
            .expect("reconstructed metadata should load")
            .contest_id,
        "abc466"
    );
}

#[test]
fn refresh_override_rejects_healthy_metadata_id_mismatch_and_newer_versions() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    create_workspace(&destination, "arc001");

    let error = resolve_refresh_contest_id(&destination, Some("abc466"), false)
        .expect_err("healthy metadata for a different contest must not be overwritten");
    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == io::ErrorKind::InvalidData
    ));

    std::fs::write(
        destination.join(".atc").join("contest.toml"),
        "version = 99\ncontest_id = \"abc466\"\nproblems = []\n",
    )
    .unwrap();
    let error = resolve_refresh_contest_id(&destination, Some("abc466"), false)
        .expect_err("a newer metadata schema must not be overwritten implicitly");
    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == io::ErrorKind::InvalidData
    ));
}

#[test]
fn refresh_override_rejects_wrong_directory_name() {
    let temp = tempfile::tempdir().expect("temporary directory should be created");
    let destination = temp.path().join("other");
    std::fs::create_dir(&destination).expect("directory should be created");
    std::fs::create_dir(destination.join(".atc")).expect("marker should be created");

    for force in [false, true] {
        let error = resolve_refresh_contest_id(&destination, Some("abc466"), force)
            .expect_err("override must match the directory name even with force");

        assert!(matches!(error, AppError::Io(_)));
    }
}

#[test]
fn forced_refresh_still_rejects_unsafe_contest_ids_and_invalid_markers() {
    let temp = tempfile::tempdir().expect("temporary directory should be created");
    let destination = temp.path().join("abc350");
    std::fs::create_dir(&destination).expect("directory should be created");

    let error = resolve_refresh_contest_id(&destination, Some("../abc350"), true)
        .expect_err("force must not bypass contest path validation");
    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidInput
    ));

    std::fs::write(destination.join(".atc"), "not a directory")
        .expect("invalid marker should be written");
    let error = resolve_refresh_contest_id(&destination, Some("abc350"), true)
        .expect_err("force must only permit a missing marker");
    assert!(matches!(
        error,
        AppError::Io(source) if source.kind() == std::io::ErrorKind::InvalidInput
    ));
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
