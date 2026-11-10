use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::authentication_modal::{self, AuthenticationModalController};
use super::explorer::{self, ExplorerState};
use super::folder_selection::{
    FolderDialogFailure, FolderSelection, FolderSelectionRequest, NativeFolderPicker, select_folder,
};
use super::home::{
    centered_rect, centered_row, logo_size, menu_line, truncate_start_with_ellipsis,
};
use super::settings_screen::{self, SettingsPage, SettingsTransition};
use super::template_modal::{
    OpenTemplateModal, TemplateAction, TemplateModalTransition, TemplateRequest,
};
use super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminalEvent};
use super::view;
use super::{
    HomeActionPaths, HomeEditorOutcome, HomeEditorResult, ResolvedEditor, ShortcutHelpTransition,
    TerminaSession, is_shortcut_help_key, shortcut_help_transition,
};
use crate::workspace_registry::store::{RegistryApply, RegistryLoad, RegistryStore};
use crate::workspace_registry::{
    DirectoryIdentity, EntryRef, RegistryEntry, RegistryMutation, RegistryMutationResult,
    RegistrySnapshot,
};
use crate::{branding, config::Config};

const GLOBAL_HOME_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_DISCARDED_TRANSITION_EVENTS: usize = 256;
const LEFT_PANE_MIN_WIDTH: u16 = 32;
const LEFT_PANE_MAX_WIDTH: u16 = 48;
const DASHBOARD_MIN_WIDTH: u16 = 30;
const COMPACT_PANE_WIDTH: u16 = 20;
const ACTION_MENU_WIDTH: u16 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeftPane {
    Recent,
    Explorer,
}

