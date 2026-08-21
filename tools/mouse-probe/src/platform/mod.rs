#[cfg(target_os = "macos")]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
pub(crate) use unix::TerminalState;
#[cfg(windows)]
pub(crate) use windows::TerminalState;

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("mouse-probe Phase 0 supports only Windows and macOS");
