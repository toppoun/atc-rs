#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalEvent {
    Key(KeyEvent),
    Pointer(PointerEvent),
    Resize(TerminalSize),
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TerminalSize {
    pub(super) columns: u16,
    pub(super) rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct KeyEvent {
    pub(super) code: KeyCode,
    pub(super) kind: KeyEventKind,
    pub(super) modifiers: Modifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyCode {
    Char(char),
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Modifiers {
    pub(super) shift: bool,
    pub(super) control: bool,
    pub(super) alt: bool,
    pub(super) super_key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PointerEvent {
    pub(super) kind: PointerKind,
    pub(super) position: PointerPosition,
    pub(super) modifiers: Modifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerKind {
    Down(PointerButton),
    Up(PointerButton),
    Drag(PointerButton),
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerPosition {
    Cells {
        column: u16,
        row: u16,
    },
    #[allow(dead_code, reason = "reserved for the later pixel-input adapter")]
    AbsolutePixels {
        x: u32,
        y: u32,
    },
}

impl PointerPosition {
    pub(super) fn cells(self) -> Option<(u16, u16)> {
        match self {
            Self::Cells { column, row } => Some((column, row)),
            Self::AbsolutePixels { .. } => None,
        }
    }
}
