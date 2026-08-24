use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

use crate::attempt::{clean_cancellation_io_error, io_error_is_clean_cancellation};

const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionCheckpoint {
    ChildSpawned,
    CancelObserved,
    ChildReaped,
    PipeThreadsJoined,
}

#[derive(Debug)]
pub enum ExecutionOutcome {
    Exited(ExitStatus),
    TimedOut,
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub outcome: ExecutionOutcome,
    pub stdout: String,
    pub stdout_is_utf8: bool,
    pub stdout_truncated: bool,
    pub stderr: String,
    pub elapsed: Duration,
}

#[derive(Debug, Default)]
pub struct BuildOptions {
    pub debug_include_dir: Option<PathBuf>,
}

pub fn execute_python_in(
    source: &Path,
    input: &str,
    python: &str,
    timeout: Duration,
    working_directory: &Path,
) -> Result<ExecutionResult, io::Error> {
    let args = vec![source.as_os_str().to_owned()];

    execute_in(Path::new(python), &args, input, timeout, working_directory)
}

pub fn execute_python_with_cancel_in(
    source: &Path,
    input: &str,
    python: &str,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
    working_directory: &Path,
) -> Result<ExecutionResult, io::Error> {
    let args = vec![source.as_os_str().to_owned()];

    execute_with_cancel_in(
        Path::new(python),
        &args,
        input,
        timeout,
        is_cancelled,
        working_directory,
    )
}

pub fn compile_cpp_in(
    source: &Path,
    output: &Path,
    compiler: &str,
    cpp_flags: &[String],
    timeout: Duration,
    options: &BuildOptions,
    working_directory: &Path,
) -> Result<ExecutionResult, io::Error> {
    let args = cpp_arguments(source, output, cpp_flags, options);

    execute_in(Path::new(compiler), &args, "", timeout, working_directory)
}

