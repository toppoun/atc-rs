use std::{ops::Range, time::Duration};

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::app::{
    CaseSelection, CaseVerdict, DetailMode, ProblemState, RunPhase, StressPhase, StressSetupState,
    UserInputEditTarget, UserInputSelection, WatchApp,
};
use super::detail::{
    DetailDocument, DetailFoldAnimationFrame, DetailSectionClip, DetailSectionKind,
};
use super::detail_layout::DetailViewport;
use super::detail_layout::{DetailLayout, animated_body_clip};
use super::detail_scrollbar::{
    DetailScrollbarGeometry, DetailScrollbarInteraction, DetailScrollbarPixelGeometry,
    VerticalScrollbarGeometry, render_detail_scrollbar,
};
use super::mouse::MouseMode;
use super::{
    CommandPalette, EditorTargetModal, FrontendActionAvailability, OpenSettingsModal,
    OpenSourceModal, OpenTemplateModal, OpenWorkspaceSettingsModal, RefreshContestModal,
    RefreshContestModalState, SwitchContestModal, SwitchContestModalState,
};
use crate::language::Language;

const SAMPLES_PANE_WIDTH: u16 = 20;
const MIN_DETAIL_WIDTH: u16 = 30;
const MIN_SAMPLES_LAYOUT_WIDTH: u16 = SAMPLES_PANE_WIDTH + MIN_DETAIL_WIDTH;
const COMMAND_PALETTE_WIDTH: u16 = 76;
const COMMAND_PALETTE_BORDER_ROWS: u16 = 2;
const COMMAND_PALETTE_COMMAND_ROW_OFFSET: u16 = 2;
const COMMAND_PALETTE_ROWS_AFTER_COMMANDS: u16 = 3;
const COMMAND_PALETTE_FIXED_HEIGHT: u16 = COMMAND_PALETTE_BORDER_ROWS
    + COMMAND_PALETTE_COMMAND_ROW_OFFSET
    + COMMAND_PALETTE_ROWS_AFTER_COMMANDS;
const COMMAND_PALETTE_MAX_VISIBLE_COMMANDS: usize = 10;
const COMMAND_PALETTE_LABEL_WIDTH: usize = 18;
const COMMAND_PALETTE_SCROLLBAR_GUTTER_WIDTH: u16 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandPaletteLayout {
    area: Rect,
    inner_area: Rect,
    query_area: Rect,
    command_area: Rect,
    list_area: Rect,
    scrollbar_gutter: Option<Rect>,
    status_area: Rect,
    help_area: Rect,
    command_capacity: usize,
    viewport: Range<usize>,
    show_scrollbar: bool,
    list_width: usize,
}

fn command_palette_viewport(total: usize, selected: usize, capacity: usize) -> Range<usize> {
    if total == 0 || capacity == 0 {
        return 0..0;
    }

    let capacity = capacity.min(total);
    let selected = selected.min(total - 1);
    let start = selected.saturating_add(1).saturating_sub(capacity);
    start..start.saturating_add(capacity).min(total)
}

