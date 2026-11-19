use super::*;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read};
use std::net::TcpListener;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, COORD, CTRL_C_EVENT, GenerateConsoleCtrlEvent,
    GetConsoleProcessList, GetConsoleScreenBufferInfo, ReadConsoleOutputCharacterW,
    SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

const LIMIT: Duration = Duration::from_secs(15);
static CONSOLE_TEST: Mutex<()> = Mutex::new(());

fn wait(child: &mut Child, limit: Duration) -> ExitStatus {
    if let Some(status) = child.wait_timeout(limit).unwrap() {
        status
    } else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("child did not exit within {limit:?}");
    }
}

fn isolated_console(name: &str) -> bool {
    const CHILD: &str = "ATC_BATCH_CLI_CONSOLE_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        unsafe extern "system" fn protect_driver(event: u32) -> windows_sys::core::BOOL {
            (event == CTRL_C_EVENT).into()
        }
        // SAFETY: the driver owns a private console. The handler protects only this process.
        assert_ne!(unsafe { SetConsoleCtrlHandler(Some(protect_driver), 1) }, 0);
        return true;
    }
    // Do not overlap console creation and control delivery across the test cases.
    let _console_test = CONSOLE_TEST
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut driver = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .creation_flags(CREATE_NEW_CONSOLE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait(&mut driver, Duration::from_secs(60));
    let output = driver.wait_with_output().unwrap();
    assert!(
        status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    false
}

fn interrupt(child: &Child) {
    let mut attached = [0u32; 4];
    // SAFETY: the fixed-size buffer is valid for the number of process IDs passed.
    let count = unsafe { GetConsoleProcessList(attached.as_mut_ptr(), attached.len() as u32) };
    assert_eq!(count, 2, "unexpected console membership: {attached:?}");
    assert!(attached.contains(&child.id()) && attached.contains(&std::process::id()));
    // SAFETY: group zero broadcasts only within the driver's private console.
    assert_ne!(
        unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) },
        0,
        "{}",
        io::Error::last_os_error()
    );
}

