use super::*;
use crate::tui::detail::{DetailDocument, DetailSectionKind};
use crate::tui::message::{
    RunKind, UserInputRunEvent, UserInputRunResult, UserInputRunStatus as Status,
    UserInputRunTarget,
};

fn fixture(inputs: &[(u64, &str)]) -> (tempfile::TempDir, PathBuf, WatchApp) {
    let (temp, destination) = user_input_workspace();
    if !inputs.is_empty() {
        write_sync_inputs(&destination, "A", inputs);
    }
    let mut app = loaded_user_input_app(&destination);
    let source = destination.join("A.py");
    fs::write(&source, "pass\n").unwrap();
    app.source_changed(0, source, Language::Python);
    (temp, destination, app)
}

#[test]
fn run_without_selected_source_reports_failure_on_the_input_without_storage_or_sample_changes() {
    let mut app = user_input_app("input");
    app.begin_selected_user_input_edit().unwrap();
    assert!(run_selected_user_input(&mut app, None));
    assert!(app.take_user_input_run_request().is_none());
    assert_eq!(last(&app, 3).unwrap().status, Status::Failed);
    assert!(text(&app).contains("No source is selected"));
    assert!(sync_notice(&app).is_none());
    assert!(app.user_input_editor_active());
}

#[test]
fn source_change_is_problem_local_and_clears_draft_result_without_changing_its_buffer() {
    let (_temp, destination) = user_input_workspace();
    write_sync_inputs(&destination, "A", &[(7, "seven")]);
    write_sync_inputs(&destination, "B", &[(7, "other problem")]);
    let mut app = sync_test_app(&destination, &[0, 0]);
    for (problem, index) in [(0, "A"), (1, "B")] {
        let path = destination.join(format!("{index}.py"));
        fs::write(&path, "pass").unwrap();
        app.source_changed(problem, path, Language::Python);
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        complete(&mut app, &request, index);
    }
    app.select_problem(0);
    app.begin_new_user_input().unwrap();
    app.edit_user_input_insert("draft\r\n");
    run_selected_user_input(&mut app, None);
    let request = take_run(&mut app);
    complete(&mut app, &request, "draft result");
    let editor = app.selected_user_input_edit().unwrap().clone();
    fs::write(destination.join("A.py"), "changed").unwrap();
    app.source_changed(0, destination.join("A.py"), Language::Python);
    let ready = app.problems()[0].user_inputs.ready().unwrap();
    assert!(
        ready
            .last_run(app::UserInputSelection::Persisted(7))
            .is_none()
    );
    assert!(ready.last_run(app::UserInputSelection::Draft).is_none());
    assert_eq!(ready.edit(), Some(&editor));
    assert_eq!(
        app.problems()[1]
            .user_inputs
            .ready()
            .unwrap()
            .last_run(app::UserInputSelection::Persisted(7))
            .unwrap()
            .stdout,
        "B"
    );
    assert!(app.take_user_input_run_request().is_none());
}

fn take_run(app: &mut WatchApp) -> RunRequest {
    let request = app.take_user_input_run_request().expect("one Run required");
    assert!(app.take_user_input_run_request().is_none(), "duplicate Run");
    assert!(matches!(request.kind, RunKind::UserInput(_)));
    request
}

fn stdin(request: &RunRequest) -> &str {
    let RunKind::UserInput(snapshot) = &request.kind else {
        panic!("not a User Input run")
    };
    &snapshot.input
}

fn result_event(output: &str) -> UserInputRunEvent {
    UserInputRunEvent::Finished(UserInputRunResult {
        status: Status::Finished,
        stdout: output.to_string(),
        stderr: "diagnostic".to_string(),
        elapsed: Duration::from_millis(12),
    })
}

fn complete(app: &mut WatchApp, request: &RunRequest, output: &str) {
    assert!(app.run_started(request.problem, request.run_id));
    assert!(apply_event(app, request, result_event(output)));
}

fn snapshot(request: &RunRequest) -> &Arc<message::UserInputRunSnapshot> {
    let RunKind::UserInput(snapshot) = &request.kind else {
        panic!("User Input required")
    };
    snapshot
}

fn apply_event(app: &mut WatchApp, request: &RunRequest, event: UserInputRunEvent) -> bool {
    app.user_input_run_event(request.problem, request.run_id, snapshot(request), event)
}

fn last(app: &WatchApp, id: u64) -> Option<&app::UserInputRunState> {
    app.current_problem()
        .unwrap()
        .user_inputs
        .ready()
        .unwrap()
        .last_run(app::UserInputSelection::Persisted(id))
}

fn text(app: &WatchApp) -> String {
    DetailDocument::from_app(app)
        .segments()
        .map(|segment| segment.text())
        .collect()
}

fn storage_tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, at: &Path, result: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.insert(path.strip_prefix(root).unwrap().to_path_buf(), Vec::new());
                visit(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut result = Default::default();
    visit(root, root, &mut result);
    result
}

#[test]
fn readonly_run_owns_exact_stdin_and_does_not_write_storage() {
    let exact = "1\t2\r\n界😀\n\n ";
    let (_temp, destination, mut app) = fixture(&[(7, exact)]);
    use_echo_source(&destination);
    let before = storage_tree(&destination);
    assert!(run_selected_user_input(&mut app, Some(&destination)));
    let request = take_run(&mut app);
    assert_eq!(stdin(&request).as_bytes(), exact.as_bytes());
    assert_eq!(before, storage_tree(&destination));
    assert_eq!(app.current_problem().unwrap().total_cases, 0);
    assert!(app.current_problem().unwrap().run.cases.is_empty());
    let RunKind::UserInput(snapshot) = &request.kind else {
        unreachable!()
    };
    assert_eq!(snapshot.target, UserInputRunTarget::Persisted(7));
    assert!(!request.kind.preserve_on_preemption());
    let messages = execute_echo(&destination, &request);
    deliver(&mut app, &destination, messages);
    assert_eq!(last(&app, 7).unwrap().stdout.as_bytes(), exact.as_bytes());
    assert_eq!(before, storage_tree(&destination));
}

#[test]
fn readonly_run_sync_uses_latest_disk_content() {
    let (_temp, destination, mut app) = fixture(&[(7, "cached")]);
    fs::write(
        destination.join(".atc/user-inputs/A/7.in"),
        "latest\r\n\t界\n",
    )
    .unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    assert_eq!(stdin(&take_run(&mut app)), "latest\r\n\t界\n");
}

#[test]
fn readonly_run_deleted_target_never_runs_the_fallback_row() {
    let (_temp, destination, mut app) = fixture(&[(7, "deleted"), (9, "fallback")]);
    fs::remove_file(destination.join(".atc/user-inputs/A/7.in")).unwrap();
    assert!(run_selected_user_input(&mut app, Some(&destination)));
    assert!(app.take_user_input_run_request().is_none());
    assert_eq!(
        app.selected_user_input(),
        Some(app::UserInputSelection::Persisted(9))
    );
    assert!(sync_notice(&app).unwrap().contains("removed externally"));
}

