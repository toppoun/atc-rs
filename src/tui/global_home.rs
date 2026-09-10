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
const GLOBAL_HOME_ACTIONS: [(&str, &str); 4] = [
    ("Open Workspace", "o"),
    ("Commands", ":"),
    ("Shortcuts", "?"),
    ("Quit", "q"),
];
const MENU_WIDTH: u16 = 23;
const MENU_HEIGHT: u16 = GLOBAL_HOME_ACTIONS.len() as u16;
const SUBTITLE: &str = "AtCoder workspace launcher";
const DIRECTORY_PREFIX: &str = "Directory  ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GlobalHomeExit {
    Quit,
    OpenWorkspace(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalHomeCommand {
    OpenWorkspace,
    Quit,
}

impl GlobalHomeCommand {
    const ALL: [Self; 2] = [Self::OpenWorkspace, Self::Quit];

    const fn label(self) -> &'static str {
        match self {
            Self::OpenWorkspace => "Open Workspace",
            Self::Quit => "Quit",
        }
    }

    const fn shortcut(self) -> &'static str {
        match self {
            Self::OpenWorkspace => "o",
            Self::Quit => "q",
        }
    }
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
    current_directory: PathBuf,
    path_input: Option<PathInputModal>,
    error: Option<String>,
    shortcut_help_visible: bool,
    palette: GlobalHomeCommandPalette,
}

