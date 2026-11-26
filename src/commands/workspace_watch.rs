use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::RunnerConfig;
use crate::language::Language;
use crate::model::Contest;
use crate::safe_file::WatchedFileStamp;
use crate::tui::message::{
    Message, RunGeneration, RunKind, RunRequest, RunResults, RunTarget, RunWorkerCommand,
};
use crate::watcher::{
    self, WatchInterests, WorkspaceBatch, WorkspaceFileWatcher, WorkspaceRegistration,
};
use crate::workspace::batch::BatchWorkspace;
use crate::workspace::diagnosis::{ContestDiagnosis, diagnose_watch_contest};
use crate::workspace::update_cancellation::UpdateCancellation;
use crate::workspace::{self, ExplicitContestRoute, PhysicalDirectoryIdentity, WatchManifestStamp};

use super::attempt_executor::RunContext;
use super::watch_source::build_watched_sources;
use super::watch_worker::RunWorker;

const POLL: Duration = Duration::from_millis(20);
const MANIFEST_POLL: Duration = Duration::from_secs(1);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

pub(super) struct WorkspaceWatcher {
    stop: Arc<AtomicBool>,
    commands: mpsc::SyncSender<Command>,
    handle: Option<JoinHandle<io::Result<()>>>,
    status: Arc<Mutex<WatchStatus>>,
}

#[derive(Clone, Default)]
pub(super) struct WatchStatus {
    pub(super) indexed: usize,
    pub(super) registered: usize,
    pub(super) peak_paths: usize,
    pub(super) accepted: u64,
    pub(super) latest_save: Option<SaveReceipt>,
    latest_notification: Option<SaveReceipt>,
    pending_notification: Option<SaveReceipt>,
    pub(super) warning: Option<String>,
}

/// Save order is independent of attempt admission, cancellation and requeue.
#[derive(Clone)]
pub(super) struct SaveReceipt {
    pub(super) sequence: u64,
    pub(super) target: RunTarget,
    pub(super) language: Language,
    pub(super) path: PathBuf,
    pub(super) origin: SaveOrigin,
    pub(super) context: Arc<RunContext>,
    pub(super) results: RunResults,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SaveOrigin {
    Notification,
    Reconciliation,
}

#[derive(Clone)]
pub(super) struct SaveReceipts(Arc<Mutex<WatchStatus>>);

impl SaveReceipts {
    pub(super) fn accepted_sequence(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepted
    }

    // The pending notification also makes an earlier prepared view ineligible while a batch
    // is still being admitted. A manual switch can still fence every accepted sequence.
    pub(super) fn latest_after(&self, sequence: u64) -> (Option<SaveReceipt>, bool) {
        let status = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if status
            .pending_notification
            .as_ref()
            .is_some_and(|save| save.sequence > sequence)
        {
            (None, true)
        } else {
            (
                status
                    .latest_notification
                    .as_ref()
                    .filter(|save| save.sequence > sequence)
                    .cloned(),
                false,
            )
        }
    }

    pub(super) fn is_current(&self, sequence: u64) -> bool {
        let status = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.pending_notification.is_none()
            && status
                .latest_notification
                .as_ref()
                .is_some_and(|save| save.sequence == sequence)
    }
}

type RunRegistration = (RunGeneration, RunResults);

enum Command {
    Open {
        destination: PathBuf,
        contest: Contest,
        reply: mpsc::Sender<io::Result<RunRegistration>>,
    },
    Pause {
        id: String,
        reply: mpsc::Sender<io::Result<()>>,
    },
    Resume {
        id: String,
        reply: mpsc::Sender<io::Result<()>>,
    },
    #[cfg(test)]
    Reconcile {
        rescan: bool,
        reply: mpsc::Sender<io::Result<()>>,
    },
    #[cfg(test)]
    DropNotifications(mpsc::Sender<io::Result<()>>),
    #[cfg(test)]
    SourceBatch(Vec<PathBuf>, mpsc::Sender<io::Result<()>>),
}

impl WorkspaceWatcher {
    pub(super) fn start(
        root: &Path,
        config: RunnerConfig,
        worker: Arc<RunWorker>,
    ) -> io::Result<Self> {
        let root = std::path::absolute(root)?;
        let root_identity = workspace::contest_directory_identity(&root)?;
        let file_watcher = WorkspaceFileWatcher::new()?;
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(WatchStatus::default()));
        let (commands, inbox) = mpsc::sync_channel(8);
        let (ready, started) = mpsc::channel();
        let thread_stop = stop.clone();
        let thread_status = status.clone();
        let handle = thread::Builder::new()
            .name("atc-workspace-watch".into())
            .spawn(move || {
                let mut index = WatchIndex {
                    root,
                    root_identity,
                    config,
                    worker,
                    file_watcher,
                    stop: thread_stop,
                    status: thread_status,
                    contests: BTreeMap::new(),
                    sources: HashMap::new(),
                    installed: BTreeMap::new(),
                    manifests: HashMap::new(),
                    paused: HashSet::new(),
                    next_epoch: 0,
                    retry: false,
                };
                let initial = index.initialize();
                let _ = ready.send(
                    initial
                        .as_ref()
                        .copied()
                        .map_err(|error| io::Error::new(error.kind(), error.to_string())),
                );
                let result = initial.and_then(|()| index.run(&inbox));
                let cleanup = index.retire_all();
                result.and(cleanup)
            })?;
        let mut watcher = Self {
            stop,
            commands,
            handle: Some(handle),
            status,
        };
        if let Err(error) = started.recv().map_err(|_| closed())? {
            let _ = watcher.join();
            return Err(error);
        }
        Ok(watcher)
    }

    pub(super) fn saves(&self) -> SaveReceipts {
        SaveReceipts(self.status.clone())
    }

    pub(super) fn attach_registration(
        &self,
        destination: &Path,
        contest: &Contest,
    ) -> io::Result<RunRegistration> {
        self.request(|reply| Command::Open {
            destination: destination.to_path_buf(),
            contest: contest.clone(),
            reply,
        })
    }

