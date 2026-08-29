use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::TerminaBackend;
use ratatui::{Frame, Terminal as RatatuiTerminal};
use termina::escape::csi::{Csi, DecModeSetting, DecPrivateMode, DecPrivateModeCode, Mode, Window};
use termina::{
    Event, EventReader, PlatformHandle, PlatformTerminal, SgrMouseInput,
    Terminal as TerminaTerminal,
};

use super::mouse::{
    HighResRetry, MouseMode, PixelCoordinateOrigin, TerminalPixelMetrics,
    TerminalPixelMetricsError, trusted_pixel_origin,
};
use super::termina_adapter;

type SessionRatatuiTerminal = RatatuiTerminal<TerminaBackend<SessionTerminal>>;
type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

const INITIAL_PIXEL_QUERY_TIMEOUT: Duration = Duration::from_millis(200);
const PIXEL_ENABLE_VERIFY_TIMEOUT: Duration = Duration::from_millis(100);
const RESIZE_METRIC_QUERY_TIMEOUT: Duration = Duration::from_millis(150);

#[derive(Debug)]
pub(super) enum SuspendedRunError<E> {
    Suspend(io::Error),
    SuspendAndResume {
        suspend: io::Error,
        resume: io::Error,
    },
    Operation(E),
    Resume(io::Error),
    OperationAndResume {
        operation: E,
        resume: io::Error,
    },
}

impl<E: std::fmt::Display> std::fmt::Display for SuspendedRunError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Suspend(error) => {
                write!(formatter, "failed to suspend the TUI terminal: {error}")
            }
            Self::SuspendAndResume { suspend, resume } => write!(
                formatter,
                "failed to restore the TUI terminal: {resume}; suspension also failed: {suspend}"
            ),
            Self::Operation(error) => error.fmt(formatter),
            Self::Resume(error) => write!(formatter, "failed to restore the TUI terminal: {error}"),
            Self::OperationAndResume { operation, resume } => write!(
                formatter,
                "failed to restore the TUI terminal: {resume}; editor also failed: {operation}"
            ),
        }
    }
}

fn run_suspended_with<S, T, E>(
    state: &mut S,
    suspend: impl FnOnce(&mut S) -> io::Result<()>,
    operation: impl FnOnce(&mut S) -> Result<T, E>,
    resume: impl FnOnce(&mut S) -> io::Result<()>,
) -> Result<T, SuspendedRunError<E>> {
    if let Err(suspend) = suspend(state) {
        return match resume(state) {
            Ok(()) => Err(SuspendedRunError::Suspend(suspend)),
            Err(resume) => Err(SuspendedRunError::SuspendAndResume { suspend, resume }),
        };
    }

    let operation = operation(state);
    let resume = resume(state);
    match (operation, resume) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(operation), Ok(())) => Err(SuspendedRunError::Operation(operation)),
        (Ok(_), Err(resume)) => Err(SuspendedRunError::Resume(resume)),
        (Err(operation), Err(resume)) => {
            Err(SuspendedRunError::OperationAndResume { operation, resume })
        }
    }
}

