use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
#[cfg(test)]
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

    pub fn next_batch_timeout_with_cancel(
        &self,
        timeout: Duration,
        is_cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Option<Vec<PathBuf>>> {
        receive_next_batch_timeout_with_cancel(&self.rx, DEBOUNCE_DURATION, timeout, is_cancelled)
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

        let pending = collect_batch_ordered(rx, first, debounce_duration, None)?;

        if !pending.is_empty() {
            let mut paths = pending;
            paths.sort();
            return Ok(paths);
        }
    }
}

#[cfg(test)]
fn receive_next_batch_timeout(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    debounce_duration: Duration,
    timeout: Duration,
) -> io::Result<Option<Vec<PathBuf>>> {
    let first = match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "filesystem watcher disconnected",
            ));
        }
    };

    let paths = collect_batch_ordered(
        rx,
        first,
        debounce_duration,
        Some(Instant::now() + debounce_duration),
    )?;
    Ok(Some(paths))
}

fn receive_next_batch_timeout_with_cancel(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    debounce_duration: Duration,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<Vec<PathBuf>>> {
    if is_cancelled() {
        return Ok(None);
    }

    let first = match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "filesystem watcher disconnected",
            ));
        }
    };

    let mut pending = Vec::new();
    collect_result_ordered(first, &mut pending)?;

    let deadline = Instant::now() + debounce_duration;
    let cancel_poll_interval = if timeout.is_zero() {
        debounce_duration
    } else {
        timeout.min(debounce_duration)
    };

    loop {
        if is_cancelled() {
            return Ok(None);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(Some(pending));
        }

        match rx.recv_timeout(remaining.min(cancel_poll_interval)) {
            Ok(result) => collect_result_ordered(result, &mut pending)?,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "filesystem watcher disconnected",
                ));
            }
        }
    }
}

fn collect_batch_ordered(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    first: notify::Result<Event>,
    debounce_duration: Duration,
    deadline: Option<Instant>,
) -> io::Result<Vec<PathBuf>> {
    let mut pending = Vec::new();
    collect_result_ordered(first, &mut pending)?;

    loop {
        let wait = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(debounce_duration)
            .min(debounce_duration);
        if wait.is_zero() {
            break;
        }

        match rx.recv_timeout(wait) {
            Ok(result) => collect_result_ordered(result, &mut pending)?,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "filesystem watcher disconnected",
                ));
            }
        }
    }

    Ok(pending)
}

fn collect_result_ordered(
    result: notify::Result<Event>,
    pending: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let event = result.map_err(io::Error::other)?;

    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return Ok(());
    }

    for path in event.paths {
        if let Some(position) = pending.iter().position(|existing| existing == &path) {
            pending.remove(position);
        }
        pending.push(path);
    }

    Ok(())
}

#[cfg(test)]
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
    use std::cell::Cell;

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

    #[test]
    fn timeout_wait_returns_none_without_an_event() {
        let (_tx, rx) = mpsc::channel::<notify::Result<Event>>();

        let batch =
            receive_next_batch_timeout(&rx, Duration::from_millis(1), Duration::ZERO).unwrap();

        assert!(batch.is_none());
    }

    #[test]
    fn cancellation_interrupts_an_active_debounce_without_returning_a_partial_batch() {
        let (tx, rx) = mpsc::channel();
        tx.send(event(
            EventKind::Modify(ModifyKind::Any),
            Path::new("A.cpp"),
        ))
        .unwrap();
        let checks = Cell::new(0);

        let batch = receive_next_batch_timeout_with_cancel(
            &rx,
            Duration::from_secs(1),
            Duration::from_millis(20),
            &|| {
                let next = checks.get() + 1;
                checks.set(next);
                next >= 2
            },
        )
        .unwrap();

        assert!(batch.is_none());
        assert_eq!(checks.get(), 2);
    }

    #[test]
    fn timeout_wait_preserves_the_order_of_each_paths_last_event() {
        let (tx, rx) = mpsc::channel();
        let a = PathBuf::from("A.cpp");
        let b = PathBuf::from("B.py");
        tx.send(event(EventKind::Modify(ModifyKind::Any), &b))
            .unwrap();
        tx.send(event(EventKind::Create(CreateKind::Any), &a))
            .unwrap();
        tx.send(event(EventKind::Modify(ModifyKind::Any), &b))
            .unwrap();

        let batch = receive_next_batch_timeout(&rx, Duration::from_millis(1), Duration::ZERO)
            .unwrap()
            .unwrap();

        assert_eq!(batch, [a, b]);
    }
}
