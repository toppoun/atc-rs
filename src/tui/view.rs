use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

use super::app::{CaseState, CaseVerdict, ProblemState, RunPhase, WatchApp};
use crate::language::Language;

pub fn render(frame: &mut Frame, app: &WatchApp) -> u16 {
    let area = frame.area();
    let current_problem = app.current_problem();

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

    let language = current_problem
        .and_then(|problem| problem.source.as_ref())
        .map(|source| language_label(source.language))
        .unwrap_or("-");

    let debug = if app.debug_enabled() {
        "DEBUG ON"
    } else {
        "DEBUG OFF"
    };

    let summary = current_problem
        .map(run_summary)
        .unwrap_or_else(|| "-".to_string());

    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            app.contest_id(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" │ "),
        Span::raw(source),
        Span::raw(" │ "),
        Span::raw(language),
        Span::raw(" │ "),
        Span::raw(debug),
        Span::raw(" │ "),
        Span::styled(summary, summary_style(current_problem)),
        Span::raw(" "),
    ]);

    let outer = Block::default().title(title).borders(Borders::ALL);

    let inner = outer.inner(area);

    frame.render_widget(outer, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);

    // A ✓   B ✗   C …   D ·
    let problems =
        Paragraph::new(problem_status_line(app)).block(Block::default().borders(Borders::BOTTOM));

    frame.render_widget(problems, rows[0]);

    // 選択中sample / compile error等の詳細
    let detail_text = detail_text(app);

    let content_height = detail_text.lines().count().max(1);

    let viewport_height = usize::from(rows[1].height);

    let max_scroll = content_height
        .saturating_sub(viewport_height)
        .min(usize::from(u16::MAX)) as u16;

    let scroll = app.detail_scroll().min(max_scroll);

    let detail = Paragraph::new(detail_text).scroll((scroll, 0));

    frame.render_widget(detail, rows[1]);

    if max_scroll > 0 {
        let mut scrollbar_state =
            ScrollbarState::new(usize::from(max_scroll) + 1).position(usize::from(scroll));

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);

        frame.render_stateful_widget(scrollbar, rows[1], &mut scrollbar_state);
    }

    // まだ実装済みの操作だけ表示する
    let footer = Paragraph::new("d debug   ↑↓/j k sample   ←→/h l problem   wheel scroll   q quit")
        .block(Block::default().borders(Borders::TOP));

    frame.render_widget(footer, rows[2]);

    max_scroll
}

fn language_label(language: Language) -> &'static str {
    match language {
        Language::Cpp => "C++",
        Language::Python => "Python",
    }
}

fn run_summary(problem: &ProblemState) -> String {
    let run = &problem.run;

    match run.phase {
        RunPhase::Idle => "Idle".to_string(),

        RunPhase::Queued => "Queued".to_string(),

        RunPhase::Compiling => "Compiling...".to_string(),

        RunPhase::Running => "Running...".to_string(),

        RunPhase::Finished => {
            format!("{}/{} AC", run.accepted, run.total_cases)
        }

        RunPhase::CompileError => "CE".to_string(),

        RunPhase::CompileTimedOut => "Compile TLE".to_string(),

        RunPhase::NoSamples => "No Samples".to_string(),

        RunPhase::Failed => "Failed".to_string(),
    }
}

fn summary_style(problem: Option<&ProblemState>) -> Style {
    let Some(problem) = problem else {
        return Style::default();
    };

    match problem.run.phase {
        RunPhase::Finished
            if problem.run.total_cases > 0 && problem.run.accepted == problem.run.total_cases =>
        {
            Style::default().fg(Color::Green)
        }

        RunPhase::Finished
        | RunPhase::CompileError
        | RunPhase::CompileTimedOut
        | RunPhase::Failed => Style::default().fg(Color::Red),

        RunPhase::Queued | RunPhase::Compiling | RunPhase::Running => {
            Style::default().fg(Color::Yellow)
        }

        RunPhase::Idle | RunPhase::NoSamples => Style::default().fg(Color::DarkGray),
    }
}

fn problem_status_line(app: &WatchApp) -> Line<'static> {
    let selected = app.current_problem().map(|problem| problem.index.as_str());

    let mut spans = Vec::new();

    for (index, problem) in app.problems().iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }

        let symbol = problem_symbol(problem);

        let mut style = problem_style(problem);

        if selected == Some(problem.index.as_str()) {
            style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        }

        spans.push(Span::styled(format!("{} {}", problem.index, symbol), style));
    }

    Line::from(spans)
}

