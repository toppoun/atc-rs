use std::io;
use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

use super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminalEvent};
use super::view::{self, ContestOpenPurpose};
use super::{
    ContestOpenController, ContestOpenKeyResult, ContestSwitchResolution, ContestSwitchTask,
    ShortcutHelpTransition, TerminaSession, command_matches, is_command_palette_open_key,
    is_shortcut_help_key, shortcut_help_transition,
};

const HOME_POLL_INTERVAL: Duration = Duration::from_millis(20);

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

pub(crate) fn run<T>(
    terminal: &mut TerminaSession,
    resolve: &mut dyn FnMut(&str) -> ContestSwitchResolution,
    task: ContestSwitchTask,
    mut start_contest: impl FnMut() -> Result<T, String>,
) -> io::Result<HomeExit<T>> {
    let mut state = HomeState::new(resolve, task);
    let mut dirty = true;

    loop {
        dirty |= state.handle_operation_messages();
        if state.open_requested() {
            if let Some(contest) = state.start_requested_contest(&mut start_contest) {
                return Ok(HomeExit::Contest(contest));
            }
            dirty = true;
        }

        if dirty {
            terminal.draw(|frame| render(frame, &state))?;
            terminal.note_redraw_completed();
            terminal.refresh_mouse_after_redraw(false)?;
            terminal.retry_high_res_after_redraw(false)?;
            dirty = false;
        }

        if !terminal.poll(HOME_POLL_INTERVAL)? {
            continue;
        }
        match terminal.read()? {
            TerminalEvent::Key(key) => match state.handle_key(key) {
                HomeAction::None => dirty = true,
                HomeAction::OpenContest => {
                    dirty = true;
                }
                HomeAction::Quit => return Ok(HomeExit::Quit),
            },
            TerminalEvent::Resize(_) => {
                terminal.note_resize_dispatched();
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

fn render(frame: &mut Frame<'_>, state: &HomeState<'_>) {
    let area = centered_rect(frame.area(), 44, 10);
    let lines = vec![
        Line::styled("atc", Style::default().add_modifier(Modifier::BOLD)),
        Line::raw(""),
        Line::raw("AtCoder workspace"),
        Line::raw(""),
        Line::raw("c  Open Contest"),
        Line::raw(":  Commands"),
        Line::raw("?  Shortcuts"),
        Line::raw("q  Quit"),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        area,
    );

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
                let marker = if index == palette.selected { ">" } else { " " };
                let style = if index == palette.selected {
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Line::styled(
                    format!("{marker} {:<18} {}", command.label(), command.shortcut()),
                    style,
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
    frame.render_widget(Paragraph::new(Text::from(lines)), rows[1]);
    frame.render_widget(
        Paragraph::new("[↑↓] Select   [Enter] Run   [Esc] Cancel"),
        rows[2],
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use ratatui::{Terminal, backend::TestBackend};

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
    fn renders_minimal_home_and_home_overlays() {
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "enter a contest".into());
        let mut home = state(&mut resolve);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &home)).unwrap();
        let rendered = terminal.backend().to_string();
        for expected in [
            "atc",
            "AtCoder workspace",
            "c  Open Contest",
            ":  Commands",
            "?  Shortcuts",
            "q  Quit",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}\n{rendered}"
            );
        }

        home.handle_key(key(KeyCode::Char(':'), KeyEventKind::Press));
        terminal.draw(|frame| render(frame, &home)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Open Contest"));
        assert!(rendered.contains("Quit"));
        assert!(!rendered.contains("Run Tests"));
    }
}
