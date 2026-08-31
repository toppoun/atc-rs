use super::*;
use app::{CaseSelection, UserInputSelection};
use message::{RunKind, UserInputRunEvent, UserInputRunResult, UserInputRunStatus};
use view::{CasesRowAction, CasesRowTarget};

fn fixture(samples: usize) -> (tempfile::TempDir, PathBuf, WatchApp) {
    let (temp, destination) = user_input_workspace();
    write_sync_inputs(
        &destination,
        "A",
        &[(1, "one\r\n"), (3, "three"), (7, "seven")],
    );
    let mut app = sync_test_app(&destination, &[samples, 0]);
    app.toggle_samples_pane();
    (temp, destination, app)
}

fn armed(app: &WatchApp) -> Option<u64> {
    app.current_problem().unwrap().user_input_delete_armed
}

fn ids(app: &WatchApp) -> Vec<u64> {
    app.current_problem()
        .unwrap()
        .user_inputs
        .ready()
        .unwrap()
        .persisted()
        .iter()
        .map(|input| input.id)
        .collect()
}

fn selection(id: u64) -> CaseSelection {
    CaseSelection::UserInput(UserInputSelection::Persisted(id))
}

fn target(app: &WatchApp, action: CasesRowAction) -> CasesRowTarget {
    rendered_fold_info(app, 100, 30)
        .cases_row_targets
        .into_iter()
        .find(|target| target.action == action)
        .expect("visible row target")
}

fn click(app: &mut WatchApp, destination: &Path, action: CasesRowAction) {
    let info = rendered_fold_info(app, 100, 30);
    let target = info
        .cases_row_targets
        .iter()
        .find(|target| target.action == action)
        .unwrap();
    assert!(handle_pointer_event_with_mouse_mode(
        app,
        &mut detail_layout::DetailLayout::default(),
        &mut DetailScrollbarDragState::default(),
        pointer(
            PointerKind::Down(PointerButton::Left),
            target.area.x + 1,
            target.area.y
        ),
        &info,
        MouseMode::Cells,
        Some(destination),
    ));
}

fn delete(app: &mut WatchApp, destination: &Path, id: u64) {
    click(app, destination, CasesRowAction::DeleteUserInput(id));
    click(app, destination, CasesRowAction::DeleteUserInput(id));
}

