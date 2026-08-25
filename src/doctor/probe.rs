use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use wait_timeout::ChildExt as _;

const CAPTURE_BYTES_PER_STREAM: usize = 4 * 1024;
const DRAIN_BUFFER_BYTES: usize = 16 * 1024;
const DRAIN_BYTES_PER_STREAM_PER_PASS: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const ROOT_REAP_GRACE: Duration = Duration::from_millis(250);
const FINAL_DRAIN_GRACE: Duration = Duration::from_millis(20);
const PROBE_DEADLINE_EXPIRED: &str = "doctor probe deadline expired during process setup";

#[derive(Debug)]
pub(super) enum VersionProbeOutcome {
    Exited(ExitStatus),
    TimedOut,
}

#[derive(Debug)]
pub(super) struct VersionProbeOutput {
    pub(super) outcome: VersionProbeOutcome,
    pub(super) stdout: CapturedStream,
    pub(super) stderr: CapturedStream,
}

#[derive(Debug)]
pub(super) struct CapturedStream {
    pub(super) text: String,
    pub(super) retained_bytes: usize,
    pub(super) truncated: bool,
}

#[derive(Default)]
struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedCapture {
    fn retain(&mut self, bytes: &[u8]) {
        let remaining = CAPTURE_BYTES_PER_STREAM.saturating_sub(self.bytes.len());
        let retained = remaining.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..retained]);
        self.truncated |= retained < bytes.len();
    }

    fn finish(self) -> CapturedStream {
        CapturedStream {
            retained_bytes: self.bytes.len(),
            text: String::from_utf8_lossy(&self.bytes).into_owned(),
            truncated: self.truncated,
        }
    }
}

pub(super) fn probe_version(
    program: &Path,
    cwd: &Path,
    timeout: Duration,
) -> io::Result<VersionProbeOutput> {
    probe_version_with_spawn_observer(program, cwd, timeout, |_| {})
}

fn probe_version_with_spawn_observer(
    program: &Path,
    cwd: &Path,
    timeout: Duration,
    on_spawn: impl FnOnce(u32),
) -> io::Result<VersionProbeOutput> {
    let started = Instant::now();
    let deadline = started
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "probe timeout is too large"))?;
    let reaper = RootReaper::start()?;

    let mut command = Command::new(program);
    command
        .arg("--version")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let prepared_tree = match PreparedProbeProcessTree::prepare(&mut command, deadline) {
        Ok(prepared_tree) => prepared_tree,
        Err(error) if is_probe_deadline_error(&error) => {
            return Ok(empty_timed_out_output());
        }
        Err(error) => return Err(error),
    };
    let spawned_child = command.spawn()?;
    on_spawn(spawned_child.id());
    let process_tree = match prepared_tree.attach(&spawned_child, deadline) {
        Ok(process_tree) => process_tree,
        Err(error) => {
            let deadline_expired = is_probe_deadline_error(&error);
            let mut unattached = ProbeChild::new_unattached(spawned_child, reaper);
            let cleanup = unattached.terminate_and_reap(ROOT_REAP_GRACE);
            if deadline_expired && cleanup.is_ok() {
                return Ok(empty_timed_out_output());
            }
            return Err(with_cleanup_error(error, cleanup.err()));
        }
    };

    let mut child = ProbeChild::new(spawned_child, process_tree, reaper);
    let mut stdout = child.take_stdout()?;
    let mut stderr = child.take_stderr()?;
    prepare_probe_pipe(&stdout)?;
    prepare_probe_pipe(&stderr)?;

    let mut stdout_capture = BoundedCapture::default();
    let mut stderr_capture = BoundedCapture::default();

    loop {
        drain_probe_pipe(&mut stdout, &mut stdout_capture, deadline)?;
        drain_probe_pipe(&mut stderr, &mut stderr_capture, deadline)?;

        if let Some(status) = child.try_wait()? {
            // The root is already reaped. Closing the original group/job is best-effort cleanup
            // for ordinary descendants; detached Unix daemons are intentionally out of scope.
            child.terminate_process_tree();
            drain_after_root_exit(
                &mut stdout,
                &mut stderr,
                &mut stdout_capture,
                &mut stderr_capture,
            )?;
            return Ok(VersionProbeOutput {
                outcome: VersionProbeOutcome::Exited(status),
                stdout: stdout_capture.finish(),
                stderr: stderr_capture.finish(),
            });
        }

        let now = Instant::now();
        if now >= deadline {
            child.terminate_and_reap(ROOT_REAP_GRACE)?;
            drain_after_root_exit(
                &mut stdout,
                &mut stderr,
                &mut stdout_capture,
                &mut stderr_capture,
            )?;
            return Ok(VersionProbeOutput {
                outcome: VersionProbeOutcome::TimedOut,
                stdout: stdout_capture.finish(),
                stderr: stderr_capture.finish(),
            });
        }

        thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

fn empty_timed_out_output() -> VersionProbeOutput {
    VersionProbeOutput {
        outcome: VersionProbeOutcome::TimedOut,
        stdout: BoundedCapture::default().finish(),
        stderr: BoundedCapture::default().finish(),
    }
}

fn probe_deadline_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, PROBE_DEADLINE_EXPIRED)
}

