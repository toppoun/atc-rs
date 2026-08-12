use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitStatus;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use std::time::Instant;
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
    pub debug: bool,
}

pub fn execute_python(
    source: &Path,
    input: &str,
    python: &str,
    timeout: Duration,
) -> Result<ExecutionResult, std::io::Error> {
    let args = vec![source.to_string_lossy().into_owned()];

    execute(Path::new(python), &args, input, timeout)
}

pub fn compile_cpp(
    source: &Path,
    output: &Path,
    compiler: &str,
    cpp_flags: &[String],
    timeout: Duration,
    options: &BuildOptions,
) -> Result<ExecutionResult, std::io::Error> {
    let mut args = cpp_flags.to_vec();

    args.push(source.to_string_lossy().into_owned());
    args.push("-o".to_string());
    args.push(output.to_string_lossy().into_owned());

    // debug用フラグは後でここに足す
    if options.debug {
        // -DLOCAL
        // -I <include dir>
    }

    execute(Path::new(compiler), &args, "", timeout)
}

pub fn execute(
    program: &Path,
    args: &[String],
    input: &str,
    timeout: Duration,
) -> Result<ExecutionResult, std::io::Error> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes())?;
    }
    let mut stdout = child.stdout.take().expect("stdout should be piped");
    let mut stderr = child.stderr.take().expect("stderr should be piped");

    let stdout_handle = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output)?;
        Ok::<Vec<u8>, std::io::Error>(output)
    });

    let stderr_handle = thread::spawn(move || {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output)?;
        Ok::<Vec<u8>, std::io::Error>(output)
    });

    let outcome = match child.wait_timeout(timeout)? {
        Some(status) => ExecutionOutcome::Exited(status),

        None => {
            child.kill()?;
            child.wait()?;
            ExecutionOutcome::TimedOut
        }
    };

    let elapsed = started.elapsed();

    let stdout_bytes = stdout_handle
        .join()
        .expect("stdout reader thread should not panic")?;

    let stderr_bytes = stderr_handle
        .join()
        .expect("stderr reader thread should not panic")?;

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    Ok(ExecutionResult {
        outcome,
        stdout,
        stderr,
        elapsed,
    })
}