fn recreate_after_input_flush<R, T, E>(
    resource: &mut Option<R>,
    flush_input: impl FnOnce() -> Result<(), E>,
    operation: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    flush_input()?;
    drop(resource.take());
    operation()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PixelRefresh {
    #[default]
    None,
    AwaitingRedraw,
    ReadyAfterRedraw,
}

impl PixelRefresh {
    fn schedule_after_resize(&mut self) {
        if *self != Self::None {
            *self = Self::AwaitingRedraw;
        }
    }

    fn schedule_new(&mut self) {
        *self = Self::AwaitingRedraw;
    }

    fn observe_redraw(&mut self) {
        if *self == Self::AwaitingRedraw {
            *self = Self::ReadyAfterRedraw;
        }
    }

    const fn is_ready(self) -> bool {
        matches!(self, Self::ReadyAfterRedraw)
    }

    fn clear(&mut self) {
        *self = Self::None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleStep {
    RawMode,
    AlternateScreen,
    BracketedPaste,
    SgrMouse,
    SgrPixelsMouseMayBeActive,
    AnyEventMouse,
    CursorHidden,
    PendingInput,
}

#[cfg(test)]
const INITIALIZATION_STEPS: &[LifecycleStep] = &[
    LifecycleStep::RawMode,
    LifecycleStep::PendingInput,
    LifecycleStep::AlternateScreen,
    LifecycleStep::BracketedPaste,
    LifecycleStep::SgrMouse,
    LifecycleStep::AnyEventMouse,
    LifecycleStep::CursorHidden,
];

const CLEANUP_STEPS: &[LifecycleStep] = &[
    LifecycleStep::AnyEventMouse,
    LifecycleStep::SgrMouse,
    LifecycleStep::SgrPixelsMouseMayBeActive,
    LifecycleStep::BracketedPaste,
    LifecycleStep::PendingInput,
    LifecycleStep::CursorHidden,
    LifecycleStep::AlternateScreen,
    LifecycleStep::RawMode,
];

#[derive(Debug, Default)]
struct LifecycleState {
    raw_mode: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
    sgr_mouse: bool,
    sgr_pixels_mouse_may_be_active: bool,
    any_event_mouse: bool,
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
            LifecycleStep::BracketedPaste => self.bracketed_paste,
            LifecycleStep::SgrMouse => self.sgr_mouse,
            LifecycleStep::SgrPixelsMouseMayBeActive => self.sgr_pixels_mouse_may_be_active,
            LifecycleStep::AnyEventMouse => self.any_event_mouse,
            LifecycleStep::CursorHidden => self.cursor_hidden,
            LifecycleStep::PendingInput => self.pending_input,
        }
    }

    fn flag_mut(&mut self, step: LifecycleStep) -> &mut bool {
        match step {
            LifecycleStep::RawMode => &mut self.raw_mode,
            LifecycleStep::AlternateScreen => &mut self.alternate_screen,
            LifecycleStep::BracketedPaste => &mut self.bracketed_paste,
            LifecycleStep::SgrMouse => &mut self.sgr_mouse,
            LifecycleStep::SgrPixelsMouseMayBeActive => &mut self.sgr_pixels_mouse_may_be_active,
            LifecycleStep::AnyEventMouse => &mut self.any_event_mouse,
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
                // A failed 1003 reset means the terminal can emit more reports after this drain.
                // Keep the drain retryable until any-event tracking is confirmed disabled.
                if step != LifecycleStep::PendingInput
                    || !state.is_active(LifecycleStep::AnyEventMouse)
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
        LifecycleStep::AnyEventMouse => {
            write_dec_private_mode(output, DecPrivateModeCode::AnyEventMouse, false)
        }
        LifecycleStep::SgrMouse => {
            write_dec_private_mode(output, DecPrivateModeCode::SGRMouse, false)
        }
        LifecycleStep::SgrPixelsMouseMayBeActive => {
            write_dec_private_mode(output, DecPrivateModeCode::SGRPixelsMouse, false)
        }
        LifecycleStep::BracketedPaste => {
            write_dec_private_mode(output, DecPrivateModeCode::BracketedPaste, false)
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
        dec_private_mode(DecPrivateModeCode::AnyEventMouse, false),
        dec_private_mode(DecPrivateModeCode::SGRMouse, false),
        dec_private_mode(DecPrivateModeCode::SGRPixelsMouse, false),
        dec_private_mode(DecPrivateModeCode::BracketedPaste, false),
        dec_private_mode(DecPrivateModeCode::ShowCursor, true),
        dec_private_mode(DecPrivateModeCode::ClearAndEnableAlternateScreen, false),
    ]
    .into_iter()
    .map(|command| command.to_string())
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportedPixelMode {
    NotRecognized,
    Set,
    Reset,
    PermanentlySet,
    PermanentlyReset,
}

impl From<DecModeSetting> for ReportedPixelMode {
    fn from(setting: DecModeSetting) -> Self {
        match setting {
            DecModeSetting::NotRecognized => Self::NotRecognized,
            DecModeSetting::Set => Self::Set,
            DecModeSetting::Reset => Self::Reset,
            DecModeSetting::PermanentlySet => Self::PermanentlySet,
            DecModeSetting::PermanentlyReset => Self::PermanentlyReset,
        }
    }
}

impl ReportedPixelMode {
    const fn trace_label(self) -> &'static str {
        match self {
            Self::NotRecognized => "not-recognized",
            Self::Set => "set",
            Self::Reset => "reset",
            Self::PermanentlySet => "permanently-set",
            Self::PermanentlyReset => "permanently-reset",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityReply {
    PixelMode(ReportedPixelMode),
    AreaPixels { width: u32, height: u32 },
    CellPixels { width: i64, height: i64 },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CapabilityReplies {
    pixel_mode: Option<ReportedPixelMode>,
    area_pixels: Option<(u32, u32)>,
    cell_pixels: Option<(i64, i64)>,
    resize_seen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PixelNegotiationStage {
    Initial,
    DeferredRetry,
    InitialPostEnable,
    DeferredRetryPostEnable,
    ResizeRefresh,
}

impl PixelNegotiationStage {
    const fn trace_label(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::DeferredRetry => "deferred-retry",
            Self::InitialPostEnable => "post-enable",
            Self::DeferredRetryPostEnable => "deferred-retry-post-enable",
            Self::ResizeRefresh => "resize-refresh",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelCapabilitySnapshot {
    columns: u16,
    rows: u16,
    pixel_mode: Option<ReportedPixelMode>,
    area_pixels: Option<(u32, u32)>,
    cell_pixels: Option<(i64, i64)>,
    resize_seen: bool,
}

impl PixelCapabilitySnapshot {
    fn new(replies: CapabilityReplies, columns: u16, rows: u16) -> Self {
        Self {
            columns,
            rows,
            pixel_mode: replies.pixel_mode,
            area_pixels: replies.area_pixels,
            cell_pixels: replies.cell_pixels,
            resize_seen: replies.resize_seen,
        }
    }

    fn with_resize_seen(mut self, resize_seen: bool) -> Self {
        self.resize_seen = resize_seen;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelCandidate {
    metrics: TerminalPixelMetrics,
    origin: PixelCoordinateOrigin,
    initial: PixelCapabilitySnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PixelFallbackReason {
    OriginPolicyRejected,
    InitialModeTimeout {
        stage: PixelNegotiationStage,
        snapshot: PixelCapabilitySnapshot,
    },
    InitialModeUnsupported {
        stage: PixelNegotiationStage,
        snapshot: PixelCapabilitySnapshot,
    },
    UnexpectedInitialMode {
        stage: PixelNegotiationStage,
        snapshot: PixelCapabilitySnapshot,
    },
    MissingMetricResponses {
        stage: PixelNegotiationStage,
        snapshot: PixelCapabilitySnapshot,
        area_missing: bool,
        cell_missing: bool,
    },
    MalformedMetrics {
        stage: PixelNegotiationStage,
        snapshot: PixelCapabilitySnapshot,
    },
    InconsistentMetrics {
        stage: PixelNegotiationStage,
        snapshot: PixelCapabilitySnapshot,
    },
    ResizeInterrupted {
        stage: PixelNegotiationStage,
        snapshot: PixelCapabilitySnapshot,
    },
    PostEnableModeTimeout {
        stage: PixelNegotiationStage,
        initial: PixelCapabilitySnapshot,
    },
    PostEnableModeNotSet {
        stage: PixelNegotiationStage,
        reported: ReportedPixelMode,
        initial: PixelCapabilitySnapshot,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CellFallbackOutcome {
    Succeeded,
    Failed { error: String },
    SkippedUnsafePixelMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeferredRetryDiagnostic {
    Pending,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MouseTraceContext {
    term_program: Option<String>,
    term_program_version: Option<String>,
    pixel_origin: Option<PixelCoordinateOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MouseFallbackDiagnostic {
    context: MouseTraceContext,
    reason: PixelFallbackReason,
    cells_fallback: CellFallbackOutcome,
    deferred_retry: Option<DeferredRetryDiagnostic>,
}

impl MouseFallbackDiagnostic {
    fn new(
        context: &MouseTraceContext,
        reason: PixelFallbackReason,
        cells_fallback: CellFallbackOutcome,
    ) -> Self {
        Self {
            context: context.clone(),
            reason,
            cells_fallback,
            deferred_retry: None,
        }
    }

    fn with_deferred_retry(mut self, deferred_retry: DeferredRetryDiagnostic) -> Self {
        self.deferred_retry = Some(deferred_retry);
        self
    }

    fn format(&self) -> String {
        let mut fields = Vec::new();
        fields.push(format!("reason={}", self.reason.trace_label()));
        fields.push(format!(
            "term_program={}",
            self.context.term_program.as_deref().unwrap_or("unset")
        ));
        fields.push(format!(
            "term_program_version={}",
            self.context
                .term_program_version
                .as_deref()
                .unwrap_or("unset")
        ));
        fields.push(format!(
            "origin_policy={}",
            match self.context.pixel_origin {
                Some(PixelCoordinateOrigin::ZeroBased) => "accepted-zero-based",
                Some(PixelCoordinateOrigin::OneBased) => "accepted-one-based",
                None => "rejected",
            }
        ));

        if let Some(stage) = self.reason.stage() {
            fields.push(format!("stage={}", stage.trace_label()));
        }
        if let Some(snapshot) = self.reason.snapshot() {
            let missing_mode_label =
                if self.reason.stage() == Some(PixelNegotiationStage::ResizeRefresh) {
                    "not-queried"
                } else {
                    "timeout"
                };
            fields.push(format!(
                "reported_1016={}",
                snapshot
                    .pixel_mode
                    .map(ReportedPixelMode::trace_label)
                    .unwrap_or(missing_mode_label)
            ));
            fields.push(format!(
                "terminal_cells={}x{}",
                snapshot.columns, snapshot.rows
            ));
            fields.push(format!(
                "area_px={}",
                format_optional_pair(snapshot.area_pixels)
            ));
            fields.push(format!(
                "cell_px={}",
                format_optional_pair(snapshot.cell_pixels)
            ));
            fields.push(format!("resize_seen={}", snapshot.resize_seen));
        }
        match &self.reason {
            PixelFallbackReason::PostEnableModeTimeout { .. } => {
                fields.push("post_enable_1016=timeout".to_string());
            }
            PixelFallbackReason::PostEnableModeNotSet { reported, .. } => {
                fields.push(format!("post_enable_1016={}", reported.trace_label()));
            }
            _ => {}
        }

        match &self.cells_fallback {
            CellFallbackOutcome::Succeeded => fields.push("cells_fallback=success".to_string()),
            CellFallbackOutcome::Failed { error } => {
                fields.push("cells_fallback=failure".to_string());
                fields.push(format!("cells_fallback_error={error:?}"));
            }
            CellFallbackOutcome::SkippedUnsafePixelMode => {
                fields.push("cells_fallback=skipped-unsafe-active-1016".to_string());
            }
        }
        if let Some(deferred_retry) = self.deferred_retry {
            fields.push(format!(
                "deferred_retry={}",
                match deferred_retry {
                    DeferredRetryDiagnostic::Pending => "pending-after-resize-redraw",
                    DeferredRetryDiagnostic::Failed => "failed",
                }
            ));
        }

        fields.join("; ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MouseTraceDiagnostic {
    Fallback(MouseFallbackDiagnostic),
    DeferredRetrySucceeded,
}

impl MouseTraceDiagnostic {
    fn format_line(&self) -> String {
        match self {
            Self::Fallback(diagnostic) => {
                format!("atc terminal mouse fallback: {}", diagnostic.format())
            }
            Self::DeferredRetrySucceeded => "atc terminal mouse negotiation: initial attempt \
                 interrupted by resize; deferred retry succeeded"
                .to_string(),
        }
    }
}

impl PixelFallbackReason {
    const fn schedules_deferred_retry(&self) -> bool {
        matches!(
            self,
            Self::ResizeInterrupted {
                stage: PixelNegotiationStage::Initial | PixelNegotiationStage::InitialPostEnable,
                ..
            }
        )
    }

    const fn trace_label(&self) -> &'static str {
        match self {
            Self::OriginPolicyRejected => "terminal-origin-policy-rejected",
            Self::InitialModeTimeout {
                stage: PixelNegotiationStage::DeferredRetry,
                ..
            } => "deferred-retry-decrqm-1016-timeout",
            Self::InitialModeTimeout { .. } => "initial-decrqm-1016-timeout",
            Self::InitialModeUnsupported {
                stage: PixelNegotiationStage::DeferredRetry,
                ..
            } => "deferred-retry-1016-unsupported",
            Self::InitialModeUnsupported { .. } => "initial-1016-unsupported",
            Self::UnexpectedInitialMode {
                stage: PixelNegotiationStage::DeferredRetry,
                ..
            } => "unexpected-deferred-retry-1016-mode",
            Self::UnexpectedInitialMode { .. } => "unexpected-initial-1016-mode",
            Self::MissingMetricResponses {
                area_missing: true,
                cell_missing: true,
                ..
            } => "missing-text-area-and-cell-pixel-responses",
            Self::MissingMetricResponses {
                area_missing: true, ..
            } => "missing-text-area-pixel-response",
            Self::MissingMetricResponses {
                cell_missing: true, ..
            } => "missing-cell-pixel-response",
            Self::MissingMetricResponses { .. } => "missing-pixel-metric-response",
            Self::MalformedMetrics { .. } => "malformed-pixel-metrics",
            Self::InconsistentMetrics { .. } => "inconsistent-pixel-metrics",
            Self::ResizeInterrupted { .. } => "resize-interrupted-pixel-negotiation",
            Self::PostEnableModeTimeout { .. } => "post-enable-decrqm-1016-timeout",
            Self::PostEnableModeNotSet { .. } => "post-enable-1016-report-not-set",
        }
    }

    const fn stage(&self) -> Option<PixelNegotiationStage> {
        match self {
            Self::MissingMetricResponses { stage, .. }
            | Self::MalformedMetrics { stage, .. }
            | Self::InconsistentMetrics { stage, .. }
            | Self::ResizeInterrupted { stage, .. } => Some(*stage),
            Self::PostEnableModeTimeout { stage, .. }
            | Self::PostEnableModeNotSet { stage, .. } => Some(*stage),
            Self::InitialModeTimeout { stage, .. }
            | Self::InitialModeUnsupported { stage, .. }
            | Self::UnexpectedInitialMode { stage, .. } => Some(*stage),
            Self::OriginPolicyRejected => Some(PixelNegotiationStage::Initial),
        }
    }

    const fn snapshot(&self) -> Option<PixelCapabilitySnapshot> {
        match self {
            Self::InitialModeTimeout { snapshot, .. }
            | Self::InitialModeUnsupported { snapshot, .. }
            | Self::UnexpectedInitialMode { snapshot, .. }
            | Self::MissingMetricResponses { snapshot, .. }
            | Self::MalformedMetrics { snapshot, .. }
            | Self::InconsistentMetrics { snapshot, .. }
            | Self::ResizeInterrupted { snapshot, .. } => Some(*snapshot),
            Self::PostEnableModeTimeout { initial, .. }
            | Self::PostEnableModeNotSet { initial, .. } => Some(*initial),
            Self::OriginPolicyRejected => None,
        }
    }
}

fn format_optional_pair<T: std::fmt::Display>(pair: Option<(T, T)>) -> String {
    pair.map_or_else(
        || "timeout".to_string(),
        |(width, height)| format!("{width}x{height}"),
    )
}

impl CapabilityReplies {
    fn record(&mut self, reply: CapabilityReply) {
        match reply {
            CapabilityReply::PixelMode(mode) => self.pixel_mode = Some(mode),
            CapabilityReply::AreaPixels { width, height } => {
                self.area_pixels = Some((width, height));
            }
            CapabilityReply::CellPixels { width, height } => {
                self.cell_pixels = Some((width, height));
            }
        }
    }

    fn initial_query_complete(self) -> bool {
        match self.pixel_mode {
            Some(ReportedPixelMode::Reset) => {
                self.area_pixels.is_some() && self.cell_pixels.is_some()
            }
            Some(_) => true,
            None => false,
        }
    }

    fn metrics_complete(self) -> bool {
        self.area_pixels.is_some() && self.cell_pixels.is_some()
    }
}

fn capability_reply(event: &Event) -> Option<CapabilityReply> {
    match event {
        Event::Csi(Csi::Mode(Mode::ReportDecPrivateMode {
            mode: DecPrivateMode::Code(DecPrivateModeCode::SGRPixelsMouse),
            setting,
        })) => Some(CapabilityReply::PixelMode((*setting).into())),
        Event::Csi(Csi::Window(window)) => match window.as_ref() {
            Window::ReportTextAreaOrWindowSizePixelsResponse { width, height } => {
                Some(CapabilityReply::AreaPixels {
                    width: *width,
                    height: *height,
                })
            }
            Window::ReportCellSizePixelsResponse { width, height } => {
                Some(CapabilityReply::CellPixels {
                    // Missing values are a parsed but unusable reply, not an application CSI event.
                    // Preserve that distinction so atc's own malformed response never reaches the
                    // normal event batch as `Ignored`.
                    width: width.unwrap_or(-1),
                    height: height.unwrap_or(-1),
                })
            }
            _ => None,
        },
        _ => None,
    }
}

fn is_internal_query_reply(event: &Event) -> bool {
    capability_reply(event).is_some()
}

fn is_mouse_input(event: &Event) -> bool {
    matches!(event, Event::Mouse(_) | Event::Csi(Csi::Mouse(_)))
}

fn preserve_unrelated_query_event(
    event: Event,
    pending: &mut VecDeque<TerminalEvent>,
    replies: &mut CapabilityReplies,
) {
    if is_mouse_input(&event) {
        return;
    }
    if matches!(event, Event::WindowResized(_)) {
        replies.resize_seen = true;
    }
    pending.push_back(termina_adapter::translate(event));
}

fn collect_query_replies(
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
    timeout: Duration,
    complete: impl Fn(CapabilityReplies) -> bool,
) -> io::Result<CapabilityReplies> {
    let deadline = Instant::now() + timeout;
    let mut replies = CapabilityReplies::default();

    loop {
        if complete(replies) {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !reader.poll(Some(remaining), |_| true)? {
            break;
        }

        let event = reader.read(|_| true)?;
        if let Some(reply) = capability_reply(&event) {
            replies.record(reply);
        } else {
            preserve_unrelated_query_event(event, pending, &mut replies);
        }
    }

    Ok(replies)
}

fn query_initial_pixel_capabilities(
    output: &mut impl Write,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
) -> io::Result<CapabilityReplies> {
    let pixel_mode = DecPrivateMode::Code(DecPrivateModeCode::SGRPixelsMouse);
    write!(
        output,
        "{}{}{}",
        Csi::Mode(Mode::QueryDecPrivateMode(pixel_mode)),
        Csi::Window(Box::new(Window::ReportTextAreaSizePixels)),
        Csi::Window(Box::new(Window::ReportCellSizePixels)),
    )?;
    output.flush()?;

    collect_query_replies(
        reader,
        pending,
        INITIAL_PIXEL_QUERY_TIMEOUT,
        CapabilityReplies::initial_query_complete,
    )
}

fn query_pixel_metrics(
    output: &mut impl Write,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
    timeout: Duration,
) -> io::Result<CapabilityReplies> {
    write!(
        output,
        "{}{}",
        Csi::Window(Box::new(Window::ReportTextAreaSizePixels)),
        Csi::Window(Box::new(Window::ReportCellSizePixels)),
    )?;
    output.flush()?;

    collect_query_replies(
        reader,
        pending,
        timeout,
        CapabilityReplies::metrics_complete,
    )
}

fn query_pixel_mode(
    output: &mut impl Write,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
) -> io::Result<CapabilityReplies> {
    let pixel_mode = DecPrivateMode::Code(DecPrivateModeCode::SGRPixelsMouse);
    write!(
        output,
        "{}",
        Csi::Mode(Mode::QueryDecPrivateMode(pixel_mode))
    )?;
    output.flush()?;

    collect_query_replies(reader, pending, PIXEL_ENABLE_VERIFY_TIMEOUT, |replies| {
        replies.pixel_mode.is_some()
    })
}

fn drain_transition_mouse_input(
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
) -> io::Result<bool> {
    let mut resize_seen = false;
    while reader.poll(Some(Duration::ZERO), |_| true)? {
        let event = reader.read(|_| true)?;
        if is_mouse_input(&event) || is_internal_query_reply(&event) {
            continue;
        }
        resize_seen |= matches!(event, Event::WindowResized(_));
        pending.push_back(termina_adapter::translate(event));
    }
    Ok(resize_seen)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseTransitionStep {
    DisableTracking,
    DisableCells,
    DisablePixels,
    DrainInput,
    ParserCells,
    ParserPixels,
    EnableCells,
    EnablePixels,
    EnableTracking,
}

const CELLS_TRANSITION: &[MouseTransitionStep] = &[
    MouseTransitionStep::DisableTracking,
    MouseTransitionStep::DisablePixels,
    MouseTransitionStep::DisableCells,
    MouseTransitionStep::DrainInput,
    MouseTransitionStep::ParserCells,
    MouseTransitionStep::EnableCells,
    MouseTransitionStep::EnableTracking,
];

const PIXELS_TRANSITION_BEGIN: &[MouseTransitionStep] = &[
    MouseTransitionStep::DisableTracking,
    MouseTransitionStep::DisableCells,
    MouseTransitionStep::DisablePixels,
    MouseTransitionStep::DrainInput,
    MouseTransitionStep::ParserPixels,
    MouseTransitionStep::EnablePixels,
];

const DEFERRED_RETRY_BASELINE: &[MouseTransitionStep] = &[
    MouseTransitionStep::DisableTracking,
    MouseTransitionStep::DisableCells,
    MouseTransitionStep::DisablePixels,
    MouseTransitionStep::DrainInput,
    MouseTransitionStep::ParserCells,
];

const DISABLED_TRANSITION: &[MouseTransitionStep] = &[
    MouseTransitionStep::DisableTracking,
    MouseTransitionStep::DisableCells,
    MouseTransitionStep::DisablePixels,
    MouseTransitionStep::DrainInput,
    MouseTransitionStep::ParserPixels,
];

fn run_transition_plan<E>(
    steps: &[MouseTransitionStep],
    mut apply: impl FnMut(MouseTransitionStep) -> Result<bool, E>,
) -> Result<bool, E> {
    let mut resize_seen = false;
    for &step in steps {
        resize_seen |= apply(step)?;
    }
    Ok(resize_seen)
}

fn execute_transition(
    steps: &[MouseTransitionStep],
    output: &mut SessionTerminal,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
) -> io::Result<bool> {
    run_transition_plan(steps, |step| match step {
        MouseTransitionStep::DisableTracking => output
            .ensure_mode_disabled(
                LifecycleStep::AnyEventMouse,
                DecPrivateModeCode::AnyEventMouse,
            )
            .map(|()| false),
        MouseTransitionStep::DisableCells => output
            .ensure_mode_disabled(LifecycleStep::SgrMouse, DecPrivateModeCode::SGRMouse)
            .map(|()| false),
        MouseTransitionStep::DisablePixels => output
            .ensure_mode_disabled(
                LifecycleStep::SgrPixelsMouseMayBeActive,
                DecPrivateModeCode::SGRPixelsMouse,
            )
            .map(|()| false),
        MouseTransitionStep::DrainInput => drain_transition_mouse_input(reader, pending),
        MouseTransitionStep::ParserCells => reader
            .set_sgr_mouse_input(SgrMouseInput::Cells1006)
            .map(|()| false),
        MouseTransitionStep::ParserPixels => reader
            .set_sgr_mouse_input(SgrMouseInput::Pixels1016)
            .map(|()| false),
        MouseTransitionStep::EnableCells => output
            .change_mode(LifecycleStep::SgrMouse, DecPrivateModeCode::SGRMouse, true)
            .map(|()| false),
        MouseTransitionStep::EnablePixels => output
            .change_mode(
                LifecycleStep::SgrPixelsMouseMayBeActive,
                DecPrivateModeCode::SGRPixelsMouse,
                true,
            )
            .map(|()| false),
        MouseTransitionStep::EnableTracking => output
            .change_mode(
                LifecycleStep::AnyEventMouse,
                DecPrivateModeCode::AnyEventMouse,
                true,
            )
            .map(|()| false),
    })
}

fn transition_to_cells(
    output: &mut SessionTerminal,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
) -> io::Result<()> {
    execute_transition(CELLS_TRANSITION, output, reader, pending).map(|_| ())
}

fn transition_to_disabled(
    output: &mut SessionTerminal,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
) -> io::Result<()> {
    // Tracking is off, so the parser is normally dormant. The pixel configuration is intentional
    // for the permanently-active-1016 case: even if a terminal also violates the 1003 reset, any
    // report is decoded without truncation and rejected while `MouseMode::Disabled`.
    execute_transition(DISABLED_TRANSITION, output, reader, pending).map(|_| ())
}

fn begin_transition_to_pixels(
    output: &mut SessionTerminal,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
) -> io::Result<bool> {
    execute_transition(PIXELS_TRANSITION_BEGIN, output, reader, pending)
}

fn normalize_for_deferred_pixel_retry(
    output: &mut SessionTerminal,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
) -> io::Result<bool> {
    execute_transition(DEFERRED_RETRY_BASELINE, output, reader, pending)
}

fn finish_transition_to_pixels(output: &mut SessionTerminal) -> io::Result<()> {
    output.change_mode(
        LifecycleStep::AnyEventMouse,
        DecPrivateModeCode::AnyEventMouse,
        true,
    )
}

fn classify_pixel_metrics(
    replies: CapabilityReplies,
    columns: u16,
    rows: u16,
    stage: PixelNegotiationStage,
) -> Result<(TerminalPixelMetrics, PixelCapabilitySnapshot), PixelFallbackReason> {
    let snapshot = PixelCapabilitySnapshot::new(replies, columns, rows);
    if replies.resize_seen {
        return Err(PixelFallbackReason::ResizeInterrupted { stage, snapshot });
    }

    let area_missing = replies.area_pixels.is_none();
    let cell_missing = replies.cell_pixels.is_none();
    if area_missing || cell_missing {
        return Err(PixelFallbackReason::MissingMetricResponses {
            stage,
            snapshot,
            area_missing,
            cell_missing,
        });
    }

    let (area_width, area_height) = replies
        .area_pixels
        .expect("presence checked before metric classification");
    let (cell_width, cell_height) = replies
        .cell_pixels
        .expect("presence checked before metric classification");
    let (Ok(cell_width), Ok(cell_height)) = (u32::try_from(cell_width), u32::try_from(cell_height))
    else {
        return Err(PixelFallbackReason::MalformedMetrics { stage, snapshot });
    };

    match TerminalPixelMetrics::validate(
        columns,
        rows,
        area_width,
        area_height,
        cell_width,
        cell_height,
    ) {
        Ok(metrics) => Ok((metrics, snapshot)),
        Err(TerminalPixelMetricsError::Malformed) => {
            Err(PixelFallbackReason::MalformedMetrics { stage, snapshot })
        }
        Err(TerminalPixelMetricsError::Inconsistent) => {
            Err(PixelFallbackReason::InconsistentMetrics { stage, snapshot })
        }
    }
}

fn classify_initial_pixel_candidate(
    replies: CapabilityReplies,
    columns: u16,
    rows: u16,
    origin: PixelCoordinateOrigin,
    stage: PixelNegotiationStage,
) -> Result<PixelCandidate, PixelFallbackReason> {
    let snapshot = PixelCapabilitySnapshot::new(replies, columns, rows);
    if replies.resize_seen {
        return Err(PixelFallbackReason::ResizeInterrupted { stage, snapshot });
    }
    match replies.pixel_mode {
        None => return Err(PixelFallbackReason::InitialModeTimeout { stage, snapshot }),
        Some(ReportedPixelMode::NotRecognized | ReportedPixelMode::PermanentlyReset) => {
            return Err(PixelFallbackReason::InitialModeUnsupported { stage, snapshot });
        }
        Some(ReportedPixelMode::Reset) => {}
        Some(_) => return Err(PixelFallbackReason::UnexpectedInitialMode { stage, snapshot }),
    }

    let (metrics, initial) = classify_pixel_metrics(replies, columns, rows, stage)?;
    Ok(PixelCandidate {
        metrics,
        origin,
        initial,
    })
}

fn classify_post_enable_verification(
    replies: CapabilityReplies,
    initial: PixelCapabilitySnapshot,
    stage: PixelNegotiationStage,
) -> Result<(), PixelFallbackReason> {
    if replies.resize_seen {
        return Err(PixelFallbackReason::ResizeInterrupted {
            stage,
            snapshot: initial.with_resize_seen(true),
        });
    }

    match replies.pixel_mode {
        Some(ReportedPixelMode::Set) => Ok(()),
        None => Err(PixelFallbackReason::PostEnableModeTimeout { stage, initial }),
        Some(reported) => Err(PixelFallbackReason::PostEnableModeNotSet {
            stage,
            reported,
            initial,
        }),
    }
}

fn cell_fallback_is_safe(replies: CapabilityReplies) -> bool {
    !matches!(replies.pixel_mode, Some(ReportedPixelMode::PermanentlySet))
}

fn pixel_mouse_mode(candidate: PixelCandidate, generation: u64) -> MouseMode {
    MouseMode::Pixels {
        metrics: candidate.metrics,
        origin: candidate.origin,
        generation,
    }
}

const fn fallback_mouse_mode(cell_fallback_safe: bool) -> MouseMode {
    if cell_fallback_safe {
        MouseMode::Cells
    } else {
        MouseMode::Disabled
    }
}

fn establish_startup_fallback(
    output: &mut SessionTerminal,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
    context: &MouseTraceContext,
    reason: PixelFallbackReason,
    cell_fallback_safe: bool,
) -> io::Result<(
    MouseMode,
    Option<PixelCoordinateOrigin>,
    MouseFallbackDiagnostic,
)> {
    if cell_fallback_safe {
        transition_to_cells(output, reader, pending)?;
        Ok((
            fallback_mouse_mode(true),
            None,
            MouseFallbackDiagnostic::new(context, reason, CellFallbackOutcome::Succeeded),
        ))
    } else {
        transition_to_disabled(output, reader, pending)?;
        Ok((
            fallback_mouse_mode(false),
            None,
            MouseFallbackDiagnostic::new(
                context,
                reason,
                CellFallbackOutcome::SkippedUnsafePixelMode,
            ),
        ))
    }
}

fn establish_initial_attempt_fallback(
    output: &mut SessionTerminal,
    reader: &EventReader,
    pending: &mut VecDeque<TerminalEvent>,
    context: &MouseTraceContext,
    reason: PixelFallbackReason,
    cell_fallback_safe: bool,
) -> io::Result<(
    MouseMode,
    Option<PixelCoordinateOrigin>,
    MouseTraceDiagnostic,
    HighResRetry,
)> {
    let schedule_retry = reason.schedules_deferred_retry() && cell_fallback_safe;
    let (mode, origin, diagnostic) =
        establish_startup_fallback(output, reader, pending, context, reason, cell_fallback_safe)?;
    let mut high_res_retry = HighResRetry::None;
    let diagnostic = if schedule_retry && mode == MouseMode::Cells {
        high_res_retry.schedule_after_initial_resize();
        diagnostic.with_deferred_retry(DeferredRetryDiagnostic::Pending)
    } else {
        diagnostic
    };
    Ok((
        mode,
        origin,
        MouseTraceDiagnostic::Fallback(diagnostic),
        high_res_retry,
    ))
}

/// Sole owner of rendering, input, and terminal lifecycle for the watch TUI.
struct TerminalResources {
    terminal: SessionRatatuiTerminal,
    reader: EventReader,
}

pub(crate) struct TerminaSession {
    resources: Option<TerminalResources>,
    pending_events: VecDeque<TerminalEvent>,
    mouse_mode: MouseMode,
    pixel_origin: Option<PixelCoordinateOrigin>,
    pixel_generation: u64,
    pixel_refresh: PixelRefresh,
    high_res_retry: HighResRetry,
    mouse_trace_context: MouseTraceContext,
    mouse_trace_diagnostic: Option<MouseTraceDiagnostic>,
    panic_hook: Option<ScopedPanicHook>,
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
        let mut pending_events = VecDeque::new();

        output.change_mode(
            LifecycleStep::AlternateScreen,
            DecPrivateModeCode::ClearAndEnableAlternateScreen,
            true,
        )?;
        output.change_mode(
            LifecycleStep::BracketedPaste,
            DecPrivateModeCode::BracketedPaste,
            true,
        )?;

        // Negotiate from a mouse-disabled baseline. Since 1006 and 1016 have the same wire
        // grammar, no tracking is enabled until the parser and coordinate mode agree.
        output.ensure_mode_disabled(
            LifecycleStep::AnyEventMouse,
            DecPrivateModeCode::AnyEventMouse,
        )?;
        output.ensure_mode_disabled(LifecycleStep::SgrMouse, DecPrivateModeCode::SGRMouse)?;
        output.ensure_mode_disabled(
            LifecycleStep::SgrPixelsMouseMayBeActive,
            DecPrivateModeCode::SGRPixelsMouse,
        )?;

        let term_program = std::env::var("TERM_PROGRAM").ok();
        let trusted_origin = trusted_pixel_origin(term_program.as_deref());
        let mouse_trace_context = MouseTraceContext {
            term_program,
            term_program_version: std::env::var("TERM_PROGRAM_VERSION").ok(),
            pixel_origin: trusted_origin,
        };
        let dimensions = output.get_dimensions()?;
        let (initial_selection, cell_fallback_safe) = if let Some(origin) = trusted_origin {
            let replies =
                query_initial_pixel_capabilities(&mut output, &reader, &mut pending_events)?;
            (
                classify_initial_pixel_candidate(
                    replies,
                    dimensions.cols,
                    dimensions.rows,
                    origin,
                    PixelNegotiationStage::Initial,
                ),
                cell_fallback_is_safe(replies),
            )
        } else {
            (Err(PixelFallbackReason::OriginPolicyRejected), true)
        };

        let pixel_generation = 1;
        let (mouse_mode, pixel_origin, mouse_trace_diagnostic, high_res_retry) =
            match initial_selection {
                Ok(candidate) => {
                    let resize_seen =
                        begin_transition_to_pixels(&mut output, &reader, &mut pending_events)?;
                    let mut verification = if resize_seen {
                        CapabilityReplies {
                            resize_seen: true,
                            ..CapabilityReplies::default()
                        }
                    } else {
                        query_pixel_mode(&mut output, &reader, &mut pending_events)?
                    };
                    verification.resize_seen |=
                        drain_transition_mouse_input(&reader, &mut pending_events)?;

                    match classify_post_enable_verification(
                        verification,
                        candidate.initial,
                        PixelNegotiationStage::InitialPostEnable,
                    ) {
                        Ok(()) => {
                            finish_transition_to_pixels(&mut output)?;
                            (
                                pixel_mouse_mode(candidate, pixel_generation),
                                Some(candidate.origin),
                                None,
                                HighResRetry::None,
                            )
                        }
                        Err(reason) => {
                            let (mode, origin, diagnostic, high_res_retry) =
                                establish_initial_attempt_fallback(
                                    &mut output,
                                    &reader,
                                    &mut pending_events,
                                    &mouse_trace_context,
                                    reason,
                                    cell_fallback_is_safe(verification),
                                )?;
                            (mode, origin, Some(diagnostic), high_res_retry)
                        }
                    }
                }
                Err(reason) => {
                    let (mode, origin, diagnostic, high_res_retry) =
                        establish_initial_attempt_fallback(
                            &mut output,
                            &reader,
                            &mut pending_events,
                            &mouse_trace_context,
                            reason,
                            cell_fallback_safe,
                        )?;
                    (mode, origin, Some(diagnostic), high_res_retry)
                }
            };

        output.change_mode(
            LifecycleStep::CursorHidden,
            DecPrivateModeCode::ShowCursor,
            false,
        )?;

        let backend = TerminaBackend::new(output);
        let terminal = RatatuiTerminal::new(backend)?;

        Ok(Self {
            resources: Some(TerminalResources { terminal, reader }),
            pending_events,
            mouse_mode,
            pixel_origin,
            pixel_generation,
            pixel_refresh: PixelRefresh::None,
            high_res_retry,
            mouse_trace_context,
            mouse_trace_diagnostic,
            panic_hook: Some(panic_hook),
        })
    }

    pub(super) fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<()> {
        self.resources
            .as_mut()
            .expect("live TUI session must own terminal resources")
            .terminal
            .draw(render)
            .map(|_| ())
    }

    pub(super) fn poll(&mut self, wait: Duration) -> io::Result<bool> {
        if !self.pending_events.is_empty() {
            return Ok(true);
        }

        let deadline = Instant::now() + wait;
        let mut remaining = wait;
        loop {
            let reader = &self
                .resources
                .as_ref()
                .expect("live TUI session must own terminal resources")
                .reader;
            if !reader.poll(Some(remaining), |_| true)? {
                return Ok(false);
            }
            let event = reader.read(|_| true)?;
            if let Some(event) = self.application_event(event) {
                self.pending_events.push_back(event);
                return Ok(true);
            }
            remaining = deadline.saturating_duration_since(Instant::now());
        }
    }

    pub(super) fn read(&mut self) -> io::Result<TerminalEvent> {
        // Termina may discard unsupported byte sequences inside its parser. Do not synthesize
        // events for those bytes; every event Termina does emit occupies exactly one application
        // batch slot, including emitted-but-unused events translated to `Ignored`. Replies to
        // atc's own bounded queries are consumed internally and do not occupy a batch slot.
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(event);
        }
        loop {
            let event = self
                .resources
                .as_ref()
                .expect("live TUI session must own terminal resources")
                .reader
                .read(|_| true)?;
            if let Some(event) = self.application_event(event) {
                return Ok(event);
            }
        }
    }

    fn application_event(&mut self, event: Event) -> Option<TerminalEvent> {
        if is_internal_query_reply(&event) {
            return None;
        }

        if matches!(event, Event::WindowResized(_)) {
            self.invalidate_pixel_metrics();
        }

        let mut event = termina_adapter::translate(event);
        if let TerminalEvent::Pointer(PointerEvent {
            position: PointerPosition::AbsolutePixels { .. },
            pixel_generation,
            ..
        }) = &mut event
        {
            *pixel_generation = match self.mouse_mode {
                MouseMode::Pixels { generation, .. } => Some(generation),
                MouseMode::Disabled | MouseMode::Cells => None,
            };
        }
        Some(event)
    }

    fn invalidate_pixel_metrics(&mut self) {
        if matches!(self.mouse_mode, MouseMode::Pixels { .. }) {
            self.pixel_generation = self.pixel_generation.saturating_add(1);
            self.mouse_mode = MouseMode::Disabled;
            self.pixel_refresh.schedule_new();
        } else {
            // A second Resize can be read while the first one is waiting for its redraw. The
            // eventual refresh must correspond to the last dispatched geometry, not an earlier
            // redraw in the same burst.
            self.pixel_refresh.schedule_after_resize();
        }
    }

    pub(super) fn mouse_mode(&self) -> MouseMode {
        self.mouse_mode
    }

    pub(crate) fn mouse_mode_label(&self) -> &'static str {
        self.mouse_mode.label()
    }

    pub(crate) fn mouse_trace_line(&self) -> Option<String> {
        self.mouse_trace_diagnostic
            .as_ref()
            .map(MouseTraceDiagnostic::format_line)
    }

    pub(super) fn note_resize_dispatched(&mut self) {
        self.pixel_refresh.schedule_after_resize();
        self.high_res_retry.observe_resize_boundary();
    }

    pub(super) fn note_redraw_completed(&mut self) {
        self.pixel_refresh.observe_redraw();
    }

    fn resize_work_is_blocked(&self, application_resize_pending: bool) -> bool {
        application_resize_pending
            || self
                .pending_events
                .iter()
                .any(|event| matches!(event, TerminalEvent::Resize(_)))
    }

    fn pending_quit(&self) -> bool {
        self.pending_events.iter().any(|event| {
            matches!(
                event,
                TerminalEvent::Key(KeyEvent {
                    code: KeyCode::Char('q'),
                    kind: KeyEventKind::Press,
                    ..
                })
            )
        })
    }

    fn buffer_available_application_events(&mut self) -> io::Result<()> {
        while self
            .resources
            .as_ref()
            .expect("live TUI session must own terminal resources")
            .reader
            .poll(Some(Duration::ZERO), |_| true)?
        {
            let event = self
                .resources
                .as_ref()
                .expect("live TUI session must own terminal resources")
                .reader
                .read(|_| true)?;
            if let Some(event) = self.application_event(event) {
                self.pending_events.push_back(event);
            }
        }
        Ok(())
    }

    fn complete_deferred_retry_fallback(
        &mut self,
        reason: PixelFallbackReason,
        cell_fallback_safe: bool,
    ) -> io::Result<()> {
        let fallback = {
            let resources = self
                .resources
                .as_mut()
                .expect("live TUI session must own terminal resources");
            if cell_fallback_safe {
                transition_to_cells(
                    resources.terminal.backend_mut().terminal_mut(),
                    &resources.reader,
                    &mut self.pending_events,
                )
            } else {
                transition_to_disabled(
                    resources.terminal.backend_mut().terminal_mut(),
                    &resources.reader,
                    &mut self.pending_events,
                )
            }
        };

        match fallback {
            Ok(()) => {
                self.mouse_mode = fallback_mouse_mode(cell_fallback_safe);
                self.pixel_origin = None;
                let outcome = if cell_fallback_safe {
                    CellFallbackOutcome::Succeeded
                } else {
                    CellFallbackOutcome::SkippedUnsafePixelMode
                };
                self.mouse_trace_diagnostic = Some(MouseTraceDiagnostic::Fallback(
                    MouseFallbackDiagnostic::new(&self.mouse_trace_context, reason, outcome)
                        .with_deferred_retry(DeferredRetryDiagnostic::Failed),
                ));
                Ok(())
            }
            Err(error) => {
                self.mouse_trace_diagnostic = Some(MouseTraceDiagnostic::Fallback(
                    MouseFallbackDiagnostic::new(
                        &self.mouse_trace_context,
                        reason,
                        CellFallbackOutcome::Failed {
                            error: error.to_string(),
                        },
                    )
                    .with_deferred_retry(DeferredRetryDiagnostic::Failed),
                ));
                Err(error)
            }
        }
    }

    pub(super) fn retry_high_res_after_redraw(
        &mut self,
        application_resize_pending: bool,
    ) -> io::Result<()> {
        if self.high_res_retry != HighResRetry::ReadyAfterRedraw {
            return Ok(());
        }
        // A Resize can arrive during the redraw and already be buffered in the sole EventReader
        // without appearing in either application queue yet. Preserve all such input before
        // deciding whether this is the settled boundary for the one allowed retry.
        self.buffer_available_application_events()?;
        let resize_pending = self.resize_work_is_blocked(application_resize_pending);
        if self.pending_quit() || !self.high_res_retry.take_after_redraw(resize_pending) {
            return Ok(());
        }

        debug_assert_eq!(self.mouse_mode, MouseMode::Cells);
        self.pending_events
            .retain(|event| !matches!(event, TerminalEvent::Pointer(_)));
        let dimensions = self
            .resources
            .as_ref()
            .expect("live TUI session must own terminal resources")
            .terminal
            .backend()
            .terminal()
            .get_dimensions()?;
        let origin = self
            .mouse_trace_context
            .pixel_origin
            .ok_or_else(|| io::Error::other("deferred pixel retry lost its trusted origin"))?;
        let (initial_selection, cell_fallback_safe) = {
            let resources = self
                .resources
                .as_mut()
                .expect("live TUI session must own terminal resources");
            let output = resources.terminal.backend_mut().terminal_mut();
            let resize_seen = normalize_for_deferred_pixel_retry(
                output,
                &resources.reader,
                &mut self.pending_events,
            )?;
            let replies = if resize_seen {
                CapabilityReplies {
                    resize_seen: true,
                    ..CapabilityReplies::default()
                }
            } else {
                query_initial_pixel_capabilities(
                    output,
                    &resources.reader,
                    &mut self.pending_events,
                )?
            };
            (
                classify_initial_pixel_candidate(
                    replies,
                    dimensions.cols,
                    dimensions.rows,
                    origin,
                    PixelNegotiationStage::DeferredRetry,
                ),
                cell_fallback_is_safe(replies),
            )
        };

        let candidate = match initial_selection {
            Ok(candidate) => candidate,
            Err(reason) => {
                return self.complete_deferred_retry_fallback(reason, cell_fallback_safe);
            }
        };

        let verification = {
            let resources = self
                .resources
                .as_mut()
                .expect("live TUI session must own terminal resources");
            let output = resources.terminal.backend_mut().terminal_mut();
            let resize_seen =
                begin_transition_to_pixels(output, &resources.reader, &mut self.pending_events)?;
            let mut verification = if resize_seen {
                CapabilityReplies {
                    resize_seen: true,
                    ..CapabilityReplies::default()
                }
            } else {
                query_pixel_mode(output, &resources.reader, &mut self.pending_events)?
            };
            verification.resize_seen |=
                drain_transition_mouse_input(&resources.reader, &mut self.pending_events)?;
            verification
        };

        if let Err(reason) = classify_post_enable_verification(
            verification,
            candidate.initial,
            PixelNegotiationStage::DeferredRetryPostEnable,
        ) {
            return self
                .complete_deferred_retry_fallback(reason, cell_fallback_is_safe(verification));
        }

        finish_transition_to_pixels(
            self.resources
                .as_mut()
                .expect("live TUI session must own terminal resources")
                .terminal
                .backend_mut()
                .terminal_mut(),
        )?;
        self.mouse_mode = pixel_mouse_mode(candidate, self.pixel_generation);
        self.pixel_origin = Some(candidate.origin);
        self.mouse_trace_diagnostic = Some(MouseTraceDiagnostic::DeferredRetrySucceeded);
        Ok(())
    }

    pub(super) fn refresh_mouse_after_redraw(
        &mut self,
        application_resize_pending: bool,
    ) -> io::Result<()> {
        if !self.pixel_refresh.is_ready() {
            return Ok(());
        }
        self.buffer_available_application_events()?;
        if self.pending_quit()
            || !self.pixel_refresh.is_ready()
            || self.resize_work_is_blocked(application_resize_pending)
        {
            return Ok(());
        }

        self.pending_events.retain(|event| {
            !matches!(
                event,
                TerminalEvent::Pointer(PointerEvent {
                    position: PointerPosition::AbsolutePixels { .. },
                    ..
                })
            )
        });

        let dimensions = self
            .resources
            .as_ref()
            .expect("live TUI session must own terminal resources")
            .terminal
            .backend()
            .terminal()
            .get_dimensions()?;
        let origin = self.pixel_origin;
        let refreshed = {
            let resources = self
                .resources
                .as_mut()
                .expect("live TUI session must own terminal resources");
            let output = resources.terminal.backend_mut().terminal_mut();
            output.ensure_mode_disabled(
                LifecycleStep::AnyEventMouse,
                DecPrivateModeCode::AnyEventMouse,
            )?;
            let resize_seen =
                drain_transition_mouse_input(&resources.reader, &mut self.pending_events)?;
            let replies = if resize_seen {
                CapabilityReplies {
                    resize_seen: true,
                    ..CapabilityReplies::default()
                }
            } else {
                query_pixel_metrics(
                    output,
                    &resources.reader,
                    &mut self.pending_events,
                    RESIZE_METRIC_QUERY_TIMEOUT,
                )?
            };
            classify_pixel_metrics(
                replies,
                dimensions.cols,
                dimensions.rows,
                PixelNegotiationStage::ResizeRefresh,
            )
            .and_then(|(metrics, _)| {
                origin
                    .map(|origin| (metrics, origin))
                    .ok_or(PixelFallbackReason::OriginPolicyRejected)
            })
        };

        match refreshed {
            Ok((metrics, origin)) => {
                finish_transition_to_pixels(
                    self.resources
                        .as_mut()
                        .expect("live TUI session must own terminal resources")
                        .terminal
                        .backend_mut()
                        .terminal_mut(),
                )?;
                self.mouse_mode = MouseMode::Pixels {
                    metrics,
                    origin,
                    generation: self.pixel_generation,
                };
                self.mouse_trace_diagnostic = None;
            }
            Err(reason) => {
                let resources = self
                    .resources
                    .as_mut()
                    .expect("live TUI session must own terminal resources");
                let fallback = transition_to_cells(
                    resources.terminal.backend_mut().terminal_mut(),
                    &resources.reader,
                    &mut self.pending_events,
                );
                match fallback {
                    Ok(()) => {
                        self.mouse_mode = MouseMode::Cells;
                        self.pixel_origin = None;
                        self.mouse_trace_diagnostic = Some(MouseTraceDiagnostic::Fallback(
                            MouseFallbackDiagnostic::new(
                                &self.mouse_trace_context,
                                reason,
                                CellFallbackOutcome::Succeeded,
                            ),
                        ));
                    }
                    Err(error) => {
                        self.mouse_trace_diagnostic = Some(MouseTraceDiagnostic::Fallback(
                            MouseFallbackDiagnostic::new(
                                &self.mouse_trace_context,
                                reason,
                                CellFallbackOutcome::Failed {
                                    error: error.to_string(),
                                },
                            ),
                        ));
                        return Err(error);
                    }
                }
            }
        }
        self.pixel_refresh.clear();
        Ok(())
    }

    pub(crate) fn restore(&mut self) -> io::Result<()> {
        let Some(resources) = self.resources.as_mut() else {
            return Ok(());
        };
        resources.terminal.backend_mut().terminal_mut().restore()
    }

    fn restart(&mut self) -> io::Result<()> {
        // A failed suspension keeps only the uncertain lifecycle steps active so cleanup can be
        // retried. Finish that cleanup before replacing the handles.
        self.restore()?;

        // Termina's platform terminal restores its captured cooked/platform modes from `Drop`.
        // Destroy both the old terminal and its last EventReader before starting the replacement;
        // otherwise the old drop would run after `start` and undo the replacement's raw mode.
        let mut replacement =
            recreate_after_input_flush(&mut self.resources, flush_terminal_input, Self::start)?;

        // `start` temporarily nests a panic hook. Restore this live session's hook before moving
        // the replacement terminal state into it.
        drop(replacement.panic_hook.take());

        self.resources = replacement.resources.take();
        std::mem::swap(&mut self.pending_events, &mut replacement.pending_events);
        std::mem::swap(&mut self.mouse_mode, &mut replacement.mouse_mode);
        std::mem::swap(&mut self.pixel_origin, &mut replacement.pixel_origin);
        std::mem::swap(
            &mut self.pixel_generation,
            &mut replacement.pixel_generation,
        );
        std::mem::swap(&mut self.pixel_refresh, &mut replacement.pixel_refresh);
        std::mem::swap(&mut self.high_res_retry, &mut replacement.high_res_retry);
        std::mem::swap(
            &mut self.mouse_trace_context,
            &mut replacement.mouse_trace_context,
        );
        std::mem::swap(
            &mut self.mouse_trace_diagnostic,
            &mut replacement.mouse_trace_diagnostic,
        );

        Ok(())
    }

    pub(super) fn suspend_and_run<T, E>(
        &mut self,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, SuspendedRunError<E>> {
        let mut operation = Some(operation);
        run_suspended_with(
            self,
            Self::restore,
            |_| {
                operation
                    .take()
                    .expect("editor operation runs at most once")()
            },
            Self::restart,
        )
    }
}

impl Drop for TerminaSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TerminalEvent {
    Key(KeyEvent),
    Paste(String),
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
    Enter,
    Escape,
    Backspace,
    Delete,
    Home,
    End,
    Tab,
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
    pub(super) pixel_generation: Option<u64>,
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
    Cells { column: u16, row: u16 },
    AbsolutePixels { x: u32, y: u32 },
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[derive(Default)]
    struct SuspendedHarness {
        events: Vec<&'static str>,
        suspend_error: bool,
        resume_error: bool,
    }

    fn harness_suspend(harness: &mut SuspendedHarness) -> io::Result<()> {
        harness.events.push("suspend");
        if harness.suspend_error {
            Err(io::Error::other("suspend failed"))
        } else {
            Ok(())
        }
    }

    fn harness_resume(harness: &mut SuspendedHarness) -> io::Result<()> {
        harness.events.push("restore");
        if harness.resume_error {
            Err(io::Error::other("restore failed"))
        } else {
            Ok(())
        }
    }

    fn complete_replies() -> CapabilityReplies {
        CapabilityReplies {
            pixel_mode: Some(ReportedPixelMode::Reset),
            area_pixels: Some((800, 480)),
            cell_pixels: Some((10, 20)),
            resize_seen: false,
        }
    }

    fn classify_initial(replies: CapabilityReplies) -> Result<PixelCandidate, PixelFallbackReason> {
        classify_initial_pixel_candidate(
            replies,
            80,
            24,
            PixelCoordinateOrigin::ZeroBased,
            PixelNegotiationStage::Initial,
        )
    }

    #[test]
    fn capability_diagnostics_classify_every_initial_fallback_reason() {
        let origin = PixelCoordinateOrigin::ZeroBased;
        let candidate = classify_initial(complete_replies())
            .expect("complete consistent replies should select a pixel candidate");
        assert_eq!(candidate.metrics.area_width_px, 800);
        assert_eq!(candidate.origin, origin);

        let mut unsupported = complete_replies();
        unsupported.pixel_mode = Some(ReportedPixelMode::NotRecognized);
        let unsupported = classify_initial(unsupported).unwrap_err();
        assert!(matches!(
            unsupported,
            PixelFallbackReason::InitialModeUnsupported { .. }
        ));
        assert!(!unsupported.schedules_deferred_retry());

        let timeout = CapabilityReplies::default();
        assert!(matches!(
            classify_initial(timeout),
            Err(PixelFallbackReason::InitialModeTimeout { .. })
        ));

        let mut permanently_reset = complete_replies();
        permanently_reset.pixel_mode = Some(ReportedPixelMode::PermanentlyReset);
        assert!(matches!(
            classify_initial(permanently_reset),
            Err(PixelFallbackReason::InitialModeUnsupported { .. })
        ));

        let mut unexpected = complete_replies();
        unexpected.pixel_mode = Some(ReportedPixelMode::Set);
        assert!(matches!(
            classify_initial(unexpected),
            Err(PixelFallbackReason::UnexpectedInitialMode { .. })
        ));

        let mut area_timeout = complete_replies();
        area_timeout.area_pixels = None;
        assert!(matches!(
            classify_initial(area_timeout),
            Err(PixelFallbackReason::MissingMetricResponses {
                area_missing: true,
                cell_missing: false,
                ..
            })
        ));

        let mut cell_timeout = complete_replies();
        cell_timeout.cell_pixels = None;
        assert!(matches!(
            classify_initial(cell_timeout),
            Err(PixelFallbackReason::MissingMetricResponses {
                area_missing: false,
                cell_missing: true,
                ..
            })
        ));

        let mut malformed = complete_replies();
        malformed.cell_pixels = Some((-1, 20));
        assert!(matches!(
            classify_initial(malformed),
            Err(PixelFallbackReason::MalformedMetrics { .. })
        ));

        let mut inconsistent = complete_replies();
        inconsistent.area_pixels = Some((801, 480));
        assert!(matches!(
            classify_initial(inconsistent),
            Err(PixelFallbackReason::InconsistentMetrics { .. })
        ));

        let mut resized = complete_replies();
        resized.resize_seen = true;
        let interrupted = classify_initial(resized).unwrap_err();
        assert!(matches!(
            interrupted,
            PixelFallbackReason::ResizeInterrupted {
                stage: PixelNegotiationStage::Initial,
                ..
            }
        ));
        assert!(interrupted.schedules_deferred_retry());
        let snapshot = interrupted.snapshot().unwrap();
        assert_eq!(snapshot.area_pixels, Some((800, 480)));
        assert_eq!(snapshot.cell_pixels, Some((10, 20)));
        assert!(snapshot.resize_seen);
    }

    #[test]
    fn only_an_initial_resize_schedules_the_deferred_retry() {
        let mut replies = complete_replies();
        replies.resize_seen = true;

        let initial = classify_initial(replies).unwrap_err();
        assert!(initial.schedules_deferred_retry());

        let retry = classify_initial_pixel_candidate(
            replies,
            80,
            24,
            PixelCoordinateOrigin::ZeroBased,
            PixelNegotiationStage::DeferredRetry,
        )
        .unwrap_err();
        assert!(!retry.schedules_deferred_retry());

        let refresh = classify_pixel_metrics(replies, 80, 24, PixelNegotiationStage::ResizeRefresh)
            .unwrap_err();
        assert!(!refresh.schedules_deferred_retry());

        let post_enable = classify_post_enable_verification(
            CapabilityReplies {
                resize_seen: true,
                ..CapabilityReplies::default()
            },
            PixelCapabilitySnapshot::new(complete_replies(), 80, 24),
            PixelNegotiationStage::InitialPostEnable,
        )
        .unwrap_err();
        assert!(post_enable.schedules_deferred_retry());

        let deferred_post_enable = classify_post_enable_verification(
            CapabilityReplies {
                resize_seen: true,
                ..CapabilityReplies::default()
            },
            PixelCapabilitySnapshot::new(complete_replies(), 80, 24),
            PixelNegotiationStage::DeferredRetryPostEnable,
        )
        .unwrap_err();
        assert!(!deferred_post_enable.schedules_deferred_retry());
    }

    #[test]
    fn suspended_operation_orders_suspend_launch_restore_and_restores_after_success() {
        let mut harness = SuspendedHarness::default();
        let result = run_suspended_with(
            &mut harness,
            harness_suspend,
            |harness| {
                harness.events.push("launch");
                Ok::<_, &'static str>("complete")
            },
            harness_resume,
        );

        assert_eq!(result.unwrap(), "complete");
        assert_eq!(harness.events, ["suspend", "launch", "restore"]);
    }

    #[test]
    fn suspended_operation_restores_after_spawn_or_nonzero_failure() {
        for operation_error in ["spawn failed", "editor exited 7"] {
            let mut harness = SuspendedHarness::default();
            let error = run_suspended_with(
                &mut harness,
                harness_suspend,
                |harness| {
                    harness.events.push("launch");
                    Err::<(), _>(operation_error)
                },
                harness_resume,
            )
            .unwrap_err();

            assert!(
                matches!(error, SuspendedRunError::Operation(error) if error == operation_error)
            );
            assert_eq!(harness.events, ["suspend", "launch", "restore"]);
        }
    }

    #[test]
    fn suspended_operation_prioritizes_restore_failure_and_preserves_editor_error() {
        let mut harness = SuspendedHarness {
            resume_error: true,
            ..SuspendedHarness::default()
        };
        let error = run_suspended_with(
            &mut harness,
            harness_suspend,
            |harness| {
                harness.events.push("launch");
                Err::<(), _>("spawn failed")
            },
            harness_resume,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SuspendedRunError::OperationAndResume { .. }
        ));
        let display = error.to_string();
        assert!(display.starts_with("failed to restore the TUI terminal"));
        assert!(display.contains("spawn failed"));
        assert_eq!(harness.events, ["suspend", "launch", "restore"]);
    }

    #[test]
    fn failed_suspend_skips_editor_but_still_attempts_tui_restoration() {
        let mut harness = SuspendedHarness {
            suspend_error: true,
            ..SuspendedHarness::default()
        };
        let error = run_suspended_with(
            &mut harness,
            harness_suspend,
            |harness| {
                harness.events.push("must-not-launch");
                Ok::<_, &'static str>(())
            },
            harness_resume,
        )
        .unwrap_err();

        assert!(matches!(error, SuspendedRunError::Suspend(_)));
        assert_eq!(harness.events, ["suspend", "restore"]);
    }

    #[test]
    fn terminal_input_is_flushed_and_old_resources_drop_before_recreation_starts() {
        struct DropMarker(std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>);

        impl Drop for DropMarker {
            fn drop(&mut self) {
                self.0.borrow_mut().push("drop-old");
            }
        }

        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut resource = Some(DropMarker(std::rc::Rc::clone(&events)));
        let replacement = recreate_after_input_flush(
            &mut resource,
            || {
                events.borrow_mut().push("flush-input");
                Ok::<_, std::convert::Infallible>(())
            },
            || {
                events.borrow_mut().push("start-new");
                Ok::<_, std::convert::Infallible>("replacement")
            },
        )
        .unwrap();

        assert_eq!(replacement, "replacement");
        assert!(resource.is_none());
        assert_eq!(*events.borrow(), ["flush-input", "drop-old", "start-new"]);
    }

    #[test]
    fn failed_post_editor_input_flush_keeps_old_resources_for_final_cleanup() {
        let mut resource = Some("old-terminal");
        let mut started = false;
        let error = recreate_after_input_flush(
            &mut resource,
            || Err::<(), _>("flush failed"),
            || {
                started = true;
                Ok("replacement")
            },
        )
        .unwrap_err();

        assert_eq!(error, "flush failed");
        assert!(!started);
        assert_eq!(resource, Some("old-terminal"));
    }

    #[test]
    fn pixel_refresh_requires_a_redraw_and_rearms_for_each_resize_in_the_burst() {
        let mut refresh = PixelRefresh::None;
        refresh.schedule_new();
        assert_eq!(refresh, PixelRefresh::AwaitingRedraw);
        assert!(!refresh.is_ready());

        refresh.observe_redraw();
        assert!(refresh.is_ready());

        refresh.schedule_after_resize();
        assert_eq!(refresh, PixelRefresh::AwaitingRedraw);
        assert!(!refresh.is_ready());
        refresh.observe_redraw();
        assert!(refresh.is_ready());

        refresh.clear();
        refresh.schedule_after_resize();
        assert_eq!(refresh, PixelRefresh::None);
    }

    #[test]
    fn post_enable_diagnostics_accept_only_set_and_classify_every_failure() {
        let initial = PixelCapabilitySnapshot::new(complete_replies(), 80, 24);
        let set = CapabilityReplies {
            pixel_mode: Some(ReportedPixelMode::Set),
            ..CapabilityReplies::default()
        };
        assert_eq!(
            classify_post_enable_verification(
                set,
                initial,
                PixelNegotiationStage::InitialPostEnable,
            ),
            Ok(())
        );

        assert!(matches!(
            classify_post_enable_verification(
                CapabilityReplies::default(),
                initial,
                PixelNegotiationStage::InitialPostEnable,
            ),
            Err(PixelFallbackReason::PostEnableModeTimeout { .. })
        ));

        for pixel_mode in [
            Some(ReportedPixelMode::NotRecognized),
            Some(ReportedPixelMode::Reset),
            Some(ReportedPixelMode::PermanentlySet),
            Some(ReportedPixelMode::PermanentlyReset),
        ] {
            assert!(matches!(
                classify_post_enable_verification(
                    CapabilityReplies {
                        pixel_mode,
                        ..CapabilityReplies::default()
                    },
                    initial,
                    PixelNegotiationStage::InitialPostEnable,
                ),
                Err(PixelFallbackReason::PostEnableModeNotSet { .. })
            ));
        }
        assert!(matches!(
            classify_post_enable_verification(
                CapabilityReplies {
                    resize_seen: true,
                    ..set
                },
                initial,
                PixelNegotiationStage::InitialPostEnable,
            ),
            Err(PixelFallbackReason::ResizeInterrupted {
                stage: PixelNegotiationStage::InitialPostEnable,
                ..
            })
        ));
    }

    #[test]
    fn fallback_trace_formats_identity_mode_metrics_reason_and_success() {
        let context = MouseTraceContext {
            term_program: Some("vscode".to_string()),
            term_program_version: Some("1.134.0".to_string()),
            pixel_origin: Some(PixelCoordinateOrigin::ZeroBased),
        };
        let mut replies = complete_replies();
        replies.area_pixels = Some((801, 480));
        let snapshot = PixelCapabilitySnapshot::new(replies, 80, 24);
        let diagnostic = MouseFallbackDiagnostic::new(
            &context,
            PixelFallbackReason::InconsistentMetrics {
                stage: PixelNegotiationStage::Initial,
                snapshot,
            },
            CellFallbackOutcome::Succeeded,
        );

        assert_eq!(
            diagnostic.format(),
            "reason=inconsistent-pixel-metrics; term_program=vscode; \
             term_program_version=1.134.0; origin_policy=accepted-zero-based; \
             stage=initial; reported_1016=reset; \
             terminal_cells=80x24; area_px=801x480; cell_px=10x20; \
             resize_seen=false; cells_fallback=success"
        );
    }

    #[test]
    fn fallback_trace_distinguishes_timeouts_reports_policy_and_cell_failure() {
        let context = MouseTraceContext {
            term_program: Some("vscode".to_string()),
            term_program_version: Some("1.134.0".to_string()),
            pixel_origin: Some(PixelCoordinateOrigin::ZeroBased),
        };
        let initial = PixelCapabilitySnapshot::new(complete_replies(), 80, 24);

        let timeout = MouseFallbackDiagnostic::new(
            &context,
            PixelFallbackReason::PostEnableModeTimeout {
                stage: PixelNegotiationStage::InitialPostEnable,
                initial,
            },
            CellFallbackOutcome::Succeeded,
        )
        .format();
        assert!(timeout.contains("reason=post-enable-decrqm-1016-timeout"));
        assert!(timeout.contains("post_enable_1016=timeout"));

        let not_set = MouseFallbackDiagnostic::new(
            &context,
            PixelFallbackReason::PostEnableModeNotSet {
                stage: PixelNegotiationStage::InitialPostEnable,
                reported: ReportedPixelMode::Reset,
                initial,
            },
            CellFallbackOutcome::Succeeded,
        )
        .format();
        assert!(not_set.contains("reason=post-enable-1016-report-not-set"));
        assert!(not_set.contains("post_enable_1016=reset"));

        let rejected_context = MouseTraceContext {
            pixel_origin: None,
            ..context.clone()
        };
        let policy = MouseFallbackDiagnostic::new(
            &rejected_context,
            PixelFallbackReason::OriginPolicyRejected,
            CellFallbackOutcome::Succeeded,
        )
        .format();
        assert!(policy.contains("reason=terminal-origin-policy-rejected"));
        assert!(policy.contains("origin_policy=rejected"));

        let failure = MouseFallbackDiagnostic::new(
            &rejected_context,
            PixelFallbackReason::OriginPolicyRejected,
            CellFallbackOutcome::Failed {
                error: "1006 setup failed".to_string(),
            },
        )
        .format();
        assert!(failure.contains("cells_fallback=failure"));
        assert!(failure.contains("cells_fallback_error=\"1006 setup failed\""));
    }

    #[test]
    fn deferred_retry_trace_distinguishes_pending_success_and_failure() {
        let context = MouseTraceContext {
            term_program: Some("vscode".to_string()),
            term_program_version: Some("1.134.0".to_string()),
            pixel_origin: Some(PixelCoordinateOrigin::ZeroBased),
        };
        let mut replies = complete_replies();
        replies.resize_seen = true;
        let reason = classify_initial(replies).unwrap_err();

        let pending =
            MouseFallbackDiagnostic::new(&context, reason.clone(), CellFallbackOutcome::Succeeded)
                .with_deferred_retry(DeferredRetryDiagnostic::Pending)
                .format();
        assert!(pending.contains("deferred_retry=pending-after-resize-redraw"));

        assert_eq!(
            MouseTraceDiagnostic::DeferredRetrySucceeded.format_line(),
            "atc terminal mouse negotiation: initial attempt interrupted by resize; deferred retry succeeded"
        );

        let failed = MouseTraceDiagnostic::Fallback(
            MouseFallbackDiagnostic::new(&context, reason, CellFallbackOutcome::Succeeded)
                .with_deferred_retry(DeferredRetryDiagnostic::Failed),
        )
        .format_line();
        assert!(failed.starts_with("atc terminal mouse fallback: reason="));
        assert!(failed.contains("deferred_retry=failed"));
    }

    #[test]
    fn metric_fallback_trace_preserves_missing_and_malformed_report_values() {
        let context = MouseTraceContext {
            term_program: Some("vscode".to_string()),
            term_program_version: Some("1.134.0".to_string()),
            pixel_origin: Some(PixelCoordinateOrigin::ZeroBased),
        };
        let mut area_missing = complete_replies();
        area_missing.area_pixels = None;
        let area_reason = classify_initial(area_missing).unwrap_err();
        let area_trace =
            MouseFallbackDiagnostic::new(&context, area_reason, CellFallbackOutcome::Succeeded)
                .format();
        assert!(area_trace.contains("reason=missing-text-area-pixel-response"));
        assert!(area_trace.contains("area_px=timeout"));
        assert!(area_trace.contains("cell_px=10x20"));

        let mut cell_missing = complete_replies();
        cell_missing.cell_pixels = None;
        let cell_reason = classify_initial(cell_missing).unwrap_err();
        let cell_trace =
            MouseFallbackDiagnostic::new(&context, cell_reason, CellFallbackOutcome::Succeeded)
                .format();
        assert!(cell_trace.contains("reason=missing-cell-pixel-response"));
        assert!(cell_trace.contains("area_px=800x480"));
        assert!(cell_trace.contains("cell_px=timeout"));

        let mut malformed = complete_replies();
        malformed.cell_pixels = Some((-1, 20));
        let malformed_reason = classify_initial(malformed).unwrap_err();
        let malformed_trace = MouseFallbackDiagnostic::new(
            &context,
            malformed_reason,
            CellFallbackOutcome::Succeeded,
        )
        .format();
        assert!(malformed_trace.contains("reason=malformed-pixel-metrics"));
        assert!(malformed_trace.contains("cell_px=-1x20"));
    }

    #[test]
    fn only_permanently_active_pixel_mode_makes_cell_fallback_unsafe() {
        for pixel_mode in [
            None,
            Some(ReportedPixelMode::NotRecognized),
            Some(ReportedPixelMode::Set),
            Some(ReportedPixelMode::Reset),
            Some(ReportedPixelMode::PermanentlyReset),
        ] {
            assert!(cell_fallback_is_safe(CapabilityReplies {
                pixel_mode,
                ..CapabilityReplies::default()
            }));
        }

        assert!(!cell_fallback_is_safe(CapabilityReplies {
            pixel_mode: Some(ReportedPixelMode::PermanentlySet),
            ..CapabilityReplies::default()
        }));
    }

    #[test]
    fn set_then_resize_can_fall_back_to_cells_and_arm_the_initial_retry() {
        let verification = CapabilityReplies {
            pixel_mode: Some(ReportedPixelMode::Set),
            resize_seen: true,
            ..CapabilityReplies::default()
        };
        let reason = classify_post_enable_verification(
            verification,
            PixelCapabilitySnapshot::new(complete_replies(), 80, 24),
            PixelNegotiationStage::InitialPostEnable,
        )
        .unwrap_err();

        assert!(reason.schedules_deferred_retry());
        assert!(cell_fallback_is_safe(verification));
    }

    #[test]
    fn transition_plans_keep_parser_and_protocol_changes_in_safe_order() {
        assert_eq!(
            CELLS_TRANSITION,
            [
                MouseTransitionStep::DisableTracking,
                MouseTransitionStep::DisablePixels,
                MouseTransitionStep::DisableCells,
                MouseTransitionStep::DrainInput,
                MouseTransitionStep::ParserCells,
                MouseTransitionStep::EnableCells,
                MouseTransitionStep::EnableTracking,
            ]
        );
        assert_eq!(
            PIXELS_TRANSITION_BEGIN,
            [
                MouseTransitionStep::DisableTracking,
                MouseTransitionStep::DisableCells,
                MouseTransitionStep::DisablePixels,
                MouseTransitionStep::DrainInput,
                MouseTransitionStep::ParserPixels,
                MouseTransitionStep::EnablePixels,
            ]
        );
        assert_eq!(
            DEFERRED_RETRY_BASELINE,
            [
                MouseTransitionStep::DisableTracking,
                MouseTransitionStep::DisableCells,
                MouseTransitionStep::DisablePixels,
                MouseTransitionStep::DrainInput,
                MouseTransitionStep::ParserCells,
            ]
        );
        assert_eq!(
            DISABLED_TRANSITION,
            [
                MouseTransitionStep::DisableTracking,
                MouseTransitionStep::DisableCells,
                MouseTransitionStep::DisablePixels,
                MouseTransitionStep::DrainInput,
                MouseTransitionStep::ParserPixels,
            ]
        );
    }

    #[test]
    fn deferred_retry_protocol_is_coherent_on_success_and_cell_fallback() {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum ParserMode {
            Cells,
            Pixels,
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        struct ProtocolState {
            tracking_1003: bool,
            cells_1006: bool,
            pixels_1016: bool,
            parser: ParserMode,
        }

        impl ProtocolState {
            fn apply(&mut self, step: MouseTransitionStep) {
                match step {
                    MouseTransitionStep::DisableTracking => self.tracking_1003 = false,
                    MouseTransitionStep::DisableCells => self.cells_1006 = false,
                    MouseTransitionStep::DisablePixels => self.pixels_1016 = false,
                    MouseTransitionStep::DrainInput => {}
                    MouseTransitionStep::ParserCells => self.parser = ParserMode::Cells,
                    MouseTransitionStep::ParserPixels => self.parser = ParserMode::Pixels,
                    MouseTransitionStep::EnableCells => self.cells_1006 = true,
                    MouseTransitionStep::EnablePixels => self.pixels_1016 = true,
                    MouseTransitionStep::EnableTracking => self.tracking_1003 = true,
                }
                assert!(!(self.cells_1006 && self.pixels_1016));
            }

            fn apply_all(&mut self, steps: &[MouseTransitionStep]) {
                for &step in steps {
                    self.apply(step);
                }
            }
        }

        let waiting = ProtocolState {
            tracking_1003: true,
            cells_1006: true,
            pixels_1016: false,
            parser: ParserMode::Cells,
        };

        let mut successful = waiting;
        successful.apply_all(DEFERRED_RETRY_BASELINE);
        assert_eq!(
            successful,
            ProtocolState {
                tracking_1003: false,
                cells_1006: false,
                pixels_1016: false,
                parser: ParserMode::Cells,
            }
        );
        successful.apply_all(PIXELS_TRANSITION_BEGIN);
        successful.apply(MouseTransitionStep::EnableTracking);
        assert_eq!(
            successful,
            ProtocolState {
                tracking_1003: true,
                cells_1006: false,
                pixels_1016: true,
                parser: ParserMode::Pixels,
            }
        );

        let candidate = classify_initial_pixel_candidate(
            complete_replies(),
            80,
            24,
            PixelCoordinateOrigin::ZeroBased,
            PixelNegotiationStage::DeferredRetry,
        )
        .unwrap();
        assert_eq!(
            classify_post_enable_verification(
                CapabilityReplies {
                    pixel_mode: Some(ReportedPixelMode::Set),
                    ..CapabilityReplies::default()
                },
                candidate.initial,
                PixelNegotiationStage::DeferredRetryPostEnable,
            ),
            Ok(())
        );
        assert!(matches!(
            pixel_mouse_mode(candidate, 7),
            MouseMode::Pixels { generation: 7, .. }
        ));

        let mut failed = waiting;
        failed.apply_all(DEFERRED_RETRY_BASELINE);
        failed.apply_all(CELLS_TRANSITION);
        assert_eq!(failed, waiting);
        assert_eq!(fallback_mouse_mode(true), MouseMode::Cells);
    }

    #[test]
    fn failed_cell_setup_stops_before_tracking_can_be_enabled() {
        let mut visited = Vec::new();
        let error = run_transition_plan(CELLS_TRANSITION, |step| {
            visited.push(step);
            if step == MouseTransitionStep::EnableCells {
                Err(io::Error::other("1006 setup failed"))
            } else {
                Ok(false)
            }
        })
        .unwrap_err();

        assert_eq!(error.to_string(), "1006 setup failed");
        assert_eq!(visited, CELLS_TRANSITION[..CELLS_TRANSITION.len() - 1]);
        assert!(!visited.contains(&MouseTransitionStep::EnableTracking));
    }

    #[test]
    fn resize_interrupted_negotiation_preserves_q_and_resize_in_order() {
        use termina::event::{
            KeyCode as TerminaKeyCode, KeyEvent as TerminaKeyEvent,
            KeyEventKind as TerminaKeyEventKind, KeyEventState, Modifiers as TerminaModifiers,
            MouseButton, MouseEvent, MouseEventKind,
        };

        let key = Event::Key(TerminaKeyEvent {
            code: TerminaKeyCode::Char('q'),
            kind: TerminaKeyEventKind::Press,
            modifiers: TerminaModifiers::NONE,
            state: KeyEventState::NONE,
        });
        let resize = Event::WindowResized(termina::WindowSize {
            cols: 100,
            rows: 40,
            pixel_width: None,
            pixel_height: None,
        });
        let mouse = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 2,
            modifiers: TerminaModifiers::NONE,
        });
        let mut replies = complete_replies();
        let mut pending = VecDeque::new();

        preserve_unrelated_query_event(key, &mut pending, &mut replies);
        preserve_unrelated_query_event(mouse, &mut pending, &mut replies);
        preserve_unrelated_query_event(resize, &mut pending, &mut replies);

        assert_eq!(pending.len(), 2);
        assert!(matches!(
            pending[0],
            TerminalEvent::Key(KeyEvent {
                code: KeyCode::Char('q'),
                kind: KeyEventKind::Press,
                ..
            })
        ));
        assert!(matches!(pending[1], TerminalEvent::Resize(_)));
        assert!(replies.resize_seen);
        let interrupted = classify_initial(replies).unwrap_err();
        assert!(interrupted.schedules_deferred_retry());
    }

    #[test]
    fn own_metric_replies_include_unusable_values_and_never_become_ignored_events() {
        let malformed = Event::Csi(Csi::Window(Box::new(
            Window::ReportCellSizePixelsResponse {
                width: None,
                height: Some(20),
            },
        )));
        assert_eq!(
            capability_reply(&malformed),
            Some(CapabilityReply::CellPixels {
                width: -1,
                height: 20,
            })
        );
        assert!(is_internal_query_reply(&malformed));
    }

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
        state.activate(LifecycleStep::AnyEventMouse);
        state.activate(LifecycleStep::PendingInput);
        let mut first_pass = Vec::new();

        cleanup_lifecycle(&mut state, |step| {
            first_pass.push(step);
            if step == LifecycleStep::AnyEventMouse {
                Err(io::Error::other("1003 reset failed"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert_eq!(
            first_pass,
            [LifecycleStep::AnyEventMouse, LifecycleStep::PendingInput]
        );
        assert!(state.is_active(LifecycleStep::AnyEventMouse));
        assert!(state.is_active(LifecycleStep::PendingInput));

        let mut retry = Vec::new();
        cleanup_lifecycle(&mut state, |step| {
            retry.push(step);
            Ok(())
        })
        .unwrap();

        assert_eq!(
            retry,
            [LifecycleStep::AnyEventMouse, LifecycleStep::PendingInput]
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
    fn panic_cleanup_resets_pixel_and_bracketed_paste_modes_without_enabling_them() {
        let cleanup = panic_cleanup_sequence();

        assert!(cleanup.contains("\u{1b}[?1003l"));
        assert!(!cleanup.contains("\u{1b}[?1003h"));
        assert!(cleanup.contains("\u{1b}[?1016l"));
        assert!(!cleanup.contains("\u{1b}[?1016h"));
        assert!(cleanup.contains("\u{1b}[?2004l"));
        assert!(!cleanup.contains("\u{1b}[?2004h"));
        assert!(
            CLEANUP_STEPS
                .iter()
                .position(|step| *step == LifecycleStep::BracketedPaste)
                .unwrap()
                < CLEANUP_STEPS
                    .iter()
                    .position(|step| *step == LifecycleStep::PendingInput)
                    .unwrap()
        );
    }
}