fn command_palette_layout(frame_area: Rect, total: usize, selected: usize) -> CommandPaletteLayout {
    let desired_command_rows = total.clamp(1, COMMAND_PALETTE_MAX_VISIBLE_COMMANDS);
    let desired_height = COMMAND_PALETTE_FIXED_HEIGHT
        .saturating_add(u16::try_from(desired_command_rows).unwrap_or(u16::MAX));
    let width = frame_area.width.min(COMMAND_PALETTE_WIDTH);
    let height = frame_area.height.min(desired_height);
    let area = Rect::new(
        frame_area
            .x
            .saturating_add(frame_area.width.saturating_sub(width) / 2),
        frame_area
            .y
            .saturating_add(frame_area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    let inner_area = Block::default().borders(Borders::ALL).inner(area);
    let command_capacity =
        usize::from(height.saturating_sub(COMMAND_PALETTE_FIXED_HEIGHT)).min(desired_command_rows);
    let viewport = command_palette_viewport(total, selected, command_capacity);
    let command_area = clipped_rect(
        inner_area,
        COMMAND_PALETTE_COMMAND_ROW_OFFSET,
        u16::try_from(command_capacity).unwrap_or(u16::MAX),
    );
    let show_scrollbar = total > command_capacity
        && command_capacity > 0
        && inner_area.width > COMMAND_PALETTE_SCROLLBAR_GUTTER_WIDTH;
    let scrollbar_gutter = show_scrollbar.then(|| {
        Rect::new(
            command_area.x.saturating_add(
                command_area
                    .width
                    .saturating_sub(COMMAND_PALETTE_SCROLLBAR_GUTTER_WIDTH),
            ),
            command_area.y,
            COMMAND_PALETTE_SCROLLBAR_GUTTER_WIDTH.min(command_area.width),
            command_area.height,
        )
    });
    let list_width = command_area
        .width
        .saturating_sub(scrollbar_gutter.map_or(0, |gutter| gutter.width));
    let list_area = Rect::new(
        command_area.x,
        command_area.y,
        list_width,
        command_area.height,
    );
    let status_row = COMMAND_PALETTE_COMMAND_ROW_OFFSET
        .saturating_add(u16::try_from(command_capacity).unwrap_or(u16::MAX));

    CommandPaletteLayout {
        area,
        inner_area,
        query_area: clipped_rect(inner_area, 0, 1),
        command_area,
        list_area,
        scrollbar_gutter,
        status_area: clipped_rect(inner_area, status_row, 1),
        help_area: clipped_rect(inner_area, status_row.saturating_add(2), 1),
        command_capacity,
        viewport,
        show_scrollbar,
        list_width: usize::from(list_width),
    }
}

fn clipped_rect(area: Rect, row_offset: u16, height: u16) -> Rect {
    let row_offset = row_offset.min(area.height);
    Rect::new(
        area.x,
        area.y.saturating_add(row_offset),
        area.width,
        height.min(area.height.saturating_sub(row_offset)),
    )
}

fn fit_command_palette_row(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let text_width = UnicodeWidthStr::width(text);
    if text_width <= width {
        let mut fitted = text.to_string();
        fitted.push_str(&" ".repeat(width - text_width));
        return fitted;
    }

    if width == 1 {
        return text
            .graphemes(true)
            .find(|grapheme| UnicodeWidthStr::width(*grapheme) == 1)
            .unwrap_or(" ")
            .to_string();
    }

    let content_width = width - 1;
    let mut fitted = String::new();
    let mut fitted_width = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if fitted_width.saturating_add(grapheme_width) > content_width {
            break;
        }
        fitted.push_str(grapheme);
        fitted_width = fitted_width.saturating_add(grapheme_width);
    }
    fitted.push('…');
    fitted.push_str(&" ".repeat(width.saturating_sub(fitted_width.saturating_add(1))));
    fitted
}

fn command_palette_row(marker: &str, label: &str, shortcut: Option<&str>, width: usize) -> String {
    let shortcut = shortcut.unwrap_or("");
    let aligned = format!("{marker} {label:<COMMAND_PALETTE_LABEL_WIDTH$} {shortcut}");
    let compact = if shortcut.is_empty() {
        format!("{marker} {label}")
    } else {
        format!("{marker} {label} {shortcut}")
    };
    let base = if UnicodeWidthStr::width(aligned.as_str()) <= width {
        aligned
    } else if UnicodeWidthStr::width(compact.as_str()) <= width {
        compact
    } else {
        format!("{marker} {label}")
    };
    fit_command_palette_row(&base, width)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetailSectionHeaderTarget {
    pub(super) kind: DetailSectionKind,
    pub(super) area: Rect,
    pub(super) detail_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UserInputDetailAction {
    Run,
    Edit,
    Save,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UserInputDetailActionTarget {
    pub(super) action: UserInputDetailAction,
    pub(super) area: Rect,
    pub(super) detail_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CasesRowAction {
    Select(CaseSelection),
    DeleteUserInput(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CasesRowTarget {
    pub(super) action: CasesRowAction,
    pub(super) area: Rect,
    pub(super) problem: usize,
    pub(super) revision: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderInfo {
    pub max_detail_scroll: Option<usize>,
    pub samples_area: Option<Rect>,
    pub(super) samples_body_area: Option<Rect>,
    pub(super) new_input_area: Option<Rect>,
    pub detail_area: Rect,
    pub(super) detail_scrollbar: Option<DetailScrollbarInteraction>,
    pub(super) detail_section_headers: Vec<DetailSectionHeaderTarget>,
    pub(super) user_input_detail_actions: Vec<UserInputDetailActionTarget>,
    pub(super) cases_row_targets: Vec<CasesRowTarget>,
    pub(super) editor_cursor: Option<(u16, u16)>,
    pub(super) editor_scroll_reconciliation: Option<usize>,
}

impl RenderInfo {
    pub(super) fn cases_row_target_at(
        &self,
        app: &WatchApp,
        column: u16,
        row: u16,
    ) -> Option<CasesRowTarget> {
        self.cases_row_targets.iter().copied().find(|target| {
            Some(target.problem) == app.selected_problem()
                && target.revision == app.detail_revision()
                && contains_rect(target.area, column, row)
        })
    }

    pub(super) fn detail_section_header_at(
        &self,
        detail_revision: u64,
        column: u16,
        row: u16,
    ) -> Option<DetailSectionHeaderTarget> {
        self.detail_section_headers.iter().copied().find(|header| {
            header.detail_revision == detail_revision
                && column >= header.area.x
                && column < header.area.x.saturating_add(header.area.width)
                && row >= header.area.y
                && row < header.area.y.saturating_add(header.area.height)
        })
    }

    pub(super) fn user_input_detail_action_at(
        &self,
        detail_revision: u64,
        column: u16,
        row: u16,
    ) -> Option<UserInputDetailActionTarget> {
        self.user_input_detail_actions
            .iter()
            .copied()
            .find(|target| {
                target.detail_revision == detail_revision
                    && column >= target.area.x
                    && column < target.area.right()
                    && row >= target.area.y
                    && row < target.area.bottom()
            })
    }
}

fn cases_pane_areas(area: Rect) -> (Rect, Rect, Option<Rect>) {
    let content_width = area.width.saturating_sub(1);
    if area.height >= 2 {
        let separator = Rect::new(area.x, area.bottom().saturating_sub(2), content_width, 1);
        let action = Rect::new(area.x, area.bottom().saturating_sub(1), content_width, 1);
        let body = Rect::new(area.x, area.y, content_width, area.height.saturating_sub(2));
        (body, action, Some(separator))
    } else {
        (
            Rect::new(area.x, area.y, content_width, 0),
            Rect::new(area.x, area.y, content_width, area.height.min(1)),
            None,
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct FrontendOverlays<'a> {
    pub(super) switch_modal: Option<&'a SwitchContestModal>,
    pub(super) refresh_modal: Option<&'a RefreshContestModal>,
    pub(super) source_modal: Option<&'a OpenSourceModal>,
    pub(super) editor_target_modal: Option<&'a EditorTargetModal>,
    pub(super) command_palette: Option<&'a CommandPalette>,
}

#[cfg(test)]
pub(super) fn render(
    frame: &mut Frame,
    app: &WatchApp,
    detail_layout: &mut DetailLayout,
) -> RenderInfo {
    render_with_mouse_mode(frame, app, detail_layout, MouseMode::Cells)
}

#[cfg(test)]
pub(super) fn render_with_mouse_mode(
    frame: &mut Frame,
    app: &WatchApp,
    detail_layout: &mut DetailLayout,
    mouse_mode: MouseMode,
) -> RenderInfo {
    render_frontend_with_mouse_mode(
        frame,
        app,
        detail_layout,
        mouse_mode,
        false,
        FrontendOverlays::default(),
    )
}

#[cfg(test)]
pub(super) fn render_frontend_with_mouse_mode(
    frame: &mut Frame,
    app: &WatchApp,
    detail_layout: &mut DetailLayout,
    mouse_mode: MouseMode,
    workspace_available: bool,
    overlays: FrontendOverlays<'_>,
) -> RenderInfo {
    render_frontend_with_pointer(
        frame,
        app,
        detail_layout,
        mouse_mode,
        None,
        None,
        workspace_available,
        overlays,
    )
}

pub(super) fn render_frontend_with_pointer(
    frame: &mut Frame,
    app: &WatchApp,
    detail_layout: &mut DetailLayout,
    mouse_mode: MouseMode,
    detail_pointer: Option<(u16, u16)>,
    fold_animation: Option<DetailFoldAnimationFrame>,
    workspace_available: bool,
    overlays: FrontendOverlays<'_>,
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
    let problem_line = problem_status_line(app);
    let problem_line_width = problem_line.width();
    let problem_block = Block::default().borders(Borders::BOTTOM);
    let navigation_area = problem_block.inner(rows[0]);
    let problems = Paragraph::new(problem_line).block(problem_block);

    frame.render_widget(problems, rows[0]);

    if let Some(notice) =
        current_problem.and_then(|problem| problem.user_input_sync_notice.as_deref())
    {
        render_problem_sync_notice(frame, navigation_area, problem_line_width, notice);
    }

    // 選択中sample / compile error等の詳細
    let show_samples = app.samples_pane_enabled()
        && current_problem.is_some()
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

    let mut cases_row_targets = Vec::new();
    let (samples_body_area, new_input_area) = if let Some(samples_area) = samples_area {
        frame.render_widget(Block::default().borders(Borders::RIGHT), samples_area);
        let (body, action, separator) = cases_pane_areas(samples_area);
        let (text, targets) = samples_content(app, body);
        cases_row_targets = targets;
        frame.render_widget(Paragraph::new(text), body);
        if let Some(separator) = separator {
            frame.render_widget(Block::default().borders(Borders::TOP), separator);
        }
        frame.render_widget(Paragraph::new("+ New Input"), action);
        (Some(body), (action.height > 0).then_some(action))
    } else {
        (None, None)
    };

    let full_detail_document = DetailDocument::from_app(app);
    let mut animation_wrap_width = detail_area.width;
    let section_clip = fold_animation.and_then(|animation| {
        animated_section_clip(&full_detail_document, animation, animation_wrap_width)
    });
    let mut detail_document = if section_clip.is_some() {
        DetailDocument::from_app_with_clip(app, section_clip)
    } else {
        full_detail_document
    };
    let viewport_height = usize::from(detail_area.height);
    let mut detail_viewport = detail_layout.viewport(
        &detail_document,
        app.detail_revision(),
        detail_area.width,
        viewport_height,
        app.detail_scroll(),
    );
    if let Some(animation) = fold_animation {
        let actual_wrap_width = detail_layout.current_wrap_width();
        if actual_wrap_width != animation_wrap_width {
            animation_wrap_width = actual_wrap_width;
            let section_clip =
                animated_section_clip(&detail_document, animation, animation_wrap_width);
            detail_document = DetailDocument::from_app_with_clip(app, section_clip);
            detail_viewport = detail_layout.viewport(
                &detail_document,
                app.detail_revision(),
                detail_area.width,
                viewport_height,
                app.detail_scroll(),
            );
        }
    }
    detail_layout.stage_analysis_command(&detail_document);
    let mut detail_section_headers: Vec<DetailSectionHeaderTarget> = detail_viewport
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
    let editor_cursor = editor_cursor_cell(app, &detail_document, &detail_viewport, detail_area);
    let editor_scroll_reconciliation =
        editor_scroll_reconciliation(app, &detail_viewport, viewport_height);
    let detail = Paragraph::new(detail_viewport.text);

    frame.render_widget(detail, detail_area);

    if let Some((column, row)) = editor_cursor
        && let Some(cell) = frame.buffer_mut().cell_mut((column, row))
    {
        cell.modifier.insert(Modifier::REVERSED);
    }

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

    if let Some(scrollbar) = detail_scrollbar.as_ref() {
        for header in &mut detail_section_headers {
            header.area.width = scrollbar.geometry.gutter.x.saturating_sub(header.area.x);
        }
    }

    let user_input_detail_actions =
        user_input_detail_action_targets(app, &mut detail_section_headers, app.detail_revision());
    for target in &user_input_detail_actions {
        let label = match target.action {
            UserInputDetailAction::Run => "[Run]",
            UserInputDetailAction::Edit => "[Edit]",
            UserInputDetailAction::Save => "[Save]",
            UserInputDetailAction::Cancel => "[Cancel]",
        };
        frame.render_widget(Paragraph::new(label), target.area);
    }

    let render_info = RenderInfo {
        cases_row_targets,
        max_detail_scroll: detail_viewport.max_scroll,
        samples_area,
        samples_body_area,
        new_input_area,
        detail_area,
        detail_scrollbar,
        detail_section_headers,
        user_input_detail_actions,
        editor_cursor,
        editor_scroll_reconciliation,
    };
    let overlay_active = overlays.switch_modal.is_some()
        || overlays.refresh_modal.is_some()
        || overlays.source_modal.is_some()
        || overlays.editor_target_modal.is_some()
        || overlays.command_palette.is_some();
    if !overlay_active
        && !matches!(mouse_mode, MouseMode::Disabled)
        && let Some((column, row)) = detail_pointer
    {
        if let Some(target) = render_info.cases_row_target_at(app, column, row)
            && matches!(target.action, CasesRowAction::DeleteUserInput(_))
        {
            frame.buffer_mut().set_style(
                target.area,
                Style::default().fg(Color::White).bg(Color::DarkGray),
            );
        } else if let Some(action) =
            render_info.user_input_detail_action_at(app.detail_revision(), column, row)
        {
            frame
                .buffer_mut()
                .set_style(action.area, Style::default().bg(Color::DarkGray));
        } else if let Some(header) =
            render_info.detail_section_header_at(app.detail_revision(), column, row)
        {
            frame
                .buffer_mut()
                .set_style(header.area, Style::default().bg(Color::DarkGray));
        } else if let Some(action) = render_info
            .new_input_area
            .filter(|area| contains_rect(*area, column, row))
        {
            frame
                .buffer_mut()
                .set_style(action, Style::default().bg(Color::DarkGray));
        }
    }

    let footer_base = if current_problem
        .is_some_and(|problem| matches!(&problem.stress_setup, StressSetupState::Required { .. }))
    {
        "s samples   S stress   i initialize   d debug   r rerun   ↑↓/j k case   ←→/h l problem   wheel scroll"
    } else {
        "s samples   S stress   d debug   r rerun   ↑↓/j k case   ←→/h l problem   wheel scroll"
    };
    let footer_text = if workspace_available {
        format!(": commands   c contest   q quit   {footer_base}")
    } else {
        format!(": commands   q quit   {footer_base}")
    };
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::TOP));

    frame.render_widget(footer, rows[2]);

    if let Some(modal) = overlays.refresh_modal {
        render_refresh_contest_modal(frame, modal);
    } else if let Some(modal) = overlays.switch_modal {
        render_switch_contest_modal(frame, modal);
    } else if let Some(modal) = overlays.source_modal {
        render_open_source_modal(frame, app, modal);
    } else if let Some(modal) = overlays.editor_target_modal {
        render_editor_target_modal(frame, modal);
    } else if let Some(command_palette) = overlays.command_palette {
        render_command_palette(frame, app, command_palette, workspace_available);
    }

    render_info
}

fn contains_rect(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn render_problem_sync_notice(
    frame: &mut Frame,
    navigation_area: Rect,
    navigation_width: usize,
    notice: &str,
) {
    let compact = if let Some(ordinal) = notice
        .strip_prefix("User Input ")
        .and_then(|notice| notice.strip_suffix(" was removed externally."))
        .and_then(|ordinal| ordinal.parse::<usize>().ok())
    {
        format!("! Input {ordinal} removed")
    } else if let Some(count) = notice
        .strip_suffix(" User Inputs were removed externally.")
        .and_then(|count| count.parse::<usize>().ok())
    {
        format!("! {count} inputs removed")
    } else if notice.starts_with("Could not delete User Input:") {
        "! Input delete failed".to_string()
    } else {
        "! Input sync failed".to_string()
    };
    let width = usize::from(navigation_area.width);
    // Reuse the navigation row without moving or covering any problem spans.
    // Paragraph clips the compact notice without wrapping or allocating another row.
    let start = navigation_width
        .saturating_add(2)
        .max(width.saturating_sub(compact.width()));
    if start >= width || navigation_area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(compact).style(Style::default().fg(Color::Yellow)),
        Rect::new(
            navigation_area.x.saturating_add(start as u16),
            navigation_area.y,
            (width - start) as u16,
            1,
        ),
    );
}

fn user_input_detail_action_targets(
    app: &WatchApp,
    headers: &mut Vec<DetailSectionHeaderTarget>,
    detail_revision: u64,
) -> Vec<UserInputDetailActionTarget> {
    let actions: &[UserInputDetailAction] = if app.user_input_editor_active() {
        &[
            UserInputDetailAction::Save,
            UserInputDetailAction::Run,
            UserInputDetailAction::Cancel,
        ]
    } else if matches!(
        app.selected_user_input(),
        Some(UserInputSelection::Persisted(_))
    ) {
        &[UserInputDetailAction::Edit, UserInputDetailAction::Run]
    } else {
        &[]
    };
    if actions.is_empty() {
        return Vec::new();
    }
    let Some(index) = headers
        .iter()
        .position(|header| header.kind == DetailSectionKind::Input)
    else {
        return Vec::new();
    };
    let mut header = headers[index];
    if app.user_input_editor_active() {
        headers.remove(index);
    }

    let action_width = |action| match action {
        UserInputDetailAction::Run => 5_u16,
        UserInputDetailAction::Edit | UserInputDetailAction::Save => 6_u16,
        UserInputDetailAction::Cancel => 8_u16,
    };
    let labels_width = actions.iter().copied().map(action_width).sum::<u16>();
    let gaps = u16::try_from(actions.len().saturating_sub(1)).unwrap_or(u16::MAX);
    let total_width = labels_width.saturating_add(gaps);
    if header.area.width < total_width.saturating_add(4) {
        return Vec::new();
    }
    let mut x = header.area.right().saturating_sub(total_width);
    let mut targets = Vec::with_capacity(actions.len());
    for (position, action) in actions.iter().copied().enumerate() {
        let width = action_width(action);
        targets.push(UserInputDetailActionTarget {
            action,
            area: Rect::new(x, header.area.y, width, 1),
            detail_revision,
        });
        x = x.saturating_add(width);
        if position + 1 < actions.len() {
            x = x.saturating_add(1);
        }
    }
    if !app.user_input_editor_active() {
        header.area.width = targets[0].area.x.saturating_sub(header.area.x);
        headers[index] = header;
    }
    targets
}

fn editor_cursor_cell(
    app: &WatchApp,
    document: &DetailDocument<'_>,
    viewport: &DetailViewport,
    area: Rect,
) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let cursor = document.editor_cursor()?;
    let edit = app.selected_user_input_edit()?;
    let visual = viewport.editor_cursor?;
    let viewport_row = visual.visual_row.checked_sub(viewport.effective_scroll)?;
    if viewport_row >= usize::from(area.height) {
        return None;
    }
    let local_cursor = cursor.raw_position.0.checked_sub(cursor.content_start.0)?;
    let local_start = visual
        .raw_row_start
        .0
        .saturating_sub(cursor.content_start.0)
        .min(edit.buffer().len());
    let prefix = edit.buffer().get(local_start..local_cursor)?;
    let column = u16::try_from(UnicodeWidthStr::width(prefix)).ok()?;
    let column = area.x.checked_add(column)?;
    let row = area.y.checked_add(u16::try_from(viewport_row).ok()?)?;
    (column < area.right() && row < area.bottom()).then_some((column, row))
}

fn editor_scroll_reconciliation(
    app: &WatchApp,
    viewport: &DetailViewport,
    viewport_height: usize,
) -> Option<usize> {
    if viewport_height == 0 {
        return None;
    }
    let cursor_row = viewport.editor_cursor?.visual_row;
    let top = viewport.effective_scroll;
    let bottom = top.saturating_add(viewport_height);
    let target = if cursor_row < top {
        cursor_row
    } else if cursor_row >= bottom {
        cursor_row.saturating_sub(viewport_height.saturating_sub(1))
    } else {
        return None;
    };
    let target = target.min(viewport.max_scroll?);
    (target != app.detail_scroll()).then_some(target)
}

fn animated_section_clip(
    document: &DetailDocument<'_>,
    animation: DetailFoldAnimationFrame,
    wrap_width: u16,
) -> Option<DetailSectionClip> {
    let body = document.section_body(animation.kind)?;
    let clip = animated_body_clip(body, wrap_width, animation.expanded_fraction);
    Some(DetailSectionClip {
        kind: animation.kind,
        prefix_len: clip.prefix_len,
    })
}

fn editor_modal_geometry(frame_area: Rect, desired_height: u16) -> (Rect, usize) {
    let width = frame_area.width.min(76);
    let height = frame_area.height.min(desired_height);
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let line_width = usize::from(Block::default().borders(Borders::ALL).inner(area).width);
    (area, line_width)
}

fn render_editor_target_modal(frame: &mut Frame, modal: &EditorTargetModal) {
    match modal {
        EditorTargetModal::Settings(modal) => render_open_settings_modal(frame, modal),
        EditorTargetModal::WorkspaceSettings(modal) => {
            render_open_workspace_settings_modal(frame, modal);
        }
        EditorTargetModal::Template(modal) => render_open_template_modal(frame, modal),
    }
}

fn render_editor_modal(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    lines: Vec<Line<'static>>,
) {
    let paragraph = Paragraph::new(Text::from(lines)).block(
        Block::default()
            .title(format!(" {title} "))
            .borders(Borders::ALL),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn append_modal_error(lines: &mut Vec<Line<'static>>, error: Option<&str>, line_width: usize) {
    if let Some(error) = error {
        for line in error.lines() {
            lines.push(Line::styled(
                fit_command_palette_row(line, line_width),
                Style::default().fg(Color::Red),
            ));
        }
    }
}

fn render_open_settings_modal(frame: &mut Frame, modal: &OpenSettingsModal) {
    use crate::user_config_fs::EditableFileState;

    let (area, line_width) = editor_modal_geometry(frame.area(), 12);
    let destination = modal
        .target()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unavailable: {error}"));
    let (status, action, inspection_error) = match modal.file_state() {
        Ok(EditableFileState::Existing) => ("existing", "[Enter] Open", None),
        Ok(EditableFileState::Missing) => ("not initialized", "[i] Initialize & Open", None),
        Err(error) => ("unavailable", "", Some(error)),
    };
    let mut lines = vec![
        Line::raw(fit_command_palette_row(status, line_width)),
        Line::raw(""),
        Line::raw(fit_command_palette_row("Destination:", line_width)),
        Line::raw(fit_command_palette_row(
            &format!("  {destination}"),
            line_width,
        )),
        Line::raw(""),
        Line::raw(fit_command_palette_row(action, line_width)),
        Line::raw(fit_command_palette_row("[Esc] Close", line_width)),
    ];
    append_modal_error(
        &mut lines,
        inspection_error.as_deref().or(modal.error.as_deref()),
        line_width,
    );
    render_editor_modal(frame, area, "Open Settings", lines);
}

fn render_open_workspace_settings_modal(frame: &mut Frame, modal: &OpenWorkspaceSettingsModal) {
    use crate::workspace::WorkspaceConfigFileState;

    let (area, line_width) = editor_modal_geometry(frame.area(), 12);
    let (status, action, inspection_error) = match modal.file_state() {
        Ok(WorkspaceConfigFileState::Existing) => ("existing", "[Enter] Open", None),
        Ok(WorkspaceConfigFileState::Missing) => ("workspace settings file is missing", "", None),
        Err(error) => ("unavailable", "", Some(error)),
    };
    let mut lines = vec![
        Line::raw(fit_command_palette_row(status, line_width)),
        Line::raw(""),
        Line::raw(fit_command_palette_row("Destination:", line_width)),
        Line::raw(fit_command_palette_row(
            &format!("  {}", modal.target().display()),
            line_width,
        )),
        Line::raw(""),
        Line::raw(fit_command_palette_row(action, line_width)),
        Line::raw(fit_command_palette_row("[Esc] Close", line_width)),
    ];
    append_modal_error(
        &mut lines,
        inspection_error.as_deref().or(modal.error.as_deref()),
        line_width,
    );
    render_editor_modal(frame, area, "Open Workspace Settings", lines);
}

fn render_open_template_modal(frame: &mut Frame, modal: &OpenTemplateModal) {
    use crate::user_config_fs::EditableFileState;

    let (area, line_width) = editor_modal_geometry(frame.area(), 15);
    let mut lines = Vec::new();
    let mut inspection_error = None;
    for language in Language::ALL {
        let marker = if modal.selected_language() == language {
            ">"
        } else {
            " "
        };
        let mut states = Vec::new();
        if modal.current_language() == Some(language) {
            states.push("current");
        }
        match modal.file_state_for(language) {
            Ok(EditableFileState::Missing) => states.push("not initialized"),
            Ok(EditableFileState::Existing) => {}
            Err(error) => {
                states.push("unavailable");
                inspection_error.get_or_insert(error);
            }
        }
        let state = if states.is_empty() {
            String::new()
        } else {
            format!("  {}", states.join(", "))
        };
        let row = fit_command_palette_row(
            &format!("{marker} {:<8}{state}", language_label(language)),
            line_width,
        );
        let style = if modal.selected_language() == language {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::styled(row, style));
    }

    let destination = modal
        .selected_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unavailable: {error}"));
    let action = match modal.file_state_for(modal.selected_language()) {
        Ok(EditableFileState::Existing) => "[Enter] Open",
        Ok(EditableFileState::Missing) => "[i] Initialize & Open",
        Err(error) => {
            inspection_error.get_or_insert(error);
            ""
        }
    };
    lines.extend([
        Line::raw(""),
        Line::raw(fit_command_palette_row("Destination:", line_width)),
        Line::raw(fit_command_palette_row(
            &format!("  {destination}"),
            line_width,
        )),
        Line::raw(""),
        Line::raw(fit_command_palette_row(action, line_width)),
        Line::raw(fit_command_palette_row(
            "[↑/↓ or j/k] Select   [Esc] Close",
            line_width,
        )),
    ]);
    append_modal_error(
        &mut lines,
        inspection_error.as_deref().or(modal.error.as_deref()),
        line_width,
    );
    render_editor_modal(frame, area, "Open Template", lines);
}

fn render_open_source_modal(frame: &mut Frame, app: &WatchApp, modal: &OpenSourceModal) {
    let frame_area = frame.area();
    let width = frame_area.width.min(76);
    let height = frame_area.height.min(16);
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let line_width = usize::from(inner.width);
    let current = modal.current_language(app);
    let mut lines = vec![
        Line::raw(fit_command_palette_row(
            &format!("Problem: {}", modal.problem_index),
            line_width,
        )),
        Line::raw(""),
    ];

    for language in Language::ALL {
        let marker = if modal.selected_language() == language {
            ">"
        } else {
            " "
        };
        let mut states = Vec::new();
        if current == Some(language) {
            states.push("current");
        }
        if modal.path_for(language).is_ok_and(|path| !path.is_file()) {
            states.push("not created");
        }
        let state = if states.is_empty() {
            String::new()
        } else {
            format!("  {}", states.join(", "))
        };
        let row = fit_command_palette_row(
            &format!("{marker} {:<8}{state}", language_label(language)),
            line_width,
        );
        let style = if modal.selected_language() == language {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::styled(row, style));
    }

    let selected = modal.selected_path();
    let selected_exists = selected.as_ref().is_ok_and(|path| path.is_file());
    let destination = selected
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unavailable: {error}"));
    lines.extend([
        Line::raw(""),
        Line::raw(fit_command_palette_row("Destination:", line_width)),
        Line::raw(fit_command_palette_row(
            &format!("  {destination}"),
            line_width,
        )),
        Line::raw(""),
        Line::raw(fit_command_palette_row(
            if selected_exists {
                "[Enter] Open"
            } else {
                "[i] Create & Open"
            },
            line_width,
        )),
        Line::raw(fit_command_palette_row(
            "[↑/↓ or j/k] Select   [Esc] Close",
            line_width,
        )),
    ]);
    if let Some(error) = modal.error.as_deref() {
        for line in error.lines() {
            lines.push(Line::styled(
                fit_command_palette_row(line, line_width),
                Style::default().fg(Color::Red),
            ));
        }
    }

    let paragraph = Paragraph::new(Text::from(lines)).block(
        Block::default()
            .title(" Open Source ")
            .borders(Borders::ALL),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn render_command_palette(
    frame: &mut Frame,
    app: &WatchApp,
    palette: &CommandPalette,
    workspace_available: bool,
) {
    let actions = palette.filtered_actions();
    let layout = command_palette_layout(frame.area(), actions.len(), palette.selected_index());
    let block = Block::default()
        .title(" Command Palette ")
        .borders(Borders::ALL);

    frame.render_widget(Clear, layout.area);
    frame.render_widget(block, layout.area);
    frame.render_widget(Clear, layout.inner_area);
    frame.render_widget(
        Paragraph::new(fit_command_palette_row(
            &format!("> {}", palette.query),
            usize::from(layout.query_area.width),
        )),
        layout.query_area,
    );

    if actions.is_empty() {
        if layout.list_area.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::styled(
                    fit_command_palette_row("  No matching commands", layout.list_width),
                    Style::default().fg(Color::DarkGray),
                )),
                layout.list_area,
            );
        }
    } else {
        let scrollbar = layout.show_scrollbar.then(|| {
            VerticalScrollbarGeometry::new(
                u16::try_from(layout.command_capacity).unwrap_or(u16::MAX),
                actions.len().saturating_sub(layout.command_capacity),
                layout.viewport.start,
                layout.command_capacity,
            )
            .expect("overflowing command viewport must have scrollbar geometry")
        });

        let mut lines = Vec::with_capacity(layout.viewport.len());
        for index in layout.viewport.clone() {
            let action = actions[index];
            let availability = action.availability(app, workspace_available);
            let marker = if palette.is_selected(index) { ">" } else { " " };
            let row =
                command_palette_row(marker, action.label(), action.shortcut(), layout.list_width);

            let mut style = match availability {
                FrontendActionAvailability::Available => Style::default(),
                FrontendActionAvailability::Unavailable(_) => Style::default().fg(Color::DarkGray),
            };
            if palette.is_selected(index) {
                style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
            }
            lines.push(Line::styled(row, style));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), layout.list_area);

        if let (Some(scrollbar), Some(gutter)) = (scrollbar, layout.scrollbar_gutter) {
            let scrollbar_lines = (0..gutter.height)
                .map(|row| Line::raw(scrollbar.symbol_at(row)))
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(Text::from(scrollbar_lines)),
                Rect::new(gutter.x, gutter.y, 1, gutter.height),
            );
        }
    }
    let status = match palette
        .selected_action()
        .map(|action| action.availability(app, workspace_available))
    {
        Some(FrontendActionAvailability::Unavailable(reason)) => {
            format!("  Unavailable: {reason}")
        }
        Some(FrontendActionAvailability::Available) | None => String::new(),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit_command_palette_row(&status, usize::from(layout.status_area.width)),
            Style::default().fg(Color::DarkGray),
        )),
        layout.status_area,
    );
    frame.render_widget(
        Paragraph::new(fit_command_palette_row(
            "[↑↓] Select   [Enter] Run   [Esc] Cancel",
            usize::from(layout.help_area.width),
        )),
        layout.help_area,
    );
}

fn render_switch_contest_modal(frame: &mut Frame, modal: &SwitchContestModal) {
    let frame_area = frame.area();
    let width = frame_area.width.min(76);
    let height = frame_area.height.min(16);
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    );

    let destination = modal
        .destination
        .as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    let mut lines = vec![
        Line::raw("Contest:"),
        Line::from(vec![Span::raw("> "), Span::raw(modal.contest_id.clone())]),
        Line::raw(""),
        Line::raw("Destination:"),
        Line::raw(format!("  {destination}")),
    ];

    match modal.state {
        SwitchContestModalState::Input => {
            if modal.target == Some(super::ContestSwitchTarget::Missing) {
                lines.push(Line::raw(""));
                lines.push(Line::raw("Contest does not exist."));
            } else if modal.target == Some(super::ContestSwitchTarget::RepairRequired) {
                lines.push(Line::raw(""));
                lines.push(Line::raw("Contest data requires repair."));
            }
            if let Some(error) = modal.error.as_deref() {
                lines.push(Line::styled(
                    error.to_string(),
                    Style::default().fg(Color::Red),
                ));
            }
            lines.push(Line::raw(""));
            let action = match modal.target {
                Some(super::ContestSwitchTarget::Missing) => {
                    "[Enter] Create & Switch   [Esc] Cancel"
                }
                Some(super::ContestSwitchTarget::RepairRequired) => {
                    "[Enter] Repair & Switch   [Esc] Cancel"
                }
                Some(super::ContestSwitchTarget::Existing) => "[Enter] Switch   [Esc] Cancel",
                None => "[Esc] Cancel",
            };
            lines.push(Line::raw(action));
        }
        SwitchContestModalState::Creating | SwitchContestModalState::Repairing => {
            let (verb, noun) = if modal.state == SwitchContestModalState::Repairing {
                ("Repairing", "Repair")
            } else {
                ("Creating", "Creation")
            };
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!("{verb} {}", modal.contest_id)));
            append_recent_contest_progress(
                &mut lines,
                &modal.progress,
                5,
                usize::from(width.saturating_sub(2)),
            );
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!(
                "{noun} is running and cannot be cancelled."
            )));
        }
        SwitchContestModalState::Failed => {
            lines.push(Line::raw(""));
            let failure = match modal.mutation {
                Some(super::ContestSwitchMutation::Repair) => "Repair & Switch failed:",
                _ => "Create & Switch failed:",
            };
            lines.push(Line::styled(failure, Style::default().fg(Color::Red)));
            if let Some(error) = modal.error.as_deref() {
                lines.push(Line::styled(
                    error.to_string(),
                    Style::default().fg(Color::Red),
                ));
            }
            lines.push(Line::raw(""));
            lines.push(Line::raw("[Enter] Retry   [Esc] Dismiss"));
        }
    }
    let text = Text::from(lines);
    let modal = Paragraph::new(text)
        .block(
            Block::default()
                .title(" Switch Contest ")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(Clear, area);
    frame.render_widget(modal, area);
}

