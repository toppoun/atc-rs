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
