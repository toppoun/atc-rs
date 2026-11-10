use super::*;
use crate::tui::{test_key_enter as enter, test_key_escape as esc, test_key_press as key};
use std::fs;

struct Fixture {
    root: tempfile::TempDir,
    store: RegistryStore,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = test_registry(root.path());
        Self { root, store }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        fs::create_dir(&path).unwrap();
        write_empty_workspace(&path);
        path
    }

    fn registry_file(&self) -> PathBuf {
        crate::paths::RegistryLocation::from_platform_base(self.root.path().to_path_buf())
            .unwrap()
            .file()
    }

    fn entries(&self) -> Vec<(PathBuf, Option<i64>)> {
        match self.store.load().unwrap() {
            RegistryLoad::Missing => Vec::new(),
            RegistryLoad::Snapshot(snapshot) => snapshot
                .entries_by_recent_use()
                .iter()
                .map(|entry| (entry.root().to_path_buf(), entry.last_used_seq()))
                .collect(),
        }
    }

    fn register(&self, path: &Path, used: bool) -> EntryRef {
        let checked = workspace::inspect_launcher_workspace(path).unwrap();
        assert!(checked.initialized);
        let evidence =
            VerifiedWorkspace::from_verified_root(path.to_path_buf(), checked.identity).unwrap();
        let mutation = if used {
            RegistryMutation::RecordDirectWorkspaceOpen(&evidence)
        } else {
            RegistryMutation::Register(&evidence)
        };
        match self.store.apply(mutation).unwrap() {
            RegistryApply::Applied {
                result:
                    RegistryMutationResult::Recorded(entry) | RegistryMutationResult::Registered(entry),
                ..
            } => entry,
            outcome => panic!("fixture registration failed: {outcome:?}"),
        }
    }

    fn run(&self, root: &Path, terminal: BareTerminalSpy) -> Result<AppLocation, AppError> {
        run_application_at_with(root, |mut location| {
            run_bare_application(
                &mut location,
                &mut crate::tui::FrontendPreferences::default(),
                Ok(&self.store),
                || Ok(terminal),
            )?;
            Ok(location)
        })
    }

    fn launch(&self, terminal: BareTerminalSpy) -> Result<AppLocation, AppError> {
        self.run(self.root.path(), terminal)
    }
}

#[test]
fn displayed_recent_order_includes_unopened_entries_without_inspecting_their_folders() {
    let fixture = Fixture::new();
    let unopened = fixture.workspace("unused-one");
    let old = fixture.workspace("old-workspace");
    let new = fixture.workspace("latest-workspace");
    let later_unopened = fixture.workspace("unused-two");
    fixture.register(&unopened, false);
    fixture.register(&old, true);
    fixture.register(&new, true);
    fixture.register(&later_unopened, false);
    fs::rename(&old, fixture.root.path().join("moved")).unwrap();
    let baseline = fs::read(fixture.registry_file()).unwrap();

    let probe = BareApplicationProbe::default();
    let terminal = BareTerminalSpy::new([vec![key('j'), key('q')]], probe.clone());
    assert!(matches!(
        fixture.launch(terminal).unwrap(),
        AppLocation::GlobalHome(_)
    ));
    let frames = probe.rendered_global_frames.borrow();
    let first = &frames[0];
    let rows = [
        "latest-workspace",
        "old-workspace",
        "unused-one",
        "unused-two",
        "Create Workspace",
    ];
    let positions: Vec<_> = rows.iter().map(|row| first.find(row).unwrap()).collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(first.contains(&new.display().to_string()));
    assert!(frames[1].contains("> old-workspace"));
    assert_eq!(fs::read(fixture.registry_file()).unwrap(), baseline);
}

#[test]
fn both_native_picker_actions_validate_and_open_without_overwriting_the_marker() {
    for trigger in [enter(), key('o')] {
        let fixture = Fixture::new();
        let selected = fixture.workspace("picked");
        let marker = selected.join(".atc-workspace.toml");
        let baseline = fs::read(&marker).unwrap();
        let probe = BareApplicationProbe::default();
        let mut terminal = BareTerminalSpy::new(
            [vec![trigger, key('o'), enter(), key('q')], vec![key('q')]],
            probe.clone(),
        );
        terminal.folder_dialog = Some(FolderDialogResult::Selected(selected.clone()));
        let AppLocation::Workspace(runtime) = fixture.launch(terminal).unwrap() else {
            panic!("folder selection must open Workspace Home");
        };
        assert_eq!(
            runtime.app_context.workspace_root(),
            Some(selected.as_path())
        );
        assert_eq!(*probe.folder_steps.borrow(), ["suspend", "resume"]);
        assert_eq!(probe.workspace_reads.get(), 1);
        assert_eq!(probe.contest_frontend_dispatches.get(), 0);
        assert_eq!(fixture.entries(), [(selected, Some(2))]);
        assert_eq!(fs::read(marker).unwrap(), baseline);
    }
}

