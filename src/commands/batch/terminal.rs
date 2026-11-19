use super::super::migrate::read_migration_confirmation;
use crate::ui::{Event, Reporter, write_contest_update_event};
use crate::workspace::update_cancellation::UpdateCancellation;
use std::cell::Cell;
use std::fmt;
use std::io::{self, Write};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

const INTERRUPT_POLL: Duration = Duration::from_millis(25);

struct WriteRequest {
    bytes: Vec<u8>,
    done: Sender<io::Result<()>>,
}

/// Standard handles can block independently of console control events. Only the worker
/// uses stderr, and the command never joins a worker stalled in an OS I/O call.
#[derive(Clone)]
pub(super) struct BatchOutput<'a> {
    writer: Sender<WriteRequest>,
    unavailable: Rc<Cell<bool>>,
    cancellation: UpdateCancellation<'a>,
}

impl<'a> BatchOutput<'a> {
    pub(super) fn new(cancellation: UpdateCancellation<'a>) -> io::Result<Self> {
        let (writer, requests) = mpsc::channel::<WriteRequest>();
        thread::Builder::new()
            .name("batch-output".into())
            .spawn(move || {
                let mut stderr = io::stderr();
                for request in requests {
                    let result = stderr
                        .write_all(&request.bytes)
                        .and_then(|()| stderr.flush());
                    let failed = result.is_err();
                    if request.done.send(result).is_err() || failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            writer,
            unavailable: Rc::default(),
            cancellation,
        })
    }

    fn send(&self, bytes: Vec<u8>) -> io::Result<()> {
        if self.unavailable.get() {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        let (done, result) = mpsc::channel();
        self.writer
            .send(WriteRequest { bytes, done })
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        let result = wait_for_io(result, self.cancellation);
        self.unavailable.set(result.is_err());
        result
    }
}

impl Write for BatchOutput<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.send(bytes.to_vec())?;
        Ok(bytes.len())
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> io::Result<()> {
        self.send(args.to_string().into_bytes())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Each write is acknowledged only after stderr is flushed.
        Ok(())
    }
}

impl Reporter for BatchOutput<'_> {
    fn report(&mut self, event: Event<'_>) {
        let _ = write_contest_update_event(self, &event);
    }
}

pub(super) fn wait_for_approval(cancellation: UpdateCancellation<'_>) -> io::Result<bool> {
    let (done, answer) = mpsc::channel();
    thread::Builder::new()
        .name("batch-approval".into())
        .spawn(move || {
            let _ = done.send(read_migration_confirmation(&mut io::stdin().lock()));
        })?;
    wait_for_io(answer, cancellation)
}

fn wait_for_io<T>(
    result: Receiver<io::Result<T>>,
    cancellation: UpdateCancellation<'_>,
) -> io::Result<T> {
    loop {
        match result.recv_timeout(INTERRUPT_POLL) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            Err(mpsc::RecvTimeoutError::Timeout) if cancellation.is_requested() => {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
