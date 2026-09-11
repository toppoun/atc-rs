use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::explorer::{self, ExplorerState};
use super::home::{
    centered_rect, centered_row, logo_size, menu_line, truncate_start_with_ellipsis,
};
use super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminalEvent};
use super::{
    ShortcutHelpTransition, TerminaSession, command_matches, is_command_palette_open_key,
    is_shortcut_help_key, shortcut_help_transition, view,
};
use crate::branding;

const GLOBAL_HOME_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_DISCARDED_TRANSITION_EVENTS: usize = 256;
const GLOBAL_HOME_ACTIONS: [(&str, &str); 5] = [
    ("Open", "o"),
    ("Go to Path", "g"),
    ("Commands", ":"),
    ("Shortcuts", "?"),
    ("Quit", "q"),
];
const MENU_WIDTH: u16 = 23;
const MENU_HEIGHT: u16 = GLOBAL_HOME_ACTIONS.len() as u16;
const SUBTITLE: &str = "AtCoder workspace launcher";
const SELECTED_PREFIX: &str = "Selected  ";
const EXPLORER_MIN_WIDTH: u16 = 32;
const EXPLORER_MAX_WIDTH: u16 = 48;
const DASHBOARD_MIN_WIDTH: u16 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GlobalHomeExit {
    Quit,
    OpenWorkspace(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalHomeCommand {
    Open,
    GoToPath,
    Quit,
}

impl GlobalHomeCommand {
    const ALL: [Self; 3] = [Self::Open, Self::GoToPath, Self::Quit];

    const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::GoToPath => "Go to Path",
            Self::Quit => "Quit",
        }
    }

    const fn shortcut(self) -> &'static str {
        match self {
            Self::Open => "o",
            Self::GoToPath => "g",
            Self::Quit => "q",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalHomeErrorKind {
    WorkspaceOpen,
    GoToPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalHomeError {
    kind: GlobalHomeErrorKind,
    message: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct GlobalHomeCommandPalette {
    open: bool,
    query: String,
    selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteKeyResult {
    Handled,
    Execute(GlobalHomeCommand),
}

impl GlobalHomeCommandPalette {
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

    fn filtered_commands(&self) -> Vec<GlobalHomeCommand> {
        GlobalHomeCommand::ALL
            .into_iter()
            .filter(|command| command_matches(command.label(), &self.query))
            .collect()
    }

    fn selected_command(&self) -> Option<GlobalHomeCommand> {
        self.filtered_commands().get(self.selected).copied()
    }

    fn handle_key(&mut self, key: KeyEvent) -> PaletteKeyResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return PaletteKeyResult::Handled;
        }

        match key.code {
            KeyCode::Escape if key.kind == KeyEventKind::Press => {
                self.close();
                PaletteKeyResult::Handled
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => self
                .selected_command()
                .map(PaletteKeyResult::Execute)
                .unwrap_or(PaletteKeyResult::Handled),
            KeyCode::Backspace => {
                if let Some((start, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(start);
                }
                self.selected = 0;
                PaletteKeyResult::Handled
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
                PaletteKeyResult::Handled
            }
            KeyCode::Down => {
                let count = self.filtered_commands().len();
                self.selected = if count <= 1 {
                    0
                } else {
                    (self.selected + 1) % count
                };
                PaletteKeyResult::Handled
            }
            KeyCode::Char(character)
                if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
            {
                self.query.push(character);
                self.selected = 0;
                PaletteKeyResult::Handled
            }
            _ => PaletteKeyResult::Handled,
        }
    }

    fn handle_paste(&mut self, text: &str) {
        self.query.extend(
            text.chars()
                .filter(|character| !matches!(character, '\r' | '\n')),
        );
        self.selected = 0;
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PathInputModal {
    value: String,
    cursor: usize,
}

impl PathInputModal {
    fn previous_grapheme_boundary(&self) -> usize {
        self.value[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(start, _)| start)
    }

    fn next_grapheme_boundary(&self) -> usize {
        let tail = &self.value[self.cursor..];
        tail.grapheme_indices(true)
            .nth(1)
            .map_or(self.value.len(), |(next, _)| self.cursor + next)
    }

    fn insert_char(&mut self, character: char) {
        self.value.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn insert_text(&mut self, text: &str) {
        let single_line: String = text
            .chars()
            .filter(|character| !matches!(character, '\r' | '\n'))
            .collect();
        self.value.insert_str(self.cursor, &single_line);
        self.cursor += single_line.len();
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        match key.code {
            KeyCode::Backspace => {
                let previous = self.previous_grapheme_boundary();
                self.value.drain(previous..self.cursor);
                self.cursor = previous;
            }
            KeyCode::Delete => {
                let next = self.next_grapheme_boundary();
                self.value.drain(self.cursor..next);
            }
            KeyCode::Left => self.cursor = self.previous_grapheme_boundary(),
            KeyCode::Right => self.cursor = self.next_grapheme_boundary(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            KeyCode::Char(character)
                if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
            {
                self.insert_char(character);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlobalHomeState {
    explorer: ExplorerState,
    path_input: Option<PathInputModal>,
    error: Option<GlobalHomeError>,
    shortcut_help_visible: bool,
    palette: GlobalHomeCommandPalette,
    explorer_overlay_visible: bool,
    explorer_pane_visible: bool,
}

impl GlobalHomeState {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            explorer: ExplorerState::new(root),
            path_input: None,
            error: None,
            shortcut_help_visible: false,
            palette: GlobalHomeCommandPalette::default(),
            explorer_overlay_visible: false,
            explorer_pane_visible: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn explorer_root(&self) -> &Path {
        self.explorer.root()
    }

    #[cfg(test)]
    pub(crate) fn explorer_selected_path(&self) -> &Path {
        self.explorer.selected_path()
    }

    pub(crate) fn show_workspace_open_error(&mut self, message: String) {
        self.path_input = None;
        self.palette.close();
        self.shortcut_help_visible = false;
        self.error = Some(GlobalHomeError {
            kind: GlobalHomeErrorKind::WorkspaceOpen,
            message,
        });
    }

    fn open_path_input(&mut self) {
        self.path_input = Some(PathInputModal::default());
        self.palette.close();
        self.shortcut_help_visible = false;
    }

    fn selected_open_exit(&self) -> GlobalHomeExit {
        GlobalHomeExit::OpenWorkspace(self.explorer.selected_path().to_path_buf())
    }

    fn request_open(&mut self) -> Option<GlobalHomeExit> {
        self.palette.close();
        if self.explorer_pane_visible {
            Some(self.selected_open_exit())
        } else {
            self.explorer_overlay_visible = true;
            None
        }
    }

    fn submit_path_input(&mut self, input: PathInputModal) {
        let candidate = resolve_explorer_path(self.explorer.root(), &input.value);
        let mut rebased = self.explorer.clone();
        if rebased.rebase(candidate.clone()) {
            self.explorer = rebased;
        } else {
            self.error = Some(GlobalHomeError {
                kind: GlobalHomeErrorKind::GoToPath,
                message: rebased.status().map_or_else(
                    || format!("Could not go to {}", candidate.display()),
                    str::to_owned,
                ),
            });
        }
    }

    fn handle_explorer_navigation(&mut self, key: KeyEvent) -> bool {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return false;
        }

        match key.code {
            KeyCode::Down => self.explorer.select_next(),
            KeyCode::Char('j') if has_plain_modifiers(key) => self.explorer.select_next(),
            KeyCode::Up => self.explorer.select_previous(),
            KeyCode::Char('k') if has_plain_modifiers(key) => self.explorer.select_previous(),
            KeyCode::Right => self.explorer.select_right(),
            KeyCode::Char('l') if has_plain_modifiers(key) => self.explorer.select_right(),
            KeyCode::Left => self.explorer.select_left(),
            KeyCode::Char('h') if has_plain_modifiers(key) => self.explorer.select_left(),
            KeyCode::Enter if key.kind == KeyEventKind::Press => self.explorer.toggle_selected(),
            KeyCode::Backspace if key.kind == KeyEventKind::Press => {
                self.explorer.rebase_to_parent()
            }
            KeyCode::Char('r') if key.kind == KeyEventKind::Press && has_plain_modifiers(key) => {
                self.explorer.reload_selected()
            }
            _ => return false,
        };
        true
    }

    fn handle_explorer_overlay_key(&mut self, key: KeyEvent) -> Option<GlobalHomeExit> {
        if key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Escape => {
                    self.explorer_overlay_visible = false;
                    return None;
                }
                KeyCode::Char('o') if has_plain_modifiers(key) => {
                    return Some(self.selected_open_exit());
                }
                KeyCode::Char('g') if has_plain_modifiers(key) => {
                    self.open_path_input();
                    return None;
                }
                _ => {}
            }
        }

        self.handle_explorer_navigation(key);
        None
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<GlobalHomeExit> {
        if self.error.is_some() {
            if key.kind == KeyEventKind::Press
                && matches!(key.code, KeyCode::Enter | KeyCode::Escape)
            {
                self.error = None;
            }
            return None;
        }

        if self.path_input.is_some() {
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Escape {
                self.path_input = None;
                return None;
            }
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
                let input = self
                    .path_input
                    .take()
                    .expect("active path input must remain present until Enter");
                self.submit_path_input(input);
                return None;
            }
            self.path_input
                .as_mut()
                .expect("active path input must remain present")
                .handle_edit_key(key);
            return None;
        }

        if self.palette.is_active() {
            return match self.palette.handle_key(key) {
                PaletteKeyResult::Execute(GlobalHomeCommand::Open) => self.request_open(),
                PaletteKeyResult::Execute(GlobalHomeCommand::GoToPath) => {
                    self.open_path_input();
                    None
                }
                PaletteKeyResult::Execute(GlobalHomeCommand::Quit) => {
                    self.palette.close();
                    Some(GlobalHomeExit::Quit)
                }
                PaletteKeyResult::Handled => None,
            };
        }

        match shortcut_help_transition(self.shortcut_help_visible, key) {
            ShortcutHelpTransition::KeepAndConsume => return None,
            ShortcutHelpTransition::DismissAndConsume => {
                self.shortcut_help_visible = false;
                return None;
            }
            ShortcutHelpTransition::DismissAndPassThrough => {
                self.shortcut_help_visible = false;
            }
            ShortcutHelpTransition::PassThrough => {}
        }

        if self.explorer_overlay_visible {
            return self.handle_explorer_overlay_key(key);
        }

        if key.kind != KeyEventKind::Press {
            if self.explorer_pane_visible {
                self.handle_explorer_navigation(key);
            }
            return None;
        }
        if is_shortcut_help_key(key) {
            self.shortcut_help_visible = true;
            None
        } else if is_command_palette_open_key(key) {
            self.palette.open();
            None
        } else {
            match key.code {
                KeyCode::Char('o') if has_plain_modifiers(key) => self.request_open(),
                KeyCode::Char('g') if has_plain_modifiers(key) => {
                    self.open_path_input();
                    None
                }
                KeyCode::Char('q') if has_plain_modifiers(key) => Some(GlobalHomeExit::Quit),
                _ => {
                    if self.explorer_pane_visible {
                        self.handle_explorer_navigation(key);
                    }
                    None
                }
            }
        }
    }

    fn handle_paste(&mut self, text: &str) {
        if self.error.is_some() {
            return;
        }
        if let Some(input) = self.path_input.as_mut() {
            input.insert_text(text);
        } else if self.palette.is_active() {
            self.palette.handle_paste(text);
        }
    }
}

fn has_plain_modifiers(key: KeyEvent) -> bool {
    !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key
}

pub(crate) fn resolve_explorer_path(current_root: &Path, input: &str) -> PathBuf {
    if input.is_empty() {
        return current_root.to_path_buf();
    }

    let input = PathBuf::from(input);
    if input.is_absolute() {
        input
    } else {
        current_root.join(input)
    }
}

pub(crate) trait GlobalHomeTerminal {
    fn draw_global_home(&mut self, render: &mut dyn FnMut(&mut Frame<'_>)) -> io::Result<()>;
    fn finish_global_home_redraw(&mut self) -> io::Result<()>;
    fn note_global_home_resize(&mut self);
    fn poll_global_home(&mut self, wait: Duration) -> io::Result<bool>;
    fn read_global_home(&mut self) -> io::Result<TerminalEvent>;

    fn discard_global_home_input_batch(&mut self) -> io::Result<()> {
        for _ in 0..MAX_DISCARDED_TRANSITION_EVENTS {
            if !self.poll_global_home(Duration::ZERO)? {
                break;
            }
            let _ = self.read_global_home()?;
        }
        Ok(())
    }
}

impl GlobalHomeTerminal for TerminaSession {
    fn draw_global_home(&mut self, render: &mut dyn FnMut(&mut Frame<'_>)) -> io::Result<()> {
        self.draw(|frame| render(frame))
    }

    fn finish_global_home_redraw(&mut self) -> io::Result<()> {
        self.note_redraw_completed();
        self.refresh_mouse_after_redraw(false)?;
        self.retry_high_res_after_redraw(false)
    }

    fn note_global_home_resize(&mut self) {
        self.note_resize_dispatched();
    }

    fn poll_global_home(&mut self, wait: Duration) -> io::Result<bool> {
        self.poll(wait)
    }

    fn read_global_home(&mut self) -> io::Result<TerminalEvent> {
        self.read()
    }
}

pub(crate) fn run_with_terminal(
    terminal: &mut impl GlobalHomeTerminal,
    state: &mut GlobalHomeState,
) -> io::Result<GlobalHomeExit> {
    let mut dirty = true;

    loop {
        if dirty {
            terminal.draw_global_home(&mut |frame| render(frame, state))?;
            terminal.finish_global_home_redraw()?;
            dirty = false;
        }

        if !terminal.poll_global_home(GLOBAL_HOME_POLL_INTERVAL)? {
            continue;
        }

        let exit = match terminal.read_global_home()? {
            TerminalEvent::Key(key) => state.handle_key(key),
            TerminalEvent::Paste(text) => {
                state.handle_paste(&text);
                None
            }
            TerminalEvent::Resize(_) => {
                terminal.note_global_home_resize();
                None
            }
            TerminalEvent::Pointer(_) | TerminalEvent::Ignored => None,
        };
        if let Some(exit) = exit {
            return Ok(exit);
        }
        dirty = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GlobalHomeLayout {
    logo: Option<Rect>,
    subtitle: Option<Rect>,
    menu: Rect,
    footer: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GlobalHomePanes {
    explorer: Option<Rect>,
    dashboard: Rect,
}

fn global_home_panes(area: Rect) -> GlobalHomePanes {
    let proportional = area.width.saturating_mul(30) / 100;
    let explorer_width = proportional.clamp(EXPLORER_MIN_WIDTH, EXPLORER_MAX_WIDTH);
    if area.width.saturating_sub(explorer_width) < DASHBOARD_MIN_WIDTH {
        return GlobalHomePanes {
            explorer: None,
            dashboard: area,
        };
    }

    GlobalHomePanes {
        explorer: Some(Rect::new(area.x, area.y, explorer_width, area.height)),
        dashboard: Rect::new(
            area.x.saturating_add(explorer_width),
            area.y,
            area.width.saturating_sub(explorer_width),
            area.height,
        ),
    }
}

fn global_home_layout(area: Rect) -> GlobalHomeLayout {
    if area.width == 0 || area.height == 0 {
        return GlobalHomeLayout {
            logo: None,
            subtitle: None,
            menu: Rect::new(area.x, area.y, 0, 0),
            footer: None,
        };
    }

    let show_footer = area.height >= MENU_HEIGHT.saturating_add(1);
    let bottom_margin = u16::from(show_footer && area.height >= MENU_HEIGHT.saturating_add(4));
    let footer_y = area
        .y
        .saturating_add(area.height)
        .saturating_sub(1)
        .saturating_sub(bottom_margin);
    let body_height = if show_footer {
        footer_y.saturating_sub(area.y)
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

    let footer = show_footer.then(|| {
        let margin = u16::from(area.width >= 4);
        Rect::new(
            area.x.saturating_add(margin),
            footer_y,
            area.width.saturating_sub(margin.saturating_mul(2)),
            1,
        )
    });

    GlobalHomeLayout {
        logo,
        subtitle,
        menu: centered_row(area, menu_y, MENU_WIDTH, MENU_HEIGHT.min(body_height)),
        footer,
    }
}

fn prefixed_path_line(prefix: &str, path: &Path, width: usize) -> String {
    let path = path.to_string_lossy();
    let full = format!("{prefix}{path}");
    if UnicodeWidthStr::width(full.as_str()) <= width {
        return full;
    }

    let prefix_width = UnicodeWidthStr::width(prefix);
    if width <= prefix_width {
        truncate_start_with_ellipsis(&full, width)
    } else {
        format!(
            "{prefix}{}",
            truncate_start_with_ellipsis(&path, width - prefix_width)
        )
    }
}

fn render(frame: &mut Frame<'_>, state: &mut GlobalHomeState) {
    let panes = global_home_panes(frame.area());
    state.explorer_pane_visible = panes.explorer.is_some();
    if state.explorer_pane_visible {
        state.explorer_overlay_visible = false;
    }

    if let Some(area) = panes.explorer {
        explorer::render(frame, area, &mut state.explorer);
    }

    let layout = global_home_layout(panes.dashboard);

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
        let lines = GLOBAL_HOME_ACTIONS
            .iter()
            .map(|(label, shortcut)| menu_line(label, shortcut, usize::from(layout.menu.width)));
        frame.render_widget(Paragraph::new(Text::from_iter(lines)), layout.menu);
    }
    if let Some(area) = layout.footer {
        frame.render_widget(
            Paragraph::new(prefixed_path_line(
                SELECTED_PREFIX,
                state.explorer.selected_path(),
                usize::from(area.width),
            ))
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
            area,
        );
    }

    if state.explorer_overlay_visible {
        render_explorer_overlay(frame, &mut state.explorer);
    }

    if state.shortcut_help_visible {
        render_shortcuts(frame, state.explorer_pane_visible);
    }
    if state.palette.is_active() {
        render_palette(frame, &state.palette);
    }
    if let Some(input) = state.path_input.as_ref() {
        render_path_input(frame, state.explorer.root(), input);
    }
    if let Some(error) = state.error.as_ref() {
        render_error(frame, error);
    }
}

fn render_shortcuts(frame: &mut Frame<'_>, explorer_pane_visible: bool) {
    let area = centered_rect(frame.area(), 46, 14);
    let open_help = if explorer_pane_visible {
        "o         Open"
    } else {
        "o         Explorer, then Open"
    };
    let lines = vec![
        Line::raw(open_help),
        Line::raw("g         Go to Path"),
        Line::raw("Enter     Expand / Collapse"),
        Line::raw("↑↓ jk     Navigate"),
        Line::raw("←→ hl     Collapse / Expand"),
        Line::raw("Backspace Parent"),
        Line::raw("r         Reload"),
        Line::raw(":         Commands"),
        Line::raw("?         Shortcuts"),
        Line::raw("q         Quit"),
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

fn render_explorer_overlay(frame: &mut Frame<'_>, state: &mut ExplorerState) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let help_height = u16::from(area.height >= 2);
    let explorer_area = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(help_height),
    );
    explorer::render(frame, explorer_area, state);
    if help_height > 0 {
        let help_area = Rect::new(
            area.x,
            area.y.saturating_add(area.height.saturating_sub(1)),
            area.width,
            1,
        );
        frame.render_widget(
            Paragraph::new("o Open   g Go to Path   Esc Close")
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            help_area,
        );
    }
}

fn render_palette(frame: &mut Frame<'_>, palette: &GlobalHomeCommandPalette) {
    let commands = palette.filtered_commands();
    let height = 7u16.saturating_add(u16::try_from(commands.len()).unwrap_or(u16::MAX));
    let area = centered_rect(frame.area(), 52, height);
    let block = Block::default()
        .title(" Command Palette ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    let query_height = inner.height.min(2);
    let help_height = inner.height.saturating_sub(query_height).min(2);
    let list_area = Rect::new(
        inner.x,
        inner.y.saturating_add(query_height),
        inner.width,
        inner.height.saturating_sub(query_height + help_height),
    );
    let list_area = view::command_palette_list_area(list_area, None);
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
    frame.render_widget(block, area);
    if query_height > 0 {
        frame.render_widget(
            Paragraph::new(format!("> {}", palette.query)),
            Rect::new(inner.x, inner.y, inner.width, query_height),
        );
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), list_area);
    if help_height > 0 {
        frame.render_widget(
            Paragraph::new("[↑↓] Select   [Enter] Run   [Esc] Cancel"),
            Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(inner.height.saturating_sub(help_height)),
                inner.width,
                help_height,
            ),
        );
    }
}

fn render_path_input(frame: &mut Frame<'_>, current_root: &Path, input: &PathInputModal) {
    let area = centered_rect(frame.area(), 64, 9);
    let block = Block::default().title(" Go to Path ").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let width = usize::from(inner.width);
    let current = prefixed_path_line("Current root: ", current_root, width);
    let path = prefixed_text_line("Path: ", &input.value, width);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::raw(current),
            Line::raw(""),
            Line::raw(path),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(" Go        "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw(" Cancel"),
            ]),
        ])),
        inner,
    );
}

fn prefixed_text_line(prefix: &str, value: &str, width: usize) -> String {
    let full = format!("{prefix}{value}");
    if UnicodeWidthStr::width(full.as_str()) <= width {
        return full;
    }
    let prefix_width = UnicodeWidthStr::width(prefix);
    if width <= prefix_width {
        truncate_start_with_ellipsis(&full, width)
    } else {
        format!(
            "{prefix}{}",
            truncate_start_with_ellipsis(value, width - prefix_width)
        )
    }
}

fn render_error(frame: &mut Frame<'_>, error: &GlobalHomeError) {
    let area = centered_rect(frame.area(), 64, 9);
    let title = match error.kind {
        GlobalHomeErrorKind::WorkspaceOpen => " Workspace Open Failed ",
        GlobalHomeErrorKind::GoToPath => " Go to Path Failed ",
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(format!("{}\n\nEnter / Esc  Dismiss", error.message))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use super::*;
    use crate::tui::terminal::Modifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::default(),
        }
    }

    fn draw(state: &mut GlobalHomeState, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    struct ScriptedGlobalTerminal {
        batches: VecDeque<VecDeque<TerminalEvent>>,
        active_batch: VecDeque<TerminalEvent>,
        width: u16,
        height: u16,
        frames: Vec<String>,
        reads: usize,
    }

    impl ScriptedGlobalTerminal {
        fn new(
            width: u16,
            height: u16,
            batches: impl IntoIterator<Item = Vec<TerminalEvent>>,
        ) -> Self {
            Self {
                batches: batches.into_iter().map(VecDeque::from).collect(),
                active_batch: VecDeque::new(),
                width,
                height,
                frames: Vec::new(),
                reads: 0,
            }
        }
    }

    impl GlobalHomeTerminal for ScriptedGlobalTerminal {
        fn draw_global_home(&mut self, render: &mut dyn FnMut(&mut Frame<'_>)) -> io::Result<()> {
            let mut terminal = Terminal::new(TestBackend::new(self.width, self.height)).unwrap();
            terminal.draw(|frame| render(frame)).unwrap();
            self.frames.push(buffer_text(terminal.backend().buffer()));
            Ok(())
        }

        fn finish_global_home_redraw(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn note_global_home_resize(&mut self) {}

        fn poll_global_home(&mut self, wait: Duration) -> io::Result<bool> {
            if !self.active_batch.is_empty() {
                return Ok(true);
            }
            if wait == Duration::ZERO {
                return Ok(false);
            }
            let Some(batch) = self.batches.pop_front() else {
                return Err(io::Error::other("scripted Global Home input exhausted"));
            };
            self.active_batch = batch;
            Ok(!self.active_batch.is_empty())
        }

        fn read_global_home(&mut self) -> io::Result<TerminalEvent> {
            self.reads = self.reads.saturating_add(1);
            self.active_batch
                .pop_front()
                .ok_or_else(|| io::Error::other("scripted Global Home batch is empty"))
        }
    }

    fn event(code: KeyCode) -> TerminalEvent {
        TerminalEvent::Key(key(code))
    }

    fn repeat_event(code: KeyCode) -> TerminalEvent {
        TerminalEvent::Key(KeyEvent {
            code,
            kind: KeyEventKind::Repeat,
            modifiers: Modifiers::default(),
        })
    }

    #[test]
    fn production_dashboard_and_explorer_show_phase_3b_2_actions_and_selection() {
        let mut state = GlobalHomeState::new(PathBuf::from(r"D:\current\directory"));
        let rendered = buffer_text(&draw(&mut state, 80, 24));

        for expected in branding::ascii_logo_lines().chain([
            SUBTITLE,
            "Explorer",
            "Open",
            "Go to Path",
            "Commands",
            "Shortcuts",
            "Quit",
            r"Selected  D:\current\directory",
        ]) {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
        assert!(!rendered.contains("Browse Directories"));
        assert!(!rendered.contains("Recent Workspaces"));
    }

    #[test]
    fn explorer_path_resolution_preserves_empty_relative_and_absolute_semantics() {
        let current = tempfile::tempdir().unwrap();
        let absolute = tempfile::tempdir().unwrap();

        assert_eq!(resolve_explorer_path(current.path(), ""), current.path());
        assert_eq!(
            resolve_explorer_path(current.path(), "workspace"),
            current.path().join("workspace")
        );
        assert_eq!(
            resolve_explorer_path(current.path(), "workspace with spaces"),
            current.path().join("workspace with spaces")
        );
        assert_eq!(
            resolve_explorer_path(current.path(), absolute.path().to_str().unwrap()),
            absolute.path()
        );
        assert_eq!(
            resolve_explorer_path(current.path(), "."),
            current.path().join(".")
        );
        assert_eq!(
            resolve_explorer_path(current.path(), ".."),
            current.path().join("..")
        );

        #[cfg(windows)]
        assert_eq!(
            resolve_explorer_path(current.path(), r"D:\My Projects\atcoder"),
            PathBuf::from(r"D:\My Projects\atcoder")
        );
    }

    #[test]
    fn empty_go_to_path_rebases_the_original_pathbuf_without_display_roundtrip() {
        let current = tempfile::tempdir().unwrap();
        let original = current.path().to_path_buf();
        let mut state = GlobalHomeState::new(original.clone());
        state.handle_key(key(KeyCode::Char('g')));

        assert_eq!(state.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(state.explorer.root(), original);
        assert_eq!(state.explorer.selected_path(), original);
        assert!(state.path_input.is_none());
    }

    #[test]
    fn path_modal_owns_q_colon_and_question_mark_until_escape() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char('g')));
        for character in ['q', ':', '?'] {
            assert_eq!(state.handle_key(key(KeyCode::Char(character))), None);
        }
        assert_eq!(state.path_input.as_ref().unwrap().value, "q:?");

        state.handle_key(key(KeyCode::Escape));
        assert!(state.path_input.is_none());
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q'))),
            Some(GlobalHomeExit::Quit)
        );
    }

    #[test]
    fn palette_searches_open_go_to_path_and_quit_and_runs_each_owner() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char(':')));
        for character in "path".chars() {
            state.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            state.palette.filtered_commands(),
            [GlobalHomeCommand::GoToPath]
        );

        state.handle_key(key(KeyCode::Enter));
        assert!(state.path_input.is_some());
        assert!(!state.palette.is_active());

        state.handle_key(key(KeyCode::Escape));
        let _ = draw(&mut state, 80, 24);
        state.handle_key(key(KeyCode::Char(':')));
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Some(GlobalHomeExit::OpenWorkspace(PathBuf::from("root")))
        );
    }

    #[test]
    fn error_modal_has_highest_precedence_and_dismisses_with_enter_or_escape() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.show_workspace_open_error("Not an atc workspace: root/missing".to_string());

        state.handle_key(key(KeyCode::Char('q')));
        assert!(state.error.is_some());
        assert_eq!(state.handle_key(key(KeyCode::Enter)), None);
        assert!(state.error.is_none());

        state.handle_key(key(KeyCode::Char('g')));
        assert!(state.path_input.is_some());
    }

    #[test]
    fn shortcut_help_matches_global_actions_and_is_one_shot() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char('?')));
        let rendered = buffer_text(&draw(&mut state, 80, 24));
        for expected in [
            "o         Open",
            "g         Go to Path",
            "Enter     Expand / Collapse",
            "↑↓ jk     Navigate",
            "←→ hl     Collapse / Expand",
            "Backspace Parent",
            "r         Reload",
            ":         Commands",
            "?         Shortcuts",
            "q         Quit",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }

        let exit = state.handle_key(key(KeyCode::Char('o')));
        assert!(!state.shortcut_help_visible);
        assert_eq!(
            exit,
            Some(GlobalHomeExit::OpenWorkspace(PathBuf::from("root")))
        );
    }

    #[test]
    fn palette_selection_highlights_the_full_usable_row_with_unicode_width() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char(':')));
        let buffer = draw(&mut state, 80, 24);
        let palette_area = centered_rect(Rect::new(0, 0, 80, 24), 52, 10);
        let inner = Block::default().borders(Borders::ALL).inner(palette_area);
        let list_area = view::command_palette_list_area(
            Rect::new(
                inner.x,
                inner.y.saturating_add(2),
                inner.width,
                inner.height.saturating_sub(4),
            ),
            None,
        );
        for column in list_area.x..list_area.x.saturating_add(list_area.width) {
            assert!(
                buffer
                    .cell((column, list_area.y))
                    .unwrap()
                    .modifier
                    .contains(Modifier::REVERSED),
                "column {column} was outside the selected-row highlight"
            );
        }
        assert!(
            !buffer
                .cell((palette_area.x, list_area.y))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn unicode_and_tiny_layouts_do_not_panic_and_keep_the_directory_tail_when_possible() {
        let mut state = GlobalHomeState::new(PathBuf::from(
            r"C:\Users\ユーザー\very-long-parent\競プロ\atcoder",
        ));

        for (width, height) in [(0, 0), (1, 1), (12, 4), (30, 8), (80, 24)] {
            let _ = draw(&mut state, width, height);
        }

        let line = prefixed_path_line(SELECTED_PREFIX, state.explorer.selected_path(), 30);
        assert!(UnicodeWidthStr::width(line.as_str()) <= 30);
        assert!(line.ends_with(r"\競プロ\atcoder"));
        assert!(!line.contains('\u{fffd}'));
    }

    #[test]
    fn path_and_error_modals_render_safely_in_small_terminals() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char('g')));
        state.handle_paste("relative/workspace");
        for (width, height) in [(0, 0), (1, 1), (8, 3), (24, 5), (80, 24)] {
            let _ = draw(&mut state, width, height);
        }

        state.show_workspace_open_error("failed".to_string());
        for (width, height) in [(0, 0), (1, 1), (8, 3), (24, 5), (80, 24)] {
            let _ = draw(&mut state, width, height);
        }
    }

    #[test]
    fn responsive_panes_apply_threshold_clamp_centering_separator_and_selected_footer() {
        for (width, expected_explorer, expected_dashboard) in [
            (61, None, 61),
            (62, Some(32), 30),
            (80, Some(32), 48),
            (120, Some(36), 84),
            (160, Some(48), 112),
        ] {
            let area = Rect::new(0, 0, width, 24);
            let panes = global_home_panes(area);
            assert_eq!(panes.explorer.map(|area| area.width), expected_explorer);
            assert_eq!(panes.dashboard.width, expected_dashboard);
            assert_eq!(
                panes.dashboard.x,
                expected_explorer.unwrap_or_default(),
                "unexpected dashboard origin at width {width}"
            );
            let layout = global_home_layout(panes.dashboard);
            assert_eq!(
                layout.menu.x,
                panes
                    .dashboard
                    .x
                    .saturating_add(panes.dashboard.width.saturating_sub(MENU_WIDTH) / 2),
                "menu was not centered inside the right pane at width {width}"
            );

            let mut state = GlobalHomeState::new(PathBuf::from("selected-root"));
            let buffer = draw(&mut state, width, 24);
            let text = buffer_text(&buffer);
            assert!(text.contains("Selected  selected-root"));
            if let Some(explorer_width) = expected_explorer {
                assert!(text.contains("Explorer"));
                assert_eq!(buffer.cell((explorer_width - 1, 0)).unwrap().symbol(), "│");
            } else {
                assert!(!text.contains("Explorer"));
            }
        }
    }

    #[test]
    fn narrow_open_uses_full_screen_overlay_and_resize_to_wide_closes_it_without_state_loss() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("a-child");
        std::fs::create_dir(&child).unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());

        let dashboard = buffer_text(&draw(&mut state, 61, 12));
        assert!(!dashboard.contains("Explorer"));
        assert_eq!(state.handle_key(key(KeyCode::Char('o'))), None);
        assert!(state.explorer_overlay_visible);

        let overlay = buffer_text(&draw(&mut state, 61, 12));
        assert!(overlay.contains("Explorer"));
        assert!(overlay.contains("o Open   g Go to Path   Esc Close"));
        state.handle_key(key(KeyCode::Enter));
        state.handle_key(key(KeyCode::Char('j')));
        assert_eq!(state.explorer.selected_path(), child);

        let _ = draw(&mut state, 80, 12);
        assert!(!state.explorer_overlay_visible);
        assert_eq!(state.explorer.selected_path(), child);
    }

    #[test]
    fn explorer_overlay_renderer_survives_zero_very_narrow_and_very_short_geometry() {
        for (width, height) in [(0, 0), (1, 1), (4, 2), (20, 3)] {
            let mut state = GlobalHomeState::new(PathBuf::from("root"));
            state.explorer_overlay_visible = true;
            let _ = draw(&mut state, width, height);
        }
    }

    #[test]
    fn production_loop_routes_tree_navigation_and_opens_the_selected_directory() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("a-workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("nested")).unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            24,
            [
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('j'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('o'))],
            ],
        );

        assert_eq!(
            run_with_terminal(&mut terminal, &mut state).unwrap(),
            GlobalHomeExit::OpenWorkspace(workspace.clone())
        );
        assert_eq!(state.explorer.selected_path(), workspace);
        assert!(terminal.frames.iter().any(|frame| frame.contains("nested")));
    }

    #[test]
    fn production_loop_go_to_path_handles_relative_then_absolute_without_opening_or_chdir() {
        let root = tempfile::tempdir().unwrap();
        let relative = root.path().join("relative");
        std::fs::create_dir(&relative).unwrap();
        let absolute = tempfile::tempdir().unwrap();
        let cwd_before = std::env::current_dir().unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            24,
            [
                vec![event(KeyCode::Char('g'))],
                vec![TerminalEvent::Paste("relative".to_string())],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('g'))],
                vec![TerminalEvent::Paste(
                    absolute.path().to_string_lossy().into_owned(),
                )],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal(&mut terminal, &mut state).unwrap(),
            GlobalHomeExit::Quit
        );
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| { frame.contains(relative.to_string_lossy().as_ref()) })
        );
        assert_eq!(state.explorer.root(), absolute.path());
        assert_eq!(state.explorer.selected_path(), absolute.path());
        assert_eq!(std::env::current_dir().unwrap(), cwd_before);
    }

    #[test]
    fn go_to_path_failure_closes_input_shows_global_error_and_preserves_entire_explorer() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("child")).unwrap();
        let file = root.path().join("file.txt");
        std::fs::write(&file, "not a directory").unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        state.handle_key(key(KeyCode::Enter));
        state.handle_key(key(KeyCode::Char('j')));
        let old_explorer = state.explorer.clone();

        state.handle_key(key(KeyCode::Char('g')));
        state.handle_paste(file.to_string_lossy().as_ref());
        assert_eq!(state.handle_key(key(KeyCode::Enter)), None);

        assert_eq!(state.explorer, old_explorer);
        assert!(state.path_input.is_none());
        assert!(matches!(
            state.error.as_ref().map(|error| error.kind),
            Some(GlobalHomeErrorKind::GoToPath)
        ));
        assert!(
            state
                .error
                .as_ref()
                .unwrap()
                .message
                .contains("path is not a directory")
        );
        let rendered = buffer_text(&draw(&mut state, 80, 24));
        assert!(rendered.contains("Go to Path Failed"));
    }

    #[test]
    fn open_and_modal_or_overlay_transitions_preserve_same_batch_ownership() {
        let root = tempfile::tempdir().unwrap();

        let mut wide_state = GlobalHomeState::new(root.path().to_path_buf());
        let mut wide_terminal = ScriptedGlobalTerminal::new(
            80,
            24,
            [vec![event(KeyCode::Char('o')), event(KeyCode::Char('q'))]],
        );
        assert_eq!(
            run_with_terminal(&mut wide_terminal, &mut wide_state).unwrap(),
            GlobalHomeExit::OpenWorkspace(root.path().to_path_buf())
        );
        assert_eq!(wide_terminal.reads, 1);

        let mut narrow_state = GlobalHomeState::new(root.path().to_path_buf());
        let mut narrow_terminal = ScriptedGlobalTerminal::new(
            61,
            24,
            [
                vec![event(KeyCode::Char('o')), event(KeyCode::Char('q'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        assert_eq!(
            run_with_terminal(&mut narrow_terminal, &mut narrow_state).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(narrow_terminal.reads, 4);

        let mut modal_state = GlobalHomeState::new(root.path().to_path_buf());
        let mut modal_terminal = ScriptedGlobalTerminal::new(
            80,
            24,
            [
                vec![event(KeyCode::Char('g')), event(KeyCode::Char('q'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        assert_eq!(
            run_with_terminal(&mut modal_terminal, &mut modal_state).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(modal_terminal.reads, 4);
        assert!(
            modal_terminal
                .frames
                .iter()
                .any(|frame| frame.contains("Path: q"))
        );
    }

    #[test]
    fn production_narrow_palette_open_opens_the_explorer_overlay_before_opening() {
        let root = tempfile::tempdir().unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            61,
            24,
            [
                vec![event(KeyCode::Char(':'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal(&mut terminal, &mut state).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.reads, 4);
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("o Open   g Go to Path   Esc Close"))
        );
    }

    #[test]
    fn production_narrow_palette_open_gives_same_batch_q_to_the_overlay() {
        let root = tempfile::tempdir().unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            61,
            24,
            [
                vec![event(KeyCode::Char(':'))],
                vec![event(KeyCode::Enter), event(KeyCode::Char('q'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal(&mut terminal, &mut state).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.reads, 5);
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("Explorer"))
        );
    }

    #[test]
    fn production_wide_palette_open_returns_without_discarding_same_batch_input() {
        let root = tempfile::tempdir().unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            80,
            24,
            [
                vec![event(KeyCode::Char(':'))],
                vec![event(KeyCode::Enter), event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal(&mut terminal, &mut state).unwrap(),
            GlobalHomeExit::OpenWorkspace(root.path().to_path_buf())
        );
        assert_eq!(terminal.reads, 2);
    }

    #[test]
    fn production_narrow_hidden_enter_keeps_the_root_collapsed_and_unloaded() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("child")).unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        let old_explorer = state.explorer.clone();
        let mut terminal = ScriptedGlobalTerminal::new(
            61,
            24,
            [vec![event(KeyCode::Enter)], vec![event(KeyCode::Char('q'))]],
        );

        assert_eq!(
            run_with_terminal(&mut terminal, &mut state).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(state.explorer, old_explorer);
    }

    #[test]
    fn production_narrow_hidden_navigation_and_repeat_leave_explorer_and_cwd_unchanged() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("a-child")).unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        assert!(state.explorer.toggle_selected());
        std::fs::create_dir(root.path().join("b-child-added-after-load")).unwrap();
        let old_explorer = state.explorer.clone();
        let cwd_before = std::env::current_dir().unwrap();
        let mut terminal = ScriptedGlobalTerminal::new(
            61,
            24,
            [
                vec![repeat_event(KeyCode::Char('j'))],
                vec![event(KeyCode::Char('j'))],
                vec![event(KeyCode::Backspace)],
                vec![event(KeyCode::Char('r'))],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal(&mut terminal, &mut state).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(state.explorer, old_explorer);
        assert_eq!(std::env::current_dir().unwrap(), cwd_before);
    }
}