fn rendered(app: &WatchApp, width: u16, height: u16) -> (String, view::RenderInfo) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut info = view::RenderInfo::default();
    terminal
        .draw(|frame| {
            info = view::render(frame, app, &mut detail_layout::DetailLayout::default());
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (text, info)
}

fn run(app: &mut WatchApp, id: u64, output: &str) -> message::RunRequest {
    app.select_case(selection(id));
    assert!(app.enqueue_selected_user_input());
    let request = app.take_user_input_run_request().unwrap();
    assert!(app.run_started(request.problem, request.run_id));
    let RunKind::UserInput(snapshot) = &request.kind else {
        panic!("User Input run")
    };
    assert!(app.user_input_run_event(request.problem, request.run_id, snapshot, result(output)));
    request
}

fn result(output: &str) -> UserInputRunEvent {
    UserInputRunEvent::Finished(UserInputRunResult {
        status: UserInputRunStatus::Finished,
        stdout: output.to_string(),
        stderr: "program stderr".to_string(),
        elapsed: Duration::from_millis(7),
    })
}

fn last(app: &WatchApp, selection: UserInputSelection) -> Option<&app::UserInputRunState> {
    app.current_problem()
        .unwrap()
        .user_inputs
        .ready()
        .unwrap()
        .last_run(selection)
}

#[test]
fn persisted_buttons_are_adjacent_and_only_the_armed_id_shows_question_mark() {
    let (_temp, destination, mut app) = fixture(0);
    let (text, info) = rendered(&app, 100, 30);
    assert!(text.contains("> Input 1   ×"));
    assert!(text.contains("  Input 2   ×"));
    assert_eq!(text.matches('×').count(), 3);
    assert!(!text.contains("×?"));
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    let (text, armed_info) = rendered(&app, 100, 30);
    assert!(text.contains("  Input 2   ×?"));
    assert_eq!(text.matches("×?").count(), 1);
    for original in info.cases_row_targets {
        assert!(
            armed_info
                .cases_row_targets
                .iter()
                .any(|now| { now.action == original.action && now.area == original.area })
        );
    }
}

#[test]
fn editor_target_and_draft_have_no_button_but_other_rows_do() {
    let (_temp, destination, mut app) = fixture(0);
    app.begin_selected_user_input_edit().unwrap();
    for dirty in [false, true] {
        if dirty {
            app.edit_user_input_insert("dirty");
        }
        let (text, info) = rendered(&app, 100, 30);
        assert_eq!(text.matches('×').count(), 2);
        assert!(
            !info
                .cases_row_targets
                .iter()
                .any(|t| t.action == CasesRowAction::DeleteUserInput(1))
        );
        assert!(
            info.cases_row_targets
                .iter()
                .any(|t| t.action == CasesRowAction::DeleteUserInput(3))
        );
        assert!(!click_user_input_delete(&mut app, 1, Some(&destination)));
        assert_eq!(ids(&app), [1, 3, 7]);
    }
    app.cancel_user_input_edit();
    assert_eq!(rendered(&app, 100, 30).0.matches('×').count(), 3);
    app.begin_new_user_input().unwrap();
    let (text, info) = rendered(&app, 100, 30);
    assert!(text.contains("> Draft *"));
    assert_eq!(text.matches('×').count(), 3);
    assert_eq!(
        info.cases_row_targets
            .iter()
            .filter(|t| matches!(t.action, CasesRowAction::DeleteUserInput(_)))
            .count(),
        3
    );
    app.edit_user_input_insert("saved draft");
    save_selected_user_input(&mut app, &destination);
    assert!(!app.user_input_editor_active());
    assert_eq!(rendered(&app, 100, 30).0.matches('×').count(), 4);
    app.begin_selected_user_input_edit().unwrap();
    app.edit_user_input_insert(" saved edit");
    save_selected_user_input(&mut app, &destination);
    assert!(!app.user_input_editor_active());
    assert_eq!(rendered(&app, 100, 30).0.matches('×').count(), 4);
}

#[test]
fn first_click_is_disk_readonly_and_second_click_removes_only_canonical_id_without_renumbering() {
    let (_temp, destination, mut app) = fixture(0);
    // Establish metadata before comparing the complete storage tree.
    crate::user_input::save_user_input(&destination, "A", 1, "one\r\n").unwrap();
    let directory = destination.join(".atc/user-inputs/A");
    fs::write(directory.join("notes.txt"), "leave me").unwrap();
    fs::write(directory.join("03.in"), "noncanonical").unwrap();
    let files = || -> std::collections::BTreeMap<_, _> {
        fs::read_dir(&directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect()
    };
    let before = files();
    let selected = app.case_selection();
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    assert_eq!(armed(&app), Some(3));
    assert_eq!(before, files());
    assert_eq!(app.case_selection(), selected);
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    assert_eq!(armed(&app), None);
    assert_eq!(app.case_selection(), selected);
    assert_eq!(ids(&app), [1, 7]);
    let mut expected = before;
    expected.remove(std::ffi::OsStr::new("3.in"));
    assert_eq!(files(), expected);
    let (text, _) = rendered(&app, 100, 30);
    assert!(text.contains("Input 2   ×"));
    assert!(!text.contains("Input 3"));
}

#[test]
fn different_delete_button_moves_arming_without_deleting_the_previous_target() {
    let (_temp, destination, mut app) = fixture(0);
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(7));
    assert_eq!(armed(&app), Some(7));
    assert_eq!(ids(&app), [1, 3, 7]);
    assert!(destination.join(".atc/user-inputs/A/3.in").is_file());
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(7));
    assert_eq!(ids(&app), [1, 3]);
}

#[test]
fn arming_survives_external_ordinal_change_but_stale_geometry_cannot_delete_another_row() {
    let (_temp, destination, mut app) = fixture(0);
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    let old = rendered_fold_info(&app, 100, 30);
    let old_target = target(&app, CasesRowAction::DeleteUserInput(3));
    crate::user_input::delete_user_input(&destination, "A", 1).unwrap();
    assert!(sync_user_inputs_for_problem(
        &mut app,
        Some(&destination),
        0
    ));
    assert_eq!(armed(&app), Some(3));
    assert!(rendered(&app, 100, 30).0.contains("Input 1   ×?"));
    assert!(
        old.cases_row_target_at(&app, old_target.area.x, old_target.area.y)
            .is_none()
    );
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    assert_eq!(ids(&app), [7]);
    assert_eq!(
        fs::read_to_string(destination.join(".atc/user-inputs/A/7.in")).unwrap(),
        "seven"
    );
}

#[test]
fn selected_delete_reuses_external_sync_fallback_for_middle_last_final_and_no_normal_case() {
    for (selected, remove_first, samples, expected) in [
        (3, false, 1, Some(selection(7))),
        (7, false, 1, Some(selection(3))),
        (1, true, 1, Some(CaseSelection::Test(0))),
        (1, true, 0, None),
    ] {
        let (_temp, destination, mut app) = fixture(samples);
        if remove_first {
            delete(&mut app, &destination, 3);
            delete(&mut app, &destination, 7);
        }
        app.select_case(selection(selected));
        delete(&mut app, &destination, selected);
        assert_eq!(app.case_selection(), expected);
    }
}

