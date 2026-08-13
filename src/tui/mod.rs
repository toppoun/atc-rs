pub mod app;
pub mod message;
pub mod reporter;
pub mod view;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseEvent, MouseEventKind};
use message::{Message, RunRequest};
use ratatui::DefaultTerminal;

use crate::model::Contest;
use app::WatchApp;

const MAX_MESSAGES_PER_TICK: usize = 256;
const DETAIL_SCROLL_LINES: u16 = 3;

fn handle_messages(
    app: &mut WatchApp,
    message_rx: &Receiver<Message>,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    let mut changed = false;

    for _ in 0..MAX_MESSAGES_PER_TICK {
        match message_rx.try_recv() {
            Ok(Message::SourceChanged {
                problem,
                path,
                language,
            }) => {
                if app.source_changed(problem, path, language) {
                    changed = true;

                    if let Some(request) = app.queue_run(problem) {
                        run_tx.send(request).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "test worker request channel disconnected",
                            )
                        })?;
                    }
                }
            }

            Ok(Message::WatcherFailed(error)) => {
                return Err(error);
            }

            Ok(Message::WorkerFailed(error)) => {
                return Err(error);
            }

            Err(TryRecvError::Empty) => {
                break;
            }

            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "background message channel disconnected",
                ));
            }
            Ok(Message::RunStarted { run_id, problem }) => {
                if app.run_started(problem, run_id) {
                    changed = true;
                }
            }

            Ok(Message::RunEvent {
                run_id,
                problem,
                event,
            }) => {
                if app.run_event(problem, run_id, event) {
                    changed = true;
                }
            }

            Ok(Message::RunCompleted { run_id, problem }) => {
                if app.run_completed(problem, run_id) {
                    changed = true;
                }
            }

            Ok(Message::RunFailed {
                run_id,
                problem,
                error,
            }) => {
                if app.run_failed(problem, run_id, error) {
                    changed = true;
                }
            }
        }
    }

    Ok(changed)
}

pub fn run(
    terminal: &mut DefaultTerminal,
    contest: &Contest,
    sample_counts: Vec<usize>,
    message_rx: Receiver<Message>,
    run_tx: Sender<RunRequest>,
) -> io::Result<()> {
    let mut app = WatchApp::new(contest, sample_counts)?;

    let mut dirty = true;

    let mut render_info = view::RenderInfo::default();

    while !app.should_quit() {
        if handle_messages(&mut app, &message_rx, &run_tx)? {
            dirty = true;
        }

        if dirty {
            let mut next_render_info = view::RenderInfo::default();

            terminal.draw(|frame| {
                next_render_info = view::render(frame, &app);
            })?;

            render_info = next_render_info;

            app.clamp_detail_scroll(render_info.max_detail_scroll);

            dirty = false;
        }

        if event::poll(Duration::from_millis(20))? {
            match event::read()? {
                Event::Key(key) => {
                    if handle_key_event(&mut app, key) {
                        dirty = true;
                    }
                }

                Event::Mouse(mouse) => {
                    if handle_mouse_event(&mut app, mouse, &render_info) {
                        dirty = true;
                    }
                }

                Event::Resize(_, _) => {
                    dirty = true;
                }

                _ => {}
            }
        }
    }

    Ok(())
}

fn handle_key_event(app: &mut WatchApp, key: KeyEvent) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }

    match key.code {
        KeyCode::Char('q') if key.kind == KeyEventKind::Press => {
            app.quit();
            true
        }

        KeyCode::Char('d') if key.kind == KeyEventKind::Press => {
            app.toggle_debug();
            true
        }

        KeyCode::Char('s') if key.kind == KeyEventKind::Press => {
            app.toggle_samples_pane();
            true
        }

        KeyCode::Char('h') | KeyCode::Left => app.previous_problem(),

        KeyCode::Char('l') | KeyCode::Right => app.next_problem(),

        KeyCode::Char('j') | KeyCode::Down => app.next_case(),

        KeyCode::Char('k') | KeyCode::Up => app.previous_case(),

        _ => false,
    }
}

