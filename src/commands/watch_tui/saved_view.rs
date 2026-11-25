use super::{
    ContestSession, FinalizedContestEntry, SilentContestReporter, finalize_prepared_contest_entry,
    prepare_workspace_contest_entry,
};
use crate::app_context::AppContext;
use crate::commands::workspace_watch::{SaveOrigin, SaveReceipt, SaveReceipts};
use crate::tui::{
    ContestSwitchMutation, ContestSwitchRequest, SavedSource, SavedViewEvent, SavedViewUpdate,
    message::{RunScope, RunWorkerCommand},
};
use crate::workspace;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc::Sender};
use std::thread::{self, JoinHandle};

pub(super) struct SavedViewSwitch {
    receipts: SaveReceipts,
    root: PathBuf,
    handled_through: u64,
    job: Option<Preparation>,
    failure: Option<Arc<str>>,
    next: Option<(ContestSession, SavedSource)>,
}

struct Preparation {
    save: SaveReceipt,
    handle: JoinHandle<io::Result<Option<FinalizedContestEntry>>>,
}

impl SavedViewSwitch {
    pub(super) fn new(root: PathBuf, receipts: SaveReceipts) -> Self {
        Self {
            receipts,
            root,
            handled_through: 0,
            job: None,
            failure: None,
            next: None,
        }
    }

    pub(super) fn take_next(&mut self) -> Option<(ContestSession, SavedSource)> {
        self.next.take()
    }

    pub(super) fn update(
        &mut self,
        event: SavedViewEvent,
        visible: &RunScope,
        run_tx: &Sender<RunWorkerCommand>,
    ) -> SavedViewUpdate {
        let blocked = match event {
            SavedViewEvent::Poll { blocked } => blocked,
            SavedViewEvent::ManualCommit => {
                // This is the same lock boundary at which the watcher enqueues and numbers saves.
                self.handled_through = self.receipts.accepted_sequence();
                self.failure = None;
                return SavedViewUpdate::Status(None);
            }
        };
        let (latest, collecting) = self.receipts.latest_after(self.handled_through);
        if blocked && (latest.is_some() || collecting) {
            return SavedViewUpdate::Status(Some("Saved source switch on hold".into()));
        }
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
        {
            let job = self.job.take().unwrap();
            let prepared = job
                .handle
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("saved source preparation panicked")));
            if latest
                .as_ref()
                .is_some_and(|save| save.sequence == job.save.sequence)
            {
                return self.complete(job.save, prepared, run_tx);
            }
        }
        if let Some(save) = latest {
            self.failure = None;
            if self.job.is_none() {
                let root = self.root.clone();
                let visible = visible.clone();
                let candidate = save.clone();
                match thread::Builder::new()
                    .name("atc-saved-view".into())
                    .spawn(move || prepare(&root, &visible, &candidate))
                {
                    Ok(handle) => self.job = Some(Preparation { save, handle }),
                    Err(error) => return self.fail_if_current(&save, error),
                }
            }
            return Self::preparing();
        }
        if collecting {
            return Self::preparing();
        }
        SavedViewUpdate::Status(self.failure.clone())
    }

    fn complete(
        &mut self,
        save: SaveReceipt,
        prepared: io::Result<Option<FinalizedContestEntry>>,
        run_tx: &Sender<RunWorkerCommand>,
    ) -> SavedViewUpdate {
        if !self.receipts.is_current(save.sequence) {
            return Self::preparing();
        }
        if !save.results.is_registered() {
            return self.fail_if_current(&save, io::Error::other("contest generation expired"));
        }
        let session = match prepared.and_then(|entry| {
            entry
                .map(|entry| {
                    ContestSession::start_saved_view(
                        entry,
                        save.context.generation.clone(),
                        save.results.clone(),
                        run_tx.clone(),
                    )
                })
                .transpose()
        }) {
            Ok(session) => session,
            Err(error) => return self.fail_if_current(&save, error),
        };
        // Construction and subscription have succeeded. Compare order as the last step before
        // committing, including sends accepted while preparing or subscribing to the new view.
        if let Err(error) = validate(&save) {
            return self.fail_if_current(&save, error);
        }
        if !self.receipts.is_current(save.sequence) {
            return Self::preparing();
        }
        self.handled_through = save.sequence;
        self.failure = None;
        let source = SavedSource {
            target: save.target,
            path: save.path,
            language: save.language,
        };
        if let Some(session) = session {
            self.next = Some((session, source));
            SavedViewUpdate::Switch
        } else {
            SavedViewUpdate::Select(source)
        }
    }

    fn fail_if_current(&mut self, save: &SaveReceipt, error: io::Error) -> SavedViewUpdate {
        if !self.receipts.is_current(save.sequence) {
            return Self::preparing();
        }
        self.handled_through = save.sequence;
        self.failure = Some(
            format!(
                "Saved source switch failed for {}: {error}",
                save.target.scope.contest_id()
            )
            .into(),
        );
        SavedViewUpdate::Status(self.failure.clone())
    }

    fn preparing() -> SavedViewUpdate {
        SavedViewUpdate::Status(Some("Preparing saved source view…".into()))
    }
}