#[test]
fn other_row_delete_preserves_the_entire_persisted_or_draft_editor_and_baseline() {
    for draft in [false, true] {
        let (_temp, destination, mut app) = fixture(0);
        if draft {
            app.begin_new_user_input().unwrap();
        } else {
            app.begin_selected_user_input_edit().unwrap();
        }
        app.edit_user_input_insert("abc\nlonger line\nxy");
        app.edit_user_input_up(); // Non-default preferred column.
        let snapshot = app.user_input_save_snapshot().unwrap();
        app.fail_user_input_save(&snapshot, "retained save error".into(), Some(99))
            .unwrap();
        let editor = app.active_user_input_edit().unwrap().clone();
        let snapshot = app.user_input_save_snapshot().unwrap();
        let selected = app.case_selection();
        // Simulate unrelated disk edits: Delete must not import these into the baseline.
        crate::user_input::save_user_input(&destination, "A", 1, "external change").unwrap();
        delete(&mut app, &destination, 3);
        assert_eq!(app.active_user_input_edit(), Some(&editor));
        assert_eq!(app.user_input_save_snapshot(), Some(snapshot));
        assert_eq!(app.case_selection(), selected);
        assert_eq!(ids(&app), [1, 7]);
    }
}

#[test]
fn missing_target_is_success_equivalent_and_only_reconciles_that_id() {
    let (_temp, destination, mut app) = fixture(0);
    app.select_case(selection(3));
    app.source_changed(0, destination.join("A.py"), Language::Python);
    run(&mut app, 3, "old result");
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    crate::user_input::delete_user_input(&destination, "A", 3).unwrap();
    crate::user_input::save_user_input(&destination, "A", 1, "external").unwrap();
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    assert_eq!(ids(&app), [1, 7]);
    assert_eq!(app.case_selection(), Some(selection(7)));
    assert_eq!(armed(&app), None);
    assert_eq!(sync_notice(&app), None);
    assert!(last(&app, UserInputSelection::Persisted(3)).is_none());
    assert_eq!(
        app.current_problem()
            .unwrap()
            .user_inputs
            .ready()
            .unwrap()
            .persisted()[0]
            .content,
        "one\r\n"
    );
}

#[test]
fn filesystem_error_retains_row_result_selection_editor_and_shows_separate_notice() {
    let (_temp, destination, mut app) = fixture(0);
    app.source_changed(0, destination.join("A.py"), Language::Python);
    run(&mut app, 3, "keep result");
    let original = last(&app, UserInputSelection::Persisted(3))
        .unwrap()
        .clone();
    app.select_case(selection(1));
    app.begin_selected_user_input_edit().unwrap();
    app.edit_user_input_insert("unsaved");
    let editor = app.active_user_input_edit().unwrap().clone();
    // A metadata directory is a deterministic filesystem failure on Windows and Unix.
    fs::create_dir(destination.join(".atc/user-inputs/A/meta.toml")).unwrap();
    delete(&mut app, &destination, 3);
    assert_eq!(ids(&app), [1, 3, 7]);
    assert_eq!(armed(&app), None);
    assert_eq!(app.case_selection(), Some(selection(1)));
    assert_eq!(app.active_user_input_edit(), Some(&editor));
    assert_eq!(
        last(&app, UserInputSelection::Persisted(3)),
        Some(&original)
    );
    assert!(
        sync_notice(&app)
            .unwrap()
            .starts_with("Could not delete User Input:")
    );
    assert!(rendered(&app, 100, 30).0.contains("! Input delete failed"));
    assert!(destination.join(".atc/user-inputs/A/3.in").is_file());
}

#[test]
fn not_found_without_a_successful_backend_reload_is_not_assumed_deleted() {
    let (_temp, destination, mut app) = fixture(0);
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    fs::remove_file(destination.join(".atc/user-inputs/A/3.in")).unwrap();
    fs::write(
        destination.join(".atc/user-inputs/A/meta.toml"),
        "invalid metadata",
    )
    .unwrap();
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    assert_eq!(ids(&app), [1, 3, 7]);
    assert_eq!(armed(&app), None);
    assert!(
        sync_notice(&app)
            .unwrap()
            .starts_with("Could not delete User Input:")
    );
}