fn problem_symbol(problem: &ProblemState) -> &'static str {
    let run = &problem.run;

    match run.phase {
        RunPhase::Idle => "·",

        RunPhase::Queued | RunPhase::Compiling | RunPhase::Running => "…",

        RunPhase::Finished if run.total_cases > 0 && run.accepted == run.total_cases => "✓",

        RunPhase::Finished
        | RunPhase::CompileError
        | RunPhase::CompileTimedOut
        | RunPhase::Failed => "✗",

        RunPhase::NoSamples => "-",
    }
}

fn problem_style(problem: &ProblemState) -> Style {
    match problem.run.phase {
        RunPhase::Finished
            if problem.run.total_cases > 0 && problem.run.accepted == problem.run.total_cases =>
        {
            Style::default().fg(Color::Green)
        }

        RunPhase::Finished
        | RunPhase::CompileError
        | RunPhase::CompileTimedOut
        | RunPhase::Failed => Style::default().fg(Color::Red),

        RunPhase::Queued | RunPhase::Compiling | RunPhase::Running => {
            Style::default().fg(Color::Yellow)
        }

        RunPhase::Idle | RunPhase::NoSamples => Style::default().fg(Color::DarkGray),
    }
}

fn detail_text(app: &WatchApp) -> String {
    let Some(problem) = app.current_problem() else {
        return "No problems".to_string();
    };

    let run = &problem.run;

    match run.phase {
        RunPhase::Idle => "Waiting for a source change...".to_string(),

        RunPhase::Queued => "Queued...".to_string(),

        RunPhase::Compiling => {
            format!("Compiling {}...", problem.index)
        }

        RunPhase::CompileError => {
            let mut text = "Compile Error".to_string();

            if let Some(error) = &run.error {
                append_section(&mut text, "compiler output", error);
            }

            text
        }

        RunPhase::CompileTimedOut => "Compile Timed Out".to_string(),

        RunPhase::NoSamples => "No samples".to_string(),

        RunPhase::Failed => {
            let mut text = "Run Failed".to_string();

            if let Some(error) = &run.error {
                append_section(&mut text, "error", error);
            }

            text
        }

        RunPhase::Running | RunPhase::Finished => sample_detail(app, problem),
    }
}

fn sample_detail(app: &WatchApp, problem: &ProblemState) -> String {
    let total = problem.run.total_cases;

    if total == 0 {
        return "Running samples...".to_string();
    }

    let Some(case) = app.selected_case_state() else {
        return format!(
            "sample {} / {}\n\nPending...",
            app.selected_case() + 1,
            total,
        );
    };

    let mut text = format!(
        "sample {} / {}   {}{}",
        app.selected_case() + 1,
        total,
        verdict_label(case.verdict),
        elapsed_label(case.elapsed),
    );

    match case.verdict {
        CaseVerdict::Pending => {
            text.push_str("\n\nPending...");
        }

        CaseVerdict::Accepted => {
            text.push_str("\n\nAccepted");

            if let Some(stderr) = &case.stderr {
                append_section(&mut text, "stderr", stderr);
            }
        }

        CaseVerdict::WrongAnswer => {
            append_section(
                &mut text,
                "expected",
                case.expected.as_deref().unwrap_or(""),
            );

            append_section(&mut text, "actual", case.actual.as_deref().unwrap_or(""));

            if let Some(stderr) = &case.stderr {
                append_section(&mut text, "stderr", stderr);
            }
        }

        CaseVerdict::RuntimeError => {
            text.push_str("\n\nRuntime Error");

            if let Some(stderr) = &case.stderr {
                append_section(&mut text, "stderr", stderr);
            }
        }

        CaseVerdict::TimedOut => {
            text.push_str("\n\nTime Limit Exceeded");

            if let Some(stderr) = &case.stderr {
                append_section(&mut text, "stderr", stderr);
            }
        }
    }

    text
}

fn verdict_label(verdict: CaseVerdict) -> &'static str {
    match verdict {
        CaseVerdict::Pending => "Pending",
        CaseVerdict::Accepted => "AC",
        CaseVerdict::WrongAnswer => "WA",
        CaseVerdict::RuntimeError => "RE",
        CaseVerdict::TimedOut => "TLE",
    }
}

fn elapsed_label(elapsed: Option<Duration>) -> String {
    let Some(elapsed) = elapsed else {
        return String::new();
    };

    format!("   {:.1} ms", elapsed.as_secs_f64() * 1000.0)
}

fn append_section(text: &mut String, label: &str, content: &str) {
    text.push_str("\n\n");
    text.push_str(label);
    text.push('\n');

    if content.is_empty() {
        text.push_str("(empty)");
    } else {
        text.push_str(content);
    }
}