fn check_probe_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(probe_deadline_error())
    } else {
        Ok(())
    }
}

fn is_probe_deadline_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::TimedOut && error.to_string() == PROBE_DEADLINE_EXPIRED
}

fn drain_after_root_exit(
    stdout: &mut ChildStdout,
    stderr: &mut ChildStderr,
    stdout_capture: &mut BoundedCapture,
    stderr_capture: &mut BoundedCapture,
) -> io::Result<()> {
    let deadline = Instant::now()
        .checked_add(FINAL_DRAIN_GRACE)
        .expect("short final drain grace cannot overflow Instant");

    loop {
        let stdout_read = drain_probe_pipe(stdout, stdout_capture, deadline)?;
        let stderr_read = drain_probe_pipe(stderr, stderr_capture, deadline)?;
        if stdout_read == 0 && stderr_read == 0 || Instant::now() >= deadline {
            return Ok(());
        }
    }
}

fn drain_probe_pipe<P>(
    pipe: &mut P,
    capture: &mut BoundedCapture,
    deadline: Instant,
) -> io::Result<usize>
where
    P: Read + ProbePipeHandle,
{
    let mut total = 0;
    let mut buffer = [0_u8; DRAIN_BUFFER_BYTES];

    while total < DRAIN_BYTES_PER_STREAM_PER_PASS && Instant::now() < deadline {
        let read = read_available(pipe, &mut buffer)?;
        if read == 0 {
            break;
        }
        capture.retain(&buffer[..read]);
        total += read;
    }

    Ok(total)
}

#[cfg(unix)]
trait ProbePipeHandle: std::os::fd::AsRawFd {}

#[cfg(unix)]
impl<T: std::os::fd::AsRawFd> ProbePipeHandle for T {}

#[cfg(windows)]
trait ProbePipeHandle: std::os::windows::io::AsRawHandle {}

#[cfg(windows)]
impl<T: std::os::windows::io::AsRawHandle> ProbePipeHandle for T {}

