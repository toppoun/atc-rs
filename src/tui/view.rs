use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};

use super::app::{
    CaseVerdict, DetailMode, ProblemState, RunPhase, StressPhase, StressSetupState, WatchApp,
};
use super::detail::{DetailDocument, DetailSectionKind};
use super::detail_layout::DetailLayout;
use super::detail_scrollbar::{
    DetailScrollbarGeometry, DetailScrollbarInteraction, DetailScrollbarPixelGeometry,
    render_detail_scrollbar,
};
use super::mouse::MouseMode;
use crate::language::Language;

const SAMPLES_PANE_WIDTH: u16 = 20;
const MIN_DETAIL_WIDTH: u16 = 30;
const MIN_SAMPLES_LAYOUT_WIDTH: u16 = SAMPLES_PANE_WIDTH + MIN_DETAIL_WIDTH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetailSectionHeaderTarget {
    pub(super) kind: DetailSectionKind,
    pub(super) area: Rect,
    pub(super) detail_revision: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderInfo {
    pub max_detail_scroll: Option<usize>,
    pub samples_area: Option<Rect>,
    pub detail_area: Rect,
    pub(super) detail_scrollbar: Option<DetailScrollbarInteraction>,
    pub(super) detail_section_headers: Vec<DetailSectionHeaderTarget>,
}

#[cfg(test)]
pub(super) fn render(
    frame: &mut Frame,
    app: &WatchApp,
    detail_layout: &mut DetailLayout,
) -> RenderInfo {
    render_with_mouse_mode(frame, app, detail_layout, MouseMode::Cells)
}

pub(super) fn render_with_mouse_mode(
    frame: &mut Frame,
    app: &WatchApp,
    detail_layout: &mut DetailLayout,
    mouse_mode: MouseMode,
) -> RenderInfo {
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

    let detail_document = DetailDocument::from_app(app);
    let viewport_height = usize::from(detail_area.height);
    let detail_viewport = detail_layout.viewport(
        &detail_document,
        app.detail_revision(),
        detail_area.width,
        viewport_height,
        app.detail_scroll(),
    );
    detail_layout.stage_analysis_command(&detail_document);
    let detail_section_headers = detail_viewport
        .visible_section_headers
        .iter()
        .filter_map(|header| {
            let viewport_row = u16::try_from(header.viewport_row).ok()?;
            Some(DetailSectionHeaderTarget {
                kind: header.kind,
                area: Rect::new(
                    detail_area.x,
                    detail_area.y.saturating_add(viewport_row),
                    detail_area.width,
                    1,
                ),
                detail_revision: app.detail_revision(),
            })
        })
        .collect();
    let detail = Paragraph::new(detail_viewport.text);

    frame.render_widget(detail, detail_area);

    let mut detail_scrollbar = None;
    if let Some(max_detail_scroll) = detail_viewport.max_scroll
        && max_detail_scroll > 0
        && let Some(geometry) = DetailScrollbarGeometry::new(
            detail_area,
            max_detail_scroll,
            detail_viewport.effective_scroll,
            viewport_height,
            detail_viewport
                .exact_section_visual_rows
                .as_deref()
                .unwrap_or_default(),
        )
    {
        let pixel_geometry = match mouse_mode {
            MouseMode::Pixels {
                metrics,
                generation,
                ..
            } => DetailScrollbarPixelGeometry::new(&geometry, metrics.cell_height_px, generation),
            MouseMode::Disabled | MouseMode::Cells => None,
        };
        render_detail_scrollbar(frame, &geometry, pixel_geometry);
        if detail_viewport.exact_section_visual_rows.is_some()
            && let Some(identity) = detail_viewport.exact_layout_identity
        {
            detail_scrollbar = DetailScrollbarInteraction::new(identity, geometry, pixel_geometry);
        }
    }

    let footer_text = if current_problem
        .is_some_and(|problem| matches!(&problem.stress_setup, StressSetupState::Required { .. }))
    {
        "s samples   S stress   i initialize   d debug   r rerun   ↑↓/j k case   ←→/h l problem   wheel scroll   q quit"
    } else {
        "s samples   S stress   d debug   r rerun   ↑↓/j k case   ←→/h l problem   wheel scroll   q quit"
    };
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::TOP));

    frame.render_widget(footer, rows[2]);

    RenderInfo {
        max_detail_scroll: detail_viewport.max_scroll,
        samples_area,
        detail_area,
        detail_scrollbar,
        detail_section_headers,
    }
}

fn language_label(language: Language) -> &'static str {
    match language {
        Language::Cpp => "C++",
        Language::Python => "Python",
    }
}

fn run_summary(problem: &ProblemState) -> String {
    if problem.detail_mode == DetailMode::Stress {
        if problem.stress.phase != StressPhase::Idle {
            return stress_summary(problem);
        }
        if let Some(summary) = stress_setup_summary(problem) {
            return summary;
        }
    }

    let run = &problem.run;

    match run.phase {
        RunPhase::Idle => "Idle".to_string(),
        RunPhase::Queued => "Queued".to_string(),
        RunPhase::Compiling => "Compiling...".to_string(),
        RunPhase::Running => "Running...".to_string(),
        RunPhase::Finished => format!("{}/{} AC", run.accepted, run.total_cases),
        RunPhase::CompileError => "CE".to_string(),
        RunPhase::CompileTimedOut => "Compile TLE".to_string(),
        RunPhase::NoSamples => "No Samples".to_string(),
        RunPhase::Cancelled => "Cancelled".to_string(),
        RunPhase::Failed => "Failed".to_string(),
    }
}

fn stress_setup_summary(problem: &ProblemState) -> Option<String> {
    match &problem.stress_setup {
        StressSetupState::None => None,
        StressSetupState::Required { .. } => Some("STRESS Setup required".to_string()),
        StressSetupState::Initialized => Some("STRESS Initialized".to_string()),
        StressSetupState::Error { .. } => Some("STRESS Setup error".to_string()),
    }
}