#[test]
fn readonly_run_sync_failure_leaves_cache_and_result_untouched() {
    let (_temp, destination, mut app) = fixture(&[(7, "cached")]);
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    complete(&mut app, &request, "old output");
    let before = app.current_problem().unwrap().user_inputs.clone();
    fs::write(destination.join(".atc/user-inputs/A/7.in"), [0xff]).unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    assert!(app.take_user_input_run_request().is_none());
    assert_eq!(app.current_problem().unwrap().user_inputs, before);
    assert!(sync_notice(&app).unwrap().contains("Could not refresh"));
}

#[test]
fn dirty_run_skips_sync_owns_buffer_and_never_saves() {
    let (_temp, destination, mut app) = fixture(&[(7, "disk\n")]);
    use_echo_source(&destination);
    app.begin_selected_user_input_edit().unwrap();
    app.edit_user_input_insert("1\t2\r\n界😀\n");
    let exact = app.selected_user_input_edit().unwrap().buffer().to_string();
    let before = storage_tree(&destination);
    assert!(run_selected_user_input(&mut app, None)); // no filesystem context is needed
    let request = take_run(&mut app);
    app.edit_user_input_insert("later edit");
    assert_eq!(stdin(&request), exact);
    assert_eq!(before, storage_tree(&destination));
    assert!(app.user_input_editor_active());
    let messages = execute_echo(&destination, &request);
    deliver(&mut app, &destination, messages);
    assert!(last(&app, 7).is_none()); // Buffer changed after dispatch; completion is stale.
    assert_eq!(before, storage_tree(&destination));
}

#[test]
fn draft_run_never_creates_storage_or_allocates_persisted_id() {
    let (_temp, destination, mut app) = fixture(&[]);
    use_echo_source(&destination);
    app.begin_new_user_input().unwrap();
    app.edit_user_input_insert("1\t2\r\n界😀\n");
    let before = storage_tree(&destination);
    run_selected_user_input(&mut app, None);
    let request = take_run(&mut app);
    assert_eq!(stdin(&request), "1\t2\r\n界😀\n");
    assert!(!destination.join(".atc/user-inputs").exists());
    assert_eq!(before, storage_tree(&destination));
    assert!(
        app.current_problem()
            .unwrap()
            .user_inputs
            .ready()
            .unwrap()
            .persisted()
            .is_empty()
    );
    let messages = execute_echo(&destination, &request);
    deliver(&mut app, &destination, messages);
    assert_eq!(before, storage_tree(&destination));
}

#[test]
fn output_stderr_elapsed_and_execution_status_have_no_judgement_or_expected() {
    for status in [
        Status::Finished,
        Status::RuntimeError,
        Status::TimedOut,
        Status::CompileError,
        Status::CompileTimedOut,
        Status::Cancelled,
        Status::Failed,
    ] {
        let (_temp, destination, mut app) = fixture(&[(7, "input")]);
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();
        tx.send(Message::UserInputRunEvent {
            problem: 0,
            run_id: request.run_id,
            snapshot: Arc::clone(snapshot(&request)),
            event: UserInputRunEvent::Finished(UserInputRunResult {
                status,
                stdout: "output text".to_string(),
                stderr: "stderr text".to_string(),
                elapsed: Duration::from_millis(12),
            }),
        })
        .unwrap();
        handle_messages(&mut app, &rx, &run_tx).unwrap();
        let rendered = text(&app);
        assert_eq!(
            rendered.split_once("\n\n").unwrap().1,
            format!(
                "{}   12.0 ms\n\n▼ Input\ninput\n\n▼ Output\noutput text\n\n▼ Stderr\nstderr text",
                status.label()
            )
        );
        for forbidden in ["Expected", "AC", "WA"] {
            assert!(!rendered.contains(forbidden), "{rendered}");
        }
        assert!(run_rx.try_recv().is_err());
        assert!(app.current_problem().unwrap().run.cases.is_empty());
    }
}

#[test]
fn active_status_precedes_input_without_output_or_stderr() {
    let (_temp, destination, mut app) = fixture(&[(7, "input\n")]);
    app.source_changed(0, destination.join("A.cpp"), Language::Cpp);
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    for status in [Status::Queued, Status::Compiling, Status::Running] {
        match status {
            Status::Compiling => assert!(app.run_started(0, request.run_id)),
            Status::Running => assert!(apply_event(&mut app, &request, UserInputRunEvent::Running)),
            _ => {}
        }
        let rendered = text(&app);
        assert_eq!(
            rendered.split_once("\n\n").unwrap().1,
            format!("{}\n\n▼ Input\ninput\n", status.label())
        );
        for forbidden in ["Output", "Stderr", "Expected", "AC", "WA"] {
            assert!(!rendered.contains(forbidden), "{rendered}");
        }
    }
}

#[test]
fn runner_diagnostic_precedes_input_and_keeps_save_error_separate() {
    for editing in [false, true] {
        let (_temp, destination, mut app) = fixture(&[(7, "input")]);
        if editing {
            app.begin_selected_user_input_edit().unwrap();
            let snapshot = app.user_input_save_snapshot().unwrap();
            app.fail_user_input_save(&snapshot, "permission denied".into(), None)
                .unwrap();
        }
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        assert!(app.run_failed(0, request.run_id, "could not launch candidate".into()));
        let rendered = text(&app);
        let input = if editing {
            "Save failed: permission denied\n\n▼ Input — Editing"
        } else {
            "▼ Input"
        };
        assert_eq!(
            rendered.split_once("\n\n").unwrap().1,
            format!(
                "Failed\nError: could not launch candidate\n\n{input}\ninput\n\n▼ Output\n(empty)"
            )
        );
        assert!(!rendered.contains("Stderr"));
    }
}

#[test]
fn empty_stderr_is_omitted_and_output_folds_use_existing_sections() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    apply_event(
        &mut app,
        &request,
        UserInputRunEvent::Finished(UserInputRunResult {
            status: Status::Finished,
            stdout: String::new(),
            stderr: String::new(),
            elapsed: Duration::ZERO,
        }),
    );
    assert!(text(&app).contains("▼ Output\n(empty)"));
    assert!(!text(&app).contains("Stderr"));
    app.toggle_detail_section(DetailSectionKind::Actual);
    assert!(text(&app).contains("▶ Output"));
}

#[test]
fn each_stable_input_retains_its_own_result_across_selection_and_renumbering() {
    let (_temp, destination, mut app) = fixture(&[(2, "two"), (7, "seven"), (9, "nine")]);
    for id in [7, 9] {
        select_sync_input(&mut app, id);
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        complete(&mut app, &request, &format!("output {id}"));
    }
    fs::remove_file(destination.join(".atc/user-inputs/A/2.in")).unwrap();
    sync_user_inputs_for_problem(&mut app, Some(&destination), 0);
    for id in [7, 9, 7] {
        select_sync_input(&mut app, id);
        assert_eq!(last(&app, id).unwrap().stdout, format!("output {id}"));
        assert!(text(&app).contains(&format!("output {id}")));
    }
}

