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
    HomeActionPaths, HomeEditorOutcome, HomeEditorResult, ResolvedEditor, ShortcutHelpTransition,
    TerminaSession, is_shortcut_help_key, shortcut_help_transition,
};
use crate::{branding, config::Config};

const GLOBAL_HOME_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_DISCARDED_TRANSITION_EVENTS: usize = 256;
const GLOBAL_HOME_ACTIONS: [Option<(&str, &str)>; 8] = [
    Some(("Open", "o")),
    Some(("Go to Path", "g")),
    None,
    Some(("Global Config", "G")),
    Some(("Authentication Cookie", "a")),
    None,
    Some(("Explorer Shortcuts", "?")),
    Some(("Quit", "q")),
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
    InitializeWorkspace(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalHomeErrorKind {
    WorkspaceOpen,
    WorkspaceInitialization,
    WorkspaceInitializedOpen,
    GoToPath,
    GlobalConfig,
    AuthenticationCookie,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalHomeError {
    kind: GlobalHomeErrorKind,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitializeWorkspaceModal {
    target: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitializeGlobalConfigModal {
    target: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GlobalHomeFileAction {
    OpenGlobalConfig,
    ShowAuthenticationCookie,
    InitializeGlobalConfig(PathBuf),
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
    initialize_workspace: Option<InitializeWorkspaceModal>,
    initialize_global_config: Option<InitializeGlobalConfigModal>,
    file_action: Option<GlobalHomeFileAction>,
    shortcut_help_visible: bool,
    explorer_overlay_visible: bool,
    explorer_pane_visible: bool,
}

impl GlobalHomeState {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            explorer: ExplorerState::new(root),
            path_input: None,
            error: None,
            initialize_workspace: None,
            initialize_global_config: None,
            file_action: None,
            shortcut_help_visible: false,
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

    #[cfg(test)]
    pub(crate) fn initialize_workspace_target(&self) -> Option<&Path> {
        self.initialize_workspace
            .as_ref()
            .map(|modal| modal.target.as_path())
    }

    pub(crate) fn show_workspace_open_error(&mut self, message: String) {
        self.show_workspace_error(GlobalHomeErrorKind::WorkspaceOpen, message);
    }

    pub(crate) fn show_initialize_workspace_confirmation(&mut self, target: PathBuf) {
        self.path_input = None;
        self.initialize_global_config = None;
        self.file_action = None;
        self.shortcut_help_visible = false;
        self.initialize_workspace = Some(InitializeWorkspaceModal { target });
    }

    pub(crate) fn show_workspace_initialization_error(&mut self, message: String) {
        self.show_workspace_error(GlobalHomeErrorKind::WorkspaceInitialization, message);
    }

    pub(crate) fn show_workspace_initialized_open_error(&mut self, message: String) {
        self.show_workspace_error(GlobalHomeErrorKind::WorkspaceInitializedOpen, message);
    }

    fn show_workspace_error(&mut self, kind: GlobalHomeErrorKind, message: String) {
        self.initialize_workspace = None;
        self.initialize_global_config = None;
        self.file_action = None;
        self.path_input = None;
        self.shortcut_help_visible = false;
        self.error = Some(GlobalHomeError { kind, message });
    }

    fn show_home_action_error(&mut self, kind: GlobalHomeErrorKind, message: String) {
        self.show_workspace_error(kind, message);
    }

    fn show_initialize_global_config(&mut self, target: PathBuf) {
        self.initialize_workspace = None;
        self.path_input = None;
        self.shortcut_help_visible = false;
        self.error = None;
        self.initialize_global_config = Some(InitializeGlobalConfigModal { target });
    }

    fn take_file_action(&mut self) -> Option<GlobalHomeFileAction> {
        self.file_action.take()
    }

    fn open_path_input(&mut self) {
        self.path_input = Some(PathInputModal::default());
        self.shortcut_help_visible = false;
    }

    fn selected_open_exit(&self) -> GlobalHomeExit {
        GlobalHomeExit::OpenWorkspace(self.explorer.selected_path().to_path_buf())
    }

    fn request_open(&mut self) -> Option<GlobalHomeExit> {
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

        if self.initialize_workspace.is_some() {
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Escape {
                self.initialize_workspace = None;
                return None;
            }
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
                let modal = self
                    .initialize_workspace
                    .take()
                    .expect("active initialization confirmation must remain present until Enter");
                return Some(GlobalHomeExit::InitializeWorkspace(modal.target));
            }
            return None;
        }

        if self.initialize_global_config.is_some() {
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Escape {
                self.initialize_global_config = None;
                return None;
            }
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
                let modal = self
                    .initialize_global_config
                    .take()
                    .expect("global config initialization modal must remain active");
                self.file_action = Some(GlobalHomeFileAction::InitializeGlobalConfig(modal.target));
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
        } else {
            match key.code {
                KeyCode::Char('o') if has_plain_modifiers(key) => self.request_open(),
                KeyCode::Char('g') if has_plain_modifiers(key) => {
                    self.open_path_input();
                    None
                }
                KeyCode::Char('G') if has_plain_modifiers(key) => {
                    self.file_action = Some(GlobalHomeFileAction::OpenGlobalConfig);
                    None
                }
                KeyCode::Char('a') if has_plain_modifiers(key) => {
                    self.file_action = Some(GlobalHomeFileAction::ShowAuthenticationCookie);
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
        if self.error.is_some()
            || self.initialize_workspace.is_some()
            || self.initialize_global_config.is_some()
        {
            return;
        }
        if let Some(input) = self.path_input.as_mut() {
            input.insert_text(text);
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

    fn resolve_global_home_editor(&mut self, _config: &Config) -> Result<ResolvedEditor, String> {
        Err("editor launching is unavailable".to_string())
    }

    fn launch_global_home_editor(
        &mut self,
        _config: &Config,
        _editor: &ResolvedEditor,
        _target: &Path,
    ) -> io::Result<HomeEditorOutcome> {
        Ok(HomeEditorOutcome {
            result: HomeEditorResult::RecoverableError(
                "editor launching is unavailable".to_string(),
            ),
            discard_input_batch: false,
        })
    }

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

    fn resolve_global_home_editor(&mut self, config: &Config) -> Result<ResolvedEditor, String> {
        super::resolve_live_home_editor(self, config)
    }

    fn launch_global_home_editor(
        &mut self,
        config: &Config,
        editor: &ResolvedEditor,
        target: &Path,
    ) -> io::Result<HomeEditorOutcome> {
        super::launch_live_home_editor(self, config, editor, target)
    }
}

pub(crate) fn run_with_terminal(
    terminal: &mut impl GlobalHomeTerminal,
    state: &mut GlobalHomeState,
) -> io::Result<GlobalHomeExit> {
    let paths = HomeActionPaths::current();
    run_with_terminal_and_paths(terminal, state, &paths)
}

fn run_with_terminal_and_paths(
    terminal: &mut impl GlobalHomeTerminal,
    state: &mut GlobalHomeState,
    paths: &HomeActionPaths,
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
        if let Some(action) = state.take_file_action() {
            handle_file_action(terminal, state, paths, action)?;
        }
        dirty = true;
    }
}

fn editor_config(paths: &HomeActionPaths) -> Config {
    paths
        .global_config()
        .ok()
        .and_then(|path| Config::load_from(path).ok())
        .unwrap_or_default()
}

fn launch_target(
    terminal: &mut impl GlobalHomeTerminal,
    state: &mut GlobalHomeState,
    paths: &HomeActionPaths,
    target: &Path,
    kind: GlobalHomeErrorKind,
) -> io::Result<()> {
    let config = editor_config(paths);
    let editor = match terminal.resolve_global_home_editor(&config) {
        Ok(editor) => editor,
        Err(error) => {
            state.show_home_action_error(kind, error);
            return Ok(());
        }
    };
    launch_resolved_target(terminal, state, &config, &editor, target, kind)
}

fn launch_resolved_target(
    terminal: &mut impl GlobalHomeTerminal,
    state: &mut GlobalHomeState,
    config: &Config,
    editor: &ResolvedEditor,
    target: &Path,
    kind: GlobalHomeErrorKind,
) -> io::Result<()> {
    let outcome = terminal.launch_global_home_editor(config, editor, target)?;
    if outcome.discard_input_batch {
        terminal.discard_global_home_input_batch()?;
    }
    match outcome.result {
        HomeEditorResult::Launched => {}
        HomeEditorResult::RecoverableError(error) => {
            state.show_home_action_error(kind, error);
        }
    }
    Ok(())
}

fn handle_file_action(
    terminal: &mut impl GlobalHomeTerminal,
    state: &mut GlobalHomeState,
    paths: &HomeActionPaths,
    action: GlobalHomeFileAction,
) -> io::Result<()> {
    match action {
        GlobalHomeFileAction::OpenGlobalConfig => {
            let target = match paths.global_config() {
                Ok(target) => target,
                Err(error) => {
                    state.show_home_action_error(
                        GlobalHomeErrorKind::GlobalConfig,
                        error.to_string(),
                    );
                    return Ok(());
                }
            };
            match crate::user_config_fs::inspect_editable_file(target, "global config file") {
                Ok(crate::user_config_fs::EditableFileState::Existing) => launch_target(
                    terminal,
                    state,
                    paths,
                    target,
                    GlobalHomeErrorKind::GlobalConfig,
                ),
                Ok(crate::user_config_fs::EditableFileState::Missing) => {
                    state.show_initialize_global_config(target.to_path_buf());
                    Ok(())
                }
                Err(error) => {
                    state.show_home_action_error(
                        GlobalHomeErrorKind::GlobalConfig,
                        error.to_string(),
                    );
                    Ok(())
                }
            }
        }
        GlobalHomeFileAction::InitializeGlobalConfig(target) => {
            let config = editor_config(paths);
            let editor = match terminal.resolve_global_home_editor(&config) {
                Ok(editor) => editor,
                Err(error) => {
                    state.show_home_action_error(GlobalHomeErrorKind::GlobalConfig, error);
                    return Ok(());
                }
            };
            let mut reporter = super::EditorInitializationReporter;
            if let Err(error) = crate::commands::initialize_config_at(&target, &mut reporter) {
                state.show_home_action_error(
                    GlobalHomeErrorKind::GlobalConfig,
                    format!("failed to initialize global config: {error}"),
                );
                return Ok(());
            }
            launch_resolved_target(
                terminal,
                state,
                &config,
                &editor,
                &target,
                GlobalHomeErrorKind::GlobalConfig,
            )
        }
        GlobalHomeFileAction::ShowAuthenticationCookie => {
            state.show_home_action_error(
                GlobalHomeErrorKind::AuthenticationCookie,
                super::authentication_cookie_status(paths),
            );
            Ok(())
        }
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

#[cfg(test)]
pub(crate) fn prefixed_path_line_for_test(prefix: &str, path: &Path, width: usize) -> String {
    prefixed_path_line(prefix, path, width)
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
        let lines = GLOBAL_HOME_ACTIONS.iter().map(|action| match action {
            Some((label, shortcut)) => menu_line(label, shortcut, usize::from(layout.menu.width)),
            None => Line::raw(""),
        });
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
    if let Some(input) = state.path_input.as_ref() {
        render_path_input(frame, state.explorer.root(), input);
    }
    if let Some(modal) = state.initialize_workspace.as_ref() {
        render_initialize_workspace(frame, modal);
    }
    if let Some(modal) = state.initialize_global_config.as_ref() {
        render_initialize_global_config(frame, modal);
    }
    if let Some(error) = state.error.as_ref() {
        render_error(frame, error);
    }
}

fn render_initialize_workspace(frame: &mut Frame<'_>, modal: &InitializeWorkspaceModal) {
    let area = centered_rect(frame.area(), 64, 11);
    let block = Block::default()
        .title(" Initialize Workspace ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let target = prefixed_path_line("", &modal.target, usize::from(inner.width));
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::raw("This directory is not an atc workspace."),
            Line::raw(""),
            Line::raw("Initialize here?"),
            Line::raw(""),
            Line::raw(target),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(" Initialize       "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw(" Cancel"),
            ]),
        ]))
        .wrap(Wrap { trim: false }),
        inner,
    );
}

fn render_shortcuts(frame: &mut Frame<'_>, _explorer_pane_visible: bool) {
    let area = centered_rect(frame.area(), 46, 11);
    let lines = vec![
        Line::raw("Enter     Expand / Collapse"),
        Line::raw("↑↓ jk     Navigate"),
        Line::raw("←→ hl     Collapse / Expand"),
        Line::raw("Backspace Parent"),
        Line::raw("r         Reload"),
        Line::raw(""),
        Line::raw("? keep open   Esc close"),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .title(" Explorer Shortcuts ")
                .borders(Borders::ALL),
        ),
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

fn render_initialize_global_config(frame: &mut Frame<'_>, modal: &InitializeGlobalConfigModal) {
    let area = centered_rect(frame.area(), 64, 11);
    let block = Block::default()
        .title(" Initialize Global Config ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let target = prefixed_path_line("", &modal.target, usize::from(inner.width));
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::raw("Global config does not exist."),
            Line::raw(""),
            Line::raw(target),
            Line::raw(""),
            Line::raw("Create the comments-only default and open it?"),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(" Initialize & Open       "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw(" Cancel"),
            ]),
        ]))
        .wrap(Wrap { trim: false }),
        inner,
    );
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
    let height = match error.kind {
        GlobalHomeErrorKind::WorkspaceInitialization
        | GlobalHomeErrorKind::WorkspaceInitializedOpen
        | GlobalHomeErrorKind::AuthenticationCookie => 13,
        GlobalHomeErrorKind::WorkspaceOpen | GlobalHomeErrorKind::GoToPath => 9,
        GlobalHomeErrorKind::GlobalConfig => 11,
    };
    let area = centered_rect(frame.area(), 64, height);
    let title = match error.kind {
        GlobalHomeErrorKind::WorkspaceOpen => " Workspace Open Failed ",
        GlobalHomeErrorKind::WorkspaceInitialization => " Workspace Initialization Failed ",
        GlobalHomeErrorKind::WorkspaceInitializedOpen => " Workspace Initialized, Open Failed ",
        GlobalHomeErrorKind::GoToPath => " Go to Path Failed ",
        GlobalHomeErrorKind::GlobalConfig => " Global Config Failed ",
        GlobalHomeErrorKind::AuthenticationCookie => " Authentication Cookie ",
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
    use crate::tui::terminal::{Modifiers, PointerEvent, PointerKind, PointerPosition};

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
        editor_targets: Vec<PathBuf>,
        editor_configs_have_override: Vec<bool>,
        launched_editors: Vec<ResolvedEditor>,
        use_production_editor_resolver: bool,
        resolve_error: Option<String>,
        editor_outcome: HomeEditorOutcome,
        discarded_events: usize,
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
                editor_targets: Vec::new(),
                editor_configs_have_override: Vec::new(),
                launched_editors: Vec::new(),
                use_production_editor_resolver: false,
                resolve_error: None,
                editor_outcome: HomeEditorOutcome {
                    result: HomeEditorResult::Launched,
                    discard_input_batch: false,
                },
                discarded_events: 0,
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

        fn resolve_global_home_editor(
            &mut self,
            config: &Config,
        ) -> Result<ResolvedEditor, String> {
            self.editor_configs_have_override
                .push(config.editor.is_some());
            if let Some(error) = self.resolve_error.clone() {
                Err(error)
            } else if self.use_production_editor_resolver {
                crate::editor::resolve(config).map_err(|error| error.to_string())
            } else {
                Ok(ResolvedEditor {
                    program: "test-editor".into(),
                    args: Vec::new(),
                    mode: crate::editor::EditorLaunchMode::External,
                    source: crate::editor::EditorSource::EditorEnv,
                })
            }
        }

        fn launch_global_home_editor(
            &mut self,
            _config: &Config,
            editor: &ResolvedEditor,
            target: &Path,
        ) -> io::Result<HomeEditorOutcome> {
            self.launched_editors.push(editor.clone());
            self.editor_targets.push(target.to_path_buf());
            Ok(self.editor_outcome.clone())
        }

        fn discard_global_home_input_batch(&mut self) -> io::Result<()> {
            while self.poll_global_home(Duration::ZERO)? {
                let _ = self.read_global_home()?;
                self.discarded_events += 1;
            }
            Ok(())
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

    fn cookie_location(root: &Path) -> crate::paths::CookieLocation {
        let platform_base = root.join("platform-state");
        let state_dir = platform_base.join("atc").join("state");
        let file = state_dir.join("cookie");
        crate::paths::CookieLocation {
            platform_base,
            state_dir,
            file,
        }
    }

    fn write_cookie(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(target, link);

        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("failed to create cookie symlink: {error}"),
        }
    }

    #[test]
    fn global_config_actions_open_invalid_existing_and_initialize_missing_files() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        let cookie = cookie_location(temp.path());
        let paths = HomeActionPaths::for_test(config.clone(), cookie);
        std::fs::write(&config, "invalid = [\n").unwrap();
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('G'))],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(&mut terminal, &mut state, &paths).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(
            terminal.editor_targets.as_slice(),
            std::slice::from_ref(&config)
        );
        assert_eq!(terminal.editor_configs_have_override, [false]);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "invalid = [\n");

        std::fs::remove_file(&config).unwrap();
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('G'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        assert_eq!(
            run_with_terminal_and_paths(&mut terminal, &mut state, &paths).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            crate::config::INITIAL_CONFIG
        );
        assert_eq!(terminal.editor_targets, [config]);
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("Initialize & Open"))
        );
    }

    #[test]
    fn valid_global_config_editor_override_reaches_the_home_resolver() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(
            &config,
            "[editor]\ncommand = \"configured-editor\"\nmode = \"terminal\"\n",
        )
        .unwrap();
        let paths = HomeActionPaths::for_test(config.clone(), cookie_location(temp.path()));
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('G'))],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.use_production_editor_resolver = true;

        assert_eq!(
            run_with_terminal_and_paths(&mut terminal, &mut state, &paths).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.editor_targets, [config]);
        assert_eq!(terminal.launched_editors.len(), 1);
        let resolved = &terminal.launched_editors[0];
        assert_eq!(
            resolved.program,
            std::ffi::OsString::from("configured-editor")
        );
        assert_eq!(resolved.mode, crate::editor::EditorLaunchMode::Terminal);
        assert_eq!(resolved.source, crate::editor::EditorSource::Config);
    }

    #[test]
    fn global_home_resolves_editor_before_initializing_global_config() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        let paths = HomeActionPaths::for_test(config.clone(), cookie_location(temp.path()));
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('G'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.resolve_error = Some("No editor configured.".to_string());

        assert_eq!(
            run_with_terminal_and_paths(&mut terminal, &mut state, &paths).unwrap(),
            GlobalHomeExit::Quit
        );
        assert!(!config.exists());
        assert!(terminal.editor_targets.is_empty());
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("No editor configured."))
        );
    }

    #[test]
    fn authentication_cookie_reports_a_safe_existing_path_without_opening_or_rendering_its_value() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        let cookie = cookie_location(temp.path());
        std::fs::create_dir_all(&cookie.state_dir).unwrap();
        let secret = "REVEL_SESSION=super-secret-cookie-value";
        write_cookie(&cookie.file, secret);
        let paths = HomeActionPaths::for_test(config, cookie.clone());
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('a'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(&mut terminal, &mut state, &paths).unwrap(),
            GlobalHomeExit::Quit
        );
        assert!(terminal.editor_targets.is_empty());
        assert!(terminal.frames.iter().any(|frame| {
            frame.contains("Status: Configured")
                && frame.contains("REVEL_SESSION=<value>")
                && frame.contains("cookie")
        }));
        assert!(terminal.frames.iter().all(|frame| !frame.contains(secret)));
    }

    #[test]
    fn missing_cookie_is_not_created_and_shows_setup_guidance() {
        let temp = tempfile::tempdir().unwrap();
        let cookie = cookie_location(temp.path());
        let paths = HomeActionPaths::for_test(temp.path().join("config.toml"), cookie.clone());
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('a'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(&mut terminal, &mut state, &paths).unwrap(),
            GlobalHomeExit::Quit
        );
        assert!(!cookie.file.exists());
        assert!(terminal.editor_targets.is_empty());
        assert!(terminal.frames.iter().any(|frame| {
            frame.contains("Status: Not configured")
                && frame.contains("REVEL_SESSION=<value>")
                && frame.contains("Path:")
                && frame.contains("cookie")
        }));
    }

    #[test]
    fn cookie_symlink_is_rejected_without_exposing_or_opening_the_target() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        let secret = "REVEL_SESSION=external-secret";
        write_cookie(external.path(), secret);
        let cookie = cookie_location(temp.path());
        std::fs::create_dir_all(&cookie.state_dir).unwrap();
        if !create_file_symlink(external.path(), &cookie.file) {
            return;
        }
        let paths = HomeActionPaths::for_test(temp.path().join("config.toml"), cookie);
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('a'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(&mut terminal, &mut state, &paths).unwrap(),
            GlobalHomeExit::Quit
        );
        assert!(terminal.editor_targets.is_empty());
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("Status: Invalid"))
        );
        assert!(terminal.frames.iter().all(|frame| !frame.contains(secret)));
        assert_eq!(std::fs::read_to_string(external.path()).unwrap(), secret);
    }

    #[test]
    fn terminal_editor_discards_the_rest_of_the_current_global_home_input_batch() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let paths = HomeActionPaths::for_test(config, cookie_location(temp.path()));
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('G')), event(KeyCode::Char('q'))],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.editor_outcome = HomeEditorOutcome {
            result: HomeEditorResult::Launched,
            discard_input_batch: true,
        };

        assert_eq!(
            run_with_terminal_and_paths(&mut terminal, &mut state, &paths).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.discarded_events, 1);
        assert_eq!(terminal.reads, 3);
    }

    #[test]
    fn recoverable_terminal_editor_failure_discards_queued_modal_and_quit_input() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let paths = HomeActionPaths::for_test(config, cookie_location(temp.path()));
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![
                    event(KeyCode::Char('G')),
                    event(KeyCode::Enter),
                    event(KeyCode::Char('q')),
                ],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.editor_outcome = HomeEditorOutcome {
            result: HomeEditorResult::RecoverableError("editor launch failed".to_string()),
            discard_input_batch: true,
        };

        assert_eq!(
            run_with_terminal_and_paths(&mut terminal, &mut state, &paths).unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.discarded_events, 2);
        assert_eq!(terminal.reads, 5);
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("editor launch failed"))
        );
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
            "Global Config",
            "Authentication Cookie",
            "Explorer Shortcuts",
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
    fn colon_is_ignored_and_uppercase_g_is_distinct_from_go_to_path() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        assert_eq!(state.handle_key(key(KeyCode::Char(':'))), None);
        assert!(state.take_file_action().is_none());
        assert_eq!(state.handle_key(key(KeyCode::Char('G'))), None);
        assert_eq!(
            state.take_file_action(),
            Some(GlobalHomeFileAction::OpenGlobalConfig)
        );
        assert_eq!(state.handle_key(key(KeyCode::Char('g'))), None);
        assert!(state.path_input.is_some());
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
    fn initialization_confirmation_owns_input_and_returns_the_original_pathbuf_snapshot() {
        let selected_directory = tempfile::tempdir().unwrap();
        let changed_directory = tempfile::tempdir().unwrap();
        let selected = selected_directory.path().to_path_buf();
        let changed_selection = changed_directory.path().to_path_buf();
        let mut state = GlobalHomeState::new(selected.clone());
        state.show_initialize_workspace_confirmation(selected.clone());
        assert!(state.explorer.rebase(changed_selection.clone()));
        let explorer_after_selection_change = state.explorer.clone();

        for code in [
            KeyCode::Char('o'),
            KeyCode::Char('g'),
            KeyCode::Char(':'),
            KeyCode::Char('?'),
            KeyCode::Char('q'),
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Char('r'),
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Backspace,
        ] {
            assert_eq!(state.handle_key(key(code)), None);
        }
        assert_eq!(
            state.handle_key(KeyEvent {
                code: KeyCode::Enter,
                kind: KeyEventKind::Repeat,
                modifiers: Modifiers::default(),
            }),
            None
        );
        state.handle_paste("ignored paste");

        assert_eq!(state.explorer, explorer_after_selection_change);
        assert_eq!(state.explorer.selected_path(), changed_selection);
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Some(GlobalHomeExit::InitializeWorkspace(selected))
        );
        assert!(state.initialize_workspace.is_none());
    }

    #[test]
    fn initialization_confirmation_consumes_pointer_events_in_the_production_loop() {
        let target = PathBuf::from("confirmed-target");
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.show_initialize_workspace_confirmation(target.clone());
        let pointer = TerminalEvent::Pointer(PointerEvent {
            kind: PointerKind::Move,
            position: PointerPosition::Cells { column: 3, row: 4 },
            modifiers: Modifiers::default(),
            pixel_generation: None,
        });
        let mut terminal = ScriptedGlobalTerminal::new(
            80,
            24,
            [
                vec![pointer],
                vec![event(KeyCode::Char('q'))],
                vec![event(KeyCode::Enter)],
            ],
        );

        assert_eq!(
            run_with_terminal(&mut terminal, &mut state).unwrap(),
            GlobalHomeExit::InitializeWorkspace(target)
        );
        assert_eq!(terminal.reads, 3);
    }

    #[test]
    fn initialization_confirmation_cancel_preserves_wide_and_narrow_explorer_context() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("child");
        std::fs::create_dir(&child).unwrap();

        let mut wide = GlobalHomeState::new(root.path().to_path_buf());
        let _ = draw(&mut wide, 80, 24);
        wide.handle_key(key(KeyCode::Enter));
        wide.handle_key(key(KeyCode::Char('j')));
        let wide_explorer = wide.explorer.clone();
        wide.show_initialize_workspace_confirmation(child.clone());
        assert_eq!(wide.handle_key(key(KeyCode::Escape)), None);
        assert_eq!(wide.explorer, wide_explorer);
        assert!(!wide.explorer_overlay_visible);

        let mut narrow = GlobalHomeState::new(root.path().to_path_buf());
        let _ = draw(&mut narrow, 61, 24);
        narrow.handle_key(key(KeyCode::Char('o')));
        narrow.handle_key(key(KeyCode::Enter));
        narrow.handle_key(key(KeyCode::Char('j')));
        let narrow_explorer = narrow.explorer.clone();
        narrow.show_initialize_workspace_confirmation(child);
        assert_eq!(narrow.handle_key(key(KeyCode::Escape)), None);
        assert_eq!(narrow.explorer, narrow_explorer);
        assert!(narrow.explorer_overlay_visible);
        assert_eq!(narrow.handle_key(key(KeyCode::Char('q'))), None);
        assert!(narrow.explorer_overlay_visible);
    }

    #[test]
    fn initialization_confirmation_renders_unicode_safe_tail_and_tiny_frames() {
        let target = PathBuf::from(r"C:\very-long-parent\another-long-parent\projects\日本語\abc");
        let clipped = prefixed_path_line("", &target, 30);
        assert!(UnicodeWidthStr::width(clipped.as_str()) <= 30);
        assert!(clipped.ends_with(r"\projects\日本語\abc"));
        assert!(!clipped.contains('\u{fffd}'));
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.show_initialize_workspace_confirmation(target);

        for (width, height) in [(0, 0), (0, 8), (8, 0), (1, 1), (8, 3), (24, 5)] {
            let _ = draw(&mut state, width, height);
        }

        let rendered = buffer_text(&draw(&mut state, 80, 24));
        for expected in [
            "Initialize Workspace",
            "This directory is not an atc workspace.",
            "Initialize here?",
            "abc",
            "Enter Initialize",
            "Esc Cancel",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
        assert!(!rendered.contains('\u{fffd}'));
    }

    #[cfg(unix)]
    #[test]
    fn initialization_confirmation_preserves_non_utf8_path_identity() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let target = PathBuf::from(OsString::from_vec(b"workspace-\xff".to_vec()));
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.show_initialize_workspace_confirmation(target.clone());

        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Some(GlobalHomeExit::InitializeWorkspace(target))
        );
    }

    #[test]
    fn explorer_shortcut_help_is_retained_and_one_shot() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char('?')));
        let rendered = buffer_text(&draw(&mut state, 80, 24));
        for expected in [
            "Explorer Shortcuts",
            "Enter     Expand / Collapse",
            "↑↓ jk     Navigate",
            "←→ hl     Collapse / Expand",
            "Backspace Parent",
            "r         Reload",
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
                assert!(state.explorer_pane_visible);
                assert_eq!(buffer.cell((explorer_width - 1, 0)).unwrap().symbol(), "│");
            } else {
                assert!(!state.explorer_pane_visible);
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
        assert!(!state.explorer_pane_visible);
        assert!(dashboard.contains("Explorer Shortcuts"));
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
        let visible_relative = prefixed_path_line(SELECTED_PREFIX, &relative, 66);
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| { frame.contains(&visible_relative) })
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
    fn production_colon_is_ignored_without_consuming_later_input() {
        let root = tempfile::tempdir().unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            80,
            24,
            [vec![event(KeyCode::Char(':')), event(KeyCode::Char('q'))]],
        );

        assert_eq!(
            run_with_terminal(&mut terminal, &mut state).unwrap(),
            GlobalHomeExit::Quit
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