#[cfg(unix)]
fn prepare_probe_pipe(pipe: &impl ProbePipeHandle) -> io::Result<()> {
    let descriptor = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn prepare_probe_pipe(_pipe: &impl ProbePipeHandle) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn read_available<P: Read + ProbePipeHandle>(pipe: &mut P, buffer: &mut [u8]) -> io::Result<usize> {
    match pipe.read(buffer) {
        Ok(read) => Ok(read),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(0),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(0),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn read_available<P: Read + ProbePipeHandle>(pipe: &mut P, buffer: &mut [u8]) -> io::Result<usize> {
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let mut available = 0_u32;
    let peeked = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle().cast(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &raw mut available,
            std::ptr::null_mut(),
        )
    };
    if peeked == 0 {
        let error = io::Error::last_os_error();
        return if matches!(
            error.raw_os_error(),
            Some(code) if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32
        ) {
            Ok(0)
        } else {
            Err(error)
        };
    }
    if available == 0 {
        return Ok(0);
    }

    let readable = buffer.len().min(available as usize);
    match pipe.read(&mut buffer[..readable]) {
        Ok(read) => Ok(read),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(0),
        Err(error) => Err(error),
    }
}

struct ProbeChild {
    child: Option<Child>,
    process_tree: Option<ProbeProcessTree>,
    reaped: bool,
    reaper: RootReaper,
}

impl ProbeChild {
    fn new(child: Child, process_tree: ProbeProcessTree, reaper: RootReaper) -> Self {
        Self {
            child: Some(child),
            process_tree: Some(process_tree),
            reaped: false,
            reaper,
        }
    }

    fn new_unattached(child: Child, reaper: RootReaper) -> Self {
        Self {
            child: Some(child),
            process_tree: None,
            reaped: false,
            reaper,
        }
    }

    fn child_mut(&mut self) -> io::Result<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| io::Error::other("probe child is unavailable"))
    }

    fn take_stdout(&mut self) -> io::Result<ChildStdout> {
        self.child_mut()?
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("probe stdout was not piped"))
    }

    fn take_stderr(&mut self) -> io::Result<ChildStderr> {
        self.child_mut()?
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("probe stderr was not piped"))
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child_mut()?.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    fn terminate_process_tree(&self) {
        if let Some(process_tree) = &self.process_tree {
            let _ = process_tree.terminate();
        }
    }

    fn terminate_and_reap(&mut self, grace: Duration) -> io::Result<()> {
        self.terminate_process_tree();
        let kill_error = self.child_mut()?.kill().err();

        match self.child_mut()?.wait_timeout(grace) {
            Ok(Some(_)) => {
                self.reaped = true;
                Ok(())
            }
            Ok(None) => {
                let timeout_error = with_cleanup_error(
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "probe root did not exit within {:.3}s after termination",
                            grace.as_secs_f64()
                        ),
                    ),
                    kill_error,
                );
                let reaper_error = self.reap_in_background().err();
                Err(with_cleanup_error(timeout_error, reaper_error))
            }
            Err(wait_error) => {
                let wait_error = with_cleanup_error(wait_error, kill_error);
                let reaper_error = self.reap_in_background().err();
                Err(with_cleanup_error(wait_error, reaper_error))
            }
        }
    }

    fn reap_in_background(&mut self) -> io::Result<()> {
        let Some(child) = self.child.take() else {
            return Ok(());
        };

        match self.reaper.submit(child) {
            Ok(()) => Ok(()),
            Err((child, error)) => {
                self.child = Some(child);
                Err(error)
            }
        }
    }
}

impl Drop for ProbeChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }

        self.terminate_process_tree();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            if matches!(child.try_wait(), Ok(Some(_))) {
                self.reaped = true;
                return;
            }
        }
        let _ = self.reap_in_background();
    }
}

struct RootReaper {
    sender: SyncSender<Child>,
}

impl RootReaper {
    fn start() -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Child>(1);
        thread::Builder::new()
            .name("atc-doctor-root-reaper".to_owned())
            .spawn(move || {
                if let Ok(mut child) = receiver.recv() {
                    let _ = child.wait();
                }
            })?;

        Ok(Self { sender })
    }

    fn submit(&self, child: Child) -> Result<(), (Child, io::Error)> {
        self.sender.try_send(child).map_err(|error| match error {
            mpsc::TrySendError::Full(child) => (
                child,
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "doctor probe root reaper already owns a child",
                ),
            ),
            mpsc::TrySendError::Disconnected(child) => (
                child,
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "doctor probe root reaper stopped before accepting the child",
                ),
            ),
        })
    }
}

fn with_cleanup_error(original: io::Error, cleanup: Option<io::Error>) -> io::Error {
    match cleanup {
        Some(cleanup) => io::Error::new(
            original.kind(),
            format!("{original}; probe cleanup also failed: {cleanup}"),
        ),
        None => original,
    }
}

#[cfg(unix)]
struct PreparedProbeProcessTree;

#[cfg(unix)]
impl PreparedProbeProcessTree {
    fn prepare(command: &mut Command, _deadline: Instant) -> io::Result<Self> {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
        Ok(Self)
    }