#[test]
fn cancelling_a_native_selection_keeps_the_launcher_and_does_not_register() {
    let fixture = Fixture::new();
    let probe = BareApplicationProbe::default();
    let mut terminal = BareTerminalSpy::new(
        [
            vec![key('o'), key('q'), enter()],
            vec![key('?')],
            vec![esc(), key('q')],
        ],
        probe.clone(),
    );
    terminal.folder_dialog = Some(FolderDialogResult::Cancelled);
    assert!(matches!(
        fixture.launch(terminal).unwrap(),
        AppLocation::GlobalHome(_)
    ));
    let frames = probe.rendered_global_frames.borrow();
    assert!(frames[1].contains("> Create Workspace"));
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("Enter Select") && frame.contains("d Remove from List"))
    );
    assert!(
        frames
            .iter()
            .all(|frame| !frame.contains("Folder Dialog Failed"))
    );
    assert_eq!(*probe.folder_steps.borrow(), ["suspend", "resume"]);
    assert!(fixture.entries().is_empty());
}

#[test]
fn unavailable_picker_opens_explorer_and_failed_picker_waits_for_an_explicit_switch() {
    for unavailable in [true, false] {
        let fixture = Fixture::new();
        let probe = BareApplicationProbe::default();
        let mut batches = vec![vec![key('o'), key('b')]];
        if !unavailable {
            batches.push(vec![key('b')]);
        }
        // The fallback accepts the same Explorer keys, including reload and expansion.
        batches.extend([vec![enter(), key('r')], vec![esc(), key('q')]]);
        let mut terminal = BareTerminalSpy::new(batches, probe.clone());
        terminal.folder_dialog = Some(if unavailable {
            FolderDialogResult::Unavailable
        } else {
            FolderDialogResult::Failed(FolderDialogFailure::new(io::Error::other(
                "picker failure detail",
            )))
        });

        let AppLocation::GlobalHome(state) = fixture.launch(terminal).unwrap() else {
            panic!("Explorer fallback must return to Global Home");
        };
        assert_eq!(state.explorer_root(), fixture.root.path());
        let frames = probe.rendered_global_frames.borrow();
        assert_eq!(frames[1].contains("Folder Dialog Failed"), !unavailable);
        assert_eq!(frames[1].contains("o Open   g Go to Path"), unavailable);
        assert!(
            frames
                .iter()
                .any(|frame| frame.contains("o Open   g Go to Path"))
        );
        assert!(
            frames
                .iter()
                .all(|frame| !frame.contains("picker failure detail"))
        );
        assert!(fixture.entries().is_empty());
    }
}

#[test]
fn terminal_resume_failure_uses_cleanup_and_drops_the_selection() {
    let fixture = Fixture::new();
    let selected = fixture.workspace("picked");
    let probe = BareApplicationProbe::default();
    let mut terminal = BareTerminalSpy::new([vec![key('o'), enter()]], probe.clone());
    terminal.folder_dialog = Some(FolderDialogResult::Selected(selected));
    terminal.resume_error = true;
    let error = fixture
        .launch(terminal)
        .err()
        .expect("failed resume must be fatal");
    assert!(
        error
            .to_string()
            .contains("scripted folder dialog resume failed")
    );
    assert_eq!(probe.global_draws.get(), 1);
    assert_eq!(probe.workspace_draws.get(), 0);
    assert_eq!(probe.terminal_restores.get(), 1);
    assert!(fixture.entries().is_empty());
}

#[test]
fn direct_launch_records_only_successful_valid_home_draws() {
    let fixture = Fixture::new();
    let valid = fixture.workspace("valid");
    let invalid = fixture.workspace("invalid");
    fs::write(invalid.join(".atc-workspace.toml"), "broken").unwrap();
    let probe = BareApplicationProbe::default();
    let mut bad_draw = BareTerminalSpy::new([], probe.clone());
    bad_draw.workspace_draw_error = true;
    assert!(fixture.run(&valid, bad_draw).is_err());
    assert!(fixture.entries().is_empty());
    let quit = || BareTerminalSpy::new([vec![key('q')]], probe.clone());
    assert!(matches!(
        fixture.run(&invalid, quit()).unwrap(),
        AppLocation::Workspace(_)
    ));
    assert!(fixture.entries().is_empty());
    fixture.run(&valid, quit()).unwrap();
    fixture.run(&valid, quit()).unwrap();
    assert_eq!(fixture.entries(), [(valid, Some(3))]);
    assert_eq!(probe.global_draws.get(), 0);
}

