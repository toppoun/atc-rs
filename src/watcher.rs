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
        receive_next_batch(&self.rx, DEBOUNCE_DURATION)
    }
}

fn receive_next_batch(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    debounce_duration: Duration,
) -> io::Result<Vec<PathBuf>> {
    loop {
        let first = rx.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "filesystem watcher disconnected")
        })?;

        let mut pending = HashSet::new();

        collect_result(first, &mut pending)?;

        loop {
            match rx.recv_timeout(debounce_duration) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

    fn event(kind: EventKind, path: &Path) -> notify::Result<Event> {
        Ok(Event::new(kind).add_path(path.to_path_buf()))
    }

    #[test]
    fn collects_create_modify_and_remove_but_ignores_access() {
        let mut pending = HashSet::new();
        let created = PathBuf::from("created.cpp");
        let modified = PathBuf::from("modified.cpp");
        let removed = PathBuf::from("removed.cpp");
        let accessed = PathBuf::from("accessed.cpp");

        collect_result(
            event(EventKind::Create(CreateKind::Any), &created),
            &mut pending,
        )
        .unwrap();
        collect_result(
            event(EventKind::Modify(ModifyKind::Any), &modified),
            &mut pending,
        )
        .unwrap();
        collect_result(
            event(EventKind::Remove(RemoveKind::Any), &removed),
            &mut pending,
        )
        .unwrap();
        collect_result(
            event(EventKind::Access(AccessKind::Any), &accessed),
            &mut pending,
        )
        .unwrap();

        assert_eq!(pending.len(), 3);
        assert!(pending.contains(&created));
        assert!(pending.contains(&modified));
        assert!(pending.contains(&removed));
        assert!(!pending.contains(&accessed));
    }

    #[test]
    fn collects_the_same_path_only_once_per_batch() {
        let mut pending = HashSet::new();
        let path = PathBuf::from("A.cpp");

        collect_result(
            event(EventKind::Create(CreateKind::Any), &path),
            &mut pending,
        )
        .unwrap();
        collect_result(
            event(EventKind::Modify(ModifyKind::Any), &path),
            &mut pending,
        )
        .unwrap();
        collect_result(
            event(EventKind::Remove(RemoveKind::Any), &path),
            &mut pending,
        )
        .unwrap();

        assert_eq!(pending, HashSet::from([path]));
    }

    #[test]
    fn debounce_returns_a_sorted_deduplicated_batch_and_skips_access_events() {
        let (tx, rx) = mpsc::channel();
        let a = PathBuf::from("A.cpp");
        let b = PathBuf::from("B.py");

        tx.send(event(EventKind::Access(AccessKind::Any), &a))
            .unwrap();
        tx.send(event(EventKind::Modify(ModifyKind::Any), &b))
            .unwrap();
        tx.send(event(EventKind::Create(CreateKind::Any), &a))
            .unwrap();
        tx.send(event(EventKind::Modify(ModifyKind::Any), &b))
            .unwrap();

        let batch = receive_next_batch(&rx, Duration::from_millis(1)).unwrap();

        assert_eq!(batch, [a, b]);
    }

    #[test]
    fn disconnected_watcher_is_a_broken_pipe_error() {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        drop(tx);

        let error = receive_next_batch(&rx, Duration::from_millis(1)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
