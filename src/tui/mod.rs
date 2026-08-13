pub mod app;
mod detail;
mod detail_layout;
pub mod message;
pub mod reporter;
pub mod view;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseEvent, MouseEventKind};
use message::{Message, RunRequest};
use ratatui::DefaultTerminal;

use crate::model::Contest;
use app::WatchApp;

const MAX_MESSAGES_PER_TICK: usize = 256;
const MAX_TERMINAL_EVENTS_PER_TICK: usize = 256;
const DETAIL_SCROLL_LINES: usize = 3;
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(20);

fn send_run_request(run_tx: &Sender<RunRequest>, request: RunRequest) -> io::Result<()> {
    run_tx.send(request).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test worker request channel disconnected",
        )
    })
}

fn queue_problem_run(
    app: &mut WatchApp,
    problem: usize,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    let Some(request) = app.queue_run(problem) else {
        return Ok(false);
    };

    send_run_request(run_tx, request)?;

    Ok(true)
}

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
                    queue_problem_run(app, problem, run_tx)?;
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
    let mut detail_layout = detail_layout::DetailLayout::default();
    let mut terminal_events = VecDeque::new();

    while !app.should_quit() {
        if terminal_events.is_empty() {
            terminal_events = read_terminal_events(Duration::ZERO)?;
        }

        if contains_quit_event(&terminal_events) {
            app.quit();
            break;
        }

        if take_leading_resizes(&mut terminal_events) {
            dirty = true;
        }

        if handle_messages(&mut app, &message_rx, &run_tx)? {
            dirty = true;
        }

        // message batch処理中にqが到着していれば、重いwrap/再描画より優先する。
        if terminal_events.is_empty() {
            terminal_events = read_terminal_events(Duration::ZERO)?;
        }

        if contains_quit_event(&terminal_events) {
            app.quit();
            break;
        }

        if take_leading_resizes(&mut terminal_events) {
            dirty = true;
        }

        if dirty {
            let mut next_render_info = view::RenderInfo::default();

            terminal.draw(|frame| {
                next_render_info = view::render(frame, &app, &mut detail_layout);
            })?;

            render_info = next_render_info;

            if let Some(max_detail_scroll) = render_info.max_detail_scroll {
                app.clamp_detail_scroll(max_detail_scroll);
            }

            dirty = false;
        }

        if terminal_events.is_empty() {
            terminal_events = read_terminal_events(TERMINAL_POLL_INTERVAL)?;
        }

        // qは同じbatch内のresize/mouseより先に扱い、再描画を挟まず終了する。
        if contains_quit_event(&terminal_events) {
            app.quit();
            continue;
        }

        if handle_terminal_events(&mut app, &render_info, &mut terminal_events, &run_tx)? {
            dirty = true;
        }
    }

    Ok(())
}

fn read_terminal_events(wait: Duration) -> io::Result<VecDeque<Event>> {
    read_terminal_events_with(wait, event::poll, event::read)
}

fn read_terminal_events_with(
    wait: Duration,
    mut poll_event: impl FnMut(Duration) -> io::Result<bool>,
    mut read_event: impl FnMut() -> io::Result<Event>,
) -> io::Result<VecDeque<Event>> {
    let mut events = VecDeque::new();

    if !poll_event(wait)? {
        return Ok(events);
    }

    for index in 0..MAX_TERMINAL_EVENTS_PER_TICK {
        let terminal_event = read_event()?;
        let should_quit = is_quit_event(&terminal_event);
        events.push_back(terminal_event);

        if should_quit || index + 1 == MAX_TERMINAL_EVENTS_PER_TICK || !poll_event(Duration::ZERO)?
        {
            break;
        }
    }

    Ok(events)
}

fn contains_quit_event(events: &VecDeque<Event>) -> bool {
    events.iter().any(is_quit_event)
}

fn is_quit_event(terminal_event: &Event) -> bool {
    matches!(
        terminal_event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            kind: KeyEventKind::Press,
            ..
        })
    )
}

fn take_leading_resizes(events: &mut VecDeque<Event>) -> bool {
    let mut found = false;

    while matches!(events.front(), Some(Event::Resize(_, _))) {
        events.pop_front();
        found = true;
    }

    found
}

