use ratatui::{
    Frame,
    widgets::{Block, Borders, Paragraph},
};

use super::app::{RunPhase, WatchApp};

pub fn render(frame: &mut Frame, app: &WatchApp) {
    let current_problem = app.current_problem();

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
        format!("{} / {}", app.selected_case() + 1, total_cases)
    };

    let debug = if app.debug_enabled() { "ON" } else { "OFF" };

    let source = current_problem
        .and_then(|problem| problem.source.as_ref())
        .map(|source| {
            source
                .path
                .file_name()
                .unwrap_or(source.path.as_os_str())
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "-".to_string());

    let run = current_problem
        .map(run_status)
        .unwrap_or_else(|| "-".to_string());

    let text = format!(
        "Contest: {}\n\
         Problem: {} - {}\n\
         Source:  {}\n\
         Sample:  {}\n\
         Debug:   {}\n\
         Run:     {}\n\n\
         h/l or ←/→ : problem\n\
         j/k or ↓/↑ : sample\n\
         d           : debug\n\
         q           : quit",
        app.contest_id(),
        problem,
        title,
        source,
        sample,
        debug,
        run,
    );

    let block = Block::default().title(" atc watch ").borders(Borders::ALL);

    let paragraph = Paragraph::new(text).block(block);

    frame.render_widget(paragraph, frame.area());
}

fn run_status(problem: &super::app::ProblemState) -> String {
    let run = &problem.run;

    match run.phase {
        RunPhase::Idle => "Idle".to_string(),

        RunPhase::Queued => "Queued".to_string(),

        RunPhase::Compiling => "Compiling".to_string(),

        RunPhase::Running => {
            if run.total_cases == 0 {
                "Running".to_string()
            } else {
                format!("Running ({} cases)", run.total_cases)
            }
        }

        RunPhase::Finished => {
            format!("Finished ({} / {} AC)", run.accepted, run.total_cases)
        }

        RunPhase::CompileError => "Compile Error".to_string(),

        RunPhase::CompileTimedOut => "Compile Timed Out".to_string(),

        RunPhase::NoSamples => "No Samples".to_string(),

        RunPhase::Failed => "Failed".to_string(),
    }
}