#[test]
fn save_success_draft_saved_and_unchanged_each_queue_one_exact_snapshot() {
    for mode in ["draft", "saved", "unchanged"] {
        let (_temp, destination, mut app) = fixture(&[(7, "old\r\n")]);
        if mode == "draft" {
            app.begin_new_user_input().unwrap();
        } else {
            app.begin_selected_user_input_edit().unwrap();
        }
        if mode != "unchanged" {
            app.edit_user_input_insert("new\t界\n");
        }
        let snapshot = app.user_input_save_snapshot().unwrap();
        assert!(save_selected_user_input(&mut app, &destination));
        assert!(!app.user_input_editor_active());
        let request = take_run(&mut app);
        assert_eq!(stdin(&request), snapshot.content);
        let Some(app::UserInputSelection::Persisted(id)) = app.selected_user_input() else {
            panic!()
        };
        let RunKind::UserInput(run_snapshot) = &request.kind else {
            panic!()
        };
        assert_eq!(run_snapshot.target, UserInputRunTarget::Persisted(id));
        assert_eq!(
            fs::read(destination.join(format!(".atc/user-inputs/A/{id}.in"))).unwrap(),
            snapshot.content.as_bytes()
        );
        assert!(!save_selected_user_input(&mut app, &destination));
        assert!(app.take_user_input_run_request().is_none());
        fs::write(
            destination.join(format!(".atc/user-inputs/A/{id}.in")),
            "later disk change",
        )
        .unwrap();
        assert_eq!(stdin(&request), snapshot.content);
    }
}

#[test]
fn save_conflict_missing_and_io_failure_never_queue_run() {
    for mode in ["conflict", "missing", "io"] {
        let (_temp, destination, mut app) = fixture(&[(7, "baseline")]);
        app.begin_selected_user_input_edit().unwrap();
        app.edit_user_input_insert("edit");
        let path = destination.join(".atc/user-inputs/A/7.in");
        match mode {
            "conflict" => fs::write(&path, "external").unwrap(),
            "missing" => fs::remove_file(&path).unwrap(),
            _ => fs::write(&path, [0xff]).unwrap(),
        }
        assert!(save_selected_user_input(&mut app, &destination));
        assert!(app.take_user_input_run_request().is_none());
        assert!(app.user_input_editor_active());
        assert!(
            app.selected_user_input_edit()
                .unwrap()
                .save_error()
                .is_some()
        );
        assert_eq!(
            app.selected_user_input_edit().unwrap().buffer(),
            "baselineedit"
        );
        if mode == "missing" {
            assert_eq!(
                app.selected_user_input(),
                Some(app::UserInputSelection::Draft)
            );
        }
    }
}

#[test]
fn draft_preinstall_failure_and_ambiguous_install_never_run() {
    let (_temp, destination, mut app) = fixture(&[]);
    app.begin_new_user_input().unwrap();
    app.edit_user_input_insert("draft");
    fs::remove_dir(destination.join(".atc")).unwrap();
    save_selected_user_input(&mut app, &destination);
    assert!(app.take_user_input_run_request().is_none());
    fs::create_dir(destination.join(".atc")).unwrap();
    let snapshot = app.user_input_save_snapshot().unwrap();
    reconcile_draft_create_after_install(
        &mut app,
        &destination,
        &snapshot,
        99,
        io::Error::other("after install"),
    );
    save_selected_user_input(&mut app, &destination);
    assert!(app.take_user_input_run_request().is_none());
    assert!(app.user_input_editor_active());
}

#[test]
fn partial_save_success_converges_once_for_draft_and_persisted() {
    for draft in [true, false] {
        let (_temp, destination, mut app) = fixture(&[(7, "old")]);
        if draft {
            app.begin_new_user_input().unwrap();
        } else {
            app.begin_selected_user_input_edit().unwrap();
        }
        app.edit_user_input_insert("saved\r\n");
        let snapshot = app.user_input_save_snapshot().unwrap();
        if draft {
            let failure = crate::user_input::create_user_input_with_after_install_hook(
                &destination,
                "A",
                &snapshot.content,
                || Err(io::Error::other("post-install failure")),
            )
            .unwrap_err();
            let crate::user_input::UserInputCreateError::AfterInstall { id, error } = failure
            else {
                panic!()
            };
            assert!(reconcile_draft_create_after_install(
                &mut app,
                &destination,
                &snapshot,
                id,
                error
            ));
            assert!(!reconcile_draft_create_after_install(
                &mut app,
                &destination,
                &snapshot,
                id,
                io::Error::other("duplicate")
            ));
        } else {
            crate::user_input::save_user_input(&destination, "A", 7, &snapshot.content).unwrap();
            assert!(reconcile_persisted_save_error(
                &mut app,
                &destination,
                &snapshot,
                7,
                io::Error::other("post-save")
            ));
            assert!(!reconcile_persisted_save_error(
                &mut app,
                &destination,
                &snapshot,
                7,
                io::Error::other("duplicate")
            ));
        }
        assert_eq!(stdin(&take_run(&mut app)), snapshot.content);
        assert!(!app.user_input_editor_active());
    }
}

#[test]
fn editor_mutations_clear_results_including_paste_enter_tab_backspace_and_delete() {
    for code in [
        KeyCode::Char('界'),
        KeyCode::Enter,
        KeyCode::Tab,
        KeyCode::Backspace,
        KeyCode::Delete,
    ] {
        let (_temp, destination, mut app) = fixture(&[(7, "abc\n")]);
        app.begin_selected_user_input_edit().unwrap();
        if code == KeyCode::Delete {
            app.edit_user_input_left();
        }
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        complete(&mut app, &request, "old");
        assert!(handle_user_input_editor_key(
            &mut app,
            key(code, KeyEventKind::Press),
            None
        ));
        assert!(last(&app, 7).is_none());
    }
    let (_temp, destination, mut app) = fixture(&[(7, "abc")]);
    app.begin_selected_user_input_edit().unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    complete(&mut app, &request, "old");
    let (tx, _rx) = mpsc::channel();
    let mut events = VecDeque::from([TerminalEvent::Paste("pasted\r\n\t界".to_string())]);
    handle_terminal_events(&mut app, &view::RenderInfo::default(), &mut events, &tx).unwrap();
    assert!(last(&app, 7).is_none());
}

#[test]
fn cursor_moves_and_noop_mutation_keep_result() {
    let (_temp, destination, mut app) = fixture(&[(7, "abc\nxyz")]);
    app.begin_selected_user_input_edit().unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    complete(&mut app, &request, "kept");
    for code in [
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::End,
    ] {
        handle_user_input_editor_key(&mut app, key(code, KeyEventKind::Press), None);
        assert_eq!(last(&app, 7).unwrap().stdout, "kept");
    }
    assert!(!app.edit_user_input_insert(""));
    assert!(!app.edit_user_input_delete());
    assert!(last(&app, 7).is_some());
}

