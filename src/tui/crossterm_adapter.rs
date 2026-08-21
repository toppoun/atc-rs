use std::io;
use std::time::Duration;

use crossterm::event::{self, Event};

use super::terminal::{
    KeyCode, KeyEvent, KeyEventKind, Modifiers, PointerButton, PointerEvent, PointerKind,
    PointerPosition, TerminalEvent, TerminalSize,
};

pub(super) fn poll(wait: Duration) -> io::Result<bool> {
    event::poll(wait)
}

pub(super) fn read() -> io::Result<TerminalEvent> {
    event::read().map(translate)
}

fn translate(event: Event) -> TerminalEvent {
    match event {
        Event::Key(event) => {
            translate_key(event).map_or(TerminalEvent::Ignored, TerminalEvent::Key)
        }
        Event::Mouse(event) => TerminalEvent::Pointer(PointerEvent {
            kind: translate_pointer_kind(event.kind),
            position: PointerPosition::Cells {
                column: event.column,
                row: event.row,
            },
            modifiers: translate_modifiers(event.modifiers),
        }),
        Event::Resize(columns, rows) => TerminalEvent::Resize(TerminalSize { columns, rows }),
        Event::FocusGained | Event::FocusLost | Event::Paste(_) => TerminalEvent::Ignored,
    }
}

fn translate_key(event: event::KeyEvent) -> Option<KeyEvent> {
    Some(KeyEvent {
        code: match event.code {
            event::KeyCode::Char(character) => KeyCode::Char(character),
            event::KeyCode::Left => KeyCode::Left,
            event::KeyCode::Right => KeyCode::Right,
            event::KeyCode::Up => KeyCode::Up,
            event::KeyCode::Down => KeyCode::Down,
            _ => return None,
        },
        kind: match event.kind {
            event::KeyEventKind::Press => KeyEventKind::Press,
            event::KeyEventKind::Repeat => KeyEventKind::Repeat,
            event::KeyEventKind::Release => KeyEventKind::Release,
        },
        modifiers: translate_modifiers(event.modifiers),
    })
}

fn translate_pointer_kind(kind: event::MouseEventKind) -> PointerKind {
    match kind {
        event::MouseEventKind::Down(button) => PointerKind::Down(translate_pointer_button(button)),
        event::MouseEventKind::Up(button) => PointerKind::Up(translate_pointer_button(button)),
        event::MouseEventKind::Drag(button) => PointerKind::Drag(translate_pointer_button(button)),
        event::MouseEventKind::Moved => PointerKind::Move,
        event::MouseEventKind::ScrollUp => PointerKind::ScrollUp,
        event::MouseEventKind::ScrollDown => PointerKind::ScrollDown,
        event::MouseEventKind::ScrollLeft => PointerKind::ScrollLeft,
        event::MouseEventKind::ScrollRight => PointerKind::ScrollRight,
    }
}

fn translate_pointer_button(button: event::MouseButton) -> PointerButton {
    match button {
        event::MouseButton::Left => PointerButton::Left,
        event::MouseButton::Right => PointerButton::Right,
        event::MouseButton::Middle => PointerButton::Middle,
    }
}