impl GlobalHomeState {
    pub(crate) fn new(current_directory: PathBuf) -> Self {
        Self {
            current_directory,
            path_input: None,
            error: None,
            shortcut_help_visible: false,
            palette: GlobalHomeCommandPalette::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn current_directory(&self) -> &Path {
        &self.current_directory
    }

    pub(crate) fn show_workspace_open_error(&mut self, message: String) {
        self.path_input = None;
        self.palette.close();
        self.shortcut_help_visible = false;
        self.error = Some(message);
    }

    fn open_path_input(&mut self) {
        self.path_input = Some(PathInputModal::default());
        self.palette.close();
        self.shortcut_help_visible = false;
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
                return Some(GlobalHomeExit::OpenWorkspace(resolve_workspace_path(
                    &self.current_directory,
                    &input.value,
                )));
            }
            self.path_input
                .as_mut()
                .expect("active path input must remain present")
                .handle_edit_key(key);
            return None;
        }

        if self.palette.is_active() {
            return match self.palette.handle_key(key) {
                PaletteKeyResult::Execute(GlobalHomeCommand::OpenWorkspace) => {
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

        if key.kind != KeyEventKind::Press {
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
                KeyCode::Char('o')
                    if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
                {
                    self.open_path_input();
                    None
                }
                KeyCode::Char('q')
                    if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
                {
                    Some(GlobalHomeExit::Quit)
                }
                _ => None,
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

pub(crate) fn resolve_workspace_path(current_directory: &Path, input: &str) -> PathBuf {
    if input.is_empty() {
        return current_directory.to_path_buf();
    }

    let input = PathBuf::from(input);
    if input.is_absolute() {
        input
    } else {
        current_directory.join(input)
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
            if matches!(exit, GlobalHomeExit::OpenWorkspace(_)) {
                terminal.discard_global_home_input_batch()?;
            }
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
    directory: Option<Rect>,
}

fn global_home_layout(area: Rect) -> GlobalHomeLayout {
    if area.width == 0 || area.height == 0 {
        return GlobalHomeLayout {
            logo: None,
            subtitle: None,
            menu: Rect::new(area.x, area.y, 0, 0),
            directory: None,
        };
    }

    let show_directory = area.height >= MENU_HEIGHT.saturating_add(1);
    let bottom_margin = u16::from(show_directory && area.height >= MENU_HEIGHT.saturating_add(4));
    let directory_y = area
        .y
        .saturating_add(area.height)
        .saturating_sub(1)
        .saturating_sub(bottom_margin);
    let body_height = if show_directory {
        directory_y.saturating_sub(area.y)
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

    let directory = show_directory.then(|| {
        let margin = u16::from(area.width >= 4);
        Rect::new(
            area.x.saturating_add(margin),
            directory_y,
            area.width.saturating_sub(margin.saturating_mul(2)),
            1,
        )
    });

    GlobalHomeLayout {
        logo,
        subtitle,
        menu: centered_row(area, menu_y, MENU_WIDTH, MENU_HEIGHT.min(body_height)),
        directory,
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

fn render(frame: &mut Frame<'_>, state: &GlobalHomeState) {
    let layout = global_home_layout(frame.area());

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
    if let Some(area) = layout.directory {
        frame.render_widget(
            Paragraph::new(prefixed_path_line(
                DIRECTORY_PREFIX,
                &state.current_directory,
                usize::from(area.width),
            ))
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
    if let Some(input) = state.path_input.as_ref() {
        render_path_input(frame, &state.current_directory, input);
    }
    if let Some(error) = state.error.as_deref() {
        render_error(frame, error);
    }
}

fn render_shortcuts(frame: &mut Frame<'_>) {
    let area = centered_rect(frame.area(), 38, 9);
    let lines = vec![
        Line::raw("o  Open Workspace"),
        Line::raw(":  Commands"),
        Line::raw("?  Shortcuts"),
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

fn render_path_input(frame: &mut Frame<'_>, current_directory: &Path, input: &PathInputModal) {
    let area = centered_rect(frame.area(), 64, 9);
    let block = Block::default()
        .title(" Open Workspace ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let width = usize::from(inner.width);
    let current = prefixed_path_line("Current: ", current_directory, width);
    let path = prefixed_text_line("Path: ", &input.value, width);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::raw(current),
            Line::raw(""),
            Line::raw(path),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(" Open      "),
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

fn render_error(frame: &mut Frame<'_>, error: &str) {
    let area = centered_rect(frame.area(), 64, 9);
    let block = Block::default()
        .title(" Workspace Open Failed ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(format!("{error}\n\nEnter / Esc  Dismiss")).wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
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

    fn draw(state: &GlobalHomeState, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    #[test]
    fn dashboard_contains_only_phase_3a_2_actions_and_explicit_directory() {
        let state = GlobalHomeState::new(PathBuf::from(r"D:\current\directory"));
        let rendered = buffer_text(&draw(&state, 80, 24));

        for expected in branding::ascii_logo_lines().chain([
            SUBTITLE,
            "Open Workspace",
            "Commands",
            "Shortcuts",
            "Quit",
            r"Directory  D:\current\directory",
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
    fn path_resolution_preserves_empty_relative_and_absolute_semantics() {
        let current = tempfile::tempdir().unwrap();
        let absolute = tempfile::tempdir().unwrap();

        assert_eq!(resolve_workspace_path(current.path(), ""), current.path());
        assert_eq!(
            resolve_workspace_path(current.path(), "workspace"),
            current.path().join("workspace")
        );
        assert_eq!(
            resolve_workspace_path(current.path(), "workspace with spaces"),
            current.path().join("workspace with spaces")
        );
        assert_eq!(
            resolve_workspace_path(current.path(), absolute.path().to_str().unwrap()),
            absolute.path()
        );
        assert_eq!(
            resolve_workspace_path(current.path(), "."),
            current.path().join(".")
        );
        assert_eq!(
            resolve_workspace_path(current.path(), ".."),
            current.path().join("..")
        );

        #[cfg(windows)]
        assert_eq!(
            resolve_workspace_path(current.path(), r"D:\My Projects\atcoder"),
            PathBuf::from(r"D:\My Projects\atcoder")
        );
    }

    #[test]
    fn empty_enter_returns_the_original_pathbuf_without_display_roundtrip() {
        let current = PathBuf::from("workspace/競プロ");
        let mut state = GlobalHomeState::new(current.clone());
        state.handle_key(key(KeyCode::Char('o')));

        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Some(GlobalHomeExit::OpenWorkspace(current))
        );
    }

    #[test]
    fn path_modal_owns_q_colon_and_question_mark_until_escape() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char('o')));
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
    fn palette_searches_open_workspace_and_quit_and_opens_the_path_modal() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char(':')));
        for character in "workspace".chars() {
            state.handle_key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            state.palette.filtered_commands(),
            [GlobalHomeCommand::OpenWorkspace]
        );

        state.handle_key(key(KeyCode::Enter));
        assert!(state.path_input.is_some());
        assert!(!state.palette.is_active());
    }

    #[test]
    fn error_modal_has_highest_precedence_and_dismisses_with_enter_or_escape() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.show_workspace_open_error("Not an atc workspace: root/missing".to_string());

        state.handle_key(key(KeyCode::Char('q')));
        assert!(state.error.is_some());
        assert_eq!(state.handle_key(key(KeyCode::Enter)), None);
        assert!(state.error.is_none());

        state.handle_key(key(KeyCode::Char('o')));
        assert!(state.path_input.is_some());
    }

    #[test]
    fn shortcut_help_matches_global_actions_and_is_one_shot() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char('?')));
        let rendered = buffer_text(&draw(&state, 80, 24));
        for expected in [
            "o  Open Workspace",
            ":  Commands",
            "?  Shortcuts",
            "q  Quit",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }

        state.handle_key(key(KeyCode::Char('o')));
        assert!(!state.shortcut_help_visible);
        assert!(state.path_input.is_some());
    }

    #[test]
    fn palette_selection_highlights_the_full_usable_row_with_unicode_width() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char(':')));
        let buffer = draw(&state, 80, 24);
        let palette_area = centered_rect(Rect::new(0, 0, 80, 24), 52, 9);
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
        let state = GlobalHomeState::new(PathBuf::from(
            r"C:\Users\ユーザー\very-long-parent\競プロ\atcoder",
        ));

        for (width, height) in [(0, 0), (1, 1), (12, 4), (30, 8), (80, 24)] {
            let _ = draw(&state, width, height);
        }

        let line = prefixed_path_line(DIRECTORY_PREFIX, &state.current_directory, 30);
        assert!(UnicodeWidthStr::width(line.as_str()) <= 30);
        assert!(line.ends_with(r"\競プロ\atcoder"));
        assert!(!line.contains('\u{fffd}'));
    }

    #[test]
    fn path_and_error_modals_render_safely_in_small_terminals() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char('o')));
        state.handle_paste("relative/workspace");
        for (width, height) in [(0, 0), (1, 1), (8, 3), (24, 5), (80, 24)] {
            let _ = draw(&state, width, height);
        }

        state.show_workspace_open_error("failed".to_string());
        for (width, height) in [(0, 0), (1, 1), (8, 3), (24, 5), (80, 24)] {
            let _ = draw(&state, width, height);
        }
    }
}