#[test]
fn exact_revert_or_cancel_never_resurrects_result_and_cancel_dirty_run_clears_it() {
    for rerun in [false, true] {
        let (_temp, destination, mut app) = fixture(&[(7, "original")]);
        app.begin_selected_user_input_edit().unwrap();
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        complete(&mut app, &request, "old");
        app.edit_user_input_insert("x");
        app.edit_user_input_backspace();
        assert_eq!(app.selected_user_input_edit().unwrap().buffer(), "original");
        assert!(last(&app, 7).is_none());
        app.edit_user_input_insert("dirty");
        if rerun {
            run_selected_user_input(&mut app, None);
            let request = take_run(&mut app);
            complete(&mut app, &request, "dirty result");
        }
        app.cancel_user_input_edit();
        assert!(last(&app, 7).is_none());
    }
}

#[test]
fn sync_changes_clear_only_changed_or_deleted_ids_and_active_edit_skips_sync() {
    let (_temp, destination, mut app) = fixture(&[(7, "seven"), (9, "nine")]);
    for id in [7, 9] {
        select_sync_input(&mut app, id);
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        complete(&mut app, &request, "kept");
    }
    fs::write(destination.join(".atc/user-inputs/A/7.in"), "changed").unwrap();
    app.begin_selected_user_input_edit().unwrap();
    assert!(!sync_user_inputs_for_problem(
        &mut app,
        Some(&destination),
        0
    ));
    assert!(last(&app, 7).is_some());
    app.cancel_user_input_edit();
    sync_user_inputs_for_problem(&mut app, Some(&destination), 0);
    assert!(last(&app, 7).is_none());
    assert!(last(&app, 9).is_some());
    fs::remove_file(destination.join(".atc/user-inputs/A/9.in")).unwrap();
    sync_user_inputs_for_problem(&mut app, Some(&destination), 0);
    assert!(last(&app, 9).is_none());
}

#[test]
fn source_revision_clears_all_results_preserves_editor_and_only_queues_samples() {
    let (_temp, destination, mut app) = fixture(&[(7, "seven"), (9, "nine")]);
    for id in [7, 9] {
        select_sync_input(&mut app, id);
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        complete(&mut app, &request, "old");
    }
    app.begin_selected_user_input_edit().unwrap();
    let snapshot = app.user_input_save_snapshot().unwrap();
    let (tx, rx) = mpsc::channel();
    let (run_tx, run_rx) = mpsc::channel();
    let path = destination.join("A.py");
    fs::write(&path, "print('changed')").unwrap();
    tx.send(Message::SourceChanged {
        problem: 0,
        path,
        language: Language::Python,
    })
    .unwrap();
    handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap();
    assert!(last(&app, 7).is_none());
    assert!(last(&app, 9).is_none());
    select_sync_input(&mut app, 9);
    assert_eq!(app.user_input_save_snapshot().unwrap(), snapshot);
    assert!(matches!(received_run(&run_rx).kind, RunKind::Samples));
    assert!(run_rx.try_recv().is_err());
    assert!(app.take_user_input_run_request().is_none());
}

#[test]
fn source_notification_invalidates_results_even_if_content_was_restored() {
    let (_temp, destination, mut app) = fixture(&[(7, "seven")]);
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    complete(&mut app, &request, "kept");
    app.source_changed(0, destination.join("A.py"), Language::Python);
    assert!(last(&app, 7).is_none());
    assert!(!apply_event(&mut app, &request, result_event("late")));
}

#[test]
fn stale_completion_after_mutation_source_change_or_deletion_is_ignored() {
    for mode in ["mutation", "source", "deletion", "sync"] {
        let (_temp, destination, mut app) = fixture(&[(7, "old")]);
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        app.run_started(0, request.run_id);
        match mode {
            "mutation" => {
                app.begin_selected_user_input_edit().unwrap();
                app.edit_user_input_insert("x");
            }
            "source" => {
                fs::write(destination.join("A.py"), "changed").unwrap();
                app.source_changed(0, destination.join("A.py"), Language::Python);
            }
            "deletion" => {
                fs::remove_file(destination.join(".atc/user-inputs/A/7.in")).unwrap();
                sync_user_inputs_for_problem(&mut app, Some(&destination), 0);
            }
            _ => {
                fs::write(destination.join(".atc/user-inputs/A/7.in"), "changed").unwrap();
                sync_user_inputs_for_problem(&mut app, Some(&destination), 0);
            }
        }
        assert!(!apply_event(&mut app, &request, result_event("stale")));
        assert!(!app.run_started(0, request.run_id));
        assert!(!app.run_failed(0, request.run_id, "stale failure".to_string()));
        assert!(last(&app, 7).is_none());
    }
}

#[test]
fn repeated_run_latest_wins_even_if_old_started_and_completed_messages_arrive_later() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    run_selected_user_input(&mut app, Some(&destination));
    let first = take_run(&mut app);
    run_selected_user_input(&mut app, Some(&destination));
    let second = take_run(&mut app);
    assert!(second.run_id > first.run_id);
    assert!(!app.run_started(0, first.run_id));
    complete(&mut app, &second, "latest");
    assert!(!apply_event(&mut app, &first, result_event("old")));
    assert!(!app.run_started(0, first.run_id));
    assert_eq!(last(&app, 7).unwrap().stdout, "latest");
}

#[test]
fn cancelled_or_replaced_draft_rejects_stale_completion() {
    let (_temp, _destination, mut app) = fixture(&[]);
    app.begin_new_user_input().unwrap();
    app.edit_user_input_insert("same");
    run_selected_user_input(&mut app, None);
    let old = take_run(&mut app);
    app.cancel_user_input_edit();
    assert!(!apply_event(&mut app, &old, result_event("old")));
    app.begin_new_user_input().unwrap();
    app.edit_user_input_insert("same");
    run_selected_user_input(&mut app, None);
    let new = take_run(&mut app);
    let (RunKind::UserInput(a), RunKind::UserInput(b)) = (&old.kind, &new.kind) else {
        panic!()
    };
    assert_ne!(a.target, b.target);
    assert!(!apply_event(&mut app, &old, result_event("old")));
    complete(&mut app, &new, "new");
    assert!(text(&app).contains("new"));
}

#[test]
fn accepted_other_job_cancels_user_input_without_requeue_or_late_running_state() {
    let (_temp, destination, mut app) = fixture(&[(7, "seven"), (9, "nine")]);
    run_selected_user_input(&mut app, Some(&destination));
    let first = take_run(&mut app);
    select_sync_input(&mut app, 9);
    run_selected_user_input(&mut app, Some(&destination));
    let second = take_run(&mut app);
    assert_eq!(last(&app, 7).unwrap().status, Status::Cancelled);
    assert!(!apply_event(&mut app, &first, UserInputRunEvent::Running));
    app.queue_run(0).unwrap();
    assert_eq!(last(&app, 9).unwrap().status, Status::Cancelled);
    assert!(!apply_event(&mut app, &second, result_event("late")));
}

