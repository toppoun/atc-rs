use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use crate::config::RunnerConfig;
use crate::error::AppError;
use crate::language::Language;
use crate::runner::{self, ExecutionOutcome, ExecutionResult};
use crate::tui::message::{
    Message, RunKind, RunRequest, UserInputRunEvent, UserInputRunResult, UserInputRunStatus,
};

use super::test::duration_from_seconds;

// A one-shot execution, with no Sample, comparator, saved-case load, or storage write.
pub(super) fn run(
    destination: &Path,
    request: &RunRequest,
    config: &RunnerConfig,
    messages: &Sender<Message>,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<(), AppError> {
    let RunKind::UserInput(snapshot) = &request.kind else {
        return Err(io::Error::other("expected a User Input request").into());
    };
    // Match the normal sample runner's live source path and execution semantics.
    // Only stdin is captured by the request; the compiler/interpreter reads the source.
    let source = destination.join(format!(
        "{}.{}",
        snapshot.problem_index,
        request.language.extension()
    ));
    if !source.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("source file not found: {}", source.display()),
        )
        .into());
    }
    let timeout = duration_from_seconds(config.timeout_seconds, "runner.timeout_seconds")?;
    let compile_timeout = duration_from_seconds(
        config.compile_timeout_seconds,
        "runner.compile_timeout_seconds",
    )?;
    let publish = |event| {
        messages
            .send(Message::UserInputRunEvent {
                run_id: request.run_id,
                problem: request.problem,
                snapshot: Arc::clone(snapshot),
                event,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI receiver disconnected"))
    };
    let result = match request.language {
        Language::Python => {
            publish(UserInputRunEvent::Running)?;
            runner::execute_python_with_cancel_in(
                &source,
                &snapshot.input,
                &config.python,
                timeout,
                is_cancelled,
                destination,
            )?
        }
        Language::Cpp => {
            // The sample runner also builds once per attempt into a private temporary directory.
            let build = tempfile::tempdir()?;
            let output = build.path().join(format!(
                "{}{}",
                snapshot.problem_index,
                std::env::consts::EXE_SUFFIX
            ));
            let options = runner::BuildOptions {
                debug_include_dir: if request.debug {
                    Some(crate::debug::materialize_debug_header()?)
                } else {
                    None
                },
            };
            let compiled = runner::compile_cpp_with_cancel_in(
                &source,
                &output,
                &config.cpp_compiler,
                &config.cpp_flags,
                compile_timeout,
                &options,
                is_cancelled,
                destination,
            )?;
            if !matches!(&compiled.outcome, ExecutionOutcome::Exited(status) if status.success()) {
                publish(UserInputRunEvent::Finished(result(compiled, true)))?;
                return Ok(());
            }
            publish(UserInputRunEvent::Running)?;
            runner::execute_with_cancel_in(
                &output,
                &[],
                &snapshot.input,
                timeout,
                is_cancelled,
                destination,
            )?
        }
    };
    publish(UserInputRunEvent::Finished(self::result(result, false)))?;
    Ok(())
}

fn result(result: ExecutionResult, compiling: bool) -> UserInputRunResult {
    let status = match (&result.outcome, compiling) {
        (ExecutionOutcome::TimedOut, true) => UserInputRunStatus::CompileTimedOut,
        (ExecutionOutcome::TimedOut, false) => UserInputRunStatus::TimedOut,
        (ExecutionOutcome::Exited(status), _) if status.success() => UserInputRunStatus::Finished,
        (ExecutionOutcome::Exited(_), true) => UserInputRunStatus::CompileError,
        (ExecutionOutcome::Exited(_), false) => UserInputRunStatus::RuntimeError,
    };
    UserInputRunResult {
        status,
        stdout: result.stdout,
        stderr: result.stderr,
        elapsed: result.elapsed,
    }
}

#[cfg(test)]
pub(crate) fn execute_user_input_for_test(
    destination: &Path,
    request: RunRequest,
    config: RunnerConfig,
) -> Vec<Message> {
    use crate::model::Problem;
    use crate::tui::message::RunWorkerCommand;
    use std::sync::mpsc;
    use std::time::Duration;
    let (tx, rx) = mpsc::channel();
    let worker = super::watch_worker::RunWorker::start(
        destination.to_path_buf(),
        "abc123".into(),
        vec![Problem {
            index: "A".into(),
            title: "A".into(),
            task_id: "abc123_a".into(),
            url: "https://example.invalid/a".into(),
            sample_count: 42,
        }],
        config,
        tx,
    )
    .unwrap();
    worker
        .sender()
        .send(RunWorkerCommand::Run(request))
        .unwrap();
    let mut messages = Vec::new();
    loop {
        let message = rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let terminal = matches!(
            message,
            Message::RunCompleted { .. } | Message::RunFailed { .. }
        );
        messages.push(message);
        if terminal {
            break;
        }
    }
    worker.stop_and_join().unwrap();
    assert!(rx.try_recv().is_err(), "event after joined worker");
    messages
}

#[cfg(test)]
mod tests {
    use super::super::attempt_executor::AttemptExecutor;
    use super::*;
    use crate::attempt::AttemptOutcome;
    use crate::model::Problem;
    use crate::tui::message::{UserInputRunSnapshot, UserInputRunTarget};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    fn executable(names: &[&str]) -> String {
        names
            .iter()
            .find(|name| {
                std::process::Command::new(name)
                    .arg("--version")
                    .output()
                    .is_ok_and(|output| output.status.success())
            })
            .expect("runner integration tests require Python and a C++ compiler")
            .to_string()
    }

    fn python_config() -> RunnerConfig {
        RunnerConfig {
            python: executable(&["python3", "python"]),
            ..RunnerConfig::default()
        }
    }

    fn request(language: Language, input: &str) -> RunRequest {
        RunRequest {
            problem: 0,
            run_id: 17,
            language,
            debug: false,
            kind: RunKind::UserInput(Arc::new(UserInputRunSnapshot {
                problem_index: "A".to_string(),
                target: UserInputRunTarget::Draft(4),
                input: Arc::from(input),
                source_revision: 8,
                start_gate: Default::default(),
            })),
        }
    }

    fn executor(path: &Path, config: RunnerConfig, tx: Sender<Message>) -> AttemptExecutor {
        AttemptExecutor::new(
            path.to_path_buf(),
            "abc123".to_string(),
            vec![Problem {
                index: "A".to_string(),
                title: "A".to_string(),
                task_id: "abc123_a".to_string(),
                url: "https://example.invalid/a".to_string(),
                sample_count: 42,
            }],
            config,
            tx,
        )
    }

    fn execute_source(
        source: &str,
        request: RunRequest,
        config: RunnerConfig,
    ) -> UserInputRunResult {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path()
                .join(format!("A.{}", request.language.extension())),
            source,
        )
        .unwrap();
        // No sample directory, no User Input directory: the worker must only use the payload.
        let (tx, rx) = mpsc::channel();
        let executor = executor(temp.path(), config, tx);
        let (completion_tx, completion_rx) = mpsc::channel();
        let active = executor.spawn(request, completion_tx).unwrap();
        completion_rx.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(matches!(active.join().unwrap(), AttemptOutcome::Completed));
        let messages = rx.try_iter().collect::<Vec<_>>();
        assert!(matches!(
            messages.first(),
            Some(Message::RunStarted {
                run_id: 17,
                problem: 0
            })
        ));
        assert!(!messages.iter().any(|message| matches!(
            message,
            Message::RunEvent { .. } | Message::StressEvent { .. }
        )));
        assert!(!temp.path().join(".atc").exists());
        messages
            .into_iter()
            .find_map(|message| match message {
                Message::UserInputRunEvent {
                    event: UserInputRunEvent::Finished(result),
                    ..
                } => Some(result),
                _ => None,
            })
            .expect("User Input result")
    }

    #[test]
    fn python_receives_exact_crlf_tabs_unicode_and_trailing_newlines() {
        let input = "1\t2\r\n界😀\n\n";
        let result = execute_source(
            "import sys\nsys.stdout.buffer.write(sys.stdin.buffer.read())\nsys.stderr.buffer.write(b'diagnostic\\r\\n')\n",
            request(Language::Python, input),
            python_config(),
        );
        assert_eq!(result.stdout.as_bytes(), input.as_bytes());
        assert_eq!(result.stderr, "diagnostic\r\n");
        assert_eq!(result.status, UserInputRunStatus::Finished);
        assert!(result.elapsed > Duration::ZERO);
    }

    #[test]
    fn python_nonzero_exit_is_runtime_error_with_captured_output() {
        let result = execute_source(
            "import sys\nprint('partial')\nsys.stderr.write('failure')\nsys.exit(3)\n",
            request(Language::Python, ""),
            python_config(),
        );
        assert_eq!(result.status, UserInputRunStatus::RuntimeError);
        assert!(result.stdout.contains("partial"));
        assert!(result.stderr.contains("failure"));
    }

    #[test]
    fn python_timeout_cleans_up_descendants_and_captures_output() {
        let config = RunnerConfig {
            timeout_seconds: 0.5,
            ..python_config()
        };
        let started = Instant::now();
        let result = execute_source(
            "import sys, subprocess, time\nsubprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\nprint('started', flush=True)\nsys.stderr.write('stderr'); sys.stderr.flush()\ntime.sleep(30)\n",
            request(Language::Python, ""),
            config,
        );
        assert_eq!(result.status, UserInputRunStatus::TimedOut);
        assert!(result.stdout.contains("started"));
        assert!(result.stderr.contains("stderr"));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(result.elapsed >= Duration::from_millis(500));
    }

    #[test]
    fn cpp_compiles_and_runs_without_loading_or_judging_samples() {
        let config = RunnerConfig {
            cpp_compiler: executable(&["g++", "clang++"]),
            ..RunnerConfig::default()
        };
        let result = execute_source(
            "#include <iostream>\nint main(){int a,b;std::cin>>a>>b;std::cout<<a+b;std::cerr<<\"diagnostic\";}\n",
            request(Language::Cpp, "2 4\n"),
            config,
        );
        assert_eq!(result.status, UserInputRunStatus::Finished);
        assert_eq!(result.stdout, "6");
        assert_eq!(result.stderr, "diagnostic");
        assert!(result.elapsed > Duration::ZERO);
    }

    #[test]
    fn cpp_compile_error_is_a_user_input_result_with_elapsed_and_diagnostics() {
        let config = RunnerConfig {
            cpp_compiler: executable(&["g++", "clang++"]),
            ..RunnerConfig::default()
        };
        let result = execute_source("this is not C++;", request(Language::Cpp, "unused"), config);
        assert_eq!(result.status, UserInputRunStatus::CompileError);
        assert!(!result.stderr.is_empty());
        assert!(result.elapsed > Duration::ZERO);
    }

    #[test]
    fn compile_timeout_uses_existing_runner_deadline_and_capture() {
        let config = RunnerConfig {
            cpp_compiler: executable(&["python3", "python"]),
            cpp_flags: vec!["-c".into(), "import sys,time; print('compiling',flush=True); sys.stderr.write('diagnostic'); sys.stderr.flush(); time.sleep(30)".into()],
            compile_timeout_seconds: 0.5, ..RunnerConfig::default()
        };
        let result = execute_source("unused source", request(Language::Cpp, "unused"), config);
        assert_eq!(result.status, UserInputRunStatus::CompileTimedOut);
        assert!(result.stdout.contains("compiling"));
        assert!(result.stderr.contains("diagnostic"));
        assert!(result.elapsed >= Duration::from_millis(500));
    }

    #[test]
    fn runtime_and_compile_cancellation_join_process_tree_before_completion() {
        for compiling in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let script = "import pathlib,subprocess,sys,time\nsubprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])\npathlib.Path('started').write_text('ready')\ntime.sleep(30)\n";
            let mut config = python_config();
            config.timeout_seconds = 30.0;
            config.compile_timeout_seconds = 30.0;
            let language = if compiling {
                config.cpp_compiler = config.python.clone();
                config.cpp_flags = vec!["-c".into(), script.into()];
                Language::Cpp
            } else {
                Language::Python
            };
            std::fs::write(
                temp.path().join(format!("A.{}", language.extension())),
                script,
            )
            .unwrap();
            let (tx, rx) = mpsc::channel();
            let executor = executor(temp.path(), config, tx);
            let (completion_tx, completion_rx) = mpsc::channel();
            let active = executor
                .spawn(request(language, ""), completion_tx)
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !temp.path().join("started").exists() {
                assert!(Instant::now() < deadline, "child did not start");
                std::thread::sleep(Duration::from_millis(5));
            }
            let started = Instant::now();
            active.request_cancel();
            completion_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(matches!(active.join().unwrap(), AttemptOutcome::Cancelled));
            assert!(started.elapsed() < Duration::from_secs(10));
            assert!(!rx.try_iter().any(|message| matches!(
                message,
                Message::UserInputRunEvent {
                    event: UserInputRunEvent::Finished(_),
                    ..
                } | Message::RunEvent { .. }
            )));
        }
    }

    #[test]
    fn source_removal_prevents_old_stdin_execution_on_a_recreated_real_source() {
        use super::super::attempt_executor::spawn_with;
        use super::super::watch_worker::RunWorker;
        use crate::attempt::{clean_cancellation_io_error, run_attempt};
        use crate::tui::message::RunWorkerCommand;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("A.py");
        std::fs::write(&source, "pass\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let executor = executor(temp.path(), python_config(), tx.clone());
        let (spawned_tx, spawned_rx) = mpsc::channel();
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut first_release = Some(release_rx);
        let worker = RunWorker::start_with_test_attempt(tx, move |request, completion| {
            spawned_tx.send(request.clone()).unwrap();
            if matches!(request.kind, RunKind::Samples) {
                let release = first_release.take();
                let waiting_tx = waiting_tx.clone();
                return spawn_with(request, completion, move |cancellation| {
                    run_attempt(&cancellation, |is_cancelled| {
                        if let Some(release) = release {
                            let deadline = Instant::now() + Duration::from_secs(5);
                            while !is_cancelled() {
                                assert!(Instant::now() < deadline);
                                std::thread::yield_now();
                            }
                            waiting_tx.send(()).unwrap();
                            release.recv_timeout(Duration::from_secs(5)).unwrap();
                            Err(clean_cancellation_io_error().into())
                        } else {
                            Ok(())
                        }
                    })
                });
            }
            executor.spawn(request, completion)
        })
        .unwrap();
        let sender = worker.sender();
        let x = RunRequest {
            problem: 1,
            run_id: 1,
            language: Language::Python,
            debug: false,
            kind: RunKind::Samples,
        };
        sender.send(RunWorkerCommand::Run(x.clone())).unwrap();
        assert_eq!(spawned_rx.recv_timeout(Duration::from_secs(5)).unwrap(), x);
        let old = request(Language::Python, "old stdin");
        sender.send(RunWorkerCommand::Run(old.clone())).unwrap();
        waiting_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let RunKind::UserInput(old_snapshot) = &old.kind else {
            unreachable!()
        };
        std::fs::remove_file(&source).unwrap();
        let before_source_revision = old_snapshot.source_revision + 1;
        old_snapshot
            .start_gate
            .retire_before(before_source_revision);
        let retirement = RunWorkerCommand::RetireUserInputRuns {
            problem: 0,
            before_source_revision,
        };
        sender.send(retirement.clone()).unwrap();
        std::fs::write(&source, "import sys,pathlib\ndata=sys.stdin.buffer.read()\nwith pathlib.Path('executed').open('ab') as f: f.write(data)\nsys.stdout.buffer.write(data)\n").unwrap();
        release_tx.send(()).unwrap();
        // The preserved sample is a scheduler barrier: an unretired old input would have
        // started before it. No newer request has been issued yet to hide that failure.
        let next = spawned_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            next, x,
            "old input was physically admitted after source recreation"
        );
        assert!(!temp.path().join("executed").exists());

        let mut fresh = old;
        fresh.run_id += 1;
        let RunKind::UserInput(snapshot) = &mut fresh.kind else {
            unreachable!()
        };
        let snapshot = Arc::make_mut(snapshot);
        snapshot.source_revision += 2;
        snapshot.input = Arc::from("new stdin");
        let fresh_id = fresh.run_id;
        sender.send(RunWorkerCommand::Run(fresh)).unwrap();
        sender.send(retirement).unwrap(); // Even a delayed duplicate must preserve new work.
        let mut output = None;
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Message::UserInputRunEvent {
                    run_id,
                    event: UserInputRunEvent::Finished(result),
                    ..
                } if run_id == fresh_id => output = Some(result),
                Message::RunCompleted { run_id, .. } if run_id == fresh_id => break,
                Message::RunFailed { error, .. } => panic!("{error}"),
                Message::WorkerFailed(error) => panic!("{error}"),
                _ => {}
            }
        }
        worker.stop_and_join().unwrap();
        assert_eq!(output.unwrap().stdout, "new stdin");
        assert_eq!(
            std::fs::read(temp.path().join("executed")).unwrap(),
            b"new stdin"
        );
        assert!(spawned_rx.try_iter().all(|request| request.run_id != 17));
    }

    #[test]
    fn worker_repeated_user_input_run_preempts_old_process_and_finishes_latest_once() {
        use super::super::watch_worker::RunWorker;
        use crate::tui::message::RunWorkerCommand;
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("A.py"),
            "import sys,time,pathlib\ndata=sys.stdin.buffer.read()\nif data == b'block':\n pathlib.Path('started').write_text('ready')\n time.sleep(30)\nsys.stdout.buffer.write(data)\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let executor = executor(temp.path(), python_config(), tx.clone());
        let worker = RunWorker::start_with_test_attempt(tx, move |request, completion| {
            executor.spawn(request, completion)
        })
        .unwrap();
        worker
            .sender()
            .send(RunWorkerCommand::Run(request(Language::Python, "block")))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !temp.path().join("started").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let latest = RunRequest {
            run_id: 18,
            ..request(Language::Python, "latest\r\n\t界\n")
        };
        worker.sender().send(RunWorkerCommand::Run(latest)).unwrap();
        let mut results = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Message::UserInputRunEvent {
                    run_id,
                    event: UserInputRunEvent::Finished(result),
                    ..
                } => results.push((run_id, result)),
                Message::RunCompleted { run_id: 18, .. } => break,
                Message::RunStarted { .. }
                | Message::UserInputRunEvent {
                    event: UserInputRunEvent::Running,
                    ..
                } => {}
                other => panic!("unexpected worker message: {other:?}"),
            }
        }
        worker.stop_and_join().unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 18);
        assert_eq!(results[0].1.stdout, "latest\r\n\t界\n");
        assert_eq!(results[0].1.status, UserInputRunStatus::Finished);
    }

    fn finished(messages: Vec<Message>, request: &RunRequest) -> UserInputRunResult {
        let RunKind::UserInput(identity) = &request.kind else {
            unreachable!()
        };
        messages
            .into_iter()
            .find_map(|message| match message {
                Message::UserInputRunEvent {
                    snapshot,
                    event: UserInputRunEvent::Finished(result),
                    ..
                } => {
                    assert_eq!(&snapshot, identity);
                    Some(result)
                }
                Message::RunFailed { error, .. } => panic!("runner failed: {error}"),
                _ => None,
            })
            .expect("finished result")
    }

    #[test]
    fn python_user_input_uses_normal_live_script_invocation() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("contest with spaces");
        std::fs::create_dir(&destination).unwrap();
        let path = destination.join("A.py");
        std::fs::write(&path, "raise AssertionError('previous source')\n").unwrap();
        let input = "\r\n\t界😀\n\n";
        let request = request(Language::Python, input);
        // Source changes after the request was made are read through the normal live path.
        let source = concat!(
            "# coding: latin-1\nfrom __future__ import annotations\n",
            "import sys, builtins, __main__, sibling\n",
            "__builtins__.print('normal script')\n",
            "print(__file__, __name__, __package__, __spec__, __cached__)\n",
            "print(type(__builtins__), __builtins__ is builtins)\n",
            "print(type(__loader__), __loader__.name, __loader__.path)\n",
            "print(sys.argv, sys.path[0], __main__.__dict__ is globals(), sibling.VALUE)\n",
            "sys.stdout.flush()\nsys.stdout.buffer.write('caf",
        );
        let mut source = source.as_bytes().to_vec();
        source.extend_from_slice(
            b"\xe9'.encode('utf-8'))\nsys.stdout.buffer.write(sys.stdin.buffer.read())\n",
        );
        std::fs::write(&path, &source).unwrap();
        std::fs::write(destination.join("sibling.py"), "VALUE = 'local import'\n").unwrap();
        let config = python_config();
        let expected = runner::execute_python_with_cancel_in(
            &path,
            input,
            &config.python,
            Duration::from_secs(10),
            &|| false,
            &destination,
        )
        .unwrap();
        assert!(
            matches!(expected.outcome, ExecutionOutcome::Exited(status) if status.success()),
            "{}",
            expected.stderr
        );
        let actual = finished(
            execute_user_input_for_test(&destination, request.clone(), config),
            &request,
        );
        assert_eq!(
            actual.status,
            UserInputRunStatus::Finished,
            "{}",
            actual.stderr
        );
        assert_eq!(actual.stdout, expected.stdout);
        assert_eq!(actual.stderr, expected.stderr);
        assert!(actual.stdout.ends_with(input));
        assert_eq!(std::fs::read(&path).unwrap(), source);
        assert!(!destination.join(".atc").exists());
    }

    #[test]
    fn cpp_user_input_uses_normal_live_compile_flags_includes_and_debug() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("contest with spaces");
        std::fs::create_dir_all(destination.join("nested")).unwrap();
        std::fs::write(
            destination.join("local.hpp"),
            "#include \"nested/inner.hpp\"\n",
        )
        .unwrap();
        std::fs::write(destination.join("nested/inner.hpp"), "#define VALUE 42\n").unwrap();
        let path = destination.join("A.cpp");
        std::fs::write(&path, "#error previous source\n").unwrap();
        let input = "input\twith\nnewlines\n";
        let request = RunRequest {
            debug: true,
            ..request(Language::Cpp, input)
        };
        let source = concat!(
            "\u{feff}#include <iostream>\n#include \"local.hpp\"\n#include <atc/debug.hpp>\n",
            "#ifndef LOCAL\n#error missing LOCAL\n#endif\n",
            "int main(){std::cout<<__FILE__<<'\\n'<<VALUE<<'\\n'<<EXTRA<<'\\n'<<std::cin.rdbuf();}\n",
        );
        std::fs::write(&path, source).unwrap();
        let mut config = RunnerConfig {
            cpp_compiler: executable(&["g++", "clang++"]),
            ..Default::default()
        };
        config.cpp_flags.push("-DEXTRA=7".into());
        let output = temp
            .path()
            .join(format!("A{}", std::env::consts::EXE_SUFFIX));
        let options = runner::BuildOptions {
            debug_include_dir: Some(crate::debug::materialize_debug_header().unwrap()),
        };
        let compiled = runner::compile_cpp_with_cancel_in(
            &path,
            &output,
            &config.cpp_compiler,
            &config.cpp_flags,
            Duration::from_secs(20),
            &options,
            &|| false,
            &destination,
        )
        .unwrap();
        assert!(
            matches!(compiled.outcome, ExecutionOutcome::Exited(status) if status.success()),
            "{}",
            compiled.stderr
        );
        let expected = runner::execute_with_cancel_in(
            &output,
            &[],
            input,
            Duration::from_secs(10),
            &|| false,
            &destination,
        )
        .unwrap();
        assert!(
            matches!(expected.outcome, ExecutionOutcome::Exited(status) if status.success()),
            "{}",
            expected.stderr
        );
        let actual = finished(
            execute_user_input_for_test(&destination, request.clone(), config),
            &request,
        );
        assert_eq!(
            actual.status,
            UserInputRunStatus::Finished,
            "{}",
            actual.stderr
        );
        assert_eq!(actual.stdout, expected.stdout);
        assert_eq!(actual.stderr, expected.stderr);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(!destination.join(".atc").exists());
    }
}
