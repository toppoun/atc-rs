//! Interactive developer check for the native picker and terminal lifecycle.

use std::io;

use ratatui::widgets::Paragraph;

use super::super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminaSession, TerminalEvent};
use super::{FolderSelection, FolderSelectionRequest, NativeFolderPicker, select_folder};

pub(crate) fn run() -> io::Result<()> {
    let initial_dir = std::env::current_dir()?;
    let mut terminal = TerminaSession::start()?;
    let mut picker = NativeFolderPicker;
    let mut status = String::from("Ready");
    loop {
        terminal.draw(|frame| {
            frame.render_widget(
                Paragraph::new(format!(
                    "Folder picker probe\nEnter: open dialog   q / Esc: quit\n\n{status}"
                )),
                frame.area(),
            );
        })?;
        terminal.note_redraw_completed();
        terminal.refresh_mouse_after_redraw(false)?;
        terminal.retry_high_res_after_redraw(false)?;
        match terminal.read()? {
            TerminalEvent::Key(KeyEvent {
                code: KeyCode::Enter,
                kind: KeyEventKind::Press,
                ..
            }) => {
                // This loop dispatches one event at a time. The next iteration redraws before reading.
                let selection = select_folder(
                    &mut terminal,
                    FolderSelectionRequest::SystemDialog {
                        picker: &mut picker,
                        initial_dir: Some(&initial_dir),
                    },
                )?;
                status = match selection {
                    FolderSelection::Selected(path) => format!("Selected: {:?}", path.as_os_str()),
                    FolderSelection::Cancelled => "Cancelled".into(),
                    FolderSelection::OpenExplorer(reason) => {
                        format!("Explorer fallback: {reason:?}")
                    }
                    FolderSelection::Failed(error) => error.to_string(),
                };
            }
            TerminalEvent::Key(KeyEvent {
                code: KeyCode::Char('q') | KeyCode::Escape,
                kind: KeyEventKind::Press,
                ..
            }) => {
                return terminal.restore();
            }
            TerminalEvent::Resize(_) => terminal.note_resize_dispatched(),
            _ => {}
        }
    }
}
