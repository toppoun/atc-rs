use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

use super::app::{CaseVerdict, ProblemState, RunPhase, WatchApp};
use super::detail::DetailDocument;
use crate::language::Language;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const SAMPLES_PANE_WIDTH: u16 = 20;
const MIN_DETAIL_WIDTH: u16 = 30;
const MIN_SAMPLES_LAYOUT_WIDTH: u16 = SAMPLES_PANE_WIDTH + MIN_DETAIL_WIDTH;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderInfo {
    pub max_detail_scroll: usize,
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

    // detailは描画前にterminal cell幅でsoft wrapする。
    let detail_document = DetailDocument::from_app(app);
    let viewport_height = usize::from(detail_area.height);

    // まずdetail幅全部を使ってwrapする。
    let mut wrapped_detail_text = wrap_detail_document(&detail_document, detail_area.width);

    let mut max_detail_scroll = max_scroll(wrapped_detail_text.height(), viewport_height);

    if max_detail_scroll > 0 && detail_area.width > 1 {
        wrapped_detail_text =
            wrap_detail_document(&detail_document, detail_area.width.saturating_sub(1));

        max_detail_scroll = max_scroll(wrapped_detail_text.height(), viewport_height);
    }

    let scroll = app.detail_scroll().min(max_detail_scroll);
    let viewport_detail_text = viewport_text(wrapped_detail_text, scroll, viewport_height);

    let detail = Paragraph::new(viewport_detail_text);

    frame.render_widget(detail, detail_area);

    if max_detail_scroll > 0 {
        let (content_length, position) = scrollbar_metrics(max_detail_scroll, scroll);
        let mut scrollbar_state = ScrollbarState::new(content_length).position(position);

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);

        frame.render_stateful_widget(scrollbar, detail_area, &mut scrollbar_state);
    }

    let footer = Paragraph::new(
        "s samples   d debug   r rerun   ↑↓/j k sample   ←→/h l problem   wheel scroll   q quit",
    )
    .block(Block::default().borders(Borders::TOP));

    frame.render_widget(footer, rows[2]);

    RenderInfo {
        max_detail_scroll,
        samples_area,
        detail_area,
    }
}

fn wrap_detail_document(document: &DetailDocument<'_>, width: u16) -> Text<'static> {
    let width = usize::from(width);
    let mut lines = Vec::new();
    let mut logical_line_fragments = Vec::new();

    for segment in document.segments() {
        let text = segment.text();
        let mut start = 0;

        for (newline, _) in text.match_indices('\n') {
            logical_line_fragments.push(&text[start..newline]);
            wrap_logical_line_fragments(&logical_line_fragments, width, &mut lines);
            logical_line_fragments.clear();
            start = newline + 1;
        }

        logical_line_fragments.push(&text[start..]);
    }

    wrap_logical_line_fragments(&logical_line_fragments, width, &mut lines);

    Text::from(lines)
}

fn wrap_logical_line_fragments(fragments: &[&str], width: usize, lines: &mut Vec<Line<'static>>) {
    let mut non_empty = fragments
        .iter()
        .copied()
        .filter(|fragment| !fragment.is_empty());
    let Some(first) = non_empty.next() else {
        lines.push(Line::from(""));
        return;
    };

    if non_empty.next().is_none() {
        wrap_logical_line(first, width, lines);
        return;
    }

    // 通常のraw outputは1つのfragmentのまま処理される。これはdocumentの
    // segment境界がlogical lineの途中にある場合だけ使う小さな互換経路。
    let capacity = fragments.iter().map(|fragment| fragment.len()).sum();
    let mut logical_line = String::with_capacity(capacity);
    for fragment in fragments {
        logical_line.push_str(fragment);
    }

    wrap_logical_line(&logical_line, width, lines);
}

fn wrap_logical_line(logical_line: &str, width: usize, lines: &mut Vec<Line<'static>>) {
    if width == 0 || logical_line.is_empty() {
        lines.push(Line::from(logical_line.to_owned()));
        return;
    }

    let mut current = String::new();
    let mut current_width = 0usize;

    for token in UnicodeSegmentation::split_word_bounds(logical_line) {
        let token_width = UnicodeWidthStr::width(token);

        // token自体が1行に収まる場合。
        if token_width <= width {
            if !current.is_empty() && current_width.saturating_add(token_width) > width {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }

            current.push_str(token);
            current_width = current_width.saturating_add(token_width);

            if current_width == width {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }

            continue;
        }

        // tokenそのものが横幅より長い場合。
        // まず現在の行を確定する。
        if current_width > 0 {
            lines.push(Line::from(std::mem::take(&mut current)));
            current_width = 0;
        }

        // 長すぎるtokenだけgrapheme単位でhard wrapする。
        for grapheme in UnicodeSegmentation::graphemes(token, true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);

            if current_width > 0 && current_width.saturating_add(grapheme_width) > width {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }

            current.push_str(grapheme);
            current_width = current_width.saturating_add(grapheme_width);

            if current_width >= width {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }
        }
    }

    if !current.is_empty() {
        lines.push(Line::from(current));
    }
}

fn max_scroll(content_height: usize, viewport_height: usize) -> usize {
    content_height.saturating_sub(viewport_height)
}

fn viewport_text(
    wrapped_text: Text<'static>,
    absolute_scroll: usize,
    viewport_height: usize,
) -> Text<'static> {
    Text::from(
        wrapped_text
            .lines
            .into_iter()
            .skip(absolute_scroll)
            .take(viewport_height)
            .collect::<Vec<_>>(),
    )
}

fn scrollbar_metrics(max_scroll: usize, scroll: usize) -> (usize, usize) {
    (max_scroll.saturating_add(1), scroll.min(max_scroll))
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

    fn render_info(app: &WatchApp, width: u16, height: u16) -> RenderInfo {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut info = RenderInfo::default();

        terminal
            .draw(|frame| {
                info = render(frame, app);
            })
            .unwrap();

        info
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
    fn scrollbar_metrics_preserve_large_absolute_positions() {
        assert_eq!(scrollbar_metrics(99_970, 0), (99_971, 0));
        assert_eq!(scrollbar_metrics(99_970, 70_000), (99_971, 70_000));
        assert_eq!(scrollbar_metrics(99_970, 100_000), (99_971, 99_970));
        assert_eq!(
            scrollbar_metrics(usize::MAX, usize::MAX),
            (usize::MAX, usize::MAX)
        );
    }

    #[test]
    fn rendered_max_clamps_an_absolute_scroll_that_is_past_the_new_bottom() {
        let mut app = app();
        app.scroll_detail_down(100_000);

        let info = render_info(&app, 20, 7);
        assert!(info.max_detail_scroll < 100_000);

        app.clamp_detail_scroll(info.max_detail_scroll);
        assert_eq!(app.detail_scroll(), info.max_detail_scroll);
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