    pub(super) fn pause(&self, id: &str) -> io::Result<()> {
        self.request(|reply| Command::Pause {
            id: id.to_owned(),
            reply,
        })
    }

    pub(super) fn resume(&self, id: &str) -> io::Result<()> {
        self.request(|reply| Command::Resume {
            id: id.to_owned(),
            reply,
        })
    }

    #[cfg(test)]
    pub(super) fn reconcile(&self, rescan: bool) -> io::Result<()> {
        self.request(|reply| Command::Reconcile { rescan, reply })
    }

    fn request<T>(
        &self,
        command: impl FnOnce(mpsc::Sender<io::Result<T>>) -> Command,
    ) -> io::Result<T> {
        let (reply, result) = mpsc::channel();
        self.commands.send(command(reply)).map_err(|_| closed())?;
        result.recv().map_err(|_| closed())?
    }

    #[cfg(test)]
    pub(super) fn status(&self) -> WatchStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub(super) fn stop(mut self) -> io::Result<()> {
        self.request_stop();
        self.join()
    }

    fn join(&mut self) -> io::Result<()> {
        self.handle.take().map_or(Ok(()), |handle| {
            handle
                .join()
                .map_err(|_| io::Error::other("Workspace watcher panicked"))?
        })
    }
}

impl Drop for WorkspaceWatcher {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.join();
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "Workspace watcher is stopped")
}

struct WatchIndex {
    root: PathBuf,
    root_identity: PhysicalDirectoryIdentity,
    config: RunnerConfig,
    worker: Arc<RunWorker>,
    file_watcher: WorkspaceFileWatcher,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<WatchStatus>>,
    contests: BTreeMap<String, WatchedContest>,
    sources: HashMap<PathBuf, SourceEntry>,
    installed: BTreeMap<PathBuf, WorkspaceRegistration>,
    manifests: HashMap<PathBuf, Option<WatchManifestStamp>>,
    paused: HashSet<String>,
    next_epoch: u64,
    retry: bool,
}

struct WatchedContest {
    context: Arc<RunContext>,
    results: RunResults,
    epoch: u64,
}

struct SourceEntry {
    path: PathBuf,
    target: RunTarget,
    language: Language,
    epoch: u64,
    stamp: Option<WatchedFileStamp>,
    #[cfg(target_os = "macos")]
    require_new_write: bool,
}

impl WatchIndex {
    fn initialize(&mut self) -> io::Result<()> {
        self.reconcile(true, None)?;
        #[cfg(target_os = "macos")]
        if self.installed.is_empty() {
            let status = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return Err(io::Error::other(status.warning.clone().unwrap_or_else(
                || "No FSEvents registration could be started".into(),
            )));
        }
        Ok(())
    }

