use super::test::find_problem;
use crate::atcoder;
use crate::atcoder::submit::{SubmitOutcome, SubmitRequest};
use crate::error::AppError;
use crate::language::Language;
use crate::model::Problem;
use crate::workspace;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub(crate) fn submit(
    problem_index: &str,
    cli_contest: Option<&str>,
    cli_language: Option<Language>,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let plan = resolve_submit_plan(&cwd, problem_index, cli_contest, cli_language)?;
    let atcoder = atcoder::AtCoderClient::new()?;
    let stdout = io::stdout();

    execute_submit(plan, &mut stdout.lock(), read_source_file, |request| {
        Ok(atcoder.submit(request)?)
    })
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

fn resolve_submit_plan(
    launch_root: &Path,
    problem_index: &str,
    cli_contest: Option<&str>,
    cli_language: Option<Language>,
) -> Result<SubmitPlan, AppError> {
    let destination = workspace::resolve_contest_target(launch_root, cli_contest)?;
    workspace::validate_workspace_marker(&destination)?;

    let contest = workspace::load_metadata(&destination)?;
    workspace::validate_contest_paths(&contest)?;
    if let Some(contest_id) = cli_contest {
        workspace::validate_contest_identity(&contest, contest_id)?;
    }

    let problem = find_problem(&contest, problem_index)?;
    let source = resolve_submit_source(&destination, problem, cli_language)?;

    Ok(SubmitPlan {
        contest_id: contest.contest_id.clone(),
        task_id: problem.task_id.clone(),
        problem_index: problem.index.clone(),
        source,
    })
}

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
    let SubmitPlan {
        contest_id,
        task_id,
        problem_index,
        source,
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

    let request = SubmitRequest::new(contest_id, task_id, source.language, source_snapshot);

    // FnOnce makes a CLI-level retry impossible: this invocation can call the backend once.
    match submit_once(request)? {
        SubmitOutcome::Accepted => {
            // The remote side effect is already confirmed. A local reporting failure must not
            // turn this into a failed command because callers commonly retry nonzero exits.
            let _ = writeln!(output, "Submitted: {problem_index}");
            Ok(())
        }
        SubmitOutcome::UnknownSubmissionOutcome => Err(AppError::UnknownSubmissionOutcome),
    }
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
    let plan = resolve_submit_plan(launch_root, problem_index, cli_contest, cli_language)?;
    execute_submit(plan, output, read_source, submit_once)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atcoder::submit::SubmitError;
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
    ) -> Result<Language, AppError> {
        let selected = Cell::new(None);
        submit_at(
            destination,
            "A",
            None,
            specified_language,
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

        assert_eq!(capture_language(temp.path(), None).unwrap(), Language::Cpp);
    }

    #[test]
    fn no_language_selects_the_only_existing_python_source() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Python, "print(1)\n");

        assert_eq!(
            capture_language(temp.path(), None).unwrap(),
            Language::Python
        );
    }

    #[test]
    fn both_sources_require_an_explicit_language_even_with_python_config_default() {
        let temp = tempfile::tempdir().unwrap();
        contest(temp.path(), "abc430", "A", "abc430_a");
        write_source(temp.path(), "A", Language::Cpp, "cpp\n");
        write_source(temp.path(), "A", Language::Python, "python\n");
        let config = Config::parse("defaults.language = \"python\"\n").unwrap();
        assert_eq!(config.defaults.language, Language::Python);

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
            Language::Cpp
        );
        assert_eq!(
            capture_language(temp.path(), Some(Language::Python)).unwrap(),
            Language::Python
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
                let (contest_id, task_id, language, source) = request.test_parts();
                assert_eq!(contest_id, "adt_easy_20260826_1");
                assert_eq!(task_id, "abc430_a");
                assert_ne!(task_id, "adt_easy_20260826_1_a");
                assert_eq!(language, Language::Cpp);
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

        submit_at(
            root.path(),
            "A",
            Some("abc430"),
            None,
            &mut Vec::new(),
            read_source_file,
            |request| {
                assert_eq!(request.test_parts().0, "abc430");
                assert_eq!(request.test_parts().2, Language::Python);
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
}