fn translate_modifiers(modifiers: event::KeyModifiers) -> Modifiers {
    Modifiers {
        shift: modifiers.contains(event::KeyModifiers::SHIFT),
        control: modifiers.contains(event::KeyModifiers::CONTROL),
        alt: modifiers.contains(event::KeyModifiers::ALT),
        super_key: modifiers.contains(event::KeyModifiers::SUPER),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
        KeyEventKind as CrosstermKeyEventKind, KeyModifiers, MouseButton as CrosstermMouseButton,
        MouseEvent as CrosstermMouseEvent, MouseEventKind as CrosstermMouseEventKind,
    };

    use super::*;

    fn key(code: CrosstermKeyCode, kind: CrosstermKeyEventKind, modifiers: KeyModifiers) -> Event {
        Event::Key(CrosstermKeyEvent::new_with_kind(code, modifiers, kind))
    }

    fn pointer(kind: CrosstermMouseEventKind, modifiers: KeyModifiers) -> Event {
        Event::Mouse(CrosstermMouseEvent {
            kind,
            column: 17,
            row: 9,
            modifiers,
        })
    }

    #[test]
    fn translates_q_without_normalizing_its_character() {
        assert_eq!(
            translate(key(
                CrosstermKeyCode::Char('q'),
                CrosstermKeyEventKind::Press,
                KeyModifiers::NONE,
            )),
            TerminalEvent::Key(KeyEvent {
                code: KeyCode::Char('q'),
                kind: KeyEventKind::Press,
                modifiers: Modifiers::default(),
            })
        );
    }

    #[test]
    fn preserves_uppercase_s_and_modifiers() {
        assert_eq!(
            translate(key(
                CrosstermKeyCode::Char('S'),
                CrosstermKeyEventKind::Press,
                KeyModifiers::SHIFT
                    | KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER,
            )),
            TerminalEvent::Key(KeyEvent {
                code: KeyCode::Char('S'),
                kind: KeyEventKind::Press,
                modifiers: Modifiers {
                    shift: true,
                    control: true,
                    alt: true,
                    super_key: true,
                },
            })
        );
    }

    #[test]
    fn translates_arrow_keys() {
        for (source, expected) in [
            (CrosstermKeyCode::Left, KeyCode::Left),
            (CrosstermKeyCode::Right, KeyCode::Right),
            (CrosstermKeyCode::Up, KeyCode::Up),
            (CrosstermKeyCode::Down, KeyCode::Down),
        ] {
            assert_eq!(
                translate(key(
                    source,
                    CrosstermKeyEventKind::Press,
                    KeyModifiers::NONE,
                )),
                TerminalEvent::Key(KeyEvent {
                    code: expected,
                    kind: KeyEventKind::Press,
                    modifiers: Modifiers::default(),
                })
            );
        }
    }

    #[test]
    fn translates_press_repeat_and_release() {
        for (source, expected) in [
            (CrosstermKeyEventKind::Press, KeyEventKind::Press),
            (CrosstermKeyEventKind::Repeat, KeyEventKind::Repeat),
            (CrosstermKeyEventKind::Release, KeyEventKind::Release),
        ] {
            let TerminalEvent::Key(event) =
                translate(key(CrosstermKeyCode::Char('j'), source, KeyModifiers::NONE))
            else {
                panic!("supported key must translate to a key event");
            };
            assert_eq!(event.kind, expected);
        }
    }

    #[test]
    fn translates_resize_cell_dimensions() {
        assert_eq!(
            translate(Event::Resize(120, 42)),
            TerminalEvent::Resize(TerminalSize {
                columns: 120,
                rows: 42,
            })
        );
    }

    #[test]
    fn translates_button_down_up_and_drag_at_cell_coordinates() {
        for (source, expected) in [
            (
                CrosstermMouseEventKind::Down(CrosstermMouseButton::Left),
                PointerKind::Down(PointerButton::Left),
            ),
            (
                CrosstermMouseEventKind::Up(CrosstermMouseButton::Left),
                PointerKind::Up(PointerButton::Left),
            ),
            (
                CrosstermMouseEventKind::Drag(CrosstermMouseButton::Left),
                PointerKind::Drag(PointerButton::Left),
            ),
            (
                CrosstermMouseEventKind::Down(CrosstermMouseButton::Right),
                PointerKind::Down(PointerButton::Right),
            ),
            (
                CrosstermMouseEventKind::Up(CrosstermMouseButton::Middle),
                PointerKind::Up(PointerButton::Middle),
            ),
        ] {
            let TerminalEvent::Pointer(event) = translate(pointer(source, KeyModifiers::CONTROL))
            else {
                panic!("mouse event must translate to a pointer event");
            };
            assert_eq!(event.kind, expected);
            assert_eq!(
                event.position,
                PointerPosition::Cells { column: 17, row: 9 }
            );
            assert!(event.modifiers.control);
        }
    }

    #[test]
    fn translates_vertical_and_horizontal_wheel() {
        for (source, expected) in [
            (CrosstermMouseEventKind::ScrollUp, PointerKind::ScrollUp),
            (CrosstermMouseEventKind::ScrollDown, PointerKind::ScrollDown),
            (CrosstermMouseEventKind::ScrollLeft, PointerKind::ScrollLeft),
            (
                CrosstermMouseEventKind::ScrollRight,
                PointerKind::ScrollRight,
            ),
        ] {
            let TerminalEvent::Pointer(event) = translate(pointer(source, KeyModifiers::NONE))
            else {
                panic!("mouse event must translate to a pointer event");
            };
            assert_eq!(event.kind, expected);
        }
    }

    #[test]
    fn translates_pointer_move_without_adding_behavior() {
        let TerminalEvent::Pointer(event) =
            translate(pointer(CrosstermMouseEventKind::Moved, KeyModifiers::NONE))
        else {
            panic!("mouse movement must translate to a pointer event");
        };
        assert_eq!(event.kind, PointerKind::Move);
    }

    #[test]
    fn intentionally_ignores_unused_events_and_unsupported_keys() {
        assert_eq!(translate(Event::FocusGained), TerminalEvent::Ignored);
        assert_eq!(translate(Event::FocusLost), TerminalEvent::Ignored);
        assert_eq!(
            translate(Event::Paste("pasted".to_string())),
            TerminalEvent::Ignored
        );
        assert_eq!(
            translate(key(
                CrosstermKeyCode::Enter,
                CrosstermKeyEventKind::Press,
                KeyModifiers::NONE,
            )),
            TerminalEvent::Ignored
        );
    }
}
