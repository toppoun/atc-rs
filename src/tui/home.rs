use std::io;
use std::path::Path;
use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminalEvent};
use super::view::{self, ContestOpenPurpose};
use super::{
    ContestOpenController, ContestOpenKeyResult, ContestSwitchResolution, ContestSwitchTask,
    ShortcutHelpTransition, SubmissionHub, TerminaSession, command_matches,
    is_command_palette_open_key, is_shortcut_help_key, shortcut_help_transition,
};
use crate::branding;

const HOME_POLL_INTERVAL: Duration = Duration::from_millis(20);
const HOME_ACTIONS: [(&str, &str); 4] = [
    ("Open Contest", "c"),
    ("Commands", ":"),
    ("Shortcuts", "?"),
    ("Quit", "q"),
];
const MENU_WIDTH: u16 = 23;
const MENU_HEIGHT: u16 = HOME_ACTIONS.len() as u16;
const SUBTITLE: &str = "AtCoder workspace";
const WORKSPACE_PREFIX: &str = "Workspace  ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HomeAction {
    None,
    OpenContest,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeCommand {
    OpenContest,
    Quit,
}

impl HomeCommand {
    const ALL: [Self; 2] = [Self::OpenContest, Self::Quit];

    const fn label(self) -> &'static str {
        match self {
            Self::OpenContest => "Open Contest",
            Self::Quit => "Quit",
        }
    }

    const fn shortcut(self) -> &'static str {
        match self {
            Self::OpenContest => "c",
            Self::Quit => "q",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct HomeCommandPalette {
    open: bool,
    query: String,
    selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomePaletteKeyResult {
    NotHandled,
    Handled,
    Execute(HomeCommand),
}

impl HomeCommandPalette {
    fn is_active(&self) -> bool {
        self.open
    }

    fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
    }

    fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = 0;
    }

    fn filtered_commands(&self) -> Vec<HomeCommand> {
        HomeCommand::ALL
            .into_iter()
            .filter(|command| command_matches(command.label(), &self.query))
            .collect()
    }

    fn selected_command(&self) -> Option<HomeCommand> {
        self.filtered_commands().get(self.selected).copied()
    }

    fn handle_key(&mut self, key: KeyEvent) -> HomePaletteKeyResult {
        if !self.is_active() {
            return HomePaletteKeyResult::NotHandled;
        }
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return HomePaletteKeyResult::Handled;
        }

        match key.code {
            KeyCode::Escape if key.kind == KeyEventKind::Press => {
                self.close();
                HomePaletteKeyResult::Handled
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => self
                .selected_command()
                .map(HomePaletteKeyResult::Execute)
                .unwrap_or(HomePaletteKeyResult::Handled),
            KeyCode::Backspace => {
                if let Some((start, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(start);
                }
                self.selected = 0;
                HomePaletteKeyResult::Handled
            }
            KeyCode::Up => {
                let count = self.filtered_commands().len();
                self.selected = if count <= 1 {
                    0
                } else if self.selected == 0 {
                    count - 1
                } else {
                    self.selected.min(count - 1) - 1
                };
                HomePaletteKeyResult::Handled
            }
            KeyCode::Down => {
                let count = self.filtered_commands().len();
                self.selected = if count <= 1 {
                    0
                } else {
                    (self.selected + 1) % count
                };
                HomePaletteKeyResult::Handled
            }
            KeyCode::Char(character)
                if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
            {
                self.query.push(character);
                self.selected = 0;
                HomePaletteKeyResult::Handled
            }
            _ => HomePaletteKeyResult::Handled,
        }
    }
}

/// Workspace Home owns only Home-specific UI state. Contest state remains mandatory inside
/// `WatchApp`/`SessionRuntime` and is created only after this state produces a prepared handoff.
struct HomeState<'a> {
    shortcut_help_visible: bool,
    palette: HomeCommandPalette,
    open_contest: ContestOpenController<'a>,
}

