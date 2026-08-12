use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

const DEBOUNCE_DURATION: Duration = Duration::from_millis(150);

pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<Event>>,
}

impl FileWatcher {
    pub fn new(directory: &Path) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |result| {
            let _ = tx.send(result);
        })
        .map_err(io::Error::other)?;

        watcher
            .watch(directory, RecursiveMode::NonRecursive)
            .map_err(io::Error::other)?;

        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    pub fn next_batch(&self) -> io::Result<Vec<PathBuf>> {
        loop {
            let first = self.rx.recv().map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "filesystem watcher disconnected")
            })?;

            let mut pending = HashSet::new();

            collect_result(first, &mut pending)?;

            loop {
                match self.rx.recv_timeout(DEBOUNCE_DURATION) {
                    Ok(result) => {
                        collect_result(result, &mut pending)?;
                    }

                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        break;
                    }

                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "filesystem watcher disconnected",
                        ));
                    }
                }
            }

            if !pending.is_empty() {
                let mut paths: Vec<_> = pending.into_iter().collect();
                paths.sort();
                return Ok(paths);
            }
        }
    }
}

fn collect_result(result: notify::Result<Event>, pending: &mut HashSet<PathBuf>) -> io::Result<()> {
    let event = result.map_err(io::Error::other)?;

    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return Ok(());
    }

    pending.extend(event.paths);

    Ok(())
}
