use super::{DEBOUNCE_DURATION, WorkspaceFileWatcher, WorkspaceRegistration};
use notify::{ErrorKind, PathOp, RecursiveMode, UpdatePathsError, WatchPathConfig, Watcher};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

type Registrations = BTreeMap<PathBuf, WorkspaceRegistration>;

impl WorkspaceFileWatcher {
    pub(crate) fn replace_registrations(
        &mut self,
        installed: &mut Registrations,
        desired: Registrations,
        stop: &AtomicBool,
        mut report: impl FnMut(String),
    ) -> io::Result<()> {
        if *installed == desired || stop.load(Ordering::Acquire) {
            return Ok(());
        }
        // Re-register on an execution generation change even if the directory's inode matches.
        // Removing the old entry first gives notify a new per-watch event ID cutoff.
        let removed = installed
            .iter()
            .filter(|(path, current)| desired.get(*path) != Some(*current))
            .map(|(path, _)| PathOp::unwatch(path.clone()));
        let added: Vec<_> = desired
            .iter()
            .filter(|(path, next)| installed.get(*path) != Some(*next))
            .collect();
        let operations: Vec<_> = removed
            .chain(
                added
                    .iter()
                    .map(|(path, registration)| watch(path, registration)),
            )
            .collect();
        match self.apply_paths(operations) {
            Ok(()) => *installed = desired,
            Err(error) => {
                report(format!("update FSEvents registrations: {}", error.source));
                if matches!(error.source.kind, ErrorKind::MaxFilesWatch)
                    && error.origin.is_some()
                    && error.remaining.len() + 1 == added.len()
                    && installed
                        .iter()
                        .all(|(path, old)| desired.get(path) == Some(old))
                    && !installed.is_empty()
                {
                    // The first addition exceeded the backend's limit. With no earlier
                    // removals or additions, notify keeps the confirmed stream running.
                    self.request_recovery();
                    return Ok(());
                }

                let mut fallback = desired.clone();
                match (&error.origin, &error.source.kind) {
                    (Some(PathOp::Watch(path, _)), ErrorKind::MaxFilesWatch) => {
                        // Keep confirmed paths and only retry additions that fit before the limit.
                        let prefix = added.len() - error.remaining.len() - 1;
                        fallback.retain(|path, next| {
                            installed.get(path) == Some(next)
                                || added[..prefix].iter().any(|(added, _)| *added == path)
                        });
                        debug_assert!(!fallback.contains_key(path));
                    }
                    (Some(PathOp::Watch(path, _)), _) => {
                        fallback.remove(path);
                    }
                    (None, _) => fallback.retain(|path, next| installed.get(path) == Some(next)),
                    _ => {}
                }
                // An error for a path operation can hide a second error while starting the
                // updated stream. A fresh watcher must acknowledge the entire fallback set.
                installed.clear();
                self.reset_backend()?;
                if !fallback.is_empty() && !stop.load(Ordering::Acquire) {
                    match self.apply_paths(
                        fallback
                            .iter()
                            .map(|(path, next)| watch(path, next))
                            .collect(),
                    ) {
                        Ok(()) => *installed = fallback,
                        Err(error) => {
                            report(format!("restore FSEvents registrations: {}", error.source));
                            self.reset_backend()?;
                        }
                    }
                }
            }
        }
        self.request_recovery();
        Ok(())
    }

    fn reset_backend(&mut self) -> io::Result<()> {
        self.watcher = Self::backend(self.inbox.clone())?;
        Ok(())
    }

    fn apply_paths(&mut self, operations: Vec<PathOp>) -> Result<(), Box<UpdatePathsError>> {
        #[cfg(test)]
        {
            self.applied_batches += 1;
        }
        #[cfg(test)]
        if std::mem::take(&mut self.failed_start) {
            return Err(Box::new(UpdatePathsError::new(
                notify::Error::new(ErrorKind::WatchStartFailed),
                None,
                Vec::new(),
            )));
        }
        self.watcher.update_paths(operations).map_err(Box::new)
    }

    fn request_recovery(&self) {
        let mut inbox = self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Give delivered notifications the existing debounce window before comparing
        // fingerprints. Recovery itself carries no notification order.
        inbox.recover = true;
        inbox
            .deadline
            .get_or_insert(Instant::now() + DEBOUNCE_DURATION);
    }
}