#[test]
fn only_the_deleted_result_is_removed_and_unrelated_inflight_completion_still_applies() {
    let (_temp, destination, mut app) = fixture(1);
    app.source_changed(0, destination.join("A.py"), Language::Python);
    let samples = app.queue_run(0).unwrap();
    app.run_started(0, samples.run_id);
    for event in [
        message::TestEvent::TestRunStarted { total_cases: 1 },
        message::TestEvent::TestCaseComparison {
            number: 1,
            input: "sample".into(),
            expected: "expected".into(),
            actual: "sample result".into(),
        },
        message::TestEvent::TestCaseAccepted {
            number: 1,
            elapsed: Duration::from_millis(12),
        },
        message::TestEvent::TestRunFinished {
            accepted: 1,
            total_cases: 1,
        },
    ] {
        assert!(app.run_event(0, samples.run_id, event));
    }
    let stress = app.queue_stress(0, 17).unwrap();
    app.run_started(0, stress.run_id);
    assert!(app.stress_event(
        0,
        stress.run_id,
        message::StressEvent::Started {
            base_seed: 17,
            case_limit: Some(4)
        }
    ));
    assert!(app.stress_event(
        0,
        stress.run_id,
        message::StressEvent::Finished {
            cases: 4,
            elapsed: Duration::from_secs(1)
        }
    ));
    run(&mut app, 1, "one result");
    let one = last(&app, UserInputSelection::Persisted(1))
        .unwrap()
        .clone();
    run(&mut app, 3, "three result");
    app.begin_new_user_input().unwrap();
    app.edit_user_input_insert("draft");
    app.enqueue_selected_user_input();
    let draft_run = app.take_user_input_run_request().unwrap();
    let RunKind::UserInput(snapshot) = &draft_run.kind else {
        panic!()
    };
    assert!(app.user_input_run_event(0, draft_run.run_id, snapshot, result("draft result")));
    let draft = last(&app, UserInputSelection::Draft).unwrap().clone();
    app.select_case(selection(7));
    app.enqueue_selected_user_input();
    let seven = app.take_user_input_run_request().unwrap();
    let normal_before = format!("{:?}", app.current_problem().unwrap().run);
    let stress_before = format!("{:?}", app.current_problem().unwrap().stress);
    delete(&mut app, &destination, 3);
    assert_eq!(last(&app, UserInputSelection::Persisted(1)), Some(&one));
    assert_eq!(last(&app, UserInputSelection::Draft), Some(&draft));
    assert!(last(&app, UserInputSelection::Persisted(3)).is_none());
    assert!(app.run_started(0, seven.run_id));
    let RunKind::UserInput(snapshot) = &seven.kind else {
        panic!()
    };
    assert!(app.user_input_run_event(0, seven.run_id, snapshot, result("seven result")));
    assert_eq!(
        last(&app, UserInputSelection::Persisted(7)).unwrap().stdout,
        "seven result"
    );
    assert_eq!(
        format!("{:?}", app.current_problem().unwrap().run),
        normal_before
    );
    assert_eq!(
        format!("{:?}", app.current_problem().unwrap().stress),
        stress_before
    );
}

#[test]
fn armed_state_does_not_make_the_next_detail_action_hitbox_stale() {
    for action in [
        view::UserInputDetailAction::Edit,
        view::UserInputDetailAction::Run,
        view::UserInputDetailAction::Save,
        view::UserInputDetailAction::Cancel,
    ] {
        let (_temp, destination, mut app) = fixture(0);
        if matches!(
            action,
            view::UserInputDetailAction::Save | view::UserInputDetailAction::Cancel
        ) {
            app.begin_selected_user_input_edit().unwrap();
            app.edit_user_input_insert("changed");
        }
        click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
        let info = rendered_fold_info(&app, 100, 30);
        let at = info
            .user_input_detail_actions
            .iter()
            .find(|target| target.action == action)
            .unwrap()
            .area;
        assert!(handle_pointer_event_with_mouse_mode(
            &mut app,
            &mut detail_layout::DetailLayout::default(),
            &mut DetailScrollbarDragState::default(),
            pointer(PointerKind::Down(PointerButton::Left), at.x, at.y),
            &info,
            MouseMode::Cells,
            Some(&destination)
        ));
        assert_eq!(armed(&app), None);
        match action {
            view::UserInputDetailAction::Edit => assert!(app.user_input_editor_active()),
            view::UserInputDetailAction::Run => {
                assert!(last(&app, UserInputSelection::Persisted(1)).is_some())
            }
            view::UserInputDetailAction::Save | view::UserInputDetailAction::Cancel => {
                assert!(!app.user_input_editor_active())
            }
        }
        assert_eq!(ids(&app), [1, 3, 7]);
    }
}

