use ratatui::{
    Frame,
    widgets::{Block, Borders, Paragraph},
};

use super::app::WatchApp;

pub fn render(frame: &mut Frame, app: &WatchApp) {
    let current_problem = app.problems.get(app.selected_problem);

    let problem = current_problem
        .map(|problem| problem.index.as_str())
        .unwrap_or("-");

    let title = current_problem
        .map(|problem| problem.title.as_str())
        .unwrap_or("-");

    let total_cases = current_problem
        .map(|problem| problem.total_cases)
        .unwrap_or(0);

    let sample = if total_cases == 0 {
        "-".to_string()
    } else {
        format!("{} / {}", app.selected_case + 1, total_cases)
    };

    let debug = if app.debug { "ON" } else { "OFF" };

    let text = format!(
        "Contest: {}\n\
     Problem: {} - {}\n\
     Sample:  {}\n\
     Debug:   {}\n\n\
     h/l or ←/→ : problem\n\
     j/k or ↓/↑ : sample\n\
     d           : debug\n\
     q           : quit",
        app.contest_id, problem, title, sample, debug,
    );

    let block = Block::default().title(" atc watch ").borders(Borders::ALL);

    let paragraph = Paragraph::new(text).block(block);

    frame.render_widget(paragraph, frame.area());
}