fn contains(area: ratatui::layout::Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn handle_mouse_event(
    app: &mut WatchApp,
    mouse: MouseEvent,
    render_info: &view::RenderInfo,
) -> bool {
    if let Some(samples_area) = render_info.samples_area
        && contains(samples_area, mouse.column, mouse.row)
    {
        return match mouse.kind {
            MouseEventKind::ScrollUp => app.previous_case(),

            MouseEventKind::ScrollDown => app.next_case(),

            _ => false,
        };
    }

    if contains(render_info.detail_area, mouse.column, mouse.row) {
        return match mouse.kind {
            MouseEventKind::ScrollUp => app.scroll_detail_up(DETAIL_SCROLL_LINES),

            MouseEventKind::ScrollDown => {
                let previous = app.detail_scroll();

                app.scroll_detail_down(DETAIL_SCROLL_LINES);

                app.clamp_detail_scroll(render_info.max_detail_scroll);

                app.detail_scroll() != previous
            }

            _ => false,
        };
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;
    use crate::model::{Contest, Problem};
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    fn app() -> WatchApp {
        app_with_problems(&[3])
    }

    fn app_with_problems(sample_counts: &[usize]) -> WatchApp {
        WatchApp::new(
            &Contest {
                contest_id: "abc123".to_string(),
                problems: sample_counts
                    .iter()
                    .enumerate()
                    .map(|(index, _)| Problem {
                        index: char::from(b'A' + index as u8).to_string(),
                        title: format!("Problem {index}"),
                        task_id: format!("abc123_{index}"),
                        url: format!("https://example.invalid/{index}"),
                    })
                    .collect(),
            },
            sample_counts.to_vec(),
        )
        .unwrap()
    }

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
    }

    #[test]
    fn press_and_repeat_are_handled_but_release_is_ignored() {
        let mut app = app();

        handle_key_event(&mut app, key(KeyCode::Char('j'), KeyEventKind::Press));
        assert_eq!(app.selected_case(), 1);

        handle_key_event(&mut app, key(KeyCode::Down, KeyEventKind::Repeat));
        assert_eq!(app.selected_case(), 2);

        handle_key_event(&mut app, key(KeyCode::Char('q'), KeyEventKind::Release));
        assert!(!app.should_quit());
        handle_key_event(&mut app, key(KeyCode::Char('q'), KeyEventKind::Press));
        assert!(app.should_quit());
    }

    #[test]
    fn repeat_does_not_toggle_debug_repeatedly() {
        let mut app = app();

        handle_key_event(&mut app, key(KeyCode::Char('d'), KeyEventKind::Press));
        assert!(app.debug_enabled());
        handle_key_event(&mut app, key(KeyCode::Char('d'), KeyEventKind::Repeat));
        assert!(app.debug_enabled());
    }

    #[test]
    fn up_and_k_move_to_the_previous_case() {
        let mut app = app();

        handle_key_event(&mut app, key(KeyCode::Up, KeyEventKind::Press));
        assert_eq!(app.selected_case(), 2);
        handle_key_event(&mut app, key(KeyCode::Char('k'), KeyEventKind::Press));
        assert_eq!(app.selected_case(), 1);
    }

    #[test]
    fn unknown_and_no_op_navigation_keys_are_not_dirty() {
        let mut app = app_with_problems(&[1]);

        assert!(!handle_key_event(
            &mut app,
            key(KeyCode::Char('x'), KeyEventKind::Press)
        ));
        assert!(!handle_key_event(
            &mut app,
            key(KeyCode::Right, KeyEventKind::Press)
        ));
        assert!(!handle_key_event(
            &mut app,
            key(KeyCode::Down, KeyEventKind::Press)
        ));
    }

    #[test]
    fn source_messages_update_state_and_multiple_messages_use_the_latest_source() {
        let mut app = app_with_problems(&[3, 2]);
        app.previous_case();
        let (tx, rx) = mpsc::channel();
        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.cpp"),
            language: Language::Cpp,
        })
        .unwrap();
        tx.send(Message::SourceChanged {
            problem: 1,
            path: PathBuf::from("B.cpp"),
            language: Language::Cpp,
        })
        .unwrap();
        tx.send(Message::SourceChanged {
            problem: 1,
            path: PathBuf::from("B.py"),
            language: Language::Python,
        })
        .unwrap();

        let (run_tx, _run_rx) = mpsc::channel();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        let problem = app.current_problem().unwrap();
        assert_eq!(problem.index, "B");
        assert_eq!(app.selected_case(), 0);
        let source = problem.source.as_ref().unwrap();
        assert_eq!(source.path, Path::new("B.py"));
        assert_eq!(source.language, Language::Python);
    }

    #[test]
    fn message_drain_is_bounded_to_keep_input_responsive() {
        let mut app = app();

        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        // 最初の1tickで処理できる上限いっぱいまでC++の変更を積む
        for _ in 0..MAX_MESSAGES_PER_TICK {
            tx.send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("A.cpp"),
                language: Language::Cpp,
            })
            .unwrap();
        }

        // 257件目。これは最初のhandle_messagesでは処理されないはず
        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.py"),
            language: Language::Python,
        })
        .unwrap();

        // 1tick目
        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        assert_eq!(
            app.current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .language,
            Language::Cpp
        );

        // 256件だけRunRequestが作られている
        let first_requests: Vec<_> = run_rx.try_iter().collect();

        assert_eq!(first_requests.len(), MAX_MESSAGES_PER_TICK);
        assert_eq!(first_requests[0].run_id, 1);
        assert_eq!(
            first_requests.last().unwrap().run_id,
            MAX_MESSAGES_PER_TICK as u64
        );

        // 2tick目で残っていたA.pyを処理
        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        assert_eq!(
            app.current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .language,
            Language::Python
        );

        let second_requests: Vec<_> = run_rx.try_iter().collect();

        assert_eq!(second_requests.len(), 1);
        assert_eq!(second_requests[0].run_id, MAX_MESSAGES_PER_TICK as u64 + 1);
        assert_eq!(second_requests[0].problem, 0);
        assert_eq!(second_requests[0].language, Language::Python);
    }

    #[test]
    fn watcher_failure_and_disconnected_channel_are_errors() {
        let mut app = app();

        let (tx, rx) = mpsc::channel();
        let (run_tx, _run_rx) = mpsc::channel();

        tx.send(Message::WatcherFailed(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "watch failed",
        )))
        .unwrap();

        let error = handle_messages(&mut app, &rx, &run_tx).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "watch failed");

        let (tx, rx) = mpsc::channel();
        tx.send(Message::WorkerFailed(io::Error::other("worker panicked")))
            .unwrap();
        let (run_tx, _run_rx) = mpsc::channel();

        let error = handle_messages(&mut app, &rx, &run_tx).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "worker panicked");

        // background側の全Senderが消えた場合
        let (tx, rx) = mpsc::channel::<Message>();
        drop(tx);

        let (run_tx, _run_rx) = mpsc::channel();

        let error = handle_messages(&mut app, &rx, &run_tx).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "background message channel disconnected");
    }

    #[test]
    fn key_and_source_message_processing_coexist() {
        let mut app = app_with_problems(&[3, 2]);

        assert!(handle_key_event(
            &mut app,
            key(KeyCode::Down, KeyEventKind::Press)
        ));
        assert_eq!(app.selected_case(), 1);

        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        tx.send(Message::SourceChanged {
            problem: 1,
            path: PathBuf::from("B.py"),
            language: Language::Python,
        })
        .unwrap();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        assert_eq!(app.current_problem().unwrap().index, "B");
        assert_eq!(app.selected_case(), 0);

        // source変更からworkerへのRunRequestも作られている
        let request = run_rx.try_recv().unwrap();

        assert_eq!(request.run_id, 1);
        assert_eq!(request.problem, 1);
        assert_eq!(request.language, Language::Python);
        assert!(!request.debug);

        // Message処理後もkeyboard操作できる
        assert!(handle_key_event(
            &mut app,
            key(KeyCode::Down, KeyEventKind::Press)
        ));
        assert_eq!(app.selected_case(), 1);
    }

    #[test]
    fn stale_run_messages_in_the_same_drain_cannot_overwrite_a_newer_request() {
        let mut app = app();
        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.cpp"),
            language: Language::Cpp,
        })
        .unwrap();
        tx.send(Message::RunStarted {
            run_id: 1,
            problem: 0,
        })
        .unwrap();
        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.py"),
            language: Language::Python,
        })
        .unwrap();
        tx.send(Message::RunEvent {
            run_id: 1,
            problem: 0,
            event: message::TestEvent::TestRunFinished {
                accepted: 3,
                total_cases: 3,
            },
        })
        .unwrap();
        tx.send(Message::RunCompleted {
            run_id: 1,
            problem: 0,
        })
        .unwrap();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        let requests: Vec<_> = run_rx.try_iter().collect();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].run_id, 1);
        assert_eq!(requests[1].run_id, 2);
        assert_eq!(requests[1].language, Language::Python);

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.id, Some(2));
        assert_eq!(run.phase, app::RunPhase::Queued);
        assert_eq!(run.language, Some(Language::Python));
        assert_eq!(run.accepted, 0);
    }
    #[test]
    #[test]
    fn samples_pane_toggles_only_on_key_press() {
        let mut app = app();

        assert!(!app.samples_pane_enabled());

        assert!(handle_key_event(
            &mut app,
            key(KeyCode::Char('s'), KeyEventKind::Press),
        ));
        assert!(app.samples_pane_enabled());

        assert!(!handle_key_event(
            &mut app,
            key(KeyCode::Char('s'), KeyEventKind::Repeat),
        ));
        assert!(app.samples_pane_enabled());

        assert!(handle_key_event(
            &mut app,
            key(KeyCode::Char('s'), KeyEventKind::Press),
        ));
        assert!(!app.samples_pane_enabled());
    }
    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }
    #[test]
    fn mouse_wheel_over_samples_changes_sample() {
        let mut app = app();

        app.scroll_detail_down(10);

        let info = view::RenderInfo {
            max_detail_scroll: 20,
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
        };

        assert!(handle_mouse_event(
            &mut app,
            mouse(MouseEventKind::ScrollDown, 5, 5),
            &info,
        ));

        assert_eq!(app.selected_case(), 1);
        assert_eq!(app.detail_scroll(), 0);
    }
    #[test]
    fn mouse_wheel_over_detail_scrolls_detail() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: 20,
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
        };

        assert!(handle_mouse_event(
            &mut app,
            mouse(MouseEventKind::ScrollDown, 30, 5),
            &info,
        ));

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 3);
    }
    #[test]
    fn mouse_wheel_at_detail_bottom_is_not_dirty() {
        let mut app = app();

        app.scroll_detail_down(10);

        let info = view::RenderInfo {
            max_detail_scroll: 10,
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 60, 10),
        };

        assert!(!handle_mouse_event(
            &mut app,
            mouse(MouseEventKind::ScrollDown, 30, 5),
            &info,
        ));

        assert_eq!(app.detail_scroll(), 10);
    }
    #[test]
    fn mouse_wheel_outside_content_is_ignored() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: 20,
            samples_area: Some(ratatui::layout::Rect::new(0, 5, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 5, 40, 10),
        };

        assert!(!handle_mouse_event(
            &mut app,
            mouse(MouseEventKind::ScrollDown, 5, 1),
            &info,
        ));

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 0);
    }
}
