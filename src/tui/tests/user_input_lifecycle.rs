use super::*;
use crate::tui::message::{RunKind, UserInputRunEvent, UserInputRunResult, UserInputRunStatus};
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture() -> (tempfile::TempDir, PathBuf, WatchApp) {
    let (temp, destination) = user_input_workspace();
    for index in ["A", "B"] {
        write_sync_inputs(&destination, index, &[(7, "original\r\nsecond line")]);
        fs::write(destination.join(format!("{index}.py")), "pass\n").unwrap();
    }
    let mut app = sync_test_app(&destination, &[1, 1]);
    app.source_changed(1, destination.join("B.py"), Language::Python);
    app.source_changed(0, destination.join("A.py"), Language::Python);
    select_sync_input(&mut app, 7);
    (temp, destination, app)
}

fn input_states(app: &WatchApp) -> Vec<app::UserInputState> {
    app.problems()
        .iter()
        .map(|problem| problem.user_inputs.clone())
        .collect()
}

#[test]
fn refresh_rejects_every_active_editor_without_touching_state_or_starting_fetch() {
    // Include clean editors: whether an edit is dirty is not the refresh boundary.
    for (name, draft, dirty, selection) in [
        ("selected draft", true, true, 0),
        ("draft hidden by sample", true, true, 1),
        ("dirty persisted hidden by sample", false, true, 1),
        ("editor in another problem", true, true, 2),
        ("clean persisted in another problem", false, false, 2),
    ] {
        let (_temp, destination, mut app) = fixture();
        if draft {
            app.begin_new_user_input().unwrap();
        } else {
            app.begin_selected_user_input_edit().unwrap();
        }
        if dirty {
            app.edit_user_input_insert("changed\r\n界 input");
        }
        app.edit_user_input_end();
        app.edit_user_input_up(); // Set preferred_column as well as cursor.
        let save = app.user_input_save_snapshot().unwrap();
        app.fail_user_input_save(&save, "previous save error".into(), None)
            .unwrap();
        match selection {
            1 => {
                app.select_case(app::CaseSelection::Test(0));
            }
            2 => {
                app.select_problem(1);
            }
            _ => {}
        }
        let before = input_states(&app); // Includes baseline, buffer, cursor, preferred column, error.
        let selected = (
            app.selected_problem(),
            app.case_selection(),
            app.detail_scroll(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let task_calls = Arc::clone(&calls);
        let mut refresh = RefreshContestController::new(
            "abc123",
            Arc::new(move |_| {
                task_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            Arc::new(|| Ok(true)),
            None,
        );
        assert_eq!(
            FrontendAction::RefreshContest.availability(&app, false),
            FrontendActionAvailability::Unavailable(REFRESH_EDIT_NOTICE),
            "{name}"
        );
        let (run_tx, run_rx) = mpsc::channel();
        assert!(
            execute_frontend_action(
                &mut app,
                FrontendAction::RefreshContest,
                TerminalInputContext::new(&run_tx, Some(&destination), None),
                None,
                Some(&mut refresh),
                None,
                None,
            )
            .unwrap()
        );
        assert_eq!(
            refresh.modal().unwrap().error.as_deref(),
            Some(REFRESH_EDIT_NOTICE)
        );
        // Retry from the actual modal dispatcher must not bypass the guard either.
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);
        handle_refresh_frontend_events(&mut app, &mut events, &mut refresh, None).unwrap();
        assert!(refresh.operation.active.is_none(), "{name}");
        assert!(!refresh.refresh_requested, "{name}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{name}");
        assert_eq!(input_states(&app), before, "{name}");
        assert_eq!(
            (
                app.selected_problem(),
                app.case_selection(),
                app.detail_scroll()
            ),
            selected,
            "{name}"
        );
        assert!(run_rx.try_recv().is_err());
    }
}

#[test]
fn refresh_without_an_editor_starts_fetch_and_requests_rebuild() {
    let (_temp, destination, mut app) = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let task_calls = Arc::clone(&calls);
    let mut refresh = RefreshContestController::new(
        "abc123",
        Arc::new(move |_| {
            task_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        Arc::new(|| Ok(true)),
        None,
    );
    let (run_tx, _run_rx) = mpsc::channel();
    execute_frontend_action(
        &mut app,
        FrontendAction::RefreshContest,
        TerminalInputContext::new(&run_tx, Some(&destination), None),
        None,
        Some(&mut refresh),
        None,
        None,
    )
    .unwrap();
    wait_for_refresh_operation(&mut refresh);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(refresh.refresh_requested);
}

#[test]
fn source_message_between_preflights_preserves_editor_routing_and_exact_edit_state() {
    for problem in [0, 1] {
        for character in ['q', 'r', ':'] {
            let (_temp, destination, mut app) = fixture();
            app.begin_selected_user_input_edit().unwrap();
            app.edit_user_input_insert("unsaved\n");
            app.edit_user_input_up();
            let before = input_states(&app);
            let save = app.user_input_save_snapshot().unwrap();
            let selected = (app.selected_problem(), app.case_selection());
            let mut events = VecDeque::from([TerminalEvent::Key(key(
                KeyCode::Char(character),
                KeyEventKind::Press,
            ))]);
            assert!(!contains_plain_global_quit_event(&events, &app));
            let (tx, rx) = mpsc::channel();
            let (run_tx, run_rx) = mpsc::channel();
            tx.send(Message::SourceChanged {
                problem,
                path: destination.join(if problem == 0 { "A.py" } else { "B.py" }),
                language: Language::Python,
            })
            .unwrap();
            handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap();
            assert_eq!(input_states(&app), before);
            assert_eq!(app.user_input_save_snapshot().unwrap(), save);
            assert_eq!((app.selected_problem(), app.case_selection()), selected);
            assert!(!contains_plain_global_quit_event(&events, &app));
            let editor = app.selected_user_input_edit().unwrap();
            let mut expected = editor.buffer().to_string();
            expected.insert(editor.cursor(), character);
            let info = rendered_fold_info(&app, 120, 35);
            dispatch_sync_events(&mut app, &info, &mut events, &destination);
            assert!(!app.should_quit());
            assert_eq!(app.selected_user_input_edit().unwrap().buffer(), expected);
            assert!(matches!(
                run_rx.try_recv().unwrap(),
                RunWorkerCommand::Run(RunRequest {
                    kind: RunKind::Samples,
                    ..
                })
            ));
            assert!(
                run_rx.try_recv().is_err(),
                "source notifications must not enqueue User Input"
            );
        }
    }
}

#[test]
fn background_tick_waits_for_all_delivered_terminal_events_not_only_q() {
    let (_temp, destination, mut app) = fixture();
    app.begin_selected_user_input_edit().unwrap();
    let context = workspace_context(destination.parent().unwrap());
    let mut resolve = |_: &str| ContestSwitchResolution::rejected(None, "unused".into());
    let mut switch = ContestSwitchController::new(
        &context,
        &destination,
        &mut resolve,
        successful_create_task(),
    );
    let mut refresh =
        RefreshContestController::new("abc123", Arc::new(|_| Ok(())), Arc::new(|| Ok(true)), None);
    let (tx, rx) = mpsc::channel();
    let (run_tx, run_rx) = mpsc::channel();
    let (analysis_tx, _analysis_requests) = mpsc::channel();
    let (_analysis_results, analysis_rx) = mpsc::channel();
    let mut layout = detail_layout::DetailLayout::default();
    tx.send(Message::SourceChanged {
        problem: 1,
        path: destination.join("B.py"),
        language: Language::Python,
    })
    .unwrap();
    for event in [
        TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
        TerminalEvent::Paste("paste\r\n".into()),
        TerminalEvent::Pointer(pointer(PointerKind::Down(PointerButton::Left), 2, 2)),
    ] {
        let events = VecDeque::from([event]);
        assert!(
            !handle_background_events(
                &mut app,
                &events,
                &destination,
                SessionChannels::new(&rx, &run_tx, &analysis_tx, &analysis_rx),
                &mut switch,
                &mut refresh,
                &mut layout
            )
            .unwrap()
        );
        assert!(run_rx.try_recv().is_err());
    }
    assert!(
        handle_background_events(
            &mut app,
            &VecDeque::new(),
            &destination,
            SessionChannels::new(&rx, &run_tx, &analysis_tx, &analysis_rx),
            &mut switch,
            &mut refresh,
            &mut layout
        )
        .unwrap()
    );
    assert!(matches!(
        run_rx.try_recv().unwrap(),
        RunWorkerCommand::Run(RunRequest {
            problem: 1,
            kind: RunKind::Samples,
            ..
        })
    ));
    assert!(app.user_input_editor_active());
}

#[test]
fn a_new_q_after_successful_save_is_global_quit() {
    let (_temp, destination, mut app) = fixture();
    assert!(contains_plain_global_quit_event(
        &VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char('q'),
            KeyEventKind::Press
        ))]),
        &app
    ));
    app.begin_selected_user_input_edit().unwrap();
    app.edit_user_input_insert("saved");
    save_user_input_by_key(&mut app, &destination);
    assert!(!app.user_input_editor_active());
    let mut events = VecDeque::from([TerminalEvent::Key(key(
        KeyCode::Char('q'),
        KeyEventKind::Press,
    ))]);
    assert!(contains_plain_global_quit_event(&events, &app));
    let info = rendered_fold_info(&app, 120, 35);
    dispatch_sync_events(&mut app, &info, &mut events, &destination);
    assert!(app.should_quit());
}

#[test]
fn source_removal_invalidates_old_runs_preserves_edits_and_allows_recreation() {
    let (_temp, destination, mut app) = fixture();
    app.begin_selected_user_input_edit().unwrap();
    app.edit_user_input_insert("unsaved\r\n");
    app.edit_user_input_up();
    let edit = app.user_input_save_snapshot().unwrap();
    assert!(run_selected_user_input(&mut app, Some(&destination)));
    let request = app.take_user_input_run_request().unwrap();
    let RunKind::UserInput(snapshot) = &request.kind else {
        panic!("expected User Input")
    };
    assert!(app.run_started(0, request.run_id));
    let unrelated = format!("{:?}", app.problems()[1]);
    let selection = app.case_selection();
    fs::remove_file(destination.join("A.py")).unwrap();
    let (tx, rx) = mpsc::channel();
    let (run_tx, run_rx) = mpsc::channel();
    tx.send(Message::SourceRemoved {
        problem: 0,
        path: destination.join("A.py"),
        language: Language::Python,
    })
    .unwrap();
    handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap();
    assert_eq!(
        run_rx.try_recv().unwrap(),
        RunWorkerCommand::RetireUserInputRuns {
            problem: 0,
            before_source_revision: snapshot.source_revision + 1,
        }
    );
    assert!(snapshot.start_gate.is_retired(snapshot.source_revision));
    assert!(app.problems()[0].source.is_none());
    assert!(
        app.problems()[0]
            .user_inputs
            .ready()
            .unwrap()
            .last_run(app::UserInputSelection::Persisted(7))
            .is_none()
    );
    assert_eq!(app.user_input_save_snapshot().unwrap(), edit);
    assert_eq!(app.case_selection(), selection);
    assert_eq!(format!("{:?}", app.problems()[1]), unrelated);
    tx.send(Message::RunStarted {
        problem: 0,
        run_id: request.run_id,
    })
    .unwrap();
    tx.send(Message::UserInputRunEvent {
        problem: 0,
        run_id: request.run_id,
        snapshot: Arc::clone(snapshot),
        event: UserInputRunEvent::Finished(UserInputRunResult {
            status: UserInputRunStatus::Finished,
            stdout: "stale".into(),
            stderr: String::new(),
            elapsed: Duration::from_millis(1),
        }),
    })
    .unwrap();
    tx.send(Message::RunFailed {
        problem: 0,
        run_id: request.run_id,
        error: "stale failure".into(),
    })
    .unwrap();
    tx.send(Message::RunCompleted {
        problem: 0,
        run_id: request.run_id,
    })
    .unwrap();
    assert!(!handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap());
    assert!(app.queue_run(0).is_none());
    assert!(app.queue_stress(0, 1).is_none());
    run_selected_user_input(&mut app, Some(&destination));
    assert!(app.take_user_input_run_request().is_none());
    assert!(run_rx.try_recv().is_err());
    fs::write(destination.join("A.py"), "print(1)").unwrap();
    tx.send(Message::SourceChanged {
        problem: 0,
        path: destination.join("A.py"),
        language: Language::Python,
    })
    .unwrap();
    handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap();
    assert_eq!(
        app.problems()[0].source.as_ref().unwrap().path,
        destination.join("A.py")
    );
    assert!(matches!(
        run_rx.try_recv().unwrap(),
        RunWorkerCommand::Run(RunRequest {
            kind: RunKind::Samples,
            ..
        })
    ));
    assert!(run_rx.try_recv().is_err());
    run_selected_user_input(&mut app, Some(&destination));
    let next = app.take_user_input_run_request().unwrap();
    let RunKind::UserInput(next_snapshot) = next.kind else {
        panic!("expected User Input")
    };
    assert_eq!(next_snapshot.source_revision, snapshot.source_revision + 2);
    assert_eq!(
        next_snapshot
            .start_gate
            .start_if_current(next_snapshot.source_revision, || "new run"),
        Some("new run")
    );
    assert!(
        snapshot
            .start_gate
            .start_if_current(snapshot.source_revision, || panic!("old run admitted"))
            .is_none()
    );
}

#[test]
fn source_notifications_clear_finished_input_results_only_for_the_changed_problem() {
    for removed in [false, true] {
        let (_temp, destination, mut app) = fixture();
        for problem in [0, 1] {
            app.select_problem(problem);
            select_sync_input(&mut app, 7);
            run_selected_user_input(&mut app, Some(&destination));
            let request = app.take_user_input_run_request().unwrap();
            let RunKind::UserInput(snapshot) = request.kind else {
                panic!("expected User Input")
            };
            assert!(app.run_started(problem, request.run_id));
            assert!(app.user_input_run_event(
                problem,
                request.run_id,
                &snapshot,
                UserInputRunEvent::Finished(UserInputRunResult {
                    status: UserInputRunStatus::Finished,
                    stdout: "previous output".into(),
                    stderr: "previous diagnostic".into(),
                    elapsed: Duration::from_millis(1),
                }),
            ));
        }
        app.select_problem(0);
        select_sync_input(&mut app, 7);
        app.begin_selected_user_input_edit().unwrap();
        app.edit_user_input_insert("unsaved\r\n界");
        app.edit_user_input_up();
        let editor = app.selected_user_input_edit().unwrap().clone();
        let other = format!("{:?}", app.problems()[1]);
        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();
        let path = destination.join("A.py");
        let message = if removed {
            fs::remove_file(&path).unwrap();
            Message::SourceRemoved {
                problem: 0,
                path,
                language: Language::Python,
            }
        } else {
            fs::write(&path, "print(1)").unwrap();
            Message::SourceChanged {
                problem: 0,
                path,
                language: Language::Python,
            }
        };
        tx.send(message).unwrap();
        handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap();
        assert!(
            app.problems()[0]
                .user_inputs
                .ready()
                .unwrap()
                .last_run(app::UserInputSelection::Persisted(7))
                .is_none()
        );
        assert_eq!(app.selected_user_input_edit(), Some(&editor));
        assert_eq!(format!("{:?}", app.problems()[1]), other);
        let requests: Vec<_> = run_rx.try_iter().collect();
        if removed {
            assert!(matches!(
                requests.as_slice(),
                [RunWorkerCommand::RetireUserInputRuns { problem: 0, .. }]
            ));
        } else {
            assert!(matches!(
                requests.as_slice(),
                [RunWorkerCommand::Run(RunRequest {
                    kind: RunKind::Samples,
                    ..
                })]
            ));
        }
    }
}

#[test]
fn source_removal_discards_input_requests_that_have_not_reached_the_worker() {
    let (_temp, destination, mut app) = fixture();
    app.begin_selected_user_input_edit().unwrap();
    app.edit_user_input_insert("queued input");
    assert!(run_selected_user_input(&mut app, Some(&destination)));
    let editor = app.selected_user_input_edit().unwrap().clone();
    let (tx, rx) = mpsc::channel();
    let (run_tx, run_rx) = mpsc::channel();
    tx.send(Message::SourceRemoved {
        problem: 0,
        path: destination.join("A.py"),
        language: Language::Python,
    })
    .unwrap();
    handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap();
    assert!(app.take_user_input_run_request().is_none());
    assert!(matches!(
        run_rx.try_recv().unwrap(),
        RunWorkerCommand::RetireUserInputRuns { problem: 0, .. }
    ));
    assert!(run_rx.try_recv().is_err());
    assert_eq!(app.selected_user_input_edit(), Some(&editor));
}

#[test]
fn source_removal_revokes_sent_requests_even_after_their_ui_result_was_cleared() {
    let (_temp, destination, mut app) = fixture();
    app.begin_selected_user_input_edit().unwrap();
    let (run_tx, run_rx) = mpsc::channel();
    run_selected_user_input(&mut app, Some(&destination));
    flush_user_input_run_requests(&mut app, &run_tx).unwrap();
    let RunKind::UserInput(old) = received_run(&run_rx).kind else {
        panic!("expected User Input");
    };
    assert!(app.take_user_input_run_request().is_none());
    app.edit_user_input_insert("changed after handoff");
    assert!(
        app.problems()[0]
            .user_inputs
            .ready()
            .unwrap()
            .last_run(app::UserInputSelection::Persisted(7))
            .is_none()
    );

    let (tx, rx) = mpsc::channel();
    fs::remove_file(destination.join("A.py")).unwrap();
    tx.send(Message::SourceRemoved {
        problem: 0,
        path: destination.join("A.py"),
        language: Language::Python,
    })
    .unwrap();
    handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap();
    assert_eq!(
        run_rx.try_recv().unwrap(),
        RunWorkerCommand::RetireUserInputRuns {
            problem: 0,
            before_source_revision: old.source_revision + 1
        }
    );
    assert!(
        old.start_gate
            .start_if_current(old.source_revision, || panic!(
                "sent request was not revoked"
            ))
            .is_none()
    );
    assert!(run_rx.try_recv().is_err());
}

#[test]
fn removing_an_unselected_language_does_not_clear_the_current_source() {
    let (_temp, destination, mut app) = fixture();
    let before = format!("{app:?}");
    let (tx, rx) = mpsc::channel();
    let (run_tx, run_rx) = mpsc::channel();
    tx.send(Message::SourceRemoved {
        problem: 0,
        path: destination.join("A.cpp"),
        language: Language::Cpp,
    })
    .unwrap();
    assert!(!handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap());
    assert_eq!(format!("{app:?}"), before);
    assert!(run_rx.try_recv().is_err());
}

#[test]
fn removing_an_active_stress_source_sends_the_existing_cancel_command() {
    let (_temp, destination, mut app) = fixture();
    let request = app.queue_stress(0, 1).unwrap();
    let (tx, rx) = mpsc::channel();
    let (run_tx, run_rx) = mpsc::channel();
    tx.send(Message::SourceRemoved {
        problem: 0,
        path: destination.join("A.py"),
        language: Language::Python,
    })
    .unwrap();
    handle_messages_with_destination(&mut app, &rx, &run_tx, Some(&destination)).unwrap();
    assert!(matches!(
        run_rx.try_recv().unwrap(),
        RunWorkerCommand::RetireUserInputRuns { problem: 0, .. }
    ));
    assert_eq!(
        run_rx.try_recv().unwrap(),
        RunWorkerCommand::CancelStress {
            problem: 0,
            run_id: request.run_id
        }
    );
    assert!(app.active_stress_identity().is_none());
    assert_eq!(app.problems()[0].stress.phase, app::StressPhase::Cancelled);
}