fn append_recent_contest_progress(
    lines: &mut Vec<Line<'static>>,
    progress: &[super::ContestOperationProgress],
    capacity: usize,
    width: usize,
) {
    lines.extend(
        progress
            .iter()
            .rev()
            .take(capacity)
            .rev()
            .map(|progress| Line::raw(fit_command_palette_row(&progress.display_line(), width))),
    );
}

fn render_refresh_contest_modal(frame: &mut Frame, modal: &RefreshContestModal) {
    let frame_area = frame.area();
    let width = frame_area.width.min(76);
    let height = frame_area.height.min(14);
    let area = Rect::new(
        frame_area
            .x
            .saturating_add(frame_area.width.saturating_sub(width) / 2),
        frame_area
            .y
            .saturating_add(frame_area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    let line_width = usize::from(width.saturating_sub(2));
    let mut lines = vec![Line::raw(fit_command_palette_row(
        &format!("Contest: {}", modal.contest_id),
        line_width,
    ))];

    match modal.state {
        RefreshContestModalState::Running => {
            lines.push(Line::raw(""));
            append_recent_contest_progress(&mut lines, &modal.progress, 7, line_width);
            lines.push(Line::raw(""));
            lines.push(Line::raw(fit_command_palette_row(
                "Refresh is running and cannot be cancelled.",
                line_width,
            )));
        }
        RefreshContestModalState::Failed => {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Refresh failed:",
                Style::default().fg(Color::Red),
            ));
            if let Some(error) = modal.error.as_deref() {
                lines.push(Line::styled(
                    fit_command_palette_row(error, line_width),
                    Style::default().fg(Color::Red),
                ));
            }
            lines.push(Line::raw(""));
            lines.push(Line::raw(fit_command_palette_row(
                "[Enter] Retry   [Esc] Close",
                line_width,
            )));
        }
    }

    let paragraph = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(" Refresh Contest ")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
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
    UserInputsHeader,
    UserInput { id: u64, number: usize, dirty: bool },
    Draft,
    UserInputError,
}

impl SampleRow {
    fn selection(self) -> Option<CaseSelection> {
        match self {
            Self::Sample { flat_index, .. } | Self::Stress { flat_index } => {
                Some(CaseSelection::Test(flat_index))
            }
            Self::UserInput { id, .. } => {
                Some(CaseSelection::UserInput(UserInputSelection::Persisted(id)))
            }
            Self::Draft => Some(CaseSelection::UserInput(UserInputSelection::Draft)),
            Self::Blank | Self::StressHeader | Self::UserInputsHeader | Self::UserInputError => {
                None
            }
        }
    }
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

    match &problem.user_inputs {
        super::app::UserInputState::Ready(ready) => {
            let dirty_target = ready
                .edit()
                .filter(|_| ready.edit_is_dirty() == Some(true))
                .map(|edit| edit.target());
            let has_draft = ready
                .edit()
                .is_some_and(|edit| edit.target() == UserInputEditTarget::Draft);
            if !ready.persisted().is_empty() || has_draft {
                if !rows.is_empty() {
                    rows.push(SampleRow::Blank);
                }
                rows.push(SampleRow::UserInputsHeader);
                rows.extend(
                    ready
                        .persisted()
                        .iter()
                        .enumerate()
                        .map(|(position, input)| SampleRow::UserInput {
                            id: input.id,
                            number: position.saturating_add(1),
                            dirty: dirty_target == Some(UserInputEditTarget::Persisted(input.id)),
                        }),
                );
                if has_draft {
                    rows.push(SampleRow::Draft);
                }
            }
        }
        super::app::UserInputState::Error { .. } => {
            if !rows.is_empty() {
                rows.push(SampleRow::Blank);
            }
            rows.push(SampleRow::UserInputsHeader);
            rows.push(SampleRow::UserInputError);
        }
    }

    rows
}

#[cfg(test)]
fn samples_text(app: &WatchApp, height: u16) -> Text<'static> {
    samples_content(app, Rect::new(0, 0, SAMPLES_PANE_WIDTH - 1, height)).0
}