    fn attach(self, child: &Child, _deadline: Instant) -> io::Result<ProbeProcessTree> {
        Ok(ProbeProcessTree {
            process_group: child.id() as libc::pid_t,
        })
    }
}

#[cfg(unix)]
struct ProbeProcessTree {
    process_group: libc::pid_t,
}

#[cfg(unix)]
impl ProbeProcessTree {
    fn terminate(&self) -> io::Result<()> {
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
struct PreparedProbeProcessTree {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl PreparedProbeProcessTree {
    fn prepare(command: &mut Command, deadline: Instant) -> io::Result<Self> {
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        check_probe_deadline(deadline)?;
        command.creation_flags(CREATE_SUSPENDED);

        let raw_job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        let create_error = raw_job.is_null().then(io::Error::last_os_error);
        if raw_job.is_null() {
            check_probe_deadline(deadline)?;
            return Err(create_error.expect("null Job handle must have an OS error"));
        }

        let job = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw_job.cast()) };
        check_probe_deadline(deadline)?;
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
        let configure_error = (configured == 0).then(io::Error::last_os_error);
        check_probe_deadline(deadline)?;
        if configured == 0 {
            return Err(configure_error.expect("failed Job configuration must have an OS error"));
        }

        Ok(Self { job })
    }

    fn attach(self, child: &Child, deadline: Instant) -> io::Result<ProbeProcessTree> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        check_probe_deadline(deadline)?;
        let assigned = unsafe {
            AssignProcessToJobObject(
                self.job.as_raw_handle().cast(),
                child.as_raw_handle().cast(),
            )
        };
        let assignment_error = (assigned == 0).then(io::Error::last_os_error);
        check_probe_deadline(deadline)?;
        if assigned == 0 {
            return Err(assignment_error.expect("failed Job assignment must have an OS error"));
        }

        resume_suspended_probe(child.id(), deadline)?;
        Ok(ProbeProcessTree { job: self.job })
    }
}

#[cfg(windows)]
fn resume_suspended_probe(process_id: u32, deadline: Instant) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    check_probe_deadline(deadline)?;
    let raw_snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    let snapshot_error = (raw_snapshot == INVALID_HANDLE_VALUE).then(io::Error::last_os_error);
    if raw_snapshot == INVALID_HANDLE_VALUE {
        check_probe_deadline(deadline)?;
        return Err(snapshot_error.expect("invalid thread snapshot must have an OS error"));
    }
    let snapshot =
        unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw_snapshot.cast()) };
    check_probe_deadline(deadline)?;

    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    let first_entry = unsafe { Thread32First(snapshot.as_raw_handle().cast(), &raw mut entry) };
    let first_error = (first_entry == 0).then(io::Error::last_os_error);
    check_probe_deadline(deadline)?;
    if first_entry == 0 {
        return Err(first_error.expect("failed first thread entry must have an OS error"));
    }

    let thread_id = find_probe_thread_entry(
        process_id,
        entry,
        || check_probe_deadline(deadline),
        |entry| Ok(unsafe { Thread32Next(snapshot.as_raw_handle().cast(), entry) } != 0),
    )?;

    check_probe_deadline(deadline)?;
    let raw_thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    let open_error = raw_thread.is_null().then(io::Error::last_os_error);
    if raw_thread.is_null() {
        check_probe_deadline(deadline)?;
        return Err(open_error.expect("null thread handle must have an OS error"));
    }
    let thread = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw_thread.cast()) };
    check_probe_deadline(deadline)?;
    let previous_suspend_count = unsafe { ResumeThread(thread.as_raw_handle().cast()) };
    let resume_error = (previous_suspend_count == u32::MAX).then(io::Error::last_os_error);
    check_probe_deadline(deadline)?;
    if previous_suspend_count == u32::MAX {
        return Err(resume_error.expect("failed thread resume must have an OS error"));
    }
    Ok(())
}