fn dispatch(
    app: &mut WatchApp,
    info: &view::RenderInfo,
    events: &mut VecDeque<TerminalEvent>,
    destination: &Path,
    tx: &Sender<RunWorkerCommand>,
) {
    super::super::handle_terminal_events_with_mouse_mode(
        app,
        &mut detail_layout::DetailLayout::default(),
        &mut DetailScrollbarDragState::default(),
        info,
        events,
        MouseMode::Cells,
        FrontendInputContext {
            terminal: TerminalInputContext::new(tx, Some(destination), None),
            contest_switch: None,
            contest_refresh: None,
            command_palette: None,
            open_source: None,
            editor_targets: None,
            editor: None,
        },
    )
    .unwrap();
}

#[test]
fn run_and_save_pointer_then_q_use_ordered_actual_editor_state() {
    for mode in [
        "readonly run",
        "editing run",
        "save success",
        "save failure",
    ] {
        let (_temp, destination, mut app) = fixture(&[(7, "input")]);
        if mode != "readonly run" {
            app.begin_selected_user_input_edit().unwrap();
        }
        if mode == "save failure" {
            fs::write(destination.join(".atc/user-inputs/A/7.in"), "conflict").unwrap();
        }
        let info = rendered_fold_info(&app, 120, 35);
        let action = if mode.starts_with("save") {
            view::UserInputDetailAction::Save
        } else {
            view::UserInputDetailAction::Run
        };
        let target = info
            .user_input_detail_actions
            .iter()
            .find(|target| target.action == action)
            .unwrap();
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                PointerKind::Down(PointerButton::Left),
                target.area.x,
                target.area.y,
            )),
            TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
        ]);
        assert!(!contains_plain_global_quit_event(&events, &app));
        let (tx, rx) = mpsc::channel();
        dispatch(&mut app, &info, &mut events, &destination, &tx);
        assert_eq!(
            events.len(),
            1,
            "later q must not be consumed before pointer action redraw"
        );
        let requests = received_runs(&rx);
        assert_eq!(requests.len(), usize::from(mode != "save failure"));
        if let Some(request) = requests.first() {
            assert_eq!(stdin(request), "input");
        }
        let info = rendered_fold_info(&app, 120, 35);
        dispatch(&mut app, &info, &mut events, &destination, &tx);
        let editing = matches!(mode, "editing run" | "save failure");
        assert_eq!(app.should_quit(), !editing);
        assert_eq!(app.user_input_editor_active(), editing);
        if editing {
            assert_eq!(app.selected_user_input_edit().unwrap().buffer(), "inputq");
        }
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn ctrl_s_success_sends_one_run_before_following_global_q() {
    let (_temp, destination, mut app) = fixture(&[]);
    app.begin_new_user_input().unwrap();
    app.edit_user_input_insert("saved");
    let mut events = VecDeque::from([
        TerminalEvent::Key(user_input_ctrl_s()),
        TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
    ]);
    assert!(!contains_plain_global_quit_event(&events, &app));
    let (tx, rx) = mpsc::channel();
    let info = rendered_fold_info(&app, 120, 35);
    dispatch(&mut app, &info, &mut events, &destination, &tx);
    assert_eq!(stdin(&received_run(&rx)), "saved");
    assert!(rx.try_recv().is_err());
    assert!(contains_plain_global_quit_event(&events, &app));
}

#[test]
fn readonly_and_editing_actions_are_ordered_and_do_not_overlap_folds() {
    for mode in ["readonly", "editing", "draft"] {
        let (_temp, destination, mut app) = fixture(&[(7, "input")]);
        app.source_changed(0, destination.join("A.cpp"), Language::Cpp);
        if mode == "editing" {
            app.begin_selected_user_input_edit().unwrap();
            app.edit_user_input_insert("!");
        }
        if mode == "draft" {
            app.begin_new_user_input().unwrap();
            app.edit_user_input_insert("input!");
        }
        let info = rendered_fold_info(&app, 120, 35);
        let actions = info
            .user_input_detail_actions
            .iter()
            .map(|target| target.action)
            .collect::<Vec<_>>();
        use view::UserInputDetailAction::*;
        assert_eq!(
            actions,
            if mode == "readonly" {
                vec![Edit, Run]
            } else {
                vec![Save, Run, Cancel]
            }
        );
        for pair in info.user_input_detail_actions.windows(2) {
            assert!(pair[0].area.right() < pair[1].area.x);
        }
        for target in &info.user_input_detail_actions {
            for x in target.area.x..target.area.right() {
                assert_eq!(
                    info.user_input_detail_action_at(app.detail_revision(), x, target.area.y),
                    Some(*target)
                );
                assert!(info.detail_section_headers.iter().all(|header| !contains(
                    header.area,
                    x,
                    target.area.y
                )));
            }
        }
        let before = info;
        let before_editor = app.selected_user_input_edit().cloned();
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        for status in [
            Status::Queued,
            Status::Compiling,
            Status::Running,
            Status::Finished,
        ] {
            match status {
                Status::Compiling => assert!(app.run_started(0, request.run_id)),
                Status::Running => {
                    assert!(apply_event(&mut app, &request, UserInputRunEvent::Running))
                }
                Status::Finished => {
                    assert!(apply_event(&mut app, &request, result_event("output")))
                }
                _ => {}
            }
            let rendered = text(&app);
            let header = if mode == "readonly" {
                "▼ Input"
            } else {
                "▼ Input — Editing *"
            };
            assert!(rendered.find(status.label()).unwrap() < rendered.find(header).unwrap());
            let info = rendered_fold_info(&app, 120, 35);
            assert_eq!(
                info.user_input_detail_actions.len(),
                before.user_input_detail_actions.len()
            );
            for (target, original) in info
                .user_input_detail_actions
                .iter()
                .zip(&before.user_input_detail_actions)
            {
                assert_eq!(target.action, original.action);
                let mut shifted = original.area;
                // The status uses the existing leading blank row plus one new row.
                shifted.y += 1;
                assert_eq!(target.area, shifted);
                for x in target.area.x..target.area.right() {
                    assert_eq!(
                        info.user_input_detail_action_at(app.detail_revision(), x, target.area.y),
                        Some(*target)
                    );
                    assert!(info.detail_section_headers.iter().all(|header| !contains(
                        header.area,
                        x,
                        target.area.y
                    )));
                }
                assert!(
                    info.user_input_detail_action_at(
                        app.detail_revision(),
                        target.area.right(),
                        target.area.y
                    )
                    .is_none()
                );
                assert!(
                    info.user_input_detail_action_at(
                        app.detail_revision(),
                        target.area.x,
                        target.area.y - 1
                    )
                    .is_none()
                );
            }
            let screen = rendered_app_text(&app, 120, 35);
            let header_row = info.user_input_detail_actions[0].area.y;
            let header_text = screen.lines().nth(usize::from(header_row)).unwrap();
            assert!(header_text.contains(header));
            assert!(header_text.contains(if mode == "readonly" {
                "[Edit] [Run]"
            } else {
                "[Save] [Run] [Cancel]"
            }));
            assert_eq!(app.selected_user_input_edit(), before_editor.as_ref());
            assert_eq!(
                info.editor_cursor,
                before.editor_cursor.map(|(x, y)| (x, y + 1))
            );
            assert_eq!(info.editor_scroll_reconciliation, None);
            assert_eq!(app.detail_scroll(), 0);
        }
    }
}