fn samples_content(app: &WatchApp, body: Rect) -> (Text<'static>, Vec<CasesRowTarget>) {
    let Some(problem) = app.current_problem() else {
        return (Text::default(), Vec::new());
    };
    let mut targets = Vec::new();

    let mut lines = if problem.sample_cases > 0 {
        vec![
            Line::styled("Samples", Style::default().add_modifier(Modifier::BOLD)),
            Line::from(""),
        ]
    } else {
        Vec::new()
    };

    let rows = sample_rows(problem);
    let visible = usize::from(body.height).saturating_sub(lines.len());
    let selection = (problem.detail_mode == DetailMode::Samples)
        .then(|| app.case_selection())
        .flatten();
    let selected_row = rows
        .iter()
        .position(|row| row.selection() == selection && row.selection().is_some())
        .unwrap_or(0);
    let range = sample_window(rows.len(), selected_row, visible);
    let selected_test = selection.and_then(|selection| match selection {
        CaseSelection::Test(index) => Some(index),
        CaseSelection::UserInput(_) => None,
    });

    for row in &rows[range] {
        let y = body.y.saturating_add(lines.len() as u16);
        match *row {
            SampleRow::Sample { flat_index, number } => {
                lines.push(sample_line(problem, flat_index, number, selected_test));
            }
            SampleRow::Blank => lines.push(Line::from("")),
            SampleRow::StressHeader => lines.push(Line::styled(
                "Stress",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            SampleRow::Stress { flat_index } => {
                lines.push(sample_line(problem, flat_index, 1, selected_test));
            }
            SampleRow::UserInputsHeader => lines.push(Line::styled(
                "User Inputs",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            SampleRow::UserInput { id, number, dirty } => {
                let mut line = user_input_line(
                    &format!("Input {number}{}", if dirty { " *" } else { "" }),
                    selection == Some(CaseSelection::UserInput(UserInputSelection::Persisted(id))),
                );
                let offset = line.width().saturating_add(2);
                // Reserve the full armed width so a second click never shifts the hitbox.
                // Hide instead of clipping the label or writing into the pane border.
                if app.can_delete_user_input(id) && offset + 4 <= usize::from(body.width) {
                    let armed = problem.user_input_delete_armed == Some(id);
                    targets.push(CasesRowTarget {
                        action: CasesRowAction::DeleteUserInput(id),
                        area: Rect::new(body.x + offset as u16, y, 4, 1),
                        problem: app.selected_problem().unwrap(),
                        revision: app.detail_revision(),
                    });
                    line.spans.push(Span::raw("  "));
                    line.spans.push(Span::styled(
                        if armed { " ×? " } else { " ×  " },
                        Style::default().fg(if armed {
                            Color::Yellow
                        } else {
                            Color::DarkGray
                        }),
                    ));
                }
                lines.push(line);
            }
            SampleRow::Draft => lines.push(user_input_line(
                "Draft *",
                selection == Some(CaseSelection::UserInput(UserInputSelection::Draft)),
            )),
            SampleRow::UserInputError => lines.push(Line::styled(
                "  ! Failed to load",
                Style::default().fg(Color::Red),
            )),
        }
        if let Some(selection) = row.selection() {
            let line_width = lines
                .last()
                .map_or(0, Line::width)
                .min(usize::from(body.width));
            // The label target ends before the Delete padding; targets never overlap.
            let width = targets
                .last()
                .filter(|target| {
                    target.area.y == y
                        && matches!(target.action, CasesRowAction::DeleteUserInput(_))
                })
                .map_or(line_width as u16, |target| target.area.x - body.x - 2);
            if width > 0 {
                targets.push(CasesRowTarget {
                    action: CasesRowAction::Select(selection),
                    area: Rect::new(body.x, y, width, 1),
                    problem: app.selected_problem().unwrap(),
                    revision: app.detail_revision(),
                });
            }
        }
    }

    (Text::from(lines), targets)
}

fn user_input_line(label: &str, selected: bool) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    let style = if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::styled(format!("{marker} {label}"), style)
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
    use crate::tui::app::{PersistedUserInputState, UserInputReadyState, UserInputState};
    use crate::tui::detail_layout::{max_scroll, viewport_text, wrap_detail_document};
    use crate::tui::message::{StressEvent, TestEvent};
    use crate::tui::mouse::{PixelCoordinateOrigin, TerminalPixelMetrics};
    use ratatui::{Terminal, backend::TestBackend};
    use std::fs;
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
                    sample_count: 1,
                }],
            },
            vec![1],
        )
        .unwrap()
    }

    fn app_with_user_inputs(sample_count: usize, user_inputs: UserInputState) -> WatchApp {
        WatchApp::new_with_session_data(
            &Contest {
                contest_id: "abc123".to_string(),
                problems: vec![Problem {
                    index: "A".to_string(),
                    title: "Problem A".to_string(),
                    task_id: "abc123_a".to_string(),
                    url: "https://example.invalid/a".to_string(),
                    sample_count,
                }],
            },
            vec![sample_count],
            vec![None],
            vec![user_inputs],
        )
        .unwrap()
    }

    fn user_input_detail_app() -> WatchApp {
        app_with_user_inputs(
            0,
            UserInputState::loaded(vec![PersistedUserInputState {
                id: 3,
                content: "user input body".to_string(),
            }]),
        )
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

    fn render_with_pointer_position(
        app: &WatchApp,
        pointer: Option<(u16, u16)>,
        width: u16,
        height: u16,
    ) -> (ratatui::buffer::Buffer, RenderInfo) {
        render_with_pointer_and_animation(app, pointer, None, width, height)
    }

    fn render_with_pointer_and_animation(
        app: &WatchApp,
        pointer: Option<(u16, u16)>,
        animation: Option<DetailFoldAnimationFrame>,
        width: u16,
        height: u16,
    ) -> (ratatui::buffer::Buffer, RenderInfo) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut info = RenderInfo::default();
        let mut detail_layout = DetailLayout::default();

        terminal
            .draw(|frame| {
                info = render_frontend_with_pointer(
                    frame,
                    app,
                    &mut detail_layout,
                    MouseMode::Cells,
                    pointer,
                    animation,
                    false,
                    FrontendOverlays::default(),
                );
            })
            .unwrap();

        (terminal.backend().buffer().clone(), info)
    }

    fn buffer_symbols(buffer: &ratatui::buffer::Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
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

    fn rendered_frontend_text(
        app: &WatchApp,
        switch_available: bool,
        modal: Option<&SwitchContestModal>,
    ) -> String {
        rendered_frontend_text_with_palette(app, switch_available, modal, None, 100, 20)
    }

    fn rendered_frontend_text_with_palette(
        app: &WatchApp,
        switch_available: bool,
        modal: Option<&SwitchContestModal>,
        palette: Option<&CommandPalette>,
        width: u16,
        height: u16,
    ) -> String {
        let buffer = rendered_frontend_buffer_with_palette(
            app,
            switch_available,
            modal,
            palette,
            width,
            height,
        );
        let mut text = String::new();
        for row in 0..height {
            for column in 0..width {
                text.push_str(buffer.cell((column, row)).unwrap().symbol());
            }
            text.push('\n');
        }
        text
    }

    fn rendered_open_source_text(
        app: &WatchApp,
        modal: &OpenSourceModal,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut detail_layout = DetailLayout::default();
        terminal
            .draw(|frame| {
                render_frontend_with_mouse_mode(
                    frame,
                    app,
                    &mut detail_layout,
                    MouseMode::Cells,
                    false,
                    FrontendOverlays {
                        source_modal: Some(modal),
                        ..FrontendOverlays::default()
                    },
                );
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

    fn rendered_editor_target_text(
        app: &WatchApp,
        modal: &EditorTargetModal,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut detail_layout = DetailLayout::default();
        terminal
            .draw(|frame| {
                render_frontend_with_mouse_mode(
                    frame,
                    app,
                    &mut detail_layout,
                    MouseMode::Cells,
                    false,
                    FrontendOverlays {
                        editor_target_modal: Some(modal),
                        ..FrontendOverlays::default()
                    },
                );
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

    fn rendered_refresh_text(
        app: &WatchApp,
        modal: &RefreshContestModal,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut detail_layout = DetailLayout::default();
        terminal
            .draw(|frame| {
                render_frontend_with_mouse_mode(
                    frame,
                    app,
                    &mut detail_layout,
                    MouseMode::Cells,
                    false,
                    FrontendOverlays {
                        refresh_modal: Some(modal),
                        ..FrontendOverlays::default()
                    },
                );
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

    fn rendered_frontend_buffer_with_palette(
        app: &WatchApp,
        switch_available: bool,
        modal: Option<&SwitchContestModal>,
        palette: Option<&CommandPalette>,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut detail_layout = DetailLayout::default();
        terminal
            .draw(|frame| {
                render_frontend_with_mouse_mode(
                    frame,
                    app,
                    &mut detail_layout,
                    MouseMode::Cells,
                    switch_available,
                    FrontendOverlays {
                        switch_modal: modal,
                        refresh_modal: None,
                        source_modal: None,
                        editor_target_modal: None,
                        command_palette: palette,
                    },
                );
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_row_text(buffer: &ratatui::buffer::Buffer, x: u16, y: u16, width: usize) -> String {
        (0..width)
            .map(|offset| {
                buffer
                    .cell((x.saturating_add(offset as u16), y))
                    .unwrap()
                    .symbol()
            })
            .collect()
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

    fn semantic_sample_app(debug: bool) -> WatchApp {
        let mut app = app();
        if debug {
            app.toggle_debug();
        }
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let request = app.queue_run(0).unwrap();
        assert_eq!(request.debug, debug);
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseComparison {
                number: 1,
                input: "sample input\n".to_string(),
                expected: "expected\n".to_string(),
                actual: "actual\n".to_string(),
            },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseStderr {
                number: 1,
                stderr: "stderr\n".to_string(),
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
        assert!(rendered_buffer_text(&app, 120, 20).contains(": commands"));
        assert!(!rendered_buffer_text(&app, 120, 20).contains("i initialize"));

        assert!(app.set_stress_setup_required(0, true, true));
        assert!(rendered_buffer_text(&app, 120, 20).contains("i initialize"));

        assert!(app.set_stress_setup_initialized(0));
        assert!(!rendered_buffer_text(&app, 120, 20).contains("i initialize"));

        assert!(app.set_stress_setup_error(0, "invalid target".to_string()));
        assert!(!rendered_buffer_text(&app, 120, 20).contains("i initialize"));
    }

    #[test]
    fn command_palette_renders_query_selection_shortcuts_and_dedicated_unavailable_status() {
        let mut active_app = app();
        let app = app();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "sw".to_string();

        let rendered =
            rendered_frontend_text_with_palette(&app, false, None, Some(&palette), 100, 20);
        assert!(rendered.contains("Command Palette"));
        assert!(rendered.contains("> sw"));
        assert!(rendered.contains("> Switch Contest"));
        assert!(rendered.contains(" c"));
        assert!(!rendered.contains("unavailable —"));
        assert!(rendered.contains("Unavailable: not in a workspace"));
        assert!(rendered.contains("[↑↓] Select"));

        let buffer =
            rendered_frontend_buffer_with_palette(&app, false, None, Some(&palette), 100, 20);
        assert!(buffer.content().iter().any(|cell| {
            cell.symbol() == "S"
                && cell.fg == Color::DarkGray
                && cell.modifier.contains(Modifier::REVERSED)
        }));

        palette.query = "str".to_string();
        let rendered =
            rendered_frontend_text_with_palette(&app, true, None, Some(&palette), 100, 20);
        assert!(rendered.contains("Start Stress"));
        assert!(rendered.contains("Stop Stress"));
        assert!(rendered.contains("Initialize Stress"));
        assert!(!rendered.contains("unavailable —"));
        assert!(rendered.contains("Unavailable: no source file"));

        palette.selected = 2;
        let rendered =
            rendered_frontend_text_with_palette(&app, true, None, Some(&palette), 100, 20);
        assert!(rendered.contains("Unavailable: stress initialization not required"));

        palette.query = "stop".to_string();
        palette.selected = 0;
        let rendered =
            rendered_frontend_text_with_palette(&app, true, None, Some(&palette), 100, 20);
        assert!(rendered.contains("> Stop Stress"));
        assert!(rendered.contains("Unavailable: stress is not running"));
        let unavailable =
            rendered_frontend_buffer_with_palette(&app, true, None, Some(&palette), 100, 20);
        assert!(unavailable.content().iter().any(|cell| {
            cell.symbol() == "S"
                && cell.fg == Color::DarkGray
                && cell.modifier.contains(Modifier::REVERSED)
        }));

        assert!(active_app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        assert!(active_app.queue_stress(0, 123).is_some());
        let rendered =
            rendered_frontend_text_with_palette(&active_app, true, None, Some(&palette), 100, 20);
        assert!(rendered.contains("> Stop Stress"));
        assert!(!rendered.contains("Unavailable:"));
        let available =
            rendered_frontend_buffer_with_palette(&active_app, true, None, Some(&palette), 100, 20);
        assert!(available.content().iter().any(|cell| {
            cell.symbol() == "S"
                && cell.fg != Color::DarkGray
                && cell.modifier.contains(Modifier::REVERSED)
        }));
    }

    #[test]
    fn command_palette_renders_zero_results_and_is_safe_in_small_frames() {
        let app = app();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "nope".to_string();

        let rendered =
            rendered_frontend_text_with_palette(&app, false, None, Some(&palette), 100, 20);
        assert!(rendered.contains("> nope"));
        assert!(rendered.contains("No matching commands"));

        for (width, height) in [(0, 0), (1, 1), (2, 2), (3, 2), (5, 3), (5, 12), (80, 3)] {
            rendered_frontend_buffer_with_palette(&app, false, None, Some(&palette), width, height);
        }
    }

    #[test]
    fn command_palette_status_row_is_reserved_and_tracks_only_selected_availability() {
        let app = app();
        let frame = Rect::new(0, 0, 76, 20);
        let mut palette = CommandPalette::default();
        palette.open();
        let action_count = palette.filtered_actions().len();

        let unavailable_layout =
            command_palette_layout(frame, action_count, palette.selected_index());
        let unavailable_buffer = rendered_frontend_buffer_with_palette(
            &app,
            false,
            None,
            Some(&palette),
            frame.width,
            frame.height,
        );
        let status_y = unavailable_layout.status_area.y;
        let status_x = unavailable_layout.status_area.x;
        let status_width = usize::from(unavailable_layout.status_area.width);
        let unavailable_status =
            buffer_row_text(&unavailable_buffer, status_x, status_y, status_width);
        assert!(unavailable_status.contains("Unavailable: no source file"));
        assert!((0..status_width).all(|offset| {
            unavailable_buffer
                .cell((status_x.saturating_add(offset as u16), status_y))
                .unwrap()
                .fg
                == Color::DarkGray
        }));

        assert!(palette.select_next());
        let available_layout =
            command_palette_layout(frame, action_count, palette.selected_index());
        let available_buffer = rendered_frontend_buffer_with_palette(
            &app,
            false,
            None,
            Some(&palette),
            frame.width,
            frame.height,
        );
        let available_status = buffer_row_text(&available_buffer, status_x, status_y, status_width);
        assert!(available_status.trim().is_empty());
        assert_eq!(available_layout.area, unavailable_layout.area);
        assert_eq!(
            available_layout.command_capacity,
            unavailable_layout.command_capacity
        );
    }

    #[test]
    fn command_palette_status_row_truncates_safely_in_narrow_frames() {
        let app = app();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "ini".to_string();
        let frame = Rect::new(0, 0, 24, 20);
        let layout = command_palette_layout(frame, 1, 0);
        let buffer = rendered_frontend_buffer_with_palette(
            &app,
            false,
            None,
            Some(&palette),
            frame.width,
            frame.height,
        );
        let status_y = layout.status_area.y;
        let status_width = usize::from(layout.status_area.width);
        let status = buffer_row_text(&buffer, layout.status_area.x, status_y, status_width);
        assert_eq!(UnicodeWidthStr::width(status.as_str()), status_width);
        assert!(status.contains("Unavailable:"));
        assert!(status.contains('…'));
        assert!(!status.contains("unavailable —"));
    }

    #[test]
    fn command_palette_viewport_follows_selection_without_stored_scroll_state() {
        assert_eq!(command_palette_viewport(3, 0, 6), 0..3);
        assert_eq!(command_palette_viewport(10, 0, 6), 0..6);
        assert_eq!(command_palette_viewport(10, 5, 6), 0..6);
        assert_eq!(command_palette_viewport(10, 6, 6), 1..7);
        assert_eq!(command_palette_viewport(10, 7, 6), 2..8);
        assert_eq!(command_palette_viewport(10, 9, 6), 4..10);
        assert_eq!(command_palette_viewport(10, 9, 0), 0..0);

        let wrapped_to_last = command_palette_viewport(10, 9, 4);
        assert!(wrapped_to_last.contains(&9));
        let after_resize = command_palette_viewport(10, 7, 3);
        assert_eq!(after_resize, 5..8);
        assert!(after_resize.contains(&7));
    }

    #[test]
    fn command_palette_height_tracks_content_and_caps_large_lists() {
        assert_eq!(COMMAND_PALETTE_FIXED_HEIGHT, 7);
        let frame = Rect::new(0, 0, 100, 40);
        let few = command_palette_layout(frame, 3, 0);
        assert_eq!(few.area.height, COMMAND_PALETTE_FIXED_HEIGHT + 3);
        assert_eq!(few.command_capacity, 3);
        assert!(!few.show_scrollbar);

        let many = command_palette_layout(frame, 30, 0);
        assert_eq!(
            many.area.height,
            COMMAND_PALETTE_FIXED_HEIGHT + COMMAND_PALETTE_MAX_VISIBLE_COMMANDS as u16
        );
        assert_eq!(many.command_capacity, COMMAND_PALETTE_MAX_VISIBLE_COMMANDS);
        assert_eq!(many.viewport, 0..COMMAND_PALETTE_MAX_VISIBLE_COMMANDS);
        assert!(many.show_scrollbar);
        assert_eq!(many.command_area.height, 10);
        assert_eq!(many.list_area.height, many.command_area.height);
        assert_eq!(many.scrollbar_gutter.unwrap().height, 10);
        assert_eq!(many.status_area.y, many.command_area.bottom());
        assert_eq!(many.help_area.y, many.status_area.y.saturating_add(2));

        let small = command_palette_layout(Rect::new(0, 0, 40, 9), 30, 12);
        assert_eq!(small.area.height, 9);
        assert_eq!(small.command_capacity, 2);
        assert!(small.viewport.contains(&12));
        assert!(small.show_scrollbar);

        let tiny = command_palette_layout(Rect::new(0, 0, 1, 1), 30, 29);
        assert_eq!(tiny.command_capacity, 0);
        assert_eq!(tiny.viewport, 0..0);
        assert!(!tiny.show_scrollbar);
    }

    #[test]
    fn command_palette_overflow_clips_rows_and_renders_shared_scrollbar_glyphs() {
        let app = app();
        let mut palette = CommandPalette::default();
        palette.open();
        let frame = Rect::new(0, 0, 100, 11);
        let action_count = palette.filtered_actions().len();
        let layout = command_palette_layout(frame, action_count, palette.selected_index());
        assert_eq!(layout.command_capacity, 4);
        assert!(layout.show_scrollbar);

        let buffer = rendered_frontend_buffer_with_palette(
            &app,
            false,
            None,
            Some(&palette),
            frame.width,
            frame.height,
        );
        let command_y = layout.command_area.y;
        let scrollbar_x = layout.scrollbar_gutter.unwrap().x;
        assert_eq!(buffer.cell((scrollbar_x, command_y)).unwrap().symbol(), "↑");
        assert_eq!(
            buffer
                .cell((
                    scrollbar_x,
                    command_y.saturating_add(layout.command_capacity as u16 - 1),
                ))
                .unwrap()
                .symbol(),
            "↓"
        );
        assert!((0..layout.command_capacity).any(|row| {
            buffer
                .cell((scrollbar_x, command_y.saturating_add(row as u16)))
                .unwrap()
                .symbol()
                == "█"
        }));

        for expected in 1..=5 {
            assert!(palette.select_next());
            assert_eq!(palette.selected_index(), expected);
            let rendered = rendered_frontend_text_with_palette(
                &app,
                false,
                None,
                Some(&palette),
                frame.width,
                frame.height,
            );
            assert!(rendered.contains(palette.selected_action().unwrap().label()));
        }
        let shifted = command_palette_layout(frame, action_count, palette.selected_index());
        assert_eq!(shifted.viewport, 2..6);
        let rendered = rendered_frontend_text_with_palette(
            &app,
            false,
            None,
            Some(&palette),
            frame.width,
            frame.height,
        );
        assert!(rendered.contains("Open Template"));
        assert!(!rendered.contains("Run Tests"));

        palette.selected = 0;
        assert!(palette.select_previous());
        assert_eq!(palette.selected_index(), action_count - 1);
        let wrapped = command_palette_layout(frame, action_count, palette.selected_index());
        assert_eq!(
            wrapped.viewport,
            action_count - layout.command_capacity..action_count
        );
        let rendered = rendered_frontend_text_with_palette(
            &app,
            false,
            None,
            Some(&palette),
            frame.width,
            frame.height,
        );
        assert!(rendered.contains("Switch Contest"));

        palette.query = "str".to_string();
        palette.reset_selection();
        let filtered = command_palette_layout(frame, 3, palette.selected_index());
        assert_eq!(filtered.viewport, 0..3);
        assert!(!filtered.show_scrollbar);
        let rendered = rendered_frontend_text_with_palette(
            &app,
            false,
            None,
            Some(&palette),
            frame.width,
            frame.height,
        );
        assert!(rendered.contains("> Start Stress"));
        assert!(rendered.contains("Stop Stress"));
        assert!(rendered.contains("Initialize Stress"));
    }

    #[test]
    fn command_palette_scrollbar_is_contained_and_preserves_every_modal_border() {
        let app = app();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = " ".repeat(200);

        for (width, height) in [(76, 11), (30, 10), (24, 11)] {
            let frame = Rect::new(0, 0, width, height);
            let layout = command_palette_layout(
                frame,
                palette.filtered_actions().len(),
                palette.selected_index(),
            );
            assert!(layout.show_scrollbar, "{width}x{height}");
            let gutter = layout.scrollbar_gutter.unwrap();
            let buffer = rendered_frontend_buffer_with_palette(
                &app,
                false,
                None,
                Some(&palette),
                width,
                height,
            );

            let right_border = layout.area.right().saturating_sub(1);
            let bottom_border = layout.area.bottom().saturating_sub(1);
            assert_eq!(gutter.right(), right_border);
            assert_eq!(gutter.y, layout.command_area.y);
            assert_eq!(gutter.height, layout.command_area.height);
            assert_eq!(
                buffer
                    .cell((layout.area.x, layout.area.y))
                    .unwrap()
                    .symbol(),
                "┌"
            );
            assert_eq!(
                buffer.cell((right_border, layout.area.y)).unwrap().symbol(),
                "┐"
            );
            assert_eq!(
                buffer
                    .cell((layout.area.x, bottom_border))
                    .unwrap()
                    .symbol(),
                "└"
            );
            assert_eq!(
                buffer.cell((right_border, bottom_border)).unwrap().symbol(),
                "┘"
            );
            for x in layout.area.x.saturating_add(1)..right_border {
                assert_eq!(buffer.cell((x, bottom_border)).unwrap().symbol(), "─");
            }
            for y in layout.area.y.saturating_add(1)..bottom_border {
                assert_eq!(buffer.cell((right_border, y)).unwrap().symbol(), "│");
            }

            let symbol_x = gutter.x;
            let border_gap_x = gutter.x.saturating_add(1);
            assert_eq!(
                buffer.cell((symbol_x, layout.area.y)).unwrap().symbol(),
                "─"
            );
            assert_eq!(
                buffer.cell((border_gap_x, layout.area.y)).unwrap().symbol(),
                "─"
            );
            assert_eq!(
                buffer
                    .cell((symbol_x, layout.command_area.y))
                    .unwrap()
                    .symbol(),
                "↑"
            );
            assert_eq!(
                buffer
                    .cell((symbol_x, layout.command_area.bottom().saturating_sub(1),))
                    .unwrap()
                    .symbol(),
                "↓"
            );
            assert!(
                (layout.command_area.y..layout.command_area.bottom())
                    .any(|y| { buffer.cell((symbol_x, y)).unwrap().symbol() == "█" })
            );
            for y in layout.command_area.y..layout.command_area.bottom() {
                let gap = buffer.cell((border_gap_x, y)).unwrap();
                assert_eq!(gap.symbol(), " ");
                assert!(!gap.modifier.contains(Modifier::REVERSED));
            }

            for area in [layout.query_area, layout.status_area, layout.help_area] {
                if area.height == 0 {
                    continue;
                }
                assert!(
                    !["↑", "↓", "█"].contains(&buffer.cell((symbol_x, area.y)).unwrap().symbol())
                );
            }
            assert!(layout.command_area.bottom() <= layout.status_area.y);
            assert!(layout.help_area.bottom() <= bottom_border);

            let rendered = rendered_frontend_text_with_palette(
                &app,
                false,
                None,
                Some(&palette),
                width,
                height,
            );
            assert!(rendered.contains("Command Palette"));
        }
    }

    #[test]
    fn command_palette_selected_rows_fill_list_width_but_not_scrollbar_gutter() {
        let app = app();
        let selected_width = |query: &str| {
            let mut palette = CommandPalette::default();
            palette.open();
            palette.query = query.to_string();
            let frame = Rect::new(0, 0, 76, 20);
            let layout = command_palette_layout(frame, 1, 0);
            let buffer = rendered_frontend_buffer_with_palette(
                &app,
                false,
                None,
                Some(&palette),
                frame.width,
                frame.height,
            );
            let row = layout.list_area.y;
            let start = layout.list_area.x;
            let reversed = (0..layout.list_width)
                .filter(|offset| {
                    buffer
                        .cell((start.saturating_add(*offset as u16), row))
                        .unwrap()
                        .modifier
                        .contains(Modifier::REVERSED)
                })
                .count();
            (layout.list_width, reversed, buffer, start, row)
        };

        let (available_width, available_reversed, _, _, _) = selected_width("deb");
        let (disabled_width, disabled_reversed, disabled, start, row) = selected_width("tes");
        assert_eq!(available_width, disabled_width);
        assert_eq!(available_reversed, available_width);
        assert_eq!(disabled_reversed, disabled_width);
        for offset in 0..disabled_width {
            assert_eq!(
                disabled
                    .cell((start.saturating_add(offset as u16), row))
                    .unwrap()
                    .fg,
                Color::DarkGray
            );
        }

        let mut palette = CommandPalette::default();
        palette.open();
        let frame = Rect::new(0, 0, 76, 10);
        let action_count = palette.filtered_actions().len();
        let layout = command_palette_layout(frame, action_count, 0);
        let buffer = rendered_frontend_buffer_with_palette(
            &app,
            false,
            None,
            Some(&palette),
            frame.width,
            frame.height,
        );
        let scrollbar_x = layout.scrollbar_gutter.unwrap().x;
        let selected_row = layout.list_area.y;
        assert!(layout.show_scrollbar);
        for x in scrollbar_x..layout.scrollbar_gutter.unwrap().right() {
            assert!(
                !buffer
                    .cell((x, selected_row))
                    .unwrap()
                    .modifier
                    .contains(Modifier::REVERSED)
            );
        }

        assert!(palette.select_next());
        let available_layout =
            command_palette_layout(frame, action_count, palette.selected_index());
        let available_buffer = rendered_frontend_buffer_with_palette(
            &app,
            false,
            None,
            Some(&palette),
            frame.width,
            frame.height,
        );
        let available_row = available_layout
            .list_area
            .y
            .saturating_add((palette.selected_index() - available_layout.viewport.start) as u16);
        assert_eq!(
            (0..available_layout.list_width)
                .filter(|offset| {
                    available_buffer
                        .cell((
                            available_layout.list_area.x.saturating_add(*offset as u16),
                            available_row,
                        ))
                        .unwrap()
                        .modifier
                        .contains(Modifier::REVERSED)
                })
                .count(),
            available_layout.list_width
        );
        let available_gutter = available_layout.scrollbar_gutter.unwrap();
        for x in available_gutter.x..available_gutter.right() {
            assert!(
                !available_buffer
                    .cell((x, available_row))
                    .unwrap()
                    .modifier
                    .contains(Modifier::REVERSED)
            );
        }
    }

    #[test]
    fn command_palette_rows_truncate_unicode_safely_and_prioritize_label() {
        let medium = command_palette_row(">", "Switch Contest", Some("c"), 30);
        assert_eq!(UnicodeWidthStr::width(medium.as_str()), 30);
        assert!(medium.contains("Switch Contest"));
        assert!(medium.contains(" c"));
        assert!(!medium.contains("Unavailable"));

        let narrow = command_palette_row(">", "Run Tests", Some("r"), 10);
        assert_eq!(UnicodeWidthStr::width(narrow.as_str()), 10);
        assert!(narrow.starts_with("> Run"));
        assert!(narrow.contains('…'));

        let no_shortcut = command_palette_row(">", "Stop Stress", None, 30);
        assert_eq!(UnicodeWidthStr::width(no_shortcut.as_str()), 30);
        assert!(no_shortcut.contains("Stop Stress"));
        assert!(!no_shortcut.contains("None"));

        for width in 0..=12 {
            let fitted =
                fit_command_palette_row("  Unavailable: stress initialization not required", width);
            assert_eq!(UnicodeWidthStr::width(fitted.as_str()), width);
        }
    }

    #[test]
    fn workspace_footer_and_switch_modal_render_destination_and_error() {
        let app = app();
        assert!(!rendered_frontend_text(&app, false, None).contains("c contest"));
        assert!(rendered_frontend_text(&app, true, None).contains("c contest"));

        let modal = SwitchContestModal {
            contest_id: "abc467".to_string(),
            destination: Some(PathBuf::from("D:/atcoder/ABC/abc467")),
            error: Some("contest metadata is invalid".to_string()),
            ..SwitchContestModal::default()
        };
        let rendered = rendered_frontend_text(&app, true, Some(&modal));
        assert!(rendered.contains("Switch Contest"));
        assert!(rendered.contains("> abc467"));
        assert!(rendered.contains("D:/atcoder/ABC/abc467"));
        assert!(rendered.contains("contest metadata is invalid"));
        assert!(rendered.contains("[Esc] Cancel"));
        assert!(!rendered.contains("[Enter]"));
    }

    #[test]
    fn open_source_modal_renders_fixed_languages_current_missing_destination_and_actions() {
        let temp = tempfile::tempdir().unwrap();
        let cpp = temp.path().join("A.cpp");
        fs::write(&cpp, "source").unwrap();
        let mut app = app();
        app.source_changed(0, cpp.clone(), Language::Cpp);
        let mut controller = super::super::OpenSourceController::new(temp.path(), Language::Python);
        assert!(controller.open(&app));
        assert_eq!(controller.modal().unwrap().selected_path().unwrap(), cpp);

        let rendered = rendered_open_source_text(&app, controller.modal().unwrap(), 100, 20);
        assert!(rendered.contains("Open Source"));
        assert!(rendered.contains("Problem: A"));
        let cpp_position = rendered.find("C++").unwrap();
        let python_position = rendered.find("Python").unwrap();
        assert!(cpp_position < python_position);
        assert!(rendered.contains("C++       current"));
        assert!(rendered.contains("Python    not created"));
        assert!(rendered.contains("A.cpp"));
        assert!(rendered.contains("[Enter] Open"));

        controller.modal.as_mut().unwrap().selected_language = Language::Python;
        assert_eq!(
            controller.modal().unwrap().selected_path().unwrap(),
            temp.path().join("A.py")
        );
        let rendered = rendered_open_source_text(&app, controller.modal().unwrap(), 100, 20);
        assert!(rendered.contains("A.py"));
        assert!(rendered.contains("[i] Create & Open"));
        assert!(rendered.contains("C++       current"));
    }

    #[test]
    fn open_source_modal_truncates_unicode_destination_and_survives_narrow_tiny_frames() {
        let root = tempfile::tempdir().unwrap();
        let unicode = root
            .path()
            .join("非常に長いコンテスト保存先")
            .join("さらに長いディレクトリ名");
        fs::create_dir_all(&unicode).unwrap();
        let app = app();
        let mut controller = super::super::OpenSourceController::new(&unicode, Language::Python);
        assert!(controller.open(&app));
        controller.modal.as_mut().unwrap().error =
            Some("No editor configured.\nSet VISUAL or EDITOR, or configure [editor].".to_string());
        let actual_path = unicode.join("A.py");

        let narrow = rendered_open_source_text(&app, controller.modal().unwrap(), 32, 16);
        assert!(narrow.contains("Open Source"));
        assert!(narrow.contains("Problem: A"));
        assert!(narrow.contains('…'));
        assert!(!narrow.contains(&actual_path.display().to_string()));

        for (width, height) in [(20, 8), (8, 4), (1, 1), (0, 0)] {
            let _ = rendered_open_source_text(&app, controller.modal().unwrap(), width, height);
        }
    }

    #[test]
    fn editor_target_modals_render_states_actions_and_fixed_template_rows() {
        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("config.toml");
        let templates_dir = temp.path().join("templates");
        fs::write(&config_file, [0xff, 0xfe]).unwrap();
        let mut controller = super::super::EditorTargetController::new(
            temp.path(),
            Language::Python,
            Ok(config_file.clone()),
            Ok(templates_dir.clone()),
            Some(temp.path()),
        );
        let app = app();

        controller.open_settings();
        let EditorTargetModal::Settings(settings) = controller.modal().unwrap() else {
            panic!("expected settings modal");
        };
        assert_eq!(settings.target().unwrap(), config_file);
        let rendered = rendered_editor_target_text(&app, controller.modal().unwrap(), 100, 20);
        assert!(rendered.contains("Open Settings"));
        assert!(rendered.contains("existing"));
        assert!(rendered.contains("Destination:"));
        assert!(rendered.contains("[Enter] Open"));
        fs::remove_file(&config_file).unwrap();
        let rendered = rendered_editor_target_text(&app, controller.modal().unwrap(), 100, 20);
        assert!(rendered.contains("not initialized"));
        assert!(rendered.contains("[i] Initialize & Open"));

        let workspace_file = crate::workspace::workspace_config_path(temp.path());
        fs::write(&workspace_file, "malformed = [\n").unwrap();
        controller.open_workspace_settings();
        let EditorTargetModal::WorkspaceSettings(workspace) = controller.modal().unwrap() else {
            panic!("expected workspace settings modal");
        };
        assert_eq!(workspace.target(), workspace_file);
        let rendered = rendered_editor_target_text(&app, controller.modal().unwrap(), 100, 20);
        assert!(rendered.contains("Open Workspace Settings"));
        assert!(rendered.contains("Destination:"));
        assert!(rendered.contains("[Enter] Open"));
        assert!(!rendered.contains("Initialize & Open"));
        fs::remove_file(&workspace_file).unwrap();
        let rendered = rendered_editor_target_text(&app, controller.modal().unwrap(), 100, 20);
        assert!(rendered.contains("workspace settings file is missing"));
        assert!(!rendered.contains("[Enter] Open"));
        assert!(!rendered.contains("Initialize & Open"));

        fs::create_dir(&templates_dir).unwrap();
        let cpp = crate::template::source_template_path(&templates_dir, Language::Cpp);
        fs::write(&cpp, "custom").unwrap();
        controller.open_template(&app);
        let rendered = rendered_editor_target_text(&app, controller.modal().unwrap(), 100, 20);
        let cpp_position = rendered.find("C++").unwrap();
        let python_position = rendered.find("Python").unwrap();
        assert!(cpp_position < python_position);
        assert!(rendered.contains("Python    not initialized"));
        assert!(rendered.contains("[i] Initialize & Open"));
        assert!(!rendered.contains("Problem:"));
    }

    #[test]
    fn editor_target_modals_truncate_unicode_destinations_and_survive_tiny_frames() {
        let temp = tempfile::tempdir().unwrap();
        let unicode = temp
            .path()
            .join("非常に長いユーザー設定ディレクトリ")
            .join("さらに長い保存先")
            .join("config.toml");
        let mut controller = super::super::EditorTargetController::new(
            temp.path(),
            Language::Cpp,
            Ok(unicode.clone()),
            Ok(temp.path().join("非常に長いテンプレート保存先")),
            Some(temp.path()),
        );
        let app = app();
        controller.open_settings();

        let narrow = rendered_editor_target_text(&app, controller.modal().unwrap(), 32, 12);
        assert!(narrow.contains("Open Settings"));
        assert!(narrow.contains('…'));
        assert!(!narrow.contains(&unicode.display().to_string()));

        for (width, height) in [(20, 8), (8, 4), (1, 1), (0, 0)] {
            let _ = rendered_editor_target_text(&app, controller.modal().unwrap(), width, height);
        }
    }

    #[test]
    fn missing_and_running_switch_modals_explain_confirmation_and_cancellation() {
        let app = app();
        let missing = SwitchContestModal {
            contest_id: "abc470".to_string(),
            destination: Some(PathBuf::from("D:/atcoder/ABC/abc470")),
            target: Some(super::super::ContestSwitchTarget::Missing),
            ..SwitchContestModal::default()
        };

        let rendered = rendered_frontend_text(&app, true, Some(&missing));
        assert!(rendered.contains("Contest does not exist."));
        assert!(rendered.contains("[Enter] Create & Switch"));

        let creating = SwitchContestModal {
            state: SwitchContestModalState::Creating,
            progress: vec![super::super::ContestOperationProgress::ProblemFetching {
                index: "A".to_string(),
                current: 1,
                total: 2,
            }],
            ..missing
        };
        let rendered = rendered_frontend_text(&app, true, Some(&creating));
        assert!(rendered.contains("Creating abc470"));
        assert!(rendered.contains("[1/2] Fetching A..."));
        assert!(rendered.contains("cannot be cancelled"));

        let repair = SwitchContestModal {
            contest_id: "abc470".to_string(),
            destination: Some(PathBuf::from("D:/atcoder/ABC/abc470")),
            target: Some(super::super::ContestSwitchTarget::RepairRequired),
            ..SwitchContestModal::default()
        };
        let rendered = rendered_frontend_text(&app, true, Some(&repair));
        assert!(rendered.contains("Contest data requires repair."));
        assert!(rendered.contains("[Enter] Repair & Switch"));

        let repairing = SwitchContestModal {
            state: SwitchContestModalState::Repairing,
            mutation: Some(super::super::ContestSwitchMutation::Repair),
            progress: vec![super::super::ContestOperationProgress::WorkspaceRepaired {
                destination: PathBuf::from("D:/atcoder/ABC/abc470"),
            }],
            ..repair
        };
        let rendered = rendered_frontend_text(&app, true, Some(&repairing));
        assert!(rendered.contains("Repairing abc470"));
        assert!(rendered.contains("Contest repaired"));
        assert!(rendered.contains("Repair is running and cannot be cancelled."));

        let failed = SwitchContestModal {
            state: SwitchContestModalState::Failed,
            error: Some("network unavailable".to_string()),
            ..repairing
        };
        let rendered = rendered_frontend_text(&app, true, Some(&failed));
        assert!(rendered.contains("Repair & Switch failed:"));
        assert!(rendered.contains("[Enter] Retry"));
    }

    #[test]
    fn refresh_modal_renders_progress_failure_help_and_tiny_frames() {
        let app = app();
        let running = RefreshContestModal {
            contest_id: "abc469".to_string(),
            state: RefreshContestModalState::Running,
            progress: vec![
                super::super::ContestOperationProgress::ContestFetched {
                    contest_id: "abc469".to_string(),
                    problems: 7,
                },
                super::super::ContestOperationProgress::ProblemFetching {
                    index: "C".to_string(),
                    current: 3,
                    total: 7,
                },
            ],
            error: None,
        };
        let rendered = rendered_refresh_text(&app, &running, 100, 20);
        assert!(rendered.contains("Refresh Contest"));
        assert!(rendered.contains("Contest: abc469"));
        assert!(rendered.contains("Found 7 problems in abc469"));
        assert!(rendered.contains("[3/7] Fetching C..."));
        assert!(rendered.contains("cannot be cancelled"));
        assert!(!rendered.contains("Contest refreshed"));

        let failed = RefreshContestModal {
            state: RefreshContestModalState::Failed,
            error: Some("network unavailable ".repeat(30)),
            ..running
        };
        let rendered = rendered_refresh_text(&app, &failed, 48, 14);
        assert!(rendered.contains("Refresh failed:"));
        assert!(rendered.contains("network unavailable"));
        assert!(rendered.contains("[Enter] Retry"));
        assert!(rendered.contains("[Esc] Close"));
        assert!(rendered.contains('…'));

        for (width, height) in [(32, 10), (16, 6), (8, 4), (1, 1), (0, 0)] {
            let _ = rendered_refresh_text(&app, &failed, width, height);
        }
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
    fn semantic_header_hover_highlights_only_the_shared_interactive_row() {
        let app = foldable_app("actual body".to_string());
        let (_, initial) = render_with_pointer_position(&app, None, 100, 40);
        let target = *initial
            .detail_section_headers
            .iter()
            .find(|header| header.kind == DetailSectionKind::Expected)
            .unwrap();
        let pointer = (target.area.right().saturating_sub(1), target.area.y);

        assert_eq!(
            initial.detail_section_header_at(app.detail_revision(), pointer.0, pointer.1),
            Some(target)
        );
        assert!(
            initial
                .detail_section_header_at(app.detail_revision(), target.area.right(), target.area.y)
                .is_none()
        );

        let (hovered, hovered_info) = render_with_pointer_position(&app, Some(pointer), 100, 40);
        let hovered_target = hovered_info
            .detail_section_header_at(app.detail_revision(), pointer.0, pointer.1)
            .unwrap();
        assert_eq!(hovered_target, target);
        assert!(
            (hovered_target.area.x..hovered_target.area.right()).all(|column| {
                hovered.cell((column, hovered_target.area.y)).unwrap().bg == Color::DarkGray
            })
        );

        let (outside, _) =
            render_with_pointer_position(&app, Some((target.area.right(), target.area.y)), 100, 40);
        assert!((target.area.x..target.area.right()).all(|column| {
            outside.cell((column, target.area.y)).unwrap().bg != Color::DarkGray
        }));
    }

    #[test]
    fn semantic_header_hover_survives_folding_at_the_same_pointer_row() {
        let mut app = foldable_app("actual body".to_string());
        let (_, expanded) = render_with_pointer_position(&app, None, 100, 40);
        let expanded_input = *expanded
            .detail_section_headers
            .iter()
            .find(|header| header.kind == DetailSectionKind::Input)
            .unwrap();
        let pointer = (
            expanded_input.area.x.saturating_add(1),
            expanded_input.area.y,
        );
        let (expanded_hover, _) = render_with_pointer_position(&app, Some(pointer), 100, 40);
        assert_eq!(expanded_hover.cell(pointer).unwrap().bg, Color::DarkGray);

        app.toggle_detail_section(DetailSectionKind::Input);
        let (folded_hover, folded) = render_with_pointer_position(&app, Some(pointer), 100, 40);
        let folded_input = folded
            .detail_section_header_at(app.detail_revision(), pointer.0, pointer.1)
            .unwrap();
        assert_eq!(folded_input.kind, DetailSectionKind::Input);
        assert_eq!(folded_input.area.y, expanded_input.area.y);
        assert_eq!(folded_hover.cell(pointer).unwrap().bg, Color::DarkGray);
        assert_eq!(
            folded_hover
                .cell((folded_input.area.x, folded_input.area.y))
                .unwrap()
                .symbol(),
            "▶"
        );
    }

    #[test]
    fn semantic_header_hover_is_shared_by_normal_debug_and_stress_details() {
        for app in [
            semantic_sample_app(false),
            semantic_sample_app(true),
            foldable_app("actual body".to_string()),
            user_input_detail_app(),
        ] {
            let (_, initial) = render_with_pointer_position(&app, None, 100, 40);
            let target = initial.detail_section_headers[0];
            let pointer = (target.area.x, target.area.y);
            let (hovered, _) = render_with_pointer_position(&app, Some(pointer), 100, 40);
            assert_eq!(hovered.cell(pointer).unwrap().bg, Color::DarkGray);
        }
    }

    #[test]
    fn fold_animation_clips_the_same_semantic_bodies_for_test_stress_and_user_input_details() {
        for (app, kind, body) in [
            (
                semantic_sample_app(false),
                DetailSectionKind::Expected,
                "expected",
            ),
            (
                semantic_sample_app(true),
                DetailSectionKind::Input,
                "sample input",
            ),
            (
                foldable_app("actual body".to_string()),
                DetailSectionKind::Input,
                "input body",
            ),
            (
                user_input_detail_app(),
                DetailSectionKind::Input,
                "user input body",
            ),
        ] {
            let hidden_frame = DetailFoldAnimationFrame {
                kind,
                expanded_fraction: 0.0,
            };
            let (hidden, hidden_info) =
                render_with_pointer_and_animation(&app, None, Some(hidden_frame), 100, 40);
            assert!(
                hidden_info
                    .detail_section_headers
                    .iter()
                    .any(|header| header.kind == kind)
            );
            assert!(!buffer_symbols(&hidden).contains(body));

            let shown_frame = DetailFoldAnimationFrame {
                kind,
                expanded_fraction: 1.0,
            };
            let (shown, _) =
                render_with_pointer_and_animation(&app, None, Some(shown_frame), 100, 40);
            assert!(buffer_symbols(&shown).contains(body));
        }
    }

    #[test]
    fn fold_animation_keeps_hover_on_the_current_header_layout() {
        let app = foldable_app("actual body\nsecond row\nthird row".to_string());
        let frame = DetailFoldAnimationFrame {
            kind: DetailSectionKind::Actual,
            expanded_fraction: 0.5,
        };
        let (_, initial) = render_with_pointer_and_animation(&app, None, Some(frame), 100, 40);
        let target = *initial
            .detail_section_headers
            .iter()
            .find(|header| header.kind == DetailSectionKind::Actual)
            .unwrap();
        let pointer = (target.area.right().saturating_sub(1), target.area.y);

        let (hovered, hovered_info) =
            render_with_pointer_and_animation(&app, Some(pointer), Some(frame), 100, 40);

        assert_eq!(
            hovered_info.detail_section_header_at(app.detail_revision(), pointer.0, pointer.1),
            Some(target)
        );
        assert_eq!(hovered.cell(pointer).unwrap().bg, Color::DarkGray);
    }

    #[test]
    fn fold_animation_uses_the_same_wrap_width_as_the_rendered_detail() {
        // A 100-column terminal leaves a 98-column detail pane inside the outer border.
        // With no scrollbar this body is one visual row, so a 50% frame rounds to the
        // complete row rather than clipping its final column.
        let body = "x".repeat(98);
        let app = foldable_app(body.clone());
        let frame = DetailFoldAnimationFrame {
            kind: DetailSectionKind::Actual,
            expanded_fraction: 0.5,
        };

        let (rendered, info) = render_with_pointer_and_animation(&app, None, Some(frame), 100, 40);

        assert!(info.detail_scrollbar.is_none());
        assert!(buffer_symbols(&rendered).contains(&body));
    }

    #[test]
    fn long_fold_animation_limits_scroll_range_and_is_resize_safe() {
        let mut app = foldable_app("actual body\n".repeat(1_000));
        let expanded = render_info(&app, 80, 20).max_detail_scroll.unwrap();
        assert!(expanded > 900);

        let frame = DetailFoldAnimationFrame {
            kind: DetailSectionKind::Actual,
            expanded_fraction: 1.0,
        };
        let (_, animated) = render_with_pointer_and_animation(&app, None, Some(frame), 80, 20);
        let animated_max = animated.max_detail_scroll.unwrap();
        assert!(animated_max < expanded);

        app.scroll_detail_down(usize::MAX);
        for (width, height) in [(1, 1), (2, 3), (20, 5), (100, 40)] {
            let _ = render_with_pointer_and_animation(
                &app,
                None,
                Some(DetailFoldAnimationFrame {
                    kind: DetailSectionKind::Actual,
                    expanded_fraction: 0.5,
                }),
                width,
                height,
            );
        }
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
                    sample_count: 2,
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
    fn user_input_rows_are_sorted_selected_and_rendered_without_test_results() {
        let mut app = app_with_user_inputs(
            1,
            UserInputState::loaded(vec![
                PersistedUserInputState {
                    id: 8,
                    content: "eight".to_string(),
                },
                PersistedUserInputState {
                    id: 1,
                    content: "one".to_string(),
                },
                PersistedUserInputState {
                    id: 3,
                    content: "three".to_string(),
                },
            ]),
        );

        assert_eq!(
            sample_rows(app.current_problem().unwrap()),
            vec![
                SampleRow::Sample {
                    flat_index: 0,
                    number: 1,
                },
                SampleRow::Blank,
                SampleRow::UserInputsHeader,
                SampleRow::UserInput {
                    id: 1,
                    number: 1,
                    dirty: false,
                },
                SampleRow::UserInput {
                    id: 3,
                    number: 2,
                    dirty: false,
                },
                SampleRow::UserInput {
                    id: 8,
                    number: 3,
                    dirty: false,
                },
            ]
        );
        assert!(app.next_case());
        let text = samples_text(&app, 20);
        let lines = text_lines(&text);
        assert!(lines.iter().any(|line| line == "> Input 1   ×  "));
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("AC") || line.contains("ms"))
        );
        let selected = text
            .lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    == "> Input 1   ×  "
            })
            .unwrap();
        assert!(selected.style.add_modifier.contains(Modifier::BOLD));

        assert!(app.next_case());
        assert!(app.next_case());
        assert_eq!(
            app.case_selection(),
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(8)))
        );
        let lines = text_lines(&samples_text(&app, 20));
        assert!(lines.iter().any(|line| line == "> Input 3   ×  "));
        assert!(!lines.iter().any(|line| line.starts_with("  Input 4")));
    }

    #[test]
    fn delete_button_hides_when_body_cannot_fit_armed_width_without_clipping_label() {
        let mut app = app_with_user_inputs(
            0,
            UserInputState::loaded(vec![PersistedUserInputState {
                id: 7,
                content: "input".to_string(),
            }]),
        );
        for armed in [false, true] {
            if armed {
                app.arm_user_input_delete(7);
            }
            for width in 0..=25 {
                let body = Rect::new(0, 0, width, 3);
                let (text, targets) = samples_content(&app, body);
                let line = text.lines[1].to_string();
                assert!(line.starts_with("> Input 1"));
                let delete = targets
                    .iter()
                    .find(|t| matches!(t.action, CasesRowAction::DeleteUserInput(_)));
                if width < 15 {
                    assert_eq!(line, "> Input 1");
                    assert!(delete.is_none());
                } else {
                    assert_eq!(
                        line,
                        if armed {
                            "> Input 1   ×? "
                        } else {
                            "> Input 1   ×  "
                        }
                    );
                    assert_eq!(delete.unwrap().area, Rect::new(11, 1, 4, 1));
                }
                // Render into a sentinel border to check clipping never wraps or overwrites.
                let mut buffer = ratatui::buffer::Buffer::filled(
                    Rect::new(0, 0, width + 1, 4),
                    ratatui::buffer::Cell::new("#"),
                );
                ratatui::widgets::Widget::render(Paragraph::new(text), body, &mut buffer);
                for y in 0..4 {
                    assert_eq!(buffer[(width, y)].symbol(), "#");
                }
                for x in 0..=width {
                    assert_eq!(buffer[(x, 3)].symbol(), "#");
                }
            }
        }
    }

    #[test]
    fn contiguous_backend_ids_also_render_as_contiguous_display_ordinals() {
        let app = app_with_user_inputs(
            0,
            UserInputState::loaded(
                [1, 2, 3]
                    .into_iter()
                    .map(|id| PersistedUserInputState {
                        id,
                        content: id.to_string(),
                    })
                    .collect(),
            ),
        );
        assert_eq!(
            text_lines(&samples_text(&app, 10)),
            [
                "User Inputs",
                "> Input 1   ×  ",
                "  Input 2   ×  ",
                "  Input 3   ×  "
            ]
        );
        assert_eq!(
            app.case_selection(),
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(1)))
        );
    }

    #[test]
    fn dirty_marker_tracks_the_problem_local_edit_target_even_when_another_row_is_selected() {
        let mut app = app_with_user_inputs(
            0,
            UserInputState::loaded(vec![
                PersistedUserInputState {
                    id: 3,
                    content: "original".to_string(),
                },
                PersistedUserInputState {
                    id: 8,
                    content: "other".to_string(),
                },
            ]),
        );
        app.begin_selected_user_input_edit().unwrap();
        assert!(
            !text_lines(&samples_text(&app, 10))
                .iter()
                .any(|line| line.contains('*'))
        );
        assert!(app.edit_user_input_insert("x"));
        assert!(app.next_case());
        assert_eq!(
            app.case_selection(),
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(8)))
        );
        let lines = text_lines(&samples_text(&app, 10));
        assert!(lines.iter().any(|line| line == "  Input 1 *"));
        assert!(lines.iter().any(|line| line == "> Input 2   ×  "));

        assert!(app.previous_case());
        assert!(app.edit_user_input_backspace());
        assert!(
            !text_lines(&samples_text(&app, 10))
                .iter()
                .any(|line| line.contains('*'))
        );
    }

    fn navigation_test_app(problem_count: usize) -> WatchApp {
        let contest = Contest {
            contest_id: "abc123".to_string(),
            problems: (0..problem_count)
                .map(|offset| {
                    let index = char::from(b'A' + offset as u8);
                    Problem {
                        index: index.to_string(),
                        title: format!("Problem {index}"),
                        task_id: format!("abc123_{index}"),
                        url: format!("https://example.invalid/{index}"),
                        sample_count: 1,
                    }
                })
                .collect(),
        };
        WatchApp::new(&contest, vec![1; problem_count]).unwrap()
    }

    #[test]
    fn sync_notice_renders_on_problem_navigation_without_changing_detail() {
        for (loaded_ids, compact) in [
            (Ok(vec![1, 7]), "! Input 2 removed"),
            (Ok(vec![]), "! 3 inputs removed"),
            (
                Err("permission denied: storage path".to_string()),
                "! Input sync failed",
            ),
            (
                Err("3 User Inputs were removed externally.".to_string()),
                "! Input sync failed",
            ),
        ] {
            let persisted = [1, 3, 7]
                .into_iter()
                .map(|id| PersistedUserInputState {
                    id,
                    content: id.to_string(),
                })
                .collect::<Vec<_>>();
            let mut app = app_with_user_inputs(1, UserInputState::loaded(persisted.clone()));
            let document_before = DetailDocument::from_app(&app)
                .segments()
                .map(|segment| segment.text())
                .collect::<String>();
            let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
            let mut layout = DetailLayout::default();
            let before = render_with_layout(&mut terminal, &app, &mut layout);
            let buffer_before = terminal.backend().buffer().clone();
            let navigation_before = problem_status_line(&app);
            assert_eq!(
                buffer_row_text(
                    &buffer_before,
                    before.detail_area.x,
                    before.detail_area.y,
                    usize::from(before.detail_area.width),
                )
                .trim_end(),
                "A - Problem A"
            );

            let loaded = loaded_ids.map(|ids| {
                persisted
                    .iter()
                    .filter(|input| ids.contains(&input.id))
                    .cloned()
                    .collect()
            });
            assert!(app.reconcile_user_input_sync(0, loaded));
            assert_eq!(
                DetailDocument::from_app(&app)
                    .segments()
                    .map(|segment| segment.text())
                    .collect::<String>(),
                document_before
            );
            // Reuse the populated layout and terminal to cover notice-only redraws.
            let after = render_with_layout(&mut terminal, &app, &mut layout);
            assert_eq!(after.detail_area, before.detail_area);
            assert_eq!(after.max_detail_scroll, before.max_detail_scroll);
            assert_eq!(problem_status_line(&app), navigation_before);
            let buffer = terminal.backend().buffer();
            let navigation = buffer_row_text(buffer, 1, 1, 98);
            assert!(navigation.starts_with("A ·  "));
            assert!(navigation.trim_end().ends_with(compact), "{navigation:?}");
            for y in 0..12 {
                if y != 1 {
                    for x in 0..100 {
                        assert_eq!(buffer.cell((x, y)), buffer_before.cell((x, y)));
                    }
                }
            }

            assert!(app.reconcile_user_input_sync(0, Ok(persisted)));
            render_with_layout(&mut terminal, &app, &mut layout);
            assert_eq!(terminal.backend().buffer(), &buffer_before);
        }
    }

    #[test]
    fn sync_notice_clips_without_overwriting_navigation_or_adding_rows_on_small_terminals() {
        for width in [1, 2, 8, 16, 28, 50, 80] {
            for height in [1, 2, 3, 5, 8, 12] {
                for problem_count in [1, 4] {
                    for samples_pane in [false, true] {
                        let mut app = navigation_test_app(problem_count);
                        if samples_pane {
                            app.toggle_samples_pane();
                        }
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        let mut layout = DetailLayout::default();
                        let before = render_with_layout(&mut terminal, &app, &mut layout);
                        let buffer_before = terminal.backend().buffer().clone();
                        let protected_navigation_end = 1 + problem_status_line(&app).width() + 2;
                        assert!(app.reconcile_user_input_sync(0, Err("read error".to_string())));
                        let after = render_with_layout(&mut terminal, &app, &mut layout);
                        assert_eq!(after.detail_area, before.detail_area);
                        assert_eq!(after.max_detail_scroll, before.max_detail_scroll);
                        let buffer = terminal.backend().buffer();
                        for y in 0..height {
                            for x in 0..width {
                                let old = buffer_before.cell((x, y)).unwrap();
                                if y != 1
                                    || usize::from(x) < protected_navigation_end
                                    || old.symbol() != " "
                                {
                                    assert_eq!(buffer.cell((x, y)), Some(old));
                                }
                            }
                        }
                        if width == 28 && height == 12 && problem_count == 4 {
                            let navigation = buffer_row_text(buffer, 1, 1, 26);
                            assert_eq!(navigation, "A ·   B ·   C ·   D ·  ! I");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sync_notice_navigation_is_problem_local_and_clears_after_success() {
        let mut app = navigation_test_app(2);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut layout = DetailLayout::default();
        assert!(app.reconcile_user_input_sync(0, Err("A storage error".to_string())));
        for (problem, has_notice) in [(0, true), (1, false), (0, true)] {
            app.select_problem(problem);
            render_with_layout(&mut terminal, &app, &mut layout);
            let navigation = buffer_row_text(terminal.backend().buffer(), 1, 1, 98);
            assert_eq!(navigation.contains("! Input sync failed"), has_notice);
            assert!(app.problems()[0].user_input_sync_notice.is_some());
        }
        assert!(app.reconcile_user_input_sync(0, Ok(Vec::new())));
        render_with_layout(&mut terminal, &app, &mut layout);
        assert_eq!(
            buffer_row_text(terminal.backend().buffer(), 1, 1, 98).trim_end(),
            "A ·   B ·"
        );
    }

    #[test]
    fn sync_notice_stays_on_problem_navigation_when_detail_scrolls() {
        let mut app = app_with_user_inputs(
            0,
            UserInputState::loaded(vec![PersistedUserInputState {
                id: 7,
                content: "input body\n".repeat(30),
            }]),
        );
        assert!(app.reconcile_user_input_sync(0, Err("read error".to_string())));
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let mut layout = DetailLayout::default();
        render_with_layout(&mut terminal, &app, &mut layout);
        let navigation_before = buffer_row_text(terminal.backend().buffer(), 1, 1, 78);
        assert!(navigation_before.contains("! Input sync failed"));
        assert!(app.scroll_detail_down(1));
        let info = render_with_layout(&mut terminal, &app, &mut layout);
        assert_eq!(
            buffer_row_text(terminal.backend().buffer(), 1, 1, 78),
            navigation_before
        );
        for y in info.detail_area.y..info.detail_area.bottom() {
            let row = buffer_row_text(
                terminal.backend().buffer(),
                info.detail_area.x,
                y,
                usize::from(info.detail_area.width),
            );
            assert!(!row.contains("! Input sync failed"));
        }
        assert!(
            app.current_problem()
                .unwrap()
                .user_input_sync_notice
                .is_some()
        );
    }

    #[test]
    fn sync_notice_does_not_add_navigation_click_targets_or_change_existing_hitboxes() {
        use crate::tui::terminal::{
            Modifiers, PointerButton, PointerEvent, PointerKind, PointerPosition,
        };
        use crate::tui::{DetailScrollbarDragState, handle_pointer_event_with_mouse_mode};

        let mut app = navigation_test_app(3);
        app.reconcile_user_input_sync(
            0,
            Ok(vec![PersistedUserInputState {
                id: 7,
                content: "input".to_string(),
            }]),
        );
        app.next_case();
        app.toggle_samples_pane();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut layout = DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let before = render_with_layout(&mut terminal, &app, &mut layout);
        assert!(!before.user_input_detail_actions.is_empty());
        let target_geometry = |info: &RenderInfo| {
            (
                info.samples_area,
                info.samples_body_area,
                info.new_input_area,
                info.detail_area,
                info.detail_section_headers
                    .iter()
                    .map(|target| (target.kind, target.area))
                    .collect::<Vec<_>>(),
                info.user_input_detail_actions
                    .iter()
                    .map(|target| (target.action, target.area))
                    .collect::<Vec<_>>(),
            )
        };
        let selection = app.case_selection();
        for has_notice in [false, true] {
            if has_notice {
                assert!(app.reconcile_user_input_sync(0, Err("read error".to_string())));
            }
            let info = render_with_layout(&mut terminal, &app, &mut layout);
            assert_eq!(target_geometry(&info), target_geometry(&before));
            assert_eq!(
                buffer_row_text(terminal.backend().buffer(), 1, 1, 98)
                    .contains("! Input sync failed"),
                has_notice
            );
            // Navigation has no click targets today. Both its labels and the
            // entire notice/gap region must remain inert in the production handler.
            for column in 1..99 {
                assert!(!handle_pointer_event_with_mouse_mode(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    PointerEvent {
                        kind: PointerKind::Down(PointerButton::Left),
                        position: PointerPosition::Cells { column, row: 1 },
                        modifiers: Modifiers::default(),
                        pixel_generation: None,
                    },
                    &info,
                    MouseMode::Cells,
                    None,
                ));
                assert_eq!(app.selected_problem(), Some(0));
                assert_eq!(app.case_selection(), selection);
            }
        }
    }

    #[test]
    fn user_input_load_error_is_a_visible_non_selectable_section() {
        let mut app = app_with_user_inputs(
            0,
            UserInputState::load_error(
                "permission denied: full detail stays out of pane".to_string(),
            ),
        );
        app.toggle_samples_pane();

        assert_eq!(
            sample_rows(app.current_problem().unwrap()),
            vec![SampleRow::UserInputsHeader, SampleRow::UserInputError]
        );
        assert_eq!(
            text_lines(&samples_text(&app, 10)),
            ["User Inputs", "  ! Failed to load"]
        );
        assert_eq!(app.case_selection(), None);
        assert!(render_info(&app, 52, 12).samples_area.is_some());
    }

    #[test]
    fn user_input_only_problem_can_show_the_cases_pane() {
        let mut app = app_with_user_inputs(
            0,
            UserInputState::loaded(vec![PersistedUserInputState {
                id: 3,
                content: "only input".to_string(),
            }]),
        );
        app.toggle_samples_pane();

        let info = render_info(&app, 52, 12);
        assert!(info.samples_area.is_some());
        assert_eq!(
            text_lines(&samples_text(&app, 10)),
            ["User Inputs", "> Input 1   ×  "]
        );
    }

    #[test]
    fn new_input_footer_is_fixed_outside_the_case_window_and_uses_disjoint_hitboxes() {
        let mut app = app_with_user_inputs(30, UserInputState::default());
        app.toggle_samples_pane();
        for _ in 0..25 {
            assert!(app.next_case());
        }

        let (buffer, info) = render_with_pointer_position(&app, None, 80, 14);
        let pane = info.samples_area.expect("Cases pane must be visible");
        let body = info
            .samples_body_area
            .expect("Cases body must be published");
        let action = info
            .new_input_area
            .expect("New Input action must be published");
        assert_eq!(action.y, pane.bottom().saturating_sub(1));
        assert!(body.bottom() <= action.y);
        assert_eq!(action.right(), pane.right().saturating_sub(1));
        assert!(!contains_rect(body, action.x, action.y));
        assert!(!contains_rect(
            action,
            pane.right().saturating_sub(1),
            action.y
        ));
        assert!(
            buffer_row_text(&buffer, action.x, action.y, usize::from(action.width))
                .starts_with("+ New Input")
        );
        assert_eq!(sample_rows(app.current_problem().unwrap()).len(), 30);
        assert_eq!(
            sample_window(30, 25, usize::from(body.height)).len(),
            usize::from(body.height)
        );
    }

    #[test]
    fn new_input_footer_hover_matches_its_click_area_only() {
        let mut app = app_with_user_inputs(0, UserInputState::default());
        app.toggle_samples_pane();
        let (_, initial) = render_with_pointer_position(&app, None, 80, 12);
        let action = initial.new_input_area.unwrap();
        let pointer = (action.x, action.y);
        let (hovered, hovered_info) = render_with_pointer_position(&app, Some(pointer), 80, 12);
        assert_eq!(hovered.cell(pointer).unwrap().bg, Color::DarkGray);
        assert_eq!(hovered_info.new_input_area, Some(action));

        if action.y > 0 {
            let separator = (action.x, action.y - 1);
            let (not_hovered, _) = render_with_pointer_position(&app, Some(separator), 80, 12);
            assert_ne!(not_hovered.cell(separator).unwrap().bg, Color::DarkGray);
        }
    }

    #[test]
    fn no_cases_still_shows_new_input_and_tiny_case_heights_are_safe() {
        let mut app = app_with_user_inputs(0, UserInputState::default());
        app.toggle_samples_pane();

        let normal = render_info(&app, 52, 12);
        assert!(normal.samples_area.is_some());
        assert!(normal.new_input_area.is_some());
        assert_eq!(sample_rows(app.current_problem().unwrap()), []);

        for height in 0..=7 {
            let (_, info) = render_with_pointer_position(&app, None, 52, height);
            if let Some(action) = info.new_input_area {
                assert!(action.height <= 1);
                assert!(action.bottom() <= info.samples_area.unwrap().bottom());
            }
        }
    }

    #[test]
    fn persisted_edit_and_editing_cancel_actions_are_separate_from_fold_headers() {
        let mut app = user_input_detail_app();
        let (_, read_only) = render_with_pointer_position(&app, None, 100, 30);
        let edit = read_only.user_input_detail_actions[0];
        assert_eq!(edit.action, UserInputDetailAction::Edit);
        let input_header = read_only
            .detail_section_headers
            .iter()
            .find(|header| header.kind == DetailSectionKind::Input)
            .unwrap();
        assert!(input_header.area.right() <= edit.area.x);
        let (edit_hover, _) =
            render_with_pointer_position(&app, Some((edit.area.x, edit.area.y)), 100, 30);
        assert_eq!(
            edit_hover.cell((edit.area.x, edit.area.y)).unwrap().bg,
            Color::DarkGray
        );

        app.begin_selected_user_input_edit().unwrap();
        let (buffer, editing) = render_with_pointer_position(&app, None, 100, 30);
        assert_eq!(editing.user_input_detail_actions.len(), 3);
        let save = editing.user_input_detail_actions[0];
        let run = editing.user_input_detail_actions[1];
        let cancel = editing.user_input_detail_actions[2];
        assert_eq!(save.action, UserInputDetailAction::Save);
        assert_eq!(run.action, UserInputDetailAction::Run);
        assert_eq!(cancel.action, UserInputDetailAction::Cancel);
        assert!(save.area.right() < cancel.area.x);
        assert!(
            editing
                .detail_section_headers
                .iter()
                .all(|header| header.kind != DetailSectionKind::Input)
        );
        assert!(buffer_symbols(&buffer).contains("Input — Editing"));
        assert!(!buffer_symbols(&buffer).contains("Input — Editing *"));
        assert!(buffer_symbols(&buffer).contains("[Save] [Run] [Cancel]"));
        let (save_hover, _) =
            render_with_pointer_position(&app, Some((save.area.x, save.area.y)), 100, 30);
        assert_eq!(
            save_hover.cell((save.area.x, save.area.y)).unwrap().bg,
            Color::DarkGray
        );
        let (cancel_hover, _) =
            render_with_pointer_position(&app, Some((cancel.area.x, cancel.area.y)), 100, 30);
        assert_eq!(
            cancel_hover
                .cell((cancel.area.x, cancel.area.y))
                .unwrap()
                .bg,
            Color::DarkGray
        );

        assert!(app.edit_user_input_insert("x"));
        let dirty = rendered_buffer_text(&app, 100, 30);
        assert!(dirty.contains("Input — Editing *"));

        for width in 0..=20 {
            let (_, narrow) = render_with_pointer_position(&app, None, width, 8);
            if narrow.user_input_detail_actions.len() == 3 {
                assert!(
                    narrow.user_input_detail_actions[0].area.right()
                        < narrow.user_input_detail_actions[1].area.x
                );
            }
        }
    }

    #[test]
    fn editor_renders_visible_cursor_for_empty_and_exact_multiline_buffers() {
        let mut empty = app_with_user_inputs(0, UserInputState::default());
        empty.begin_new_user_input().unwrap();
        let (empty_buffer, empty_info) = render_with_pointer_position(&empty, None, 80, 20);
        let empty_cursor = empty_info
            .editor_cursor
            .expect("empty editor needs a cursor");
        assert!(
            empty_buffer
                .cell(empty_cursor)
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(empty.selected_user_input_edit().unwrap().buffer(), "");

        assert!(empty.edit_user_input_insert("a\r\n\n界\n"));
        let (text_buffer, text_info) = render_with_pointer_position(&empty, None, 80, 20);
        let cursor = text_info
            .editor_cursor
            .expect("trailing line needs a cursor");
        assert!(
            text_buffer
                .cell(cursor)
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(
            empty.selected_user_input_edit().unwrap().buffer(),
            "a\r\n\n界\n"
        );
    }

    #[test]
    fn editor_cursor_requests_vertical_scroll_until_it_is_visible() {
        let mut app = app_with_user_inputs(0, UserInputState::default());
        app.begin_new_user_input().unwrap();
        let text = (0..80)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        assert!(app.edit_user_input_insert(&text));

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = DetailLayout::default();
        let mut cursor = None;
        for _ in 0..8 {
            let mut info = RenderInfo::default();
            terminal
                .draw(|frame| info = render(frame, &app, &mut layout))
                .unwrap();
            cursor = info.editor_cursor;
            let Some(target) = info.editor_scroll_reconciliation else {
                break;
            };
            assert!(app.reconcile_detail_scroll(target));
        }
        assert!(cursor.is_some());
        assert!(app.detail_scroll() > 0);
    }

    fn settle_editor_cursor_scroll(
        app: &mut WatchApp,
        terminal: &mut Terminal<TestBackend>,
        layout: &mut DetailLayout,
    ) -> RenderInfo {
        for _ in 0..8 {
            let info = render_with_layout(terminal, app, layout);
            let Some(target) = info.editor_scroll_reconciliation else {
                assert!(info.editor_cursor.is_some());
                return info;
            };
            assert!(app.reconcile_detail_scroll(target));
        }
        panic!("editor cursor scroll did not converge");
    }

    #[test]
    fn unicode_soft_wrap_cursor_scroll_converges_and_remains_stable() {
        let mut app = app_with_user_inputs(0, UserInputState::default());
        app.begin_new_user_input().unwrap();
        assert!(app.edit_user_input_insert(&"😀".repeat(400)));

        let backend = TestBackend::new(34, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = DetailLayout::default();

        let eof = settle_editor_cursor_scroll(&mut app, &mut terminal, &mut layout);
        let eof_scroll = app.detail_scroll();
        assert!(eof_scroll > 0);
        assert!(eof.editor_cursor.is_some());
        for _ in 0..3 {
            let stable = render_with_layout(&mut terminal, &app, &mut layout);
            assert_eq!(stable.editor_scroll_reconciliation, None);
            assert_eq!(app.detail_scroll(), eof_scroll);
            assert!(stable.editor_cursor.is_some());
        }

        assert!(app.edit_user_input_home());
        settle_editor_cursor_scroll(&mut app, &mut terminal, &mut layout);
        let beginning_scroll = app.detail_scroll();
        assert!(beginning_scroll < eof_scroll);

        for _ in 0..200 {
            assert!(app.edit_user_input_right());
        }
        settle_editor_cursor_scroll(&mut app, &mut terminal, &mut layout);
        let middle_scroll = app.detail_scroll();
        assert!(middle_scroll > beginning_scroll && middle_scroll < eof_scroll);
        for _ in 0..3 {
            let stable = render_with_layout(&mut terminal, &app, &mut layout);
            assert_eq!(stable.editor_scroll_reconciliation, None);
            assert_eq!(app.detail_scroll(), middle_scroll);
        }
    }

    #[test]
    fn lazy_unicode_cursor_uses_exact_count_provenance() {
        let mut app = app_with_user_inputs(0, UserInputState::default());
        app.begin_new_user_input().unwrap();
        assert!(app.edit_user_input_insert(&"😀".repeat(17_000)));

        let backend = TestBackend::new(34, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = DetailLayout::default();
        render_with_layout(&mut terminal, &app, &mut layout);
        let document = DetailDocument::from_app(&app);
        apply_ready_count(&mut layout, &document);

        settle_editor_cursor_scroll(&mut app, &mut terminal, &mut layout);
        let stable_scroll = app.detail_scroll();
        assert!(stable_scroll > 0);
        for _ in 0..3 {
            let stable = render_with_layout(&mut terminal, &app, &mut layout);
            assert_eq!(stable.editor_scroll_reconciliation, None);
            assert_eq!(app.detail_scroll(), stable_scroll);
            assert!(stable.editor_cursor.is_some());
        }
    }

    #[test]
    fn ascii_soft_wrap_cursor_scroll_still_converges() {
        let mut app = app_with_user_inputs(0, UserInputState::default());
        app.begin_new_user_input().unwrap();
        assert!(app.edit_user_input_insert(&"x".repeat(1_200)));
        let backend = TestBackend::new(34, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = DetailLayout::default();

        settle_editor_cursor_scroll(&mut app, &mut terminal, &mut layout);
        let stable_scroll = app.detail_scroll();
        for _ in 0..3 {
            let stable = render_with_layout(&mut terminal, &app, &mut layout);
            assert_eq!(stable.editor_scroll_reconciliation, None);
            assert_eq!(app.detail_scroll(), stable_scroll);
        }
    }

    fn rendered_editor_cursor_for(content: &str, width: u16) -> (RenderInfo, usize) {
        let mut app = app_with_user_inputs(0, UserInputState::default());
        app.begin_new_user_input().unwrap();
        assert!(app.edit_user_input_insert(content));
        let info = render_info(&app, width.saturating_add(2), 20);
        let wrap_width = usize::from(info.detail_area.width);
        (info, wrap_width)
    }

    #[test]
    fn eof_cursor_uses_the_visual_insertion_point_at_wrap_boundaries() {
        let width = 20u16;
        for (content, expected_row_offset, expected_column_offset) in [
            ("x".repeat(usize::from(width.saturating_sub(1))), 0, 19),
            ("x".repeat(usize::from(width)), 1, 0),
            ("x".repeat(usize::from(width.saturating_add(1))), 1, 1),
            (format!("{}\n", "x".repeat(usize::from(width))), 1, 0),
        ] {
            let (info, actual_width) = rendered_editor_cursor_for(&content, width);
            assert_eq!(actual_width, usize::from(width));
            let cursor = info.editor_cursor.unwrap();
            let content_row = info
                .detail_section_headers
                .iter()
                .find(|header| header.kind == DetailSectionKind::Input)
                .map(|header| header.area.y.saturating_add(1))
                .unwrap_or(info.detail_area.y.saturating_add(4));
            assert_eq!(
                cursor.0.saturating_sub(info.detail_area.x),
                expected_column_offset
            );
            assert_eq!(
                cursor.1.saturating_sub(content_row),
                expected_row_offset,
                "content={content:?}, cursor={cursor:?}, detail={:?}",
                info.detail_area
            );
        }

        let unicode = "界".repeat(usize::from(width / 2));
        let (info, actual_width) = rendered_editor_cursor_for(&unicode, width);
        assert_eq!(actual_width, usize::from(width));
        let cursor = info.editor_cursor.unwrap();
        assert_eq!(cursor.0, info.detail_area.x);
        assert_eq!(cursor.1, info.detail_area.y.saturating_add(5));

        let mut app = app_with_user_inputs(0, UserInputState::default());
        app.begin_new_user_input().unwrap();
        assert!(app.edit_user_input_insert(&"x".repeat(usize::from(width.saturating_sub(1)))));
        let backend = TestBackend::new(width.saturating_add(2), 7);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = DetailLayout::default();
        let settled = settle_editor_cursor_scroll(&mut app, &mut terminal, &mut layout);
        assert_eq!(settled.editor_cursor.unwrap().0, settled.detail_area.x);
        assert_eq!(settled.editor_scroll_reconciliation, None);
    }

    #[test]
    fn draft_only_problem_renders_the_cases_pane_selection_and_input_detail() {
        let draft = "draft first line\r\n\r\ndraft last line\r\n";
        let mut ready = UserInputReadyState::default();
        ready.begin_draft().unwrap();
        ready.edit_mut().unwrap().replace_buffer(draft.to_string());
        let mut app = app_with_user_inputs(0, UserInputState::Ready(ready));

        assert_eq!(app.current_problem().unwrap().sample_cases, 0);
        assert!(app.current_problem().unwrap().saved_stress_case.is_none());
        assert_eq!(app.current_problem().unwrap().total_cases, 0);
        assert!(app.current_problem().unwrap().run.cases.is_empty());
        assert_eq!(
            app.case_selection(),
            Some(CaseSelection::UserInput(UserInputSelection::Draft))
        );
        assert_eq!(
            text_lines(&samples_text(&app, 10)),
            ["User Inputs", "> Draft *"]
        );

        app.toggle_samples_pane();
        assert!(render_info(&app, 80, 16).samples_area.is_some());
        let rendered = rendered_buffer_text(&app, 80, 16);
        assert!(rendered.contains("User Inputs"));
        assert!(rendered.contains("> Draft"));
        assert!(rendered.contains("▼ Input"));
        assert!(rendered.contains("draft first line"));
        assert!(rendered.contains("draft last line"));
        assert_eq!(
            DetailDocument::from_app(&app).section_body(DetailSectionKind::Input),
            Some(draft)
        );
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
    fn cases_pane_geometry_is_saturating_at_heights_zero_one_and_two() {
        let area = |height| Rect::new(5, 7, 20, height);

        let (body, footer, separator) = cases_pane_areas(area(0));
        assert_eq!(body, Rect::new(5, 7, 19, 0));
        assert_eq!(footer, Rect::new(5, 7, 19, 0));
        assert_eq!(separator, None);

        let (body, footer, separator) = cases_pane_areas(area(1));
        assert_eq!(body, Rect::new(5, 7, 19, 0));
        assert_eq!(footer, Rect::new(5, 7, 19, 1));
        assert_eq!(separator, None);

        let (body, footer, separator) = cases_pane_areas(area(2));
        assert_eq!(body, Rect::new(5, 7, 19, 0));
        assert_eq!(footer, Rect::new(5, 8, 19, 1));
        assert_eq!(separator, Some(Rect::new(5, 7, 19, 1)));
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