#[cfg(windows)]
fn find_probe_thread_entry(
    process_id: u32,
    mut entry: windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32,
    mut check_deadline: impl FnMut() -> io::Result<()>,
    mut advance: impl FnMut(
        &mut windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32,
    ) -> io::Result<bool>,
) -> io::Result<u32> {
    loop {
        check_deadline()?;
        if entry.th32OwnerProcessID == process_id {
            return Ok(entry.th32ThreadID);
        }

        let advanced = advance(&mut entry)?;
        check_deadline()?;
        if !advanced {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("suspended probe process {process_id} has no resumable thread"),
            ));
        }
    }
}

#[cfg(windows)]
struct ProbeProcessTree {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl ProbeProcessTree {
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::{
        BoundedCapture, CAPTURE_BYTES_PER_STREAM, RootReaper, VersionProbeOutcome, probe_version,
        probe_version_with_spawn_observer,
    };

    const TEST_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn successful_version_probe_captures_output() {
        let root = TempDir::new().expect("temp dir");
        let script = write_version_script(
            root.path(),
            "successful-version",
            "echo example-runner 1.2.3",
            "printf 'example-runner 1.2.3\\n'",
        );

        let output = probe_version(&script, root.path(), TEST_PROBE_TIMEOUT).expect("probe");

        assert!(matches!(output.outcome, VersionProbeOutcome::Exited(status) if status.success()));
        assert!(output.stdout.text.contains("example-runner 1.2.3"));
        assert!(output.stderr.text.is_empty());
        assert!(!output.stdout.truncated);
        assert!(!output.stderr.truncated);
    }

    #[test]
    fn nonzero_version_probe_preserves_exit_status_and_stderr() {
        let root = TempDir::new().expect("temp dir");
        let script = write_version_script(
            root.path(),
            "nonzero-version",
            "echo version failed 1>&2\r\nexit /b 7",
            "printf 'version failed\\n' >&2\nexit 7",
        );

        let output = probe_version(&script, root.path(), TEST_PROBE_TIMEOUT).expect("probe");

        assert!(
            matches!(output.outcome, VersionProbeOutcome::Exited(status) if status.code() == Some(7))
        );
        assert!(output.stderr.text.contains("version failed"));
    }

    #[test]
    fn empty_version_output_is_preserved() {
        let root = TempDir::new().expect("temp dir");
        let script = write_version_script(root.path(), "empty-version", "exit /b 0", "exit 0");

        let output = probe_version(&script, root.path(), TEST_PROBE_TIMEOUT).expect("probe");

        assert!(matches!(output.outcome, VersionProbeOutcome::Exited(status) if status.success()));
        assert_eq!(output.stdout.text, "");
        assert_eq!(output.stderr.text, "");
        assert_eq!(output.stdout.retained_bytes, 0);
        assert_eq!(output.stderr.retained_bytes, 0);
    }

    #[test]
    fn multiline_version_output_is_available_to_the_sanitizer() {
        let root = TempDir::new().expect("temp dir");
        let script = write_version_script(
            root.path(),
            "multiline-version",
            "echo first line\r\necho second line",
            "printf 'first line\\nsecond line\\n'",
        );

        let output = probe_version(&script, root.path(), TEST_PROBE_TIMEOUT).expect("probe");

        assert!(output.stdout.text.contains("first line"));
        assert!(output.stdout.text.contains("second line"));
        assert_eq!(
            super::super::first_useful_line_in_capture(&output.stdout).as_deref(),
            Some("first line")
        );
    }

    #[test]
    fn control_characters_are_escaped_in_version_diagnostic() {
        let root = TempDir::new().expect("temp dir");
        let script = write_version_script(
            root.path(),
            "control-version",
            "<nul set /p \"=\u{1b}[31mred\u{1b}[0m\u{7}\"",
            "printf '\\033[31mred\\033[0m\\007\\n'",
        );

        let output = probe_version(&script, root.path(), TEST_PROBE_TIMEOUT).expect("probe");
        let diagnostic =
            super::super::first_useful_line_in_capture(&output.stdout).expect("version diagnostic");

        assert!(diagnostic.contains("red"));
        assert!(diagnostic.contains("\\u{1b}"));
        assert!(!diagnostic.chars().any(char::is_control));
    }