#[allow(clippy::too_many_arguments)]
pub fn compile_cpp_with_cancel_in(
    source: &Path,
    output: &Path,
    compiler: &str,
    cpp_flags: &[String],
    timeout: Duration,
    options: &BuildOptions,
    is_cancelled: &dyn Fn() -> bool,
    working_directory: &Path,
) -> Result<ExecutionResult, io::Error> {
    let args = cpp_arguments(source, output, cpp_flags, options);

    execute_with_cancel_in(
        Path::new(compiler),
        &args,
        "",
        timeout,
        is_cancelled,
        working_directory,
    )
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

#[cfg(test)]
pub fn execute(
    program: &Path,
    args: &[OsString],
    input: &str,
    timeout: Duration,
) -> Result<ExecutionResult, io::Error> {
    execute_with_cancel(program, args, input, timeout, &|| false)
}

#[cfg(test)]
pub fn execute_with_cancel(
    program: &Path,
    args: &[OsString],
    input: &str,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<ExecutionResult, io::Error> {
    execute_with_cancel_observer(program, args, input, timeout, is_cancelled, &|_| {})
}

pub fn execute_in(
    program: &Path,
    args: &[OsString],
    input: &str,
    timeout: Duration,
    working_directory: &Path,
) -> Result<ExecutionResult, io::Error> {
    execute_with_cancel_in(program, args, input, timeout, &|| false, working_directory)
}

pub fn execute_with_cancel_in(
    program: &Path,
    args: &[OsString],
    input: &str,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
    working_directory: &Path,
) -> Result<ExecutionResult, io::Error> {
    execute_with_cancel_observer_in(
        program,
        args,
        input,
        timeout,
        is_cancelled,
        &|_| {},
        Some(working_directory),
    )
}

#[cfg(test)]
pub(crate) fn execute_with_cancel_observer(
    program: &Path,
    args: &[OsString],
    input: &str,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
    observer: &dyn Fn(ExecutionCheckpoint),
) -> Result<ExecutionResult, io::Error> {
    execute_with_cancel_observer_in(program, args, input, timeout, is_cancelled, observer, None)
}

#[allow(clippy::too_many_arguments)]
fn execute_with_cancel_observer_in(
    program: &Path,
    args: &[OsString],
    input: &str,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
    observer: &dyn Fn(ExecutionCheckpoint),
    working_directory: Option<&Path>,
) -> Result<ExecutionResult, io::Error> {
    if is_cancelled() {
        return Err(clean_cancellation_io_error());
    }

    let mut command = Command::new(program);
    if let Some(working_directory) = working_directory {
        command.current_dir(working_directory);
    }
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let process_tree = PreparedProcessTree::prepare(&mut command)?;
    let mut spawned_child = command.spawn()?;
    let process_tree = match process_tree.attach(&spawned_child) {
        Ok(process_tree) => process_tree,
        Err(error) => {
            let kill_error = spawned_child.kill().err();
            let wait_error = spawned_child.wait().err();
            let cleanup_error = wait_error.or(kill_error);
            return Err(with_cleanup_error(error, cleanup_error));
        }
    };

    let started = Instant::now();
    let mut child = ChildGuard::new(spawned_child, process_tree);

    let stdin = child.take_stdin()?;
    let stdout = child.take_stdout()?;
    let stderr = child.take_stderr()?;

    let stdout_handle = thread::spawn(move || read_all(stdout));
    let stderr_handle = thread::spawn(move || read_all(stderr));

    let input = input.as_bytes().to_vec();
    let stdin_handle = thread::spawn(move || write_input(stdin, &input));

    observer(ExecutionCheckpoint::ChildSpawned);

    let outcome_result = wait_for_child(&mut child, started, timeout, is_cancelled, observer);

    let elapsed = started.elapsed();

    drop(child);

    let stdin_result = join_worker(stdin_handle, "stdin writer");
    let stdout_result = join_worker(stdout_handle, "stdout reader");
    let stderr_result = join_worker(stderr_handle, "stderr reader");
    observer(ExecutionCheckpoint::PipeThreadsJoined);

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

    let stdout_capture = stdout_result?;
    let stderr_capture = stderr_result?;

    let stdout_is_utf8 = std::str::from_utf8(&stdout_capture.bytes).is_ok();
    let stdout = display_captured_output(&stdout_capture, "stdout");
    let stderr = display_captured_output(&stderr_capture, "stderr");

    Ok(ExecutionResult {
        outcome,
        stdout,
        stdout_is_utf8,
        stdout_truncated: stdout_capture.truncated,
        stderr,
        elapsed,
    })
}

fn wait_for_child(
    child: &mut ChildGuard,
    started: Instant,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
    observer: &dyn Fn(ExecutionCheckpoint),
) -> io::Result<ExecutionOutcome> {
    loop {
        if is_cancelled() {
            observer(ExecutionCheckpoint::CancelObserved);
            let cleanup = child.terminate_and_wait();
            if cleanup.is_ok() {
                observer(ExecutionCheckpoint::ChildReaped);
            }
            return cancellation_result(cleanup);
        }

        let remaining = timeout.saturating_sub(started.elapsed());

        let wait_for = remaining.min(CANCEL_POLL_INTERVAL);

        match child.wait_timeout(wait_for) {
            Ok(Some(status)) => {
                child.terminate_descendants()?;
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

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_all(pipe: impl Read) -> io::Result<CapturedOutput> {
    read_bounded(pipe, MAX_CAPTURE_BYTES)
}

fn read_bounded(mut pipe: impl Read, limit: usize) -> io::Result<CapturedOutput> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;

    loop {
        let read = pipe.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        let remaining = limit.saturating_sub(bytes.len());
        let retained = remaining.min(read);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }

    Ok(CapturedOutput { bytes, truncated })
}

fn display_captured_output(output: &CapturedOutput, stream: &str) -> String {
    let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
    if output.truncated {
        text.push_str(&format!(
            "\n[atc: {stream} truncated after {MAX_CAPTURE_BYTES} bytes]\n"
        ));
    }
    text
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
    process_tree: ProcessTree,
    process_tree_terminated: bool,
    reaped: bool,
}

impl ChildGuard {
    fn new(child: Child, process_tree: ProcessTree) -> Self {
        Self {
            child,
            process_tree,
            process_tree_terminated: false,
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

    fn terminate_descendants(&mut self) -> io::Result<()> {
        if self.process_tree_terminated {
            return Ok(());
        }

        self.process_tree.terminate()?;
        self.process_tree_terminated = true;
        Ok(())
    }

    fn terminate_and_wait(&mut self) -> io::Result<()> {
        let tree_error = self.terminate_descendants().err();
        let kill_error = self.child.kill().err();

        match self.child.wait() {
            Ok(_) => {
                self.reaped = true;
                if let Some(tree_error) = tree_error {
                    return Err(tree_error);
                }
                // kill() can race with a process that exits at the timeout boundary. A successful
                // wait proves that it has still been reaped, so that kill error is harmless.
                Ok(())
            }
            Err(wait_error) => {
                let mut details = Vec::new();
                if let Some(tree_error) = tree_error {
                    details.push(format!("failed to terminate process tree: {tree_error}"));
                }
                if let Some(kill_error) = kill_error {
                    details.push(format!("failed to kill child: {kill_error}"));
                }
                details.push(format!("failed to wait: {wait_error}"));

                Err(io::Error::new(wait_error.kind(), details.join("; ")))
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.terminate_descendants();
        if !self.reaped {
            let _ = self.child.kill();
            if self.child.wait().is_ok() {
                self.reaped = true;
            }
        }
    }
}

#[cfg(unix)]
struct PreparedProcessTree;

#[cfg(unix)]
impl PreparedProcessTree {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
        Ok(Self)
    }

    fn attach(self, child: &Child) -> io::Result<ProcessTree> {
        Ok(ProcessTree {
            process_group: child.id() as libc::pid_t,
        })
    }
}

#[cfg(unix)]
struct ProcessTree {
    process_group: libc::pid_t,
}

#[cfg(unix)]
impl ProcessTree {
    fn terminate(&self) -> io::Result<()> {
        // Each child is placed in its own process group before exec. A negative PID targets the
        // entire group, including ordinary descendants that inherited the group.
        let result = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }

        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
struct PreparedProcessTree {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl PreparedProcessTree {
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        // Assigning an already-running process to a job races with short-lived programs and with
        // descendants spawned before assignment. Start suspended so the job owns the process tree
        // before any user code can execute.
        command.creation_flags(CREATE_SUSPENDED);

        let raw_job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw_job.is_null() {
            return Err(io::Error::last_os_error());
        }

        let job = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw_job.cast()) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { job })
    }

    fn attach(self, child: &Child) -> io::Result<ProcessTree> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let assigned = unsafe {
            AssignProcessToJobObject(
                self.job.as_raw_handle().cast(),
                child.as_raw_handle().cast(),
            )
        };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }

        resume_suspended_process(child.id())?;

        Ok(ProcessTree { job: self.job })
    }
}

#[cfg(windows)]
fn resume_suspended_process(process_id: u32) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    let raw_snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw_snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe {
        std::os::windows::io::OwnedHandle::from_raw_handle(raw_snapshot.cast())
    };

    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    if unsafe { Thread32First(snapshot.as_raw_handle().cast(), &raw mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }

    loop {
        if entry.th32OwnerProcessID == process_id {
            let raw_thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if raw_thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe {
                std::os::windows::io::OwnedHandle::from_raw_handle(raw_thread.cast())
            };

            if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }

        if unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &raw mut entry) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("suspended process {process_id} has no resumable thread"),
            ));
        }
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl ProcessTree {
    fn terminate(&self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        let terminated = unsafe { TerminateJobObject(self.job.as_raw_handle().cast(), 1) };
        if terminated == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
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
        assert!(!result.stdout_truncated);
    }

    #[test]
    fn capture_limit_retains_a_prefix_and_drains_the_rest() {
        let captured = read_bounded(std::io::Cursor::new(b"0123456789"), 4).unwrap();

        assert_eq!(captured.bytes, b"0123");
        assert!(captured.truncated);
    }

    #[test]
    fn explicit_working_directory_is_used_by_the_child() {
        let temp = tempfile::tempdir().unwrap();
        let result = execute_in(
            &std::env::current_exe().unwrap(),
            &helper_args("working_directory_helper"),
            "",
            Duration::from_secs(5),
            temp.path(),
        )
        .unwrap();

        assert!(matches!(result.outcome, ExecutionOutcome::Exited(status) if status.success()));
        let reported = result
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("working-directory="))
            .expect("helper should report its working directory");
        assert_eq!(
            std::fs::canonicalize(reported).unwrap(),
            std::fs::canonicalize(temp.path()).unwrap()
        );
    }

    #[test]
    fn invalid_utf8_stdout_is_marked_even_when_the_process_succeeds() {
        let result = execute(
            &std::env::current_exe().unwrap(),
            &helper_args("invalid_utf8_stdout_helper"),
            "",
            Duration::from_secs(5),
        )
        .unwrap();

        assert!(matches!(result.outcome, ExecutionOutcome::Exited(status) if status.success()));
        assert!(!result.stdout_is_utf8);
        assert!(result.stdout.contains('\u{fffd}'));
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
    fn timeout_terminates_descendants_that_inherit_the_output_pipes() {
        let started = Instant::now();
        let result = execute(
            &std::env::current_exe().unwrap(),
            &helper_args("spawn_descendant_and_sleep_helper"),
            "",
            Duration::from_millis(200),
        )
        .unwrap();

        assert!(matches!(result.outcome, ExecutionOutcome::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn successful_child_cannot_detach_a_descendant_that_holds_output_pipes() {
        let started = Instant::now();
        let result = execute(
            &std::env::current_exe().unwrap(),
            &helper_args("spawn_descendant_and_exit_helper"),
            "",
            Duration::from_secs(5),
        )
        .unwrap();

        assert!(matches!(result.outcome, ExecutionOutcome::Exited(status) if status.success()));
        assert!(started.elapsed() < Duration::from_secs(5));
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
    fn invalid_utf8_stdout_helper() {
        io::stdout().write_all(&[0xff]).unwrap();
        io::stdout().flush().unwrap();
    }

    #[test]
    #[ignore = "launched as a child process by runner tests"]
    fn working_directory_helper() {
        println!(
            "working-directory={}",
            std::env::current_dir().unwrap().display()
        );
    }

    fn write_continuously(mut stdout: bool, mut stderr: bool) {
        let block = vec![b'x'; 64 * 1024];
        let mut stdout_handle = io::stdout().lock();
        let mut stderr_handle = io::stderr().lock();

        loop {
            if stdout && stdout_handle.write_all(&block).is_err() {
                stdout = false;
            }
            if stderr && stderr_handle.write_all(&block).is_err() {
                stderr = false;
            }
            if !stdout && !stderr {
                return;
            }
        }
    }

    #[test]
    #[ignore = "launched as a child process by cancellation stress tests"]
    fn continuous_stdout_helper() {
        write_continuously(true, false);
    }

    #[test]
    #[ignore = "launched as a child process by cancellation stress tests"]
    fn continuous_stderr_helper() {
        write_continuously(false, true);
    }

    #[test]
    #[ignore = "launched as a child process by cancellation stress tests"]
    fn continuous_stdout_stderr_helper() {
        write_continuously(true, true);
    }

    #[test]
    #[ignore = "launched as a child process by runner tests"]
    fn sleep_helper() {
        thread::sleep(Duration::from_secs(10));
    }

    #[test]
    #[ignore = "launched as a child process by runner tests"]
    #[allow(clippy::zombie_processes)]
    fn spawn_descendant_and_exit_helper() {
        Command::new(std::env::current_exe().unwrap())
            .args(helper_args("sleep_helper"))
            .spawn()
            .unwrap();
    }

    #[test]
    #[ignore = "launched as a child process by runner tests"]
    #[allow(clippy::zombie_processes)]
    fn spawn_descendant_and_sleep_helper() {
        Command::new(std::env::current_exe().unwrap())
            .args(helper_args("sleep_helper"))
            .spawn()
            .unwrap();
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
