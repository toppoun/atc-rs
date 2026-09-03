use super::test::find_problem;
use crate::atcoder;
use crate::atcoder::submission_tracking::{SubmissionId, SubmissionStatus};
use crate::atcoder::submit::{SubmitOutcome, SubmitRequest};
use crate::config::Config;
use crate::error::AppError;
use crate::language::{Language, PythonRuntime, SubmissionTarget};
use crate::model::Problem;
use crate::workspace;

use std::cell::RefCell;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

const RUNTIME_PYTHON_ONLY_ERROR: &str = "--runtime is only valid for Python submissions";

pub(crate) fn submit(
    problem_index: &str,
    cli_contest: Option<&str>,
    cli_language: Option<Language>,
    cli_runtime: Option<PythonRuntime>,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let config = Config::load()?;
    let plan = resolve_submit_plan(
        &cwd,
        problem_index,
        cli_contest,
        cli_language,
        cli_runtime,
        config.submit.python_runtime,
    )?;
    let atcoder = atcoder::AtCoderClient::new()?;
    let stdout = io::stdout();

    execute_submit_with_tracking(
        plan,
        &mut stdout.lock(),
        read_source_file,
        |contest_id, task_id, language_id| {
            atcoder
                .capture_submission_baseline(contest_id, task_id, language_id)
                .ok()
        },
        |request, before_post| {
            Ok(atcoder.submit_with_before_post(request, |language_id| {
                before_post(language_id);
            })?)
        },
        |baseline| atcoder.discover_submission_id(baseline).map_err(drop),
        |contest_id, submission_id, on_status| {
            atcoder
                .watch_submission(contest_id, submission_id, on_status)
                .map_err(drop)
        },
    )
}

#[derive(Debug, PartialEq, Eq)]
struct ResolvedSource {
    language: Language,
    path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct SubmitPlan {
    contest_id: String,
    task_id: String,
    problem_index: String,
    source: ResolvedSource,
    target: SubmissionTarget,
}

fn resolve_submit_source(
    destination: &Path,
    problem: &Problem,
    specified_language: Option<Language>,
) -> io::Result<ResolvedSource> {
    // Submit intentionally does not consult Config::defaults. Ambiguity must be resolved by
    // an explicit -l/--language selection.
    if let Some(language) = specified_language {
        let path = workspace::source_file_path(destination, &problem.index, language)?;
        if source_file_exists(&path)? {
            return Ok(ResolvedSource { language, path });
        }

        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} source file not found for problem {}: {}",
                language_label(language),
                problem.index,
                path.display()
            ),
        ));
    }

    let cpp_path = workspace::source_file_path(destination, &problem.index, Language::Cpp)?;
    let python_path = workspace::source_file_path(destination, &problem.index, Language::Python)?;
    let cpp_exists = source_file_exists(&cpp_path)?;
    let python_exists = source_file_exists(&python_path)?;

    match (cpp_exists, python_exists) {
        (true, false) => Ok(ResolvedSource {
            language: Language::Cpp,
            path: cpp_path,
        }),
        (false, true) => Ok(ResolvedSource {
            language: Language::Python,
            path: python_path,
        }),
        (true, true) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Both C++ and Python sources exist for problem {}.\nSpecify a language with `-l cpp` or `-l python`.",
                problem.index
            ),
        )),
        (false, false) => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no C++ or Python source file found for problem {}",
                problem.index
            ),
        )),
    }
}

fn source_file_exists(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_source_file(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
}

fn language_label(language: Language) -> &'static str {
    match language {
        Language::Cpp => "C++",
        Language::Python => "Python",
    }
}

fn resolve_submission_target(
    source_language: Language,
    cli_runtime: Option<PythonRuntime>,
    configured_runtime: PythonRuntime,
) -> io::Result<SubmissionTarget> {
    match (source_language, cli_runtime) {
        (Language::Cpp, Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            RUNTIME_PYTHON_ONLY_ERROR,
        )),
        (Language::Cpp, None) => Ok(SubmissionTarget::Cpp),
        (Language::Python, runtime) => Ok(SubmissionTarget::Python(
            runtime.unwrap_or(configured_runtime),
        )),
    }
}