#[test]
fn deleting_before_dispatch_drops_only_the_targets_pending_request() {
    let (_temp, destination, mut app) = fixture(0);
    app.source_changed(0, destination.join("A.py"), Language::Python);
    app.select_case(selection(3));
    app.enqueue_selected_user_input();
    // Use the controller directly so its normal post-pointer flush has not run yet.
    assert!(click_user_input_delete(&mut app, 3, Some(&destination)));
    assert!(click_user_input_delete(&mut app, 3, Some(&destination)));
    assert!(app.take_user_input_run_request().is_none());
}

#[test]
fn deleting_an_inflight_target_rejects_late_started_running_finished_and_failure() {
    let (_temp, destination, mut app) = fixture(0);
    app.source_changed(0, destination.join("A.py"), Language::Python);
    app.select_case(selection(3));
    app.enqueue_selected_user_input();
    let request = app.take_user_input_run_request().unwrap();
    delete(&mut app, &destination, 3);
    let RunKind::UserInput(snapshot) = &request.kind else {
        panic!()
    };
    assert!(!app.run_started(0, request.run_id));
    assert!(!app.user_input_run_event(0, request.run_id, snapshot, UserInputRunEvent::Running));
    assert!(!app.user_input_run_event(0, request.run_id, snapshot, result("late")));
    assert!(!app.run_failed(0, request.run_id, "late failure".to_string()));
    assert!(last(&app, UserInputSelection::Persisted(3)).is_none());
}

#[test]
fn row_click_keyboard_navigation_problem_switch_and_source_transition_disarm() {
    let (_temp, destination, mut app) = fixture(1);
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    click(&mut app, &destination, CasesRowAction::Select(selection(7)));
    assert_eq!(app.case_selection(), Some(selection(7)));
    assert_eq!(armed(&app), None);
    let (tx, _rx) = mpsc::channel();
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    handle_key_event(&mut app, key(KeyCode::Up, KeyEventKind::Press), &tx).unwrap();
    assert_eq!(armed(&app), None);
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    app.select_problem(1);
    assert_eq!(armed(&app), None);
    app.select_problem(0);
    assert_eq!(armed(&app), None);
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    app.source_changed(0, destination.join("A.py"), Language::Python);
    assert_eq!(armed(&app), None);
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    click(
        &mut app,
        &destination,
        CasesRowAction::Select(CaseSelection::Test(0)),
    );
    assert_eq!(armed(&app), None);
}

