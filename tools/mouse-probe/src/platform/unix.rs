use std::io::{self, Read};
use std::mem::MaybeUninit;
use std::time::Duration;

pub(crate) struct TerminalState {
    original: libc::termios,
    restored: bool,
}

impl TerminalState {
    pub(crate) fn enter() -> io::Result<Self> {
        let mut original = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `original` points to writable storage for a termios value.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: tcgetattr succeeded and initialized the complete termios value.
        let original = unsafe { original.assume_init() };
        let mut active = original;

        // Preserve every setting except canonical line buffering and echo.
        active.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL);
        active.c_cc[libc::VMIN] = 1;
        active.c_cc[libc::VTIME] = 0;

        // SAFETY: stdin is a terminal fd, and `active` is a valid termios value.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &active) } == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            original,
            restored: false,
        })
    }

    pub(crate) fn read_with_timeout<R: Read>(
        &self,
        input: &mut R,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> io::Result<Option<usize>> {
        let mut descriptor = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: `descriptor` points to one initialized pollfd for the duration
        // of the call, and stdin remains open while the probe is running.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result == -1 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(None)
            } else {
                Err(error)
            };
        }
        if result == 0 || descriptor.revents & libc::POLLIN == 0 {
            return Ok(None);
        }
        input.read(buffer).map(Some)
    }

    pub(crate) fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        // Mark restored after the attempt so Drop does not repeatedly alter a
        // terminal whose state may have changed after an explicit restore error.
        let result =
            // SAFETY: `original` is the exact termios value returned by tcgetattr.
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original) };
        self.restored = true;
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
