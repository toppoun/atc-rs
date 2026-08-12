pub mod app;
pub mod message;
pub mod view;
use std::sync::mpsc::{Receiver, TryRecvError};

use message::Message;
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::DefaultTerminal;

use crate::model::Contest;
use app::WatchApp;

fn handle_messages(app: &mut WatchApp, message_rx: &Receiver<Message>) -> io::Result<bool> {
    let mut changed = false;

    loop {
        match message_rx.try_recv() {
            Ok(Message::SourceChanged {
                problem,
                path,
                language,
            }) => {
                if app.source_changed(problem, path, language) {
                    changed = true;
                }
            }

            Ok(Message::WatcherFailed(error)) => {
                return Err(error);
            }

            Err(TryRecvError::Empty) => {
                break;
            }

            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "watcher thread disconnected",
                ));
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
) -> io::Result<()> {
    let mut app = WatchApp::new(contest, sample_counts)?;

    let mut dirty = true;

    while !app.should_quit() {
        if handle_messages(&mut app, &message_rx)? {
            dirty = true;
        }

        if dirty {
            terminal.draw(|frame| {
                view::render(frame, &app);
            })?;

            dirty = false;
        }

        if event::poll(Duration::from_millis(20))?
            && let Event::Key(key) = event::read()?
        {
            if handle_key_event(&mut app, key) {
                dirty = true;
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

        KeyCode::Char('h') | KeyCode::Left => {
            app.previous_problem();
            true
        }

        KeyCode::Char('l') | KeyCode::Right => {
            app.next_problem();
            true
        }

        KeyCode::Char('j') | KeyCode::Down => {
            app.next_case();
            true
        }

        KeyCode::Char('k') | KeyCode::Up => {
            app.previous_case();
            true
        }

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Contest, Problem};
    use crossterm::event::KeyModifiers;

    fn app() -> WatchApp {
        WatchApp::new(
            &Contest {
                contest_id: "abc123".to_string(),
                problems: vec![Problem {
                    index: "A".to_string(),
                    title: "Problem A".to_string(),
                    task_id: "abc123_a".to_string(),
                    url: "https://example.invalid/a".to_string(),
                }],
            },
            vec![3],
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
}
