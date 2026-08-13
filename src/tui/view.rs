use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

use super::app::{CaseVerdict, ProblemState, RunPhase, WatchApp};
use crate::language::Language;

const SAMPLES_PANE_WIDTH: u16 = 20;
const MIN_DETAIL_WIDTH: u16 = 30;
const MIN_SAMPLES_LAYOUT_WIDTH: u16 = SAMPLES_PANE_WIDTH + MIN_DETAIL_WIDTH;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderInfo {
    pub max_detail_scroll: u16,
    pub samples_area: Option<Rect>,
    pub detail_area: Rect,
}

pub fn render(frame: &mut Frame, app: &WatchApp) -> RenderInfo {
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
    let show_samples = app.samples_pane_enabled()
        && current_problem.is_some_and(|problem| problem.total_cases > 0)
        && rows[1].width >= MIN_SAMPLES_LAYOUT_WIDTH;

    let (samples_area, detail_area) = if show_samples {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(SAMPLES_PANE_WIDTH), Constraint::Min(1)])
            .split(rows[1]);

        (Some(columns[0]), columns[1])
    } else {
        (None, rows[1])
    };

    if let Some(samples_area) = samples_area {
        let samples = Paragraph::new(samples_text(app, samples_area.height))
            .block(Block::default().borders(Borders::RIGHT));

        frame.render_widget(samples, samples_area);
    }

    // 選択中sample / compile error等の詳細
    let detail_text = Text::raw(detail_text(app));

    // Paragraphはwrapしないため、Text::height()がそのまま縦方向のcontent高さ。
    let content_height = detail_text.height();
    let viewport_height = usize::from(detail_area.height);

    let max_scroll = max_scroll(content_height, viewport_height);

    let scroll = app.detail_scroll().min(max_scroll);

    let detail = Paragraph::new(detail_text).scroll((scroll, 0));

    frame.render_widget(detail, detail_area);

    if max_scroll > 0 {
        let mut scrollbar_state =
            ScrollbarState::new(usize::from(max_scroll) + 1).position(usize::from(scroll));

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);

        frame.render_stateful_widget(scrollbar, detail_area, &mut scrollbar_state);
    }

    // まだ実装済みの操作だけ表示する
    let footer = Paragraph::new(
        "s samples   d debug   ↑↓/j k sample   ←→/h l problem   wheel scroll   q quit",
    )
    .block(Block::default().borders(Borders::TOP));

    frame.render_widget(footer, rows[2]);

    RenderInfo {
        max_detail_scroll: max_scroll,
        samples_area,
        detail_area,
    }
}

fn max_scroll(content_height: usize, viewport_height: usize) -> u16 {
    content_height
        .saturating_sub(viewport_height)
        .min(usize::from(u16::MAX)) as u16
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

fn samples_text(app: &WatchApp, height: u16) -> Text<'static> {
    let Some(problem) = app.current_problem() else {
        return Text::default();
    };

    let total = problem.total_cases;

    let mut lines = vec![
        Line::styled("Samples", Style::default().add_modifier(Modifier::BOLD)),
        Line::from(""),
    ];

    let visible = usize::from(height.saturating_sub(2));

    let range = sample_window(total, app.selected_case(), visible);

    for index in range {
        lines.push(sample_line(problem, index, app.selected_case()));
    }

    Text::from(lines)
}

fn sample_window(total: usize, selected: usize, visible: usize) -> std::ops::Range<usize> {
    if total == 0 || visible == 0 {
        return 0..0;
    }

    let visible = visible.min(total);
    let selected = selected.min(total - 1);

    let mut start = selected.saturating_sub(visible / 2);

    if start + visible > total {
        start = total - visible;
    }

    start..start + visible
}

fn sample_line(problem: &ProblemState, index: usize, selected: usize) -> Line<'static> {
    let case = problem.run.cases.get(index);

    let (verdict, mut style) = match case {
        Some(case) => match case.verdict {
            CaseVerdict::Pending => ("·", Style::default().fg(Color::DarkGray)),

            CaseVerdict::Accepted => ("AC", Style::default().fg(Color::Green)),

            CaseVerdict::WrongAnswer => ("WA", Style::default().fg(Color::Red)),

            CaseVerdict::RuntimeError => ("RE", Style::default().fg(Color::Red)),

            CaseVerdict::TimedOut => ("TLE", Style::default().fg(Color::Red)),
        },

        None => ("·", Style::default().fg(Color::DarkGray)),
    };

    let marker = if index == selected { ">" } else { " " };

    if index == selected {
        style = style.add_modifier(Modifier::BOLD);
    }

    let elapsed = case
        .and_then(|case| case.elapsed)
        .map(compact_elapsed_label)
        .unwrap_or_default();

    let text = if elapsed.is_empty() {
        format!("{marker} {:>2}  {verdict}", index + 1)
    } else {
        format!("{marker} {:>2}  {:<3}  {elapsed}", index + 1, verdict,)
    };

    Line::styled(text, style)
}

fn compact_elapsed_label(elapsed: Duration) -> String {
    format!("{:.1}ms", elapsed.as_secs_f64() * 1000.0)
}

fn detail_text(app: &WatchApp) -> String {
    let Some(problem) = app.current_problem() else {
        return "No problems".to_string();
    };

    let run = &problem.run;

    let detail = match run.phase {
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
    };

    format!("{} - {}\n\n{detail}", problem.index, problem.title)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Contest, Problem};
    use ratatui::{Terminal, backend::TestBackend};

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
            vec![1],
        )
        .unwrap()
    }

    #[test]
    fn max_scroll_has_no_off_by_one_and_saturates_to_u16() {
        assert_eq!(max_scroll(0, 0), 0);
        assert_eq!(max_scroll(3, 3), 0);
        assert_eq!(max_scroll(4, 3), 1);
        assert_eq!(max_scroll(10, 3), 7);
        assert_eq!(max_scroll(usize::MAX, 0), u16::MAX);
    }

    #[test]
    fn rendering_extremely_small_terminals_does_not_panic() {
        for (width, height) in [(0, 0), (1, 1), (2, 2), (5, 3)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let app = app();

            terminal
                .draw(|frame| {
                    let _ = render(frame, &app);
                })
                .unwrap();
        }
    }
    #[test]
    fn sample_window_keeps_selected_sample_visible() {
        assert_eq!(sample_window(0, 0, 5), 0..0);
        assert_eq!(sample_window(10, 0, 5), 0..5);
        assert_eq!(sample_window(10, 2, 5), 0..5);
        assert_eq!(sample_window(10, 5, 5), 3..8);
        assert_eq!(sample_window(10, 9, 5), 5..10);
    }

    #[test]
    fn sample_window_handles_more_space_than_samples() {
        assert_eq!(sample_window(3, 0, 10), 0..3);
        assert_eq!(sample_window(3, 2, 10), 0..3);
    }
}