impl<'a> HomeState<'a> {
    fn new(
        resolve: &'a mut dyn FnMut(&str) -> ContestSwitchResolution,
        task: ContestSwitchTask,
    ) -> Self {
        Self {
            shortcut_help_visible: false,
            palette: HomeCommandPalette::default(),
            open_contest: ContestOpenController::new(resolve, task),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> HomeAction {
        if self.open_contest.modal_active() {
            let mut identity = |resolution| resolution;
            let mut no_current_destination = |_destination: &std::path::Path| false;
            return match self.open_contest.handle_key(
                key,
                &mut identity,
                &mut no_current_destination,
            ) {
                ContestOpenKeyResult::OpenRequested => HomeAction::OpenContest,
                ContestOpenKeyResult::Handled | ContestOpenKeyResult::NotHandled => {
                    HomeAction::None
                }
            };
        }

        if self.palette.is_active() {
            return match self.palette.handle_key(key) {
                HomePaletteKeyResult::Execute(HomeCommand::OpenContest) => {
                    self.palette.close();
                    self.open_contest.open();
                    HomeAction::None
                }
                HomePaletteKeyResult::Execute(HomeCommand::Quit) => {
                    self.palette.close();
                    HomeAction::Quit
                }
                HomePaletteKeyResult::NotHandled | HomePaletteKeyResult::Handled => {
                    HomeAction::None
                }
            };
        }

        match shortcut_help_transition(self.shortcut_help_visible, key) {
            ShortcutHelpTransition::KeepAndConsume => return HomeAction::None,
            ShortcutHelpTransition::DismissAndConsume => {
                self.shortcut_help_visible = false;
                return HomeAction::None;
            }
            ShortcutHelpTransition::DismissAndPassThrough => {
                self.shortcut_help_visible = false;
            }
            ShortcutHelpTransition::PassThrough => {}
        }

        if key.kind != KeyEventKind::Press {
            return HomeAction::None;
        }
        if is_shortcut_help_key(key) {
            self.shortcut_help_visible = true;
            HomeAction::None
        } else if is_command_palette_open_key(key) {
            self.palette.open();
            HomeAction::None
        } else {
            match key.code {
                KeyCode::Char('c')
                    if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
                {
                    self.open_contest.open();
                    HomeAction::None
                }
                KeyCode::Char('q')
                    if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
                {
                    HomeAction::Quit
                }
                _ => HomeAction::None,
            }
        }
    }

    fn handle_operation_messages(&mut self) -> bool {
        self.open_contest.handle_operation_messages()
    }

    fn open_requested(&self) -> bool {
        self.open_contest.open_requested
    }

    fn start_requested_contest<T>(
        &mut self,
        start_contest: &mut impl FnMut() -> Result<T, String>,
    ) -> Option<T> {
        if !self.open_requested() {
            return None;
        }
        self.open_contest.open_requested = false;
        match start_contest() {
            Ok(contest) => Some(contest),
            Err(error) => {
                self.open_contest.open_requested = false;
                if let Some(modal) = self.open_contest.modal.as_mut() {
                    modal.state = super::SwitchContestModalState::Failed;
                    modal.error = Some(error);
                    modal.target = Some(super::ContestSwitchTarget::Existing);
                    modal.mutation = None;
                }
                None
            }
        }
    }
}

pub(crate) enum HomeExit<T> {
    Quit,
    Contest(T),
}

pub(super) trait HomeTerminal {
    fn draw_home(&mut self, render: &mut dyn FnMut(&mut Frame<'_>)) -> io::Result<()>;
    fn finish_home_redraw(&mut self) -> io::Result<()>;
    fn note_home_resize(&mut self);
    fn poll_home(&mut self, wait: Duration) -> io::Result<bool>;
    fn read_home(&mut self) -> io::Result<TerminalEvent>;
}

impl HomeTerminal for TerminaSession {
    fn draw_home(&mut self, render: &mut dyn FnMut(&mut Frame<'_>)) -> io::Result<()> {
        self.draw(|frame| render(frame))
    }

    fn finish_home_redraw(&mut self) -> io::Result<()> {
        self.note_redraw_completed();
        self.refresh_mouse_after_redraw(false)?;
        self.retry_high_res_after_redraw(false)
    }

    fn note_home_resize(&mut self) {
        self.note_resize_dispatched();
    }

    fn poll_home(&mut self, wait: Duration) -> io::Result<bool> {
        self.poll(wait)
    }

    fn read_home(&mut self) -> io::Result<TerminalEvent> {
        self.read()
    }
}

pub(crate) fn run<T>(
    terminal: &mut TerminaSession,
    workspace_root: &Path,
    submissions: &mut SubmissionHub,
    resolve: &mut dyn FnMut(&str) -> ContestSwitchResolution,
    task: ContestSwitchTask,
    start_contest: impl FnMut() -> Result<T, String>,
) -> io::Result<HomeExit<T>> {
    run_with_terminal(
        terminal,
        workspace_root,
        submissions,
        resolve,
        task,
        start_contest,
    )
}

pub(super) fn run_with_terminal<T>(
    terminal: &mut impl HomeTerminal,
    workspace_root: &Path,
    submissions: &mut SubmissionHub,
    resolve: &mut dyn FnMut(&str) -> ContestSwitchResolution,
    task: ContestSwitchTask,
    mut start_contest: impl FnMut() -> Result<T, String>,
) -> io::Result<HomeExit<T>> {
    let mut state = HomeState::new(resolve, task);
    let mut dirty = true;

    loop {
        // Submission-only changes stay invisible on Home, so intentionally ignore dirtiness.
        let _ = submissions.handle_events();
        dirty |= state.handle_operation_messages();
        if state.open_requested() {
            if let Some(contest) = state.start_requested_contest(&mut start_contest) {
                return Ok(HomeExit::Contest(contest));
            }
            dirty = true;
        }

        if dirty {
            terminal.draw_home(&mut |frame| render(frame, &state, workspace_root))?;
            terminal.finish_home_redraw()?;
            dirty = false;
        }

        if !terminal.poll_home(HOME_POLL_INTERVAL)? {
            continue;
        }
        match terminal.read_home()? {
            TerminalEvent::Key(key) => match state.handle_key(key) {
                HomeAction::None => dirty = true,
                HomeAction::OpenContest => {
                    dirty = true;
                }
                HomeAction::Quit => return Ok(HomeExit::Quit),
            },
            TerminalEvent::Resize(_) => {
                terminal.note_home_resize();
                dirty = true;
            }
            TerminalEvent::Paste(_) | TerminalEvent::Pointer(_) | TerminalEvent::Ignored => {}
        }
    }
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = area.width.min(width);
    let height = area.height.min(height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HomeLayout {
    logo: Option<Rect>,
    subtitle: Option<Rect>,
    menu: Rect,
    workspace: Option<Rect>,
}

fn logo_size() -> (u16, u16) {
    let width = branding::ascii_logo_lines()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    let height = branding::ascii_logo_lines().count();
    (
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(height).unwrap_or(u16::MAX),
    )
}

fn centered_row(area: Rect, y: u16, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        y,
        width,
        height,
    )
}

fn home_layout(area: Rect) -> HomeLayout {
    if area.width == 0 || area.height == 0 {
        return HomeLayout {
            logo: None,
            subtitle: None,
            menu: Rect::new(area.x, area.y, 0, 0),
            workspace: None,
        };
    }

    let show_workspace = area.height >= MENU_HEIGHT.saturating_add(1);
    let bottom_margin = u16::from(show_workspace && area.height >= MENU_HEIGHT.saturating_add(4));
    let workspace_y = area
        .y
        .saturating_add(area.height)
        .saturating_sub(1)
        .saturating_sub(bottom_margin);
    let body_height = if show_workspace {
        workspace_y.saturating_sub(area.y)
    } else {
        area.height
    };

    let subtitle_width = u16::try_from(UnicodeWidthStr::width(SUBTITLE)).unwrap_or(u16::MAX);
    let show_subtitle =
        area.width >= subtitle_width && body_height >= MENU_HEIGHT.saturating_add(2);
    let (logo_width, logo_height) = logo_size();
    let full_content_height = logo_height
        .saturating_add(1)
        .saturating_add(1)
        .saturating_add(1)
        .saturating_add(MENU_HEIGHT);
    let show_logo = show_subtitle && area.width >= logo_width && body_height >= full_content_height;

    let content_height = if show_logo {
        full_content_height
    } else if show_subtitle {
        MENU_HEIGHT.saturating_add(2)
    } else {
        MENU_HEIGHT.min(body_height)
    };
    let content_y = area
        .y
        .saturating_add(body_height.saturating_sub(content_height) / 2);

    let (logo, subtitle, menu_y) = if show_logo {
        (
            Some(centered_row(area, content_y, logo_width, logo_height)),
            Some(centered_row(
                area,
                content_y.saturating_add(logo_height).saturating_add(1),
                subtitle_width,
                1,
            )),
            content_y.saturating_add(logo_height).saturating_add(3),
        )
    } else if show_subtitle {
        (
            None,
            Some(centered_row(area, content_y, subtitle_width, 1)),
            content_y.saturating_add(2),
        )
    } else {
        (None, None, content_y)
    };

    let workspace = show_workspace.then(|| {
        let margin = u16::from(area.width >= 4);
        Rect::new(
            area.x.saturating_add(margin),
            workspace_y,
            area.width.saturating_sub(margin.saturating_mul(2)),
            1,
        )
    });

    HomeLayout {
        logo,
        subtitle,
        menu: centered_row(area, menu_y, MENU_WIDTH, MENU_HEIGHT.min(body_height)),
        workspace,
    }
}

fn menu_line(label: &'static str, shortcut: &'static str, width: usize) -> Line<'static> {
    let shortcut_width = UnicodeWidthStr::width(shortcut);
    let shortcut_style = Style::default().fg(Color::Yellow);
    if width <= shortcut_width {
        return Line::styled(
            view::clip_text_with_ellipsis(shortcut, width),
            shortcut_style,
        );
    }

    let label_width = width.saturating_sub(shortcut_width).saturating_sub(1);
    let label = view::clip_text_with_ellipsis(label, label_width);
    let padding = width
        .saturating_sub(UnicodeWidthStr::width(label.as_str()))
        .saturating_sub(shortcut_width);
    Line::from(vec![
        Span::raw(label),
        Span::raw(" ".repeat(padding)),
        Span::styled(shortcut, shortcut_style),
    ])
}

fn truncate_start_with_ellipsis(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }

    let content_width = width - 1;
    let mut suffix = Vec::new();
    let mut suffix_width = 0usize;
    for grapheme in text.graphemes(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if suffix_width.saturating_add(grapheme_width) > content_width {
            break;
        }
        suffix.push(grapheme);
        suffix_width = suffix_width.saturating_add(grapheme_width);
    }

    let mut fitted = String::from("…");
    fitted.extend(suffix.into_iter().rev());
    fitted
}

fn workspace_line(workspace_root: &Path, width: usize) -> String {
    let path = workspace_root.to_string_lossy();
    let full = format!("{WORKSPACE_PREFIX}{path}");
    if UnicodeWidthStr::width(full.as_str()) <= width {
        return full;
    }

    let prefix_width = UnicodeWidthStr::width(WORKSPACE_PREFIX);
    if width <= prefix_width {
        truncate_start_with_ellipsis(&full, width)
    } else {
        format!(
            "{WORKSPACE_PREFIX}{}",
            truncate_start_with_ellipsis(&path, width - prefix_width)
        )
    }
}

fn render(frame: &mut Frame<'_>, state: &HomeState<'_>, workspace_root: &Path) {
    let layout = home_layout(frame.area());

    if let Some(area) = layout.logo {
        let lines = branding::ascii_logo_lines().map(|line| {
            Line::styled(
                line,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        });
        frame.render_widget(Paragraph::new(Text::from_iter(lines)), area);
    }
    if let Some(area) = layout.subtitle {
        frame.render_widget(
            Paragraph::new(SUBTITLE)
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            area,
        );
    }
    if layout.menu.width > 0 && layout.menu.height > 0 {
        let lines = HOME_ACTIONS
            .iter()
            .map(|(label, shortcut)| menu_line(label, shortcut, usize::from(layout.menu.width)));
        frame.render_widget(Paragraph::new(Text::from_iter(lines)), layout.menu);
    }
    if let Some(area) = layout.workspace {
        frame.render_widget(
            Paragraph::new(workspace_line(workspace_root, usize::from(area.width)))
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            area,
        );
    }

    if state.shortcut_help_visible {
        render_shortcuts(frame);
    }
    if state.palette.is_active() {
        render_palette(frame, &state.palette);
    }
    if let Some(modal) = state.open_contest.modal() {
        view::render_contest_open_modal(frame, modal, ContestOpenPurpose::Open);
    }
}

fn render_shortcuts(frame: &mut Frame<'_>) {
    let area = centered_rect(frame.area(), 38, 8);
    let lines = vec![
        Line::raw("c  Open Contest"),
        Line::raw(":  Commands"),
        Line::raw("q  Quit"),
        Line::raw(""),
        Line::raw("? keep open   Esc close"),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().title(" Shortcuts ").borders(Borders::ALL)),
        area,
    );
}

fn render_palette(frame: &mut Frame<'_>, palette: &HomeCommandPalette) {
    let commands = palette.filtered_commands();
    let height = 7u16.saturating_add(u16::try_from(commands.len()).unwrap_or(u16::MAX));
    let area = centered_rect(frame.area(), 52, height);
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);
    let list_area = view::command_palette_list_area(rows[1], None);
    let list_width = usize::from(list_area.width);
    let lines = if commands.is_empty() {
        vec![Line::styled(
            "  No matching commands",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        commands
            .iter()
            .enumerate()
            .map(|(index, command)| {
                let selected = index == palette.selected;
                view::command_palette_line(
                    if selected { ">" } else { " " },
                    command.label(),
                    Some(command.shortcut()),
                    list_width,
                    Style::default(),
                    selected,
                )
            })
            .collect()
    };

    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .title(" Command Palette ")
            .borders(Borders::ALL),
        area,
    );
    frame.render_widget(Paragraph::new(format!("> {}", palette.query)), rows[0]);
    frame.render_widget(Paragraph::new(Text::from(lines)), list_area);
    frame.render_widget(
        Paragraph::new("[↑↓] Select   [Enter] Run   [Esc] Cancel"),
        rows[2],
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use super::*;
    use crate::tui::SwitchContestModalState;
    use crate::tui::terminal::Modifiers;

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            kind,
            modifiers: Modifiers::default(),
        }
    }

    fn state<'a>(resolve: &'a mut dyn FnMut(&str) -> ContestSwitchResolution) -> HomeState<'a> {
        HomeState::new(resolve, Arc::new(|_, _| Ok(())))
    }

    fn draw_home(home: &HomeState<'_>, workspace_root: &Path, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, home, workspace_root))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    fn row_text(buffer: &Buffer, row: u16) -> String {
        (0..buffer.area.width)
            .map(|column| buffer.cell((column, row)).unwrap().symbol())
            .collect()
    }

    fn visible_area_text(buffer: &Buffer, area: Rect) -> String {
        let mut text = String::new();
        let mut column = area.x;
        let end = area.x.saturating_add(area.width);
        while column < end {
            let symbol = buffer.cell((column, area.y)).unwrap().symbol();
            text.push_str(symbol);
            let symbol_width = u16::try_from(UnicodeWidthStr::width(symbol))
                .unwrap_or(u16::MAX)
                .max(1);
            column = column.saturating_add(symbol_width);
        }
        text
    }

    #[test]
    fn c_opens_contest_input() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        assert_eq!(
            home.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press)),
            HomeAction::None
        );
        assert!(home.open_contest.modal_active());
    }

    #[test]
    fn shortcut_help_is_one_shot_and_passes_the_next_action_once() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        home.handle_key(key(KeyCode::Char('?'), KeyEventKind::Press));
        assert!(home.shortcut_help_visible);
        home.handle_key(key(KeyCode::Char('?'), KeyEventKind::Repeat));
        assert!(home.shortcut_help_visible);
        home.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));

        assert!(!home.shortcut_help_visible);
        assert!(home.open_contest.modal_active());
        assert_eq!(home.open_contest.modal().unwrap().contest_id, "");
    }

    #[test]
    fn escape_closes_only_shortcut_help() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        home.handle_key(key(KeyCode::Char('?'), KeyEventKind::Press));
        assert_eq!(
            home.handle_key(key(KeyCode::Escape, KeyEventKind::Press)),
            HomeAction::None
        );
        assert!(!home.shortcut_help_visible);
        assert!(!home.open_contest.modal_active());
        assert!(!home.palette.is_active());
    }

    #[test]
    fn colon_opens_home_only_palette_and_q_quits_at_root() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        home.handle_key(key(KeyCode::Char(':'), KeyEventKind::Press));
        assert!(home.palette.is_active());
        assert_eq!(
            home.palette
                .filtered_commands()
                .iter()
                .map(|command| command.label())
                .collect::<Vec<_>>(),
            ["Open Contest", "Quit"]
        );
        home.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        assert_eq!(
            home.handle_key(key(KeyCode::Char('q'), KeyEventKind::Press)),
            HomeAction::Quit
        );

        home.handle_key(key(KeyCode::Char(':'), KeyEventKind::Press));
        home.handle_key(key(KeyCode::Down, KeyEventKind::Press));
        assert_eq!(
            home.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            HomeAction::Quit
        );
    }

    #[test]
    fn home_overlays_follow_single_modal_precedence() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        home.handle_key(key(KeyCode::Char(':'), KeyEventKind::Press));
        home.handle_key(key(KeyCode::Char('?'), KeyEventKind::Press));
        assert!(home.palette.is_active());
        assert!(!home.shortcut_help_visible);
        assert!(!home.open_contest.modal_active());
        assert_eq!(home.palette.query, "?");

        home.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        home.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        home.handle_key(key(KeyCode::Char(':'), KeyEventKind::Press));
        assert!(home.open_contest.modal_active());
        assert!(!home.palette.is_active());
        assert!(!home.shortcut_help_visible);
        assert_eq!(home.open_contest.modal().unwrap().contest_id, ":");
    }

    #[test]
    fn existing_contest_requests_prepared_handoff() {
        let destination = PathBuf::from("workspace/abc473");
        let mut resolve =
            |_contest_id: &str| ContestSwitchResolution::accepted(destination.clone());
        let mut home = state(&mut resolve);
        home.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        for character in "abc473".chars() {
            home.handle_key(key(KeyCode::Char(character), KeyEventKind::Press));
        }

        assert_eq!(
            home.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            HomeAction::OpenContest
        );
        assert!(home.open_requested());
        let mut start = || Ok::<_, String>("Contest");
        assert_eq!(home.start_requested_contest(&mut start), Some("Contest"));
    }

    #[test]
    fn start_failure_keeps_home_and_surfaces_error() {
        let mut resolve = |_: &str| ContestSwitchResolution::accepted(PathBuf::from("abc473"));
        let mut home = state(&mut resolve);
        home.open_contest.open();
        for character in "abc473".chars() {
            home.handle_key(key(KeyCode::Char(character), KeyEventKind::Press));
        }
        home.handle_key(key(KeyCode::Enter, KeyEventKind::Press));

        let mut start = || Err::<(), _>("watcher start failed".to_string());
        assert_eq!(home.start_requested_contest(&mut start), None);

        let modal = home.open_contest.modal().expect("Home modal must remain");
        assert_eq!(modal.state, SwitchContestModalState::Failed);
        assert_eq!(modal.error.as_deref(), Some("watcher start failed"));
    }

    #[test]
    fn missing_contest_runs_create_flow_and_requests_handoff() {
        let destination = PathBuf::from("workspace/abc999");
        let resolved_destination = destination.clone();
        let mut resolve =
            move |_contest_id: &str| ContestSwitchResolution::missing(resolved_destination.clone());
        let task = Arc::new(
            move |request: super::super::ContestSwitchRequest,
                  _reporter: &mut dyn crate::ui::Reporter| {
                assert_eq!(
                    request.mutation,
                    super::super::ContestSwitchMutation::Create
                );
                assert_eq!(request.contest_id, "abc999");
                assert_eq!(request.destination, destination);
                Ok(())
            },
        );
        let mut home = HomeState::new(&mut resolve, task);
        home.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        for character in "abc999".chars() {
            home.handle_key(key(KeyCode::Char(character), KeyEventKind::Press));
        }
        home.handle_key(key(KeyCode::Enter, KeyEventKind::Press));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !home.open_requested() && std::time::Instant::now() < deadline {
            home.handle_operation_messages();
            std::thread::yield_now();
        }

        assert!(home.open_requested());
        assert!(home.open_contest.operation.active.is_none());
        let mut start = || Ok::<_, String>("Contest");
        assert_eq!(home.start_requested_contest(&mut start), Some("Contest"));
    }

    #[test]
    fn resolution_and_create_failures_keep_home_error_visible() {
        let mut rejected = |_contest_id: &str| {
            ContestSwitchResolution::rejected(None, "invalid contest ID".to_string())
        };
        let mut home = state(&mut rejected);
        home.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        home.handle_key(key(KeyCode::Char('!'), KeyEventKind::Press));
        assert_eq!(
            home.open_contest.modal().unwrap().error.as_deref(),
            Some("invalid contest ID")
        );
        assert_eq!(
            home.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            HomeAction::None
        );
        assert!(home.open_contest.modal_active());

        let destination = PathBuf::from("workspace/abc999");
        let mut missing =
            move |_contest_id: &str| ContestSwitchResolution::missing(destination.clone());
        let task = Arc::new(
            |_: super::super::ContestSwitchRequest, _: &mut dyn crate::ui::Reporter| {
                Err(std::io::Error::other("fetch failed").into())
            },
        );
        let mut home = HomeState::new(&mut missing, task);
        home.open_contest.open();
        for character in "abc999".chars() {
            home.handle_key(key(KeyCode::Char(character), KeyEventKind::Press));
        }
        home.handle_key(key(KeyCode::Enter, KeyEventKind::Press));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while home.open_contest.operation.active.is_some() && std::time::Instant::now() < deadline {
            home.handle_operation_messages();
            std::thread::yield_now();
        }
        home.handle_operation_messages();

        let modal = home
            .open_contest
            .modal()
            .expect("Home must remain after create failure");
        assert_eq!(modal.state, SwitchContestModalState::Failed);
        assert!(
            modal
                .error
                .as_deref()
                .is_some_and(|error| error.contains("fetch failed"))
        );
        assert!(!home.open_requested());
    }

    #[test]
    fn normal_home_renders_shared_logo_dashboard_and_explicit_workspace() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let home = state(&mut resolve);
        let workspace = Path::new(r"D:\competitive-programming\atcoder");
        let buffer = draw_home(&home, workspace, 80, 24);
        let rendered = buffer_text(&buffer);

        for expected in branding::ascii_logo_lines().chain([
            SUBTITLE,
            "Open Contest",
            "Commands",
            "Shortcuts",
            "Quit",
        ]) {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
        assert!(rendered.contains(r"Workspace  D:\competitive-programming\atcoder"));
        assert!(
            !rendered.contains('┌'),
            "Home unexpectedly has an outer border"
        );
        assert!(
            !rendered.contains('└'),
            "Home unexpectedly has an outer border"
        );
    }

    #[test]
    fn menu_is_one_centered_block_with_aligned_accent_shortcuts() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let home = state(&mut resolve);
        let buffer = draw_home(&home, Path::new("workspace"), 80, 24);
        let layout = home_layout(Rect::new(0, 0, 80, 24));

        assert_eq!(layout.menu.width, MENU_WIDTH);
        assert_eq!(layout.menu.x, (80 - MENU_WIDTH) / 2);
        for (offset, (label, shortcut)) in HOME_ACTIONS.iter().enumerate() {
            let row = layout.menu.y + u16::try_from(offset).unwrap();
            let text = row_text(&buffer, row);
            let start = usize::from(layout.menu.x);
            let end = start + usize::from(layout.menu.width);
            assert_eq!(
                &text[start..end],
                menu_line(label, shortcut, usize::from(MENU_WIDTH)).to_string()
            );

            let shortcut_column = layout.menu.x + layout.menu.width - 1;
            let shortcut_cell = buffer.cell((shortcut_column, row)).unwrap();
            assert_eq!(shortcut_cell.symbol(), *shortcut);
            assert_eq!(shortcut_cell.fg, Color::Yellow);
        }
    }

    #[test]
    fn workspace_path_comes_from_the_explicit_root_instead_of_process_cwd() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let home = state(&mut resolve);
        let explicit_root = Path::new("selected-workspace-marker/root");
        let cwd = std::env::current_dir().unwrap();
        assert_ne!(explicit_root, cwd);

        let rendered = buffer_text(&draw_home(&home, explicit_root, 120, 24));

        assert!(rendered.contains("Workspace  selected-workspace-marker/root"));
        assert!(!rendered.contains(cwd.to_string_lossy().as_ref()));
    }

    #[test]
    fn long_workspace_path_truncates_from_the_start_and_stays_within_width() {
        let workspace = Path::new(
            r"C:\Users\someone\projects\very-long-parent\competitive-programming\atcoder",
        );

        let fitted = workspace_line(workspace, 40);

        assert_eq!(UnicodeWidthStr::width(fitted.as_str()), 40);
        assert!(fitted.starts_with(WORKSPACE_PREFIX));
        assert!(fitted.contains('…'));
        assert!(fitted.ends_with(r"programming\atcoder"));
        assert_eq!(workspace_line(workspace, 0), "");
        assert_eq!(workspace_line(workspace, 1), "…");
    }

    #[test]
    fn unicode_workspace_paths_use_the_production_renderer_and_preserve_the_tail() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let home = state(&mut resolve);
        let width = 30;
        let height = 8;
        let workspace_area = home_layout(Rect::new(0, 0, width, height))
            .workspace
            .expect("workspace row should fit");

        for (workspace, expected_tail) in [
            (
                Path::new(r"C:\Users\ユーザー\very-long-parent\競プロ\atcoder"),
                r"\競プロ\atcoder",
            ),
            (
                Path::new("/home/ユーザー/very-long-parent/競プロ/atcoder"),
                "/競プロ/atcoder",
            ),
        ] {
            let buffer = draw_home(&home, workspace, width, height);
            let rendered = visible_area_text(&buffer, workspace_area)
                .trim()
                .to_string();

            assert!(rendered.starts_with("Workspace  …"), "{rendered:?}");
            assert!(rendered.ends_with(expected_tail), "{rendered:?}");
            assert!(
                UnicodeWidthStr::width(rendered.as_str()) <= usize::from(workspace_area.width),
                "{rendered:?} exceeded {} columns",
                workspace_area.width
            );
            assert!(!rendered.contains('\u{fffd}'), "{rendered:?}");
        }
    }

    #[test]
    fn narrow_and_short_layouts_drop_logo_before_the_core_menu() {
        let narrow = home_layout(Rect::new(0, 0, 20, 24));
        assert_eq!(narrow.logo, None);
        assert_eq!(narrow.menu.height, MENU_HEIGHT);
        assert!(narrow.workspace.is_some());

        let short = home_layout(Rect::new(0, 0, 80, 12));
        assert_eq!(short.logo, None);
        assert_eq!(short.menu.height, MENU_HEIGHT);
        assert!(short.workspace.is_some());

        let menu_only = home_layout(Rect::new(0, 0, 80, 4));
        assert_eq!(menu_only.logo, None);
        assert_eq!(menu_only.subtitle, None);
        assert_eq!(menu_only.workspace, None);
        assert_eq!(menu_only.menu.height, MENU_HEIGHT);
    }

    #[test]
    fn extreme_home_sizes_do_not_panic() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let home = state(&mut resolve);

        for width in [0, 1, 8, 16, 20, 30, 40, 60, 80, 120, 160] {
            for height in [0, 1, 4, 8, 12, 16, 24, 40] {
                let layout = home_layout(Rect::new(0, 0, width, height));
                assert!(layout.menu.width <= width);
                assert!(layout.menu.height <= height);
                let _ = draw_home(&home, Path::new("workspace"), width, height);
            }
        }

        assert_eq!(menu_line("Open Contest", "c", 0).width(), 0);
        assert_eq!(truncate_start_with_ellipsis("workspace", 0), "");
    }

    #[test]
    fn zero_sized_test_backends_reach_the_production_home_renderer() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let home = state(&mut resolve);

        for (width, height) in [(0, 0), (0, 8), (8, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut renderer_called = false;
            terminal
                .draw(|frame| {
                    renderer_called = true;
                    assert_eq!(frame.area(), Rect::new(0, 0, width, height));
                    render(frame, &home, Path::new("workspace"));
                })
                .unwrap();

            assert!(renderer_called, "renderer was skipped for {width}x{height}");
            assert_eq!(terminal.backend().buffer().area.width, width);
            assert_eq!(terminal.backend().buffer().area.height, height);
        }
    }

    #[test]
    fn home_modals_still_render_over_the_borderless_dashboard() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        home.handle_key(key(KeyCode::Char('?'), KeyEventKind::Press));
        let shortcuts = buffer_text(&draw_home(&home, Path::new("workspace"), 80, 24));
        assert!(shortcuts.contains("Shortcuts"));
        assert!(shortcuts.contains("Esc close"));
        assert!(shortcuts.contains('┌'));

        home.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        home.handle_key(key(KeyCode::Char(':'), KeyEventKind::Press));
        let palette = buffer_text(&draw_home(&home, Path::new("workspace"), 80, 24));
        assert!(palette.contains("Command Palette"));
        assert!(palette.contains("Open Contest"));
        assert!(palette.contains("Quit"));
        assert!(!palette.contains("Run Tests"));

        home.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        home.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        let open = buffer_text(&draw_home(&home, Path::new("workspace"), 80, 24));
        assert!(open.contains("Open Contest"));
        assert!(open.contains("Contest:"));
        assert!(open.contains('┌'));
    }

    #[test]
    fn home_palette_selected_row_fills_the_shared_usable_width() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        home.handle_key(key(KeyCode::Char(':'), KeyEventKind::Press));

        let width = 80;
        let height = 24;
        let buffer = draw_home(&home, Path::new("workspace"), width, height);
        let command_count = home.palette.filtered_commands().len();
        let palette_height = 7u16.saturating_add(u16::try_from(command_count).unwrap());
        let area = centered_rect(Rect::new(0, 0, width, height), 52, palette_height);
        let inner = Block::default().borders(Borders::ALL).inner(area);
        let reversed = buffer
            .content()
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.modifier.contains(Modifier::REVERSED))
            .map(|(index, _)| {
                let index = u16::try_from(index).unwrap();
                (index % width, index / width)
            })
            .collect::<Vec<_>>();

        assert_eq!(reversed.len(), usize::from(inner.width));
        let selected_row = reversed.first().unwrap().1;
        assert_eq!(
            reversed,
            (inner.x..inner.right())
                .map(|column| (column, selected_row))
                .collect::<Vec<_>>()
        );
        for column in inner.x..inner.right() {
            assert!(
                buffer
                    .cell((column, selected_row))
                    .unwrap()
                    .modifier
                    .contains(Modifier::BOLD)
            );
            assert!(
                !buffer
                    .cell((column, selected_row.saturating_add(1)))
                    .unwrap()
                    .modifier
                    .contains(Modifier::REVERSED)
            );
        }
        assert!(
            !buffer
                .cell((area.right().saturating_sub(1), selected_row))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );

        home.handle_key(key(KeyCode::Down, KeyEventKind::Press));
        let moved = draw_home(&home, Path::new("workspace"), width, height);
        for column in inner.x..inner.right() {
            assert!(
                !moved
                    .cell((column, selected_row))
                    .unwrap()
                    .modifier
                    .contains(Modifier::REVERSED)
            );
            assert!(
                moved
                    .cell((column, selected_row.saturating_add(1)))
                    .unwrap()
                    .modifier
                    .contains(Modifier::BOLD | Modifier::REVERSED)
            );
        }
    }

    #[test]
    fn home_palette_shared_rows_survive_narrow_and_zero_sized_frames() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        home.handle_key(key(KeyCode::Char(':'), KeyEventKind::Press));

        for width in [0, 1, 2, 8, 16, 30, 52, 80] {
            for height in [0, 1, 4, 8, 12, 24] {
                let _ = draw_home(&home, Path::new("workspace"), width, height);
            }
        }
    }
}
