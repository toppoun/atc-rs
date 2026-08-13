use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

use crate::attempt::{clean_cancellation_io_error, io_error_is_clean_cancellation};

const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub enum ExecutionOutcome {
    Exited(ExitStatus),
    TimedOut,
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub outcome: ExecutionOutcome,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

#[derive(Debug, Default)]
pub struct BuildOptions {
    pub debug_include_dir: Option<PathBuf>,
}

pub fn execute_python(
    source: &Path,
    input: &str,
    python: &str,
    timeout: Duration,
) -> Result<ExecutionResult, io::Error> {
    let args = vec![source.as_os_str().to_owned()];

    execute(Path::new(python), &args, input, timeout)
}

pub fn execute_python_with_cancel(
    source: &Path,
    input: &str,
    python: &str,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<ExecutionResult, io::Error> {
    let args = vec![source.as_os_str().to_owned()];

    execute_with_cancel(Path::new(python), &args, input, timeout, is_cancelled)
}

pub fn compile_cpp(
    source: &Path,
    output: &Path,
    compiler: &str,
    cpp_flags: &[String],
    timeout: Duration,
    options: &BuildOptions,
) -> Result<ExecutionResult, io::Error> {
    let args = cpp_arguments(source, output, cpp_flags, options);

    execute(Path::new(compiler), &args, "", timeout)
}

pub fn compile_cpp_with_cancel(
    source: &Path,
    output: &Path,
    compiler: &str,
    cpp_flags: &[String],
    timeout: Duration,
    options: &BuildOptions,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<ExecutionResult, io::Error> {
    let args = cpp_arguments(source, output, cpp_flags, options);

    execute_with_cancel(Path::new(compiler), &args, "", timeout, is_cancelled)
}

fn cpp_arguments(
    source: &Path,
    output: &Path,
    cpp_flags: &[String],
    options: &BuildOptions,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = cpp_flags.iter().map(OsString::from).collect();

    args.push(source.as_os_str().to_owned());
    args.push(OsString::from("-o"));
    args.push(output.as_os_str().to_owned());

    // debug用フラグ
    if let Some(include_dir) = &options.debug_include_dir {
        args.push("-DLOCAL".into());
        args.push("-I".into());
        args.push(include_dir.as_os_str().to_os_string());
    }

    args
}

pub fn execute(
    program: &Path,
    args: &[OsString],
    input: &str,
    timeout: Duration,
) -> Result<ExecutionResult, io::Error> {
    execute_with_cancel(program, args, input, timeout, &|| false)
}

pub fn execute_with_cancel(
    program: &Path,
    args: &[OsString],
    input: &str,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<ExecutionResult, io::Error> {
    if is_cancelled() {
        return Err(clean_cancellation_io_error());
    }

    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let started = Instant::now();
    let mut child = ChildGuard::new(child);

    let stdin = child.take_stdin()?;
    let stdout = child.take_stdout()?;
    let stderr = child.take_stderr()?;

    let stdout_handle = thread::spawn(move || read_all(stdout));
    let stderr_handle = thread::spawn(move || read_all(stderr));

    let input = input.as_bytes().to_vec();
    let stdin_handle = thread::spawn(move || write_input(stdin, &input));

    let outcome_result = wait_for_child(&mut child, started, timeout, is_cancelled);

    let elapsed = started.elapsed();

    drop(child);

    let stdin_result = join_worker(stdin_handle, "stdin writer");
    let stdout_result = join_worker(stdout_handle, "stdout reader");
    let stderr_result = join_worker(stderr_handle, "stderr reader");

    let outcome = match outcome_result {
        Ok(outcome) => outcome,
        Err(error) if io_error_is_clean_cancellation(&error) => {
            // clean cancellationはpipe threadも正常にjoinできた場合だけ返す。
            // reader/writer側の失敗をCancelledとして握りつぶさない。
            stdin_result?;
            stdout_result?;
            stderr_result?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };

    stdin_result?;

    let stdout_bytes = stdout_result?;
    let stderr_bytes = stderr_result?;

    Ok(ExecutionResult {
        outcome,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        elapsed,
    })
}

fn wait_for_child(
    child: &mut ChildGuard,
    started: Instant,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
) -> io::Result<ExecutionOutcome> {
    loop {
        if is_cancelled() {
            return cancellation_result(child.terminate_and_wait());
        }

        let remaining = timeout.saturating_sub(started.elapsed());

        let wait_for = remaining.min(CANCEL_POLL_INTERVAL);

        match child.wait_timeout(wait_for) {
            Ok(Some(status)) => {
                return Ok(ExecutionOutcome::Exited(status));
            }

            Ok(None) if started.elapsed() >= timeout => {
                child.terminate_and_wait()?;

                return Ok(ExecutionOutcome::TimedOut);
            }

            Ok(None) => {}

            Err(error) => {
                let cleanup = child.terminate_and_wait();

                return Err(with_cleanup_error(error, cleanup.err()));
            }
        }
    }
}

fn cancellation_result(cleanup: io::Result<()>) -> io::Result<ExecutionOutcome> {
    match cleanup {
        Ok(()) => Err(clean_cancellation_io_error()),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("process cancellation cleanup failed: {error}"),
        )),
    }
}

fn read_all(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    pipe.read_to_end(&mut output)?;
    Ok(output)
}

fn write_input(mut stdin: impl Write, input: &[u8]) -> io::Result<()> {
    match stdin.write_all(input) {
        // Closing stdin without consuming all input is valid program behaviour. Its exit status
        // and output still determine the verdict.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

fn join_worker<T>(handle: JoinHandle<io::Result<T>>, name: &str) -> io::Result<T> {
    match handle.join() {
        Ok(result) => {
            result.map_err(|error| io::Error::new(error.kind(), format!("{name} failed: {error}")))
        }
        Err(_) => Err(io::Error::other(format!("{name} thread panicked"))),
    }
}

fn with_cleanup_error(original: io::Error, cleanup: Option<io::Error>) -> io::Error {
    let Some(cleanup) = cleanup else {
        return original;
    };

    io::Error::new(
        original.kind(),
        format!("{original}; child cleanup also failed: {cleanup}"),
    )
}

struct ChildGuard {
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn take_stdin(&mut self) -> io::Result<std::process::ChildStdin> {
        self.child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("child stdin was not piped"))
    }

    fn take_stdout(&mut self) -> io::Result<std::process::ChildStdout> {
        self.child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child stdout was not piped"))
    }

    fn take_stderr(&mut self) -> io::Result<std::process::ChildStderr> {
        self.child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("child stderr was not piped"))
    }

    fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let status = self.child.wait_timeout(timeout)?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    fn terminate_and_wait(&mut self) -> io::Result<()> {
        let kill_error = self.child.kill().err();

        match self.child.wait() {
            Ok(_) => {
                self.reaped = true;
                // kill() can race with a process that exits at the timeout boundary. A successful
                // wait proves that it has still been reaped, so that kill error is harmless.
                Ok(())
            }
            Err(wait_error) => Err(match kill_error {
                Some(kill_error) => io::Error::new(
                    wait_error.kind(),
                    format!("failed to kill child: {kill_error}; failed to wait: {wait_error}"),
                ),
                None => wait_error,
            }),
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            if self.child.wait().is_ok() {
                self.reaped = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attempt::{AttemptCancellation, AttemptOutcome, run_attempt};
    use crate::error::AppError;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn helper_args(name: &str) -> Vec<OsString> {
        vec![
            OsString::from("--exact"),
            OsString::from(format!("runner::tests::{name}")),
            OsString::from("--ignored"),
            OsString::from("--nocapture"),
        ]
    }

    #[test]
    fn cpp_arguments_add_debug_flags_and_include_root_only_for_debug_builds() {
        let source = Path::new("source.cpp");
        let output = Path::new("program");
        let flags = vec!["-std=c++23".to_string(), "-O2".to_string()];

        let normal = cpp_arguments(source, output, &flags, &BuildOptions::default());
        assert_eq!(
            normal,
            ["-std=c++23", "-O2", "source.cpp", "-o", "program"].map(OsString::from)
        );

        let include_dir = PathBuf::from("cache root with spaces").join("include");
        let debug = cpp_arguments(
            source,
            output,
            &flags,
            &BuildOptions {
                debug_include_dir: Some(include_dir.clone()),
            },
        );
        assert_eq!(&debug[..5], &normal);
        assert_eq!(debug[5], OsString::from("-DLOCAL"));
        assert_eq!(debug[6], OsString::from("-I"));
        assert_eq!(debug[7], include_dir.as_os_str());
    }

    #[cfg(unix)]
    #[test]
    fn cpp_arguments_preserve_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;

        let source = PathBuf::from(OsString::from_vec(vec![b's', 0xff]));
        let include = PathBuf::from(OsString::from_vec(vec![b'i', 0xfe]));
        let args = cpp_arguments(
            &source,
            Path::new("program"),
            &[],
            &BuildOptions {
                debug_include_dir: Some(include.clone()),
            },
        );

        assert_eq!(args[0], source.as_os_str());
        assert_eq!(args[5], include.as_os_str());
    }

    #[cfg(windows)]
    #[test]
    fn cpp_arguments_preserve_non_unicode_paths() {
        use std::os::windows::ffi::OsStringExt;

        let source = PathBuf::from(OsString::from_wide(&[b's' as u16, 0xd800]));
        let include = PathBuf::from(OsString::from_wide(&[b'i' as u16, 0xd801]));
        let args = cpp_arguments(
            &source,
            Path::new("program.exe"),
            &[],
            &BuildOptions {
                debug_include_dir: Some(include.clone()),
            },
        );

        assert_eq!(args[0], source.as_os_str());
        assert_eq!(args[5], include.as_os_str());
    }

    #[test]
    fn simultaneously_writes_input_and_drains_large_stdout_and_stderr() {
        let input = "i".repeat(512 * 1024);
        let result = execute(
            &std::env::current_exe().unwrap(),
            &helper_args("large_io_helper"),
            &input,
            Duration::from_secs(5),
        )
        .unwrap();

        assert!(matches!(result.outcome, ExecutionOutcome::Exited(status) if status.success()));
        assert!(result.stdout.contains("input-bytes=524288"));
        assert!(result.stdout.len() >= 512 * 1024);
        assert!(result.stderr.len() >= 512 * 1024);
    }

    #[test]
    fn timeout_kills_and_reaps_child_and_does_not_break_next_execution() {
        let result = execute(
            &std::env::current_exe().unwrap(),
            &helper_args("sleep_helper"),
            "",
            Duration::from_millis(100),
        )
        .unwrap();
        assert!(matches!(result.outcome, ExecutionOutcome::TimedOut));
        assert!(result.elapsed < Duration::from_secs(5));

        let result = execute(
            &std::env::current_exe().unwrap(),
            &helper_args("success_helper"),
            "",
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(result.outcome, ExecutionOutcome::Exited(status) if status.success()));
        assert!(result.stdout.contains("helper-success"));
    }

    #[test]
    fn child_closing_stdin_early_is_not_a_runner_error() {
        let input = "i".repeat(512 * 1024);
        let result = execute(
            &std::env::current_exe().unwrap(),
            &helper_args("close_stdin_helper"),
            &input,
            Duration::from_secs(5),
        )
        .unwrap();

        assert!(matches!(result.outcome, ExecutionOutcome::Exited(status) if status.success()));
        assert!(result.stdout.contains("closed-stdin"));
    }

    #[test]
    fn reader_thread_panic_is_returned_as_an_error() {
        let handle = thread::spawn(|| -> io::Result<()> { panic!("reader panic") });

        let error = join_worker(handle, "test reader").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("test reader thread panicked"));
    }

    #[test]
    #[ignore = "launched as a child process by runner tests"]
    fn large_io_helper() {
        let block = vec![b'o'; 512 * 1024];
        io::stdout().write_all(&block).unwrap();
        io::stdout().flush().unwrap();
        io::stderr().write_all(&block).unwrap();
        io::stderr().flush().unwrap();

        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input).unwrap();
        println!("input-bytes={}", input.len());
    }

    #[test]
    #[ignore = "launched as a child process by runner tests"]
    fn sleep_helper() {
        thread::sleep(Duration::from_secs(10));
    }

    #[test]
    #[ignore = "launched as a child process by runner tests"]
    fn success_helper() {
        println!("helper-success");
    }

    #[test]
    #[ignore = "launched as a child process by runner tests"]
    fn close_stdin_helper() {
        println!("closed-stdin");
    }
    #[test]
    fn cancellation_kills_and_reaps_child() {
        let cancelled = Arc::new(AtomicBool::new(false));

        let trigger = {
            let cancelled = Arc::clone(&cancelled);

            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                cancelled.store(true, Ordering::Relaxed);
            })
        };

        let started = Instant::now();

        let error = execute_with_cancel(
            &std::env::current_exe().unwrap(),
            &helper_args("sleep_helper"),
            "",
            Duration::from_secs(10),
            &|| cancelled.load(Ordering::Relaxed),
        )
        .unwrap_err();

        trigger.join().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(io_error_is_clean_cancellation(&error));

        assert!(started.elapsed() < Duration::from_secs(5));

        // cancelしたprocessをちゃんとreapできていて、
        // 次のprocess実行にも影響しないことを確認
        let result = execute(
            &std::env::current_exe().unwrap(),
            &helper_args("success_helper"),
            "",
            Duration::from_secs(5),
        )
        .unwrap();

        assert!(matches!(
            result.outcome,
            ExecutionOutcome::Exited(status)
                if status.success()
        ));
    }

    #[test]
    fn already_cancelled_execution_does_not_spawn_the_program() {
        let error = execute_with_cancel(
            Path::new("definitely-not-a-real-program"),
            &[],
            "",
            Duration::from_secs(1),
            &|| true,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(io_error_is_clean_cancellation(&error));
    }

    #[test]
    fn cancellation_cleanup_failure_is_not_a_clean_cancellation() {
        let cancellation = AttemptCancellation::new();
        cancellation.request();

        let outcome = run_attempt(&cancellation, |is_cancelled| {
            assert!(is_cancelled());
            cancellation_result(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cannot reap child",
            )))
            .map(|_| ())
            .map_err(AppError::from)
        });

        assert!(matches!(
            outcome,
            AttemptOutcome::Failed(AppError::Io(ref error))
                if error.kind() == io::ErrorKind::PermissionDenied
                    && error.to_string().contains("cannot reap child")
                    && !io_error_is_clean_cancellation(error)
        ));
    }
}