#[test]
fn user_input_status_wraps_safely_at_narrow_terminal_widths() {
    for editing in [false, true] {
        let (_temp, destination, mut app) = fixture(&[(7, "界😀 input\nsecond line\n")]);
        if editing {
            app.begin_selected_user_input_edit().unwrap();
        }
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        for terminal in [false, true] {
            if terminal {
                assert!(apply_event(
                    &mut app,
                    &request,
                    UserInputRunEvent::Finished(UserInputRunResult {
                        status: Status::TimedOut,
                        stdout: "output\n".into(),
                        stderr: "stderr\n".into(),
                        elapsed: Duration::from_micros(6500),
                    })
                ));
            } else {
                assert!(app.run_started(0, request.run_id));
            }
            let document = DetailDocument::from_app(&app);
            let raw = text(&app);
            if let Some(cursor) = document.editor_cursor() {
                let edit = app.selected_user_input_edit().unwrap();
                assert_eq!(
                    cursor.raw_position.0 - cursor.content_start.0,
                    edit.cursor()
                );
                assert!(cursor.content_start.0 > raw.find("▼ Input").unwrap());
            }
            for width in 0..=80 {
                let wrapped = detail_layout::wrap_detail_document(&document, width);
                let joined: String = wrapped
                    .lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .map(|span| span.content.as_ref())
                    .collect();
                assert_eq!(joined, raw.replace('\n', ""), "width {width}");
                for height in [0, 1, 8, 18] {
                    let info = rendered_fold_info(&app, width, height);
                    for target in &info.user_input_detail_actions {
                        assert!(target.area.right() <= width);
                        assert!(target.area.bottom() <= height);
                    }
                    if let Some((x, y)) = info.editor_cursor {
                        assert!(x < width && y < height);
                    }
                }
            }
        }
    }
}

#[test]
fn result_appearance_invalidates_detail_and_updates_bounds_without_changing_editor() {
    let content = "line one\nline two\n".repeat(20);
    let (_temp, destination, mut app) = fixture(&[(7, &content)]);
    app.begin_selected_user_input_edit().unwrap();
    app.edit_user_input_left();
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    let before_editor = app.selected_user_input_edit().unwrap().clone();
    let before_selection = app.case_selection();
    let before_revision = app.detail_revision();
    let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
    let mut layout = detail_layout::DetailLayout::default();
    let mut before = view::RenderInfo::default();
    for _ in 0..8 {
        terminal
            .draw(|frame| before = view::render(frame, &app, &mut layout))
            .unwrap();
        let Some(scroll) = before.editor_scroll_reconciliation else {
            break;
        };
        app.reconcile_detail_scroll(scroll);
    }
    let before_scroll = app.detail_scroll();
    assert!(before_scroll > 0);
    assert!(before.editor_cursor.is_some());
    complete(&mut app, &request, &"output\n".repeat(80));
    assert!(app.detail_revision() > before_revision);
    assert_eq!(app.selected_user_input_edit().unwrap(), &before_editor);
    assert_eq!(app.detail_scroll(), before_scroll);
    assert_eq!(app.case_selection(), before_selection);
    let mut after = view::RenderInfo::default();
    terminal
        .draw(|frame| after = view::render(frame, &app, &mut layout))
        .unwrap();
    assert!(after.max_detail_scroll > before.max_detail_scroll);
    assert_eq!(after.editor_cursor, before.editor_cursor);
    assert_eq!(after.editor_scroll_reconciliation, None);
    let document = DetailDocument::from_app(&app);
    assert!(
        document
            .segments()
            .any(|segment| segment.text().contains("Output"))
    );
}

fn python_runner() -> crate::config::RunnerConfig {
    let python = ["python3", "python"]
        .into_iter()
        .find(|name| {
            std::process::Command::new(name)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("Python required");
    crate::config::RunnerConfig {
        python: python.into(),
        ..Default::default()
    }
}

fn use_echo_source(destination: &Path) {
    fs::write(
        destination.join("A.py"),
        "import sys\nsys.stdout.buffer.write(sys.stdin.buffer.read())\n",
    )
    .unwrap();
}

fn execute_echo(destination: &Path, request: &RunRequest) -> Vec<Message> {
    let messages =
        crate::commands::execute_user_input_for_test(destination, request.clone(), python_runner());
    let results: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            Message::UserInputRunEvent {
                snapshot: identity,
                event: UserInputRunEvent::Finished(result),
                ..
            } => {
                assert_eq!(identity, snapshot(request));
                Some(result)
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, Status::Finished);
    assert_eq!(results[0].stdout.as_bytes(), stdin(request).as_bytes());
    assert!(results[0].stderr.is_empty());
    messages
}

fn deliver(
    app: &mut WatchApp,
    destination: &Path,
    messages: Vec<Message>,
) -> Vec<RunWorkerCommand> {
    let (tx, rx) = mpsc::channel();
    let (run_tx, run_rx) = mpsc::channel();
    for message in messages {
        tx.send(message).unwrap();
    }
    handle_messages_with_destination(app, &rx, &run_tx, Some(destination)).unwrap();
    run_rx.try_iter().collect()
}

fn assert_executed(messages: &[Message], request: &RunRequest, output: &str) {
    let mut completions = 0;
    for message in messages {
        if let Message::UserInputRunEvent {
            snapshot: identity,
            event,
            ..
        } = message
        {
            assert_eq!(identity, snapshot(request));
            if let UserInputRunEvent::Finished(result) = event {
                assert_eq!(result.status, Status::Finished);
                assert_eq!(result.stdout.trim(), output);
                completions += 1;
            }
        }
        assert!(!matches!(
            message,
            Message::RunEvent { .. } | Message::StressEvent { .. }
        ));
    }
    assert_eq!(completions, 1);
}

#[test]
fn source_a_b_run_b_restore_a_delayed_notification_never_retains_b_result() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    let path = destination.join("A.py");
    fs::write(&path, "print('A')\n").unwrap();
    app.source_changed(0, path.clone(), Language::Python);
    // No SourceChanged for B: reproduce a click inside the watcher's debounce window.
    fs::write(&path, "print('B')\n").unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    let messages = crate::commands::execute_user_input_for_test(
        &destination,
        request.clone(),
        python_runner(),
    );
    assert_executed(&messages, &request, "B");
    assert!(deliver(&mut app, &destination, messages).is_empty());
    assert_eq!(last(&app, 7).unwrap().status, Status::Finished);
    fs::write(&path, "print('A')\n").unwrap();
    let queued = deliver(
        &mut app,
        &destination,
        vec![Message::SourceChanged {
            problem: 0,
            path,
            language: Language::Python,
        }],
    );
    assert!(last(&app, 7).is_none());
    assert!(!apply_event(&mut app, &request, result_event("B")));
    assert!(matches!(
        queued.as_slice(),
        [RunWorkerCommand::Run(RunRequest {
            kind: RunKind::Samples,
            ..
        })]
    ));
    assert!(app.take_user_input_run_request().is_none());
}

#[test]
fn worker_reads_live_source_when_source_changes_before_start() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    let path = destination.join("A.py");
    fs::write(&path, "print('previous source A')\n").unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    fs::write(&path, "print('live B')\n").unwrap();
    let messages = crate::commands::execute_user_input_for_test(
        &destination,
        request.clone(),
        python_runner(),
    );
    assert_executed(&messages, &request, "live B");
    assert!(deliver(&mut app, &destination, messages).is_empty());
    assert_eq!(last(&app, 7).unwrap().status, Status::Finished);
    assert!(app.take_user_input_run_request().is_none());
}

