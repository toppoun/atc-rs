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

use super::template_modal::{
    OpenTemplateModal, TemplateAction, TemplateModalTransition, TemplateRequest,
};
use super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminalEvent};
use super::view::{self, ContestOpenPurpose};
use super::{
    ContestOpenController, ContestOpenKeyResult, ContestSwitchResolution, ContestSwitchTask,
    HomeActionPaths, HomeEditorOutcome, HomeEditorResult, ResolvedEditor, SubmissionHub,
    TerminaSession,
};
use crate::{branding, config::Config};

const HOME_POLL_INTERVAL: Duration = Duration::from_millis(20);
const HOME_ACTIONS: [Option<(&str, &str)>; 8] = [
    Some(("Open / Create Contest", "c")),
    None,
    Some(("Workspace Config", "w")),
    Some(("Global Config", "G")),
    Some(("Template", "t")),
    Some(("Authentication Cookie", "a")),
    None,
    Some(("Quit", "q")),
];
const MENU_WIDTH: u16 = 23;
const MENU_HEIGHT: u16 = HOME_ACTIONS.len() as u16;
const SUBTITLE: &str = "AtCoder workspace";
const WORKSPACE_PREFIX: &str = "Workspace  ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HomeAction {
    None,
    OpenContest,
    OpenWorkspaceConfig,
    OpenGlobalConfig,
    OpenTemplate,
    Template(TemplateRequest),
    ShowAuthenticationCookie,
    InitializeGlobalConfig(PathBuf),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeActionErrorKind {
    WorkspaceConfig,
    GlobalConfig,
    AuthenticationCookie,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HomeActionError {
    kind: HomeActionErrorKind,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitializeGlobalConfigModal {
    target: PathBuf,
}

/// Workspace Home owns only Home-specific UI state. Contest state remains mandatory inside
/// `WatchApp`/`SessionRuntime` and is created only after this state produces a prepared handoff.
struct HomeState<'a> {
    error: Option<HomeActionError>,
    initialize_global_config: Option<InitializeGlobalConfigModal>,
    template: Option<OpenTemplateModal>,
    open_contest: ContestOpenController<'a>,
}

impl<'a> HomeState<'a> {
    fn new(
        resolve: &'a mut dyn FnMut(&str) -> ContestSwitchResolution,
        task: ContestSwitchTask,
    ) -> Self {
        Self {
            error: None,
            initialize_global_config: None,
            template: None,
            open_contest: ContestOpenController::new(resolve, task),
        }
    }

    fn show_error(&mut self, kind: HomeActionErrorKind, message: String) {
        self.initialize_global_config = None;
        self.template = None;
        self.error = Some(HomeActionError { kind, message });
    }

    fn show_initialize_global_config(&mut self, target: PathBuf) {
        self.error = None;
        self.template = None;
        self.initialize_global_config = Some(InitializeGlobalConfigModal { target });
    }

    fn open_template(&mut self, templates_dir: Result<PathBuf, String>, config: &Config) {
        self.error = None;
        self.initialize_global_config = None;
        self.template = Some(OpenTemplateModal::new(
            templates_dir,
            config.defaults.language,
            config.defaults.language,
        ));
    }

    fn close_template(&mut self) {
        self.template = None;
    }

    fn show_template_error(&mut self, error: String) {
        if let Some(modal) = self.template.as_mut() {
            modal.set_error(error);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> HomeAction {
        if self.error.is_some() {
            if key.kind == KeyEventKind::Press
                && matches!(key.code, KeyCode::Enter | KeyCode::Escape)
            {
                self.error = None;
            }
            return HomeAction::None;
        }

        if self.initialize_global_config.is_some() {
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Escape {
                self.initialize_global_config = None;
                return HomeAction::None;
            }
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
                let modal = self
                    .initialize_global_config
                    .take()
                    .expect("global config initialization modal must remain active");
                return HomeAction::InitializeGlobalConfig(modal.target);
            }
            return HomeAction::None;
        }

        if let Some(modal) = self.template.as_mut() {
            return match modal.handle_key(key) {
                TemplateModalTransition::NotHandled | TemplateModalTransition::Handled => {
                    HomeAction::None
                }
                TemplateModalTransition::Close => {
                    self.template = None;
                    HomeAction::None
                }
                TemplateModalTransition::Activate(request) => HomeAction::Template(request),
            };
        }

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

        if key.kind != KeyEventKind::Press {
            return HomeAction::None;
        }
        if key.modifiers.control || key.modifiers.alt || key.modifiers.super_key {
            return HomeAction::None;
        }
        match key.code {
            KeyCode::Char('c') => {
                self.open_contest.open();
                HomeAction::None
            }
            KeyCode::Char('w') => HomeAction::OpenWorkspaceConfig,
            KeyCode::Char('G') => HomeAction::OpenGlobalConfig,
            KeyCode::Char('t') => HomeAction::OpenTemplate,
            KeyCode::Char('a') => HomeAction::ShowAuthenticationCookie,
            KeyCode::Char('q') => HomeAction::Quit,
            _ => HomeAction::None,
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

pub(crate) trait HomeTerminal {
    fn draw_home(&mut self, render: &mut dyn FnMut(&mut Frame<'_>)) -> io::Result<()>;
    fn finish_home_redraw(&mut self) -> io::Result<()>;
    fn note_home_resize(&mut self);
    fn poll_home(&mut self, wait: Duration) -> io::Result<bool>;
    fn read_home(&mut self) -> io::Result<TerminalEvent>;

    fn resolve_home_editor(&mut self, _config: &Config) -> Result<ResolvedEditor, String> {
        Err("editor launching is unavailable".to_string())
    }

    fn launch_home_editor(
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

    fn discard_home_input_batch(&mut self) -> io::Result<()> {
        for _ in 0..256 {
            if !self.poll_home(Duration::ZERO)? {
                break;
            }
            let _ = self.read_home()?;
        }
        Ok(())
    }
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

    fn resolve_home_editor(&mut self, config: &Config) -> Result<ResolvedEditor, String> {
        super::resolve_live_home_editor(self, config)
    }

    fn launch_home_editor(
        &mut self,
        config: &Config,
        editor: &ResolvedEditor,
        target: &Path,
    ) -> io::Result<HomeEditorOutcome> {
        super::launch_live_home_editor(self, config, editor, target)
    }
}

pub(crate) fn run_with_terminal<T>(
    terminal: &mut impl HomeTerminal,
    workspace_root: &Path,
    config: &Config,
    submissions: &mut SubmissionHub,
    resolve: &mut dyn FnMut(&str) -> ContestSwitchResolution,
    task: ContestSwitchTask,
    start_contest: impl FnMut() -> Result<T, String>,
) -> io::Result<HomeExit<T>> {
    let paths = HomeActionPaths::current();
    run_with_terminal_and_paths(
        terminal,
        workspace_root,
        config,
        submissions,
        resolve,
        task,
        start_contest,
        &paths,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_with_terminal_and_paths<T>(
    terminal: &mut impl HomeTerminal,
    workspace_root: &Path,
    config: &Config,
    submissions: &mut SubmissionHub,
    resolve: &mut dyn FnMut(&str) -> ContestSwitchResolution,
    task: ContestSwitchTask,
    mut start_contest: impl FnMut() -> Result<T, String>,
    paths: &HomeActionPaths,
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
                HomeAction::OpenWorkspaceConfig => {
                    open_workspace_config(terminal, &mut state, workspace_root, config)?;
                    dirty = true;
                }
                HomeAction::OpenGlobalConfig => {
                    open_global_config(terminal, &mut state, config, paths)?;
                    dirty = true;
                }
                HomeAction::OpenTemplate => {
                    state.open_template(paths.templates_dir_result(), config);
                    dirty = true;
                }
                HomeAction::Template(request) => {
                    handle_template_action(terminal, &mut state, config, request)?;
                    dirty = true;
                }
                HomeAction::ShowAuthenticationCookie => {
                    show_authentication_cookie_status(&mut state, paths);
                    dirty = true;
                }
                HomeAction::InitializeGlobalConfig(target) => {
                    initialize_and_open_global_config(terminal, &mut state, config, &target)?;
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

fn handle_template_action(
    terminal: &mut impl HomeTerminal,
    state: &mut HomeState<'_>,
    config: &Config,
    request: TemplateRequest,
) -> io::Result<()> {
    let editor = match terminal.resolve_home_editor(config) {
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
            state.show_template_error(format!("failed to initialize source template: {error}"));
            return Ok(());
        }
    }

    let outcome = terminal.launch_home_editor(config, &editor, &request.path)?;
    if outcome.discard_input_batch {
        terminal.discard_home_input_batch()?;
    }
    match outcome.result {
        HomeEditorResult::Launched => state.close_template(),
        HomeEditorResult::RecoverableError(error) => state.show_template_error(error),
    }
    Ok(())
}

fn launch_target(
    terminal: &mut impl HomeTerminal,
    state: &mut HomeState<'_>,
    config: &Config,
    target: &Path,
    kind: HomeActionErrorKind,
) -> io::Result<()> {
    let editor = match terminal.resolve_home_editor(config) {
        Ok(editor) => editor,
        Err(error) => {
            state.show_error(kind, error);
            return Ok(());
        }
    };
    launch_resolved_target(terminal, state, config, &editor, target, kind)
}

fn launch_resolved_target(
    terminal: &mut impl HomeTerminal,
    state: &mut HomeState<'_>,
    config: &Config,
    editor: &ResolvedEditor,
    target: &Path,
    kind: HomeActionErrorKind,
) -> io::Result<()> {
    let outcome = terminal.launch_home_editor(config, editor, target)?;
    if outcome.discard_input_batch {
        terminal.discard_home_input_batch()?;
    }
    match outcome.result {
        HomeEditorResult::Launched => {}
        HomeEditorResult::RecoverableError(error) => state.show_error(kind, error),
    }
    Ok(())
}

fn open_workspace_config(
    terminal: &mut impl HomeTerminal,
    state: &mut HomeState<'_>,
    workspace_root: &Path,
    config: &Config,
) -> io::Result<()> {
    let target = crate::workspace::workspace_config_path(workspace_root);
    match crate::workspace::inspect_workspace_config_file(workspace_root) {
        Ok(crate::workspace::WorkspaceConfigFileState::Existing) => launch_target(
            terminal,
            state,
            config,
            &target,
            HomeActionErrorKind::WorkspaceConfig,
        ),
        Ok(crate::workspace::WorkspaceConfigFileState::Missing) => {
            state.show_error(
                HomeActionErrorKind::WorkspaceConfig,
                format!(
                    "Workspace config is missing and was not recreated:\n{}",
                    target.display()
                ),
            );
            Ok(())
        }
        Err(error) => {
            state.show_error(HomeActionErrorKind::WorkspaceConfig, error.to_string());
            Ok(())
        }
    }
}

fn open_global_config(
    terminal: &mut impl HomeTerminal,
    state: &mut HomeState<'_>,
    config: &Config,
    paths: &HomeActionPaths,
) -> io::Result<()> {
    let target = match paths.global_config() {
        Ok(target) => target,
        Err(error) => {
            state.show_error(HomeActionErrorKind::GlobalConfig, error.to_string());
            return Ok(());
        }
    };
    match crate::user_config_fs::inspect_editable_file(target, "global config file") {
        Ok(crate::user_config_fs::EditableFileState::Existing) => launch_target(
            terminal,
            state,
            config,
            target,
            HomeActionErrorKind::GlobalConfig,
        ),
        Ok(crate::user_config_fs::EditableFileState::Missing) => {
            state.show_initialize_global_config(target.to_path_buf());
            Ok(())
        }
        Err(error) => {
            state.show_error(HomeActionErrorKind::GlobalConfig, error.to_string());
            Ok(())
        }
    }
}

fn initialize_and_open_global_config(
    terminal: &mut impl HomeTerminal,
    state: &mut HomeState<'_>,
    config: &Config,
    target: &Path,
) -> io::Result<()> {
    let editor = match terminal.resolve_home_editor(config) {
        Ok(editor) => editor,
        Err(error) => {
            state.show_error(HomeActionErrorKind::GlobalConfig, error);
            return Ok(());
        }
    };
    let mut reporter = super::EditorInitializationReporter;
    if let Err(error) = crate::commands::initialize_config_at(target, &mut reporter) {
        state.show_error(
            HomeActionErrorKind::GlobalConfig,
            format!("failed to initialize global config: {error}"),
        );
        return Ok(());
    }
    launch_resolved_target(
        terminal,
        state,
        config,
        &editor,
        target,
        HomeActionErrorKind::GlobalConfig,
    )
}

fn show_authentication_cookie_status(state: &mut HomeState<'_>, paths: &HomeActionPaths) {
    state.show_error(
        HomeActionErrorKind::AuthenticationCookie,
        super::authentication_cookie_status(paths),
    );
}

pub(super) fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
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

pub(super) fn logo_size() -> (u16, u16) {
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

pub(super) fn centered_row(area: Rect, y: u16, width: u16, height: u16) -> Rect {
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

pub(super) fn menu_line(
    label: &'static str,
    shortcut: &'static str,
    width: usize,
) -> Line<'static> {
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

pub(super) fn truncate_start_with_ellipsis(text: &str, width: usize) -> String {
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
        let lines = HOME_ACTIONS.iter().map(|action| match action {
            Some((label, shortcut)) => menu_line(label, shortcut, usize::from(layout.menu.width)),
            None => Line::raw(""),
        });
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

    if let Some(modal) = state.open_contest.modal() {
        view::render_contest_open_modal(frame, modal, ContestOpenPurpose::Open);
    }
    if let Some(modal) = state.initialize_global_config.as_ref() {
        render_initialize_global_config(frame, modal);
    }
    if let Some(error) = state.error.as_ref() {
        render_home_action_error(frame, error);
    }
    if let Some(modal) = state.template.as_ref() {
        view::render_open_template_modal(frame, modal);
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
    let target = truncate_start_with_ellipsis(
        modal.target.to_string_lossy().as_ref(),
        usize::from(inner.width),
    );
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
        ])),
        inner,
    );
}

fn render_home_action_error(frame: &mut Frame<'_>, error: &HomeActionError) {
    let title = match error.kind {
        HomeActionErrorKind::WorkspaceConfig => " Workspace Config Unavailable ",
        HomeActionErrorKind::GlobalConfig => " Global Config Failed ",
        HomeActionErrorKind::AuthenticationCookie => " Authentication Cookie ",
    };
    let area = centered_rect(frame.area(), 68, 13);
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width > 0 && inner.height > 0 {
        frame.render_widget(
            Paragraph::new(format!("{}\n\nEnter / Esc  Dismiss", error.message))
                .wrap(Wrap { trim: false }),
            inner,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
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

    #[derive(Debug)]
    struct RecordingHomeTerminal {
        targets: Vec<PathBuf>,
        resolve_error: Option<String>,
        outcome: HomeEditorOutcome,
        discarded_batches: usize,
    }

    impl Default for RecordingHomeTerminal {
        fn default() -> Self {
            Self {
                targets: Vec::new(),
                resolve_error: None,
                outcome: HomeEditorOutcome {
                    result: HomeEditorResult::Launched,
                    discard_input_batch: false,
                },
                discarded_batches: 0,
            }
        }
    }

    impl HomeTerminal for RecordingHomeTerminal {
        fn draw_home(&mut self, _render: &mut dyn FnMut(&mut Frame<'_>)) -> io::Result<()> {
            unreachable!("direct action tests do not draw")
        }

        fn finish_home_redraw(&mut self) -> io::Result<()> {
            unreachable!("direct action tests do not redraw")
        }

        fn note_home_resize(&mut self) {
            unreachable!("direct action tests do not resize")
        }

        fn poll_home(&mut self, _wait: Duration) -> io::Result<bool> {
            unreachable!("direct action tests do not poll")
        }

        fn read_home(&mut self) -> io::Result<TerminalEvent> {
            unreachable!("direct action tests do not read")
        }

        fn resolve_home_editor(&mut self, _config: &Config) -> Result<ResolvedEditor, String> {
            self.resolve_error.clone().map_or_else(
                || {
                    Ok(ResolvedEditor {
                        program: "test-editor".into(),
                        args: Vec::new(),
                        mode: crate::editor::EditorLaunchMode::External,
                        source: crate::editor::EditorSource::EditorEnv,
                    })
                },
                Err,
            )
        }

        fn launch_home_editor(
            &mut self,
            _config: &Config,
            _editor: &ResolvedEditor,
            target: &Path,
        ) -> io::Result<HomeEditorOutcome> {
            self.targets.push(target.to_path_buf());
            Ok(self.outcome.clone())
        }

        fn discard_home_input_batch(&mut self) -> io::Result<()> {
            self.discarded_batches += 1;
            Ok(())
        }
    }

    struct ScriptedHomeTerminal {
        batches: VecDeque<VecDeque<TerminalEvent>>,
        active_batch: VecDeque<TerminalEvent>,
        frames: Vec<String>,
        targets: Vec<PathBuf>,
        reads: usize,
    }

    impl ScriptedHomeTerminal {
        fn new(batches: impl IntoIterator<Item = Vec<TerminalEvent>>) -> Self {
            Self {
                batches: batches.into_iter().map(VecDeque::from).collect(),
                active_batch: VecDeque::new(),
                frames: Vec::new(),
                targets: Vec::new(),
                reads: 0,
            }
        }
    }

    impl HomeTerminal for ScriptedHomeTerminal {
        fn draw_home(&mut self, render: &mut dyn FnMut(&mut Frame<'_>)) -> io::Result<()> {
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal.draw(|frame| render(frame)).unwrap();
            self.frames.push(buffer_text(terminal.backend().buffer()));
            Ok(())
        }

        fn finish_home_redraw(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn note_home_resize(&mut self) {}

        fn poll_home(&mut self, wait: Duration) -> io::Result<bool> {
            if !self.active_batch.is_empty() {
                return Ok(true);
            }
            if wait == Duration::ZERO {
                return Ok(false);
            }
            let Some(batch) = self.batches.pop_front() else {
                return Err(io::Error::other("scripted Workspace Home input exhausted"));
            };
            self.active_batch = batch;
            Ok(!self.active_batch.is_empty())
        }

        fn read_home(&mut self) -> io::Result<TerminalEvent> {
            self.reads = self.reads.saturating_add(1);
            self.active_batch
                .pop_front()
                .ok_or_else(|| io::Error::other("scripted Workspace Home batch is empty"))
        }

        fn resolve_home_editor(&mut self, _config: &Config) -> Result<ResolvedEditor, String> {
            Ok(ResolvedEditor {
                program: "test-editor".into(),
                args: Vec::new(),
                mode: crate::editor::EditorLaunchMode::External,
                source: crate::editor::EditorSource::EditorEnv,
            })
        }

        fn launch_home_editor(
            &mut self,
            _config: &Config,
            _editor: &ResolvedEditor,
            target: &Path,
        ) -> io::Result<HomeEditorOutcome> {
            self.targets.push(target.to_path_buf());
            Ok(HomeEditorOutcome {
                result: HomeEditorResult::Launched,
                discard_input_batch: false,
            })
        }
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

    #[test]
    fn workspace_config_uses_the_active_root_and_is_never_recreated() {
        let root = tempfile::tempdir().unwrap();
        let target = crate::workspace::workspace_config_path(root.path());
        std::fs::write(&target, "malformed = [\n").unwrap();
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        let mut terminal = RecordingHomeTerminal::default();

        open_workspace_config(&mut terminal, &mut home, root.path(), &Config::default()).unwrap();
        assert_eq!(terminal.targets.as_slice(), std::slice::from_ref(&target));

        std::fs::remove_file(&target).unwrap();
        open_workspace_config(&mut terminal, &mut home, root.path(), &Config::default()).unwrap();
        assert!(!target.exists());
        assert_eq!(terminal.targets.len(), 1);
        assert!(matches!(
            home.error.as_ref().map(|error| error.kind),
            Some(HomeActionErrorKind::WorkspaceConfig)
        ));
    }

    #[test]
    fn workspace_home_global_config_opens_existing_and_initializes_only_after_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config.toml");
        let paths = HomeActionPaths::for_test(target.clone(), cookie_location(temp.path()));
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        let mut terminal = RecordingHomeTerminal::default();

        std::fs::write(&target, "invalid = [\n").unwrap();
        open_global_config(&mut terminal, &mut home, &Config::default(), &paths).unwrap();
        assert_eq!(terminal.targets.as_slice(), std::slice::from_ref(&target));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "invalid = [\n");

        std::fs::remove_file(&target).unwrap();
        open_global_config(&mut terminal, &mut home, &Config::default(), &paths).unwrap();
        assert!(home.initialize_global_config.is_some());
        assert!(!target.exists());
        let HomeAction::InitializeGlobalConfig(confirmed) =
            home.handle_key(key(KeyCode::Enter, KeyEventKind::Press))
        else {
            panic!("Enter must confirm global config initialization")
        };
        initialize_and_open_global_config(&mut terminal, &mut home, &Config::default(), &confirmed)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            crate::config::INITIAL_CONFIG
        );
        assert_eq!(terminal.targets, [target.clone(), target]);
    }

    #[test]
    fn workspace_home_resolves_editor_before_initializing_global_config() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config.toml");
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        let mut terminal = RecordingHomeTerminal {
            resolve_error: Some("No editor configured.".to_string()),
            ..RecordingHomeTerminal::default()
        };

        initialize_and_open_global_config(&mut terminal, &mut home, &Config::default(), &target)
            .unwrap();

        assert!(!target.exists());
        assert!(terminal.targets.is_empty());
        assert!(
            home.error
                .as_ref()
                .is_some_and(|error| error.message.contains("No editor configured."))
        );
    }

    #[test]
    fn workspace_home_template_uses_runtime_config_and_enter_initializes_only_selection() {
        let temp = tempfile::tempdir().unwrap();
        let templates = temp.path().join("templates");
        let paths = HomeActionPaths::for_test_with_templates(
            temp.path().join("config.toml"),
            templates.clone(),
            cookie_location(temp.path()),
        );
        let mut config = Config::default();
        config.defaults.language = crate::language::Language::Python;
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        assert_eq!(
            home.handle_key(key(KeyCode::Char('t'), KeyEventKind::Press)),
            HomeAction::OpenTemplate
        );
        home.open_template(paths.templates_dir_result(), &config);
        assert_eq!(
            home.template.as_ref().unwrap().selected_language(),
            crate::language::Language::Python
        );

        for code in [
            KeyCode::Char('q'),
            KeyCode::Char('G'),
            KeyCode::Char('a'),
            KeyCode::Char('c'),
            KeyCode::Char('o'),
            KeyCode::Char('t'),
        ] {
            assert_eq!(
                home.handle_key(key(code, KeyEventKind::Press)),
                HomeAction::None
            );
            assert!(home.template.is_some());
        }

        let HomeAction::Template(request) =
            home.handle_key(key(KeyCode::Enter, KeyEventKind::Press))
        else {
            panic!("Enter must activate the missing selected template")
        };
        assert_eq!(request.language, crate::language::Language::Python);
        assert_eq!(request.action, TemplateAction::InitializeAndOpen);
        assert_eq!(request.path, templates.join("python.py"));

        let mut terminal = RecordingHomeTerminal {
            resolve_error: Some("No editor configured.".to_string()),
            ..RecordingHomeTerminal::default()
        };
        handle_template_action(&mut terminal, &mut home, &config, request).unwrap();
        assert!(!templates.exists());
        assert!(home.template.as_ref().unwrap().error.is_some());

        terminal.resolve_error = None;
        let HomeAction::Template(request) =
            home.handle_key(key(KeyCode::Enter, KeyEventKind::Press))
        else {
            panic!("Enter must allow retry after resolver recovery")
        };
        handle_template_action(&mut terminal, &mut home, &config, request).unwrap();
        assert_eq!(
            std::fs::read(templates.join("python.py")).unwrap(),
            crate::template::builtin_template(crate::language::Language::Python).as_bytes()
        );
        assert!(!templates.join("cpp.cpp").exists());
        assert_eq!(terminal.targets, [templates.join("python.py")]);
        assert!(home.template.is_none());
    }

    #[test]
    fn workspace_template_runs_through_the_production_home_loop() {
        let temp = tempfile::tempdir().unwrap();
        let templates = temp.path().join("templates");
        let paths = HomeActionPaths::for_test_with_templates(
            temp.path().join("config.toml"),
            templates.clone(),
            cookie_location(temp.path()),
        );
        let mut config = Config::default();
        config.defaults.language = crate::language::Language::Python;
        let mut terminal = ScriptedHomeTerminal::new([
            vec![TerminalEvent::Key(key(
                KeyCode::Char('t'),
                KeyEventKind::Press,
            ))],
            vec![TerminalEvent::Key(key(
                KeyCode::Char('q'),
                KeyEventKind::Press,
            ))],
            vec![TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))],
            vec![TerminalEvent::Key(key(
                KeyCode::Char('t'),
                KeyEventKind::Press,
            ))],
            vec![TerminalEvent::Key(key(
                KeyCode::Escape,
                KeyEventKind::Press,
            ))],
            vec![TerminalEvent::Key(key(
                KeyCode::Char('q'),
                KeyEventKind::Press,
            ))],
        ]);
        let mut submissions = SubmissionHub::new();
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());

        let exit = run_with_terminal_and_paths(
            &mut terminal,
            temp.path(),
            &config,
            &mut submissions,
            &mut resolve,
            Arc::new(|_, _| Ok(())),
            || Ok::<(), String>(()),
            &paths,
        )
        .unwrap();

        assert!(matches!(exit, HomeExit::Quit));
        assert_eq!(terminal.reads, 6);
        assert_eq!(terminal.targets, [templates.join("python.py")]);
        assert_eq!(
            std::fs::read(templates.join("python.py")).unwrap(),
            crate::template::builtin_template(crate::language::Language::Python).as_bytes()
        );
        assert!(terminal.frames[1].contains("[Enter] Initialize & Open"));
        assert!(terminal.frames[2].contains("[Enter] Initialize & Open"));
        assert!(terminal.frames[4].contains("[Enter] Open"));
    }

    #[test]
    fn workspace_template_terminal_outcome_preserves_discard_and_recoverable_error_contract() {
        let temp = tempfile::tempdir().unwrap();
        let templates = temp.path().join("templates");
        std::fs::create_dir(&templates).unwrap();
        let cpp = templates.join("cpp.cpp");
        std::fs::write(&cpp, "// ready\n").unwrap();
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        let config = Config::default();
        home.open_template(Ok(templates), &config);
        let HomeAction::Template(request) =
            home.handle_key(key(KeyCode::Enter, KeyEventKind::Press))
        else {
            panic!("ready template must open")
        };
        let mut terminal = RecordingHomeTerminal {
            outcome: HomeEditorOutcome {
                result: HomeEditorResult::RecoverableError("launch failed".to_string()),
                discard_input_batch: true,
            },
            ..RecordingHomeTerminal::default()
        };
        handle_template_action(&mut terminal, &mut home, &config, request).unwrap();

        assert_eq!(terminal.discarded_batches, 1);
        assert_eq!(terminal.targets, [cpp]);
        assert_eq!(
            home.template.as_ref().unwrap().error.as_deref(),
            Some("launch failed")
        );
    }

    #[test]
    fn workspace_home_cookie_action_never_creates_a_missing_credential() {
        let temp = tempfile::tempdir().unwrap();
        let cookie = cookie_location(temp.path());
        let paths = HomeActionPaths::for_test(temp.path().join("config.toml"), cookie.clone());
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        show_authentication_cookie_status(&mut home, &paths);
        assert!(!cookie.file.exists());
        assert!(
            home.error
                .as_ref()
                .is_some_and(|error| error.message.contains("Status: Not configured")
                    && error.message.contains("REVEL_SESSION=<value>"))
        );

        home.error = None;
        std::fs::create_dir_all(&cookie.state_dir).unwrap();
        write_cookie(&cookie.file, "REVEL_SESSION=secret-not-for-ui");
        show_authentication_cookie_status(&mut home, &paths);
        assert!(home.error.as_ref().is_some_and(|error| {
            error.message.contains("Status: Configured")
                && error.message.contains(&cookie.file.display().to_string())
                && !error.message.contains("secret-not-for-ui")
        }));
    }

    #[test]
    fn terminal_editor_launch_marks_workspace_home_input_for_discard() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        let mut terminal = RecordingHomeTerminal {
            outcome: HomeEditorOutcome {
                result: HomeEditorResult::Launched,
                discard_input_batch: true,
            },
            ..RecordingHomeTerminal::default()
        };

        launch_target(
            &mut terminal,
            &mut home,
            &Config::default(),
            Path::new("config.toml"),
            HomeActionErrorKind::GlobalConfig,
        )
        .unwrap();
        assert_eq!(terminal.discarded_batches, 1);
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
    fn q_still_quits_workspace_home() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        assert_eq!(
            home.handle_key(key(KeyCode::Char('q'), KeyEventKind::Press)),
            HomeAction::Quit
        );
    }

    #[test]
    fn colon_and_question_mark_are_ignored_while_direct_actions_still_work() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        assert_eq!(
            home.handle_key(key(KeyCode::Char(':'), KeyEventKind::Press)),
            HomeAction::None
        );
        assert_eq!(
            home.handle_key(key(KeyCode::Char('?'), KeyEventKind::Press)),
            HomeAction::None
        );
        assert!(!home.open_contest.modal_active());
        assert_eq!(
            home.handle_key(key(KeyCode::Char('w'), KeyEventKind::Press)),
            HomeAction::OpenWorkspaceConfig
        );
        assert_eq!(
            home.handle_key(key(KeyCode::Char('G'), KeyEventKind::Press)),
            HomeAction::OpenGlobalConfig
        );
        assert_eq!(
            home.handle_key(key(KeyCode::Char('a'), KeyEventKind::Press)),
            HomeAction::ShowAuthenticationCookie
        );
    }

    #[test]
    fn contest_modal_owns_home_action_keys() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);

        home.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        home.handle_key(key(KeyCode::Char('G'), KeyEventKind::Press));
        assert!(home.open_contest.modal_active());
        assert_eq!(home.open_contest.modal().unwrap().contest_id, "G");
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
            "Open / Create Contest",
            "Workspace Config",
            "Global Config",
            "Template",
            "Authentication Cookie",
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
        for (offset, action) in HOME_ACTIONS.iter().enumerate() {
            let row = layout.menu.y + u16::try_from(offset).unwrap();
            let text = row_text(&buffer, row);
            let start = usize::from(layout.menu.x);
            let end = start + usize::from(layout.menu.width);
            if let Some((label, shortcut)) = action {
                assert_eq!(
                    &text[start..end],
                    menu_line(label, shortcut, usize::from(MENU_WIDTH)).to_string()
                );

                let shortcut_column = layout.menu.x + layout.menu.width - 1;
                let shortcut_cell = buffer.cell((shortcut_column, row)).unwrap();
                assert_eq!(shortcut_cell.symbol(), *shortcut);
                assert_eq!(shortcut_cell.fg, Color::Yellow);
            } else {
                assert!(text[start..end].trim().is_empty());
            }
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
        let height = 9;
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
        assert_eq!(menu_only.menu.height, 4);
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

        home.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        let open = buffer_text(&draw_home(&home, Path::new("workspace"), 80, 24));
        assert!(open.contains("Open Contest"));
        assert!(open.contains("Contest:"));
        assert!(open.contains('┌'));

        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        home.show_initialize_global_config(PathBuf::from("config.toml"));
        let initialize = buffer_text(&draw_home(&home, Path::new("workspace"), 80, 24));
        assert!(initialize.contains("Initialize Global Config"));
        assert!(initialize.contains("Initialize & Open"));

        home.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        home.show_error(HomeActionErrorKind::WorkspaceConfig, "missing".to_string());
        let error = buffer_text(&draw_home(&home, Path::new("workspace"), 80, 24));
        assert!(error.contains("Workspace Config Unavailable"));
        assert!(error.contains("Enter / Esc"));
    }
}
