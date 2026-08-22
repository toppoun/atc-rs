use termina::Event;
use termina::escape::csi::{Csi, MouseButton as CsiMouseButton, MouseReport};
use termina::event::{self, MouseEventKind};

use super::terminal::{
    KeyCode, KeyEvent, KeyEventKind, Modifiers, PointerButton, PointerEvent, PointerKind,
    PointerPosition, TerminalEvent, TerminalSize,
};

pub(super) fn translate(event: Event) -> TerminalEvent {
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
            pixel_generation: None,
        }),
        Event::WindowResized(size) => TerminalEvent::Resize(TerminalSize {
            columns: size.cols,
            rows: size.rows,
        }),
        Event::Csi(Csi::Mouse(MouseReport::Sgr1016 {
            x_pixels,
            y_pixels,
            button,
            modifiers,
        })) => TerminalEvent::Pointer(PointerEvent {
            kind: translate_sgr1016_kind(button),
            position: PointerPosition::AbsolutePixels {
                x: x_pixels,
                y: y_pixels,
            },
            modifiers: translate_modifiers(modifiers),
            pixel_generation: None,
        }),
        Event::FocusIn
        | Event::FocusOut
        | Event::Paste(_)
        | Event::Csi(_)
        | Event::Osc(_)
        | Event::Dcs(_) => TerminalEvent::Ignored,
    }
}