fn wait_until<T>(mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(Instant::now() < deadline, "synchronization point timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

fn command(root: &Path, fixture: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atc"));
    command
        .current_dir(root)
        .env("ATC_FIXTURE_DIR", fixture)
        .args(args);
    command
}

#[test]
fn interrupted_plan_with_a_full_pipe_exits_without_draining_it() {
    if !isolated_console(
        "windows_interrupt::interrupted_plan_with_a_full_pipe_exits_without_draining_it",
    ) {
        return;
    }
    let root = workspace("version = 1\n");
    for n in 100..356 {
        legacy(&root.path().join(format!("abc{n}")));
    }
    let before = tree(root.path());
    for disconnect in [false, true] {
        let mut child = command(root.path(), root.path(), &["migrate", "--all", "--yes"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stderr.take().unwrap());
        let (lines, receive) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut prefix = String::new();
            while !prefix.ends_with("Plan:\n") {
                if output.read_line(&mut prefix).unwrap() == 0 {
                    break;
                }
            }
            let _ = lines.send((prefix, output));
        });
        let (prefix, output) = receive.recv_timeout(LIMIT).unwrap();
        assert!(prefix.ends_with("Plan:\n"), "{prefix}");
        wait_until(|| {
            let mut available = 0;
            // SAFETY: the reader and its pipe handle stay open during this probe.
            let success = unsafe {
                PeekNamedPipe(
                    output.get_ref().as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(success, 0, "{}", io::Error::last_os_error());
            (available >= 4096).then_some(())
        });
        assert!(child.try_wait().unwrap().is_none());
        if disconnect {
            drop(output);
            assert_eq!(wait(&mut child, Duration::from_secs(5)).code(), Some(1));
        } else {
            interrupt(&child);
            assert_eq!(wait(&mut child, Duration::from_secs(5)).code(), Some(130));
            // The unread pipe is still open. The command did not wait to print the summary.
            drop(output);
        }
        assert_eq!(tree(root.path()), before);
    }
}

#[test]
fn interrupted_at_the_console_confirmation_prompt() {
    if !isolated_console("windows_interrupt::interrupted_at_the_console_confirmation_prompt") {
        return;
    }
    let root = workspace("version = 1\n");
    legacy(&root.path().join("abc466"));
    let before = tree(root.path());
    let console_input = OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    let console_output = OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONOUT$")
        .unwrap();
    let mut child = command(root.path(), root.path(), &["migrate", "--all"])
        .stdin(Stdio::from(console_input))
        .stderr(Stdio::from(console_output.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let screen_contains = |expected: &str| {
        let handle = console_output.as_raw_handle();
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        let mut read = 0;
        // SAFETY: console_output is the open screen buffer handle, and both output buffers are writable.
        unsafe {
            assert_ne!(GetConsoleScreenBufferInfo(handle, &mut info), 0);
            let start = info.dwCursorPosition.Y.saturating_sub(20).max(0);
            let mut text =
                vec![0u16; info.dwSize.X as usize * (info.dwCursorPosition.Y - start + 1) as usize];
            assert_ne!(
                ReadConsoleOutputCharacterW(
                    handle,
                    text.as_mut_ptr(),
                    text.len() as u32,
                    COORD { X: 0, Y: start },
                    &mut read
                ),
                0
            );
            String::from_utf16_lossy(&text[..read as usize]).contains(expected)
        }
    };
    wait_until(|| screen_contains("Proceed with migration? [y/N]").then_some(()));
    assert!(child.try_wait().unwrap().is_none());
    interrupt(&child);
    assert_eq!(wait(&mut child, Duration::from_secs(5)).code(), Some(130));
    assert!(screen_contains("Batch stopped: interrupted by Ctrl+C."));
    assert_eq!(tree(root.path()), before);
}

#[test]
fn interrupted_during_http_fetch_and_then_reran() {
    if !isolated_console("windows_interrupt::interrupted_during_http_fetch_and_then_reran") {
        return;
    }
    let root = workspace("version = 1\n");
    let ids = ["abc466", "abc467"];
    for id in ids {
        manifest(&root.path().join(id), id, 1);
    }
    let before = tree(root.path());
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let fixture_root = tempfile::tempdir().unwrap();
    fixtures(fixture_root.path(), &ids);
    let args = ["refresh", "--all", "--yes"];
    let mut child = command(root.path(), fixture_root.path(), &args)
        .env_remove("ATC_FIXTURE_DIR")
        .env("APPDATA", fixture_root.path())
        .env("LOCALAPPDATA", fixture_root.path())
        .env_remove("https_proxy")
        .env_remove("no_proxy")
        .env(
            "HTTPS_PROXY",
            format!("http://{}", proxy.local_addr().unwrap()),
        )
        .env("NO_PROXY", "")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stream = wait_until(|| match proxy.accept() {
        Ok((stream, _)) => Some(stream),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
        Err(error) => panic!("local proxy failed: {error}"),
    });
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = String::new();
    BufReader::new(&stream).read_line(&mut request).unwrap();
    assert_eq!(request, "CONNECT atcoder.jp:443 HTTP/1.1\r\n");
    interrupt(&child);
    // Keep the request open until the worker's existing HTTP deadline. This eliminates a race
    // between scheduling the Windows control handler and returning an HTTP error.
    let started = Instant::now();
    let status = wait(&mut child, LIMIT);
    drop(stream);
    let mut message = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut message)
        .unwrap();
    assert_eq!(
        status.code(),
        Some(130),
        "after {:?}: {message}",
        started.elapsed()
    );
    assert!(!message.contains("abc467  REFRESHING"), "{message}");
    summary_count(&message, "Not run:", 2);
    assert_eq!(tree(root.path()), before);
    let rerun = cli(root.path(), &args, fixture_root.path());
    assert!(rerun.status.success(), "{}", stderr(&rerun));
    summary_count(&stderr(&rerun), "Repaired:", 2);
}
