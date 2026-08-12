use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

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

pub fn compile_cpp(
    source: &Path,
    output: &Path,
    compiler: &str,
    cpp_flags: &[String],
    timeout: Duration,
    options: &BuildOptions,
) -> Result<ExecutionResult, io::Error> {
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

    execute(Path::new(compiler), &args, "", timeout)
}

pub fn execute(
    program: &Path,
    args: &[OsString],
    input: &str,
    timeout: Duration,
) -> Result<ExecutionResult, io::Error> {
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

    // Start draining output before writing input. A program may write enough output to fill
    // its pipe before it starts reading stdin, so doing either operation synchronously first
    // can deadlock.
    let stdout_handle = thread::spawn(move || read_all(stdout));
    let stderr_handle = thread::spawn(move || read_all(stderr));
    let input = input.as_bytes().to_vec();
    let stdin_handle = thread::spawn(move || write_input(stdin, &input));

    let remaining = timeout.saturating_sub(started.elapsed());
    let outcome_result = match child.wait_timeout(remaining) {
        Ok(Some(status)) => Ok(ExecutionOutcome::Exited(status)),
        Ok(None) => child
            .terminate_and_wait()
            .map(|()| ExecutionOutcome::TimedOut),
        Err(error) => {
            let cleanup = child.terminate_and_wait();
            Err(with_cleanup_error(error, cleanup.err()))
        }
    };
    let elapsed = started.elapsed();

    // Run the guard before joining the pipe workers if wait/kill failed. This makes one final
    // kill + wait attempt, allowing every pipe to reach EOF instead of leaving detached workers.
    drop(child);

    let stdin_result = join_worker(stdin_handle, "stdin writer");
    let stdout_result = join_worker(stdout_handle, "stdout reader");
    let stderr_result = join_worker(stderr_handle, "stderr reader");

    let outcome = outcome_result?;
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

    fn helper_args(name: &str) -> Vec<OsString> {
        vec![
            OsString::from("--exact"),
            OsString::from(format!("runner::tests::{name}")),
            OsString::from("--ignored"),
            OsString::from("--nocapture"),
        ]
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
}