fn watch(path: &Path, registration: &WorkspaceRegistration) -> PathOp {
    // Native FSEvents is recursive. Let notify cover same-device descendants with the
    // parent's stream root instead of holding descriptors for every registered directory.
    // WorkspaceInbox still accepts only the explicitly indexed paths and topology children.
    let mode = if registration.anchor {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    let mut config = WatchPathConfig::new(mode);
    if let Some(epoch) = registration.epoch {
        config = config.with_user_data(epoch);
    }
    PathOp::Watch(path.to_path_buf(), config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::{WatchInterests, watch_directory};
    use crate::workspace::contest_directory_identity;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    fn dir(path: PathBuf, epoch: u64) -> (PathBuf, WorkspaceRegistration) {
        fs::create_dir(&path).unwrap();
        let path = watch_directory(&path).unwrap();
        let mut registration =
            WorkspaceRegistration::new(contest_directory_identity(&path).unwrap());
        registration.epoch = Some(epoch);
        (path, registration)
    }

    fn replace(
        watcher: &mut WorkspaceFileWatcher,
        installed: &mut Registrations,
        desired: Registrations,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        watcher
            .replace_registrations(installed, desired, &AtomicBool::new(false), |error| {
                errors.push(error);
            })
            .unwrap();
        errors
    }

    #[test]
    fn batches_initial_and_changed_sets_and_skips_unchanged_sets() {
        let root = tempfile::tempdir().unwrap();
        let a = dir(root.path().join("a"), 1);
        let b = dir(root.path().join("b"), 2);
        let c = dir(root.path().join("c"), 3);
        let d = dir(root.path().join("d"), 4);
        let mut watcher = WorkspaceFileWatcher::new().unwrap();
        let mut installed = Registrations::new();
        let initial = Registrations::from([a.clone(), b.clone()]);
        assert!(replace(&mut watcher, &mut installed, initial.clone()).is_empty());
        assert_eq!(watcher.applied_batches, 1);
        assert!(replace(&mut watcher, &mut installed, initial).is_empty());
        assert_eq!(watcher.applied_batches, 1);
        let desired = Registrations::from([c.clone(), d.clone()]);
        assert!(replace(&mut watcher, &mut installed, desired.clone()).is_empty());
        assert_eq!(watcher.applied_batches, 2);
        assert!(installed == desired);
        let stop = AtomicBool::new(true);
        watcher
            .replace_registrations(&mut installed, Registrations::new(), &stop, |_| panic!())
            .unwrap();
        assert_eq!(watcher.applied_batches, 2);
        assert!(installed == desired);
    }

    #[test]
    fn failed_start_is_not_committed_and_old_registrations_can_be_restored() {
        let root = tempfile::tempdir().unwrap();
        let a = dir(root.path().join("a"), 1);
        let b = dir(root.path().join("b"), 2);
        let mut watcher = WorkspaceFileWatcher::new().unwrap();
        let mut installed = Registrations::new();
        let initial = Registrations::from([a.clone()]);
        watcher.failed_start = true;
        assert!(!replace(&mut watcher, &mut installed, initial.clone()).is_empty());
        assert!(installed.is_empty());
        assert!(watcher.watcher.watched_paths().unwrap().is_empty());
        assert!(replace(&mut watcher, &mut installed, initial.clone()).is_empty());
        watcher.failed_start = true;
        let desired = Registrations::from([a.clone(), b.clone()]);
        assert!(!replace(&mut watcher, &mut installed, desired.clone()).is_empty());
        assert!(installed == initial);
        assert_eq!(watcher.watcher.watched_paths().unwrap()[0].path, a.0);
        assert!(replace(&mut watcher, &mut installed, desired.clone()).is_empty());
        assert!(installed == desired);
    }

    #[test]
    fn a_native_path_error_can_restore_confirmed_healthy_registrations() {
        let root = tempfile::tempdir().unwrap();
        let a = dir(root.path().join("a"), 1);
        let b = dir(root.path().join("b"), 2);
        let c = dir(root.path().join("c"), 3);
        fs::remove_dir(&b.0).unwrap();
        let source = a.0.join("A.py");
        let mut watcher = WorkspaceFileWatcher::new().unwrap();
        let mut installed = Registrations::new();
        let mut interests = WatchInterests::default();
        interests.paths.insert(source.clone(), Some(1));
        watcher.set_interests(interests);
        let desired = Registrations::from([a.clone(), b, c.clone()]);
        assert!(!replace(&mut watcher, &mut installed, desired).is_empty());
        assert!(installed == Registrations::from([a, c]));
        fs::write(&source, "pass\n").unwrap();
        let timeout = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(batch) = watcher.take_ready()
                && !batch.paths.is_empty()
            {
                assert_eq!(batch.paths, [(source, 1)]);
                break;
            }
            assert!(
                Instant::now() < timeout,
                "healthy directory lost its notification"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn physical_ancestor_coverage_preserves_nonrecursive_source_registration() {
        let root = tempfile::tempdir().unwrap();
        let root = watch_directory(root.path()).unwrap();
        let a = dir(root.join("a"), 1);
        let nested = dir(a.0.join("nested"), 2);
        let source = a.0.join("A.py");
        let outside_logical_watch = nested.0.join("A.py");
        let ancestor =
            WorkspaceRegistration::for_parent(contest_directory_identity(&root).unwrap());
        let mut watcher = WorkspaceFileWatcher::new().unwrap();
        let mut installed = Registrations::new();
        let mut interests = WatchInterests::default();
        interests.paths.insert(source.clone(), Some(1));
        interests
            .paths
            .insert(outside_logical_watch.clone(), Some(1));
        watcher.set_interests(interests);
        assert!(
            replace(
                &mut watcher,
                &mut installed,
                Registrations::from([(root, ancestor), a])
            )
            .is_empty()
        );
        fs::write(&outside_logical_watch, "pass\n").unwrap();
        fs::write(&source, "pass\n").unwrap();
        let timeout = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(batch) = watcher.take_ready()
                && !batch.paths.is_empty()
            {
                assert_eq!(batch.paths, [(source, 1)]);
                break;
            }
            assert!(
                Instant::now() < timeout,
                "registered directory lost its notification"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
