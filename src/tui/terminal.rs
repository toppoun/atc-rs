use std::io::{self, IsTerminal, Write};
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::backend::TerminaBackend;
use ratatui::{Frame, Terminal as RatatuiTerminal};
use termina::escape::csi::{Csi, DecPrivateMode, DecPrivateModeCode, Mode};
use termina::{
    Event, EventReader, PlatformHandle, PlatformTerminal, SgrMouseInput,
    Terminal as TerminaTerminal,
};

use super::termina_adapter;

type SessionRatatuiTerminal = RatatuiTerminal<TerminaBackend<SessionTerminal>>;
type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleStep {
    RawMode,
    AlternateScreen,
    SgrMouse,
    SgrPixelsMouseMayBeActive,
    ButtonEventMouse,
    CursorHidden,
    PendingInput,
}

#[cfg(test)]
const INITIALIZATION_STEPS: &[LifecycleStep] = &[
    LifecycleStep::RawMode,
    LifecycleStep::PendingInput,
    LifecycleStep::AlternateScreen,
    LifecycleStep::SgrMouse,
    LifecycleStep::ButtonEventMouse,
    LifecycleStep::CursorHidden,
];

const CLEANUP_STEPS: &[LifecycleStep] = &[
    LifecycleStep::ButtonEventMouse,
    LifecycleStep::SgrMouse,
    LifecycleStep::SgrPixelsMouseMayBeActive,
    LifecycleStep::PendingInput,
    LifecycleStep::CursorHidden,
    LifecycleStep::AlternateScreen,
    LifecycleStep::RawMode,
];

#[derive(Debug, Default)]
struct LifecycleState {
    raw_mode: bool,
    alternate_screen: bool,
    sgr_mouse: bool,
    sgr_pixels_mouse_may_be_active: bool,
    button_event_mouse: bool,
    cursor_hidden: bool,
    pending_input: bool,
}

impl LifecycleState {
    fn activate(&mut self, step: LifecycleStep) {
        *self.flag_mut(step) = true;
    }

    fn deactivate(&mut self, step: LifecycleStep) {
        *self.flag_mut(step) = false;
    }

    fn is_active(&self, step: LifecycleStep) -> bool {
        match step {
            LifecycleStep::RawMode => self.raw_mode,
            LifecycleStep::AlternateScreen => self.alternate_screen,
            LifecycleStep::SgrMouse => self.sgr_mouse,
            LifecycleStep::SgrPixelsMouseMayBeActive => self.sgr_pixels_mouse_may_be_active,
            LifecycleStep::ButtonEventMouse => self.button_event_mouse,
            LifecycleStep::CursorHidden => self.cursor_hidden,
            LifecycleStep::PendingInput => self.pending_input,
        }
    }

    fn flag_mut(&mut self, step: LifecycleStep) -> &mut bool {
        match step {
            LifecycleStep::RawMode => &mut self.raw_mode,
            LifecycleStep::AlternateScreen => &mut self.alternate_screen,
            LifecycleStep::SgrMouse => &mut self.sgr_mouse,
            LifecycleStep::SgrPixelsMouseMayBeActive => &mut self.sgr_pixels_mouse_may_be_active,
            LifecycleStep::ButtonEventMouse => &mut self.button_event_mouse,
            LifecycleStep::CursorHidden => &mut self.cursor_hidden,
            LifecycleStep::PendingInput => &mut self.pending_input,
        }
    }
}

fn normalize_mode_disabled(
    state: &mut LifecycleState,
    step: LifecycleStep,
    normalize: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    // A failed write or flush may still have emitted only part of DECRST. Keep the state active so
    // rollback retries the same reset sequence; this step never has a DECSET inverse.
    state.activate(step);
    let result = normalize();
    if result.is_ok() {
        state.deactivate(step);
    }
    result
}