impl Drop for SavedViewSwitch {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            let _ = job.handle.join();
        }
    }
}

fn validate(save: &SaveReceipt) -> io::Result<()> {
    if save.origin != SaveOrigin::Notification
        || save.context.is_stale()
        || save.context.generation.scope() != &save.target.scope
        || !save.results.is_registered()
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "contest generation expired",
        ));
    }
    save.context.revalidate()?;
    if !save.context.watched_source_exists(&save.path)? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "saved source no longer exists",
        ));
    }
    Ok(())
}

fn prepare(
    root: &Path,
    visible: &RunScope,
    save: &SaveReceipt,
) -> io::Result<Option<FinalizedContestEntry>> {
    validate(save)?;
    if visible == &save.target.scope {
        return Ok(None);
    }
    let contest_id = save.target.scope.contest_id();
    let destination = workspace::resolve_active_workspace_contest_path(root, contest_id)?;
    let request = ContestSwitchRequest {
        mutation: ContestSwitchMutation::Open,
        contest_id: contest_id.to_owned(),
        destination,
    };
    let entry = prepare_workspace_contest_entry(root, &request, &mut SilentContestReporter)
        .map_err(io::Error::other)?;
    let app_context = AppContext::Workspace {
        root: root.to_path_buf(),
    };
    let finalized = finalize_prepared_contest_entry(entry, &app_context)?;
    if !save
        .context
        .matches_view(&finalized.0.input.destination, &finalized.0.input.contest)?
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "contest changed during view preparation",
        ));
    }
    Ok(Some(finalized))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch_tui::{ContestEnvironment, WorkspaceRuns};
    use crate::tui::message::RunGeneration;
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Fixture {
        session: ContestSession,
        runs: WorkspaceRuns,
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join(".atc-workspace.toml"), "version = 1\n").unwrap();
            for id in ["abc481", "abc482", "abc483"] {
                let task_id = format!("{id}_a");
                workspace::save_metadata(
                    &root.path().join(id),
                    &crate::model::Contest {
                        contest_id: id.into(),
                        problems: vec![crate::model::Problem {
                            index: "A".into(),
                            title: "Problem A".into(),
                            url: format!("https://atcoder.jp/contests/{id}/tasks/{task_id}"),
                            task_id,
                            sample_count: 0,
                        }],
                    },
                )
                .unwrap();
            }
            let runs = WorkspaceRuns::default();
            let config = crate::config::Config::parse("[watch]\ncross_contest = true\n").unwrap();
            let context = AppContext::from_launch_root(root.path()).unwrap();
            let input =
                super::super::PreparedWatchInput::load(&root.path().join("abc481"), Some("abc481"))
                    .unwrap();
            let session = ContestSession::start_entry(
                super::super::PreparedContestEntry {
                    input,
                    environment: ContestEnvironment::new(config),
                },
                &context,
                &runs,
            )
            .unwrap();
            Self {
                root,
                runs,
                session,
            }
        }

        fn coordinator(&self) -> SavedViewSwitch {
            SavedViewSwitch::new(
                self.root.path().to_path_buf(),
                self.runs
                    .workspace_watcher
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .saves(),
            )
        }

        fn save(&self, switch: &SavedViewSwitch, id: &str) -> SaveReceipt {
            let before = switch.receipts.accepted_sequence();
            fs::write(
                self.root.path().join(id).join("A.py"),
                format!("print({before})\n"),
            )
            .unwrap();
            await_value(|| switch.receipts.latest_after(before).0)
        }

        fn visible(&self) -> &RunGeneration {
            &self.session.run_generation
        }

        fn poll(&self, switch: &mut SavedViewSwitch, blocked: bool) -> SavedViewUpdate {
            switch.update(
                SavedViewEvent::Poll { blocked },
                self.visible().scope(),
                &self.session.run_tx,
            )
        }

        fn pause_prepared(
            &self,
            switch: &mut SavedViewSwitch,
            save: SaveReceipt,
        ) -> mpsc::Sender<()> {
            let prepared = prepare(&switch.root, self.visible().scope(), &save).unwrap();
            let (release, wait) = mpsc::channel();
            let handle = thread::spawn(move || {
                wait.recv_timeout(Duration::from_secs(8)).unwrap();
                Ok(prepared)
            });
            switch.job = Some(Preparation { save, handle });
            release
        }
    }

    fn await_value<T>(mut progress: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(value) = progress() {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "saved source navigation did not settle"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn newest_save_supersedes_a_view_prepared_for_an_earlier_contest() {
        let fixture = Fixture::new();
        let mut switch = fixture.coordinator();
        let older = fixture.save(&switch, "abc482");
        let release = fixture.pause_prepared(&mut switch, older.clone());
        let newest = fixture.save(&switch, "abc483");
        assert!(matches!(
            fixture.poll(&mut switch, false),
            SavedViewUpdate::Status(Some(_))
        ));
        release.send(()).unwrap();

        await_value(|| {
            matches!(fixture.poll(&mut switch, false), SavedViewUpdate::Switch).then_some(())
        });
        let (session, selection) = switch.take_next().unwrap();
        assert_eq!(session.input.contest.contest_id, "abc483");
        assert_eq!(session.run_generation.scope(), &newest.target.scope);
        assert_eq!(selection.target, newest.target);
        assert!(older.results.is_registered());
        assert!(fixture.session.results.is_registered());
        assert!(matches!(
            fixture.poll(&mut switch, false),
            SavedViewUpdate::Status(None)
        ));
    }

    #[test]
    fn latest_failure_keeps_current_view_and_never_reuses_the_earlier_candidate() {
        let fixture = Fixture::new();
        let mut switch = fixture.coordinator();
        let older = fixture.save(&switch, "abc482");
        let release = fixture.pause_prepared(&mut switch, older);
        let newest = fixture.save(&switch, "abc483");
        fs::remove_file(&newest.path).unwrap();
        release.send(()).unwrap();

        let failure = await_value(|| match fixture.poll(&mut switch, false) {
            SavedViewUpdate::Status(Some(status)) if status.contains("failed") => Some(status),
            SavedViewUpdate::Switch | SavedViewUpdate::Select(_) => {
                panic!("invalid source changed the view")
            }
            _ => None,
        });
        assert!(failure.contains("abc483") && failure.contains("no longer exists"));
        assert!(switch.take_next().is_none());
        assert_eq!(fixture.session.input.contest.contest_id, "abc481");
        assert!(
            matches!(fixture.poll(&mut switch, false), SavedViewUpdate::Status(Some(status)) if status == failure)
        );
    }

    #[test]
    fn manual_commit_fences_in_flight_preparation_and_only_a_later_save_can_switch() {
        let fixture = Fixture::new();
        let mut switch = fixture.coordinator();
        let earlier = fixture.save(&switch, "abc482");
        let release = fixture.pause_prepared(&mut switch, earlier.clone());
        let deferred = fixture.save(&switch, "abc483");
        assert!(
            matches!(fixture.poll(&mut switch, true), SavedViewUpdate::Status(Some(status)) if status.contains("hold"))
        );
        switch.update(
            SavedViewEvent::ManualCommit,
            fixture.visible().scope(),
            &fixture.session.run_tx,
        );
        release.send(()).unwrap();
        await_value(|| {
            assert!(matches!(
                fixture.poll(&mut switch, false),
                SavedViewUpdate::Status(None)
            ));
            switch.job.is_none().then_some(())
        });
        assert_eq!(switch.handled_through, deferred.sequence);
        assert!(earlier.results.is_registered());
        assert!(deferred.results.is_registered());
        let next = fixture.save(&switch, "abc482");
        await_value(|| {
            matches!(fixture.poll(&mut switch, false), SavedViewUpdate::Switch).then_some(())
        });
        assert_eq!(switch.take_next().unwrap().1.target, next.target);
    }

    #[test]
    fn refresh_retirement_rejects_an_already_prepared_view() {
        let fixture = Fixture::new();
        let mut switch = fixture.coordinator();
        let prior = fixture.save(&switch, "abc482");
        let release = fixture.pause_prepared(&mut switch, prior.clone());
        fixture
            .runs
            .workspace_watcher
            .borrow()
            .as_ref()
            .unwrap()
            .pause("abc482")
            .unwrap();
        release.send(()).unwrap();
        await_value(|| match fixture.poll(&mut switch, false) {
            SavedViewUpdate::Status(Some(status)) if status.contains("expired") => Some(()),
            SavedViewUpdate::Switch | SavedViewUpdate::Select(_) => {
                panic!("expired source changed the view")
            }
            _ => None,
        });
        assert!(switch.take_next().is_none());
        assert!(!prior.results.is_registered());
        assert!(fixture.session.results.is_registered());
    }
}