fn stress_summary(problem: &ProblemState) -> String {
    let stress = &problem.stress;

    match stress.phase {
        StressPhase::Idle => "Idle".to_string(),
        StressPhase::Queued => "STRESS Queued".to_string(),
        StressPhase::Compiling => "STRESS Compiling...".to_string(),
        StressPhase::Running => format!("STRESS {}...", stress.passed),
        StressPhase::Failed => stress
            .failure
            .as_ref()
            .map(|failure| format!("STRESS {}", failure.kind.as_str()))
            .unwrap_or_else(|| "STRESS Failed".to_string()),
        StressPhase::Finished => format!("STRESS {} passed", stress.passed),
        StressPhase::Cancelled => "STRESS Cancelled".to_string(),
        StressPhase::Error => "STRESS Error".to_string(),
    }
}

fn summary_style(problem: Option<&ProblemState>) -> Style {
    let Some(problem) = problem else {
        return Style::default();
    };

    if problem.detail_mode == DetailMode::Stress {
        if problem.stress.phase != StressPhase::Idle {
            return match problem.stress.phase {
                StressPhase::Failed | StressPhase::Error => Style::default().fg(Color::Red),
                StressPhase::Queued | StressPhase::Compiling | StressPhase::Running => {
                    Style::default().fg(Color::Yellow)
                }
                StressPhase::Finished => Style::default().fg(Color::Green),
                StressPhase::Idle | StressPhase::Cancelled => Style::default().fg(Color::DarkGray),
            };
        }
        let setup_style = match &problem.stress_setup {
            StressSetupState::None => None,
            StressSetupState::Required { .. } => Some(Style::default().fg(Color::Yellow)),
            StressSetupState::Initialized => Some(Style::default().fg(Color::Green)),
            StressSetupState::Error { .. } => Some(Style::default().fg(Color::Red)),
        };
        if let Some(style) = setup_style {
            return style;
        }
    }

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

        RunPhase::Idle | RunPhase::NoSamples | RunPhase::Cancelled => {
            Style::default().fg(Color::DarkGray)
        }
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
    match &problem.stress_setup {
        StressSetupState::Required { .. } | StressSetupState::Error { .. } => return "!",
        StressSetupState::None | StressSetupState::Initialized => {}
    }

    if problem.detail_mode == DetailMode::Stress && problem.stress.phase != StressPhase::Idle {
        return match problem.stress.phase {
            StressPhase::Queued | StressPhase::Compiling | StressPhase::Running => "…",
            StressPhase::Failed | StressPhase::Error => "✗",
            StressPhase::Finished => "✓",
            StressPhase::Idle | StressPhase::Cancelled => "·",
        };
    }

    let run = &problem.run;

    match run.phase {
        RunPhase::Idle | RunPhase::Cancelled => "·",
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
    match &problem.stress_setup {
        StressSetupState::Required { .. } => return Style::default().fg(Color::Yellow),
        StressSetupState::Error { .. } => return Style::default().fg(Color::Red),
        StressSetupState::None | StressSetupState::Initialized => {}
    }

    if problem.detail_mode == DetailMode::Stress && problem.stress.phase != StressPhase::Idle {
        return match problem.stress.phase {
            StressPhase::Failed | StressPhase::Error => Style::default().fg(Color::Red),
            StressPhase::Queued | StressPhase::Compiling | StressPhase::Running => {
                Style::default().fg(Color::Yellow)
            }
            StressPhase::Finished => Style::default().fg(Color::Green),
            StressPhase::Idle | StressPhase::Cancelled => Style::default().fg(Color::DarkGray),
        };
    }

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

        RunPhase::Idle | RunPhase::NoSamples | RunPhase::Cancelled => {
            Style::default().fg(Color::DarkGray)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleRow {
    Sample { flat_index: usize, number: usize },
    Blank,
    StressHeader,
    Stress { flat_index: usize },
}

fn sample_rows(problem: &ProblemState) -> Vec<SampleRow> {
    let mut rows = (0..problem.sample_cases)
        .map(|index| SampleRow::Sample {
            flat_index: index,
            number: index + 1,
        })
        .collect::<Vec<_>>();

    if problem.saved_stress_case.is_some() {
        if !rows.is_empty() {
            rows.push(SampleRow::Blank);
        }
        rows.push(SampleRow::StressHeader);
        rows.push(SampleRow::Stress {
            flat_index: problem.sample_cases,
        });
    }

    rows
}

fn samples_text(app: &WatchApp, height: u16) -> Text<'static> {
    let Some(problem) = app.current_problem() else {
        return Text::default();
    };

    let mut lines = vec![
        Line::styled("Samples", Style::default().add_modifier(Modifier::BOLD)),
        Line::from(""),
    ];

    let rows = sample_rows(problem);
    let visible = usize::from(height.saturating_sub(2));
    let selected_case = app.selected_case();
    let selected_row = rows
        .iter()
        .position(|row| match row {
            SampleRow::Sample { flat_index, .. } | SampleRow::Stress { flat_index } => {
                *flat_index == selected_case
            }
            SampleRow::Blank | SampleRow::StressHeader => false,
        })
        .unwrap_or(0);
    let range = sample_window(rows.len(), selected_row, visible);
    let selected = (problem.detail_mode == DetailMode::Samples).then_some(selected_case);

    for row in &rows[range] {
        match *row {
            SampleRow::Sample { flat_index, number } => {
                lines.push(sample_line(problem, flat_index, number, selected));
            }
            SampleRow::Blank => lines.push(Line::from("")),
            SampleRow::StressHeader => lines.push(Line::styled(
                "Stress",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            SampleRow::Stress { flat_index } => {
                lines.push(sample_line(problem, flat_index, 1, selected));
            }
        }
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

fn sample_line(
    problem: &ProblemState,
    flat_index: usize,
    number: usize,
    selected: Option<usize>,
) -> Line<'static> {
    let case = problem.run.cases.get(flat_index);

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

    let is_selected = selected == Some(flat_index);
    let marker = if is_selected { ">" } else { " " };

    if is_selected {
        style = style.add_modifier(Modifier::BOLD);
    }

    let elapsed = case
        .and_then(|case| case.elapsed)
        .map(compact_elapsed_label)
        .unwrap_or_default();

    let text = if elapsed.is_empty() {
        format!("{marker} {:>2}  {verdict}", number)
    } else {
        format!("{marker} {:>2}  {:<3}  {elapsed}", number, verdict,)
    };

    Line::styled(text, style)
}

fn compact_elapsed_label(elapsed: Duration) -> String {
    format!("{:.1}ms", elapsed.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;
    use crate::model::{Contest, Problem};
    use crate::stress::CandidateFailureKind;
    use crate::tui::detail_layout::{max_scroll, viewport_text, wrap_detail_document};
    use crate::tui::message::{StressEvent, TestEvent};
    use crate::tui::mouse::{PixelCoordinateOrigin, TerminalPixelMetrics};
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

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

    fn render_info(app: &WatchApp, width: u16, height: u16) -> RenderInfo {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut info = RenderInfo::default();
        let mut detail_layout = DetailLayout::default();

        terminal
            .draw(|frame| {
                info = render(frame, app, &mut detail_layout);
            })
            .unwrap();

        info
    }

    fn rendered_buffer_text(app: &WatchApp, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut detail_layout = DetailLayout::default();
        terminal
            .draw(|frame| {
                render(frame, app, &mut detail_layout);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for row in 0..height {
            for column in 0..width {
                text.push_str(buffer.cell((column, row)).unwrap().symbol());
            }
            text.push('\n');
        }
        text
    }

    fn render_with_layout(
        terminal: &mut Terminal<TestBackend>,
        app: &WatchApp,
        detail_layout: &mut DetailLayout,
    ) -> RenderInfo {
        let mut info = RenderInfo::default();
        terminal
            .draw(|frame| info = render(frame, app, detail_layout))
            .unwrap();
        info
    }

    fn apply_ready_count(layout: &mut DetailLayout, document: &DetailDocument<'_>) {
        layout.stage_analysis_command(document);
        let request = match layout
            .take_analysis_command()
            .expect("expected detail analysis")
        {
            crate::tui::detail_layout::DetailAnalysisCommand::Count(request) => request,
            crate::tui::detail_layout::DetailAnalysisCommand::BuildStructure(request) => {
                let structure = crate::tui::detail_layout::build_document_structure_cancellable(
                    &request.snapshot,
                    || false,
                )
                .unwrap();
                assert!(layout.apply_analysis_result(
                    crate::tui::detail_layout::DetailAnalysisResult::StructureReady(
                        crate::tui::detail_layout::DetailStructureResult {
                            identity: request.identity,
                            structure,
                        },
                    ),
                ));
                layout.stage_analysis_command(document);
                let Some(crate::tui::detail_layout::DetailAnalysisCommand::Count(request)) =
                    layout.take_analysis_command()
                else {
                    panic!("completed structure must stage count");
                };
                request
            }
            _ => panic!("expected staged detail count"),
        };
        let anchor = request.anchor;
        let mut never_cancel = || false;
        let count = request
            .structure
            .count_chunks(
                &request.snapshot,
                request.identity.layout_width,
                anchor,
                &mut never_cancel,
            )
            .unwrap();
        assert!(
            layout.apply_count_result(crate::tui::detail_layout::DetailCountResult {
                identity: request.identity,
                exact_layout_index: count.exact_layout_index,
                anchor,
                anchor_visual_row: count.anchor_visual_row,
                anchor_row_raw_start: count.anchor_row_raw_start,
            })
        );
    }

    fn large_compile_error_app() -> WatchApp {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));

        let mut output = String::new();
        for line in 0..3_000 {
            output.push_str(&format!("compiler-line-{line}\n"));
        }
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::CompileFailed { stderr: output },
        ));
        app
    }

    fn foldable_app(actual: String) -> WatchApp {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_stress(0, 123).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.stress_event(
            0,
            request.run_id,
            StressEvent::Started {
                base_seed: 123,
                case_limit: None,
            },
        ));
        assert!(app.stress_event(
            0,
            request.run_id,
            StressEvent::Failed {
                kind: CandidateFailureKind::WrongAnswer,
                case_number: 1,
                base_seed: 123,
                seed: 456,
                input: "input body".to_string(),
                expected: "expected body".to_string(),
                actual,
                stderr: "stderr body".to_string(),
                candidate_elapsed: Duration::from_millis(1),
                elapsed: Duration::from_millis(2),
                saved_to: PathBuf::from(".atc/stress/A"),
            },
        ));
        app
    }

    fn finished_sample_app(accepted: bool) -> WatchApp {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        let verdict_recorded = if accepted {
            app.run_event(
                0,
                request.run_id,
                TestEvent::TestCaseAccepted {
                    number: 1,
                    elapsed: Duration::from_millis(1),
                },
            )
        } else {
            app.run_event(
                0,
                request.run_id,
                TestEvent::TestCaseWrongAnswer {
                    number: 1,
                    elapsed: Duration::from_millis(1),
                },
            )
        };
        assert!(verdict_recorded);
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunFinished {
                accepted: usize::from(accepted),
                total_cases: 1,
            },
        ));
        app
    }

    #[test]
    fn stress_setup_attention_glyphs_and_header_styles_are_distinct() {
        let mut app = app();

        assert!(app.set_stress_setup_required(0, true, true));
        let problem = app.current_problem().unwrap();
        assert_eq!(run_summary(problem), "STRESS Setup required");
        assert_eq!(summary_style(Some(problem)).fg, Some(Color::Yellow));
        assert_eq!(problem_symbol(problem), "!");
        assert_eq!(problem_style(problem).fg, Some(Color::Yellow));

        assert!(app.set_stress_setup_error(0, "invalid target".to_string()));
        let problem = app.current_problem().unwrap();
        assert_eq!(run_summary(problem), "STRESS Setup error");
        assert_eq!(summary_style(Some(problem)).fg, Some(Color::Red));
        assert_eq!(problem_symbol(problem), "!");
        assert_eq!(problem_style(problem).fg, Some(Color::Red));
    }

    #[test]
    fn stress_setup_attention_glyphs_survive_switching_to_sample_detail() {
        let mut app = app();

        assert!(app.set_stress_setup_required(0, true, true));
        assert!(app.next_case());
        let problem = app.current_problem().unwrap();
        assert_eq!(problem.detail_mode, DetailMode::Samples);
        assert_eq!(problem_symbol(problem), "!");
        assert_eq!(problem_style(problem).fg, Some(Color::Yellow));

        assert!(app.set_stress_setup_error(0, "invalid target".to_string()));
        assert!(app.next_case());
        let problem = app.current_problem().unwrap();
        assert_eq!(problem.detail_mode, DetailMode::Samples);
        assert_eq!(problem_symbol(problem), "!");
        assert_eq!(problem_style(problem).fg, Some(Color::Red));
    }

    #[test]
    fn real_stress_phase_precedes_setup_header_but_not_setup_attention_glyph() {
        let mut app = app();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        assert!(app.queue_stress(0, 123).is_some());
        assert!(app.set_stress_setup_initialized(0));

        let problem = app.current_problem().unwrap();
        assert_eq!(run_summary(problem), "STRESS Queued");
        assert_eq!(summary_style(Some(problem)).fg, Some(Color::Yellow));
        assert_eq!(problem_symbol(problem), "…");
        assert_eq!(problem_style(problem).fg, Some(Color::Yellow));

        assert!(app.set_stress_setup_error(0, "invalid target".to_string()));
        let problem = app.current_problem().unwrap();
        assert_eq!(run_summary(problem), "STRESS Queued");
        assert_eq!(summary_style(Some(problem)).fg, Some(Color::Yellow));
        assert_eq!(problem_symbol(problem), "!");
        assert_eq!(problem_style(problem).fg, Some(Color::Red));
    }

    #[test]
    fn initialized_without_execution_keeps_the_neutral_problem_glyph() {
        let mut app = app();
        assert!(app.set_stress_setup_initialized(0));

        let problem = app.current_problem().unwrap();
        assert_eq!(run_summary(problem), "STRESS Initialized");
        assert_eq!(summary_style(Some(problem)).fg, Some(Color::Green));
        assert_eq!(problem_symbol(problem), "·");
        assert_ne!(problem_symbol(problem), "✓");
        assert_eq!(problem_style(problem).fg, Some(Color::DarkGray));
    }

    #[test]
    fn initialized_preserves_an_existing_wrong_answer_glyph_and_style() {
        let mut app = finished_sample_app(false);
        assert_eq!(problem_symbol(app.current_problem().unwrap()), "✗");
        assert!(app.set_stress_setup_initialized(0));

        let problem = app.current_problem().unwrap();
        assert_eq!(run_summary(problem), "STRESS Initialized");
        assert_eq!(problem_symbol(problem), "✗");
        assert_eq!(problem_style(problem).fg, Some(Color::Red));
    }

    #[test]
    fn initialized_preserves_a_real_success_glyph_and_style() {
        let mut app = finished_sample_app(true);
        assert_eq!(problem_symbol(app.current_problem().unwrap()), "✓");
        assert!(app.set_stress_setup_initialized(0));

        let problem = app.current_problem().unwrap();
        assert_eq!(run_summary(problem), "STRESS Initialized");
        assert_eq!(problem_symbol(problem), "✓");
        assert_eq!(problem_style(problem).fg, Some(Color::Green));
    }

    #[test]
    fn footer_advertises_initialize_only_while_setup_is_required() {
        let mut app = app();
        assert!(!rendered_buffer_text(&app, 120, 20).contains("i initialize"));

        assert!(app.set_stress_setup_required(0, true, true));
        assert!(rendered_buffer_text(&app, 120, 20).contains("i initialize"));

        assert!(app.set_stress_setup_initialized(0));
        assert!(!rendered_buffer_text(&app, 120, 20).contains("i initialize"));

        assert!(app.set_stress_setup_error(0, "invalid target".to_string()));
        assert!(!rendered_buffer_text(&app, 120, 20).contains("i initialize"));
    }

    fn wrap_text(text: &str, width: u16) -> Text<'static> {
        let segments = [text];
        let document = DetailDocument::from_borrowed_segments(&segments);
        wrap_detail_document(&document, width)
    }

    #[test]
    fn max_scroll_has_no_off_by_one_and_is_not_truncated_to_u16() {
        assert_eq!(max_scroll(0, 0), 0);
        assert_eq!(max_scroll(3, 3), 0);
        assert_eq!(max_scroll(4, 3), 1);
        assert_eq!(max_scroll(10, 3), 7);
        assert_eq!(max_scroll(100_000, 30), 99_970);
        assert_eq!(max_scroll(usize::MAX, 0), usize::MAX);
    }

    #[test]
    fn viewport_text_extracts_top_middle_and_bottom_without_cloning_the_document() {
        fn visual_lines(count: usize) -> Text<'static> {
            Text::from(
                (0..count)
                    .map(|line| Line::from(format!("line-{line}")))
                    .collect::<Vec<_>>(),
            )
        }

        assert_eq!(
            text_lines(&viewport_text(visual_lines(10), 0, 3)),
            ["line-0", "line-1", "line-2"]
        );
        assert_eq!(
            text_lines(&viewport_text(visual_lines(10), 4, 3)),
            ["line-4", "line-5", "line-6"]
        );
        assert_eq!(
            text_lines(&viewport_text(visual_lines(10), 7, 3)),
            ["line-7", "line-8", "line-9"]
        );
    }

    #[test]
    fn viewport_text_handles_zero_height_and_scroll_past_the_end() {
        let lines = Text::from(vec![Line::from("a"), Line::from("b")]);
        assert!(text_lines(&viewport_text(lines, 0, 0)).is_empty());

        let lines = Text::from(vec![Line::from("a"), Line::from("b")]);
        assert!(text_lines(&viewport_text(lines, 10, 1)).is_empty());
    }

    #[test]
    fn rendered_max_clamps_an_absolute_scroll_that_is_past_the_new_bottom() {
        let mut app = app();
        app.scroll_detail_down(100_000);

        let info = render_info(&app, 20, 7);
        let max_detail_scroll = info.max_detail_scroll.unwrap();
        assert!(max_detail_scroll < 100_000);

        app.clamp_detail_scroll(max_detail_scroll);
        assert_eq!(app.detail_scroll(), max_detail_scroll);
    }

    #[test]
    fn large_initial_render_reports_unknown_max_while_small_render_is_exact() {
        assert!(render_info(&app(), 80, 30).max_detail_scroll.is_some());

        let large = large_compile_error_app();
        assert_eq!(render_info(&large, 80, 30).max_detail_scroll, None);
    }

    #[test]
    fn background_count_makes_the_next_large_render_exact() {
        let app = large_compile_error_app();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = DetailLayout::default();
        let mut info = RenderInfo::default();

        terminal
            .draw(|frame| info = render(frame, &app, &mut layout))
            .unwrap();
        assert_eq!(info.max_detail_scroll, None);
        assert!(info.detail_scrollbar.is_none());

        let document = DetailDocument::from_app(&app);
        layout.complete_structure_for_test(&document);
        layout.stage_analysis_command(&document);

        let Some(crate::tui::detail_layout::DetailAnalysisCommand::Count(request)) =
            layout.take_analysis_command()
        else {
            panic!("large render must stage background counting");
        };
        let mut never_cancel = || false;
        let count = request
            .structure
            .count_chunks(
                &request.snapshot,
                request.identity.layout_width,
                request.anchor,
                &mut never_cancel,
            )
            .unwrap();
        assert!(
            layout.apply_count_result(crate::tui::detail_layout::DetailCountResult {
                identity: request.identity,
                exact_layout_index: count.exact_layout_index,
                anchor: request.anchor,
                anchor_visual_row: count.anchor_visual_row,
                anchor_row_raw_start: count.anchor_row_raw_start,
            })
        );

        terminal
            .draw(|frame| info = render(frame, &app, &mut layout))
            .unwrap();
        assert!(info.max_detail_scroll.is_some());
        assert!(info.detail_scrollbar.is_some());
    }

    #[test]
    fn visible_fold_headers_come_from_the_materialized_lazy_viewport_before_exact_count() {
        let mut app = foldable_app("actual body\n".repeat(3_000));
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = DetailLayout::default();

        let expanded = render_with_layout(&mut terminal, &app, &mut layout);
        assert_eq!(expanded.max_detail_scroll, None);
        assert!(
            expanded
                .detail_section_headers
                .iter()
                .any(|header| header.kind == DetailSectionKind::Actual)
        );

        app.toggle_detail_section(DetailSectionKind::Actual);
        let collapsed = render_with_layout(&mut terminal, &app, &mut layout);
        assert!(
            collapsed
                .detail_section_headers
                .iter()
                .any(|header| header.kind == DetailSectionKind::Stderr)
        );
    }

    #[test]
    fn collapsing_a_large_actual_reduces_exact_scroll_range() {
        let mut app = foldable_app("actual body\n".repeat(1_000));
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = DetailLayout::default();

        let expanded = render_with_layout(&mut terminal, &app, &mut layout);
        let expanded_max = expanded.max_detail_scroll.unwrap();
        assert!(expanded_max > 900);

        app.toggle_detail_section(DetailSectionKind::Actual);
        let collapsed = render_with_layout(&mut terminal, &app, &mut layout);
        let collapsed_max = collapsed.max_detail_scroll.unwrap();
        assert!(collapsed_max < expanded_max);
    }

    #[test]
    fn pixels_mode_renders_fractional_thumb_while_cells_mode_keeps_whole_cells() {
        let mut app = foldable_app("actual body\n".repeat(100));
        let mut probe_terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut probe_layout = DetailLayout::default();
        let probe = render_with_layout(&mut probe_terminal, &app, &mut probe_layout);
        let maximum = probe.max_detail_scroll.unwrap();
        let detail_area = probe.detail_area;
        let scroll = (1..maximum)
            .find(|scroll| {
                let geometry = DetailScrollbarGeometry::new(
                    detail_area,
                    maximum,
                    *scroll,
                    usize::from(detail_area.height),
                    &[],
                )
                .unwrap();
                let projection = geometry.pixel_projection(17).unwrap();
                !projection.thumb_top_px().is_multiple_of(17)
                    && geometry.thumb_start_row
                        == u16::try_from(projection.thumb_top_px() / 17).unwrap()
            })
            .unwrap();
        app.set_detail_scroll_from_user(scroll);

        let mut cells_terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut cells_layout = DetailLayout::default();
        let cells_info = render_with_layout(&mut cells_terminal, &app, &mut cells_layout);
        let cells_geometry = &cells_info.detail_scrollbar.as_ref().unwrap().geometry;
        assert!(
            cells_info
                .detail_scrollbar
                .as_ref()
                .unwrap()
                .pixel_geometry
                .is_none()
        );
        assert_eq!(
            cells_terminal
                .backend()
                .buffer()
                .cell((cells_geometry.gutter.x, cells_geometry.thumb_start_row))
                .unwrap()
                .symbol(),
            "█"
        );

        let metrics = TerminalPixelMetrics::validated(80, 20, 800, 340, 10, 17).unwrap();
        let mut pixels_terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut pixels_layout = DetailLayout::default();
        let mut pixels_info = RenderInfo::default();
        pixels_terminal
            .draw(|frame| {
                pixels_info = render_with_mouse_mode(
                    frame,
                    &app,
                    &mut pixels_layout,
                    MouseMode::Pixels {
                        metrics,
                        origin: PixelCoordinateOrigin::ZeroBased,
                        generation: 7,
                    },
                );
            })
            .unwrap();
        let pixels_scrollbar = pixels_info.detail_scrollbar.as_ref().unwrap();
        assert!(pixels_scrollbar.pixel_geometry.is_some());
        let projection = pixels_scrollbar.geometry.pixel_projection(17).unwrap();
        let top_row = u16::try_from(projection.thumb_top_px() / 17).unwrap();
        let bottom_row = u16::try_from(projection.thumb_bottom_px() / 17).unwrap();
        let top_cell = pixels_terminal
            .backend()
            .buffer()
            .cell((pixels_scrollbar.geometry.gutter.x, top_row))
            .unwrap();
        let bottom_cell = pixels_terminal
            .backend()
            .buffer()
            .cell((pixels_scrollbar.geometry.gutter.x, bottom_row))
            .unwrap();
        let block_symbols = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
        assert_ne!(top_cell.symbol(), "█");
        assert!(block_symbols.contains(&top_cell.symbol()));
        assert!(block_symbols.contains(&bottom_cell.symbol()));
        assert!(bottom_cell.modifier.contains(Modifier::REVERSED));

        let transitioned_x = pixels_scrollbar.geometry.gutter.x;
        pixels_terminal
            .draw(|frame| {
                pixels_info =
                    render_with_mouse_mode(frame, &app, &mut pixels_layout, MouseMode::Cells);
            })
            .unwrap();
        let cells_scrollbar = pixels_info.detail_scrollbar.as_ref().unwrap();
        assert!(cells_scrollbar.pixel_geometry.is_none());
        assert_eq!(cells_scrollbar.geometry.gutter.x, transitioned_x);
        let transitioned_buffer = pixels_terminal.backend().buffer();
        assert_eq!(
            transitioned_buffer
                .cell((transitioned_x, cells_scrollbar.geometry.thumb_start_row))
                .unwrap()
                .symbol(),
            "█"
        );
        for row in
            cells_scrollbar.geometry.track_start_row..cells_scrollbar.geometry.track_end_row()
        {
            assert!(
                !transitioned_buffer
                    .cell((transitioned_x, row))
                    .unwrap()
                    .modifier
                    .contains(Modifier::REVERSED)
            );
        }
    }

    #[test]
    fn fold_hides_and_restores_scrollbar_without_stale_gutter_cells() {
        let mut app = foldable_app("actual body\n".repeat(100));
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        let mut layout = DetailLayout::default();

        let expanded = render_with_layout(&mut terminal, &app, &mut layout);
        assert!(expanded.detail_scrollbar.is_some());

        for kind in [
            DetailSectionKind::Input,
            DetailSectionKind::Expected,
            DetailSectionKind::Actual,
            DetailSectionKind::Stderr,
        ] {
            app.toggle_detail_section(kind);
        }
        let collapsed = render_with_layout(&mut terminal, &app, &mut layout);
        assert_eq!(collapsed.max_detail_scroll, Some(0));
        assert!(collapsed.detail_scrollbar.is_none());

        let mut fresh_terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        let mut fresh_layout = DetailLayout::default();
        let fresh_collapsed = render_with_layout(&mut fresh_terminal, &app, &mut fresh_layout);
        assert!(fresh_collapsed.detail_scrollbar.is_none());
        assert_eq!(
            terminal.backend().buffer(),
            fresh_terminal.backend().buffer()
        );

        app.toggle_detail_section(DetailSectionKind::Actual);
        let reexpanded = render_with_layout(&mut terminal, &app, &mut layout);
        assert!(reexpanded.detail_scrollbar.is_some());

        let maximum = reexpanded.max_detail_scroll.unwrap();
        let detail_area = reexpanded.detail_area;
        let scroll = (1..maximum)
            .find(|scroll| {
                let geometry = DetailScrollbarGeometry::new(
                    detail_area,
                    maximum,
                    *scroll,
                    usize::from(detail_area.height),
                    &[],
                )
                .unwrap();
                !geometry
                    .pixel_projection(17)
                    .unwrap()
                    .thumb_top_px()
                    .is_multiple_of(17)
            })
            .unwrap();
        app.set_detail_scroll_from_user(scroll);

        let metrics = TerminalPixelMetrics::validated(80, 40, 800, 680, 10, 17).unwrap();
        let mut fractional = RenderInfo::default();
        terminal
            .draw(|frame| {
                fractional = render_with_mouse_mode(
                    frame,
                    &app,
                    &mut layout,
                    MouseMode::Pixels {
                        metrics,
                        origin: PixelCoordinateOrigin::ZeroBased,
                        generation: 19,
                    },
                );
            })
            .unwrap();
        let scrollbar = fractional.detail_scrollbar.as_ref().unwrap();
        assert!(scrollbar.pixel_geometry.is_some());
        assert!(
            (scrollbar.geometry.track_start_row..scrollbar.geometry.track_end_row()).any(|row| {
                terminal
                    .backend()
                    .buffer()
                    .cell((scrollbar.geometry.gutter.x, row))
                    .unwrap()
                    .modifier
                    .contains(Modifier::REVERSED)
            })
        );
    }

    #[test]
    fn collapsed_sections_keep_exact_semantic_markers() {
        let mut app = foldable_app("actual body".to_string());
        app.toggle_detail_section(DetailSectionKind::Actual);
        let document = DetailDocument::from_app(&app);
        let mut layout = DetailLayout::default();
        let viewport = layout.viewport(&document, app.detail_revision(), 80, 20, 0);

        assert!(
            viewport
                .exact_section_visual_rows
                .unwrap()
                .iter()
                .any(|section| section.kind == DetailSectionKind::Actual)
        );
    }

    #[test]
    fn toggles_preserve_scroll_until_existing_render_clamp_applies() {
        let mut app = foldable_app("actual body\n".repeat(1_000));
        app.scroll_detail_down(500);
        let before = app.detail_scroll();

        app.toggle_detail_section(DetailSectionKind::Actual);
        assert_eq!(app.detail_scroll(), before);
        let info = render_info(&app, 80, 20);
        let collapsed_max = info.max_detail_scroll.unwrap();
        assert!(collapsed_max < before);
        app.clamp_detail_scroll(collapsed_max);
        assert_eq!(app.detail_scroll(), collapsed_max);

        app.toggle_detail_section(DetailSectionKind::Actual);
        assert_eq!(app.detail_scroll(), collapsed_max);
    }

    #[test]
    fn expanded_background_count_cannot_publish_after_actual_is_collapsed() {
        let mut app = foldable_app("actual body\n".repeat(3_000));
        let expanded_document = DetailDocument::from_app(&app);
        let mut layout = DetailLayout::default();
        layout.viewport(&expanded_document, app.detail_revision(), 80, 20, 0);
        layout.stage_analysis_command(&expanded_document);
        let crate::tui::detail_layout::DetailAnalysisCommand::Count(request) =
            layout.take_analysis_command().unwrap()
        else {
            panic!("expanded lazy document must stage exact counting");
        };
        let anchor = request.anchor;
        let count = request
            .structure
            .count_chunks(
                &request.snapshot,
                request.identity.layout_width,
                anchor,
                || false,
            )
            .unwrap();
        let stale = crate::tui::detail_layout::DetailCountResult {
            identity: request.identity,
            exact_layout_index: count.exact_layout_index,
            anchor,
            anchor_visual_row: count.anchor_visual_row,
            anchor_row_raw_start: count.anchor_row_raw_start,
        };
        drop(expanded_document);

        app.toggle_detail_section(DetailSectionKind::Actual);
        let collapsed_document = DetailDocument::from_app(&app);
        let collapsed = layout.viewport(&collapsed_document, app.detail_revision(), 80, 20, 0);
        assert!(collapsed.max_scroll.is_some());
        assert!(!layout.apply_count_result(stale));
        let after_stale = layout.viewport(&collapsed_document, app.detail_revision(), 80, 20, 0);
        assert_eq!(after_stale.max_scroll, collapsed.max_scroll);
    }

    #[test]
    fn triangle_headers_use_terminal_cell_width_not_utf8_byte_length() {
        let app = foldable_app("body".to_string());
        let document = DetailDocument::from_app(&app);
        let lines = text_lines(&wrap_detail_document(&document, 8));

        assert!(lines.iter().any(|line| line == "▼ Actual"));
    }

    #[test]
    fn rendering_extremely_small_terminals_does_not_panic() {
        for (width, height) in [(0, 0), (1, 1), (2, 2), (5, 3)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let app = app();
            let mut detail_layout = DetailLayout::default();

            terminal
                .draw(|frame| {
                    let _ = render(frame, &app, &mut detail_layout);
                })
                .unwrap();
        }
    }
    #[test]
    fn saved_stress_case_is_rendered_after_samples_with_its_own_header() {
        let app = WatchApp::new_with_stress_cases(
            &Contest {
                contest_id: "abc123".to_string(),
                problems: vec![Problem {
                    index: "A".to_string(),
                    title: "Problem A".to_string(),
                    task_id: "abc123_a".to_string(),
                    url: "https://example.invalid/a".to_string(),
                }],
            },
            vec![2],
            vec![Some(crate::model::Sample {
                input: "1\n".to_string(),
                output: "2\n".to_string(),
            })],
        )
        .unwrap();

        assert_eq!(
            sample_rows(app.current_problem().unwrap()),
            vec![
                SampleRow::Sample {
                    flat_index: 0,
                    number: 1,
                },
                SampleRow::Sample {
                    flat_index: 1,
                    number: 2,
                },
                SampleRow::Blank,
                SampleRow::StressHeader,
                SampleRow::Stress { flat_index: 2 },
            ]
        );
        assert_eq!(sample_window(5, 4, 2), 3..5);
    }

    #[test]
    fn sample_window_keeps_selected_sample_visible() {
        assert_eq!(sample_window(0, 0, 5), 0..0);
        assert_eq!(sample_window(10, 5, 0), 0..0);
        assert_eq!(sample_window(10, 0, 5), 0..5);
        assert_eq!(sample_window(10, 2, 5), 0..5);
        assert_eq!(sample_window(10, 5, 5), 3..8);
        assert_eq!(sample_window(10, 9, 5), 5..10);
        assert_eq!(sample_window(10, 0, 1), 0..1);
        assert_eq!(sample_window(10, 9, 1), 9..10);
    }

    #[test]
    fn sample_window_handles_more_space_than_samples() {
        assert_eq!(sample_window(1, 0, 1), 0..1);
        assert_eq!(sample_window(3, 0, 10), 0..3);
        assert_eq!(sample_window(3, 2, 10), 0..3);
    }

    #[test]
    fn render_info_matches_visible_and_automatically_hidden_samples_layout() {
        let mut app = app();
        app.toggle_samples_pane();

        // outer borderを除いた幅が20 + 30のとき、Samplesは固定幅20で表示される。
        let wide = render_info(&app, 52, 12);
        let samples = wide.samples_area.expect("samples pane should be visible");
        assert_eq!(samples.width, SAMPLES_PANE_WIDTH);
        assert_eq!(samples.x.saturating_add(samples.width), wide.detail_area.x);
        assert_eq!(samples.y, wide.detail_area.y);
        assert_eq!(samples.height, wide.detail_area.height);
        assert_eq!(wide.detail_area.width, MIN_DETAIL_WIDTH);

        // 1列狭くなるとstateはONのまま、描画だけMinimalへ戻る。
        let narrow = render_info(&app, 51, 12);
        assert!(narrow.samples_area.is_none());
        assert_eq!(narrow.detail_area.x, 1);
        assert_eq!(narrow.detail_area.width, 49);
        assert!(app.samples_pane_enabled());

        // 再び広げると明示的な再toggleなしで再表示される。
        assert!(render_info(&app, 52, 12).samples_area.is_some());

        app.toggle_samples_pane();
        let disabled = render_info(&app, 52, 12);
        assert!(disabled.samples_area.is_none());
        assert_eq!(disabled.detail_area.x, 1);
        assert_eq!(disabled.detail_area.width, 50);
    }

    #[test]
    fn samples_pane_width_changes_preserve_detail_anchor_and_reconcile_scroll() {
        let mut app = large_compile_error_app();
        app.scroll_detail_down(300);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = DetailLayout::default();

        let disabled = render_with_layout(&mut terminal, &app, &mut layout);
        assert!(disabled.samples_area.is_none());
        {
            let document = DetailDocument::from_app(&app);
            apply_ready_count(&mut layout, &document);
        }
        render_with_layout(&mut terminal, &app, &mut layout);
        let disabled_anchor = layout.last_top_anchor_for_test().unwrap();
        let baseline_scroll = app.detail_scroll();

        app.toggle_samples_pane();
        assert_eq!(app.detail_scroll(), baseline_scroll);
        let enabled = render_with_layout(&mut terminal, &app, &mut layout);
        assert!(enabled.samples_area.is_some());
        assert!(enabled.detail_area.width < disabled.detail_area.width);
        assert!(layout.has_pending_width_anchor_for_test());

        let document = DetailDocument::from_app(&app);
        layout.stage_analysis_command(&document);
        let Some(crate::tui::detail_layout::DetailAnalysisCommand::Count(request)) =
            layout.take_analysis_command()
        else {
            panic!("samples-pane width change must stage anchored count");
        };
        assert_eq!(request.anchor, Some(disabled_anchor));
        let anchor = request.anchor;
        let mut never_cancel = || false;
        let count = request
            .structure
            .count_chunks(
                &request.snapshot,
                request.identity.layout_width,
                anchor,
                &mut never_cancel,
            )
            .unwrap();
        assert!(
            layout.apply_count_result(crate::tui::detail_layout::DetailCountResult {
                identity: request.identity,
                exact_layout_index: count.exact_layout_index,
                anchor,
                anchor_visual_row: count.anchor_visual_row,
                anchor_row_raw_start: count.anchor_row_raw_start,
            })
        );
        let correction = layout.take_scroll_reconciliation().unwrap();
        app.reconcile_detail_scroll(correction);
        render_with_layout(&mut terminal, &app, &mut layout);
        assert!(!layout.has_pending_width_anchor_for_test());

        let reconciled_scroll = app.detail_scroll();
        app.toggle_samples_pane();
        assert_eq!(app.detail_scroll(), reconciled_scroll);
        let disabled_again = render_with_layout(&mut terminal, &app, &mut layout);
        assert!(disabled_again.samples_area.is_none());
        assert!(layout.has_pending_width_anchor_for_test());
    }

    #[test]
    fn detail_wrap_uses_terminal_cell_width() {
        // 全角3文字で6セル。幅6なら1行、4文字目で折り返す。
        let text = wrap_text("あいうえ", 6);

        assert_eq!(text.height(), 2);
        assert_eq!(text_lines(&text), ["あいう", "え"],);
    }

    #[test]
    fn detail_wrap_preserves_explicit_blank_lines() {
        let text = wrap_text("expected\n\nactual\n", 80);

        assert_eq!(text_lines(&text), ["expected", "", "actual", ""]);
    }

    #[test]
    fn detail_wrap_is_safe_for_zero_and_narrow_widths() {
        let zero = wrap_text("abc", 0);
        assert_eq!(zero.height(), 1);

        // 1セル幅に全角文字が来てもgraphemeを壊さず1行として扱う。
        let narrow = wrap_text("あ", 1);
        assert_eq!(narrow.height(), 1);

        // standaloneのzero-width graphemeがwide graphemeから不要に分離されない。
        let zero_width = wrap_text("\u{200b}あ", 1);
        assert_eq!(text_lines(&zero_width), ["\u{200b}あ"]);
    }
    fn text_lines(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }
    #[test]
    fn detail_wraps_ascii_at_terminal_width() {
        let text = wrap_text("123456789", 4);

        assert_eq!(text_lines(&text), ["1234", "5678", "9"],);
    }
    #[test]
    fn detail_wrap_does_not_split_word_when_it_can_move_to_next_line() {
        let text = wrap_text("56 57 58 59", 7);

        assert_eq!(text_lines(&text), ["56 57 ", "58 59"],);
    }

    #[test]
    fn detail_segment_boundaries_are_not_implicit_line_or_token_boundaries() {
        let segments = ["ab", "cd", "\n", "", "\n日本", "語"];
        let document = DetailDocument::from_borrowed_segments(&segments);
        let text = wrap_detail_document(&document, 3);

        assert_eq!(text_lines(&text), ["abc", "d", "", "日", "本", "語"]);
    }

    #[test]
    fn detail_wrap_preserves_whitespace_and_unicode_graphemes() {
        let input = "e\u{301}  👩‍💻 ";
        let text = wrap_text(input, 2);

        assert_eq!(text_lines(&text).concat(), input);
        assert!(text_lines(&text).iter().any(|line| line == "👩‍💻"));
    }

    #[test]
    fn scrollbar_gutter_can_increase_wrapped_height() {
        let input = "1234 5678\nx";
        let full_width = wrap_text(input, 9);
        let with_gutter = wrap_text(input, 8);

        assert_eq!(full_width.height(), 2);
        assert_eq!(max_scroll(full_width.height(), 1), 1);
        assert_eq!(with_gutter.height(), 3);
        assert_eq!(max_scroll(with_gutter.height(), 1), 2);
    }
}
