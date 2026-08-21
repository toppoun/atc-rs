use std::io::{self, Read};
use std::time::Duration;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub(crate) struct TerminalState {
    input_handle: HANDLE,
    output_handle: HANDLE,
    original_input_mode: u32,
    original_output_mode: u32,
    restored: bool,
}

impl TerminalState {
    pub(crate) fn enter() -> io::Result<Self> {
        let input_handle = get_std_handle(STD_INPUT_HANDLE)?;
        let output_handle = get_std_handle(STD_OUTPUT_HANDLE)?;
        let original_input_mode = get_console_mode(input_handle)?;
        let original_output_mode = get_console_mode(output_handle)?;

        set_console_mode(
            output_handle,
            original_output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        )?;

        let input_mode =
            (original_input_mode | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_EXTENDED_FLAGS)
                & !(ENABLE_ECHO_INPUT
                    | ENABLE_LINE_INPUT
                    | ENABLE_PROCESSED_INPUT
                    | ENABLE_QUICK_EDIT_MODE);
        if let Err(error) = set_console_mode(input_handle, input_mode) {
            let _ = set_console_mode(output_handle, original_output_mode);
            return Err(error);
        }

        Ok(Self {
            input_handle,
            output_handle,
            original_input_mode,
            original_output_mode,
            restored: false,
        })
    }

    pub(crate) fn read_with_timeout<R: Read>(
        &self,
        input: &mut R,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> io::Result<Option<usize>> {
        if !wait_for_input(self.input_handle, timeout)? {
            return Ok(None);
        }
        input.read(buffer).map(Some)
    }

    pub(crate) fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }

        let mut first_error = None;
        if let Err(error) = set_console_mode(self.input_handle, self.original_input_mode) {
            first_error = Some(error);
        }
        if let Err(error) = set_console_mode(self.output_handle, self.original_output_mode)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        self.restored = true;

        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn get_std_handle(kind: u32) -> io::Result<HANDLE> {
    // SAFETY: GetStdHandle has no pointer arguments and returns a borrowed OS handle.
    let handle = unsafe { GetStdHandle(kind) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

fn get_console_mode(handle: HANDLE) -> io::Result<u32> {
    let mut mode = 0;
    // SAFETY: `mode` is a valid out pointer and `handle` came from GetStdHandle.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(mode)
    }
}

fn set_console_mode(handle: HANDLE, mode: u32) -> io::Result<()> {
    // SAFETY: `handle` came from GetStdHandle and `mode` is a bitmask value.
    if unsafe { SetConsoleMode(handle, mode) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wait_for_input(handle: HANDLE, timeout: Duration) -> io::Result<bool> {
    let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
    // SAFETY: `handle` remains valid for the process lifetime; this only waits on it.
    let result = unsafe { WaitForSingleObject(handle, timeout_ms) };
    if result == WAIT_OBJECT_0 {
        Ok(true)
    } else if result == WAIT_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(false)
    }
}