    #[test]
    fn very_long_unbroken_output_is_drained_but_capture_and_diagnostic_are_bounded() {
        let root = TempDir::new().expect("temp dir");
        let chunk = "x".repeat(6_000);
        let windows = format!("for /L %%i in (1,1,200) do @<nul set /p \"={chunk}\"\r\nexit /b 0");
        let unix = format!(
            "i=0\nwhile [ \"$i\" -lt 200 ]; do\n  printf '%s' '{chunk}'\n  i=$((i + 1))\ndone"
        );
        let script = write_version_script(root.path(), "long-version", &windows, &unix);

        let output = probe_version(&script, root.path(), TEST_PROBE_TIMEOUT).expect("probe");
        let version =
            super::super::first_useful_line_in_capture(&output.stdout).expect("version diagnostic");
        let diagnostic = super::super::bounded_runner_text(&format!("C++     {version}"));

        assert!(matches!(output.outcome, VersionProbeOutcome::Exited(status) if status.success()));
        assert_eq!(output.stdout.retained_bytes, CAPTURE_BYTES_PER_STREAM);
        assert!(output.stdout.truncated);
        assert_eq!(diagnostic.chars().count(), 256);
        assert!(diagnostic.ends_with('\u{2026}'));
    }

    #[test]
    fn very_large_stderr_is_drained_while_raw_capture_remains_bounded() {
        let root = TempDir::new().expect("temp dir");
        let chunk = "e".repeat(6_000);
        let windows =
            format!("for /L %%i in (1,1,200) do @<nul set /p \"={chunk}\" 1>&2\r\nexit /b 0");
        let unix = format!(
            "i=0\nwhile [ \"$i\" -lt 200 ]; do\n  printf '%s' '{chunk}' >&2\n  i=$((i + 1))\ndone"
        );
        let script = write_version_script(root.path(), "long-stderr-version", &windows, &unix);

        let output = probe_version(&script, root.path(), TEST_PROBE_TIMEOUT).expect("probe");

        assert!(matches!(output.outcome, VersionProbeOutcome::Exited(status) if status.success()));
        assert_eq!(output.stderr.retained_bytes, CAPTURE_BYTES_PER_STREAM);
        assert!(output.stderr.truncated);
        assert_eq!(output.stdout.retained_bytes, 0);
        assert!(!output.stdout.truncated);
    }

    #[test]
    fn simultaneous_large_stdout_and_stderr_are_both_drained_and_bounded() {
        let root = TempDir::new().expect("temp dir");
        let chunk = "b".repeat(2_000);
        let windows = format!(
            "for /L %%i in (1,1,20) do @(<nul set /p \"={chunk}\" & <nul set /p \"={chunk}\" 1>&2)\r\nexit /b 0"
        );
        let unix = format!(
            "i=0\nwhile [ \"$i\" -lt 20 ]; do\n  printf '%s' '{chunk}'\n  printf '%s' '{chunk}' >&2\n  i=$((i + 1))\ndone"
        );
        let script = write_version_script(root.path(), "dual-stream-version", &windows, &unix);

        let output = probe_version(&script, root.path(), TEST_PROBE_TIMEOUT).expect("probe");

        assert!(matches!(output.outcome, VersionProbeOutcome::Exited(status) if status.success()));
        assert_eq!(output.stdout.retained_bytes, CAPTURE_BYTES_PER_STREAM);
        assert_eq!(output.stderr.retained_bytes, CAPTURE_BYTES_PER_STREAM);
        assert!(output.stdout.truncated);
        assert!(output.stderr.truncated);
    }

    #[test]
    fn utf8_crossing_the_raw_boundary_is_lossy_but_safe_and_marked_truncated() {
        let mut capture = BoundedCapture::default();
        capture.retain(&vec![b'x'; CAPTURE_BYTES_PER_STREAM - 1]);
        capture.retain("é".as_bytes());

        let capture = capture.finish();
        let selected = super::super::first_useful_line_in_capture(&capture).unwrap();

        assert_eq!(capture.retained_bytes, CAPTURE_BYTES_PER_STREAM);
        assert!(capture.truncated);
        assert!(capture.text.ends_with('\u{fffd}'));
        assert!(selected.ends_with('…'));
    }