fn translate_sgr1016_kind(button: CsiMouseButton) -> PointerKind {
    match button {
        CsiMouseButton::Button1Press => PointerKind::Down(PointerButton::Left),
        CsiMouseButton::Button2Press => PointerKind::Down(PointerButton::Middle),
        CsiMouseButton::Button3Press => PointerKind::Down(PointerButton::Right),
        CsiMouseButton::Button1Release => PointerKind::Up(PointerButton::Left),
        CsiMouseButton::Button2Release => PointerKind::Up(PointerButton::Middle),
        CsiMouseButton::Button3Release => PointerKind::Up(PointerButton::Right),
        CsiMouseButton::Button1Drag => PointerKind::Drag(PointerButton::Left),
        CsiMouseButton::Button2Drag => PointerKind::Drag(PointerButton::Middle),
        CsiMouseButton::Button3Drag => PointerKind::Drag(PointerButton::Right),
        CsiMouseButton::Button4Press | CsiMouseButton::Button4Release => PointerKind::ScrollUp,
        CsiMouseButton::Button5Press | CsiMouseButton::Button5Release => PointerKind::ScrollDown,
        CsiMouseButton::Button6Press | CsiMouseButton::Button6Release => PointerKind::ScrollLeft,
        CsiMouseButton::Button7Press | CsiMouseButton::Button7Release => PointerKind::ScrollRight,
        CsiMouseButton::None => PointerKind::Move,
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

fn translate_pointer_kind(kind: MouseEventKind) -> PointerKind {
    match kind {
        MouseEventKind::Down(button) => PointerKind::Down(translate_pointer_button(button)),
        MouseEventKind::Up(button) => PointerKind::Up(translate_pointer_button(button)),
        MouseEventKind::Drag(button) => PointerKind::Drag(translate_pointer_button(button)),
        MouseEventKind::Moved => PointerKind::Move,
        MouseEventKind::ScrollUp => PointerKind::ScrollUp,
        MouseEventKind::ScrollDown => PointerKind::ScrollDown,
        MouseEventKind::ScrollLeft => PointerKind::ScrollLeft,
        MouseEventKind::ScrollRight => PointerKind::ScrollRight,
    }
}

fn translate_pointer_button(button: event::MouseButton) -> PointerButton {
    match button {
        event::MouseButton::Left => PointerButton::Left,
        event::MouseButton::Right => PointerButton::Right,
        event::MouseButton::Middle => PointerButton::Middle,
    }
}

fn translate_modifiers(modifiers: event::Modifiers) -> Modifiers {
    Modifiers {
        shift: modifiers.contains(event::Modifiers::SHIFT),
        control: modifiers.contains(event::Modifiers::CONTROL),
        alt: modifiers.contains(event::Modifiers::ALT),
        super_key: modifiers.contains(event::Modifiers::SUPER),
    }
}

#[cfg(test)]
mod tests {
    use termina::WindowSize;
    use termina::escape::csi::{Csi, Cursor, MouseButton as CsiMouseButton, MouseReport};
    use termina::event::{
        KeyCode as TerminaKeyCode, KeyEvent as TerminaKeyEvent,
        KeyEventKind as TerminaKeyEventKind, KeyEventState, Modifiers as TerminaModifiers,
        MouseButton as TerminaMouseButton, MouseEvent as TerminaMouseEvent,
    };

    use super::*;

    fn key(code: TerminaKeyCode, kind: TerminaKeyEventKind, modifiers: TerminaModifiers) -> Event {
        Event::Key(TerminaKeyEvent {
            code,
            kind,
            modifiers,
            state: KeyEventState::NONE,
        })
    }

    fn pointer(kind: MouseEventKind, modifiers: TerminaModifiers) -> Event {
        Event::Mouse(TerminaMouseEvent {
            kind,
            column: 17,
            row: 9,
            modifiers,
        })
    }

    fn pixel_pointer(x: u32, y: u32, button: CsiMouseButton, modifiers: TerminaModifiers) -> Event {
        Event::Csi(Csi::Mouse(MouseReport::Sgr1016 {
            x_pixels: x,
            y_pixels: y,
            button,
            modifiers,
        }))
    }

    #[test]
    fn translates_q_without_normalizing_its_character() {
        assert_eq!(
            translate(key(
                TerminaKeyCode::Char('q'),
                TerminaKeyEventKind::Press,
                TerminaModifiers::NONE,
            )),
            TerminalEvent::Key(KeyEvent {
                code: KeyCode::Char('q'),
                kind: KeyEventKind::Press,
                modifiers: Modifiers::default(),
            })
        );
    }

    #[test]
    fn preserves_q_case_kind_and_modifiers_for_application_priority() {
        for (character, source_kind, modifiers, expected_kind) in [
            (
                'q',
                TerminaKeyEventKind::Press,
                TerminaModifiers::CONTROL | TerminaModifiers::ALT,
                KeyEventKind::Press,
            ),
            (
                'Q',
                TerminaKeyEventKind::Press,
                TerminaModifiers::SHIFT,
                KeyEventKind::Press,
            ),
            (
                'q',
                TerminaKeyEventKind::Repeat,
                TerminaModifiers::NONE,
                KeyEventKind::Repeat,
            ),
            (
                'q',
                TerminaKeyEventKind::Release,
                TerminaModifiers::NONE,
                KeyEventKind::Release,
            ),
        ] {
            let TerminalEvent::Key(translated) =
                translate(key(TerminaKeyCode::Char(character), source_kind, modifiers))
            else {
                panic!("character key must translate to a key event");
            };

            assert_eq!(translated.code, KeyCode::Char(character));
            assert_eq!(translated.kind, expected_kind);
            assert_eq!(
                translated.modifiers.control,
                modifiers.contains(TerminaModifiers::CONTROL)
            );
            assert_eq!(
                translated.modifiers.alt,
                modifiers.contains(TerminaModifiers::ALT)
            );
        }
    }

    #[test]
    fn preserves_uppercase_s_unicode_and_relevant_modifiers() {
        for (character, expected_modifiers) in [
            (
                'S',
                Modifiers {
                    shift: true,
                    control: true,
                    alt: true,
                    super_key: true,
                },
            ),
            ('界', Modifiers::default()),
        ] {
            let source_modifiers = if character == 'S' {
                TerminaModifiers::SHIFT
                    | TerminaModifiers::CONTROL
                    | TerminaModifiers::ALT
                    | TerminaModifiers::SUPER
                    | TerminaModifiers::HYPER
                    | TerminaModifiers::META
            } else {
                TerminaModifiers::NONE
            };

            assert_eq!(
                translate(key(
                    TerminaKeyCode::Char(character),
                    TerminaKeyEventKind::Press,
                    source_modifiers,
                )),
                TerminalEvent::Key(KeyEvent {
                    code: KeyCode::Char(character),
                    kind: KeyEventKind::Press,
                    modifiers: expected_modifiers,
                })
            );
        }
    }

    #[test]
    fn translates_arrow_keys() {
        for (source, expected) in [
            (TerminaKeyCode::Left, KeyCode::Left),
            (TerminaKeyCode::Right, KeyCode::Right),
            (TerminaKeyCode::Up, KeyCode::Up),
            (TerminaKeyCode::Down, KeyCode::Down),
        ] {
            assert_eq!(
                translate(key(
                    source,
                    TerminaKeyEventKind::Press,
                    TerminaModifiers::NONE,
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
            (TerminaKeyEventKind::Press, KeyEventKind::Press),
            (TerminaKeyEventKind::Repeat, KeyEventKind::Repeat),
            (TerminaKeyEventKind::Release, KeyEventKind::Release),
        ] {
            let TerminalEvent::Key(event) = translate(key(
                TerminaKeyCode::Char('j'),
                source,
                TerminaModifiers::NONE,
            )) else {
                panic!("supported key must translate to a key event");
            };
            assert_eq!(event.kind, expected);
        }
    }

    #[test]
    fn translates_resize_cell_dimensions_and_ignores_pixels() {
        assert_eq!(
            translate(Event::WindowResized(WindowSize {
                cols: 120,
                rows: 42,
                pixel_width: Some(1440),
                pixel_height: Some(900),
            })),
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
                MouseEventKind::Down(TerminaMouseButton::Left),
                PointerKind::Down(PointerButton::Left),
            ),
            (
                MouseEventKind::Up(TerminaMouseButton::Left),
                PointerKind::Up(PointerButton::Left),
            ),
            (
                MouseEventKind::Drag(TerminaMouseButton::Left),
                PointerKind::Drag(PointerButton::Left),
            ),
            (
                MouseEventKind::Down(TerminaMouseButton::Right),
                PointerKind::Down(PointerButton::Right),
            ),
            (
                MouseEventKind::Up(TerminaMouseButton::Middle),
                PointerKind::Up(PointerButton::Middle),
            ),
        ] {
            let TerminalEvent::Pointer(event) =
                translate(pointer(source, TerminaModifiers::CONTROL))
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
            (MouseEventKind::ScrollUp, PointerKind::ScrollUp),
            (MouseEventKind::ScrollDown, PointerKind::ScrollDown),
            (MouseEventKind::ScrollLeft, PointerKind::ScrollLeft),
            (MouseEventKind::ScrollRight, PointerKind::ScrollRight),
        ] {
            let TerminalEvent::Pointer(event) = translate(pointer(source, TerminaModifiers::NONE))
            else {
                panic!("mouse event must translate to a pointer event");
            };
            assert_eq!(event.kind, expected);
        }
    }

    #[test]
    fn translates_pointer_move_without_adding_behavior() {
        let TerminalEvent::Pointer(event) = translate(pointer(
            MouseEventKind::Moved,
            TerminaModifiers::SHIFT | TerminaModifiers::ALT,
        )) else {
            panic!("mouse movement must translate to a pointer event");
        };
        assert_eq!(event.kind, PointerKind::Move);
        assert!(event.modifiers.shift);
        assert!(event.modifiers.alt);
    }

    #[test]
    fn translates_sgr1016_without_normalizing_or_truncating_raw_pixels() {
        for (x, y) in [(0, 0), (70_000, u32::MAX)] {
            let TerminalEvent::Pointer(event) = translate(pixel_pointer(
                x,
                y,
                CsiMouseButton::Button1Press,
                TerminaModifiers::NONE,
            )) else {
                panic!("SGR-Pixels input must translate to a pointer event");
            };
            assert_eq!(event.kind, PointerKind::Down(PointerButton::Left));
            assert_eq!(event.position, PointerPosition::AbsolutePixels { x, y });
            assert_eq!(event.pixel_generation, None);
        }
    }

    #[test]
    fn translates_sgr1016_buttons_motion_wheels_and_modifiers() {
        for (button, expected) in [
            (
                CsiMouseButton::Button1Press,
                PointerKind::Down(PointerButton::Left),
            ),
            (
                CsiMouseButton::Button2Release,
                PointerKind::Up(PointerButton::Middle),
            ),
            (
                CsiMouseButton::Button3Drag,
                PointerKind::Drag(PointerButton::Right),
            ),
            (CsiMouseButton::None, PointerKind::Move),
            (CsiMouseButton::Button4Press, PointerKind::ScrollUp),
            (CsiMouseButton::Button5Release, PointerKind::ScrollDown),
            (CsiMouseButton::Button6Press, PointerKind::ScrollLeft),
            (CsiMouseButton::Button7Release, PointerKind::ScrollRight),
        ] {
            let TerminalEvent::Pointer(event) = translate(pixel_pointer(
                123,
                456,
                button,
                TerminaModifiers::SHIFT | TerminaModifiers::CONTROL | TerminaModifiers::ALT,
            )) else {
                panic!("SGR-Pixels input must translate to a pointer event");
            };
            assert_eq!(event.kind, expected);
            assert!(event.modifiers.shift);
            assert!(event.modifiers.control);
            assert!(event.modifiers.alt);
        }
    }

    #[test]
    fn intentionally_ignores_unused_events_unsupported_keys_and_csi_responses() {
        assert_eq!(translate(Event::FocusIn), TerminalEvent::Ignored);
        assert_eq!(translate(Event::FocusOut), TerminalEvent::Ignored);
        assert_eq!(
            translate(Event::Paste("pasted".to_string())),
            TerminalEvent::Ignored
        );
        assert_eq!(
            translate(key(
                TerminaKeyCode::Enter,
                TerminaKeyEventKind::Press,
                TerminaModifiers::NONE,
            )),
            TerminalEvent::Ignored
        );
        assert_eq!(
            translate(Event::Csi(
                Csi::Cursor(Cursor::RequestActivePositionReport,)
            )),
            TerminalEvent::Ignored
        );
    }
}