fn resolve_submit_plan(
    launch_root: &Path,
    problem_index: &str,
    cli_contest: Option<&str>,
    cli_language: Option<Language>,
    cli_runtime: Option<PythonRuntime>,
    configured_runtime: PythonRuntime,
) -> Result<SubmitPlan, AppError> {
    if cli_language == Some(Language::Cpp) && cli_runtime.is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, RUNTIME_PYTHON_ONLY_ERROR).into());
    }

    let destination = workspace::resolve_contest_target(launch_root, cli_contest)?;
    workspace::validate_workspace_marker(&destination)?;

    let contest = workspace::load_metadata(&destination)?;
    workspace::validate_contest_paths(&contest)?;
    if let Some(contest_id) = cli_contest {
        workspace::validate_contest_identity(&contest, contest_id)?;
    }

    let problem = find_problem(&contest, problem_index)?;
    let source = resolve_submit_source(&destination, problem, cli_language)?;
    let target = resolve_submission_target(source.language, cli_runtime, configured_runtime)?;

    Ok(SubmitPlan {
        contest_id: contest.contest_id.clone(),
        task_id: problem.task_id.clone(),
        problem_index: problem.index.clone(),
        source,
        target,
    })
}

#[cfg(test)]
fn execute_submit<R, S>(
    plan: SubmitPlan,
    output: &mut impl Write,
    read_source: R,
    submit_once: S,
) -> Result<(), AppError>
where
    R: FnOnce(&Path) -> io::Result<String>,
    S: FnOnce(SubmitRequest) -> Result<SubmitOutcome, AppError>,
{
    execute_submit_with_accepted(
        plan,
        output,
        read_source,
        submit_once,
        |_, _, problem_index, output| {
            let _ = writeln!(output, "Submitted: {problem_index}");
        },
    )
}