#[test]
fn recent_open_updates_use_only_after_the_home_draw() {
    let fixture = Fixture::new();
    let path = fixture.workspace("recent");
    fixture.register(&path, false);
    let probe = BareApplicationProbe::default();
    let mut failed_draw = BareTerminalSpy::new([vec![enter()]], probe.clone());
    failed_draw.workspace_draw_error = true;
    assert!(fixture.launch(failed_draw).is_err());
    assert_eq!(fixture.entries(), [(path.clone(), None)]);
    let terminal = BareTerminalSpy::new([vec![enter(), key('q')], vec![key('q')]], probe.clone());
    assert!(matches!(
        fixture.launch(terminal).unwrap(),
        AppLocation::Workspace(_)
    ));
    assert_eq!(fixture.entries(), [(path, Some(2))]);
    assert_eq!(probe.workspace_reads.get(), 1);
}

#[test]
fn corrupt_registry_warns_in_launcher_and_direct_home_and_preserves_the_bytes() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("valid");
    let registry = fixture.registry_file();
    fs::create_dir_all(registry.parent().unwrap()).unwrap();
    fs::write(&registry, "broken = [").unwrap();
    let probe = BareApplicationProbe::default();
    let terminal = || BareTerminalSpy::new([vec![key('q')]], probe.clone());
    assert!(matches!(
        fixture.launch(terminal()).unwrap(),
        AppLocation::GlobalHome(_)
    ));
    let frames = probe.rendered_global_frames.borrow();
    assert!(frames[0].contains("Workspace Registry unavailable"));
    assert!(!frames[0].contains("No registered Workspaces"));
    assert!(matches!(
        fixture.run(&workspace, terminal()).unwrap(),
        AppLocation::Workspace(_)
    ));
    assert!(
        probe
            .rendered_workspace_frames
            .borrow()
            .iter()
            .any(|frame| frame.contains("Workspace history could not be saved"))
    );
    assert_eq!(fs::read_to_string(registry).unwrap(), "broken = [");
}

#[test]
fn workspace_selection_rejects_missing_replaced_and_uninitialized_recent_folders() {
    for (change, expected) in [
        ("move", "missing or has moved"),
        ("replace", "A different folder now exists"),
        ("marker", "Workspace marker .atc-workspace.toml is missing"),
    ] {
        let fixture = Fixture::new();
        let path = fixture.workspace("registered");
        fixture.register(&path, true);
        if change == "marker" {
            fs::remove_file(path.join(".atc-workspace.toml")).unwrap();
        } else {
            fs::rename(&path, fixture.root.path().join("moved")).unwrap();
            if change == "replace" {
                fixture.workspace("registered");
            }
        }
        let baseline = fs::read(fixture.registry_file()).unwrap();
        let probe = BareApplicationProbe::default();
        let terminal = BareTerminalSpy::new([vec![enter()], vec![esc(), key('q')]], probe.clone());
        assert!(matches!(
            fixture.launch(terminal).unwrap(),
            AppLocation::GlobalHome(_)
        ));
        let frames = probe.rendered_global_frames.borrow();
        assert!(
            frames.iter().any(|frame| frame.contains(expected)),
            "{change}"
        );
        assert!(
            frames
                .iter()
                .all(|frame| !frame.contains("Initialize Workspace"))
        );
        assert_eq!(fs::read(fixture.registry_file()).unwrap(), baseline);
    }
}

#[test]
fn removed_entry_is_never_recreated_by_a_stale_launcher_or_open_callback() {
    for after_transition in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.workspace("registered");
        let entry = fixture.register(&path, true);
        let store = test_registry(fixture.root.path());
        let remove = move || {
            assert!(matches!(
                store.apply(RegistryMutation::Remove(&entry)).unwrap(),
                RegistryApply::Applied {
                    result: RegistryMutationResult::Removed,
                    ..
                }
            ));
        };
        let probe = BareApplicationProbe::default();
        let mut terminal =
            BareTerminalSpy::new([vec![enter()], vec![esc(), key('q')]], probe.clone());
        if after_transition {
            terminal.before_workspace_draw = Some(Box::new(remove));
        } else {
            terminal.before_global_input = Some(Box::new(remove));
        }
        let result = fixture.launch(terminal).unwrap();
        assert_eq!(
            matches!(result, AppLocation::Workspace(_)),
            after_transition
        );
        let frames = if after_transition {
            &probe.rendered_workspace_frames
        } else {
            &probe.rendered_global_frames
        };
        assert!(
            frames
                .borrow()
                .iter()
                .any(|frame| frame.contains("reload Recent Workspaces")
                    || frame.contains("Reload Recent Workspaces"))
        );
        assert!(fixture.entries().is_empty());
    }
}