const LAUNCHER_ACTIONS: [(GlobalHomeFileAction, &str, &str); 4] = [
    (GlobalHomeFileAction::PickFolder, "Open Folder", "o"),
    (GlobalHomeFileAction::OpenSettings, "Settings", "s"),
    (GlobalHomeFileAction::OpenTemplate, "Templates", "t"),
    (
        GlobalHomeFileAction::OpenAuthentication,
        "Authentication",
        "a",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GlobalHomeExit {
    Quit,
    OpenWorkspace(PathBuf),
    OpenRecentWorkspace(Box<RegistryEntry>),
    InitializeWorkspace(PathBuf, Option<DirectoryIdentity>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalHomeErrorKind {
    WorkspaceOpen,
    WorkspaceInitialization,
    WorkspaceInitializedOpen,
    GoToPath,
    RegistryRemoval,
    GlobalConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalHomeError {
    kind: GlobalHomeErrorKind,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitializeWorkspaceModal {
    target: PathBuf,
    identity: Option<DirectoryIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitializeGlobalConfigModal {
    target: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GlobalHomeFileAction {
    PickFolder,
    ReloadRegistry,
    RemoveWorkspace(EntryRef),
    OpenSettings,
    OpenGlobalConfig,
    OpenTemplate,
    Template(TemplateRequest),
    OpenAuthentication,
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

#[derive(Debug)]
struct ActiveTemplateModal {
    modal: OpenTemplateModal,
    config: Config,
}

impl std::ops::Deref for ActiveTemplateModal {
    type Target = OpenTemplateModal;

    fn deref(&self) -> &Self::Target {
        &self.modal
    }
}

impl std::ops::DerefMut for ActiveTemplateModal {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.modal
    }
}

#[derive(Debug)]
pub(crate) struct GlobalHomeState {
    explorer: ExplorerState,
    path_input: Option<PathInputModal>,
    error: Option<GlobalHomeError>,
    initialize_workspace: Option<InitializeWorkspaceModal>,
    initialize_global_config: Option<InitializeGlobalConfigModal>,
    template: Option<ActiveTemplateModal>,
    authentication: AuthenticationModalController,
    file_action: Option<GlobalHomeFileAction>,
    shortcut_help_visible: bool,
    left_pane: LeftPane,
    left_focused: bool,
    recent: Vec<RegistryEntry>,
    recent_selected: usize,
    recent_scroll: usize,
    action_selected: usize,
    registry_warning: Option<String>,
    registry_notice: Option<String>,
    remove_workspace: Option<RegistryEntry>,
    folder_dialog_failed: Option<FolderDialogFailure>,
}

impl GlobalHomeState {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            explorer: ExplorerState::new(root),
            path_input: None,
            error: None,
            initialize_workspace: None,
            initialize_global_config: None,
            template: None,
            authentication: AuthenticationModalController::default(),
            file_action: None,
            shortcut_help_visible: false,
            left_pane: LeftPane::Recent,
            left_focused: false,
            recent: Vec::new(),
            recent_selected: 0,
            recent_scroll: 0,
            action_selected: 0,
            registry_warning: None,
            registry_notice: None,
            remove_workspace: None,
            folder_dialog_failed: None,
        }
    }

    pub(crate) fn load_recent_workspaces(&mut self, registry: Result<&RegistryStore, &str>) {
        self.reload_registry(registry);
        self.left_focused = !self.recent.is_empty();
    }

    fn reload_registry(&mut self, registry: Result<&RegistryStore, &str>) {
        self.registry_notice = None;
        let loaded = registry
            .map_err(str::to_owned)
            .and_then(|store| store.load().map_err(|error| error.to_string()));
        match loaded {
            Ok(RegistryLoad::Snapshot(snapshot)) => self.set_recent(snapshot),
            Ok(RegistryLoad::Missing) => self.set_recent(RegistrySnapshot::default()),
            Err(error) => {
                self.recent.clear();
                self.recent_selected = 0;
                self.recent_scroll = 0;
                self.registry_warning = Some(format!("Workspace Registry unavailable: {error}"));
            }
        }
    }

    fn set_recent(&mut self, snapshot: RegistrySnapshot) {
        let previously_selected = self
            .recent
            .get(self.recent_selected)
            .map(RegistryEntry::entry_ref);
        self.recent = snapshot
            .entries_by_recent_use()
            .into_iter()
            .cloned()
            .collect();
        self.recent_selected = previously_selected
            .and_then(|entry| self.recent.iter().position(|row| row.entry_ref() == entry))
            .unwrap_or_else(|| {
                self.recent_selected
                    .min(self.recent.len().saturating_sub(1))
            });
        self.registry_warning = None;
    }

    fn remove_from_list(&mut self, registry: Result<&RegistryStore, &str>, entry: &EntryRef) {
        self.registry_notice = None;
        let result = registry.map_err(str::to_owned).and_then(|store| {
            store
                .apply(RegistryMutation::Remove(entry))
                .map_err(|error| error.to_string())
        });
        match result {
            Ok(RegistryApply::Applied {
                snapshot,
                result: RegistryMutationResult::Removed,
            }) => {
                self.set_recent(snapshot);
                self.registry_notice =
                    Some("Removed from list. Workspace files remain on disk.".into());
            }
            Ok(RegistryApply::StaleEntry { .. }) => {
                self.show_workspace_error(
                    GlobalHomeErrorKind::RegistryRemoval,
                    "The registration changed. Press r to reload Recent Workspaces.".into(),
                );
            }
            outcome => {
                let reason = outcome
                    .err()
                    .unwrap_or_else(|| "Registry did not confirm removal".into());
                self.show_workspace_error(GlobalHomeErrorKind::RegistryRemoval, format!(
                    "Removal from the list could not be confirmed:\n{reason}\nPress r to reload Recent Workspaces."
                ));
            }
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

    pub(crate) fn show_initialize_workspace_confirmation(
        &mut self,
        target: PathBuf,
        identity: Option<DirectoryIdentity>,
    ) {
        self.path_input = None;
        self.initialize_global_config = None;
        self.template = None;
        self.file_action = None;
        self.shortcut_help_visible = false;
        self.initialize_workspace = Some(InitializeWorkspaceModal { target, identity });
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
        self.template = None;
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
        self.template = None;
        self.shortcut_help_visible = false;
        self.error = None;
        self.initialize_global_config = Some(InitializeGlobalConfigModal { target });
    }

    fn take_file_action(&mut self) -> Option<GlobalHomeFileAction> {
        self.file_action.take()
    }

    fn open_template(&mut self, templates_dir: Result<PathBuf, String>, config: Config) {
        self.initialize_workspace = None;
        self.initialize_global_config = None;
        self.path_input = None;
        self.error = None;
        self.shortcut_help_visible = false;
        let default_language = config.defaults.language;
        self.template = Some(ActiveTemplateModal {
            modal: OpenTemplateModal::new(templates_dir, default_language, default_language),
            config,
        });
    }

    fn close_template(&mut self) {
        self.template = None;
    }

    fn show_template_error(&mut self, error: String) {
        if let Some(modal) = self.template.as_mut() {
            modal.set_error(error);
        }
    }

    fn open_path_input(&mut self) {
        self.path_input = Some(PathInputModal::default());
        self.shortcut_help_visible = false;
    }

    fn selected_open_exit(&self) -> GlobalHomeExit {
        GlobalHomeExit::OpenWorkspace(self.explorer.selected_path().to_path_buf())
    }

    fn explorer_focused(&self) -> bool {
        self.left_focused && self.left_pane == LeftPane::Explorer
    }

    fn focus_explorer(&mut self) {
        self.left_pane = LeftPane::Explorer;
        self.left_focused = true;
    }

    fn activate_selected(&mut self) -> Option<GlobalHomeExit> {
        if self.left_focused {
            return self
                .recent
                .get(self.recent_selected)
                .map(|entry| GlobalHomeExit::OpenRecentWorkspace(Box::new(entry.clone())));
        }
        self.file_action = Some(LAUNCHER_ACTIONS[self.action_selected].0.clone());
        None
    }

    fn submit_path_input(&mut self, input: PathInputModal) {
        let candidate = resolve_explorer_path(self.explorer.root(), &input.value);
        let mut rebased = self.explorer.clone();
        if rebased.rebase(candidate.clone()) {
            self.explorer = rebased;
            self.focus_explorer();
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

    fn handle_explorer_key(&mut self, key: KeyEvent) -> Option<GlobalHomeExit> {
        if key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Escape => {
                    self.left_focused = false;
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
        if self.authentication.is_active() {
            let _ = self.authentication.handle_event(TerminalEvent::Key(key));
            return None;
        }
        if self.error.is_some() {
            if key.kind == KeyEventKind::Press
                && matches!(key.code, KeyCode::Enter | KeyCode::Escape)
            {
                self.error = None;
            }
            return None;
        }

        if self.folder_dialog_failed.is_some() {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('e' | 'b') if has_plain_modifiers(key) => {
                        self.folder_dialog_failed = None;
                        self.focus_explorer();
                    }
                    KeyCode::Escape | KeyCode::Enter => self.folder_dialog_failed = None,
                    _ => {}
                }
            }
            return None;
        }

        if let Some(entry) = self.remove_workspace.as_ref() {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('y') if has_plain_modifiers(key) => {
                        self.file_action =
                            Some(GlobalHomeFileAction::RemoveWorkspace(entry.entry_ref()));
                        self.remove_workspace = None;
                    }
                    KeyCode::Escape => self.remove_workspace = None,
                    _ => {}
                }
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
                return Some(GlobalHomeExit::InitializeWorkspace(
                    modal.target,
                    modal.identity,
                ));
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

        if let Some(modal) = self.template.as_mut() {
            match modal.handle_key(key) {
                TemplateModalTransition::NotHandled | TemplateModalTransition::Handled => {}
                TemplateModalTransition::Close => self.template = None,
                TemplateModalTransition::Activate(request) => {
                    self.file_action = Some(GlobalHomeFileAction::Template(request));
                }
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

        if is_shortcut_help_key(key) {
            self.shortcut_help_visible = true;
            return None;
        }
        if key.kind == KeyEventKind::Press && has_plain_modifiers(key) {
            match key.code {
                KeyCode::Tab => {
                    self.left_focused = !self.left_focused;
                    return None;
                }
                KeyCode::Char('e') => {
                    self.left_pane = match self.left_pane {
                        LeftPane::Recent => LeftPane::Explorer,
                        LeftPane::Explorer => LeftPane::Recent,
                    };
                    return None;
                }
                KeyCode::Char('b') => {
                    self.focus_explorer();
                    return None;
                }
                _ => {}
            }
        }
        if self.explorer_focused() {
            return self.handle_explorer_key(key);
        }
        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && has_plain_modifiers(key)
        {
            let (selected, len) = if self.left_focused {
                (&mut self.recent_selected, self.recent.len())
            } else {
                (&mut self.action_selected, LAUNCHER_ACTIONS.len())
            };
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(len.saturating_sub(1));
                    return None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    return None;
                }
                _ => {}
            }
        }
        if key.kind != KeyEventKind::Press || !has_plain_modifiers(key) {
            return None;
        }
        match key.code {
            KeyCode::Enter => return self.activate_selected(),
            KeyCode::Char('q') => return Some(GlobalHomeExit::Quit),
            KeyCode::Char('g') => self.open_path_input(),
            KeyCode::Char('d') if self.left_focused => {
                self.remove_workspace = self.recent.get(self.recent_selected).cloned();
            }
            KeyCode::Char('r') => self.file_action = Some(GlobalHomeFileAction::ReloadRegistry),
            KeyCode::Char(shortcut) => {
                if let Some((action, _, _)) = LAUNCHER_ACTIONS
                    .iter()
                    .find(|(_, _, key)| key.starts_with(shortcut))
                {
                    self.file_action = Some(action.clone());
                }
            }
            _ => {}
        }
        None
    }

    fn handle_paste(&mut self, text: &str) {
        if self.authentication.is_active() {
            let _ = self
                .authentication
                .handle_event(TerminalEvent::Paste(text.to_string()));
            return;
        }
        if self.error.is_some()
            || self.initialize_workspace.is_some()
            || self.initialize_global_config.is_some()
            || self.template.is_some()
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

    fn select_global_home_folder(
        &mut self,
        _request: FolderSelectionRequest<'_>,
    ) -> io::Result<FolderSelection> {
        Err(io::Error::other(
            "folder picker terminal adapter unavailable",
        ))
    }

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

    fn select_global_home_folder(
        &mut self,
        request: FolderSelectionRequest<'_>,
    ) -> io::Result<FolderSelection> {
        select_folder(self, request)
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
    registry: Result<&RegistryStore, &str>,
) -> io::Result<GlobalHomeExit> {
    let paths = HomeActionPaths::current();
    run_with_terminal_and_paths(terminal, state, &paths, registry)
}

fn run_with_terminal_and_paths(
    terminal: &mut impl GlobalHomeTerminal,
    state: &mut GlobalHomeState,
    paths: &HomeActionPaths,
    registry: Result<&RegistryStore, &str>,
) -> io::Result<GlobalHomeExit> {
    let mut settings = None;
    let mut suspended_settings = None;
    let mut reload_suspended_settings = false;
    let mut dirty = true;

    loop {
        dirty |= state.authentication.poll();
        if dirty {
            terminal.draw_global_home(&mut |frame| {
                if let Some(page) = settings.as_mut() {
                    settings_screen::render_page(frame, page);
                } else {
                    render(frame, state);
                }
            })?;
            terminal.finish_global_home_redraw()?;
            dirty = false;
        }

        if !terminal.poll_global_home(GLOBAL_HOME_POLL_INTERVAL)? {
            continue;
        }

        let event = terminal.read_global_home()?;
        if let Some(page) = settings.as_mut() {
            if matches!(event, TerminalEvent::Resize(_)) {
                terminal.note_global_home_resize();
            }
            match page.handle_event(event) {
                SettingsTransition::None => {}
                SettingsTransition::Back => settings = None,
                SettingsTransition::OpenEditor => {
                    handle_file_action(
                        terminal,
                        state,
                        paths,
                        GlobalHomeFileAction::OpenGlobalConfig,
                    )?;
                    if state.error.is_some() || state.initialize_global_config.is_some() {
                        suspended_settings = settings.take();
                        reload_suspended_settings = false;
                    } else if let Some(page) = settings.as_mut() {
                        page.reload_after_editor();
                    }
                }
            }
            dirty = true;
            continue;
        }

        let exit = match event {
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
            match action {
                GlobalHomeFileAction::PickFolder => {
                    terminal.discard_global_home_input_batch()?;
                    let selection = terminal.select_global_home_folder(
                        FolderSelectionRequest::SystemDialog {
                            picker: &mut NativeFolderPicker,
                            initial_dir: Some(state.explorer.root()),
                        },
                    )?;
                    match selection {
                        FolderSelection::Selected(path) => {
                            return Ok(GlobalHomeExit::OpenWorkspace(path));
                        }
                        FolderSelection::Cancelled => {}
                        FolderSelection::OpenExplorer(_) => state.focus_explorer(),
                        FolderSelection::Failed(error) => state.folder_dialog_failed = Some(error),
                    }
                }
                GlobalHomeFileAction::ReloadRegistry => state.reload_registry(registry),
                GlobalHomeFileAction::RemoveWorkspace(entry) => {
                    terminal.discard_global_home_input_batch()?;
                    state.remove_from_list(registry, &entry);
                }
                GlobalHomeFileAction::OpenSettings => match paths.global_config() {
                    Ok(path) => settings = Some(SettingsPage::load(path.to_path_buf())),
                    Err(error) => state.show_home_action_error(
                        GlobalHomeErrorKind::GlobalConfig,
                        error.to_string(),
                    ),
                },
                action => {
                    let initialized_from_settings = suspended_settings.is_some()
                        && matches!(action, GlobalHomeFileAction::InitializeGlobalConfig(_));
                    handle_file_action(terminal, state, paths, action)?;
                    reload_suspended_settings |= initialized_from_settings;
                }
            }
        }
        if suspended_settings.is_some()
            && state.error.is_none()
            && state.initialize_global_config.is_none()
        {
            let mut page = suspended_settings
                .take()
                .expect("suspended Settings page must remain available");
            if reload_suspended_settings {
                page.reload_after_editor();
            }
            reload_suspended_settings = false;
            settings = Some(page);
        }
        dirty = true;
    }
}

fn load_home_config(paths: &HomeActionPaths) -> Result<Config, String> {
    let path = paths
        .global_config()
        .map_err(|error| format!("Global Config invalid: {error}"))?;
    Config::load_from(path).map_err(|error| format!("Global Config invalid: {error}"))
}

fn launch_target(
    terminal: &mut impl GlobalHomeTerminal,
    state: &mut GlobalHomeState,
    target: &Path,
    kind: GlobalHomeErrorKind,
) -> io::Result<()> {
    // A valid Config keeps the normal editor policy. Invalid Config is deliberately ignored only
    // for this repair action so its own editor field cannot prevent repair.
    let config = Config::load_from(target).unwrap_or_default();
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
        GlobalHomeFileAction::PickFolder
        | GlobalHomeFileAction::ReloadRegistry
        | GlobalHomeFileAction::RemoveWorkspace(_)
        | GlobalHomeFileAction::OpenSettings => {
            unreachable!("Settings is opened by the Global Home event loop")
        }
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
                Ok(crate::user_config_fs::EditableFileState::Existing) => {
                    launch_target(terminal, state, target, GlobalHomeErrorKind::GlobalConfig)
                }
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
            let config = Config::default();
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
        GlobalHomeFileAction::OpenTemplate => {
            match load_home_config(paths) {
                Ok(config) => state.open_template(paths.templates_dir_result(), config),
                Err(error) => {
                    state.show_home_action_error(GlobalHomeErrorKind::GlobalConfig, error)
                }
            }
            Ok(())
        }
        GlobalHomeFileAction::Template(request) => {
            let resolution = match state.template.as_ref() {
                Some(active) => terminal.resolve_global_home_editor(&active.config),
                None => return Ok(()),
            };
            let editor = match resolution {
                Ok(editor) => editor,
                Err(error) => {
                    state.show_template_error(error);
                    return Ok(());
                }
            };
            if request.action == TemplateAction::InitializeAndOpen {
                let Some(templates_dir) = request.path.parent() else {
                    state.show_template_error(format!(
                        "source template has no parent directory: {}",
                        request.path.display()
                    ));
                    return Ok(());
                };
                let mut reporter = super::EditorInitializationReporter;
                if let Err(error) = crate::commands::initialize_source_templates_at(
                    templates_dir,
                    std::slice::from_ref(&request.language),
                    &mut reporter,
                ) {
                    state.show_template_error(format!(
                        "failed to initialize source template: {error}"
                    ));
                    return Ok(());
                }
            }
            let outcome = match state.template.as_ref() {
                Some(active) => {
                    terminal.launch_global_home_editor(&active.config, &editor, &request.path)?
                }
                None => return Ok(()),
            };
            if outcome.discard_input_batch {
                terminal.discard_global_home_input_batch()?;
            }
            match outcome.result {
                HomeEditorResult::Launched => state.close_template(),
                HomeEditorResult::RecoverableError(error) => state.show_template_error(error),
            }
            Ok(())
        }
        GlobalHomeFileAction::OpenAuthentication => {
            state.authentication.open(paths.authentication_target());
            Ok(())
        }
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
    render_global_home(frame, state);

    if state.shortcut_help_visible {
        render_global_shortcuts(frame, state.explorer_focused());
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
    if let Some(entry) = state.remove_workspace.as_ref() {
        render_launcher_dialog(
            frame,
            " Remove from List ",
            &format!(
                "{}\n\nRemove this registration and its history?\nWorkspace files remain on disk.\n\ny Remove from List   Esc Cancel",
                entry.root().display()
            ),
        );
    }
    if let Some(error) = state.folder_dialog_failed.as_ref() {
        render_launcher_dialog(
            frame,
            " Folder Dialog Failed ",
            &format!("{error}\n\ne Explorer   Esc Dismiss"),
        );
    }
    if let Some(error) = state.error.as_ref() {
        render_error(frame, error);
    }
    if let Some(modal) = state.template.as_ref() {
        view::render_open_template_modal(frame, modal);
    }
    if state.authentication.is_active() {
        authentication_modal::render(frame, &state.authentication);
    }
}

/// Use the original proportions when both panes fit at their usual widths.
fn global_home_panes(area: Rect) -> Option<(Rect, Rect)> {
    let proportional = area.width.saturating_mul(30) / 100;
    let regular_width = proportional.clamp(LEFT_PANE_MIN_WIDTH, LEFT_PANE_MAX_WIDTH);
    let left_width = if area.width.saturating_sub(regular_width) >= DASHBOARD_MIN_WIDTH {
        regular_width
    } else if area.width >= COMPACT_PANE_WIDTH * 2 {
        COMPACT_PANE_WIDTH
    } else {
        return None;
    };
    Some((
        Rect::new(area.x, area.y, left_width, area.height),
        Rect::new(
            area.x + left_width,
            area.y,
            area.width - left_width,
            area.height,
        ),
    ))
}

fn render_global_home(frame: &mut Frame<'_>, state: &mut GlobalHomeState) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }

    let has_registry_status = state.registry_warning.is_some() || state.registry_notice.is_some();
    let [body, registry_status, selected_path, keys] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(u16::from(has_registry_status && area.height >= 3)),
        Constraint::Length(u16::from(area.height >= 7)),
        Constraint::Length(u16::from(area.height >= 2)),
    ])
    .areas(area);

    if let Some((left, right)) = global_home_panes(body) {
        render_left_pane(frame, left, state);
        render_dashboard(frame, right, state);
    } else if state.left_focused {
        render_left_pane(frame, body, state);
    } else {
        render_dashboard(frame, body, state);
    }

    if let Some(status) = state
        .registry_warning
        .as_deref()
        .or(state.registry_notice.as_deref())
    {
        frame.render_widget(
            Paragraph::new(view::clip_text_with_ellipsis(
                status,
                usize::from(registry_status.width),
            ))
            .style(Style::default().fg(Color::Yellow)),
            registry_status,
        );
    }
    let path = match state.left_pane {
        LeftPane::Recent => state
            .recent
            .get(state.recent_selected)
            .map(RegistryEntry::root),
        LeftPane::Explorer => Some(state.explorer.selected_path()),
    };
    if let Some(path) = path {
        frame.render_widget(
            Paragraph::new(prefixed_path_line(
                "Selected  ",
                path,
                usize::from(selected_path.width),
            )),
            selected_path,
        );
    }

    let toggle = match state.left_pane {
        LeftPane::Recent => "e Explorer",
        LeftPane::Explorer => "e Recent",
    };
    let hints = match (state.explorer_focused(), keys.width) {
        (true, 64..) => {
            format!("{toggle}   Tab Focus   o Open   g Go to Path   Esc Actions   ? Help")
        }
        (true, 38..) => format!("{toggle}  Tab Focus  o Open  ? Help"),
        (false, 62..) => format!("{toggle}   Tab Focus   g Go to Path   ? Help   q Quit"),
        (false, 38..) => format!("{toggle}  Tab Focus  ? Help  q Quit"),
        (_, _) => format!("{toggle}  Tab  ?"),
    };
    frame.render_widget(Paragraph::new(hints), keys);
}

fn render_left_pane(frame: &mut Frame<'_>, area: Rect, state: &mut GlobalHomeState) {
    match state.left_pane {
        LeftPane::Recent => render_recent_workspaces(frame, area, state),
        LeftPane::Explorer => {
            render_explorer_pane(frame, area, &mut state.explorer, state.left_focused)
        }
    }
}

fn render_explorer_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut ExplorerState,
    focused: bool,
) {
    let [tree, root] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(u16::from(area.height >= 4)),
    ])
    .areas(area);
    explorer::render(frame, tree, state, focused);
    let separator = Block::default().borders(Borders::RIGHT);
    let root_label = separator.inner(root);
    frame.render_widget(separator, root);
    frame.render_widget(
        Paragraph::new(prefixed_path_line(
            "Root  ",
            state.root(),
            usize::from(root_label.width),
        )),
        root_label,
    );
}

fn render_dashboard(frame: &mut Frame<'_>, area: Rect, state: &GlobalHomeState) {
    let [title, content] = Layout::vertical([
        Constraint::Length(u16::from(area.height >= 3)),
        Constraint::Min(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(
            Line::styled("Global Home", Style::default().add_modifier(Modifier::BOLD)).centered(),
        ),
        title,
    );

    let (logo_width, logo_height) = logo_size();
    let menu_height = (LAUNCHER_ACTIONS.len() as u16).min(content.height);
    let show_logo = content.width >= logo_width && content.height >= logo_height + 1 + menu_height;
    let logo_space = if show_logo { logo_height + 1 } else { 0 };
    let y = content.y + (content.height - logo_space - menu_height) / 2;
    if show_logo {
        let lines = branding::ascii_logo_lines().map(|line| {
            Line::styled(
                line,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        });
        frame.render_widget(
            Paragraph::new(Text::from_iter(lines)),
            centered_row(content, y, logo_width, logo_height),
        );
    }

    let menu = centered_row(content, y + logo_space, ACTION_MENU_WIDTH, menu_height);
    let first = (state.action_selected + 1).saturating_sub(usize::from(menu.height));
    let lines = LAUNCHER_ACTIONS
        .iter()
        .enumerate()
        .skip(first)
        .take(usize::from(menu.height))
        .map(|(index, (_, label, shortcut))| {
            let (marker, style) = row_cursor(index == state.action_selected, !state.left_focused);
            let mut line = menu_line(label, shortcut, usize::from(menu.width.saturating_sub(2)));
            line.spans.insert(0, Span::raw(marker));
            line.style(style)
        });
    frame.render_widget(Paragraph::new(Text::from_iter(lines)), menu);
}

fn row_cursor(selected: bool, focused: bool) -> (&'static str, Style) {
    match (selected, focused) {
        (true, true) => (
            "> ",
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        (true, false) => ("· ", Style::default().add_modifier(Modifier::BOLD)),
        (false, _) => ("  ", Style::default()),
    }
}

fn render_recent_workspaces(frame: &mut Frame<'_>, area: Rect, state: &mut GlobalHomeState) {
    let separator = Block::default().borders(Borders::RIGHT);
    let inner = separator.inner(area);
    frame.render_widget(separator, area);
    if inner.is_empty() {
        return;
    }

    let [heading, body] = Layout::vertical([
        Constraint::Length(u16::from(inner.height >= 2)),
        Constraint::Min(1),
    ])
    .areas(inner);
    let rows = usize::from(body.height);
    let len = state.recent.len();
    let first = (state.recent_selected + 1).saturating_sub(rows);
    state.recent_scroll = state
        .recent_scroll
        .clamp(first, state.recent_selected)
        .min(len.saturating_sub(rows));

    let marker = if state.left_focused { "> " } else { "" };
    let mut title = format!("{marker}Recent Workspaces");
    if len > rows && heading.width >= 30 {
        title.push_str(&format!(
            "  {}–{}/{len}",
            state.recent_scroll + 1,
            state.recent_scroll + rows
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        heading,
    );
    if state.recent.is_empty() {
        let message = if state.registry_warning.is_some() {
            "Registry unavailable"
        } else if body.width >= 24 {
            "No registered Workspaces"
        } else {
            "No workspaces"
        };
        frame.render_widget(Paragraph::new(message), body);
        return;
    }

    let width = usize::from(body.width);
    let lines = state
        .recent
        .iter()
        .enumerate()
        .skip(state.recent_scroll)
        .take(rows)
        .map(|(index, entry)| {
            let (marker, style) = row_cursor(index == state.recent_selected, state.left_focused);
            let root = entry.root();
            let name = root
                .file_name()
                .unwrap_or(root.as_os_str())
                .to_string_lossy();
            let name = view::clip_text_with_ellipsis(&name, width.saturating_sub(2));
            Line::styled(format!("{marker}{name}"), style)
        });
    frame.render_widget(Paragraph::new(Text::from_iter(lines)), body);
}

fn render_launcher_dialog(frame: &mut Frame<'_>, title: &str, text: &str) {
    let area = centered_rect(frame.area(), 68, 12);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_initialize_workspace(frame: &mut Frame<'_>, modal: &InitializeWorkspaceModal) {
    let area = centered_rect(frame.area(), 64, 14);
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
            Line::raw("Initialize here with the atc init defaults?"),
            Line::raw("abc → ABC/, arc → ARC/, agc → AGC/"),
            Line::raw("Other contests go directly in this folder."),
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

pub(super) fn render_shortcuts(frame: &mut Frame<'_>) {
    render_help(
        frame,
        vec![
            Line::raw("c Open / Create Contest"),
            Line::raw("s Settings"),
            Line::raw("w Workspace Config"),
            Line::raw("t Template"),
            Line::raw("a Authentication"),
            Line::raw(""),
            Line::raw("? keep open   Esc close"),
        ],
    );
}

fn render_global_shortcuts(frame: &mut Frame<'_>, explorer: bool) {
    let lines = if explorer {
        vec![
            Line::raw("Enter     Expand / Collapse"),
            Line::raw("↑↓ jk     Navigate"),
            Line::raw("←→ hl     Collapse / Expand"),
            Line::raw("Backspace Parent"),
            Line::raw("r Reload   o Open   g Go to Path"),
            Line::raw("e Recent   Tab / Esc Actions"),
            Line::raw("? keep open   Esc close Help"),
        ]
    } else {
        vec![
            Line::raw("↑↓ jk Navigate   Enter Select"),
            Line::raw("e Recent / Explorer   Tab Focus"),
            Line::raw("o Open Folder   g Go to Path"),
            Line::raw("d Remove from List   r Reload"),
            Line::raw("s Settings   t Templates"),
            Line::raw("a Authentication   q Quit"),
            Line::raw("? keep open   Esc close"),
        ]
    };
    render_help(frame, lines);
}

fn render_help(frame: &mut Frame<'_>, lines: Vec<Line<'_>>) {
    let area = centered_rect(frame.area(), 46, 11);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().title(" Help ").borders(Borders::ALL)),
        area,
    );
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
        | GlobalHomeErrorKind::WorkspaceOpen => 15,
        GlobalHomeErrorKind::GoToPath => 9,
        GlobalHomeErrorKind::RegistryRemoval | GlobalHomeErrorKind::GlobalConfig => 11,
    };
    let area = centered_rect(frame.area(), 64, height);
    let title = match error.kind {
        GlobalHomeErrorKind::WorkspaceOpen => " Workspace Open Failed ",
        GlobalHomeErrorKind::WorkspaceInitialization => " Workspace Initialization Failed ",
        GlobalHomeErrorKind::WorkspaceInitializedOpen => " Workspace Initialized, Open Failed ",
        GlobalHomeErrorKind::GoToPath => " Go to Path Failed ",
        GlobalHomeErrorKind::RegistryRemoval => " Remove from List Failed ",
        GlobalHomeErrorKind::GlobalConfig => " Global Config Failed ",
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

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

    fn wait_for_authentication_text(state: &mut GlobalHomeState, expected: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let _ = state.authentication.poll();
            let rendered = buffer_text(&draw(state, 100, 30));
            if rendered.contains(expected) {
                return rendered;
            }
            std::thread::yield_now();
        }
        panic!("authentication modal did not render the expected state");
    }

    fn wait_for_worker_start(started: &AtomicBool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if started.load(Ordering::SeqCst) {
                return;
            }
            std::thread::yield_now();
        }
        panic!("authentication worker did not start");
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
        batch_file_writes: VecDeque<Option<(PathBuf, String)>>,
        editor_file_contents: Option<String>,
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
                batch_file_writes: VecDeque::new(),
                editor_file_contents: None,
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
            if let Some(Some((path, contents))) = self.batch_file_writes.pop_front() {
                std::fs::write(path, contents)?;
            }
            self.active_batch = batch;
            if self.active_batch.is_empty() {
                std::thread::sleep(wait.min(Duration::from_millis(10)));
            }
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
            if let Some(contents) = self.editor_file_contents.as_ref() {
                std::fs::write(target, contents)?;
            }
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
    fn settings_editor_opens_invalid_existing_and_initializes_missing_global_config() {
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
                vec![event(KeyCode::Char('s'))],
                vec![event(KeyCode::Char('e'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
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
                vec![event(KeyCode::Char('s'))],
                vec![event(KeyCode::Char('e'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
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
    fn global_home_settings_reloads_after_direct_editor_and_returns_to_home() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let paths = HomeActionPaths::for_test(config.clone(), cookie_location(temp.path()));
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('s'))],
                vec![event(KeyCode::Char('e'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.editor_file_contents = Some("[defaults]\nlanguage = \"python\"\n".to_string());

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.editor_targets, [config]);
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("Global Settings"))
        );
        assert!(terminal.frames.iter().any(|frame| {
            frame.contains("Default language")
                && frame.contains("Python")
                && frame.contains("Modified")
        }));
    }

    #[test]
    fn global_home_settings_repairs_invalid_config_with_editor() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(&config, "invalid = [\n").unwrap();
        let paths = HomeActionPaths::for_test(config.clone(), cookie_location(temp.path()));
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('s'))],
                vec![event(KeyCode::Char('e'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.editor_file_contents = Some("[defaults]\nlanguage = \"python\"\n".to_string());

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.editor_targets, [config]);
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("Invalid Config"))
        );
        assert!(terminal.frames.iter().any(|frame| {
            frame.contains("Default language")
                && frame.contains("Python")
                && frame.contains("Modified")
        }));
    }

    #[test]
    fn valid_global_config_editor_uses_the_fresh_configured_editor() {
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
                vec![event(KeyCode::Char('s'))],
                vec![event(KeyCode::Char('e'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.use_production_editor_resolver = true;
        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
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
        assert_eq!(terminal.editor_configs_have_override, [true]);
    }

    #[test]
    fn global_home_template_fresh_loads_default_and_initializes_with_enter() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        let templates = temp.path().join("templates");
        std::fs::write(&config, "[defaults]\nlanguage = \"python\"\n").unwrap();
        let paths = HomeActionPaths::for_test_with_templates(
            config.clone(),
            templates.clone(),
            cookie_location(temp.path()),
        );
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('t'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(
            std::fs::read(templates.join("python.py")).unwrap(),
            crate::template::builtin_template(crate::language::Language::Python).as_bytes()
        );
        assert!(!templates.join("cpp.cpp").exists());
        assert_eq!(terminal.editor_targets, [templates.join("python.py")]);
        assert!(terminal.frames.iter().any(|frame| {
            frame.contains("Open Template")
                && frame.contains("Python")
                && frame.contains("Default")
                && frame.contains("[Enter] Initialize & Open")
        }));

        std::fs::write(&config, "invalid = [\n").unwrap();
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        assert_eq!(state.handle_key(key(KeyCode::Char('t'))), None);
        let action = state.take_file_action().unwrap();
        let mut direct_terminal = ScriptedGlobalTerminal::new(100, 30, []);
        handle_file_action(&mut direct_terminal, &mut state, &paths, action).unwrap();
        assert!(state.template.is_none());
        let error = state
            .error
            .as_ref()
            .expect("invalid Config must remain repairable");
        assert_eq!(error.kind, GlobalHomeErrorKind::GlobalConfig);
        assert!(error.message.contains("Global Config invalid"));
    }

    #[test]
    fn global_template_session_keeps_one_config_generation_and_reloads_after_close() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        let templates = temp.path().join("templates");
        let config_a = "[defaults]\nlanguage = \"cpp\"\n\
[editor]\ncommand = \"editor-a\"\nmode = \"external\"\n";
        let config_b = "[defaults]\nlanguage = \"python\"\n\
[editor]\ncommand = \"editor-b\"\nmode = \"external\"\n";
        std::fs::write(&config_path, config_a).unwrap();
        let paths = HomeActionPaths::for_test_with_templates(
            config_path.clone(),
            templates.clone(),
            cookie_location(temp.path()),
        );
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('t'))],
                vec![event(KeyCode::Char('q'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('t'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.use_production_editor_resolver = true;
        terminal.batch_file_writes =
            VecDeque::from([None, Some((config_path.clone(), config_b.to_string()))]);

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );

        assert!(terminal.frames[2].contains("cpp.cpp       Missing    Default"));
        assert!(terminal.frames[4].contains("python.py     Missing    Default"));
        assert_eq!(
            terminal
                .launched_editors
                .iter()
                .map(|editor| editor.program.as_os_str())
                .collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("editor-a"),
                std::ffi::OsStr::new("editor-b")
            ]
        );
        assert_eq!(
            terminal.editor_targets,
            [templates.join("cpp.cpp"), templates.join("python.py")]
        );
    }

    #[test]
    fn global_template_modal_owns_home_shortcuts_until_escape() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        state.open_template(Ok(temp.path().join("templates")), Config::default());

        for code in [
            KeyCode::Char('q'),
            KeyCode::Char('G'),
            KeyCode::Char('a'),
            KeyCode::Char('o'),
            KeyCode::Char('g'),
            KeyCode::Char('t'),
        ] {
            assert_eq!(state.handle_key(key(code)), None);
            assert!(state.take_file_action().is_none());
            assert!(state.template.is_some());
        }
        assert_eq!(state.handle_key(key(KeyCode::Escape)), None);
        assert!(state.template.is_none());
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q'))),
            Some(GlobalHomeExit::Quit)
        );
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
                vec![event(KeyCode::Char('s'))],
                vec![event(KeyCode::Char('e'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.resolve_error = Some("No editor configured.".to_string());

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
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
    fn authentication_modal_reports_invalid_regular_file_without_rendering_its_value() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        let cookie = cookie_location(temp.path());
        std::fs::create_dir_all(&cookie.state_dir).unwrap();
        let secret = "super-secret-invalid-cookie-value";
        write_cookie(&cookie.file, secret);
        let paths = HomeActionPaths::for_test(config, cookie.clone());
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('a'))],
                vec![],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert!(terminal.editor_targets.is_empty());
        assert!(terminal.frames.iter().any(|frame| {
            frame.contains("Status   Invalid")
                && frame.contains("Repair Cookie")
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
                vec![],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert!(!cookie.file.exists());
        assert!(terminal.editor_targets.is_empty());
        assert!(terminal.frames.iter().any(|frame| {
            frame.contains("Status   Not configured")
                && frame.contains("Paste Cookie")
                && frame.contains("Path")
                && frame.contains("cookie")
        }));
    }

    #[test]
    fn global_home_production_loop_keeps_authentication_paste_out_of_path_input() {
        let temp = tempfile::tempdir().unwrap();
        let cookie = cookie_location(temp.path());
        let paths = HomeActionPaths::for_test(temp.path().join("config.toml"), cookie.clone());
        let root = temp.path().to_path_buf();
        let secret = "global-loop-secret";
        let mut state = GlobalHomeState::new(root.clone());
        let mut terminal = ScriptedGlobalTerminal::new(
            100,
            30,
            [
                vec![event(KeyCode::Char('a'))],
                vec![],
                vec![event(KeyCode::Char('p'))],
                vec![TerminalEvent::Paste(secret.to_string())],
                vec![TerminalEvent::Resize(
                    super::super::terminal::TerminalSize {
                        columns: 101,
                        rows: 31,
                    },
                )],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(state.explorer.root(), root);
        assert!(!cookie.file.exists());
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("REVEL_SESSION=<hidden>"))
        );
        assert!(terminal.frames.iter().all(|frame| !frame.contains(secret)));
    }

    #[test]
    fn global_home_dispatch_blocks_close_during_save_and_reset() {
        let temp = tempfile::tempdir().unwrap();
        let cookie = cookie_location(temp.path());
        let paths = HomeActionPaths::for_test(temp.path().join("config.toml"), cookie.clone());
        let mut state = GlobalHomeState::new(temp.path().to_path_buf());
        state.authentication = AuthenticationModalController::new_with_verifier(|snapshot| {
            crate::atcoder::AuthenticationVerification::authenticated_for_test(
                &snapshot,
                Some("global_user".to_string()),
            )
        });
        state.authentication.open(paths.authentication_target());
        wait_for_authentication_text(&mut state, "Not configured");

        let save_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let save_started = Arc::new(AtomicBool::new(false));
        let hook_gate = Arc::clone(&save_gate);
        let hook_started = Arc::clone(&save_started);
        state
            .authentication
            .set_worker_hook_for_test(move |purpose, stage| {
                if purpose == authentication_modal::WorkerPurpose::Write
                    && stage == authentication_modal::WorkerTestStage::BeforeWork
                {
                    hook_started.store(true, Ordering::SeqCst);
                    let (released, wake) = &*hook_gate;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                }
            });
        assert_eq!(state.handle_key(key(KeyCode::Char('p'))), None);
        state.handle_paste("global-close-safe");
        assert_eq!(state.handle_key(key(KeyCode::Enter)), None);
        wait_for_worker_start(&save_started);
        for code in [KeyCode::Escape, KeyCode::Char('q'), KeyCode::Char('a')] {
            assert_eq!(state.handle_key(key(code)), None);
            assert!(state.authentication.is_active());
        }
        assert!(buffer_text(&draw(&mut state, 100, 30)).contains("Saving..."));
        let (released, wake) = &*save_gate;
        *released.lock().unwrap() = true;
        wake.notify_all();
        wait_for_authentication_text(&mut state, "Authenticated");

        let reset_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let reset_started = Arc::new(AtomicBool::new(false));
        let hook_gate = Arc::clone(&reset_gate);
        let hook_started = Arc::clone(&reset_started);
        state
            .authentication
            .set_worker_hook_for_test(move |purpose, stage| {
                if purpose == authentication_modal::WorkerPurpose::Reset
                    && stage == authentication_modal::WorkerTestStage::BeforeWork
                {
                    hook_started.store(true, Ordering::SeqCst);
                    let (released, wake) = &*hook_gate;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                }
            });
        state.handle_key(key(KeyCode::Char('r')));
        state.handle_key(key(KeyCode::Enter));
        wait_for_worker_start(&reset_started);
        for code in [KeyCode::Escape, KeyCode::Char('q'), KeyCode::Char('a')] {
            assert_eq!(state.handle_key(key(code)), None);
            assert!(state.authentication.is_active());
        }
        assert!(buffer_text(&draw(&mut state, 100, 30)).contains("Resetting..."));
        let (released, wake) = &*reset_gate;
        *released.lock().unwrap() = true;
        wake.notify_all();
        wait_for_authentication_text(&mut state, "Not configured");
        assert!(!cookie.file.exists());
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
                vec![],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert!(terminal.editor_targets.is_empty());
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("Status   Invalid"))
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
                vec![event(KeyCode::Char('s'))],
                vec![event(KeyCode::Char('e')), event(KeyCode::Char('q'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.editor_outcome = HomeEditorOutcome {
            result: HomeEditorResult::Launched,
            discard_input_batch: true,
        };

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.discarded_events, 1);
        assert_eq!(terminal.reads, 5);
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
                vec![event(KeyCode::Char('s'))],
                vec![
                    event(KeyCode::Char('e')),
                    event(KeyCode::Enter),
                    event(KeyCode::Char('q')),
                ],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        terminal.editor_outcome = HomeEditorOutcome {
            result: HomeEditorResult::RecoverableError("editor launch failed".to_string()),
            discard_input_batch: true,
        };

        assert_eq!(
            run_with_terminal_and_paths(
                &mut terminal,
                &mut state,
                &paths,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.discarded_events, 2);
        assert_eq!(terminal.reads, 7);
        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| frame.contains("editor launch failed"))
        );
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer.cell((x, y)).unwrap().symbol())
            .collect()
    }

    fn register_paths(state: &mut GlobalHomeState, roots: impl IntoIterator<Item = PathBuf>) {
        use crate::workspace_registry::VerifiedWorkspace;

        let mut snapshot = RegistrySnapshot::default();
        for root in roots {
            let workspace = VerifiedWorkspace::from_verified_root(root, None).unwrap();
            (snapshot, _) = snapshot
                .apply(RegistryMutation::Register(&workspace))
                .unwrap();
        }
        state.set_recent(snapshot);
        state.left_focused = true;
    }

    #[test]
    fn empty_recent_and_centered_open_folder_keep_the_old_split_and_logo() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        let buffer = draw(&mut state, 80, 24);
        let rendered = buffer_text(&buffer);
        assert!(row_text(&buffer, 0).starts_with("Recent Workspaces"));
        assert!(row_text(&buffer, 0).contains("Global Home"));
        assert_eq!(buffer.cell((31, 0)).unwrap().symbol(), "│");
        assert!(row_text(&buffer, 23).contains("e Explorer   Tab Focus"));
        for expected in branding::ascii_logo_lines().chain([
            "No registered Workspaces",
            "> Open Folder",
            "Settings",
            "Templates",
            "Authentication",
            "? Help",
            "q Quit",
        ]) {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }
        for removed in [
            "Create Workspace",
            "Browse in Terminal",
            "Workspace Actions",
            "Global Actions",
        ] {
            assert!(!rendered.contains(removed));
        }
        let menu_y = (0..24)
            .find(|&y| row_text(&buffer, y).contains("Open Folder"))
            .unwrap();
        assert_eq!(buffer.cell((70, menu_y)).unwrap().symbol(), "o");
        assert!(
            buffer
                .cell((41, menu_y))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );

        let compact = draw(&mut state, 40, 12);
        assert!(row_text(&compact, 0).starts_with("Recent Workspaces"));
        assert!(row_text(&compact, 0).contains("Global Home"));
        assert_eq!(compact.cell((19, 0)).unwrap().symbol(), "│");
        assert!(buffer_text(&compact).contains("> Open Folder"));
        assert!(buffer_text(&compact).contains("Authentication"));
    }

    #[test]
    fn e_and_tab_preserve_the_recent_tree_and_action_selections_independently() {
        let root = tempfile::tempdir().unwrap();
        let launch = root.path().join("browse");
        let child = launch.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let first = root.path().join("recent-one");
        let second = root.path().join("recent-two");
        let mut state = GlobalHomeState::new(launch.clone());
        register_paths(&mut state, [first, second.clone()]);

        state.handle_key(key(KeyCode::Down));
        let dashboard = draw(&mut state, 80, 24);
        assert!(buffer_text(&dashboard).contains("> recent-two"));
        assert!(row_text(&dashboard, 22).contains(&prefixed_path_line("Selected  ", &second, 80)));

        state.handle_key(key(KeyCode::Char('e')));
        assert!(state.explorer_focused());
        state.handle_key(key(KeyCode::Enter));
        state.handle_key(key(KeyCode::Right));
        assert_eq!(state.explorer.selected_path(), child);
        let wide = draw(&mut state, 80, 24);
        assert!(row_text(&wide, 0).starts_with("> Explorer"));
        assert!(!buffer_text(&wide).contains("Recent Workspaces"));
        assert!(
            wide.cell((0, 2))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(row_text(&wide, 22).contains(&prefixed_path_line("Selected  ", &child, 80)));

        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.left_pane, LeftPane::Explorer);
        state.handle_key(key(KeyCode::Down));
        let compact = draw(&mut state, 40, 12);
        assert!(row_text(&compact, 0).starts_with("Explorer"));
        assert!(buffer_text(&compact).contains("> Settings"));
        assert_eq!(state.explorer.selected_path(), child);
        state.handle_key(key(KeyCode::Char('e')));
        assert!(!state.left_focused);
        let compact = draw(&mut state, 40, 12);
        assert!(row_text(&compact, 0).starts_with("Recent Workspaces"));
        assert!(buffer_text(&compact).contains("· recent-two"));
        assert!(buffer_text(&compact).contains("> Settings"));
        assert!(row_text(&compact, 10).contains(&prefixed_path_line("Selected  ", &second, 40)));

        state.handle_key(key(KeyCode::Tab));
        let Some(GlobalHomeExit::OpenRecentWorkspace(entry)) =
            state.handle_key(key(KeyCode::Enter))
        else {
            panic!("Enter in Recent must open the selected Workspace");
        };
        assert_eq!(entry.root(), second);
        state.handle_key(key(KeyCode::Char('e')));
        assert_eq!(state.explorer.selected_path(), child);
        state.handle_key(key(KeyCode::Backspace));
        assert_eq!(state.explorer.root(), root.path());
        state.handle_key(key(KeyCode::Escape));
        assert_eq!(state.left_pane, LeftPane::Explorer);
        assert!(!state.left_focused);
    }

    #[test]
    fn recent_uses_one_line_per_name_and_scrolls_at_80_and_40_columns() {
        let root = tempfile::tempdir().unwrap();
        let long = root
            .path()
            .join("long-parent".repeat(10))
            .join("日本語📂-long-workspace-name");
        let roots = std::iter::once(long.clone())
            .chain((1..35).map(|index| root.path().join(format!("recent-{index:02}"))));
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        register_paths(&mut state, roots);

        let wide = draw(&mut state, 80, 24);
        let cell = wide.cell((2, 1)).unwrap();
        assert_eq!(cell.symbol(), "日");
        assert!(cell.modifier.contains(Modifier::REVERSED));
        assert!(row_text(&wide, 2).contains("recent-01"));
        assert!(!row_text(&wide, 1).contains("long-parent"));
        let path_row = row_text(&wide, 22);
        assert!(path_row.contains('…'));
        assert!(path_row.contains("long-workspace-name"));
        assert!(!path_row.contains('�'));
        assert!(buffer_text(&wide).contains("Recent Workspaces  1–21/35"));

        for _ in 0..28 {
            state.handle_key(key(KeyCode::Down));
        }
        let wide = draw(&mut state, 80, 24);
        assert!(buffer_text(&wide).contains("Recent Workspaces  9–29/35"));
        assert!(buffer_text(&wide).contains("> recent-28"));
        let compact = draw(&mut state, 40, 12);
        assert!(buffer_text(&compact).contains("> recent-28"));
        assert!(row_text(&compact, 10).contains("recent-28"));
        assert!(buffer_text(&compact).contains("Open Folder"));
        assert!(buffer_text(&compact).contains("q Quit"));
        assert_eq!(state.recent_selected, 28);
    }

    #[test]
    fn right_actions_keep_focus_and_help_and_quit_remain_shortcuts() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Down));
        state.handle_key(key(KeyCode::Char('e')));
        assert_eq!(state.left_pane, LeftPane::Explorer);
        assert!(!state.left_focused);
        assert_eq!(state.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(
            state.take_file_action(),
            Some(GlobalHomeFileAction::OpenSettings)
        );
        state.handle_key(key(KeyCode::Char('?')));
        assert!(state.shortcut_help_visible);
        state.handle_key(key(KeyCode::Escape));
        assert!(!state.shortcut_help_visible);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q'))),
            Some(GlobalHomeExit::Quit)
        );
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
        assert_eq!(state.handle_key(key(KeyCode::Char('s'))), None);
        assert_eq!(
            state.take_file_action(),
            Some(GlobalHomeFileAction::OpenSettings)
        );
        assert_eq!(state.handle_key(key(KeyCode::Char('G'))), None);
        assert!(state.take_file_action().is_none());
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
        state.show_initialize_workspace_confirmation(selected.clone(), None);
        assert!(state.explorer.rebase(changed_selection.clone()));
        let explorer_after_selection_change = state.explorer.clone();

        for code in [
            KeyCode::Char('o'),
            KeyCode::Char('g'),
            KeyCode::Char('e'),
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
            KeyCode::Tab,
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
            Some(GlobalHomeExit::InitializeWorkspace(selected, None))
        );
        assert!(state.initialize_workspace.is_none());
    }

    #[test]
    fn initialization_confirmation_consumes_pointer_events_in_the_production_loop() {
        let target = PathBuf::from("confirmed-target");
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.show_initialize_workspace_confirmation(target.clone(), None);
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
            run_with_terminal(
                &mut terminal,
                &mut state,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::InitializeWorkspace(target, None)
        );
        assert_eq!(terminal.reads, 3);
    }

    #[test]
    fn initialization_confirmation_cancel_preserves_explorer_at_any_width() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("child");
        std::fs::create_dir(&child).unwrap();

        for width in [61, 80] {
            let mut state = GlobalHomeState::new(root.path().to_path_buf());
            state.handle_key(key(KeyCode::Char('b')));
            let _ = draw(&mut state, width, 24);
            state.handle_key(key(KeyCode::Enter));
            state.handle_key(key(KeyCode::Char('j')));
            let explorer = state.explorer.clone();
            state.show_initialize_workspace_confirmation(child.clone(), None);
            assert_eq!(state.handle_key(key(KeyCode::Escape)), None);
            assert_eq!(state.explorer, explorer);
            assert_eq!(state.explorer.selected_path(), child);
            assert!(state.explorer_focused());
            assert_eq!(state.handle_key(key(KeyCode::Escape)), None);
            assert_eq!(
                state.handle_key(key(KeyCode::Char('q'))),
                Some(GlobalHomeExit::Quit)
            );
        }
    }

    #[test]
    fn initialization_confirmation_renders_unicode_safe_tail_and_tiny_frames() {
        let target = PathBuf::from(r"C:\very-long-parent\another-long-parent\projects\日本語\abc");
        let clipped = prefixed_path_line("", &target, 30);
        assert!(UnicodeWidthStr::width(clipped.as_str()) <= 30);
        assert!(clipped.ends_with(r"\projects\日本語\abc"));
        assert!(!clipped.contains('\u{fffd}'));
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.show_initialize_workspace_confirmation(target, None);

        for (width, height) in [(0, 0), (0, 8), (8, 0), (1, 1), (8, 3), (24, 5)] {
            let _ = draw(&mut state, width, height);
        }

        let rendered = buffer_text(&draw(&mut state, 80, 24));
        for expected in [
            "Initialize Workspace",
            "This directory is not an atc workspace.",
            "Initialize here with the atc init defaults?",
            "abc → ABC/, arc → ARC/, agc → AGC/",
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
        state.show_initialize_workspace_confirmation(target.clone(), None);

        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Some(GlobalHomeExit::InitializeWorkspace(target, None))
        );
    }

    #[test]
    fn explorer_shortcut_help_is_retained_and_one_shot() {
        let mut state = GlobalHomeState::new(PathBuf::from("root"));
        state.handle_key(key(KeyCode::Char('b')));
        state.handle_key(key(KeyCode::Char('?')));
        let rendered = buffer_text(&draw(&mut state, 80, 24));
        for expected in [
            "Help",
            "Enter     Expand / Collapse",
            "↑↓ jk     Navigate",
            "←→ hl     Collapse / Expand",
            "Backspace Parent",
            "r Reload",
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

        let line = prefixed_path_line("Selected  ", state.explorer.selected_path(), 36);
        assert!(UnicodeWidthStr::width(line.as_str()) <= 36);
        assert!(line.contains("Selected  …"));
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
    fn explorer_focus_survives_resize_without_state_loss() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("a-child");
        std::fs::create_dir(&child).unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());

        let dashboard = buffer_text(&draw(&mut state, 39, 12));
        assert!(dashboard.contains("Tab Focus"));
        assert!(dashboard.contains("? Help") && dashboard.contains("q Quit"));
        assert_eq!(state.handle_key(key(KeyCode::Char('b'))), None);
        assert!(state.explorer_focused());

        let overlay = buffer_text(&draw(&mut state, 39, 12));
        assert!(overlay.contains("Explorer"));
        assert!(overlay.contains("e Recent"));
        assert!(!overlay.contains("Global Home"));
        state.handle_key(key(KeyCode::Enter));
        state.handle_key(key(KeyCode::Char('j')));
        assert_eq!(state.explorer.selected_path(), child);

        let _ = draw(&mut state, 80, 12);
        assert!(state.explorer_focused());
        assert_eq!(state.explorer.selected_path(), child);
    }

    #[test]
    fn explorer_view_survives_zero_very_narrow_and_very_short_geometry() {
        for (width, height) in [(0, 0), (1, 1), (4, 2), (20, 3)] {
            let mut state = GlobalHomeState::new(PathBuf::from("root"));
            state.focus_explorer();
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
                vec![event(KeyCode::Char('b'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('j'))],
                vec![event(KeyCode::Enter)],
                vec![event(KeyCode::Char('o'))],
            ],
        );

        assert_eq!(
            run_with_terminal(
                &mut terminal,
                &mut state,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
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
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );

        assert_eq!(
            run_with_terminal(
                &mut terminal,
                &mut state,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );

        assert!(
            terminal
                .frames
                .iter()
                .any(|frame| { frame.contains("Explorer") && frame.contains("relative") })
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
        state.handle_key(key(KeyCode::Char('b')));
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
        wide_state.handle_key(key(KeyCode::Char('b')));
        let mut wide_terminal = ScriptedGlobalTerminal::new(
            80,
            24,
            [vec![event(KeyCode::Char('o')), event(KeyCode::Char('q'))]],
        );
        assert_eq!(
            run_with_terminal(
                &mut wide_terminal,
                &mut wide_state,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::OpenWorkspace(root.path().to_path_buf())
        );
        assert_eq!(wide_terminal.reads, 1);

        let mut narrow_state = GlobalHomeState::new(root.path().to_path_buf());
        let mut narrow_terminal = ScriptedGlobalTerminal::new(
            61,
            24,
            [
                vec![event(KeyCode::Char('b')), event(KeyCode::Char('q'))],
                vec![event(KeyCode::Escape)],
                vec![event(KeyCode::Char('q'))],
            ],
        );
        assert_eq!(
            run_with_terminal(
                &mut narrow_terminal,
                &mut narrow_state,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
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
            run_with_terminal(
                &mut modal_terminal,
                &mut modal_state,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
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
            run_with_terminal(
                &mut terminal,
                &mut state,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(terminal.reads, 2);
    }

    #[test]
    fn production_narrow_launcher_navigation_keeps_the_explorer_unloaded() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("child")).unwrap();
        let mut state = GlobalHomeState::new(root.path().to_path_buf());
        let old_explorer = state.explorer.clone();
        let mut terminal = ScriptedGlobalTerminal::new(
            39,
            24,
            [vec![event(KeyCode::Down)], vec![event(KeyCode::Char('q'))]],
        );

        assert_eq!(
            run_with_terminal(
                &mut terminal,
                &mut state,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
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
            39,
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
            run_with_terminal(
                &mut terminal,
                &mut state,
                Err("Registry unavailable in this test")
            )
            .unwrap(),
            GlobalHomeExit::Quit
        );
        assert_eq!(state.explorer, old_explorer);
        assert_eq!(std::env::current_dir().unwrap(), cwd_before);
    }
}
