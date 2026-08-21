use std::io::{self, Write};
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
    ButtonEventMouse,
    CursorHidden,
}

#[cfg(test)]
const INITIALIZATION_STEPS: [LifecycleStep; 5] = [
    LifecycleStep::RawMode,
    LifecycleStep::AlternateScreen,
    LifecycleStep::SgrMouse,
    LifecycleStep::ButtonEventMouse,
    LifecycleStep::CursorHidden,
];

const CLEANUP_STEPS: [LifecycleStep; 5] = [
    LifecycleStep::ButtonEventMouse,
    LifecycleStep::SgrMouse,
    LifecycleStep::CursorHidden,
    LifecycleStep::AlternateScreen,
    LifecycleStep::RawMode,
];

#[derive(Debug, Default)]
struct LifecycleState {
    raw_mode: bool,
    alternate_screen: bool,
    sgr_mouse: bool,
    button_event_mouse: bool,
    cursor_hidden: bool,
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
            LifecycleStep::ButtonEventMouse => self.button_event_mouse,
            LifecycleStep::CursorHidden => self.cursor_hidden,
        }
    }

    fn flag_mut(&mut self, step: LifecycleStep) -> &mut bool {
        match step {
            LifecycleStep::RawMode => &mut self.raw_mode,
            LifecycleStep::AlternateScreen => &mut self.alternate_screen,
            LifecycleStep::SgrMouse => &mut self.sgr_mouse,
            LifecycleStep::ButtonEventMouse => &mut self.button_event_mouse,
            LifecycleStep::CursorHidden => &mut self.cursor_hidden,
        }
    }
}

fn cleanup_lifecycle(
    state: &mut LifecycleState,
    mut cleanup_step: impl FnMut(LifecycleStep) -> io::Result<()>,
) -> io::Result<()> {
    let mut first_error = None;

    for step in CLEANUP_STEPS {
        if !state.is_active(step) {
            continue;
        }

        match cleanup_step(step) {
            Ok(()) => state.deactivate(step),
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

fn restore_step(output: &mut PlatformTerminal, step: LifecycleStep) -> io::Result<()> {
    match step {
        LifecycleStep::ButtonEventMouse => {
            write_dec_private_mode(output, DecPrivateModeCode::ButtonEventMouse, false)
        }
        LifecycleStep::SgrMouse => {
            write_dec_private_mode(output, DecPrivateModeCode::SGRMouse, false)
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

    fn ensure_mode_disabled(&mut self, code: DecPrivateModeCode) -> io::Result<()> {
        write_dec_private_mode(&mut self.output, code, false)
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
}

impl ScopedPanicHook {
    fn install() -> Self {
        let previous = Arc::new(Mutex::new(Some(panic::take_hook())));
        let active = Arc::new(AtomicBool::new(true));
        let hook_previous = Arc::clone(&previous);
        let hook_active = Arc::clone(&active);
        let owner_thread = std::thread::current().id();
        let cleanup = panic_cleanup_sequence();

        panic::set_hook(Box::new(move |info| {
            if hook_active.load(Ordering::Acquire) && std::thread::current().id() == owner_thread {
                let mut stdout = io::stdout();
                let _ = stdout.write_all(cleanup.as_bytes());
                let _ = stdout.flush();
            }

            let previous = hook_previous
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(previous) = previous.as_ref() {
                previous(info);
            }
        }));

        Self { previous, active }
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

        let installed_hook = panic::take_hook();
        let previous = self
            .previous
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(previous) = previous {
            panic::set_hook(previous);
        } else {
            panic::set_hook(installed_hook);
        }
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
        output.ensure_mode_disabled(DecPrivateModeCode::SGRPixelsMouse)?;
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
        for step in INITIALIZATION_STEPS {
            state.activate(step);
        }
        let mut visited = Vec::new();

        cleanup_lifecycle(&mut state, |step| {
            visited.push(step);
            Ok(())
        })
        .unwrap();

        assert_eq!(visited, CLEANUP_STEPS);
        assert!(CLEANUP_STEPS.into_iter().all(|step| !state.is_active(step)));
    }

    #[test]
    fn every_partial_initialization_prefix_has_matching_rollback() {
        for initialized in 0..=INITIALIZATION_STEPS.len() {
            let mut state = LifecycleState::default();
            for step in INITIALIZATION_STEPS.into_iter().take(initialized) {
                state.activate(step);
            }
            let expected = CLEANUP_STEPS
                .into_iter()
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
        for step in INITIALIZATION_STEPS {
            state.activate(step);
        }
        let mut first_pass = Vec::new();

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
        assert_eq!(first_pass, CLEANUP_STEPS);
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
    fn panic_cleanup_resets_pixel_mode_without_ever_enabling_it() {
        let cleanup = panic_cleanup_sequence();

        assert!(cleanup.contains("\u{1b}[?1016l"));
        assert!(!cleanup.contains("\u{1b}[?1016h"));
    }
}