#[test]
fn remove_from_list_confirms_only_after_success_and_never_touches_workspace_files() {
    for fail_save in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.workspace("registered");
        fixture.register(&path, true);
        let marker = path.join(".atc-workspace.toml");
        let baseline = fs::read(&marker).unwrap();
        let probe = BareApplicationProbe::default();
        let mut terminal = BareTerminalSpy::new(
            [
                vec![key('d')],
                vec![esc()],
                vec![key('d')],
                vec![key('y'), enter()],
                vec![esc(), key('q')],
            ],
            probe.clone(),
        );
        if fail_save {
            let lock = fixture.registry_file().with_file_name(".workspaces.lock");
            terminal.before_global_input = Some(Box::new(move || {
                fs::remove_file(&lock).unwrap();
                fs::create_dir(&lock).unwrap();
            }));
        }
        assert!(matches!(
            fixture.launch(terminal).unwrap(),
            AppLocation::GlobalHome(_)
        ));
        let frames = probe.rendered_global_frames.borrow();
        assert!(frames.iter().any(|frame| frame.contains("Remove from List")
            && frame.contains("Workspace files remain on disk")));
        assert_eq!(
            frames
                .iter()
                .any(|frame| frame.contains("Removed from list.")),
            !fail_save
        );
        assert_eq!(
            frames
                .iter()
                .any(|frame| frame.contains("Remove from List Failed")),
            fail_save
        );
        if fail_save {
            fs::remove_dir(fixture.registry_file().with_file_name(".workspaces.lock")).unwrap();
        }
        assert_eq!(fixture.entries().len(), usize::from(fail_save));
        assert_eq!(fs::read(marker).unwrap(), baseline);
    }
}

#[test]
fn explicit_open_survives_a_registry_write_error() {
    let fixture = Fixture::new();
    let selected = fixture.workspace("selected");
    let file = fixture.registry_file();
    let lock = file.with_file_name(".workspaces.lock");
    let probe = BareApplicationProbe::default();
    let mut terminal = BareTerminalSpy::new([vec![key('o')], vec![key('q')]], probe.clone());
    terminal.folder_dialog = Some(FolderDialogResult::Selected(selected.clone()));
    terminal.before_workspace_draw = Some(Box::new(move || fs::create_dir_all(lock).unwrap()));
    let AppLocation::Workspace(runtime) = fixture.launch(terminal).unwrap() else {
        panic!("Registry write failure must keep Workspace Home available");
    };
    assert_eq!(
        runtime.app_context.workspace_root(),
        Some(selected.as_path())
    );
    assert!(
        probe
            .rendered_workspace_frames
            .borrow()
            .iter()
            .any(|frame| { frame.contains("Workspace history could not be saved") })
    );
    assert!(!file.exists());
}

#[test]
fn recovery_required_preserves_the_candidate_and_never_presents_an_empty_registry() {
    let fixture = Fixture::new();
    let selected = fixture.workspace("selected");
    let file = fixture.registry_file();
    let candidate = file.with_file_name(".workspaces-staging-replace-fixture");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&candidate, "recovery data").unwrap();
    let probe = BareApplicationProbe::default();
    let mut terminal = BareTerminalSpy::new([vec![key('o')], vec![key('q')]], probe.clone());
    terminal.folder_dialog = Some(FolderDialogResult::Selected(selected));
    assert!(matches!(
        fixture.launch(terminal).unwrap(),
        AppLocation::Workspace(_)
    ));
    let rendered = probe.rendered_global_frames.borrow();
    assert!(rendered[0].contains("Workspace Registry unavailable"));
    assert!(!rendered[0].contains("No registered Workspaces"));
    assert!(
        probe
            .rendered_workspace_frames
            .borrow()
            .iter()
            .any(|frame| { frame.contains("Workspace history could not be saved") })
    );
    assert_eq!(fs::read_to_string(candidate).unwrap(), "recovery data");
    assert!(!file.exists());
}

#[cfg(unix)]
#[test]
fn explicit_selection_rejects_a_root_symlink_with_or_without_trailing_components() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("real-workspace");
    let linked = fixture.root.path().join("linked");
    std::os::unix::fs::symlink(&workspace, &linked).unwrap();
    for selected in [linked.clone(), linked.join(""), linked.join(".")] {
        let probe = BareApplicationProbe::default();
        let mut terminal =
            BareTerminalSpy::new([vec![key('o')], vec![esc(), key('q')]], probe.clone());
        terminal.folder_dialog = Some(FolderDialogResult::Selected(selected));
        assert!(matches!(
            fixture.launch(terminal).unwrap(),
            AppLocation::GlobalHome(_)
        ));
        assert_eq!(probe.workspace_draws.get(), 0);
        assert!(
            probe
                .rendered_global_frames
                .borrow()
                .iter()
                .any(|frame| frame.contains("Workspace Open Failed"))
        );
    }
    assert!(fixture.entries().is_empty());
}