fn handle_terminal_events(
    app: &mut WatchApp,
    render_info: &view::RenderInfo,
    events: &mut VecDeque<Event>,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    let mut changed = false;

    while let Some(terminal_event) = events.pop_front() {
        if matches!(terminal_event, Event::Resize(_, _)) {
            changed = true;

            // 連続resizeは1回の再描画へまとめる。後続mouseは新しいRectが
            // 描画されてから処理し、古いRenderInfoでhit testしない。
            while matches!(events.front(), Some(Event::Resize(_, _))) {
                events.pop_front();
            }
            break;
        }

        changed |= handle_terminal_event(app, terminal_event, render_info, run_tx)?;
    }

    Ok(changed)
}

fn handle_terminal_event(
    app: &mut WatchApp,
    terminal_event: Event,
    render_info: &view::RenderInfo,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    match terminal_event {
        Event::Key(key) => handle_key_event(app, key, run_tx),

        Event::Mouse(mouse) => Ok(handle_mouse_event(app, mouse, render_info)),

        Event::Resize(_, _) => Ok(true),

        _ => Ok(false),
    }
}

fn handle_key_event(
    app: &mut WatchApp,
    key: KeyEvent,
    run_tx: &Sender<RunRequest>,
) -> io::Result<bool> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') if key.kind == KeyEventKind::Press => {
            app.quit();
            Ok(true)
        }

        KeyCode::Char('d') if key.kind == KeyEventKind::Press => {
            app.toggle_debug();

            if app.current_source_language() == Some(crate::language::Language::Cpp)
                && let Some(problem) = app.selected_problem()
            {
                queue_problem_run(app, problem, run_tx)?;
            }

            Ok(true)
        }

        KeyCode::Char('r') if key.kind == KeyEventKind::Press => {
            let Some(problem) = app.selected_problem() else {
                return Ok(false);
            };

            queue_problem_run(app, problem, run_tx)
        }

        KeyCode::Char('s') if key.kind == KeyEventKind::Press => {
            app.toggle_samples_pane();
            Ok(true)
        }

        KeyCode::Char('h') | KeyCode::Left => Ok(app.previous_problem()),

        KeyCode::Char('l') | KeyCode::Right => Ok(app.next_problem()),

        KeyCode::Char('j') | KeyCode::Down => Ok(app.next_case()),

        KeyCode::Char('k') | KeyCode::Up => Ok(app.previous_case()),

        _ => Ok(false),
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

                if let Some(max_detail_scroll) = render_info.max_detail_scroll {
                    app.clamp_detail_scroll(max_detail_scroll);
                }

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
    use std::cell::RefCell;
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

    fn handle_key(app: &mut WatchApp, code: KeyCode, kind: KeyEventKind) -> bool {
        let (run_tx, _run_rx) = mpsc::channel();

        handle_key_event(app, key(code, kind), &run_tx).unwrap()
    }

    #[test]
    fn press_and_repeat_are_handled_but_release_is_ignored() {
        let mut app = app();

        handle_key(&mut app, KeyCode::Char('j'), KeyEventKind::Press);
        assert_eq!(app.selected_case(), 1);

        handle_key(&mut app, KeyCode::Down, KeyEventKind::Repeat);
        assert_eq!(app.selected_case(), 2);

        handle_key(&mut app, KeyCode::Char('q'), KeyEventKind::Release);
        assert!(!app.should_quit());
        handle_key(&mut app, KeyCode::Char('q'), KeyEventKind::Press);
        assert!(app.should_quit());
    }

    #[test]
    fn queued_quit_is_processed_before_later_events_without_intermediate_draws() {
        let mut app = app();
        let events = RefCell::new(VecDeque::from([
            Event::Resize(80, 24),
            Event::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
            Event::Mouse(mouse(MouseEventKind::ScrollDown, 5, 5)),
        ]));
        let poll_waits = RefCell::new(Vec::new());

        let queued = read_terminal_events_with(
            TERMINAL_POLL_INTERVAL,
            |wait| {
                poll_waits.borrow_mut().push(wait);
                Ok(!events.borrow().is_empty())
            },
            || {
                events
                    .borrow_mut()
                    .pop_front()
                    .ok_or_else(|| io::Error::other("test event queue is empty"))
            },
        )
        .unwrap();

        assert!(contains_quit_event(&queued));
        app.quit();
        assert!(app.should_quit());
        assert_eq!(app.selected_case(), 0);
        assert_eq!(events.borrow().len(), 1);
        assert_eq!(
            poll_waits.into_inner(),
            [TERMINAL_POLL_INTERVAL, Duration::ZERO]
        );
    }

    #[test]
    fn terminal_event_drain_is_bounded() {
        let events = RefCell::new(VecDeque::from(vec![
            Event::Resize(80, 24);
            MAX_TERMINAL_EVENTS_PER_TICK + 1
        ]));

        let queued = read_terminal_events_with(
            Duration::ZERO,
            |_| Ok(!events.borrow().is_empty()),
            || {
                events
                    .borrow_mut()
                    .pop_front()
                    .ok_or_else(|| io::Error::other("test event queue is empty"))
            },
        )
        .unwrap();

        assert_eq!(queued.len(), MAX_TERMINAL_EVENTS_PER_TICK);
        assert_eq!(events.borrow().len(), 1);
    }

    #[test]
    fn mouse_after_resize_waits_for_new_render_info() {
        let mut app = app();
        let (run_tx, _run_rx) = mpsc::channel();

        let old_info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
        };

        let mut events = VecDeque::from([
            Event::Resize(100, 40),
            Event::Mouse(mouse(MouseEventKind::ScrollDown, 5, 5)),
        ]);

        assert!(handle_terminal_events(&mut app, &old_info, &mut events, &run_tx,).unwrap());

        assert_eq!(app.selected_case(), 0);
        assert_eq!(events.len(), 1);

        let new_info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 100, 40),
        };

        assert!(handle_terminal_events(&mut app, &new_info, &mut events, &run_tx,).unwrap());

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 3);
    }

    #[test]
    fn repeat_does_not_toggle_debug_repeatedly() {
        let mut app = app();

        handle_key(&mut app, KeyCode::Char('d'), KeyEventKind::Press);
        assert!(app.debug_enabled());
        handle_key(&mut app, KeyCode::Char('d'), KeyEventKind::Repeat);
        assert!(app.debug_enabled());
    }

    #[test]
    fn up_and_k_move_to_the_previous_case() {
        let mut app = app();

        handle_key(&mut app, KeyCode::Up, KeyEventKind::Press);
        assert_eq!(app.selected_case(), 2);
        handle_key(&mut app, KeyCode::Char('k'), KeyEventKind::Press);
        assert_eq!(app.selected_case(), 1);
    }

    #[test]
    fn unknown_and_no_op_navigation_keys_are_not_dirty() {
        let mut app = app_with_problems(&[1]);

        assert!(!handle_key(
            &mut app,
            KeyCode::Char('x'),
            KeyEventKind::Press
        ));
        assert!(!handle_key(&mut app, KeyCode::Right, KeyEventKind::Press));
        assert!(!handle_key(&mut app, KeyCode::Down, KeyEventKind::Press));
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

        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();
        assert!(
            handle_key_event(&mut app, key(KeyCode::Down, KeyEventKind::Press), &run_tx,).unwrap()
        );
        assert_eq!(app.selected_case(), 1);

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
        assert!(
            handle_key_event(&mut app, key(KeyCode::Down, KeyEventKind::Press), &run_tx,).unwrap()
        );
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
    fn samples_pane_toggles_only_on_key_press() {
        let mut app = app();

        assert!(!app.samples_pane_enabled());

        assert!(handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyEventKind::Press
        ),);
        assert!(app.samples_pane_enabled());

        assert!(!handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyEventKind::Repeat
        ),);
        assert!(app.samples_pane_enabled());

        assert!(handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyEventKind::Press
        ),);
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
            max_detail_scroll: Some(20),
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
            max_detail_scroll: Some(20),
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
    fn mouse_wheel_does_not_clamp_while_lazy_max_is_unknown() {
        let mut app = app();
        let info = view::RenderInfo {
            max_detail_scroll: None,
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 60, 10),
        };

        assert!(handle_mouse_event(
            &mut app,
            mouse(MouseEventKind::ScrollDown, 30, 5),
            &info,
        ));
        assert_eq!(app.detail_scroll(), DETAIL_SCROLL_LINES);
    }
    #[test]
    fn mouse_wheel_at_detail_bottom_is_not_dirty() {
        let mut app = app();

        app.scroll_detail_down(10);

        let info = view::RenderInfo {
            max_detail_scroll: Some(10),
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
    fn mouse_wheel_at_detail_top_is_not_dirty() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: Some(10),
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 60, 10),
        };

        assert!(!handle_mouse_event(
            &mut app,
            mouse(MouseEventKind::ScrollUp, 30, 5),
            &info,
        ));

        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn samples_and_detail_rect_boundary_is_half_open() {
        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
        };

        let mut samples_app = app();
        assert!(handle_mouse_event(
            &mut samples_app,
            mouse(MouseEventKind::ScrollDown, 19, 5),
            &info,
        ));
        assert_eq!(samples_app.selected_case(), 1);
        assert_eq!(samples_app.detail_scroll(), 0);

        let mut detail_app = app();
        assert!(handle_mouse_event(
            &mut detail_app,
            mouse(MouseEventKind::ScrollDown, 20, 5),
            &info,
        ));
        assert_eq!(detail_app.selected_case(), 0);
        assert_eq!(detail_app.detail_scroll(), 3);
    }

    #[test]
    fn samples_wheel_with_one_case_does_not_mark_ui_dirty() {
        let mut app = app_with_problems(&[1]);
        let info = view::RenderInfo {
            max_detail_scroll: Some(0),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
        };

        assert!(!handle_mouse_event(
            &mut app,
            mouse(MouseEventKind::ScrollDown, 5, 5),
            &info,
        ));
        assert_eq!(app.selected_case(), 0);
    }
    #[test]
    fn mouse_wheel_outside_content_is_ignored() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
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
    #[test]
    fn rerun_key_queues_current_source() {
        let mut app = app();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        let request = run_rx.try_recv().unwrap();

        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(!request.debug);
    }

    #[test]
    fn rerun_uses_only_the_selected_problems_confirmed_source() {
        let mut app = app_with_problems(&[1, 1]);
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        app.source_changed(1, PathBuf::from("B.py"), Language::Python);
        assert!(app.select_problem(0));

        let (run_tx, run_rx) = mpsc::channel();
        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        let request = run_rx.try_recv().unwrap();
        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
    }

    #[test]
    fn rerun_key_without_source_is_no_op() {
        let mut app = app();
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            !handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn debug_and_rerun_repeat_do_not_change_state_or_queue_requests() {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );
        let first = run_rx.try_recv().unwrap();
        assert!(first.debug);

        assert!(
            !handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Repeat),
                &run_tx,
            )
            .unwrap()
        );
        assert!(
            !handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Repeat),
                &run_tx,
            )
            .unwrap()
        );

        assert!(app.debug_enabled());
        assert_eq!(app.current_problem().unwrap().run.id, Some(first.run_id));
        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn rerun_request_channel_disconnect_is_a_fatal_error() {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let (run_tx, run_rx) = mpsc::channel();
        drop(run_rx);
        let mut events = VecDeque::from([Event::Key(key(KeyCode::Char('r'), KeyEventKind::Press))]);

        let error =
            handle_terminal_events(&mut app, &view::RenderInfo::default(), &mut events, &run_tx)
                .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            error.to_string(),
            "test worker request channel disconnected"
        );
    }

    #[test]
    fn debug_toggle_reruns_cpp_with_new_debug_state() {
        let mut app = app();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(app.debug_enabled());

        let request = run_rx.try_recv().unwrap();

        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(request.debug);

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(!app.debug_enabled());

        let request = run_rx.try_recv().unwrap();
        assert!(!request.debug);
    }
    #[test]
    fn debug_toggle_does_not_rerun_python() {
        let mut app = app();

        app.source_changed(0, PathBuf::from("A.py"), Language::Python);

        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(app.debug_enabled());
        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn save_debug_rerun_and_save_keep_monotonic_run_ids_and_latest_state() {
        let mut app = app();
        let (message_tx, message_rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        message_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("A.cpp"),
                language: Language::Cpp,
            })
            .unwrap();
        assert!(handle_messages(&mut app, &message_rx, &run_tx).unwrap());

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );
        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        message_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("A.py"),
                language: Language::Python,
            })
            .unwrap();
        assert!(handle_messages(&mut app, &message_rx, &run_tx).unwrap());

        let requests: Vec<_> = run_rx.try_iter().collect();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.run_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            requests
                .iter()
                .map(|request| (request.language, request.debug))
                .collect::<Vec<_>>(),
            [
                (Language::Cpp, false),
                (Language::Cpp, true),
                (Language::Cpp, true),
                (Language::Python, false),
            ]
        );

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.id, Some(4));
        assert_eq!(run.phase, app::RunPhase::Queued);
        assert_eq!(run.language, Some(Language::Python));
        assert!(app.debug_enabled());

        assert!(!app.run_failed(0, 3, "stale failure".to_string()));
        assert_eq!(app.current_problem().unwrap().run.id, Some(4));
        assert_eq!(
            app.current_problem().unwrap().run.phase,
            app::RunPhase::Queued
        );
    }
}