#[test]
fn source_notification_during_execution_rejects_old_worker_messages() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    fs::write(destination.join("A.py"),
        "from pathlib import Path\nPath(__file__).write_text(\"print('new source')\\n\")\nprint('old execution')\n").unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    let messages = crate::commands::execute_user_input_for_test(
        &destination,
        request.clone(),
        python_runner(),
    );
    assert_executed(&messages, &request, "old execution");
    let mut batch = vec![Message::SourceChanged {
        problem: 0,
        path: destination.join("A.py"),
        language: Language::Python,
    }];
    batch.extend(messages);
    let queued = deliver(&mut app, &destination, batch);
    assert!(matches!(
        queued.as_slice(),
        [RunWorkerCommand::Run(RunRequest {
            kind: RunKind::Samples,
            ..
        })]
    ));
    assert!(app.take_user_input_run_request().is_none());
    assert!(last(&app, 7).is_none());
    assert!(!app.run_failed(0, request.run_id, "late error".into()));
}

#[test]
fn opening_current_source_keeps_result_but_notification_clears_it_and_only_runs_samples() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    let path = destination.join("A.py");
    fs::write(&path, "print('valid')\n").unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    let messages =
        crate::commands::execute_user_input_for_test(&destination, request, python_runner());
    deliver(&mut app, &destination, messages);
    let before = last(&app, 7).unwrap().clone();
    app.select_source(0, path.clone(), Language::Python);
    assert_eq!(last(&app, 7), Some(&before));
    fs::write(&path, "print('valid')\n").unwrap();
    let queued = deliver(
        &mut app,
        &destination,
        vec![Message::SourceChanged {
            problem: 0,
            path: path.clone(),
            language: Language::Python,
        }],
    );
    assert!(last(&app, 7).is_none());
    assert!(matches!(
        queued.as_slice(),
        [RunWorkerCommand::Run(RunRequest {
            kind: RunKind::Samples,
            ..
        })]
    ));

    assert!(app.take_user_input_run_request().is_none());
}

#[test]
fn source_notification_for_missing_file_clears_result_and_never_resurrects_it() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    complete(&mut app, &request, "old");
    fs::remove_file(destination.join("A.py")).unwrap();
    let queued = deliver(
        &mut app,
        &destination,
        vec![Message::SourceChanged {
            problem: 0,
            path: destination.join("A.py"),
            language: Language::Python,
        }],
    );
    assert!(matches!(
        queued.as_slice(),
        [RunWorkerCommand::Run(RunRequest {
            kind: RunKind::Samples,
            ..
        })]
    ));
    assert!(last(&app, 7).is_none());
    fs::write(destination.join("A.py"), "pass\n").unwrap();
    assert!(deliver(&mut app, &destination, vec![]).is_empty());
    assert!(!apply_event(&mut app, &request, result_event("old")));
}

#[test]
fn missing_live_source_is_reported_by_worker_as_runner_diagnostic() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    fs::remove_file(destination.join("A.py")).unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    let messages =
        crate::commands::execute_user_input_for_test(&destination, request, python_runner());
    deliver(&mut app, &destination, messages);
    assert_eq!(last(&app, 7).unwrap().status, Status::Failed);
    assert!(
        last(&app, 7)
            .unwrap()
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("source file not found")
    );
    assert!(text(&app).contains("Error: filesystem operation failed:"));
    assert!(!text(&app).contains("Stderr"));
}

#[test]
fn completion_with_matching_run_id_but_different_revision_is_rejected() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    let mut wrong = snapshot(&request).as_ref().clone();
    wrong.source_revision += 1;
    assert!(!app.user_input_run_event(0, request.run_id, &wrong, result_event("wrong")));
    assert_eq!(last(&app, 7).unwrap().status, Status::Queued);
    complete(&mut app, &request, "correct");
}

fn edit_other_row(app: &mut WatchApp) -> (app::UserInputEditState, app::UserInputSaveSnapshot) {
    select_sync_input(app, 7);
    app.begin_selected_user_input_edit().unwrap();
    app.edit_user_input_insert("dirty\r\n\t界\n");
    app.edit_user_input_left();
    app.edit_user_input_up(); // retain a non-default preferred column as well as cursor
    let snapshot = app.user_input_save_snapshot().unwrap();
    app.fail_user_input_save(&snapshot, "previous save failure".into(), None)
        .unwrap();
    let edit = app.selected_user_input_edit().unwrap().clone();
    let snapshot = app.user_input_save_snapshot().unwrap();
    select_sync_input(app, 9);
    (edit, snapshot)
}

fn assert_other_editor_unchanged(
    app: &mut WatchApp,
    edit: &app::UserInputEditState,
    baseline: &app::UserInputSaveSnapshot,
) {
    assert_eq!(app.active_user_input_edit(), Some(edit));
    select_sync_input(app, 7);
    assert_eq!(&app.user_input_save_snapshot().unwrap(), baseline);
}

#[test]
fn readonly_run_click_with_other_row_editor_flushes_exactly_one_request() {
    let (_temp, destination, mut app) = fixture(&[(7, "first"), (9, "second")]);
    let (edit, baseline) = edit_other_row(&mut app);
    let info = rendered_fold_info(&app, 120, 35);
    let action = info
        .user_input_detail_actions
        .iter()
        .find(|action| action.action == view::UserInputDetailAction::Run)
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let mut events = VecDeque::from([TerminalEvent::Pointer(pointer(
        PointerKind::Down(PointerButton::Left),
        action.area.x,
        action.area.y,
    ))]);
    dispatch(&mut app, &info, &mut events, &destination, &tx);
    let request = received_run(&rx);
    assert_eq!(stdin(&request), "second");
    assert!(rx.try_recv().is_err());
    assert!(app.take_user_input_run_request().is_none());
    assert_other_editor_unchanged(&mut app, &edit, &baseline);
}