    fn run(&mut self, inbox: &mpsc::Receiver<Command>) -> io::Result<()> {
        let mut periodic = Instant::now() + RECONCILE_INTERVAL;
        let mut manifest_poll = Instant::now() + MANIFEST_POLL;
        while !self.stop.load(Ordering::Acquire) && !self.worker.is_stopping() {
            if let Some(batch) = self.file_watcher.take_ready() {
                self.process(batch)?;
            }
            if Instant::now() >= periodic {
                self.reconcile(true, None)?;
                periodic = Instant::now() + RECONCILE_INTERVAL;
            }
            if Instant::now() >= manifest_poll {
                if self
                    .manifests
                    .iter()
                    .any(|(path, old)| workspace::watch_manifest_stamp(path) != *old)
                {
                    self.reconcile(false, None)?;
                }
                manifest_poll = Instant::now() + MANIFEST_POLL;
            }
            match inbox.recv_timeout(POLL) {
                Ok(command) => self.handle(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Open {
                destination,
                contest,
                reply,
            } => {
                let found = self.open(&destination, &contest).or_else(|_| {
                    self.reconcile(false, None)?;
                    self.open(&destination, &contest)
                });
                let _ = reply.send(found);
            }
            Command::Pause { id, reply } => {
                self.paused.insert(id.clone());
                let _ = reply.send(self.retire(&id).and_then(|()| self.reconcile(false, None)));
            }
            Command::Resume { id, reply } => {
                self.paused.remove(&id);
                let _ = reply.send(self.reconcile(false, None));
            }
            #[cfg(test)]
            Command::Reconcile { rescan, reply } => {
                let _ = reply.send(self.reconcile(rescan, None));
            }
            #[cfg(test)]
            Command::DropNotifications(reply) => {
                self.file_watcher.set_interests(WatchInterests::default());
                #[cfg(target_os = "macos")]
                self.file_watcher.clear_scheduled_recovery();
                let _ = reply.send(Ok(()));
            }
            #[cfg(test)]
            Command::SourceBatch(paths, reply) => {
                let paths = paths
                    .into_iter()
                    .map(|path| {
                        let epoch = self.sources[&path].epoch;
                        (path, epoch)
                    })
                    .collect();
                let _ = reply.send(self.process(WorkspaceBatch {
                    paths,
                    reconcile: false,
                    rescan: false,
                    recover: false,
                    error: None,
                    peak_paths: 0,
                }));
            }
        }
    }

    fn open(&self, destination: &Path, contest: &Contest) -> io::Result<RunRegistration> {
        let watched = self.contests.get(&contest.contest_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "contest has no healthy Workspace watch route: {}",
                    destination.display()
                ),
            )
        })?;
        watched.context.revalidate()?;
        if !watched.context.matches_view(destination, contest)? {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "contest changed before attaching its Workspace view",
            ));
        }
        Ok((watched.context.generation.clone(), watched.results.clone()))
    }

    fn process(&mut self, batch: WorkspaceBatch) -> io::Result<()> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .peak_paths = batch.peak_paths;
        // Refresh topology before applying saved paths. Their captured epoch must still match.
        let reconcile = batch.reconcile || batch.rescan || self.retry;
        if reconcile {
            self.reconcile(false, batch.error.as_deref())?;
        }
        let mut verified = HashMap::new();
        let mut delivered = HashSet::new();
        for (path, epoch) in batch.paths {
            if self.source_event(&path, epoch, true, &mut verified)? {
                delivered.insert(path);
            }
        }
        {
            let mut status = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(save) = status.pending_notification.take() {
                status.latest_notification = Some(save);
            }
        }
        if (reconcile || batch.recover) && !self.file_watcher.recovery_pending() {
            self.recover_sources(&delivered, &mut verified)?;
        }
        if verified.values().any(|valid| !valid) {
            self.reconcile(false, None)?;
        }
        Ok(())
    }

    fn recover_sources(
        &mut self,
        delivered: &HashSet<PathBuf>,
        verified: &mut HashMap<u64, bool>,
    ) -> io::Result<()> {
        let mut candidates: Vec<_> = self
            .sources
            .iter()
            .map(|(path, source)| (path.clone(), source.epoch))
            .collect();
        // Missing OS events have no recoverable order. Use a stable path order for the recovered suffix.
        candidates.sort_unstable();
        for (path, epoch) in candidates {
            if !delivered.contains(&path) {
                self.source_event(&path, epoch, false, verified)?;
            }
        }
        Ok(())
    }

    fn source_event(
        &mut self,
        path: &Path,
        epoch: u64,
        notified: bool,
        verified: &mut HashMap<u64, bool>,
    ) -> io::Result<bool> {
        if self.stop.load(Ordering::Acquire) || self.worker.is_stopping() {
            return Ok(false);
        }
        let Some(source) = self
            .sources
            .get_mut(path)
            .filter(|source| source.epoch == epoch)
        else {
            return Ok(false);
        };
        #[cfg(target_os = "macos")]
        if notified
            && !path
                .parent()
                .and_then(|parent| self.installed.get(parent))
                .is_some_and(|registration| registration.epoch == Some(epoch))
        {
            return Ok(false);
        }
        let Some(contest) = self
            .contests
            .get(source.target.scope.contest_id())
            .filter(|contest| contest.epoch == epoch)
        else {
            return Ok(false);
        };
        let stamp = match contest.context.source_stamp(&source.path) {
            Ok(stamp) => stamp,
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => None,
            Err(_) => {
                // A write/rename can still be in progress. Leave the old fingerprint so
                // another notification or reconciliation can recover the final state.
                self.retry = true;
                return Ok(false);
            }
        };
        // FSEvents can repeat creation/data flags on a chmod. A ctime-only change with
        // the same fingerprint and mtime is not evidence of a save. Keep the baseline
        // so repeated notifications for that metadata state are ignored too.
        #[cfg(target_os = "macos")]
        if let (Some(current), Some(previous)) = (&stamp, &source.stamp)
            && (current.metadata_only_change_from(previous)
                || (source.require_new_write && current.same_write_as(previous)))
        {
            return Ok(false);
        }
        let fingerprint = stamp.as_ref().map(|stamp| &stamp.fingerprint);
        let previous_fingerprint = source.stamp.as_ref().map(|stamp| &stamp.fingerprint);
        if fingerprint == previous_fingerprint && (!notified || stamp.is_none()) {
            #[cfg(target_os = "macos")]
            if let (Some(current), Some(previous)) = (&stamp, &source.stamp)
                && !current.same_write_as(previous)
            {
                source.require_new_write = false;
            }
            source.stamp = stamp;
            return Ok(false);
        }
        let valid = *verified
            .entry(epoch)
            .or_insert_with(|| contest.context.revalidate().is_ok());
        if !valid {
            return Ok(false);
        }

        source.stamp = stamp;
        #[cfg(target_os = "macos")]
        {
            source.require_new_write = !notified;
        }
        let generation = &contest.context.generation;
        let gate = generation.source_gate(&source.target.task_id);
        let sender = self.worker.sender();
        if source.stamp.is_some() {
            let revision = gate.select_source(source.language);
            let request = RunRequest {
                run_id: generation.next_run_id(),
                target: source.target.clone(),
                language: source.language,
                debug: source.language == Language::Cpp && contest.results.debug_enabled(),
                kind: RunKind::Samples,
            };
            contest.results.send(Message::WorkspaceSource {
                target: source.target.clone(),
                revision,
                source: Some((source.path.clone(), source.language)),
            })?;
            let mut status = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sender
                .send(RunWorkerCommand::Run(request))
                .map_err(|_| closed())?;
            status.accepted += 1;
            let origin = if notified {
                SaveOrigin::Notification
            } else {
                SaveOrigin::Reconciliation
            };
            let save = SaveReceipt {
                sequence: status.accepted,
                target: source.target.clone(),
                language: source.language,
                path: source.path.clone(),
                origin,
                context: contest.context.clone(),
                results: contest.results.clone(),
            };
            if origin == SaveOrigin::Notification
                && path
                    .parent()
                    .is_some_and(|parent| self.installed.contains_key(parent))
            {
                status.pending_notification = Some(save.clone());
            }
            status.latest_save = Some(save);
        } else if let Some(revision) = gate.remove_source(source.language) {
            contest.results.send(Message::WorkspaceSource {
                target: source.target.clone(),
                revision,
                source: None,
            })?;
            sender
                .send(RunWorkerCommand::RetireUserInputRuns {
                    target: source.target.clone(),
                    before_source_revision: revision,
                })
                .map_err(|_| closed())?;
        }
        Ok(true)
    }

    fn retire(&mut self, id: &str) -> io::Result<()> {
        self.retire_many(std::iter::once(id.to_owned()))
    }

    fn retire_many(&mut self, ids: impl IntoIterator<Item = String>) -> io::Result<()> {
        let mut epochs = HashSet::new();
        let mut scopes = HashSet::new();
        for id in ids {
            if let Some(contest) = self.contests.remove(&id) {
                let _ = contest.results.send(Message::WorkspaceWatchInvalidated);
                epochs.insert(contest.epoch);
                scopes.insert(contest.context.generation.scope().clone());
            }
        }
        if scopes.is_empty() || self.worker.is_stopping() {
            return Ok(());
        }
        self.sources
            .retain(|_, source| !epochs.contains(&source.epoch));
        match self.worker.invalidate_scopes_and_join(scopes) {
            Err(_) if self.worker.is_stopping() => Ok(()),
            result => result,
        }
    }

    fn retire_all(&mut self) -> io::Result<()> {
        let ids: Vec<_> = self.contests.keys().cloned().collect();
        self.retire_many(ids)
    }

    fn reconcile(&mut self, rescan: bool, backend_error: Option<&str>) -> io::Result<()> {
        if self.stop.load(Ordering::Acquire) || self.worker.is_stopping() {
            return Ok(());
        }
        if workspace::contest_directory_identity(&self.root)? != self.root_identity {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Workspace directory was replaced",
            ));
        }
        self.retry = false;
        let mut diagnostics = Diagnostics::default();
        if let Some(error) = backend_error {
            diagnostics.report(error.to_owned());
        }
        let mut desired = BTreeMap::new();
        let mut interests = WatchInterests::default();
        let mut routes = BTreeMap::new();
        let observed_root = watcher::watch_directory(&self.root)?;
        desired.insert(
            observed_root.clone(),
            WorkspaceRegistration::for_parent(self.root_identity.clone()),
        );
        interests.parents.insert(observed_root.clone());
        interests
            .paths
            .insert(observed_root.join(".atc-workspace.toml"), None);
        match BatchWorkspace::scan_watch(&self.root, UpdateCancellation::for_batch(&self.stop)) {
            Ok((workspace, discovery)) if discovery.complete => {
                for (path, error) in discovery.errors {
                    diagnostics.report(format!("{}: {error}", path.display()));
                }
                let mut known_parents = HashMap::new();
                for parent in workspace.watch_parents()? {
                    let path = parent.path;
                    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
                        && let Some(observed) = known_parents
                            .get(parent)
                            .map(|path: &PathBuf| path.join(name))
                    {
                        // A configured component can have spaces and may not exist yet.
                        interests.paths.insert(observed, None);
                    }
                    match parent.identity.and_then(|identity| {
                        identity
                            .map(|identity| {
                                watcher::watch_directory(&path).map(|observed| (observed, identity))
                            })
                            .transpose()
                    }) {
                        Ok(Some((observed, identity))) => {
                            known_parents.insert(path, observed.clone());
                            interests.parents.insert(observed.clone());
                            desired.insert(observed, WorkspaceRegistration::for_parent(identity));
                        }
                        Ok(None) => {}
                        Err(error) => diagnostics.report(format!("{}: {error}", path.display())),
                    }
                }
                for (id, path, found) in discovery.contests {
                    match found.and_then(|found| workspace.watch_route(&id, &found)) {
                        Ok(Some(route)) if !self.paused.contains(&id) => {
                            routes.insert(id, route);
                        }
                        Ok(_) => {}
                        Err(error) => diagnostics.report(format!("{}: {error}", path.display())),
                    }
                }
            }
            Ok(_) => return Ok(()),
            Err(error) => diagnostics.report(format!("Workspace discovery: {error}")),
        }

        let expired: Vec<_> = self
            .contests
            .iter()
            .filter(|(id, current)| {
                !routes
                    .get(*id)
                    .is_some_and(|route| current.context.matches_watch_route(route))
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.retire_many(expired)?;

        let mut manifests = HashMap::new();
        for (id, route) in routes {
            if self.stop.load(Ordering::Acquire) || self.worker.is_stopping() {
                return Ok(());
            }
            let path = route.destination.clone();
            let observed = match watcher::watch_directory(&path) {
                Ok(path) => path,
                Err(error) => {
                    diagnostics.report(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            desired.insert(
                observed.clone(),
                WorkspaceRegistration::new(route.destination_identity().clone()),
            );
            interests.paths.insert(observed.join(".atc"), None);
            interests.paths.insert(observed.join("tests"), None);
            interests.contests.insert(observed.clone());
            // Windows blocks rename of a directory with an open handle to a descendant,
            // even with FILE_SHARE_DELETE. Poll metadata without watching .atc itself.
            manifests.insert(path.clone(), workspace::watch_manifest_stamp(&path));
            if !self.contests.contains_key(&id)
                && let Err(error) = self.add_contest(&id, route, &observed)
            {
                diagnostics.report(format!("{}: {error}", path.display()));
            }
            #[cfg(target_os = "macos")]
            if let Some(contest) = self.contests.get(&id) {
                desired
                    .get_mut(&observed)
                    .expect("contest directory is desired")
                    .epoch = Some(contest.epoch);
            }
        }
        for (path, source) in &self.sources {
            interests.paths.insert(path.clone(), Some(source.epoch));
        }
        self.manifests = manifests;
        self.file_watcher.set_interests(interests);

        #[cfg(target_os = "macos")]
        self.file_watcher.replace_registrations(
            &mut self.installed,
            desired,
            &self.stop,
            |error| diagnostics.report_registration(error),
        )?;
        #[cfg(not(target_os = "macos"))]
        self.update_registrations(desired, &mut diagnostics);

        self.publish_status(diagnostics);
        if rescan && !self.file_watcher.recovery_pending() {
            self.recover_sources(&HashSet::new(), &mut HashMap::new())?;
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn update_registrations(
        &mut self,
        desired: BTreeMap<PathBuf, WorkspaceRegistration>,
        diagnostics: &mut Diagnostics,
    ) {
        let removed: Vec<_> = self
            .installed
            .iter()
            .filter(|(path, identity)| desired.get(*path) != Some(*identity))
            .map(|(path, _)| path.clone())
            .collect();
        for path in removed {
            match self.file_watcher.unregister(&path) {
                Ok(()) => {
                    self.installed.remove(&path);
                }
                Err(error) => diagnostics.report(format!("unwatch {}: {error}", path.display())),
            }
        }
        for (path, identity) in desired {
            if self.installed.contains_key(&path) {
                continue;
            }
            match self.file_watcher.register(&path) {
                Ok(()) => {
                    self.installed.insert(path, identity);
                }
                Err(error) => diagnostics.report(format!("watch {}: {error}", path.display())),
            }
        }
    }

    fn add_contest(
        &mut self,
        id: &str,
        route: ExplicitContestRoute,
        observed: &Path,
    ) -> io::Result<()> {
        let destination = route.destination.clone();
        let contest = match diagnose_watch_contest(&destination, id) {
            ContestDiagnosis::Healthy { contest, .. } => contest,
            ContestDiagnosis::Blocked(reason) => {
                let (kind, message) = reason.error_details();
                return Err(io::Error::new(kind, message));
            }
            _ => return Ok(()),
        };
        let sources = build_watched_sources(&destination, &contest)?;
        let context = Arc::new(RunContext::from_watch_route(
            &self.root,
            route,
            &contest,
            self.config.clone(),
        )?);
        let (generation, results) = self.worker.register_shared(context.clone())?;
        self.next_epoch = self
            .next_epoch
            .checked_add(1)
            .expect("Workspace watch epoch exhausted");
        let epoch = self.next_epoch;
        for source in sources {
            let Some(name) = source.path.file_name() else {
                continue;
            };
            let stamp = context.source_stamp(&source.path).unwrap_or(None);
            self.sources.insert(
                observed.join(name),
                SourceEntry {
                    target: generation
                        .scope()
                        .target(&contest.problems[source.problem].task_id),
                    path: source.path,
                    language: source.language,
                    epoch,
                    stamp,
                    #[cfg(target_os = "macos")]
                    // A native event with the baseline's file stamp can be a delayed
                    // coalesced FSEvents notification from before this watch existed.
                    require_new_write: true,
                },
            );
        }
        self.contests.insert(
            id.to_owned(),
            WatchedContest {
                context,
                results,
                epoch,
            },
        );
        Ok(())
    }

    fn publish_status(&self, diagnostics: Diagnostics) {
        let warning = diagnostics.summary();
        {
            let mut status = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            status.warning = warning.clone();
            status.indexed = self.contests.len();
            status.registered = self.installed.len();
        }
        for contest in self.contests.values() {
            let _ = contest
                .results
                .send(Message::WorkspaceWatchWarning(warning.clone()));
        }
    }
}

#[derive(Default)]
struct Diagnostics {
    first: Option<String>,
    count: usize,
}

impl Diagnostics {
    fn report(&mut self, message: String) {
        self.count += 1;
        self.first.get_or_insert(message);
    }

    #[cfg(target_os = "macos")]
    fn report_registration(&mut self, message: String) {
        self.count += 1;
        self.first = Some(message);
    }

    fn summary(self) -> Option<String> {
        self.first
            .map(|first| format!("Workspace watch ({}): {first}", self.count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Problem, Sample};
    use crate::tui::app::{CaseVerdict, RunPhase, UserInputState, WatchApp};
    use std::fs;
    use std::process::Command as ProcessCommand;

    fn config(root: &Path, content: &str) {
        fs::write(
            root.join(".atc-workspace.toml"),
            format!("version = 1\n{content}"),
        )
        .unwrap();
    }

    fn fixture(root: &Path, id: &str, samples: bool) -> Contest {
        let contest = Contest {
            contest_id: id.into(),
            problems: vec![Problem {
                index: "A".into(),
                title: "Problem A".into(),
                task_id: format!("{id}_a"),
                url: format!("https://atcoder.jp/contests/{id}/tasks/{id}_a"),
                sample_count: usize::from(samples),
            }],
        };
        let directory = root.join(id);
        workspace::save_metadata(&directory, &contest).unwrap();
        if samples {
            workspace::save_samples(
                &directory,
                &contest.problems[0],
                &[Sample {
                    input: format!("expected-{id}\n"),
                    output: format!("expected-{id}\n"),
                }],
            )
            .unwrap();
        }
        contest
    }

    fn wait_for<T>(mut read: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(value) = read() {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "Workspace watcher did not reach expected state"
            );
            thread::sleep(Duration::from_millis(15));
        }
    }

    fn start(root: &Path, config: RunnerConfig) -> (Arc<RunWorker>, WorkspaceWatcher) {
        let worker = Arc::new(RunWorker::start_workspace().unwrap());
        let watcher = WorkspaceWatcher::start(root, config, worker.clone()).unwrap();
        (worker, watcher)
    }

    fn stop(worker: Arc<RunWorker>, watcher: WorkspaceWatcher) {
        watcher.stop().unwrap();
        Arc::into_inner(worker)
            .expect("watcher joined before worker shutdown")
            .stop_and_join()
            .unwrap();
    }

    fn python() -> RunnerConfig {
        let python = ["python3", "python"]
            .into_iter()
            .find(|name| {
                ProcessCommand::new(name)
                    .arg("--version")
                    .output()
                    .is_ok_and(|output| output.status.success())
            })
            .expect("runner tests require Python");
        RunnerConfig {
            python: python.into(),
            ..RunnerConfig::default()
        }
    }

    fn display(
        contest: &Contest,
        generation: RunGeneration,
        results: &RunResults,
    ) -> (WatchApp, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel();
        let mut app = WatchApp::new_with_run_generation(
            contest,
            generation,
            vec![contest.problems[0].sample_count],
            vec![None],
            vec![UserInputState::default()],
        )
        .unwrap();
        app.attach_run_view(results.attach(tx).unwrap()).unwrap();
        (app, rx)
    }

    #[test]
    fn native_save_runs_the_hidden_contest_source_and_samples_without_switching_view() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(root, "");
        let a = fixture(root, "abc483", true);
        let b = fixture(root, "abc480", true);
        fs::write(root.join("abc483/A.py"), "print('wrong contest')\n").unwrap();
        let (worker, watcher) = start(root, python());
        assert_eq!(watcher.status().indexed, 2);
        let (a_id, a_results) = watcher
            .attach_registration(&root.join("abc483"), &a)
            .unwrap();
        let (visible, inbox) = display(&a, a_id, &a_results);
        // The indexed source did not exist when the Workspace watcher was installed.
        fs::write(root.join("abc480/A.py"), "print('payload-abc480')\n").unwrap();
        let accepted = wait_for(|| watcher.status().latest_save);
        assert_eq!(accepted.target.scope.contest_id(), "abc480");
        assert_eq!(accepted.target.task_id.as_ref(), "abc480_a");
        assert_eq!(accepted.language, Language::Python);
        assert_eq!(accepted.sequence, 1);
        let (generation, results) = watcher
            .attach_registration(&root.join("abc480"), &b)
            .unwrap();
        let (mut restored, _inbox) = display(&b, generation, &results);
        wait_for(|| {
            restored.apply_execution_updates();
            (restored.current_problem().unwrap().run.phase == RunPhase::Finished).then_some(())
        });
        let case = &restored.current_problem().unwrap().run.cases[0];
        assert_eq!(case.verdict, CaseVerdict::WrongAnswer);
        assert_eq!(case.actual.as_deref().unwrap().trim(), "payload-abc480");
        assert_eq!(case.expected.as_deref().unwrap().trim(), "expected-abc480");
        assert_eq!(visible.contest_id(), "abc483");
        assert_eq!(visible.current_problem().unwrap().run.phase, RunPhase::Idle);
        assert!(inbox.try_recv().is_err());
        assert_eq!(watcher.status().accepted, 1);
        stop(worker, watcher);
    }

    #[test]
    fn actual_atomic_replacement_rename_and_recreation_filter_non_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(root, "");
        fixture(root, "abc483", false);
        let contest = root.join("abc483");
        let source = contest.join("A.py");
        let ignored = contest.join("A.cpp.tmp");
        fs::write(&source, "print(0)\n").unwrap();
        let (worker, watcher) = start(root, RunnerConfig::default());
        fs::write(&ignored, "ignored\n").unwrap();
        fs::write(contest.join(".gitignore"), "ignored\n").unwrap();
        fs::create_dir(contest.join("tests")).unwrap();
        fs::write(contest.join("tests/ignored.cpp"), "ignored\n").unwrap();
        thread::sleep(Duration::from_millis(350));
        assert_eq!(watcher.status().accepted, 0);

        let temporary = contest.join("atomic.py.tmp");
        fs::write(&temporary, "print(1)\n").unwrap();
        fs::rename(&temporary, &source).unwrap();
        wait_for(|| (watcher.status().accepted == 1).then_some(()));
        fs::rename(&source, &temporary).unwrap();
        thread::sleep(Duration::from_millis(350));
        assert_eq!(watcher.status().accepted, 1);
        fs::rename(&temporary, &source).unwrap();
        wait_for(|| (watcher.status().accepted == 2).then_some(()));
        fs::remove_file(&source).unwrap();
        thread::sleep(Duration::from_millis(350));
        assert_eq!(watcher.status().accepted, 2);
        fs::write(&source, "print(2)\n").unwrap();
        wait_for(|| (watcher.status().accepted == 3).then_some(()));
        stop(worker, watcher);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_chmod_does_not_run_or_offer_hidden_view_but_writes_do() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(root, "");
        let contest = fixture(root, "abc480", true);
        fixture(root, "abc483", false);
        let destination = root.join("abc480");
        let source = destination.join("A.py");
        let original = "print('expected-abc480')\n";
        fs::write(&source, original).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        let (worker, watcher) = start(root, python());
        let (generation, results) = watcher.attach_registration(&destination, &contest).unwrap();
        let saves = watcher.saves();

        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        thread::sleep(Duration::from_millis(450));
        watcher.reconcile(true).unwrap();
        assert_eq!(saves.accepted_sequence(), 0);
        assert!(saves.latest_after(0).0.is_none());
        let (app, _inbox) = display(&contest, generation, &results);
        assert_eq!(app.current_problem().unwrap().run.phase, RunPhase::Idle);
        drop(app);

        fs::write(&source, original).unwrap();
        let same_content = wait_for(|| saves.latest_after(0).0);
        assert_eq!(same_content.sequence, 1);
        assert_eq!(same_content.origin, SaveOrigin::Notification);

        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(&source, "print('new content')\n").unwrap();
        let mixed = wait_for(|| saves.latest_after(same_content.sequence).0);
        assert_eq!(mixed.sequence, 2);
        let (generation, results) = watcher.attach_registration(&destination, &contest).unwrap();
        let (mut app, _inbox) = display(&contest, generation, &results);
        wait_for(|| {
            app.apply_execution_updates();
            (app.current_problem().unwrap().run.phase == RunPhase::Finished).then_some(())
        });
        assert_eq!(
            app.current_problem().unwrap().run.cases[0]
                .actual
                .as_deref()
                .unwrap()
                .trim(),
            "new content"
        );
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        thread::sleep(Duration::from_millis(450));
        assert_eq!(saves.accepted_sequence(), mixed.sequence);
        stop(worker, watcher);
    }

    #[test]
    fn discovers_new_and_moves_existing_contests_and_retires_deleted_scopes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(root, "");
        let a = fixture(root, "abc483", false);
        let a_path = root.join("abc483");
        let mapped = root.join("Group A");
        fs::create_dir(&mapped).unwrap();
        let (worker, watcher) = start(root, RunnerConfig::default());
        let (old, old_results) = watcher.attach_registration(&a_path, &a).unwrap();
        let b = fixture(root, "abc480", false);
        let b_path = root.join("abc480");
        wait_for(|| (watcher.status().indexed == 2).then_some(()));
        let (untouched, _) = watcher.attach_registration(&b_path, &b).unwrap();
        fs::rename(&a_path, mapped.join("abc483")).unwrap();
        config(root, "[[paths]]\npattern = '^abc483$'\npath = 'Group A'\n");
        wait_for(|| old_results.attach(mpsc::channel().0).err());
        wait_for(|| {
            (watcher.status().indexed == 2 && watcher.status().registered == 4).then_some(())
        });
        fs::write(mapped.join("abc483/A.py"), "print(3)\n").unwrap();
        let received = wait_for(|| watcher.status().latest_save);
        assert_ne!(received.target.scope, *old.scope());
        assert_eq!(received.target.scope.contest_id(), "abc483");
        let (still_current, _) = watcher.attach_registration(&b_path, &b).unwrap();
        assert_eq!(still_current.scope(), untouched.scope());
        fs::remove_dir_all(mapped.join("abc483")).unwrap();
        wait_for(|| (watcher.status().indexed == 1).then_some(()));
        stop(worker, watcher);
    }

    #[test]
    fn discovers_a_new_nested_mapping_through_existing_parent_notifications() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(
            root,
            "[[paths]]\npattern = '^abc483$'\npath = 'Group A/Inner B'\n",
        );
        let (worker, watcher) = start(root, RunnerConfig::default());
        assert_eq!(watcher.status().registered, 1);
        let mapped = root.join("Group A/Inner B");
        fs::create_dir_all(&mapped).unwrap();
        fixture(&mapped, "abc483", false);
        wait_for(|| {
            (watcher.status().indexed == 1 && watcher.status().registered == 4).then_some(())
        });
        fs::write(mapped.join("abc483/A.py"), "pass\n").unwrap();
        assert_eq!(
            wait_for(|| watcher.status().latest_save)
                .target
                .scope
                .contest_id(),
            "abc483"
        );
        stop(worker, watcher);
    }

    #[test]
    fn refresh_pause_blocks_old_generation_and_keeps_other_contest_live() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(root, "");
        let mut a = fixture(root, "abc483", false);
        let b = fixture(root, "abc480", false);
        let active = root.join("abc483");
        let other = root.join("abc480");
        let (worker, watcher) = start(root, RunnerConfig::default());
        let (old, results) = watcher.attach_registration(&active, &a).unwrap();
        let (other_generation, _) = watcher.attach_registration(&other, &b).unwrap();
        watcher.pause("abc483").unwrap();
        assert!(results.attach(mpsc::channel().0).is_err());
        fs::write(active.join("A.py"), "print('during refresh')\n").unwrap();
        a.problems[0].title = "Rebuilt".into();
        workspace::save_metadata(&active, &a).unwrap();
        fs::write(other.join("A.py"), "print('other')\n").unwrap();
        let other_save = wait_for(|| watcher.status().latest_save);
        assert_eq!(other_save.target.scope, *other_generation.scope());

        watcher.resume("abc483").unwrap();
        let (new, _) = watcher.attach_registration(&active, &a).unwrap();
        assert_ne!(new.scope(), old.scope());
        thread::sleep(Duration::from_millis(350));
        assert_eq!(watcher.status().accepted, 1);
        fs::write(active.join("A.py"), "print('after refresh')\n").unwrap();
        let current = wait_for(|| {
            watcher
                .status()
                .latest_save
                .filter(|receipt| receipt.sequence == 2)
        });
        assert_eq!(current.target.scope, *new.scope());
        stop(worker, watcher);
    }

    #[test]
    fn reconciles_replaced_metadata_and_keeps_healthy_registrations_after_a_partial_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(
            root,
            "[[paths]]\npattern = '^arc100$'\npath = 'bad-parent'\n",
        );
        fs::write(root.join("bad-parent"), "can't watch a file as a directory").unwrap();
        let mut a = fixture(root, "abc483", false);
        let b = fixture(root, "abc480", false);
        let active = root.join("abc483");
        let other = root.join("abc480");
        let (worker, watcher) = start(root, RunnerConfig::default());
        assert!(watcher.status().warning.unwrap().contains("bad-parent"));
        let (old, results) = watcher.attach_registration(&active, &a).unwrap();
        let (warning_tx, warning_rx) = mpsc::channel();
        let view = results.attach(warning_tx).unwrap();
        assert!(matches!(
            warning_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Message::WorkspaceWatchWarning(Some(warning)) if warning.contains("bad-parent")
        ));
        drop(view);
        let (other_generation, _) = watcher.attach_registration(&other, &b).unwrap();
        a.problems[0].title = "Updated externally".into();
        workspace::save_metadata(&active, &a).unwrap();
        wait_for(|| results.attach(mpsc::channel().0).err());
        watcher.reconcile(true).unwrap();
        let (new, _) = watcher.attach_registration(&active, &a).unwrap();
        assert_ne!(old.scope(), new.scope());
        fs::write(other.join("A.py"), "print('healthy')\n").unwrap();
        let accepted = wait_for(|| watcher.status().latest_save);
        assert_eq!(accepted.target.scope, *other_generation.scope());
        stop(worker, watcher);
    }

    #[test]
    fn rescan_recovers_only_changed_sources_when_notifications_were_lost() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(root, "");
        fixture(root, "abc483", false);
        fixture(root, "abc480", false);
        fs::write(root.join("abc483/A.py"), "pass\n").unwrap();
        let (worker, watcher) = start(root, RunnerConfig::default());
        watcher.request(Command::DropNotifications).unwrap();
        fs::write(root.join("abc480/A.py"), "print(0)\n").unwrap();
        fs::write(root.join("abc480/A.py"), "print(1)\n").unwrap();
        thread::sleep(Duration::from_millis(350));
        assert_eq!(watcher.status().accepted, 0);
        watcher.reconcile(true).unwrap();
        let recovered = watcher.status().latest_save.unwrap();
        assert_eq!(recovered.target.scope.contest_id(), "abc480");
        assert_eq!(recovered.sequence, 1);
        assert_eq!(recovered.origin, SaveOrigin::Reconciliation);
        assert!(watcher.saves().latest_after(0).0.is_none());
        watcher.reconcile(true).unwrap();
        assert_eq!(watcher.status().accepted, 1);
        stop(worker, watcher);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recovery_deduplicates_a_late_notification_but_keeps_a_same_content_resave() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(root, "");
        fixture(root, "abc480", false);
        let destination = watcher::watch_directory(&root.join("abc480")).unwrap();
        let path = destination.join("A.py");
        fs::write(&path, "print(0)\n").unwrap();
        let (worker, watcher) = start(root, RunnerConfig::default());
        watcher.request(Command::DropNotifications).unwrap();
        watcher
            .request(|reply| Command::SourceBatch(vec![path.clone()], reply))
            .unwrap();
        assert_eq!(watcher.status().accepted, 0);
        fs::write(&path, "print(1)\n").unwrap();
        watcher.reconcile(true).unwrap();
        assert_eq!(
            watcher.status().latest_save.unwrap().origin,
            SaveOrigin::Reconciliation
        );
        watcher
            .request(|reply| Command::SourceBatch(vec![path.clone()], reply))
            .unwrap();
        assert_eq!(watcher.status().accepted, 1);
        assert!(watcher.saves().latest_after(0).0.is_none());

        let before = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "print(1)\n").unwrap();
        assert_ne!(before, fs::metadata(&path).unwrap().modified().unwrap());
        watcher
            .request(|reply| Command::SourceBatch(vec![path], reply))
            .unwrap();
        assert_eq!(watcher.status().accepted, 2);
        assert_eq!(
            watcher.saves().latest_after(0).0.unwrap().origin,
            SaveOrigin::Notification
        );
        stop(worker, watcher);
    }

    #[test]
    fn a_batch_publishes_only_its_latest_save_and_rescan_does_not_replace_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(root, "");
        fixture(root, "abc483", false);
        fixture(root, "abc480", false);
        fixture(root, "abc478", false);
        let (worker, watcher) = start(root, RunnerConfig::default());
        watcher.request(Command::DropNotifications).unwrap();
        let first = root.join("abc483/A.py");
        let last = root.join("abc480/A.cpp");
        fs::write(&first, "pass\n").unwrap();
        fs::write(&last, "int main() {}\n").unwrap();
        let observed = |path: &Path| {
            watcher::watch_directory(path.parent().unwrap())
                .unwrap()
                .join(path.file_name().unwrap())
        };
        watcher
            .request(|reply| Command::SourceBatch(vec![observed(&first), observed(&last)], reply))
            .unwrap();
        assert_eq!(watcher.status().accepted, 2);
        let saves = watcher.saves();
        let displayed = saves.latest_after(0).0.unwrap();
        assert_eq!(displayed.sequence, 2);
        assert_eq!(displayed.target.scope.contest_id(), "abc480");
        assert_eq!(displayed.language, Language::Cpp);
        assert_eq!(displayed.path, last);
        assert!(!saves.is_current(1));

        fs::write(root.join("abc478/A.py"), "pass\n").unwrap();
        thread::sleep(Duration::from_millis(350));
        watcher.reconcile(true).unwrap();
        assert_eq!(
            watcher.status().latest_save.unwrap().origin,
            SaveOrigin::Reconciliation
        );
        assert_eq!(saves.accepted_sequence(), 3);
        assert_eq!(
            saves.latest_after(0).0.unwrap().sequence,
            displayed.sequence
        );
        assert!(saves.latest_after(displayed.sequence).0.is_none());
        stop(worker, watcher);
    }

    #[test]
    fn native_saves_preempt_and_requeue_the_original_sample_run_then_join_on_stop() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        config(root, "");
        let a = fixture(root, "abc483", true);
        let b = fixture(root, "abc480", true);
        let mut runner = python();
        runner.timeout_seconds = 10.0;
        let (worker, watcher) = start(root, runner);
        let (a_generation, a_results) = watcher
            .attach_registration(&root.join("abc483"), &a)
            .unwrap();
        let (b_generation, b_results) = watcher
            .attach_registration(&root.join("abc480"), &b)
            .unwrap();
        let (mut a_display, _a_messages) = display(&a, a_generation, &a_results);
        let (mut b_display, _b_messages) = display(&b, b_generation, &b_results);
        fs::write(
            root.join("abc483/A.py"),
            "import time\ntime.sleep(2)\nprint(input())\n",
        )
        .unwrap();
        let first = wait_for(|| {
            a_display.apply_execution_updates();
            let run = &a_display.current_problem().unwrap().run;
            (run.phase == RunPhase::Running).then_some(run.id).flatten()
        });
        fs::write(
            root.join("abc480/A.py"),
            "import time\ntime.sleep(8)\nprint(input())\n",
        )
        .unwrap();
        wait_for(|| {
            a_display.apply_execution_updates();
            b_display.apply_execution_updates();
            let a_run = &a_display.current_problem().unwrap().run;
            let b_run = &b_display.current_problem().unwrap().run;
            (a_run.phase == RunPhase::Queued
                && a_run.id == Some(first)
                && b_run.phase == RunPhase::Running)
                .then_some(())
        });
        let receipts = watcher.saves();
        let latest = wait_for(|| receipts.latest_after(0).0.filter(|save| save.sequence == 2));
        assert_eq!(latest.target.scope.contest_id(), "abc480");
        assert_eq!(latest.sequence, 2);
        assert!(
            receipts.latest_after(latest.sequence).0.is_none(),
            "the requeue creates no display candidate"
        );
        let stopped_at = Instant::now();
        stop(worker, watcher);
        assert!(stopped_at.elapsed() < Duration::from_secs(5));
        assert!(a_results.attach(mpsc::channel().0).is_err());
        assert!(b_results.attach(mpsc::channel().0).is_err());
        fs::remove_dir_all(root.join("abc480")).unwrap();
    }
}