    #[test]
    fn invalid_utf8_near_the_raw_boundary_remains_memory_bounded_and_safe() {
        let mut capture = BoundedCapture::default();
        capture.retain(&vec![b'x'; CAPTURE_BYTES_PER_STREAM - 2]);
        capture.retain(&[0xff, 0xfe, 0xfd]);

        let capture = capture.finish();
        let selected = super::super::first_useful_line_in_capture(&capture).unwrap();

        assert_eq!(capture.retained_bytes, CAPTURE_BYTES_PER_STREAM);
        assert!(capture.truncated);
        assert!(capture.text.contains('\u{fffd}'));
        assert!(selected.ends_with('…'));
        assert!(!selected.chars().any(char::is_control));
    }

    #[test]
    fn clipped_capture_marks_a_short_retained_line_as_truncated() {
        let mut capture = BoundedCapture::default();
        capture.retain(&vec![b'\n'; CAPTURE_BYTES_PER_STREAM - 6]);
        capture.retain(b"version-text-that-continues");

        let capture = capture.finish();
        let selected = super::super::first_useful_line_in_capture(&capture).unwrap();

        assert_eq!(capture.retained_bytes, CAPTURE_BYTES_PER_STREAM);
        assert!(capture.truncated);
        assert_eq!(selected, "versio…");
    }

    #[test]
    fn failed_reaper_transfer_returns_child_ownership_to_the_caller() {
        let root = TempDir::new().expect("temp dir");
        let script = write_version_script(
            root.path(),
            "reaper-transfer-version",
            "ping.exe 127.0.0.1 -n 6 >nul",
            "sleep 5",
        );
        let child = Command::new(&script)
            .arg("--version")
            .spawn()
            .expect("spawn reaper transfer child");
        let process_id = child.id();
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let reaper = RootReaper { sender };

        let (mut child, error) = reaper.submit(child).unwrap_err();

        assert_eq!(child.id(), process_id);
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        child.kill().expect("terminate returned child");
        child.wait().expect("reap returned child");
    }

    #[test]
    fn timeout_returns_within_the_bounded_probe_lifecycle() {
        let root = TempDir::new().expect("temp dir");
        let script = write_version_script(
            root.path(),
            "timeout-version",
            "ping.exe 127.0.0.1 -n 6 >nul",
            "sleep 5",
        );
        let started = Instant::now();

        let mut root_process_id = None;
        let output = probe_version_with_spawn_observer(
            &script,
            root.path(),
            Duration::from_millis(100),
            |process_id| root_process_id = Some(process_id),
        )
        .expect("probe");

        assert!(matches!(output.outcome, VersionProbeOutcome::TimedOut));
        assert_root_reaped(root_process_id.expect("spawned root process"));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "probe took {:?}",
            started.elapsed()
        );
    }

    #[cfg(windows)]
    #[test]
    fn thread_snapshot_scan_checks_the_deadline_after_each_advance() {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32;

        let entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            th32OwnerProcessID: 1,
            th32ThreadID: 10,
            ..THREADENTRY32::default()
        };
        let mut deadline_checks = 0;
        let mut advance_calls = 0;

        let error = super::find_probe_thread_entry(
            99,
            entry,
            || {
                deadline_checks += 1;
                if deadline_checks == 2 {
                    Err(super::probe_deadline_error())
                } else {
                    Ok(())
                }
            },
            |entry| {
                advance_calls += 1;
                entry.th32OwnerProcessID = 2;
                entry.th32ThreadID = 11;
                Ok(true)
            },
        )
        .unwrap_err();