#[test]
fn readonly_target_refresh_changes_only_target_and_preserves_other_editor_baseline() {
    let (_temp, destination, mut app) = fixture(&[(7, "first"), (9, "second")]);
    select_sync_input(&mut app, 9);
    run_selected_user_input(&mut app, Some(&destination));
    let old = take_run(&mut app);
    complete(&mut app, &old, "old target output");
    let (edit, baseline) = edit_other_row(&mut app);
    fs::write(
        destination.join(".atc/user-inputs/A/7.in"),
        "external editor conflict",
    )
    .unwrap();
    fs::write(
        destination.join(".atc/user-inputs/A/9.in"),
        "latest\r\n\t界\n",
    )
    .unwrap();
    assert!(!sync_user_inputs_for_problem(
        &mut app,
        Some(&destination),
        0
    ));
    assert_eq!(last(&app, 9).unwrap().stdout, "old target output");
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    assert_eq!(stdin(&request), "latest\r\n\t界\n");
    assert_eq!(last(&app, 9).unwrap().status, Status::Queued);
    assert!(last(&app, 9).unwrap().stdout.is_empty());
    assert_other_editor_unchanged(&mut app, &edit, &baseline);
    // The unchanged baseline must still detect the external conflict on Save.
    save_selected_user_input(&mut app, &destination);
    assert!(app.user_input_editor_active());
    assert!(
        app.selected_user_input_edit()
            .unwrap()
            .save_error()
            .is_some()
    );
    assert!(app.take_user_input_run_request().is_none());
}

#[test]
fn missing_readonly_target_removes_only_target_and_preserves_other_editor() {
    let (_temp, destination, mut app) = fixture(&[(7, "first"), (9, "second")]);
    select_sync_input(&mut app, 9);
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    complete(&mut app, &request, "removed output");
    let (edit, baseline) = edit_other_row(&mut app);
    fs::remove_file(destination.join(".atc/user-inputs/A/9.in")).unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    assert!(app.take_user_input_run_request().is_none());
    assert!(last(&app, 9).is_none());
    assert_eq!(
        app.selected_user_input(),
        Some(app::UserInputSelection::Persisted(7))
    );
    assert_eq!(
        sync_notice(&app),
        Some("User Input 2 was removed externally.")
    );
    assert_eq!(
        app.current_problem()
            .unwrap()
            .user_inputs
            .ready()
            .unwrap()
            .persisted()
            .len(),
        1
    );
    assert_other_editor_unchanged(&mut app, &edit, &baseline);
    assert!(!apply_event(&mut app, &request, result_event("late")));
}

#[test]
fn readonly_target_load_error_preserves_cache_results_and_other_editor_with_notice() {
    let (_temp, destination, mut app) = fixture(&[(7, "first"), (9, "second")]);
    let (edit, baseline) = edit_other_row(&mut app);
    let before = app.current_problem().unwrap().user_inputs.clone();
    fs::write(destination.join(".atc/user-inputs/A/9.in"), [0xff]).unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    assert!(app.take_user_input_run_request().is_none());
    assert_eq!(app.current_problem().unwrap().user_inputs, before);
    assert!(sync_notice(&app).unwrap().contains("Could not refresh"));
    assert_other_editor_unchanged(&mut app, &edit, &baseline);
}

#[test]
fn selected_editor_run_uses_buffer_even_when_disk_input_cannot_be_loaded() {
    let (_temp, destination, mut app) = fixture(&[(7, "first"), (9, "second")]);
    let (edit, baseline) = edit_other_row(&mut app);
    select_sync_input(&mut app, 7);
    fs::write(destination.join(".atc/user-inputs/A/7.in"), [0xff]).unwrap();
    run_selected_user_input(&mut app, Some(&destination));
    assert_eq!(stdin(&take_run(&mut app)), edit.buffer());
    assert!(sync_notice(&app).is_none());
    assert_other_editor_unchanged(&mut app, &edit, &baseline);
}

#[test]
fn target_refresh_preserves_draft_install_recovery_state_in_all_outcomes() {
    for outcome in ["unchanged", "modified", "missing", "error"] {
        let (_temp, destination, mut app) = fixture(&[(7, "first"), (9, "second")]);
        app.begin_new_user_input().unwrap();
        app.edit_user_input_insert("draft\ntext");
        app.edit_user_input_up();
        let snapshot = app.user_input_save_snapshot().unwrap();
        app.fail_user_input_save(&snapshot, "install recovery".into(), Some(99))
            .unwrap();
        let edit = app.selected_user_input_edit().unwrap().clone();
        let snapshot = app.user_input_save_snapshot().unwrap();
        select_sync_input(&mut app, 9);
        let path = destination.join(".atc/user-inputs/A/9.in");
        match outcome {
            "modified" => fs::write(path, "latest").unwrap(),
            "missing" => fs::remove_file(path).unwrap(),
            "error" => fs::write(path, [0xff]).unwrap(),
            _ => {}
        }
        run_selected_user_input(&mut app, Some(&destination));
        if matches!(outcome, "modified" | "unchanged") {
            take_run(&mut app);
        }
        assert!(app.take_user_input_run_request().is_none());
        assert_eq!(app.active_user_input_edit(), Some(&edit));
        app.begin_new_user_input().unwrap();
        assert_eq!(app.user_input_save_snapshot().unwrap(), snapshot);
    }
}

#[test]
fn interpreter_spawn_failure_is_runner_diagnostic_not_program_stderr() {
    let (_temp, destination, mut app) = fixture(&[(7, "input")]);
    run_selected_user_input(&mut app, Some(&destination));
    let request = take_run(&mut app);
    let config = crate::config::RunnerConfig {
        python: destination
            .join("missing-python-executable")
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    let messages = crate::commands::execute_user_input_for_test(&destination, request, config);
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::RunFailed { .. }))
    );
    deliver(&mut app, &destination, messages);
    let run = last(&app, 7).unwrap();
    assert_eq!(run.status, Status::Failed);
    assert!(run.diagnostic.is_some());
    assert!(run.stderr.is_empty());
    let rendered = text(&app);
    assert!(rendered.contains("Failed"));
    assert!(rendered.contains("Error: "));
    assert!(!rendered.contains("Stderr"));
}

#[test]
fn real_program_stderr_is_rendered_for_success_and_runtime_error() {
    for exit in [0, 3] {
        let (_temp, destination, mut app) = fixture(&[(7, "input")]);
        fs::write(
            destination.join("A.py"),
            format!("import sys\nsys.stderr.write('program stderr')\nsys.exit({exit})\n"),
        )
        .unwrap();
        run_selected_user_input(&mut app, Some(&destination));
        let request = take_run(&mut app);
        let messages =
            crate::commands::execute_user_input_for_test(&destination, request, python_runner());
        deliver(&mut app, &destination, messages);
        let run = last(&app, 7).unwrap();
        assert_eq!(
            run.status,
            if exit == 0 {
                Status::Finished
            } else {
                Status::RuntimeError
            }
        );
        assert_eq!(run.stderr, "program stderr");
        assert!(run.diagnostic.is_none());
        assert!(text(&app).contains("▼ Stderr\nprogram stderr"));
        assert!(!text(&app).contains("Error: "));
    }
}
