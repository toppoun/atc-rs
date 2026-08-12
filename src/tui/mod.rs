pub mod app;
pub mod view;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;

use crate::model::Contest;
use app::WatchApp;

pub fn run(
    terminal: &mut DefaultTerminal,
    contest: &Contest,
    sample_counts: Vec<usize>,
) -> io::Result<()> {
    let mut app = WatchApp::new(contest, sample_counts);

    while !app.should_quit {
        terminal.draw(|frame| {
            view::render(frame, &app);
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') => app.quit(),

                KeyCode::Char('d') => app.toggle_debug(),

                KeyCode::Char('h') | KeyCode::Left => {
                    app.previous_problem();
                }

                KeyCode::Char('l') | KeyCode::Right => {
                    app.next_problem();
                }

                KeyCode::Char('k') | KeyCode::Up => {
                    app.next_case();
                }

                KeyCode::Char('j') | KeyCode::Down => {
                    app.previous_case();
                }

                _ => {}
            }
        }
    }

    Ok(())
}