fn cleanup_lifecycle(
    state: &mut LifecycleState,
    mut cleanup_step: impl FnMut(LifecycleStep) -> io::Result<()>,
) -> io::Result<()> {
    let mut first_error = None;

    for &step in CLEANUP_STEPS {
        if !state.is_active(step) {
            continue;
        }

        match cleanup_step(step) {
            Ok(()) => {
                // A failed 1002 reset means the terminal can emit more reports after this drain.
                // Keep the drain retryable until button-event tracking is confirmed disabled.
                if step != LifecycleStep::PendingInput
                    || !state.is_active(LifecycleStep::ButtonEventMouse)
                {
                    state.deactivate(step);
                }
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    first_error.map_or(Ok(()), Err)
}

fn dec_private_mode(code: DecPrivateModeCode, enabled: bool) -> Csi {
    let mode = DecPrivateMode::Code(code);
    let mode = if enabled {
        Mode::SetDecPrivateMode(mode)
    } else {
        Mode::ResetDecPrivateMode(mode)
    };
    Csi::Mode(mode)
}

fn write_dec_private_mode(
    output: &mut impl Write,
    code: DecPrivateModeCode,
    enabled: bool,
) -> io::Result<()> {
    write!(output, "{}", dec_private_mode(code, enabled))?;
    output.flush()
}

#[cfg(unix)]
fn flush_terminal_input() -> io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::fd::{AsRawFd, RawFd};

    fn flush(fd: RawFd) -> io::Result<()> {
        // SAFETY: `fd` is either the process stdin terminal or a live `/dev/tty` file descriptor,
        // and `TCIFLUSH` only discards unread terminal input.
        if unsafe { libc::tcflush(fd, libc::TCIFLUSH) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    if io::stdin().is_terminal() {
        flush(libc::STDIN_FILENO)
    } else {
        let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        flush(tty.as_raw_fd())
    }
}

#[cfg(windows)]
fn flush_terminal_input() -> io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use windows_sys::Win32::System::Console::FlushConsoleInputBuffer;

    fn flush(handle: RawHandle) -> io::Result<()> {
        if unsafe { FlushConsoleInputBuffer(handle) } != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    let stdin = io::stdin();
    if stdin.is_terminal() {
        flush(stdin.as_raw_handle())
    } else {
        let terminal = OpenOptions::new().read(true).write(true).open("CONIN$")?;
        flush(terminal.as_raw_handle())
    }
}

fn restore_step(output: &mut PlatformTerminal, step: LifecycleStep) -> io::Result<()> {
    match step {
        LifecycleStep::ButtonEventMouse => {
            write_dec_private_mode(output, DecPrivateModeCode::ButtonEventMouse, false)
        }
        LifecycleStep::SgrMouse => {
            write_dec_private_mode(output, DecPrivateModeCode::SGRMouse, false)
        }
        LifecycleStep::SgrPixelsMouseMayBeActive => {
            write_dec_private_mode(output, DecPrivateModeCode::SGRPixelsMouse, false)
        }
        LifecycleStep::CursorHidden => {
            write_dec_private_mode(output, DecPrivateModeCode::ShowCursor, true)
        }
        LifecycleStep::AlternateScreen => write_dec_private_mode(
            output,
            DecPrivateModeCode::ClearAndEnableAlternateScreen,
            false,
        ),
        LifecycleStep::RawMode => output.enter_cooked_mode(),
        LifecycleStep::PendingInput => flush_terminal_input(),
    }
}

/// Termina terminal wrapped with atc-owned application-mode rollback.
///
/// Ratatui takes ownership of its backend. Keeping the rollback state beside the platform terminal
/// ensures that a failure while Ratatui constructs its `Terminal` still restores every DEC mode
/// enabled before that construction.
#[derive(Debug)]
struct SessionTerminal {
    output: PlatformTerminal,
    lifecycle: LifecycleState,
}

impl SessionTerminal {
    fn new(output: PlatformTerminal) -> Self {
        Self {
            output,
            lifecycle: LifecycleState::default(),
        }
    }

    fn enter_raw_mode(&mut self) -> io::Result<()> {
        // Mark before attempting the transition: a failing platform call may still have changed
        // part of the terminal state, so rollback must attempt cooked mode.
        self.lifecycle.activate(LifecycleStep::RawMode);
        self.lifecycle.activate(LifecycleStep::PendingInput);
        self.output.enter_raw_mode()
    }

    fn change_mode(
        &mut self,
        step: LifecycleStep,
        code: DecPrivateModeCode,
        enabled: bool,
    ) -> io::Result<()> {
        // Likewise, a terminal write can partially succeed before returning an error.
        self.lifecycle.activate(step);
        write_dec_private_mode(&mut self.output, code, enabled)
    }

    fn ensure_mode_disabled(
        &mut self,
        step: LifecycleStep,
        code: DecPrivateModeCode,
    ) -> io::Result<()> {
        let (output, lifecycle) = (&mut self.output, &mut self.lifecycle);
        normalize_mode_disabled(lifecycle, step, || {
            write_dec_private_mode(output, code, false)
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        let (output, lifecycle) = (&mut self.output, &mut self.lifecycle);
        cleanup_lifecycle(lifecycle, |step| restore_step(output, step))
    }
}

impl Drop for SessionTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

impl Write for SessionTerminal {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.output.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

impl TerminaTerminal for SessionTerminal {
    fn enter_raw_mode(&mut self) -> io::Result<()> {
        self.output.enter_raw_mode()
    }

    fn enter_cooked_mode(&mut self) -> io::Result<()> {
        self.output.enter_cooked_mode()
    }

    fn get_dimensions(&self) -> io::Result<termina::WindowSize> {
        self.output.get_dimensions()
    }

    fn event_reader(&self) -> EventReader {
        self.output.event_reader()
    }

    fn poll<F: Fn(&Event) -> bool>(
        &self,
        filter: F,
        timeout: Option<Duration>,
    ) -> io::Result<bool> {
        self.output.poll(filter, timeout)
    }

    fn read<F: Fn(&Event) -> bool>(&self, filter: F) -> io::Result<Event> {
        self.output.read(filter)
    }

    fn set_panic_hook(&mut self, cleanup: impl Fn(&mut PlatformHandle) + Send + Sync + 'static) {
        self.output.set_panic_hook(cleanup);
    }
}

struct ScopedPanicHook {
    previous: Arc<Mutex<Option<PanicHook>>>,
    active: Arc<AtomicBool>,
    installed_hook_id: usize,
}

fn panic_hook_id(hook: &PanicHook) -> usize {
    let pointer: *const (dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static) = &**hook;
    pointer as *const () as usize
}

fn hook_after_scope(
    installed_hook_id: usize,
    current_hook: PanicHook,
    previous_hook: Option<PanicHook>,
) -> PanicHook {
    if panic_hook_id(&current_hook) == installed_hook_id {
        previous_hook.unwrap_or(current_hook)
    } else {
        // Another component replaced this scope's hook. Preserve that newer hook instead of
        // clobbering it with the hook that happened to precede this TUI session.
        current_hook
    }
}

fn write_cleanup_to(output: &mut impl Write, cleanup: &[u8]) -> io::Result<()> {
    output.write_all(cleanup)?;
    output.flush()
}

fn write_panic_cleanup(cleanup: &[u8]) -> io::Result<()> {
    if io::stdout().is_terminal() {
        return write_cleanup_to(&mut io::stdout(), cleanup);
    }

    #[cfg(unix)]
    let mut terminal = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    #[cfg(windows)]
    let mut terminal = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONOUT$")?;

    write_cleanup_to(&mut terminal, cleanup)
}

impl ScopedPanicHook {
    fn install() -> Self {
        let previous = Arc::new(Mutex::new(Some(panic::take_hook())));
        let active = Arc::new(AtomicBool::new(true));
        let hook_previous = Arc::clone(&previous);
        let hook_active = Arc::clone(&active);
        let owner_thread = std::thread::current().id();
        let cleanup = panic_cleanup_sequence();

        let hook: PanicHook = Box::new(move |info| {
            if hook_active.load(Ordering::Acquire) && std::thread::current().id() == owner_thread {
                let _ = write_panic_cleanup(cleanup.as_bytes());
            }

            let previous = hook_previous
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(previous) = previous.as_ref() {
                previous(info);
            }
        });
        let installed_hook_id = panic_hook_id(&hook);
        panic::set_hook(hook);

        Self {
            previous,
            active,
            installed_hook_id,
        }
    }
}

impl Drop for ScopedPanicHook {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);

        // Replacing a panic hook while this thread is unwinding would itself panic. In the normal
        // TUI path the hook is restored immediately; during an unhandled panic the process is
        // already exiting, and the hook has just chained the original hook.
        if std::thread::panicking() {
            return;
        }

        let current_hook = panic::take_hook();
        let previous = self
            .previous
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        panic::set_hook(hook_after_scope(
            self.installed_hook_id,
            current_hook,
            previous,
        ));
    }
}

fn panic_cleanup_sequence() -> String {
    [
        dec_private_mode(DecPrivateModeCode::ButtonEventMouse, false),
        dec_private_mode(DecPrivateModeCode::SGRMouse, false),
        dec_private_mode(DecPrivateModeCode::SGRPixelsMouse, false),
        dec_private_mode(DecPrivateModeCode::ShowCursor, true),
        dec_private_mode(DecPrivateModeCode::ClearAndEnableAlternateScreen, false),
    ]
    .into_iter()
    .map(|command| command.to_string())
    .collect()
}

/// Sole owner of rendering, input, and terminal lifecycle for the watch TUI.
pub(crate) struct TerminaSession {
    terminal: SessionRatatuiTerminal,
    reader: EventReader,
    _panic_hook: ScopedPanicHook,
}

impl TerminaSession {
    pub(crate) fn start() -> io::Result<Self> {
        let output = PlatformTerminal::new()?;
        let mut output = SessionTerminal::new(output);
        let panic_hook = ScopedPanicHook::install();

        output.enter_raw_mode()?;

        // Configure the sole shared parser before enabling SGR mouse reporting or reading input.
        let reader = output.event_reader();
        reader.set_sgr_mouse_input(SgrMouseInput::Cells1006)?;

        output.change_mode(
            LifecycleStep::AlternateScreen,
            DecPrivateModeCode::ClearAndEnableAlternateScreen,
            true,
        )?;

        // 1006 and 1016 have the same wire grammar. Explicitly keep 1016 off before selecting the
        // cell-coordinate format, then enable drag/button tracking without unpressed motion.
        output.ensure_mode_disabled(
            LifecycleStep::SgrPixelsMouseMayBeActive,
            DecPrivateModeCode::SGRPixelsMouse,
        )?;
        output.change_mode(LifecycleStep::SgrMouse, DecPrivateModeCode::SGRMouse, true)?;
        output.change_mode(
            LifecycleStep::ButtonEventMouse,
            DecPrivateModeCode::ButtonEventMouse,
            true,
        )?;
        output.change_mode(
            LifecycleStep::CursorHidden,
            DecPrivateModeCode::ShowCursor,
            false,
        )?;

        let backend = TerminaBackend::new(output);
        let terminal = RatatuiTerminal::new(backend)?;

        Ok(Self {
            terminal,
            reader,
            _panic_hook: panic_hook,
        })
    }

    pub(super) fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<()> {
        self.terminal.draw(render).map(|_| ())
    }

    pub(super) fn poll(&self, wait: Duration) -> io::Result<bool> {
        self.reader.poll(Some(wait), |_| true)
    }

    pub(super) fn read(&self) -> io::Result<TerminalEvent> {
        // Termina may discard unsupported byte sequences inside its parser. Do not synthesize
        // events for those bytes; every event Termina does emit occupies exactly one application
        // batch slot, including emitted-but-unused events translated to `Ignored`.
        self.reader.read(|_| true).map(termina_adapter::translate)
    }

    pub(crate) fn restore(&mut self) -> io::Result<()> {
        self.terminal.backend_mut().terminal_mut().restore()
    }
}

impl Drop for TerminaSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalEvent {
    Key(KeyEvent),
    Pointer(PointerEvent),
    Resize(TerminalSize),
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TerminalSize {
    pub(super) columns: u16,
    pub(super) rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct KeyEvent {
    pub(super) code: KeyCode,
    pub(super) kind: KeyEventKind,
    pub(super) modifiers: Modifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyCode {
    Char(char),
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Modifiers {
    pub(super) shift: bool,
    pub(super) control: bool,
    pub(super) alt: bool,
    pub(super) super_key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PointerEvent {
    pub(super) kind: PointerKind,
    pub(super) position: PointerPosition,
    pub(super) modifiers: Modifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerKind {
    Down(PointerButton),
    Up(PointerButton),
    Drag(PointerButton),
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerPosition {
    Cells {
        column: u16,
        row: u16,
    },
    #[allow(dead_code, reason = "reserved for the later pixel-input adapter")]
    AbsolutePixels {
        x: u32,
        y: u32,
    },
}

impl PointerPosition {
    pub(super) fn cells(self) -> Option<(u16, u16)> {
        match self {
            Self::Cells { column, row } => Some((column, row)),
            Self::AbsolutePixels { .. } => None,
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn cleanup_order_is_mouse_cursor_screen_then_platform() {
        let mut state = LifecycleState::default();
        for &step in INITIALIZATION_STEPS {
            state.activate(step);
        }
        let mut visited = Vec::new();
        let expected = CLEANUP_STEPS
            .iter()
            .copied()
            .filter(|step| state.is_active(*step))
            .collect::<Vec<_>>();

        cleanup_lifecycle(&mut state, |step| {
            visited.push(step);
            Ok(())
        })
        .unwrap();

        assert_eq!(visited, expected);
        assert!(
            CLEANUP_STEPS
                .iter()
                .copied()
                .all(|step| !state.is_active(step))
        );
    }

    #[test]
    fn every_partial_initialization_prefix_has_matching_rollback() {
        for initialized in 0..=INITIALIZATION_STEPS.len() {
            let mut state = LifecycleState::default();
            for step in INITIALIZATION_STEPS.iter().copied().take(initialized) {
                state.activate(step);
            }
            let expected = CLEANUP_STEPS
                .iter()
                .copied()
                .filter(|step| state.is_active(*step))
                .collect::<Vec<_>>();
            let mut visited = Vec::new();

            cleanup_lifecycle(&mut state, |step| {
                visited.push(step);
                Ok(())
            })
            .unwrap();

            assert_eq!(visited, expected);
        }
    }

    #[test]
    fn cleanup_continues_after_error_and_retries_only_failed_steps() {
        let mut state = LifecycleState::default();
        for &step in INITIALIZATION_STEPS {
            state.activate(step);
        }
        let mut first_pass = Vec::new();
        let expected = CLEANUP_STEPS
            .iter()
            .copied()
            .filter(|step| state.is_active(*step))
            .collect::<Vec<_>>();

        let error = cleanup_lifecycle(&mut state, |step| {
            first_pass.push(step);
            if step == LifecycleStep::CursorHidden {
                Err(io::Error::other("cursor cleanup failed"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert_eq!(error.to_string(), "cursor cleanup failed");
        assert_eq!(first_pass, expected);
        assert!(state.is_active(LifecycleStep::CursorHidden));

        let mut retry = Vec::new();
        cleanup_lifecycle(&mut state, |step| {
            retry.push(step);
            Ok(())
        })
        .unwrap();

        assert_eq!(retry, [LifecycleStep::CursorHidden]);
    }

    #[test]
    fn input_drain_retries_after_tracking_reset_failure() {
        let mut state = LifecycleState::default();
        state.activate(LifecycleStep::ButtonEventMouse);
        state.activate(LifecycleStep::PendingInput);
        let mut first_pass = Vec::new();

        cleanup_lifecycle(&mut state, |step| {
            first_pass.push(step);
            if step == LifecycleStep::ButtonEventMouse {
                Err(io::Error::other("1002 reset failed"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert_eq!(
            first_pass,
            [LifecycleStep::ButtonEventMouse, LifecycleStep::PendingInput]
        );
        assert!(state.is_active(LifecycleStep::ButtonEventMouse));
        assert!(state.is_active(LifecycleStep::PendingInput));

        let mut retry = Vec::new();
        cleanup_lifecycle(&mut state, |step| {
            retry.push(step);
            Ok(())
        })
        .unwrap();

        assert_eq!(
            retry,
            [LifecycleStep::ButtonEventMouse, LifecycleStep::PendingInput]
        );
    }

    #[test]
    fn failed_defensive_pixel_reset_retries_only_the_same_reset_step() {
        let mut state = LifecycleState::default();

        let error =
            normalize_mode_disabled(&mut state, LifecycleStep::SgrPixelsMouseMayBeActive, || {
                Err(io::Error::other("partial DECRST 1016"))
            })
            .unwrap_err();

        assert_eq!(error.to_string(), "partial DECRST 1016");
        assert!(state.is_active(LifecycleStep::SgrPixelsMouseMayBeActive));

        let mut retry = Vec::new();
        cleanup_lifecycle(&mut state, |step| {
            retry.push(step);
            Ok(())
        })
        .unwrap();

        assert_eq!(retry, [LifecycleStep::SgrPixelsMouseMayBeActive]);
    }

    #[test]
    fn successful_defensive_pixel_reset_needs_no_cleanup_step() {
        let mut state = LifecycleState::default();

        normalize_mode_disabled(&mut state, LifecycleStep::SgrPixelsMouseMayBeActive, || {
            Ok(())
        })
        .unwrap();

        let mut visited = Vec::new();
        cleanup_lifecycle(&mut state, |step| {
            visited.push(step);
            Ok(())
        })
        .unwrap();

        assert!(visited.is_empty());
    }

    #[test]
    fn a_newer_panic_hook_wins_over_the_scoped_hook_and_its_predecessor() {
        let predecessor: PanicHook = Box::new(|_: &PanicHookInfo<'_>| {});
        let installed: PanicHook = Box::new(|_: &PanicHookInfo<'_>| {});
        let installed_id = panic_hook_id(&installed);
        let newer: PanicHook = Box::new(|_: &PanicHookInfo<'_>| {});
        let newer_id = panic_hook_id(&newer);

        let selected = hook_after_scope(installed_id, newer, Some(predecessor));

        assert_eq!(panic_hook_id(&selected), newer_id);
    }

    #[test]
    fn scoped_panic_hook_restores_its_predecessor_when_still_current() {
        let predecessor: PanicHook = Box::new(|_: &PanicHookInfo<'_>| {});
        let predecessor_id = panic_hook_id(&predecessor);
        let installed: PanicHook = Box::new(|_: &PanicHookInfo<'_>| {});
        let installed_id = panic_hook_id(&installed);

        let selected = hook_after_scope(installed_id, installed, Some(predecessor));

        assert_eq!(panic_hook_id(&selected), predecessor_id);
    }

    #[test]
    fn panic_cleanup_uses_write_all_before_flush() {
        #[derive(Default)]
        struct OneByteWriter {
            bytes: Vec<u8>,
            flushes: usize,
        }

        impl Write for OneByteWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let Some(&byte) = bytes.first() else {
                    return Ok(0);
                };
                self.bytes.push(byte);
                Ok(1)
            }

            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }

        let mut output = OneByteWriter::default();
        write_cleanup_to(&mut output, b"cleanup").unwrap();

        assert_eq!(output.bytes, b"cleanup");
        assert_eq!(output.flushes, 1);
    }

    #[test]
    fn panic_cleanup_resets_pixel_mode_without_ever_enabling_it() {
        let cleanup = panic_cleanup_sequence();

        assert!(cleanup.contains("\u{1b}[?1016l"));
        assert!(!cleanup.contains("\u{1b}[?1016h"));
    }
}