fn execute_submit_with_accepted<R, S, A, W>(
    plan: SubmitPlan,
    output: &mut W,
    read_source: R,
    submit_once: S,
    on_accepted: A,
) -> Result<(), AppError>
where
    R: FnOnce(&Path) -> io::Result<String>,
    S: FnOnce(SubmitRequest) -> Result<SubmitOutcome, AppError>,
    A: FnOnce(&str, &str, &str, &mut W),
    W: Write,
{
    let SubmitPlan {
        contest_id,
        task_id,
        problem_index,
        source,
        target,
    } = plan;

    // read_source is FnOnce and is called immediately before creating the owned request.
    // read_to_string preserves trailing newlines, CRLF, and Unicode bytes for valid UTF-8.
    let source_snapshot = read_source(&source.path)?;
    if source_snapshot.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("source file is empty: {}", source.path.display()),
        )
        .into());
    }

    let request = SubmitRequest::new(contest_id.clone(), task_id.clone(), target, source_snapshot);

    // FnOnce makes a CLI-level retry impossible: this invocation can call the backend once.
    match submit_once(request)? {
        SubmitOutcome::Accepted => {
            // The remote side effect is already confirmed. A local reporting failure must not
            // turn this into a failed command because callers commonly retry nonzero exits.
            on_accepted(&contest_id, &task_id, &problem_index, output);
            Ok(())
        }
        SubmitOutcome::UnknownSubmissionOutcome => Err(AppError::UnknownSubmissionOutcome),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_submit_with_tracking<R, S, B, C, D, P, W>(
    plan: SubmitPlan,
    output: &mut W,
    read_source: R,
    capture_baseline: C,
    submit_once: S,
    discover_submission: D,
    poll_status: P,
) -> Result<(), AppError>
where
    R: FnOnce(&Path) -> io::Result<String>,
    S: FnOnce(SubmitRequest, &mut dyn FnMut(&str)) -> Result<SubmitOutcome, AppError>,
    C: FnOnce(&str, &str, &str) -> Option<B>,
    D: FnOnce(&B) -> Result<SubmissionId, ()>,
    P: FnOnce(&str, SubmissionId, &mut dyn FnMut(&SubmissionStatus) -> bool) -> Result<(), ()>,
    W: Write,
{
    let contest_id = plan.contest_id.clone();
    let task_id = plan.task_id.clone();
    let baseline = Rc::new(RefCell::new(None));
    let baseline_before_post = Rc::clone(&baseline);
    let baseline_after_acceptance = Rc::clone(&baseline);
    let mut capture_baseline = Some(capture_baseline);

    execute_submit_with_accepted(
        plan,
        output,
        read_source,
        move |request| {
            let mut before_post = |language_id: &str| {
                let Some(capture) = capture_baseline.take() else {
                    return;
                };
                // The backend invokes this after submit-page parsing and language resolution,
                // immediately before its rate-limit wait and sole physical POST attempt. Failure
                // remains best-effort and never prevents that POST.
                *baseline_before_post.borrow_mut() = capture(&contest_id, &task_id, language_id);
            };
            submit_once(request, &mut before_post)
        },
        move |contest_id, _, _, output| {
            let Some(baseline) = baseline_after_acceptance.borrow_mut().take() else {
                report_tracking_unavailable(output, false);
                return;
            };
            let Ok(submission_id) = discover_submission(&baseline) else {
                report_tracking_unavailable(output, false);
                return;
            };

            if writeln!(output, "Submitted: #{submission_id}").is_err() {
                return;
            }

            let mut output_available = true;
            let poll_result = poll_status(contest_id, submission_id, &mut |status| {
                if render_submission_status(output, status).is_ok() {
                    true
                } else {
                    output_available = false;
                    false
                }
            });
            if poll_result.is_err() && output_available {
                report_tracking_unavailable(output, true);
            }
        },
    )
}

fn render_submission_status(output: &mut impl Write, status: &SubmissionStatus) -> io::Result<()> {
    match status {
        SubmissionStatus::WaitingForJudge => writeln!(output, "Waiting for judge..."),
        SubmissionStatus::WaitingForRejudge => writeln!(output, "Waiting for rejudge..."),
        SubmissionStatus::Judging => writeln!(output, "Judging..."),
        SubmissionStatus::JudgingProgress {
            judged,
            total,
            provisional,
        } => {
            write!(output, "Judging: {judged}/{total}")?;
            if let Some(verdict) = provisional {
                write!(output, " {verdict}")?;
            }
            writeln!(output)
        }
        SubmissionStatus::Finished(verdict) => writeln!(output, "{verdict}"),
    }
}

fn report_tracking_unavailable(output: &mut impl Write, submission_id_was_reported: bool) {
    if !submission_id_was_reported && writeln!(output, "Submitted.").is_err() {
        return;
    }
    if writeln!(output, "Submission tracking is unavailable.").is_err() {
        return;
    }
    let _ = writeln!(output, "Check My Submissions for the result.");
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn submit_at<R, S>(
    launch_root: &Path,
    problem_index: &str,
    cli_contest: Option<&str>,
    cli_language: Option<Language>,
    output: &mut impl Write,
    read_source: R,
    submit_once: S,
) -> Result<(), AppError>
where
    R: FnOnce(&Path) -> io::Result<String>,
    S: FnOnce(SubmitRequest) -> Result<SubmitOutcome, AppError>,
{
    submit_at_with_runtime(
        launch_root,
        problem_index,
        cli_contest,
        cli_language,
        None,
        PythonRuntime::CPython,
        output,
        read_source,
        submit_once,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn submit_at_with_runtime<R, S>(
    launch_root: &Path,
    problem_index: &str,
    cli_contest: Option<&str>,
    cli_language: Option<Language>,
    cli_runtime: Option<PythonRuntime>,
    configured_runtime: PythonRuntime,
    output: &mut impl Write,
    read_source: R,
    submit_once: S,
) -> Result<(), AppError>
where
    R: FnOnce(&Path) -> io::Result<String>,
    S: FnOnce(SubmitRequest) -> Result<SubmitOutcome, AppError>,
{
    let plan = resolve_submit_plan(
        launch_root,
        problem_index,
        cli_contest,
        cli_language,
        cli_runtime,
        configured_runtime,
    )?;
    execute_submit(plan, output, read_source, submit_once)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atcoder::submission_tracking::Verdict;
    use crate::atcoder::submit::{SubmitError, SubmitPageError};
    use crate::config::Config;
    use crate::model::Contest;

    use std::cell::Cell;

    #[derive(Default)]
    struct BrokenPipeWriter {
        writes: usize,
    }

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "scripted closed stdout",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn contest(destination: &Path, contest_id: &str, index: &str, task_id: &str) {
        workspace::save_metadata(
            destination,
            &Contest {
                contest_id: contest_id.to_string(),
                problems: vec![Problem {
                    index: index.to_string(),
                    title: format!("Problem {index}"),
                    task_id: task_id.to_string(),
                    url: format!("https://atcoder.jp/contests/{contest_id}/tasks/{task_id}"),
                    sample_count: 0,
                }],
            },
        )
        .unwrap();
    }

    fn write_source(destination: &Path, index: &str, language: Language, source: &str) {
        let path = workspace::source_file_path(destination, index, language).unwrap();
        fs::write(path, source).unwrap();
    }

    fn capture_language(
        destination: &Path,
        specified_language: Option<Language>,
    ) -> Result<SubmissionTarget, AppError> {
        capture_target(
            destination,
            specified_language,
            None,
            PythonRuntime::CPython,
        )
    }

    fn capture_target(
        destination: &Path,
        specified_language: Option<Language>,
        cli_runtime: Option<PythonRuntime>,
        configured_runtime: PythonRuntime,
    ) -> Result<SubmissionTarget, AppError> {
        let selected = Cell::new(None);
        submit_at_with_runtime(
            destination,
            "A",
            None,
            specified_language,
            cli_runtime,
            configured_runtime,
            &mut Vec::new(),
            read_source_file,
            |request| {
                selected.set(Some(request.test_parts().2));
                Ok(SubmitOutcome::Accepted)
            },
        )?;
        Ok(selected.get().expect("submitter should receive a request"))
    }

    #[test]
    fn no_language_selects_the_only_existing_cpp_source() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "int main() {}\n");

        assert_eq!(
            capture_language(temp.path(), None).unwrap(),
            SubmissionTarget::Cpp
        );
    }

    #[test]
    fn no_language_selects_the_only_existing_python_source() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Python, "print(1)\n");

        assert_eq!(
            capture_language(temp.path(), None).unwrap(),
            SubmissionTarget::Python(PythonRuntime::CPython)
        );
    }

    #[test]
    fn both_sources_require_an_explicit_language_even_with_pypy_config() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        write_source(temp.path(), "A", Language::Python, "python\n");
        let config = Config::parse("[submit]\npython_runtime = \"pypy\"\n").unwrap();
        assert_eq!(config.submit.python_runtime, PythonRuntime::PyPy);

        let calls = Cell::new(0);
        let error = submit_at_with_runtime(
            temp.path(),
            "A",
            None,
            None,
            None,
            config.submit.python_runtime,
            &mut Vec::new(),
            read_source_file,
            |_| {
                calls.set(calls.get() + 1);
                Ok(SubmitOutcome::Accepted)
            },
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("Both C++ and Python sources exist for problem A"));
        assert!(message.contains("-l cpp"));
        assert!(message.contains("-l python"));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn explicit_language_selects_the_requested_source_when_both_exist() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        write_source(temp.path(), "A", Language::Python, "python\n");

        assert_eq!(
            capture_language(temp.path(), Some(Language::Cpp)).unwrap(),
            SubmissionTarget::Cpp
        );
        assert_eq!(
            capture_language(temp.path(), Some(Language::Python)).unwrap(),
            SubmissionTarget::Python(PythonRuntime::CPython)
        );
    }

    #[test]
    fn python_runtime_precedence_is_cli_then_config_then_cpython_builtin() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Python, "print(1)\n");

        for (cli, configured, expected) in [
            (
                None,
                PythonRuntime::CPython,
                SubmissionTarget::Python(PythonRuntime::CPython),
            ),
            (
                None,
                PythonRuntime::PyPy,
                SubmissionTarget::Python(PythonRuntime::PyPy),
            ),
            (
                Some(PythonRuntime::CPython),
                PythonRuntime::PyPy,
                SubmissionTarget::Python(PythonRuntime::CPython),
            ),
            (
                Some(PythonRuntime::PyPy),
                PythonRuntime::CPython,
                SubmissionTarget::Python(PythonRuntime::PyPy),
            ),
        ] {
            assert_eq!(
                capture_target(temp.path(), None, cli, configured).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn runtime_override_does_not_resolve_source_ambiguity() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        write_source(temp.path(), "A", Language::Python, "python\n");
        let calls = Cell::new(0);

        let error = submit_at_with_runtime(
            temp.path(),
            "A",
            None,
            None,
            Some(PythonRuntime::PyPy),
            PythonRuntime::CPython,
            &mut Vec::new(),
            read_source_file,
            |_| {
                calls.set(calls.get() + 1);
                Ok(SubmitOutcome::Accepted)
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("Specify a language"));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn explicit_language_and_runtime_form_a_typed_submission_target() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        write_source(temp.path(), "A", Language::Python, "python\n");

        assert_eq!(
            capture_target(
                temp.path(),
                Some(Language::Python),
                Some(PythonRuntime::PyPy),
                PythonRuntime::CPython,
            )
            .unwrap(),
            SubmissionTarget::Python(PythonRuntime::PyPy)
        );
        assert_eq!(
            capture_target(temp.path(), Some(Language::Cpp), None, PythonRuntime::PyPy,).unwrap(),
            SubmissionTarget::Cpp
        );
    }

    #[test]
    fn explicit_runtime_is_rejected_for_cpp_before_backend_call() {
        for (both_sources, runtime) in [
            (false, PythonRuntime::CPython),
            (false, PythonRuntime::PyPy),
            (true, PythonRuntime::CPython),
            (true, PythonRuntime::PyPy),
        ] {
            let temp = tempfile::tempdir().unwrap();
            contest(temp.path(), "abc430", "A", "abc430_a");
            write_source(temp.path(), "A", Language::Cpp, "cpp\n");
            if both_sources {
                write_source(temp.path(), "A", Language::Python, "python\n");
            }
            let calls = Cell::new(0);

            let error = submit_at_with_runtime(
                temp.path(),
                "A",
                None,
                if both_sources {
                    Some(Language::Cpp)
                } else {
                    None
                },
                Some(runtime),
                PythonRuntime::CPython,
                &mut Vec::new(),
                read_source_file,
                |_| {
                    calls.set(calls.get() + 1);
                    Ok(SubmitOutcome::Accepted)
                },
            )
            .unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains("--runtime is only valid for Python submissions")
            );
            assert_eq!(calls.get(), 0);
        }
    }

    #[test]
    fn configured_python_runtime_is_irrelevant_to_cpp_submission() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");

        assert_eq!(
            capture_target(temp.path(), None, None, PythonRuntime::PyPy).unwrap(),
            SubmissionTarget::Cpp
        );
    }

    #[test]
    fn explicit_language_never_falls_back_to_the_other_source() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Python, "python\n");

        let error = capture_language(temp.path(), Some(Language::Cpp)).unwrap_err();
        assert!(error.to_string().contains("C++ source file not found"));

        fs::remove_file(temp.path().join("A.py")).unwrap();
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        let error = capture_language(temp.path(), Some(Language::Python)).unwrap_err();
        assert!(error.to_string().contains("Python source file not found"));
    }

    #[test]
    fn no_sources_fail_before_the_backend() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        let calls = Cell::new(0);

        let error = submit_at(
            temp.path(),
            "A",
            None,
            None,
            &mut Vec::new(),
            read_source_file,
            |_| {
                calls.set(calls.get() + 1);
                Ok(SubmitOutcome::Accepted)
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("no C++ or Python source file"));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn unknown_problem_fails_before_source_read_and_backend() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        let reads = Cell::new(0);
        let calls = Cell::new(0);

        let error = submit_at(
            temp.path(),
            "Z",
            None,
            None,
            &mut Vec::new(),
            |_| {
                reads.set(reads.get() + 1);
                Ok("secret source".to_string())
            },
            |_| {
                calls.set(calls.get() + 1);
                Ok(SubmitOutcome::Accepted)
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("problem not found in this contest: Z")
        );
        assert_eq!(reads.get(), 0);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn adt_request_uses_the_stable_metadata_task_id() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "adt_easy_20260826_1", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");

        submit_at(
            temp.path(),
            "A",
            None,
            None,
            &mut Vec::new(),
            read_source_file,
            |request| {
                let (contest_id, task_id, target, source) = request.test_parts();
                assert_eq!(contest_id, "adt_easy_20260826_1");
                assert_eq!(task_id, "abc430_a");
                assert_ne!(task_id, "adt_easy_20260826_1_a");
                assert_eq!(target, SubmissionTarget::Cpp);
                assert_eq!(source, "cpp\n");
                Ok(SubmitOutcome::Accepted)
            },
        )
        .unwrap();
    }

    #[test]
    fn explicit_contest_uses_existing_workspace_routing() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join(".atc-workspace.toml"),
            concat!(
                "version = 1\n",
                "paths = [{ pattern = \"^abc[0-9]+$\", path = \"AtCoder/ABC\" }]\n"
            ),
        )
        .unwrap();
        let destination = root.path().join("AtCoder/ABC/abc430");
        fs::create_dir_all(&destination).unwrap();
        contest(&destination, "abc430", "A", "abc430_a");
        write_source(&destination, "A", Language::Python, "print(1)\n");

        submit_at_with_runtime(
            root.path(),
            "A",
            Some("abc430"),
            None,
            None,
            PythonRuntime::PyPy,
            &mut Vec::new(),
            read_source_file,
            |request| {
                assert_eq!(request.test_parts().0, "abc430");
                assert_eq!(
                    request.test_parts().2,
                    SubmissionTarget::Python(PythonRuntime::PyPy)
                );
                Ok(SubmitOutcome::Accepted)
            },
        )
        .unwrap();
    }

    #[test]
    fn source_is_read_once_and_the_exact_owned_snapshot_is_submitted() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        let original = "first line\r\nUnicode: 日本語\r\n";
        write_source(temp.path(), "A", Language::Cpp, original);
        let reads = Cell::new(0);

        submit_at(
            temp.path(),
            "A",
            None,
            None,
            &mut Vec::new(),
            |path| {
                reads.set(reads.get() + 1);
                let snapshot = fs::read_to_string(path)?;
                fs::write(path, "changed after snapshot\n")?;
                Ok(snapshot)
            },
            |request| {
                assert_eq!(request.test_parts().3, original);
                Ok(SubmitOutcome::Accepted)
            },
        )
        .unwrap();

        assert_eq!(reads.get(), 1);
        assert_eq!(
            fs::read_to_string(temp.path().join("A.cpp")).unwrap(),
            "changed after snapshot\n"
        );
    }

    #[test]
    fn empty_source_fails_closed_but_whitespace_is_not_trimmed() {
        let empty = tempfile::tempdir().unwrap();
        contest(empty.path(), "abc430", "A", "abc430_a");
        write_source(empty.path(), "A", Language::Cpp, "");
        let calls = Cell::new(0);
        let error = submit_at(
            empty.path(),
            "A",
            None,
            None,
            &mut Vec::new(),
            read_source_file,
            |_| {
                calls.set(calls.get() + 1);
                Ok(SubmitOutcome::Accepted)
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("source file is empty"));
        assert_eq!(calls.get(), 0);

        let whitespace = tempfile::tempdir().unwrap();
        contest(whitespace.path(), "abc430", "A", "abc430_a");
        write_source(whitespace.path(), "A", Language::Cpp, " \r\n");
        submit_at(
            whitespace.path(),
            "A",
            None,
            None,
            &mut Vec::new(),
            read_source_file,
            |request| {
                assert_eq!(request.test_parts().3, " \r\n");
                Ok(SubmitOutcome::Accepted)
            },
        )
        .unwrap();
    }

    #[test]
    fn backend_is_called_once_for_every_remote_outcome_and_never_retried() {
        let cases = [
            Ok(SubmitOutcome::Accepted),
            Err(SubmitError::SubmissionRejected),
            Ok(SubmitOutcome::UnknownSubmissionOutcome),
            Err(SubmitError::RateLimited),
        ];

        for result in cases {
            let temp = tempfile::tempdir().unwrap();
            contest(temp.path(), "abc430", "A", "abc430_a");
            write_source(temp.path(), "A", Language::Cpp, "secret-source\n");
            let calls = Cell::new(0);
            let command_result = submit_at(
                temp.path(),
                "A",
                None,
                None,
                &mut Vec::new(),
                read_source_file,
                |_| {
                    calls.set(calls.get() + 1);
                    result.clone().map_err(AppError::from)
                },
            );

            assert_eq!(calls.get(), 1, "result: {result:?}");
            assert_eq!(
                command_result.is_ok(),
                result == Ok(SubmitOutcome::Accepted)
            );
        }
    }

    #[test]
    fn accepted_ignores_broken_pipe_reporting_failure_without_read_or_submit_retry() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        let reads = Cell::new(0);
        let calls = Cell::new(0);
        let mut output = BrokenPipeWriter::default();

        let result = submit_at(
            temp.path(),
            "A",
            None,
            None,
            &mut output,
            |path| {
                reads.set(reads.get() + 1);
                fs::read_to_string(path)
            },
            |_| {
                calls.set(calls.get() + 1);
                Ok(SubmitOutcome::Accepted)
            },
        );

        assert!(result.is_ok(), "Accepted must remain a successful command");
        assert_eq!(reads.get(), 1);
        assert_eq!(calls.get(), 1);
        assert_eq!(output.writes, 1);
    }

    #[test]
    fn accepted_and_failure_messages_are_distinct_safe_and_have_expected_status() {
        let run = |result: Result<SubmitOutcome, SubmitError>| {
            let temp = tempfile::tempdir().unwrap();
            contest(temp.path(), "abc430", "A", "abc430_a");
            write_source(
                temp.path(),
                "A",
                Language::Cpp,
                "SOURCE_SECRET_COOKIE_CSRF\n",
            );
            let mut output = Vec::new();
            let command_result = submit_at(
                temp.path(),
                "A",
                None,
                None,
                &mut output,
                read_source_file,
                |_| result.map_err(AppError::from),
            );
            (command_result, String::from_utf8(output).unwrap())
        };

        let (accepted, output) = run(Ok(SubmitOutcome::Accepted));
        assert!(accepted.is_ok());
        assert_eq!(output, "Submitted: A\n");

        let (rejected, output) = run(Err(SubmitError::SubmissionRejected));
        assert!(output.is_empty());
        let rejected = rejected.unwrap_err().to_string();
        assert!(rejected.contains("Submission was rejected by AtCoder"));
        assert!(rejected.contains("may require browser verification"));

        let (unknown, output) = run(Ok(SubmitOutcome::UnknownSubmissionOutcome));
        assert!(output.is_empty());
        let unknown = unknown.unwrap_err().to_string();
        assert!(unknown.contains("Submission outcome is unknown"));
        assert!(unknown.contains("Check My Submissions before retrying"));

        let (authentication, _) = run(Err(SubmitError::AuthenticationRequired));
        assert!(
            authentication
                .unwrap_err()
                .to_string()
                .contains("authentication is required")
        );

        let (rate_limited, _) = run(Err(SubmitError::RateLimited));
        assert!(
            rate_limited
                .unwrap_err()
                .to_string()
                .contains("rate limited")
        );

        for text in [rejected, unknown] {
            assert!(!text.contains("SOURCE_SECRET"));
            assert!(!text.contains("COOKIE"));
            assert!(!text.contains("CSRF"));
        }
    }

    #[test]
    fn baseline_failure_disables_tracking_but_still_submits_once() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        let plan = resolve_submit_plan(temp.path(), "A", None, None, None, PythonRuntime::CPython)
            .unwrap();
        let baseline_calls = Cell::new(0);
        let submit_calls = Cell::new(0);
        let mut output = Vec::new();

        let result = execute_submit_with_tracking(
            plan,
            &mut output,
            read_source_file,
            |contest_id, task_id, language_id| {
                baseline_calls.set(baseline_calls.get() + 1);
                assert_eq!(contest_id, "abc430");
                assert_eq!(task_id, "abc430_a");
                assert_eq!(language_id, "6017");
                None::<()>
            },
            |_, before_post| {
                submit_calls.set(submit_calls.get() + 1);
                before_post("6017");
                Ok(SubmitOutcome::Accepted)
            },
            |_| panic!("discovery must stay disabled without a baseline"),
            |_, _, _| panic!("polling must stay disabled without a submission ID"),
        );

        assert!(result.is_ok());
        assert_eq!(baseline_calls.get(), 1);
        assert_eq!(submit_calls.get(), 1);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Submitted.\nSubmission tracking is unavailable.\nCheck My Submissions for the result.\n"
        );
    }

    #[test]
    fn accepted_submission_renders_id_changed_statuses_and_final_verdict() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        let plan = resolve_submit_plan(temp.path(), "A", None, None, None, PythonRuntime::CPython)
            .unwrap();
        let submit_calls = Cell::new(0);
        let mut output = Vec::new();

        let result = execute_submit_with_tracking(
            plan,
            &mut output,
            read_source_file,
            |_, _, language_id| {
                assert_eq!(language_id, "6017");
                Some(())
            },
            |_, before_post| {
                submit_calls.set(submit_calls.get() + 1);
                before_post("6017");
                Ok(SubmitOutcome::Accepted)
            },
            |_| Ok(SubmissionId::for_test(78905741)),
            |contest_id, submission_id, on_status| {
                assert_eq!(contest_id, "abc430");
                assert_eq!(submission_id.to_string(), "78905741");
                for status in [
                    SubmissionStatus::WaitingForJudge,
                    SubmissionStatus::JudgingProgress {
                        judged: 1,
                        total: 36,
                        provisional: None,
                    },
                    SubmissionStatus::JudgingProgress {
                        judged: 3,
                        total: 36,
                        provisional: Some(Verdict::WrongAnswer),
                    },
                    SubmissionStatus::Finished(Verdict::WrongAnswer),
                ] {
                    assert!(on_status(&status));
                }
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(submit_calls.get(), 1);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "Submitted: #78905741\n",
                "Waiting for judge...\n",
                "Judging: 1/36\n",
                "Judging: 3/36 WA\n",
                "WA\n"
            )
        );
    }

    #[test]
    fn tracking_failure_after_acceptance_keeps_success_exit_semantics() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        let plan = resolve_submit_plan(temp.path(), "A", None, None, None, PythonRuntime::CPython)
            .unwrap();
        let submit_calls = Cell::new(0);
        let mut output = Vec::new();

        let result = execute_submit_with_tracking(
            plan,
            &mut output,
            read_source_file,
            |_, _, _| Some(()),
            |_, before_post| {
                submit_calls.set(submit_calls.get() + 1);
                before_post("6017");
                Ok(SubmitOutcome::Accepted)
            },
            |_| Ok(SubmissionId::for_test(78905741)),
            |_, _, _| Err(()),
        );

        assert!(result.is_ok());
        assert_eq!(submit_calls.get(), 1);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output,
            concat!(
                "Submitted: #78905741\n",
                "Submission tracking is unavailable.\n",
                "Check My Submissions for the result.\n"
            )
        );
        assert!(!output.contains("outcome is unknown"));
        assert!(!output.contains("retry"));
    }

    #[test]
    fn accepted_tracking_stops_cleanly_when_stdout_breaks() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        let plan = resolve_submit_plan(temp.path(), "A", None, None, None, PythonRuntime::CPython)
            .unwrap();
        let submit_calls = Cell::new(0);
        let poll_calls = Cell::new(0);
        let mut output = BrokenPipeWriter::default();

        let result = execute_submit_with_tracking(
            plan,
            &mut output,
            read_source_file,
            |_, _, _| Some(()),
            |_, before_post| {
                submit_calls.set(submit_calls.get() + 1);
                before_post("6017");
                Ok(SubmitOutcome::Accepted)
            },
            |_| Ok(SubmissionId::for_test(78905741)),
            |_, _, _| {
                poll_calls.set(poll_calls.get() + 1);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(submit_calls.get(), 1);
        assert_eq!(poll_calls.get(), 0);
        assert_eq!(output.writes, 1);
    }

    #[test]
    fn tracking_wrapper_never_reenters_submit_for_any_backend_outcome() {
        let cases = [
            ("accepted", Ok(SubmitOutcome::Accepted), true, true),
            (
                "post transport has unknown outcome",
                Ok(SubmitOutcome::UnknownSubmissionOutcome),
                false,
                true,
            ),
            (
                "invalid request identity",
                Err(SubmitError::InvalidRequestIdentity { kind: "task ID" }),
                false,
                false,
            ),
            (
                "submit-page authentication required",
                Err(SubmitError::AuthenticationRequired),
                false,
                false,
            ),
            (
                "submit unavailable",
                Err(SubmitError::SubmitUnavailable),
                false,
                false,
            ),
            (
                "submit client initialization",
                Err(SubmitError::SubmitClientInitializationFailed),
                false,
                false,
            ),
            (
                "submit page parse",
                Err(SubmitError::SubmitPage(SubmitPageError::MalformedPage(
                    "test failure",
                ))),
                false,
                false,
            ),
            (
                "submit page transport",
                Err(SubmitError::SubmitPageFetchFailed),
                false,
                false,
            ),
            (
                "submission rejected",
                Err(SubmitError::SubmissionRejected),
                false,
                true,
            ),
            (
                "unexpected redirect",
                Err(SubmitError::UnexpectedRedirect),
                false,
                true,
            ),
            (
                "post rate limited",
                Err(SubmitError::RateLimited),
                false,
                true,
            ),
        ];

        for (name, submit_result, accepted, invokes_post_hook) in cases {
            let temp = tempfile::tempdir().unwrap();
            contest(temp.path(), "abc430", "A", "abc430_a");
            write_source(temp.path(), "A", Language::Cpp, "cpp\n");
            let plan =
                resolve_submit_plan(temp.path(), "A", None, None, None, PythonRuntime::CPython)
                    .unwrap();
            let baseline_calls = Cell::new(0);
            let submit_calls = Cell::new(0);
            let discovery_calls = Cell::new(0);
            let poll_calls = Cell::new(0);
            let mut output = Vec::new();

            let result = execute_submit_with_tracking(
                plan,
                &mut output,
                read_source_file,
                |contest_id, task_id, language_id| {
                    baseline_calls.set(baseline_calls.get() + 1);
                    assert_eq!(contest_id, "abc430");
                    assert_eq!(task_id, "abc430_a");
                    assert_eq!(language_id, "6017");
                    Some(())
                },
                |_, before_post| {
                    submit_calls.set(submit_calls.get() + 1);
                    if invokes_post_hook {
                        before_post("6017");
                        // Even a buggy duplicate hook invocation cannot capture two baselines.
                        before_post("6017");
                    }
                    submit_result.map_err(AppError::from)
                },
                |_| {
                    discovery_calls.set(discovery_calls.get() + 1);
                    Ok(SubmissionId::for_test(78905741))
                },
                |_, _, _| {
                    poll_calls.set(poll_calls.get() + 1);
                    Ok(())
                },
            );

            assert_eq!(submit_calls.get(), 1, "{name}");
            assert_eq!(baseline_calls.get(), invokes_post_hook as usize, "{name}");
            if accepted {
                assert!(result.is_ok(), "{name}: {result:?}");
                assert_eq!(discovery_calls.get(), 1, "{name}");
                assert_eq!(poll_calls.get(), 1, "{name}");
            } else {
                assert!(result.is_err(), "{name}");
                assert_eq!(discovery_calls.get(), 0, "{name}");
                assert_eq!(poll_calls.get(), 0, "{name}");
            }
        }
    }

    #[test]
    fn failure_before_the_post_hook_does_not_capture_or_start_tracking() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        let plan = resolve_submit_plan(temp.path(), "A", None, None, None, PythonRuntime::CPython)
            .unwrap();
        let baseline_calls = Cell::new(0);
        let submit_calls = Cell::new(0);
        let discovery_calls = Cell::new(0);
        let poll_calls = Cell::new(0);
        let mut output = Vec::new();

        let result = execute_submit_with_tracking(
            plan,
            &mut output,
            read_source_file,
            |_, _, _| {
                baseline_calls.set(baseline_calls.get() + 1);
                Some(())
            },
            |_, _| {
                submit_calls.set(submit_calls.get() + 1);
                Err(SubmitError::SubmitPageFetchFailed.into())
            },
            |_| {
                discovery_calls.set(discovery_calls.get() + 1);
                Ok(SubmissionId::for_test(78905741))
            },
            |_, _, _| {
                poll_calls.set(poll_calls.get() + 1);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(submit_calls.get(), 1);
        assert_eq!(baseline_calls.get(), 0);
        assert_eq!(discovery_calls.get(), 0);
        assert_eq!(poll_calls.get(), 0);
        assert!(output.is_empty());
    }
}