        assert!(super::is_probe_deadline_error(&error));
        assert_eq!(advance_calls, 1);
        assert_eq!(deadline_checks, 2);
    }

    #[cfg(windows)]
    #[test]
    fn expired_attachment_deadline_keeps_the_suspended_root_from_running_and_reaps_it() {
        let root = TempDir::new().expect("temp dir");
        let marker = root.path().join("attachment-resumed");
        let script = write_version_script(
            root.path(),
            "attachment-deadline-version",
            "echo resumed>attachment-resumed\r\nping.exe 127.0.0.1 -n 6 >nul",
            "",
        );
        let mut root_process_id = None;
        let started = Instant::now();

        let output = probe_version_with_spawn_observer(
            &script,
            root.path(),
            Duration::from_millis(500),
            |process_id| {
                root_process_id = Some(process_id);
                std::thread::sleep(Duration::from_millis(600));
            },
        )
        .expect("probe");

        assert!(matches!(output.outcome, VersionProbeOutcome::TimedOut));
        assert!(!marker.exists(), "suspended probe root was resumed");
        assert_root_reaped(root_process_id.expect("spawned root process"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    fn assert_root_reaped(process_id: u32) {
        let mut status = 0;
        let result =
            unsafe { libc::waitpid(process_id as libc::pid_t, &mut status, libc::WNOHANG) };
        assert_eq!(result, -1, "probe root {process_id} remained waitable");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD),
            "probe root {process_id} was not reaped by the probe"
        );
    }

    #[cfg(windows)]
    fn assert_root_reaped(process_id: u32) {
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
        use windows_sys::Win32::Foundation::STILL_ACTIVE;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let raw_process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
        if raw_process.is_null() {
            return;
        }

        let process =
            unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw_process.cast()) };
        let mut exit_code = 0;
        assert_ne!(
            unsafe { GetExitCodeProcess(process.as_raw_handle().cast(), &raw mut exit_code) },
            0,
            "could not inspect probe root {process_id}: {}",
            std::io::Error::last_os_error()
        );
        assert_ne!(
            exit_code, STILL_ACTIVE as u32,
            "probe root {process_id} was still running after timeout cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_exit_does_not_wait_for_a_detached_pipe_holder() {
        let root = TempDir::new().expect("temp dir");
        let test_executable = std::env::current_exe().expect("current test executable");
        let unix = format!(
            "exec {} --exact doctor::probe::tests::spawn_detached_pipe_holder_and_exit_helper --ignored --nocapture",
            shell_quote(&test_executable)
        );
        let script = write_version_script(root.path(), "detached-pipe-version", "", &unix);
        let started = Instant::now();

        let output = probe_version(&script, root.path(), Duration::from_secs(2)).expect("probe");

        assert!(matches!(output.outcome, VersionProbeOutcome::Exited(status) if status.success()));
        assert!(
            started.elapsed() < Duration::from_millis(1_500),
            "probe waited for inherited pipe EOF for {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "launched in a child process by root_exit_does_not_wait_for_a_detached_pipe_holder"]
    fn spawn_detached_pipe_holder_and_exit_helper() {
        let marker = PathBuf::from("detached-probe-ready");
        let child = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "doctor::probe::tests::detached_pipe_holder_helper",
                "--ignored",
                "--nocapture",
            ])
            .spawn()
            .expect("spawn detached pipe holder");

        let deadline = Instant::now() + Duration::from_secs(1);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            marker.exists(),
            "detached helper {} never became ready",
            child.id()
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "launched in a child process by spawn_detached_pipe_holder_and_exit_helper"]
    fn detached_pipe_holder_helper() {
        let session = unsafe { libc::setsid() };
        assert_ne!(
            session,
            -1,
            "setsid failed: {}",
            std::io::Error::last_os_error()
        );
        fs::write("detached-probe-ready", b"ready").expect("write ready marker");
        std::thread::sleep(Duration::from_secs(3));
    }

    fn write_version_script(
        root: &Path,
        name: &str,
        windows_body: &str,
        unix_body: &str,
    ) -> PathBuf {
        #[cfg(windows)]
        {
            let _ = unix_body;
            let path = root.join(format!("{name}.cmd"));
            let script = format!(
                "@echo off\r\nif not \"%~1\"==\"--version\" exit /b 64\r\n{windows_body}\r\n"
            );
            fs::write(&path, script).expect("write command script");
            path
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let _ = windows_body;
            let path = root.join(name);
            let script = format!(
                "#!/bin/sh\nif [ \"$1\" != \"--version\" ]; then exit 64; fi\n{unix_body}\n"
            );
            fs::write(&path, script).expect("write shell script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("make shell script executable");
            path
        }
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }
}