#[test]
fn new_edit_run_save_cancel_and_contest_actions_disarm() {
    for action in ["new", "edit", "run", "save", "cancel", "refresh", "stress"] {
        let (_temp, destination, mut app) = fixture(0);
        if matches!(action, "save" | "cancel") {
            app.begin_selected_user_input_edit().unwrap();
        }
        click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
        let (tx, _rx) = mpsc::channel();
        match action {
            "new" => {
                app.begin_new_user_input().unwrap();
            }
            "edit" => {
                sync_selected_user_input_before_edit(&mut app, Some(&destination));
            }
            "run" => {
                run_selected_user_input(&mut app, Some(&destination));
            }
            "save" => {
                save_selected_user_input(&mut app, &destination);
            }
            "cancel" => {
                app.cancel_user_input_edit();
            }
            "refresh" | "stress" => {
                execute_frontend_action(
                    &mut app,
                    if action == "refresh" {
                        FrontendAction::RefreshContest
                    } else {
                        FrontendAction::StartStress
                    },
                    TerminalInputContext::new(&tx, Some(&destination), None),
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(armed(&app), None, "{action}");
        assert!(destination.join(".atc/user-inputs/A/3.in").is_file());
    }
}

#[test]
fn hover_release_drag_and_scroll_do_not_execute_delete() {
    let (_temp, destination, mut app) = fixture(0);
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    for kind in [
        PointerKind::Move,
        PointerKind::Up(PointerButton::Left),
        PointerKind::Drag(PointerButton::Left),
    ] {
        let info = rendered_fold_info(&app, 100, 30);
        let at = target(&app, CasesRowAction::DeleteUserInput(3));
        handle_pointer_event_with_mouse_mode(
            &mut app,
            &mut detail_layout::DetailLayout::default(),
            &mut DetailScrollbarDragState::default(),
            pointer(kind, at.area.x, at.area.y),
            &info,
            MouseMode::Cells,
            Some(&destination),
        );
        assert_eq!(armed(&app), Some(3));
    }
    let info = rendered_fold_info(&app, 100, 30);
    handle_pointer_event_with_mouse_mode(
        &mut app,
        &mut detail_layout::DetailLayout::default(),
        &mut DetailScrollbarDragState::default(),
        pointer(
            PointerKind::ScrollDown,
            info.detail_area.x,
            info.detail_area.y,
        ),
        &info,
        MouseMode::Cells,
        Some(&destination),
    );
    assert_eq!(armed(&app), Some(3));
    let at = target(&app, CasesRowAction::DeleteUserInput(3));
    handle_pointer_event_with_mouse_mode(
        &mut app,
        &mut detail_layout::DetailLayout::default(),
        &mut DetailScrollbarDragState::default(),
        pointer(PointerKind::ScrollDown, at.area.x, at.area.y),
        &info,
        MouseMode::Cells,
        Some(&destination),
    );
    assert_eq!(ids(&app), [1, 3, 7]);
    assert!(destination.join(".atc/user-inputs/A/3.in").is_file());
}

#[test]
fn narrow_rendering_has_no_overlapping_hitboxes_or_border_and_new_input_overwrite() {
    let (_temp, _destination, mut app) = fixture(0);
    app.arm_user_input_delete(3);
    for width in 0..=65 {
        for height in [0, 1, 2, 5, 8, 12, 30] {
            let (_, info) = rendered(&app, width, height);
            for (i, a) in info.cases_row_targets.iter().enumerate() {
                let body = info.samples_body_area.unwrap();
                assert_eq!(a.area.intersection(body), a.area);
                if let Some(new) = info.new_input_area {
                    assert!(a.area.intersection(new).is_empty());
                }
                for b in &info.cases_row_targets[i + 1..] {
                    assert!(a.area.intersection(b.area).is_empty());
                }
            }
        }
    }
}

#[test]
fn ordered_delete_success_failure_and_other_editor_then_q_use_post_delete_state() {
    for mode in ["success", "failure", "editor"] {
        let (_temp, destination, mut app) = fixture(0);
        if mode == "failure" {
            fs::create_dir(destination.join(".atc/user-inputs/A/meta.toml")).unwrap();
        }
        if mode == "editor" {
            app.begin_selected_user_input_edit().unwrap();
        }
        let before_editor = app
            .active_user_input_edit()
            .map(|edit| edit.buffer().to_string());
        let at = target(&app, CasesRowAction::DeleteUserInput(3));
        let press = || {
            TerminalEvent::Pointer(pointer(
                PointerKind::Down(PointerButton::Left),
                at.area.x + 1,
                at.area.y,
            ))
        };
        let mut events = VecDeque::from([
            press(),
            press(),
            TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
        ]);
        assert!(!contains_plain_global_quit_event(&events, &app));
        let (tx, _rx) = mpsc::channel();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        for remaining in [2, 1, 0] {
            let info = rendered_fold_info(&app, 100, 30);
            assert!(
                handle_terminal_events_with_mouse_mode(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    &mut events,
                    MouseMode::Cells,
                    FrontendInputContext {
                        terminal: TerminalInputContext::new(&tx, Some(&destination), None),
                        contest_switch: None,
                        contest_refresh: None,
                        command_palette: None,
                        open_source: None,
                        editor_targets: None,
                        editor: None,
                    }
                )
                .unwrap()
            );
            assert_eq!(events.len(), remaining, "{mode}");
            if remaining == 2 {
                assert_eq!(armed(&app), Some(3));
                assert_eq!(ids(&app), [1, 3, 7]);
            } else {
                assert_eq!(armed(&app), None);
                assert_eq!(ids(&app).contains(&3), mode == "failure");
            }
        }
        if let Some(before) = before_editor {
            assert!(!app.should_quit());
            assert_eq!(
                app.active_user_input_edit().unwrap().buffer(),
                format!("{before}q")
            );
        } else {
            assert!(app.should_quit());
        }
    }
}

fn render_interaction(
    app: &WatchApp,
    terminal: &mut Terminal<TestBackend>,
    layout: &mut detail_layout::DetailLayout,
) -> view::RenderInfo {
    let mut info = view::RenderInfo::default();
    terminal
        .draw(|frame| info = view::render(frame, app, layout))
        .unwrap();
    info
}

fn dispatch_interaction(
    app: &mut WatchApp,
    layout: &mut detail_layout::DetailLayout,
    drag: &mut DetailScrollbarDragState,
    info: &view::RenderInfo,
    event: TerminalEvent,
) -> bool {
    let (tx, _rx) = mpsc::channel();
    let mut events = VecDeque::from([event]);
    let changed =
        super::super::handle_terminal_events(app, layout, drag, info, &mut events, &tx).unwrap();
    assert!(events.is_empty());
    changed
}

fn verify_armed_header_click(initially_collapsed: bool) {
    let (_temp, destination, mut app) = fixture(0);
    let kind = detail::DetailSectionKind::Input;
    if initially_collapsed {
        app.toggle_detail_section(kind);
    }
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut layout = detail_layout::DetailLayout::default();
    let mut drag = DetailScrollbarDragState::default();
    let info = render_interaction(&app, &mut terminal, &mut layout);
    let header = info
        .detail_section_headers
        .iter()
        .find(|header| header.kind == kind)
        .unwrap();
    assert!(dispatch_interaction(
        &mut app,
        &mut layout,
        &mut drag,
        &info,
        TerminalEvent::Pointer(pointer(
            PointerKind::Down(PointerButton::Left),
            header.area.x,
            header.area.y
        ))
    ));
    assert_eq!(armed(&app), None);
    let animation = drag
        .fold_animation
        .expect("header click must start animation");
    assert_eq!(animation.detail_revision, app.detail_revision());
    let midway = animation.started_at + Duration::from_millis(50);
    assert!(drag.advance_fold_animation(&mut app, midway));
    let frame = drag
        .fold_animation_frame(&app, midway)
        .expect("animation must remain valid");
    assert!((frame.expanded_fraction - 0.5).abs() < f64::EPSILON);
    assert_eq!(DETAIL_FOLD_ANIMATION_DURATION, Duration::from_millis(100));
    complete_fold_animation(&mut app, &mut drag);
    assert_eq!(
        app.detail_fold_state().is_collapsed(kind),
        !initially_collapsed
    );
    assert_eq!(ids(&app), [1, 3, 7]);
}

#[test]
fn armed_delete_then_expanded_header_click_completes_collapse_animation() {
    verify_armed_header_click(false);
}

#[test]
fn armed_delete_then_collapsed_header_click_completes_expand_animation() {
    verify_armed_header_click(true);
}

#[test]
fn armed_delete_then_thumb_down_preserves_drag_through_redraw_move_and_release() {
    let mut app = user_input_app(&"scrollable input\n".repeat(100));
    app.toggle_samples_pane();
    app.arm_user_input_delete(3);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut layout = detail_layout::DetailLayout::default();
    let mut drag = DetailScrollbarDragState::default();
    let info = render_interaction(&app, &mut terminal, &mut layout);
    let scrollbar = info
        .detail_scrollbar
        .as_ref()
        .expect("exact rendered scrollbar");
    let revision = app.detail_revision();
    let column = scrollbar.geometry.gutter.x;
    let row = scrollbar.geometry.thumb_start_row;
    assert!(dispatch_interaction(
        &mut app,
        &mut layout,
        &mut drag,
        &info,
        TerminalEvent::Pointer(pointer(PointerKind::Down(PointerButton::Left), column, row))
    ));
    assert_eq!(armed(&app), None);
    assert_eq!(app.detail_revision(), revision);
    assert_eq!(drag.active.unwrap().identity, scrollbar.identity);

    // Follow the production redraw/reconcile path before the next pointer event.
    let redrawn = render_interaction(&app, &mut terminal, &mut layout);
    drag.reconcile_render_info(&redrawn);
    assert!(drag.active.is_some());
    assert!(dispatch_interaction(
        &mut app,
        &mut layout,
        &mut drag,
        &redrawn,
        TerminalEvent::Pointer(pointer(
            PointerKind::Drag(PointerButton::Left),
            column,
            row + 3
        ))
    ));
    assert!(app.detail_scroll() > 0);
    assert!(drag.active.is_some());
    let redrawn = render_interaction(&app, &mut terminal, &mut layout);
    drag.reconcile_render_info(&redrawn);
    assert!(drag.active.is_some());
    dispatch_interaction(
        &mut app,
        &mut layout,
        &mut drag,
        &redrawn,
        TerminalEvent::Pointer(pointer(
            PointerKind::Up(PointerButton::Left),
            column,
            row + 3,
        )),
    );
    assert!(drag.active.is_none());
    assert_eq!(armed(&app), None);
}

#[test]
fn noop_case_wheel_keeps_delete_armed_in_both_directions() {
    let mut app = user_input_app("only input");
    app.toggle_samples_pane();
    app.arm_user_input_delete(3);
    let info = rendered_fold_info(&app, 100, 30);
    let at = info.samples_body_area.unwrap();
    let revision = app.detail_revision();
    for kind in [PointerKind::ScrollDown, PointerKind::ScrollUp] {
        assert!(!dispatch_mouse(
            &mut app,
            &mut detail_layout::DetailLayout::default(),
            &mut DetailScrollbarDragState::default(),
            &info,
            kind,
            at.x,
            at.y
        ));
        assert_eq!(app.case_selection(), Some(selection(3)));
        assert_eq!(armed(&app), Some(3));
        assert_eq!(app.detail_revision(), revision);
        assert!(rendered(&app, 100, 30).0.contains("Input 1   ×?"));
    }
}

#[test]
fn case_wheel_disarms_only_when_selection_actually_changes() {
    for (kind, expected) in [(PointerKind::ScrollDown, 3), (PointerKind::ScrollUp, 7)] {
        let (_temp, destination, mut app) = fixture(0);
        click(&mut app, &destination, CasesRowAction::DeleteUserInput(1));
        let info = rendered_fold_info(&app, 100, 30);
        let at = info.samples_body_area.unwrap();
        assert!(dispatch_mouse(
            &mut app,
            &mut detail_layout::DetailLayout::default(),
            &mut DetailScrollbarDragState::default(),
            &info,
            kind,
            at.x,
            at.y
        ));
        assert_eq!(app.case_selection(), Some(selection(expected)));
        assert_eq!(armed(&app), None);
        assert_eq!(ids(&app), [1, 3, 7]);
    }
}

#[test]
fn keyboard_noop_case_and_problem_navigation_keeps_delete_armed() {
    let mut app = user_input_app("only input");
    app.arm_user_input_delete(3);
    let (tx, _rx) = mpsc::channel();
    let revision = app.detail_revision();
    for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('h'),
            KeyCode::Char('l'),
        ] {
            assert!(!handle_key_event(&mut app, key(code, kind), &tx).unwrap());
            assert_eq!(armed(&app), Some(3));
            assert_eq!(app.case_selection(), Some(selection(3)));
            assert_eq!(app.detail_revision(), revision);
        }
    }
}

#[test]
fn keyboard_actual_case_navigation_disarms_in_both_directions() {
    for (code, expected) in [
        (KeyCode::Down, 3),
        (KeyCode::Up, 7),
        (KeyCode::Char('j'), 3),
        (KeyCode::Char('k'), 7),
    ] {
        let (_temp, destination, mut app) = fixture(0);
        click(&mut app, &destination, CasesRowAction::DeleteUserInput(1));
        let (tx, _rx) = mpsc::channel();
        assert!(handle_key_event(&mut app, key(code, KeyEventKind::Press), &tx).unwrap());
        assert_eq!(armed(&app), None);
        assert_eq!(app.case_selection(), Some(selection(expected)));
    }
}

#[test]
fn delete_appearance_redraw_keeps_detail_revision_and_row_hitboxes_valid() {
    let (_temp, destination, mut app) = fixture(0);
    let info = rendered_fold_info(&app, 100, 30);
    let at = target(&app, CasesRowAction::DeleteUserInput(3));
    let revision = app.detail_revision();
    click(&mut app, &destination, CasesRowAction::DeleteUserInput(3));
    assert_eq!(app.detail_revision(), revision);
    assert_eq!(
        info.cases_row_target_at(&app, at.area.x, at.area.y),
        Some(at)
    );
    assert!(rendered(&app, 100, 30).0.contains("Input 2   ×?"));
    // An explicit click on the selected label still disarms and requests redraw.
    click(&mut app, &destination, CasesRowAction::Select(selection(1)));
    assert_eq!(app.detail_revision(), revision);
    assert_eq!(
        info.cases_row_target_at(&app, at.area.x, at.area.y),
        Some(at)
    );
    assert!(!rendered(&app, 100, 30).0.contains("×?"));
    // Actual selection changes still invalidate the old Cases targets.
    app.next_case();
    assert!(
        info.cases_row_target_at(&app, at.area.x, at.area.y)
            .is_none()
    );
}

#[test]
fn detail_wheel_scroll_preserves_delete_armed_and_detail_revision() {
    let mut app = user_input_app(&"scrollable input\n".repeat(100));
    app.toggle_samples_pane();
    app.arm_user_input_delete(3);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut layout = detail_layout::DetailLayout::default();
    let mut drag = DetailScrollbarDragState::default();
    let revision = app.detail_revision();
    for kind in [PointerKind::ScrollDown, PointerKind::ScrollUp] {
        let info = render_interaction(&app, &mut terminal, &mut layout);
        let before = app.detail_scroll();
        assert!(dispatch_interaction(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            TerminalEvent::Pointer(pointer(kind, info.detail_area.x, info.detail_area.y + 1))
        ));
        assert_ne!(app.detail_scroll(), before);
        assert_eq!(armed(&app), Some(3));
        assert_eq!(app.detail_revision(), revision);
    }
}
