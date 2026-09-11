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
    path_mapper: EventPathMapper,
}

impl FileWatcher {
    pub fn new(directory: &Path) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |result| {
            let _ = tx.send(result);
        })
        .map_err(io::Error::other)?;

        let watch_directory = watch_directory(directory)?;
        watcher
            .watch(&watch_directory, RecursiveMode::NonRecursive)
            .map_err(io::Error::other)?;

        Ok(Self {
            _watcher: watcher,
            rx,
            path_mapper: EventPathMapper::new(watch_directory, directory.to_path_buf()),
        })
    }

    pub fn next_batch(&self) -> io::Result<Vec<PathBuf>> {
        receive_next_batch(&self.rx, DEBOUNCE_DURATION, &self.path_mapper)
    }

    pub fn next_batch_timeout_with_cancel(
        &self,
        timeout: Duration,
        is_cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Option<Vec<PathBuf>>> {
        receive_next_batch_timeout_with_cancel(
            &self.rx,
            DEBOUNCE_DURATION,
            timeout,
            is_cancelled,
            &self.path_mapper,
        )
    }
}

#[cfg(target_os = "macos")]
fn watch_directory(directory: &Path) -> io::Result<PathBuf> {
    directory.canonicalize()
}

#[cfg(not(target_os = "macos"))]
fn watch_directory(directory: &Path) -> io::Result<PathBuf> {
    Ok(directory.to_path_buf())
}

struct EventPathMapper {
    observed_root: PathBuf,
    reported_root: PathBuf,
}

impl EventPathMapper {
    fn new(observed_root: PathBuf, reported_root: PathBuf) -> Self {
        Self {
            observed_root,
            reported_root,
        }
    }

    fn map(&self, path: PathBuf) -> PathBuf {
        if self.observed_root == self.reported_root {
            return path;
        }
        match path.strip_prefix(&self.observed_root) {
            Ok(suffix) => self.reported_root.join(suffix),
            Err(_) => path,
        }
    }
}

fn receive_next_batch(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    debounce_duration: Duration,
    path_mapper: &EventPathMapper,
) -> io::Result<Vec<PathBuf>> {
    loop {
        let first = rx.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "filesystem watcher disconnected")
        })?;

        let pending = collect_batch_ordered(rx, first, debounce_duration, None, path_mapper)?;

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
    path_mapper: &EventPathMapper,
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
        path_mapper,
    )?;
    Ok(Some(paths))
}

fn receive_next_batch_timeout_with_cancel(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    debounce_duration: Duration,
    timeout: Duration,
    is_cancelled: &dyn Fn() -> bool,
    path_mapper: &EventPathMapper,
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
    collect_result_ordered(first, &mut pending, path_mapper)?;

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
            Ok(result) => collect_result_ordered(result, &mut pending, path_mapper)?,
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
    path_mapper: &EventPathMapper,
) -> io::Result<Vec<PathBuf>> {
    let mut pending = Vec::new();
    collect_result_ordered(first, &mut pending, path_mapper)?;

    loop {
        let wait = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(debounce_duration)
            .min(debounce_duration);
        if wait.is_zero() {
            break;
        }

        match rx.recv_timeout(wait) {
            Ok(result) => collect_result_ordered(result, &mut pending, path_mapper)?,
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
    path_mapper: &EventPathMapper,
) -> io::Result<()> {
    let event = result.map_err(io::Error::other)?;

    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return Ok(());
    }

    for path in event.paths {
        let path = path_mapper.map(path);
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

    fn identity_mapper() -> EventPathMapper {
        EventPathMapper::new(PathBuf::new(), PathBuf::new())
    }

    #[test]
    fn maps_observed_backend_root_back_to_reported_root() {
        let mapper = EventPathMapper::new(
            PathBuf::from("/private/var/folders/workspace"),
            PathBuf::from("/var/folders/workspace"),
        );
        let source = PathBuf::from("/private/var/folders/workspace/A.py");
        let unrelated = PathBuf::from("/private/var/other/A.py");
        let mut pending = Vec::new();

        collect_result_ordered(
            event(EventKind::Modify(ModifyKind::Any), &source),
            &mut pending,
            &mapper,
        )
        .unwrap();
        collect_result_ordered(
            event(EventKind::Modify(ModifyKind::Any), &unrelated),
            &mut pending,
            &mapper,
        )
        .unwrap();

        assert_eq!(
            pending,
            [PathBuf::from("/var/folders/workspace/A.py"), unrelated]
        );
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

        let batch = receive_next_batch(&rx, Duration::from_millis(1), &identity_mapper()).unwrap();

        assert_eq!(batch, [a, b]);
    }

    #[test]
    fn disconnected_watcher_is_a_broken_pipe_error() {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        drop(tx);

        let error =
            receive_next_batch(&rx, Duration::from_millis(1), &identity_mapper()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn timeout_wait_returns_none_without_an_event() {
        let (_tx, rx) = mpsc::channel::<notify::Result<Event>>();

        let batch = receive_next_batch_timeout(
            &rx,
            Duration::from_millis(1),
            Duration::ZERO,
            &identity_mapper(),
        )
        .unwrap();

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
            &identity_mapper(),
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

        let batch = receive_next_batch_timeout(
            &rx,
            Duration::from_millis(1),
            Duration::ZERO,
            &identity_mapper(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(batch, [a, b]);
    }
}
