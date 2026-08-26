pub mod app;
mod detail;
pub(crate) mod detail_analysis;
mod detail_layout;
mod detail_scrollbar;
pub mod message;
mod mouse;
pub mod reporter;
mod termina_adapter;
mod terminal;
pub mod view;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;

use crate::app_context::AppContext;
use crate::config::Config;
use crate::editor::{self, EditorLaunchMode, ResolvedEditor};
use crate::error::AppError;
use crate::language::Language;
use crate::model::Contest;
use crate::ui::{Event, Reporter};
use app::WatchApp;
use detail_layout::{DetailAnalysisCommand, DetailAnalysisResult};
pub(crate) use detail_layout::{
    DetailAnalysisCommand as SessionDetailAnalysisCommand,
    DetailAnalysisResult as SessionDetailAnalysisResult,
};
use detail_scrollbar::{DetailScrollbarHit, DetailScrollbarStableIdentity};
use message::{Message, RunRequest, RunWorkerCommand};
use mouse::{
    MouseMode, TerminalPixelMetrics, normalize_absolute_pixels, project_absolute_pixels_to_cells,
};
pub(crate) use terminal::TerminaSession;
use terminal::{
    KeyCode, KeyEvent, KeyEventKind, PointerButton, PointerEvent, PointerKind, TerminalEvent,
};

const MAX_MESSAGES_PER_TICK: usize = 256;
const MAX_DETAIL_ANALYSIS_RESULTS_PER_TICK: usize = 64;
const MAX_TERMINAL_EVENTS_PER_TICK: usize = 256;
const DETAIL_SCROLL_LINES: usize = 3;
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrontendPreferences {
    debug: bool,
    samples_pane: bool,
}

impl FrontendPreferences {
    fn apply(self, app: &mut WatchApp) {
        if self.debug != app.debug_enabled() {
            app.toggle_debug();
        }
        if self.samples_pane != app.samples_pane_enabled() {
            app.toggle_samples_pane();
        }
    }

    fn capture(&mut self, app: &WatchApp) {
        self.debug = app.debug_enabled();
        self.samples_pane = app.samples_pane_enabled();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrontendAction {
    RunTests,
    OpenSource,
    ToggleDebug,
    ToggleSamples,
    StartStress,
    StopStress,
    InitializeStress,
    SwitchContest,
}

impl FrontendAction {
    const ALL: [Self; 8] = [
        Self::RunTests,
        Self::OpenSource,
        Self::ToggleDebug,
        Self::ToggleSamples,
        Self::StartStress,
        Self::StopStress,
        Self::InitializeStress,
        Self::SwitchContest,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::RunTests => "Run Tests",
            Self::OpenSource => "Open Source",
            Self::ToggleDebug => "Toggle Debug",
            Self::ToggleSamples => "Toggle Samples",
            Self::StartStress => "Start Stress",
            Self::StopStress => "Stop Stress",
            Self::InitializeStress => "Initialize Stress",
            Self::SwitchContest => "Switch Contest",
        }
    }

    pub(super) const fn shortcut(self) -> Option<&'static str> {
        match self {
            Self::RunTests => Some("r"),
            Self::OpenSource => None,
            Self::ToggleDebug => Some("d"),
            Self::ToggleSamples => Some("s"),
            Self::StartStress => Some("S"),
            Self::StopStress => None,
            Self::InitializeStress => Some("i"),
            Self::SwitchContest => Some("c"),
        }
    }

    fn availability(
        self,
        app: &WatchApp,
        contest_switch_available: bool,
    ) -> FrontendActionAvailability {
        match self {
            Self::OpenSource if app.current_problem().is_none() => {
                FrontendActionAvailability::Unavailable("no selected problem")
            }
            Self::RunTests | Self::StartStress
                if app
                    .current_problem()
                    .and_then(|problem| problem.source.as_ref())
                    .is_none() =>
            {
                FrontendActionAvailability::Unavailable("no source file")
            }
            Self::StartStress
                if app.current_problem().is_some_and(|problem| {
                    matches!(
                        &problem.stress_setup,
                        app::StressSetupState::Required { .. }
                    )
                }) =>
            {
                FrontendActionAvailability::Unavailable("stress helpers not initialized")
            }
            Self::InitializeStress
                if !app.current_problem().is_some_and(|problem| {
                    matches!(
                        &problem.stress_setup,
                        app::StressSetupState::Required { .. }
                    )
                }) =>
            {
                FrontendActionAvailability::Unavailable("stress initialization not required")
            }
            Self::StopStress if app.active_stress_identity().is_none() => {
                FrontendActionAvailability::Unavailable("stress is not running")
            }
            Self::SwitchContest if !contest_switch_available => {
                FrontendActionAvailability::Unavailable("not in a workspace")
            }
            Self::RunTests
            | Self::OpenSource
            | Self::ToggleDebug
            | Self::ToggleSamples
            | Self::StartStress
            | Self::StopStress
            | Self::InitializeStress
            | Self::SwitchContest => FrontendActionAvailability::Available,
        }
    }

    fn from_shortcut(key: KeyEvent) -> Option<Self> {
        if key.kind != KeyEventKind::Press {
            return None;
        }

        match key.code {
            KeyCode::Char('r') => Some(Self::RunTests),
            KeyCode::Char('d') => Some(Self::ToggleDebug),
            KeyCode::Char('s') => Some(Self::ToggleSamples),
            KeyCode::Char('S') => Some(Self::StartStress),
            KeyCode::Char('i') => Some(Self::InitializeStress),
            KeyCode::Char('c')
                if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
            {
                Some(Self::SwitchContest)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OpenSourceModal {
    problem: usize,
    pub(super) problem_index: String,
    source_root: PathBuf,
    selected_language: Language,
    pub(super) error: Option<String>,
}

impl OpenSourceModal {
    pub(super) fn selected_language(&self) -> Language {
        self.selected_language
    }

    pub(super) fn path_for(&self, language: Language) -> io::Result<PathBuf> {
        crate::workspace::source_file_path(&self.source_root, &self.problem_index, language)
    }

    pub(super) fn selected_path(&self) -> io::Result<PathBuf> {
        self.path_for(self.selected_language)
    }

    pub(super) fn current_language(&self, app: &WatchApp) -> Option<Language> {
        let source = app.problems().get(self.problem)?.source.as_ref()?;
        (self.path_for(source.language).ok()? == source.path).then_some(source.language)
    }
}

fn source_modal_escape_closes(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press && key.code == KeyCode::Escape
}

#[derive(Debug)]
struct OpenSourceController {
    destination: PathBuf,
    default_language: Language,
    modal: Option<OpenSourceModal>,
    discard_input_batch: bool,
}

impl OpenSourceController {
    fn new(destination: &Path, default_language: Language) -> Self {
        Self {
            destination: destination.to_path_buf(),
            default_language,
            modal: None,
            discard_input_batch: false,
        }
    }

    fn modal(&self) -> Option<&OpenSourceModal> {
        self.modal.as_ref()
    }

    fn modal_active(&self) -> bool {
        self.modal.is_some()
    }

    fn selected_path(&self) -> io::Result<PathBuf> {
        let modal = self
            .modal
            .as_ref()
            .ok_or_else(|| io::Error::other("Open Source modal is not active"))?;
        modal.selected_path()
    }

    fn open(&mut self, app: &WatchApp) -> bool {
        let Some(problem) = app.current_problem() else {
            return false;
        };
        let Some(problem_number) = app.selected_problem() else {
            return false;
        };
        let problem_index = problem.index.clone();
        let current_language = problem.source.as_ref().and_then(|source| {
            crate::workspace::source_file_path(&self.destination, &problem_index, source.language)
                .ok()
                .filter(|path| *path == source.path)
                .map(|_| source.language)
        });
        self.discard_input_batch = false;
        self.modal = Some(OpenSourceModal {
            problem: problem_number,
            problem_index,
            source_root: self.destination.clone(),
            selected_language: current_language.unwrap_or(self.default_language),
            error: None,
        });
        true
    }

    fn close(&mut self) {
        self.modal = None;
    }

    fn take_discard_input_batch(&mut self) -> bool {
        std::mem::take(&mut self.discard_input_batch)
    }

    fn handle_key(
        &mut self,
        app: &mut WatchApp,
        key: KeyEvent,
        editor: &mut dyn SourceEditorHost,
        creator: &mut dyn SourceCreator,
    ) -> io::Result<bool> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(false);
        }

        let changed = match key.code {
            KeyCode::Escape if source_modal_escape_closes(key) => {
                self.close();
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let modal = self.modal.as_mut().expect("active modal must exist");
                modal.selected_language = match modal.selected_language {
                    Language::Cpp => Language::Python,
                    Language::Python => Language::Cpp,
                };
                modal.error = None;
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let modal = self.modal.as_mut().expect("active modal must exist");
                modal.selected_language = match modal.selected_language {
                    Language::Cpp => Language::Python,
                    Language::Python => Language::Cpp,
                };
                modal.error = None;
                true
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                return self.open_selected(app, false, editor, creator);
            }
            KeyCode::Char('i') if key.kind == KeyEventKind::Press => {
                return self.open_selected(app, true, editor, creator);
            }
            _ => false,
        };
        Ok(changed)
    }

    fn open_selected(
        &mut self,
        app: &mut WatchApp,
        create: bool,
        editor: &mut dyn SourceEditorHost,
        creator: &mut dyn SourceCreator,
    ) -> io::Result<bool> {
        let (problem, problem_index, language) = {
            let modal = self.modal.as_ref().expect("active modal must exist");
            (
                modal.problem,
                modal.problem_index.clone(),
                modal.selected_language,
            )
        };
        let target = match self.selected_path() {
            Ok(target) => target,
            Err(error) => {
                self.set_error(error.to_string());
                return Ok(true);
            }
        };
        let exists = target.is_file();
        if create == exists {
            return Ok(false);
        }

        let resolved = match editor.resolve() {
            Ok(editor) => editor,
            Err(error) => {
                self.set_error(error);
                return Ok(true);
            }
        };

        let target = if create {
            match creator.create(&self.destination, &problem_index, language) {
                Ok(path) => path,
                Err(error) => {
                    self.set_error(error);
                    return Ok(true);
                }
            }
        } else {
            target
        };

        app.source_changed(problem, target.clone(), language);
        let launch = match resolved.mode {
            EditorLaunchMode::External => editor.launch_external(&resolved, &target),
            EditorLaunchMode::Terminal => {
                // The terminal lifecycle replaces its parser and flushes platform input, but the
                // frontend may already have collected later events in this batch. Do not replay
                // those pre-editor keys or pointers after the blocking editor returns.
                self.discard_input_batch = true;
                editor.launch_terminal(&resolved, &target)
            }
        };
        match launch {
            Ok(()) => self.close(),
            Err(SourceLaunchError::Recoverable(error)) => self.set_error(error),
            Err(SourceLaunchError::TerminalRestore(error)) => {
                return Err(io::Error::other(error));
            }
        }
        Ok(true)
    }

    fn set_error(&mut self, error: String) {
        if let Some(modal) = self.modal.as_mut() {
            modal.error = Some(error);
        }
    }
}

#[derive(Debug)]
enum SourceLaunchError {
    Recoverable(String),
    TerminalRestore(String),
}

trait SourceEditorHost {
    fn resolve(&mut self) -> Result<ResolvedEditor, String>;
    fn launch_external(
        &mut self,
        editor: &ResolvedEditor,
        target: &Path,
    ) -> Result<(), SourceLaunchError>;
    fn launch_terminal(
        &mut self,
        editor: &ResolvedEditor,
        target: &Path,
    ) -> Result<(), SourceLaunchError>;
}

struct LiveSourceEditorHost<'a> {
    terminal: &'a mut TerminaSession,
    config: &'a Config,
}

impl SourceEditorHost for LiveSourceEditorHost<'_> {
    fn resolve(&mut self) -> Result<ResolvedEditor, String> {
        editor::resolve(self.config).map_err(|error| error.to_string())
    }

    fn launch_external(
        &mut self,
        editor: &ResolvedEditor,
        target: &Path,
    ) -> Result<(), SourceLaunchError> {
        editor::launch(editor, target)
            .map_err(|error| SourceLaunchError::Recoverable(error.to_string()))
    }

    fn launch_terminal(
        &mut self,
        editor: &ResolvedEditor,
        target: &Path,
    ) -> Result<(), SourceLaunchError> {
        self.terminal
            .suspend_and_run(|| editor::launch(editor, target))
            .map_err(|error| {
                let terminal_restore_failed = matches!(
                    error,
                    terminal::SuspendedRunError::SuspendAndResume { .. }
                        | terminal::SuspendedRunError::Resume(_)
                        | terminal::SuspendedRunError::OperationAndResume { .. }
                );
                let message = error.to_string();
                if terminal_restore_failed {
                    SourceLaunchError::TerminalRestore(message)
                } else {
                    SourceLaunchError::Recoverable(message)
                }
            })
    }
}

trait SourceCreator {
    fn create(
        &mut self,
        destination: &Path,
        problem_index: &str,
        language: Language,
    ) -> Result<PathBuf, String>;
}

struct DefaultSourceCreator;

impl SourceCreator for DefaultSourceCreator {
    fn create(
        &mut self,
        destination: &Path,
        problem_index: &str,
        language: Language,
    ) -> Result<PathBuf, String> {
        let mut reporter = SourceCreationReporter;
        crate::commands::create_source(destination, problem_index, language, &mut reporter)
            .map_err(|error| error.to_string())
    }
}

struct SourceCreationReporter;

impl Reporter for SourceCreationReporter {
    fn report(&mut self, _event: Event<'_>) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrontendActionAvailability {
    Available,
    Unavailable(&'static str),
}

impl FrontendActionAvailability {
    fn is_available(self) -> bool {
        self == Self::Available
    }
}

fn command_matches(label: &str, query: &str) -> bool {
    let words = label
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();

    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|token| words.iter().any(|word| word.starts_with(&token)))
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct CommandPalette {
    open: bool,
    pub(super) query: String,
    selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandPaletteKeyResult {
    NotHandled,
    Handled(bool),
    ExecuteRequested(FrontendAction),
}

impl CommandPalette {
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

    pub(super) fn filtered_actions(&self) -> Vec<FrontendAction> {
        FrontendAction::ALL
            .into_iter()
            .filter(|action| command_matches(action.label(), &self.query))
            .collect()
    }

    pub(super) fn selected_action(&self) -> Option<FrontendAction> {
        self.filtered_actions().get(self.selected).copied()
    }

    pub(super) fn is_selected(&self, index: usize) -> bool {
        self.selected == index
    }

    pub(super) fn selected_index(&self) -> usize {
        self.selected
    }

    fn reset_selection(&mut self) {
        self.selected = 0;
    }

    fn select_previous(&mut self) -> bool {
        let count = self.filtered_actions().len();
        if count <= 1 {
            self.selected = 0;
            return false;
        }
        self.selected = if self.selected == 0 {
            count - 1
        } else {
            self.selected.min(count - 1) - 1
        };
        true
    }

    fn select_next(&mut self) -> bool {
        let count = self.filtered_actions().len();
        if count <= 1 {
            self.selected = 0;
            return false;
        }
        self.selected = (self.selected + 1) % count;
        true
    }

    fn remove_last_grapheme(&mut self) -> bool {
        let Some((start, _)) = self.query.grapheme_indices(true).next_back() else {
            return false;
        };
        self.query.truncate(start);
        self.reset_selection();
        true
    }

    fn handle_key(&mut self, key: KeyEvent) -> CommandPaletteKeyResult {
        if !self.is_active() {
            return CommandPaletteKeyResult::NotHandled;
        }
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return CommandPaletteKeyResult::Handled(false);
        }

        match key.code {
            KeyCode::Escape if key.kind == KeyEventKind::Press => {
                self.close();
                CommandPaletteKeyResult::Handled(true)
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => self
                .selected_action()
                .map(CommandPaletteKeyResult::ExecuteRequested)
                .unwrap_or(CommandPaletteKeyResult::Handled(false)),
            KeyCode::Backspace => CommandPaletteKeyResult::Handled(self.remove_last_grapheme()),
            KeyCode::Up => CommandPaletteKeyResult::Handled(self.select_previous()),
            KeyCode::Down => CommandPaletteKeyResult::Handled(self.select_next()),
            KeyCode::Char(character)
                if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
            {
                self.query.push(character);
                self.reset_selection();
                CommandPaletteKeyResult::Handled(true)
            }
            _ => CommandPaletteKeyResult::Handled(false),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionExit {
    Quit,
    SwitchContest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContestSwitchResolution {
    pub(crate) destination: Option<std::path::PathBuf>,
    pub(crate) error: Option<String>,
    target: Option<ContestSwitchTarget>,
}

impl ContestSwitchResolution {
    pub(crate) fn accepted(destination: std::path::PathBuf) -> Self {
        Self {
            destination: Some(destination),
            error: None,
            target: Some(ContestSwitchTarget::Existing),
        }
    }

    pub(crate) fn missing(destination: std::path::PathBuf) -> Self {
        Self {
            destination: Some(destination),
            error: None,
            target: Some(ContestSwitchTarget::Missing),
        }
    }

    pub(crate) fn repair_required(destination: std::path::PathBuf) -> Self {
        Self {
            destination: Some(destination),
            error: None,
            target: Some(ContestSwitchTarget::RepairRequired),
        }
    }

    pub(crate) fn rejected(destination: Option<std::path::PathBuf>, error: String) -> Self {
        Self {
            destination,
            error: Some(error),
            target: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContestSwitchTarget {
    Existing,
    Missing,
    RepairRequired,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum SwitchContestModalState {
    #[default]
    Input,
    Creating,
    Repairing,
    Failed,
}

impl SwitchContestModalState {
    fn is_running(self) -> bool {
        matches!(self, Self::Creating | Self::Repairing)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContestSwitchMutation {
    Create,
    Repair,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContestSwitchProgress {
    ContestFetching {
        contest_id: String,
    },
    ContestFetched {
        contest_id: String,
        problems: usize,
    },
    ProblemFetching {
        index: String,
        current: usize,
        total: usize,
    },
    ProblemFetched {
        index: String,
        samples: usize,
    },
    ProblemFetchFailed {
        index: String,
        error: String,
    },
    WorkspaceCreated {
        destination: PathBuf,
    },
    WorkspaceRefreshed {
        destination: PathBuf,
    },
    WorkspaceRepaired {
        destination: PathBuf,
    },
}

impl ContestSwitchProgress {
    pub(super) fn display_line(&self) -> String {
        match self {
            Self::ContestFetching { .. } => "Fetching contest...".to_string(),
            Self::ContestFetched {
                contest_id,
                problems,
            } => format!("Found {problems} problems in {contest_id}"),
            Self::ProblemFetching {
                index,
                current,
                total,
            } => format!("[{current}/{total}] Fetching {index}..."),
            Self::ProblemFetched { index, samples } => {
                format!("Fetched {index} ({samples} samples)")
            }
            Self::ProblemFetchFailed { index, error } => {
                format!("Failed to fetch {index}: {error}")
            }
            Self::WorkspaceCreated { destination } => {
                format!("Created {}", destination.display())
            }
            Self::WorkspaceRefreshed { .. } => "Contest refreshed".to_string(),
            Self::WorkspaceRepaired { .. } => "Contest repaired".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContestSwitchRequest {
    pub(crate) mutation: ContestSwitchMutation,
    pub(crate) contest_id: String,
    pub(crate) destination: PathBuf,
}

pub(crate) type ContestSwitchTask = Arc<
    dyn Fn(ContestSwitchRequest, &mut dyn Reporter) -> Result<(), AppError> + Send + Sync + 'static,
>;

enum ContestSwitchOperationMessage {
    Progress(ContestSwitchProgress),
    Finished(Result<(), String>),
}

struct ContestSwitchReporter {
    tx: Sender<ContestSwitchOperationMessage>,
}

impl Reporter for ContestSwitchReporter {
    fn report(&mut self, event: Event<'_>) {
        let progress = match event {
            Event::ContestFetching { contest_id } => ContestSwitchProgress::ContestFetching {
                contest_id: contest_id.to_owned(),
            },
            Event::ContestFetched {
                contest_id,
                problems,
            } => ContestSwitchProgress::ContestFetched {
                contest_id: contest_id.to_owned(),
                problems,
            },
            Event::ProblemFetching {
                index,
                current,
                total,
            } => ContestSwitchProgress::ProblemFetching {
                index: index.to_owned(),
                current,
                total,
            },
            Event::ProblemFetched { index, samples } => ContestSwitchProgress::ProblemFetched {
                index: index.to_owned(),
                samples,
            },
            Event::ProblemFetchFailed { index, error } => {
                ContestSwitchProgress::ProblemFetchFailed {
                    index: index.to_owned(),
                    error: error.to_owned(),
                }
            }
            Event::WorkspaceCreated { destination } => ContestSwitchProgress::WorkspaceCreated {
                destination: destination.to_path_buf(),
            },
            Event::WorkspaceRefreshed { destination } => {
                ContestSwitchProgress::WorkspaceRefreshed {
                    destination: destination.to_path_buf(),
                }
            }
            Event::WorkspaceRepaired { destination } => ContestSwitchProgress::WorkspaceRepaired {
                destination: destination.to_path_buf(),
            },
            _ => return,
        };

        let _ = self
            .tx
            .send(ContestSwitchOperationMessage::Progress(progress));
    }
}

struct ActiveContestSwitchOperation {
    rx: Receiver<ContestSwitchOperationMessage>,
    handle: JoinHandle<()>,
}

struct ContestSwitchOperation {
    task: ContestSwitchTask,
    active: Option<ActiveContestSwitchOperation>,
}

impl ContestSwitchOperation {
    fn new(task: ContestSwitchTask) -> Self {
        Self { task, active: None }
    }

    fn start(&mut self, request: ContestSwitchRequest) -> io::Result<()> {
        if self.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a contest switch operation is already running",
            ));
        }

        let (tx, rx) = mpsc::channel();
        let task = Arc::clone(&self.task);
        let thread_name = match request.mutation {
            ContestSwitchMutation::Create => "atc-tui-contest-create",
            ContestSwitchMutation::Repair => "atc-tui-contest-repair",
        };
        let handle = thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let mut reporter = ContestSwitchReporter { tx: tx.clone() };
                let result = task(request, &mut reporter).map_err(|error| error.to_string());
                drop(reporter);
                let _ = tx.send(ContestSwitchOperationMessage::Finished(result));
            })?;

        self.active = Some(ActiveContestSwitchOperation { rx, handle });
        Ok(())
    }

    fn try_recv(&mut self) -> Option<ContestSwitchOperationMessage> {
        let result = self.active.as_ref()?.rx.try_recv();
        match result {
            Ok(ContestSwitchOperationMessage::Finished(result)) => {
                let join_result = self.join_active();
                Some(ContestSwitchOperationMessage::Finished(match join_result {
                    Ok(()) => result,
                    Err(error) => Err(error.to_string()),
                }))
            }
            Ok(message) => Some(message),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                let result = self.join_active().and_then(|()| {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "contest switch worker disconnected before reporting completion",
                    ))
                });
                Some(ContestSwitchOperationMessage::Finished(
                    result.map_err(|error| error.to_string()),
                ))
            }
        }
    }

    fn join_active(&mut self) -> io::Result<()> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        active
            .handle
            .join()
            .map_err(|_| io::Error::other("contest switch worker panicked"))
    }
}

impl Drop for ContestSwitchOperation {
    fn drop(&mut self) {
        let _ = self.join_active();
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct SwitchContestModal {
    pub(super) contest_id: String,
    pub(super) destination: Option<std::path::PathBuf>,
    pub(super) error: Option<String>,
    target: Option<ContestSwitchTarget>,
    pub(super) state: SwitchContestModalState,
    pub(super) progress: Vec<ContestSwitchProgress>,
    pub(super) mutation: Option<ContestSwitchMutation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContestSwitchKeyResult {
    NotHandled,
    Handled,
    SwitchRequested,
}

struct ContestSwitchController<'a> {
    available: bool,
    current_destination: &'a Path,
    modal: Option<SwitchContestModal>,
    resolve: &'a mut dyn FnMut(&str) -> ContestSwitchResolution,
    same_existing_destination: fn(&Path, &Path) -> io::Result<bool>,
    operation: ContestSwitchOperation,
    switch_requested: bool,
}

impl<'a> ContestSwitchController<'a> {
    fn new(
        context: &AppContext,
        current_destination: &'a Path,
        resolve: &'a mut dyn FnMut(&str) -> ContestSwitchResolution,
        switch_task: ContestSwitchTask,
    ) -> Self {
        Self::new_with_identity_check(
            context,
            current_destination,
            resolve,
            switch_task,
            existing_destinations_have_same_identity,
        )
    }

    fn new_with_identity_check(
        context: &AppContext,
        current_destination: &'a Path,
        resolve: &'a mut dyn FnMut(&str) -> ContestSwitchResolution,
        switch_task: ContestSwitchTask,
        same_existing_destination: fn(&Path, &Path) -> io::Result<bool>,
    ) -> Self {
        Self {
            available: context.workspace_root().is_some(),
            current_destination,
            modal: None,
            resolve,
            same_existing_destination,
            operation: ContestSwitchOperation::new(switch_task),
            switch_requested: false,
        }
    }

    fn modal(&self) -> Option<&SwitchContestModal> {
        self.modal.as_ref()
    }

    fn modal_active(&self) -> bool {
        self.modal.is_some()
    }

    fn open(&mut self) -> bool {
        if !self.available || self.modal_active() {
            return false;
        }
        self.modal = Some(SwitchContestModal::default());
        true
    }

    fn escape_dismisses_modal(&self) -> bool {
        self.modal
            .as_ref()
            .is_some_and(|modal| !modal.state.is_running())
    }

    fn normalize_resolution(
        &self,
        mut resolution: ContestSwitchResolution,
    ) -> ContestSwitchResolution {
        if resolution.target == Some(ContestSwitchTarget::RepairRequired)
            && let Some(destination) = resolution.destination.as_deref()
        {
            match (self.same_existing_destination)(destination, self.current_destination) {
                Ok(true) => {
                    resolution.target = None;
                    resolution.error = Some(
                        "Cannot repair the active contest from Switch Contest yet.".to_string(),
                    );
                }
                Ok(false) => {}
                Err(error) => {
                    resolution.target = None;
                    resolution.error = Some(format!(
                        "Cannot determine whether the repair target is the active contest: {error}"
                    ));
                }
            }
        }
        resolution
    }

    fn refresh_resolution(&mut self) {
        let Some(contest_id) = self.modal.as_ref().map(|modal| modal.contest_id.clone()) else {
            return;
        };
        let resolution = (self.resolve)(&contest_id);
        let resolution = self.normalize_resolution(resolution);
        let modal = self.modal.as_mut().expect("modal must still exist");
        modal.mutation = None;
        Self::apply_resolution(modal, resolution);
    }

    fn apply_resolution(modal: &mut SwitchContestModal, resolution: ContestSwitchResolution) {
        modal.destination = resolution.destination;
        modal.error = resolution.error;
        modal.target = resolution.target;
    }

    fn refresh_resolution_for_confirmation(&mut self) -> bool {
        let Some(modal) = self.modal.as_ref() else {
            return false;
        };
        let contest_id = modal.contest_id.clone();
        let displayed_destination = modal.destination.clone();
        let displayed_error = modal.error.clone();
        let displayed_target = modal.target;
        let retrying = modal.state == SwitchContestModalState::Failed;

        let resolution = (self.resolve)(&contest_id);
        let resolution = self.normalize_resolution(resolution);
        let unchanged = displayed_destination == resolution.destination
            && displayed_target == resolution.target
            && (retrying || displayed_error == resolution.error);

        let modal = self.modal.as_mut().expect("modal must still exist");
        modal.state = SwitchContestModalState::Input;
        modal.progress.clear();
        modal.mutation = None;
        Self::apply_resolution(modal, resolution);
        unchanged
    }

    fn remove_last_grapheme(&mut self) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        if let Some((start, _)) = modal.contest_id.grapheme_indices(true).next_back() {
            modal.contest_id.truncate(start);
        }
    }

    fn resume_editing(&mut self) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        if modal.state == SwitchContestModalState::Failed {
            modal.state = SwitchContestModalState::Input;
            modal.error = None;
            modal.progress.clear();
            modal.mutation = None;
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ContestSwitchKeyResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return if self.modal_active() {
                ContestSwitchKeyResult::Handled
            } else {
                ContestSwitchKeyResult::NotHandled
            };
        }

        if self.modal_active() {
            if self
                .modal
                .as_ref()
                .is_some_and(|modal| modal.state.is_running())
            {
                return ContestSwitchKeyResult::Handled;
            }
            if self
                .modal
                .as_ref()
                .is_some_and(|modal| modal.state == SwitchContestModalState::Failed)
                && !matches!(
                    key.code,
                    KeyCode::Enter | KeyCode::Escape | KeyCode::Backspace | KeyCode::Char(_)
                )
            {
                return ContestSwitchKeyResult::Handled;
            }

            match key.code {
                KeyCode::Escape if key.kind == KeyEventKind::Press => {
                    self.modal = None;
                }
                KeyCode::Enter if key.kind == KeyEventKind::Press => {
                    if !self.refresh_resolution_for_confirmation() {
                        return ContestSwitchKeyResult::Handled;
                    }
                    let accepted_destination = self.modal.as_ref().and_then(|modal| {
                        modal
                            .target
                            .filter(|_| modal.error.is_none())
                            .and(modal.destination.as_deref())
                    });
                    if accepted_destination == Some(self.current_destination) {
                        self.modal = None;
                    } else if accepted_destination.is_some() {
                        match self.modal.as_ref().and_then(|modal| modal.target) {
                            Some(ContestSwitchTarget::Existing) => {
                                self.switch_requested = true;
                                return ContestSwitchKeyResult::SwitchRequested;
                            }
                            Some(ContestSwitchTarget::Missing) => {
                                let modal = self.modal.as_ref().expect("modal must exist");
                                let request = ContestSwitchRequest {
                                    mutation: ContestSwitchMutation::Create,
                                    contest_id: modal.contest_id.clone(),
                                    destination: modal
                                        .destination
                                        .clone()
                                        .expect("missing target must have a destination"),
                                };
                                match self.operation.start(request) {
                                    Ok(()) => {
                                        let modal = self.modal.as_mut().expect("modal must exist");
                                        modal.state = SwitchContestModalState::Creating;
                                        modal.error = None;
                                        modal.mutation = Some(ContestSwitchMutation::Create);
                                    }
                                    Err(error) => {
                                        let modal = self.modal.as_mut().expect("modal must exist");
                                        modal.state = SwitchContestModalState::Failed;
                                        modal.error = Some(error.to_string());
                                        modal.mutation = Some(ContestSwitchMutation::Create);
                                    }
                                }
                            }
                            Some(ContestSwitchTarget::RepairRequired) => {
                                let modal = self.modal.as_ref().expect("modal must exist");
                                let request = ContestSwitchRequest {
                                    mutation: ContestSwitchMutation::Repair,
                                    contest_id: modal.contest_id.clone(),
                                    destination: modal
                                        .destination
                                        .clone()
                                        .expect("repair target must have a destination"),
                                };
                                match self.operation.start(request) {
                                    Ok(()) => {
                                        let modal = self.modal.as_mut().expect("modal must exist");
                                        modal.state = SwitchContestModalState::Repairing;
                                        modal.error = None;
                                        modal.mutation = Some(ContestSwitchMutation::Repair);
                                    }
                                    Err(error) => {
                                        let modal = self.modal.as_mut().expect("modal must exist");
                                        modal.state = SwitchContestModalState::Failed;
                                        modal.error = Some(error.to_string());
                                        modal.mutation = Some(ContestSwitchMutation::Repair);
                                    }
                                }
                            }
                            None => {}
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.resume_editing();
                    self.remove_last_grapheme();
                    self.refresh_resolution();
                }
                KeyCode::Char(character)
                    if !key.modifiers.control && !key.modifiers.alt && !key.modifiers.super_key =>
                {
                    self.resume_editing();
                    if let Some(modal) = self.modal.as_mut() {
                        modal.contest_id.push(character);
                    }
                    self.refresh_resolution();
                }
                _ => {}
            }
            return ContestSwitchKeyResult::Handled;
        }

        if FrontendAction::from_shortcut(key) == Some(FrontendAction::SwitchContest) {
            if self.open() {
                ContestSwitchKeyResult::Handled
            } else {
                ContestSwitchKeyResult::NotHandled
            }
        } else {
            ContestSwitchKeyResult::NotHandled
        }
    }

    fn handle_operation_messages(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..MAX_MESSAGES_PER_TICK {
            match self.operation.try_recv() {
                Some(ContestSwitchOperationMessage::Progress(progress)) => {
                    if let Some(modal) = self.modal.as_mut()
                        && modal.state.is_running()
                    {
                        modal.progress.push(progress);
                        changed = true;
                    }
                }
                Some(ContestSwitchOperationMessage::Finished(Ok(()))) => {
                    self.switch_requested = true;
                    changed = true;
                    break;
                }
                Some(ContestSwitchOperationMessage::Finished(Err(error))) => {
                    if let Some(modal) = self.modal.as_mut() {
                        modal.state = SwitchContestModalState::Failed;
                        modal.error = Some(error);
                    }
                    changed = true;
                    break;
                }
                None => break,
            }
        }
        changed
    }
}

fn existing_destinations_have_same_identity(left: &Path, right: &Path) -> io::Result<bool> {
    if left == right {
        return Ok(true);
    }

    let left = std::fs::canonicalize(left).map_err(|error| {
        let kind = error.kind();
        io::Error::new(
            kind,
            format!(
                "failed to resolve repair target {}: {error}",
                left.display()
            ),
        )
    })?;
    let right = std::fs::canonicalize(right).map_err(|error| {
        let kind = error.kind();
        io::Error::new(
            kind,
            format!(
                "failed to resolve active contest {}: {error}",
                right.display()
            ),
        )
    })?;
    Ok(left == right)
}

#[derive(Clone, Copy)]
pub(crate) struct StressSetupContext<'a> {
    destination: &'a Path,
    contest: &'a Contest,
}

impl<'a> StressSetupContext<'a> {
    pub(crate) fn new(destination: &'a Path, contest: &'a Contest) -> Self {
        Self {
            destination,
            contest,
        }
    }
}

pub(crate) struct SessionChannels<'a> {
    message_rx: &'a Receiver<Message>,
    run_tx: &'a Sender<RunWorkerCommand>,
    detail_analysis_tx: &'a Sender<DetailAnalysisCommand>,
    detail_analysis_rx: &'a Receiver<DetailAnalysisResult>,
}

impl<'a> SessionChannels<'a> {
    pub(crate) fn new(
        message_rx: &'a Receiver<Message>,
        run_tx: &'a Sender<RunWorkerCommand>,
        detail_analysis_tx: &'a Sender<DetailAnalysisCommand>,
        detail_analysis_rx: &'a Receiver<DetailAnalysisResult>,
    ) -> Self {
        Self {
            message_rx,
            run_tx,
            detail_analysis_tx,
            detail_analysis_rx,
        }
    }
}

pub(crate) struct SessionRuntime<'a> {
    current_destination: &'a Path,
    config: &'a Config,
    stress_setup: StressSetupContext<'a>,
    sample_counts: Vec<usize>,
    stress_cases: Vec<Option<crate::model::Sample>>,
    channels: SessionChannels<'a>,
}

impl<'a> SessionRuntime<'a> {
    pub(crate) fn new(
        current_destination: &'a Path,
        config: &'a Config,
        contest: &'a Contest,
        sample_counts: Vec<usize>,
        stress_cases: Vec<Option<crate::model::Sample>>,
        channels: SessionChannels<'a>,
    ) -> Self {
        Self {
            current_destination,
            config,
            stress_setup: StressSetupContext::new(current_destination, contest),
            sample_counts,
            stress_cases,
            channels,
        }
    }
}

#[derive(Clone, Copy)]
struct TerminalInputContext<'a> {
    run_tx: &'a Sender<RunWorkerCommand>,
    stress_setup: Option<StressSetupContext<'a>>,
}

impl<'a> TerminalInputContext<'a> {
    fn new(
        run_tx: &'a Sender<RunWorkerCommand>,
        stress_setup: Option<StressSetupContext<'a>>,
    ) -> Self {
        Self {
            run_tx,
            stress_setup,
        }
    }
}

struct FrontendInputContext<'run, 'controller, 'resolver, 'palette> {
    terminal: TerminalInputContext<'run>,
    contest_switch: Option<&'controller mut ContestSwitchController<'resolver>>,
    command_palette: Option<&'palette mut CommandPalette>,
    open_source: Option<OpenSourceInputContext<'palette>>,
}

struct OpenSourceInputContext<'a> {
    controller: &'a mut OpenSourceController,
    editor: &'a mut dyn SourceEditorHost,
    creator: &'a mut dyn SourceCreator,
}

struct StressInitializationReporter;

impl Reporter for StressInitializationReporter {
    fn report(&mut self, event: Event<'_>) {
        match event {
            Event::StressFileCreated { .. }
            | Event::StressFileExists { .. }
            | Event::StressFilesAlreadyInitialized { .. } => {}
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DetailScrollbarDrag {
    identity: DetailScrollbarStableIdentity,
    coordinate: DragCoordinate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragCoordinate {
    Cells {
        grab_offset: u16,
    },
    Pixels {
        grab_offset_px: u64,
        generation: u64,
    },
}

#[derive(Debug, Default)]
struct DetailScrollbarDragState {
    active: Option<DetailScrollbarDrag>,
}

impl DetailScrollbarDragState {
    fn cancel(&mut self) {
        self.active = None;
    }

    fn reconcile_render_info(&mut self, render_info: &view::RenderInfo) {
        if self.active.is_some_and(|drag| {
            render_info
                .detail_scrollbar
                .as_ref()
                .is_none_or(|scrollbar| scrollbar.identity != drag.identity)
        }) {
            self.cancel();
        }
    }
}

fn send_run_worker_command(
    run_tx: &Sender<RunWorkerCommand>,
    command: RunWorkerCommand,
) -> io::Result<()> {
    run_tx.send(command).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "run worker request channel disconnected",
        )
    })
}

fn send_run_request(run_tx: &Sender<RunWorkerCommand>, request: RunRequest) -> io::Result<()> {
    send_run_worker_command(run_tx, RunWorkerCommand::Run(request))
}

fn queue_problem_run(
    app: &mut WatchApp,
    problem: usize,
    run_tx: &Sender<RunWorkerCommand>,
) -> io::Result<bool> {
    let Some(request) = app.queue_run(problem) else {
        return Ok(false);
    };

    send_run_request(run_tx, request)?;

    Ok(true)
}

fn queue_problem_stress(
    app: &mut WatchApp,
    problem: usize,
    run_tx: &Sender<RunWorkerCommand>,
    setup: StressSetupContext<'_>,
) -> io::Result<bool> {
    queue_problem_stress_with_seed(app, problem, run_tx, setup, crate::stress::automatic_seed)
}

fn queue_problem_stress_with_seed(
    app: &mut WatchApp,
    problem: usize,
    run_tx: &Sender<RunWorkerCommand>,
    setup: StressSetupContext<'_>,
    automatic_seed: impl FnOnce() -> io::Result<u64>,
) -> io::Result<bool> {
    let Some(canonical_problem) = setup.contest.problems.get(problem) else {
        return Ok(app.set_stress_setup_error(
            problem,
            "selected problem is missing from contest metadata".to_string(),
        ));
    };

    let status = match crate::commands::stress::inspect_stress_files_at(
        setup.destination,
        canonical_problem,
    ) {
        Ok(status) => status,
        Err(error) => return Ok(app.set_stress_setup_error(problem, error.to_string())),
    };

    if !status.is_ready() {
        return Ok(app.set_stress_setup_required(
            problem,
            status.generator_missing,
            status.brute_missing,
        ));
    }

    let setup_cleared = app.clear_stress_setup(problem);
    let base_seed = automatic_seed()?;
    let Some(request) = app.queue_stress(problem, base_seed) else {
        return Ok(setup_cleared);
    };

    send_run_request(run_tx, request)?;

    Ok(true)
}

fn initialize_problem_stress(
    app: &mut WatchApp,
    problem: usize,
    setup: StressSetupContext<'_>,
) -> bool {
    if !app.stress_setup_required(problem) {
        return false;
    }

    let Some(canonical_problem) = setup.contest.problems.get(problem) else {
        return app.set_stress_setup_error(
            problem,
            "selected problem is missing from contest metadata".to_string(),
        );
    };

    let mut reporter = StressInitializationReporter;
    match crate::commands::stress::initialize_stress_files_at(
        setup.destination,
        canonical_problem,
        &mut reporter,
    ) {
        Ok(()) => app.set_stress_setup_initialized(problem),
        Err(error) => app.set_stress_setup_error(problem, error.to_string()),
    }
}

fn handle_messages(
    app: &mut WatchApp,
    message_rx: &Receiver<Message>,
    run_tx: &Sender<RunWorkerCommand>,
) -> io::Result<bool> {
    let mut changed = false;

    for _ in 0..MAX_MESSAGES_PER_TICK {
        match message_rx.try_recv() {
            Ok(Message::SourceChanged {
                problem,
                path,
                language,
            }) => {
                if app.source_changed(problem, path, language) {
                    changed = true;
                    queue_problem_run(app, problem, run_tx)?;
                }
            }

            Ok(Message::WatcherFailed(error)) => {
                return Err(error);
            }

            Ok(Message::WorkerFailed(error)) => {
                return Err(error);
            }

            Err(TryRecvError::Empty) => {
                break;
            }

            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "background message channel disconnected",
                ));
            }
            Ok(Message::RunStarted { run_id, problem }) => {
                if app.run_started(problem, run_id) {
                    changed = true;
                }
            }

            Ok(Message::RunRequeued { run_id, problem }) => {
                if app.run_requeued(problem, run_id) {
                    changed = true;
                }
            }

            Ok(Message::RunEvent {
                run_id,
                problem,
                event,
            }) => {
                if app.run_event(problem, run_id, event) {
                    changed = true;
                }
            }

            Ok(Message::StressEvent {
                run_id,
                problem,
                event,
            }) => {
                if app.stress_event(problem, run_id, event) {
                    changed = true;
                }
            }

            Ok(Message::RunCompleted { run_id, problem }) => {
                if app.run_completed(problem, run_id) {
                    changed = true;
                }
            }

            Ok(Message::RunFailed {
                run_id,
                problem,
                error,
            }) => {
                if app.run_failed(problem, run_id, error) {
                    changed = true;
                }
            }
        }
    }

    Ok(changed)
}

fn handle_detail_analysis_results(
    detail_layout: &mut detail_layout::DetailLayout,
    current_detail_revision: u64,
    result_rx: &Receiver<DetailAnalysisResult>,
) -> io::Result<bool> {
    let mut changed = false;

    for _ in 0..MAX_DETAIL_ANALYSIS_RESULTS_PER_TICK {
        match result_rx.try_recv() {
            Ok(result) => {
                let result_revision = match &result {
                    DetailAnalysisResult::StructureReady(result) => result.identity.revision,
                    DetailAnalysisResult::Count(result) => result.identity.revision,
                };
                if result_revision == current_detail_revision {
                    changed |= detail_layout.apply_analysis_result(result);
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "detail analysis worker result channel disconnected",
                ));
            }
        }
    }

    Ok(changed)
}

fn apply_detail_scroll_reconciliation(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
) -> bool {
    detail_layout
        .take_scroll_reconciliation()
        .is_some_and(|absolute_row| app.reconcile_detail_scroll(absolute_row))
}

fn send_detail_analysis_command(
    command_tx: &Sender<DetailAnalysisCommand>,
    command: DetailAnalysisCommand,
) -> io::Result<()> {
    command_tx.send(command).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "detail analysis worker request channel disconnected",
        )
    })
}

pub(crate) fn run(
    terminal: &mut TerminaSession,
    app_context: &AppContext,
    preferences: &mut FrontendPreferences,
    runtime: SessionRuntime<'_>,
    mut resolve_contest_switch: impl FnMut(&str) -> ContestSwitchResolution,
    contest_switch_task: ContestSwitchTask,
) -> io::Result<SessionExit> {
    let SessionRuntime {
        current_destination,
        config,
        stress_setup,
        sample_counts,
        stress_cases,
        channels:
            SessionChannels {
                message_rx,
                run_tx,
                detail_analysis_tx,
                detail_analysis_rx,
            },
    } = runtime;
    let mut app =
        WatchApp::new_with_stress_cases(stress_setup.contest, sample_counts, stress_cases)?;
    preferences.apply(&mut app);
    let mut contest_switch = ContestSwitchController::new(
        app_context,
        current_destination,
        &mut resolve_contest_switch,
        contest_switch_task,
    );
    let mut command_palette = CommandPalette::default();
    let mut open_source = OpenSourceController::new(current_destination, config.defaults.language);

    let mut dirty = true;

    let mut render_info = view::RenderInfo::default();
    let mut detail_layout = detail_layout::DetailLayout::default();
    let mut detail_scrollbar_drag = DetailScrollbarDragState::default();
    let mut terminal_events = VecDeque::new();

    while !app.should_quit() {
        if terminal_events.is_empty() {
            terminal_events = read_terminal_events(terminal, Duration::ZERO)?;
        }

        if contains_global_quit_event(
            &terminal_events,
            &app,
            &contest_switch,
            &command_palette,
            &open_source,
        ) {
            app.quit();
            break;
        }

        if take_leading_resizes(&mut terminal_events) {
            detail_scrollbar_drag.cancel();
            discard_stale_pixel_events(&mut terminal_events);
            terminal.note_resize_dispatched();
            dirty = true;
        }

        if handle_messages(&mut app, message_rx, run_tx)? {
            dirty = true;
        }

        if contest_switch.handle_operation_messages() {
            dirty = true;
        }
        if contest_switch.switch_requested {
            break;
        }

        if handle_detail_analysis_results(
            &mut detail_layout,
            app.detail_revision(),
            detail_analysis_rx,
        )? {
            dirty = true;
        }
        if apply_detail_scroll_reconciliation(&mut app, &mut detail_layout) {
            dirty = true;
        }

        // message batch処理中にqが到着していれば、重いwrap/再描画より優先する。
        if terminal_events.is_empty() {
            terminal_events = read_terminal_events(terminal, Duration::ZERO)?;
        }

        if contains_global_quit_event(
            &terminal_events,
            &app,
            &contest_switch,
            &command_palette,
            &open_source,
        ) {
            app.quit();
            break;
        }

        if take_leading_resizes(&mut terminal_events) {
            detail_scrollbar_drag.cancel();
            discard_stale_pixel_events(&mut terminal_events);
            terminal.note_resize_dispatched();
            dirty = true;
        }

        if dirty {
            let mut next_render_info = view::RenderInfo::default();
            let render_mouse_mode = terminal.mouse_mode();

            terminal.draw(|frame| {
                next_render_info = view::render_frontend_with_mouse_mode(
                    frame,
                    &app,
                    &mut detail_layout,
                    render_mouse_mode,
                    contest_switch.available,
                    view::FrontendOverlays {
                        switch_modal: contest_switch.modal(),
                        source_modal: open_source.modal(),
                        command_palette: command_palette.is_active().then_some(&command_palette),
                    },
                );
            })?;

            render_info = next_render_info;
            detail_scrollbar_drag.reconcile_render_info(&render_info);

            apply_detail_scroll_reconciliation(&mut app, &mut detail_layout);

            if let Some(max_detail_scroll) = render_info.max_detail_scroll {
                app.clamp_detail_scroll(max_detail_scroll);
            }

            if let Some(command) = detail_layout.take_analysis_command() {
                send_detail_analysis_command(detail_analysis_tx, command)?;
            }

            dirty = false;
            terminal.note_redraw_completed();
            let resize_pending = resize_event_count(&terminal_events) != 0;
            terminal.refresh_mouse_after_redraw(resize_pending)?;
            terminal.retry_high_res_after_redraw(resize_pending)?;
            if terminal.mouse_mode() != render_mouse_mode {
                // Pixel metrics become authoritative only after the settled resize/redraw
                // boundary. Draw once more so the published thumb geometry uses that mode.
                dirty = true;
                continue;
            }
        }

        if terminal_events.is_empty() {
            terminal_events = read_terminal_events(terminal, TERMINAL_POLL_INTERVAL)?;
        }

        // qは同じbatch内のresize/mouseより先に扱い、再描画を挟まず終了する。
        if contains_global_quit_event(
            &terminal_events,
            &app,
            &contest_switch,
            &command_palette,
            &open_source,
        ) {
            app.quit();
            continue;
        }

        let resize_count_before = resize_event_count(&terminal_events);
        let mouse_mode = terminal.mouse_mode();
        let mut editor = LiveSourceEditorHost { terminal, config };
        let mut creator = DefaultSourceCreator;
        if handle_terminal_events_with_mouse_mode(
            &mut app,
            &mut detail_layout,
            &mut detail_scrollbar_drag,
            &render_info,
            &mut terminal_events,
            mouse_mode,
            FrontendInputContext {
                terminal: TerminalInputContext::new(run_tx, Some(stress_setup)),
                contest_switch: Some(&mut contest_switch),
                command_palette: Some(&mut command_palette),
                open_source: Some(OpenSourceInputContext {
                    controller: &mut open_source,
                    editor: &mut editor,
                    creator: &mut creator,
                }),
            },
        )? {
            dirty = true;
        }
        if contest_switch.switch_requested {
            break;
        }
        if resize_event_count(&terminal_events) < resize_count_before {
            discard_stale_pixel_events(&mut terminal_events);
            terminal.note_resize_dispatched();
        }
    }

    preferences.capture(&app);
    if contest_switch.switch_requested {
        Ok(SessionExit::SwitchContest)
    } else {
        Ok(SessionExit::Quit)
    }
}

fn read_terminal_events(
    terminal: &mut TerminaSession,
    wait: Duration,
) -> io::Result<VecDeque<TerminalEvent>> {
    let mut events = VecDeque::new();

    if !terminal.poll(wait)? {
        return Ok(events);
    }

    for index in 0..MAX_TERMINAL_EVENTS_PER_TICK {
        let terminal_event = terminal.read()?;
        let should_quit = is_quit_event(&terminal_event);
        events.push_back(terminal_event);

        if should_quit
            || index + 1 == MAX_TERMINAL_EVENTS_PER_TICK
            || !terminal.poll(Duration::ZERO)?
        {
            break;
        }
    }

    Ok(events)
}

fn resize_event_count(events: &VecDeque<TerminalEvent>) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, TerminalEvent::Resize(_)))
        .count()
}

fn discard_stale_pixel_events(events: &mut VecDeque<TerminalEvent>) {
    events.retain(|event| {
        !matches!(
            event,
            TerminalEvent::Pointer(PointerEvent {
                position: terminal::PointerPosition::AbsolutePixels { .. },
                ..
            })
        )
    });
}

#[cfg(test)]
fn read_terminal_events_with(
    wait: Duration,
    mut poll_event: impl FnMut(Duration) -> io::Result<bool>,
    mut read_event: impl FnMut() -> io::Result<TerminalEvent>,
) -> io::Result<VecDeque<TerminalEvent>> {
    let mut events = VecDeque::new();

    if !poll_event(wait)? {
        return Ok(events);
    }

    for index in 0..MAX_TERMINAL_EVENTS_PER_TICK {
        let terminal_event = read_event()?;
        let should_quit = is_quit_event(&terminal_event);
        events.push_back(terminal_event);

        if should_quit || index + 1 == MAX_TERMINAL_EVENTS_PER_TICK || !poll_event(Duration::ZERO)?
        {
            break;
        }
    }

    Ok(events)
}

#[cfg(test)]
fn contains_quit_event(events: &VecDeque<TerminalEvent>) -> bool {
    events.iter().any(is_quit_event)
}

fn contains_global_quit_event(
    events: &VecDeque<TerminalEvent>,
    app: &WatchApp,
    contest_switch: &ContestSwitchController<'_>,
    command_palette: &CommandPalette,
    open_source: &OpenSourceController,
) -> bool {
    let mut contest_modal_active = contest_switch.modal_active();
    let mut source_modal_active = open_source.modal_active();
    let mut command_palette = command_palette.clone();

    for event in events {
        let TerminalEvent::Key(key) = event else {
            continue;
        };

        if contest_modal_active {
            if key.kind == KeyEventKind::Press
                && key.code == KeyCode::Escape
                && contest_switch.escape_dismisses_modal()
            {
                contest_modal_active = false;
            }
            continue;
        }

        if source_modal_active {
            if source_modal_escape_closes(*key) {
                source_modal_active = false;
            }
            continue;
        }

        if command_palette.is_active() {
            if let CommandPaletteKeyResult::ExecuteRequested(action) =
                command_palette.handle_key(*key)
                && action
                    .availability(app, contest_switch.available)
                    .is_available()
            {
                command_palette.close();
                match action {
                    FrontendAction::SwitchContest => contest_modal_active = true,
                    FrontendAction::OpenSource => source_modal_active = true,
                    FrontendAction::RunTests
                    | FrontendAction::ToggleDebug
                    | FrontendAction::ToggleSamples
                    | FrontendAction::StartStress
                    | FrontendAction::StopStress
                    | FrontendAction::InitializeStress => {}
                }
            }
            continue;
        }

        if key.kind != KeyEventKind::Press {
            continue;
        }

        if is_command_palette_open_key(*key) {
            command_palette.open();
            continue;
        }

        if FrontendAction::from_shortcut(*key) == Some(FrontendAction::SwitchContest)
            && contest_switch.available
        {
            contest_modal_active = true;
            continue;
        }

        if is_quit_event(event) {
            return true;
        }
    }

    false
}

fn is_command_palette_open_key(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char(':')
        && !key.modifiers.control
        && !key.modifiers.alt
        && !key.modifiers.super_key
}

fn is_quit_event(terminal_event: &TerminalEvent) -> bool {
    matches!(
        terminal_event,
        TerminalEvent::Key(KeyEvent {
            code: KeyCode::Char('q'),
            kind: KeyEventKind::Press,
            ..
        })
    )
}

fn take_leading_resizes(events: &mut VecDeque<TerminalEvent>) -> bool {
    let mut found = false;

    while matches!(events.front(), Some(TerminalEvent::Resize(_))) {
        events.pop_front();
        found = true;
    }

    found
}

#[cfg(test)]
fn handle_terminal_events(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    render_info: &view::RenderInfo,
    events: &mut VecDeque<TerminalEvent>,
    run_tx: &Sender<RunWorkerCommand>,
) -> io::Result<bool> {
    handle_terminal_events_with_mouse_mode(
        app,
        detail_layout,
        detail_scrollbar_drag,
        render_info,
        events,
        MouseMode::Cells,
        FrontendInputContext {
            terminal: TerminalInputContext::new(run_tx, None),
            contest_switch: None,
            command_palette: None,
            open_source: None,
        },
    )
}

fn handle_terminal_events_with_mouse_mode(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    render_info: &view::RenderInfo,
    events: &mut VecDeque<TerminalEvent>,
    mouse_mode: MouseMode,
    mut input: FrontendInputContext<'_, '_, '_, '_>,
) -> io::Result<bool> {
    let mut changed = false;
    let mut scrollbar_geometry_changed_by_drag = false;

    while let Some(terminal_event) = events.front() {
        if scrollbar_geometry_changed_by_drag
            && matches!(
                terminal_event,
                TerminalEvent::Pointer(PointerEvent {
                    kind: PointerKind::Down(_) | PointerKind::ScrollUp | PointerKind::ScrollDown,
                    ..
                })
            )
        {
            break;
        }

        let terminal_event = events
            .pop_front()
            .expect("front terminal event must still exist");
        if matches!(terminal_event, TerminalEvent::Resize(_)) {
            detail_scrollbar_drag.cancel();
            changed = true;

            // 連続resizeは1回の再描画へまとめる。後続mouseは新しいRectが
            // 描画されてから処理し、古いRenderInfoでhit testしない。
            while matches!(events.front(), Some(TerminalEvent::Resize(_))) {
                events.pop_front();
            }
            break;
        }

        let detail_revision_before = app.detail_revision();
        let samples_pane_before = app.samples_pane_enabled();
        let detail_scroll_before = app.detail_scroll();
        let is_left_drag = matches!(
            terminal_event,
            TerminalEvent::Pointer(PointerEvent {
                kind: PointerKind::Drag(PointerButton::Left),
                ..
            })
        );
        changed |= handle_terminal_event_with_mouse_mode(
            app,
            detail_layout,
            detail_scrollbar_drag,
            terminal_event,
            render_info,
            mouse_mode,
            &mut input,
        )?;
        if input
            .open_source
            .as_mut()
            .is_some_and(|source| source.controller.take_discard_input_batch())
        {
            events.clear();
        }
        if input
            .contest_switch
            .as_ref()
            .is_some_and(|contest_switch| contest_switch.switch_requested)
        {
            break;
        }
        if app.detail_revision() != detail_revision_before
            || app.samples_pane_enabled() != samples_pane_before
        {
            // The remaining queued pointer events must see geometry rendered
            // for the new document/mode/pane layout. Pure drag bursts do not
            // change either stable identity input and continue to batch.
            break;
        }
        if app.detail_scroll() != detail_scroll_before {
            if is_left_drag {
                // Absolute drag mapping depends only on stable track geometry
                // and remains valid throughout one delivered drag burst.
                scrollbar_geometry_changed_by_drag = true;
            } else {
                // A wheel/seek/cap action changes the rendered thumb. Leave
                // later pointer events queued until that geometry is redrawn.
                break;
            }
        }
    }

    Ok(changed)
}

fn handle_terminal_event_with_mouse_mode(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    terminal_event: TerminalEvent,
    render_info: &view::RenderInfo,
    mouse_mode: MouseMode,
    input: &mut FrontendInputContext<'_, '_, '_, '_>,
) -> io::Result<bool> {
    if let TerminalEvent::Key(key) = terminal_event
        && let Some(contest_switch) = input.contest_switch.as_deref_mut()
        && contest_switch.modal_active()
    {
        match contest_switch.handle_key(key) {
            ContestSwitchKeyResult::NotHandled => {}
            ContestSwitchKeyResult::Handled | ContestSwitchKeyResult::SwitchRequested => {
                return Ok(true);
            }
        }
    }

    if let TerminalEvent::Key(key) = terminal_event
        && input
            .open_source
            .as_ref()
            .is_some_and(|source| source.controller.modal_active())
    {
        let source = input
            .open_source
            .as_mut()
            .expect("active Open Source controller must exist");
        return source
            .controller
            .handle_key(app, key, source.editor, source.creator);
    }

    if let TerminalEvent::Key(key) = terminal_event
        && input
            .command_palette
            .as_ref()
            .is_some_and(|palette| palette.is_active())
    {
        let result = input
            .command_palette
            .as_deref_mut()
            .expect("active palette must exist")
            .handle_key(key);
        match result {
            CommandPaletteKeyResult::NotHandled => {}
            CommandPaletteKeyResult::Handled(changed) => return Ok(changed),
            CommandPaletteKeyResult::ExecuteRequested(action) => {
                let contest_switch_available = input
                    .contest_switch
                    .as_ref()
                    .is_some_and(|controller| controller.available);
                if !action
                    .availability(app, contest_switch_available)
                    .is_available()
                {
                    return Ok(false);
                }

                input
                    .command_palette
                    .as_deref_mut()
                    .expect("active palette must exist")
                    .close();
                return execute_frontend_action(
                    app,
                    action,
                    input.terminal,
                    input.contest_switch.as_deref_mut(),
                    input
                        .open_source
                        .as_mut()
                        .map(|source| &mut *source.controller),
                );
            }
        }
    }

    let frontend_interaction_active = input
        .contest_switch
        .as_ref()
        .is_some_and(|contest_switch| contest_switch.modal_active())
        || input
            .command_palette
            .as_ref()
            .is_some_and(|palette| palette.is_active())
        || input
            .open_source
            .as_ref()
            .is_some_and(|source| source.controller.modal_active());
    if frontend_interaction_active && matches!(terminal_event, TerminalEvent::Pointer(_)) {
        return Ok(false);
    }

    match terminal_event {
        TerminalEvent::Key(key) => {
            let palette_was_active = input
                .command_palette
                .as_ref()
                .is_some_and(|palette| palette.is_active());
            let changed = handle_key_event_with_frontend_context(app, key, input)?;
            if !palette_was_active
                && input
                    .command_palette
                    .as_ref()
                    .is_some_and(|palette| palette.is_active())
            {
                detail_scrollbar_drag.cancel();
            }
            Ok(changed)
        }

        TerminalEvent::Pointer(pointer) => Ok(handle_pointer_event_with_mouse_mode(
            app,
            detail_layout,
            detail_scrollbar_drag,
            pointer,
            render_info,
            mouse_mode,
        )),

        TerminalEvent::Resize(_) => {
            detail_scrollbar_drag.cancel();
            Ok(true)
        }

        TerminalEvent::Ignored => Ok(false),
    }
}

#[cfg(test)]
fn handle_key_event(
    app: &mut WatchApp,
    key: KeyEvent,
    run_tx: &Sender<RunWorkerCommand>,
) -> io::Result<bool> {
    handle_key_event_with_stress_context(app, key, run_tx, None)
}

#[cfg(test)]
fn handle_key_event_with_stress_context(
    app: &mut WatchApp,
    key: KeyEvent,
    run_tx: &Sender<RunWorkerCommand>,
    stress_setup: Option<StressSetupContext<'_>>,
) -> io::Result<bool> {
    handle_key_event_with_frontend_context(
        app,
        key,
        &mut FrontendInputContext {
            terminal: TerminalInputContext::new(run_tx, stress_setup),
            contest_switch: None,
            command_palette: None,
            open_source: None,
        },
    )
}

fn handle_key_event_with_frontend_context(
    app: &mut WatchApp,
    key: KeyEvent,
    input: &mut FrontendInputContext<'_, '_, '_, '_>,
) -> io::Result<bool> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(false);
    }

    if key.code == KeyCode::Char('q') && key.kind == KeyEventKind::Press {
        app.quit();
        return Ok(true);
    }

    if is_command_palette_open_key(key) {
        let Some(command_palette) = input.command_palette.as_deref_mut() else {
            return Ok(false);
        };
        command_palette.open();
        return Ok(true);
    }

    if let Some(action) = FrontendAction::from_shortcut(key) {
        return execute_frontend_action(
            app,
            action,
            input.terminal,
            input.contest_switch.as_deref_mut(),
            input
                .open_source
                .as_mut()
                .map(|source| &mut *source.controller),
        );
    }

    match key.code {
        KeyCode::Char('h') | KeyCode::Left => Ok(app.previous_problem()),
        KeyCode::Char('l') | KeyCode::Right => Ok(app.next_problem()),
        KeyCode::Char('j') | KeyCode::Down => Ok(app.next_case()),
        KeyCode::Char('k') | KeyCode::Up => Ok(app.previous_case()),
        _ => Ok(false),
    }
}

fn execute_frontend_action(
    app: &mut WatchApp,
    action: FrontendAction,
    input: TerminalInputContext<'_>,
    contest_switch: Option<&mut ContestSwitchController<'_>>,
    open_source: Option<&mut OpenSourceController>,
) -> io::Result<bool> {
    match action {
        FrontendAction::RunTests => {
            let Some(problem) = app.selected_problem() else {
                return Ok(false);
            };
            queue_problem_run(app, problem, input.run_tx)
        }
        FrontendAction::OpenSource => {
            Ok(open_source.is_some_and(|controller| controller.open(app)))
        }
        FrontendAction::ToggleDebug => {
            app.toggle_debug();
            if app.current_source_language() == Some(crate::language::Language::Cpp)
                && let Some(problem) = app.selected_problem()
            {
                queue_problem_run(app, problem, input.run_tx)?;
            }
            Ok(true)
        }
        FrontendAction::ToggleSamples => {
            app.toggle_samples_pane();
            Ok(true)
        }
        FrontendAction::StartStress => {
            let Some(problem) = app.selected_problem() else {
                return Ok(false);
            };
            let Some(stress_setup) = input.stress_setup else {
                return Ok(false);
            };
            queue_problem_stress(app, problem, input.run_tx, stress_setup)
        }
        FrontendAction::StopStress => {
            let Some((problem, run_id)) = app.active_stress_identity() else {
                return Ok(false);
            };
            send_run_worker_command(
                input.run_tx,
                RunWorkerCommand::CancelStress { problem, run_id },
            )?;
            Ok(app.cancel_stress(problem, run_id))
        }
        FrontendAction::InitializeStress => {
            let Some(problem) = app.selected_problem() else {
                return Ok(false);
            };
            let Some(stress_setup) = input.stress_setup else {
                return Ok(false);
            };
            Ok(initialize_problem_stress(app, problem, stress_setup))
        }
        FrontendAction::SwitchContest => {
            Ok(contest_switch.is_some_and(|controller| controller.open()))
        }
    }
}

fn contains(area: ratatui::layout::Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

#[cfg(test)]
fn handle_pointer_event(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    pointer: PointerEvent,
    render_info: &view::RenderInfo,
) -> bool {
    handle_pointer_event_with_mouse_mode(
        app,
        detail_layout,
        detail_scrollbar_drag,
        pointer,
        render_info,
        MouseMode::Cells,
    )
}

fn projected_pixel_pointer(
    pointer: PointerEvent,
    mouse_mode: MouseMode,
) -> Option<(u32, u32, TerminalPixelMetrics, u64)> {
    let terminal::PointerPosition::AbsolutePixels { x, y } = pointer.position else {
        return None;
    };
    let MouseMode::Pixels {
        metrics,
        origin,
        generation,
    } = mouse_mode
    else {
        return None;
    };
    if pointer.pixel_generation != Some(generation) {
        return None;
    }
    let (x, y) = normalize_absolute_pixels(metrics, origin, x, y)?;
    Some((x, y, metrics, generation))
}

fn project_pointer_to_cells(pointer: PointerEvent, mouse_mode: MouseMode) -> Option<(u16, u16)> {
    match (pointer.position, mouse_mode) {
        (terminal::PointerPosition::Cells { column, row }, MouseMode::Cells) => Some((column, row)),
        (
            terminal::PointerPosition::AbsolutePixels { x, y },
            MouseMode::Pixels {
                metrics,
                origin,
                generation,
            },
        ) if pointer.pixel_generation == Some(generation) => {
            project_absolute_pixels_to_cells(metrics, origin, x, y)
        }
        _ => None,
    }
}

fn handle_pointer_event_with_mouse_mode(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    detail_scrollbar_drag: &mut DetailScrollbarDragState,
    pointer: PointerEvent,
    render_info: &view::RenderInfo,
    mouse_mode: MouseMode,
) -> bool {
    if matches!(pointer.kind, PointerKind::Up(_)) {
        detail_scrollbar_drag.cancel();
        return false;
    }

    let Some((column, row)) = project_pointer_to_cells(pointer, mouse_mode) else {
        return false;
    };

    if matches!(pointer.kind, PointerKind::Down(_)) {
        // Every new press terminates a previous interaction before the new hit
        // target is interpreted.
        detail_scrollbar_drag.cancel();
    }

    if let PointerKind::Drag(PointerButton::Left) = pointer.kind {
        let Some(drag) = detail_scrollbar_drag.active else {
            return false;
        };
        let Some(scrollbar) = render_info.detail_scrollbar.as_ref() else {
            detail_scrollbar_drag.cancel();
            return false;
        };
        if scrollbar.identity != drag.identity
            || scrollbar.identity.layout.revision != app.detail_revision()
        {
            detail_scrollbar_drag.cancel();
            return false;
        }

        let target = match drag.coordinate {
            DragCoordinate::Cells { grab_offset }
                if matches!(pointer.position, terminal::PointerPosition::Cells { .. }) =>
            {
                scrollbar.geometry.scroll_for_drag(row, grab_offset)
            }
            DragCoordinate::Pixels {
                grab_offset_px,
                generation,
            } => {
                let Some((_, normalized_y, metrics, event_generation)) =
                    projected_pixel_pointer(pointer, mouse_mode)
                else {
                    return false;
                };
                if generation != event_generation {
                    return false;
                }
                scrollbar.geometry.scroll_for_pixel_drag(
                    normalized_y,
                    grab_offset_px,
                    metrics.cell_height_px,
                )
            }
            _ => return false,
        };
        return set_detail_scroll_from_user(
            app,
            detail_layout,
            target,
            Some(scrollbar.geometry.max_scroll),
        );
    }

    if let Some(samples_area) = render_info.samples_area
        && contains(samples_area, column, row)
    {
        return match pointer.kind {
            PointerKind::ScrollUp => app.previous_case(),

            PointerKind::ScrollDown => app.next_case(),

            _ => false,
        };
    }

    if let PointerKind::Down(PointerButton::Left) = pointer.kind
        && let Some(scrollbar) = render_info.detail_scrollbar.as_ref()
        && scrollbar.identity.layout.revision == app.detail_revision()
    {
        let projected_pixel = projected_pixel_pointer(pointer, mouse_mode);
        let hit = match projected_pixel {
            Some((_, normalized_y, metrics, generation)) => scrollbar.hit_test_pixels(
                column,
                row,
                normalized_y,
                metrics.cell_height_px,
                generation,
            ),
            None => scrollbar.geometry.hit_test(column, row),
        };
        if let Some(hit) = hit {
            return match hit {
                DetailScrollbarHit::Thumb { grab_offset } => {
                    let coordinate = match projected_pixel {
                        Some((_, normalized_y, metrics, generation)) => {
                            let Some(grab_offset_px) = scrollbar.pixel_grab_offset(
                                normalized_y,
                                metrics.cell_height_px,
                                generation,
                            ) else {
                                return false;
                            };
                            DragCoordinate::Pixels {
                                grab_offset_px,
                                generation,
                            }
                        }
                        None => DragCoordinate::Cells { grab_offset },
                    };
                    detail_scrollbar_drag.active = Some(DetailScrollbarDrag {
                        identity: scrollbar.identity,
                        coordinate,
                    });
                    false
                }
                DetailScrollbarHit::TopCap => set_detail_scroll_from_user(
                    app,
                    detail_layout,
                    0,
                    Some(scrollbar.geometry.max_scroll),
                ),
                DetailScrollbarHit::BottomCap => set_detail_scroll_from_user(
                    app,
                    detail_layout,
                    scrollbar.geometry.max_scroll,
                    Some(scrollbar.geometry.max_scroll),
                ),
                DetailScrollbarHit::Track => set_detail_scroll_from_user(
                    app,
                    detail_layout,
                    scrollbar.geometry.scroll_for_track_click(row),
                    Some(scrollbar.geometry.max_scroll),
                ),
            };
        }
    }

    if let PointerKind::Down(PointerButton::Left) = pointer.kind
        && let Some(header) = render_info.detail_section_headers.iter().find(|header| {
            header.detail_revision == app.detail_revision() && contains(header.area, column, row)
        })
    {
        app.toggle_detail_section(header.kind);
        return true;
    }

    if contains(render_info.detail_area, column, row) {
        return match pointer.kind {
            PointerKind::ScrollUp => {
                let target = app.detail_scroll().saturating_sub(DETAIL_SCROLL_LINES);
                set_detail_scroll_from_user(
                    app,
                    detail_layout,
                    target,
                    render_info.max_detail_scroll,
                )
            }

            PointerKind::ScrollDown => {
                let target = app.detail_scroll().saturating_add(DETAIL_SCROLL_LINES);
                set_detail_scroll_from_user(
                    app,
                    detail_layout,
                    target,
                    render_info.max_detail_scroll,
                )
            }

            _ => false,
        };
    }

    false
}

fn set_detail_scroll_from_user(
    app: &mut WatchApp,
    detail_layout: &mut detail_layout::DetailLayout,
    target: usize,
    max_scroll: Option<usize>,
) -> bool {
    detail_layout.cancel_pending_scroll_reconciliation_for_user_input();
    app.set_detail_scroll_from_user(max_scroll.map_or(target, |max| target.min(max)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;
    use crate::model::{Contest, Problem};
    use crate::stress::CandidateFailureKind;
    use ratatui::{Terminal, backend::TestBackend};
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use terminal::{PointerButton as MouseButton, PointerKind as MouseEventKind};

    fn app() -> WatchApp {
        app_with_problems(&[3])
    }

    fn app_with_problems(sample_counts: &[usize]) -> WatchApp {
        WatchApp::new(
            &contest_with_problems(sample_counts),
            sample_counts.to_vec(),
        )
        .unwrap()
    }

    fn contest_with_problems(sample_counts: &[usize]) -> Contest {
        Contest {
            contest_id: "abc123".to_string(),
            problems: sample_counts
                .iter()
                .enumerate()
                .map(|(index, sample_count)| Problem {
                    index: char::from(b'A' + index as u8).to_string(),
                    title: format!("Problem {index}"),
                    task_id: format!("abc123_{index}"),
                    url: format!("https://example.invalid/{index}"),
                    sample_count: *sample_count,
                })
                .collect(),
        }
    }

    fn handle_stress_setup_key(
        app: &mut WatchApp,
        code: KeyCode,
        destination: &Path,
        contest: &Contest,
        run_tx: &Sender<RunWorkerCommand>,
    ) -> io::Result<bool> {
        handle_key_event_with_stress_context(
            app,
            key(code, KeyEventKind::Press),
            run_tx,
            Some(StressSetupContext {
                destination,
                contest,
            }),
        )
    }

    fn foldable_app(actual: String) -> WatchApp {
        let mut app = app_with_problems(&[1]);
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_stress(0, 123).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.stress_event(
            0,
            request.run_id,
            message::StressEvent::Started {
                base_seed: 123,
                case_limit: None,
            },
        ));
        assert!(app.stress_event(
            0,
            request.run_id,
            message::StressEvent::Failed {
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

    fn rendered_fold_info(app: &WatchApp, width: u16, height: u16) -> view::RenderInfo {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout = detail_layout::DetailLayout::default();
        let mut info = view::RenderInfo::default();
        terminal
            .draw(|frame| info = view::render(frame, app, &mut layout))
            .unwrap();
        info
    }

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            kind,
            modifiers: terminal::Modifiers::default(),
        }
    }

    fn run_request(command: RunWorkerCommand) -> RunRequest {
        match command {
            RunWorkerCommand::Run(request) => request,
            RunWorkerCommand::CancelStress { problem, run_id } => {
                panic!("expected run command, got stress cancellation {problem}/{run_id}")
            }
        }
    }

    fn received_run(receiver: &Receiver<RunWorkerCommand>) -> RunRequest {
        run_request(receiver.try_recv().unwrap())
    }

    fn received_runs(receiver: &Receiver<RunWorkerCommand>) -> Vec<RunRequest> {
        receiver.try_iter().map(run_request).collect()
    }

    fn resize(columns: u16, rows: u16) -> TerminalEvent {
        TerminalEvent::Resize(terminal::TerminalSize { columns, rows })
    }

    fn handle_key(app: &mut WatchApp, code: KeyCode, kind: KeyEventKind) -> bool {
        let (run_tx, _run_rx) = mpsc::channel();

        handle_key_event(app, key(code, kind), &run_tx).unwrap()
    }

    fn handle_terminal_events(
        app: &mut WatchApp,
        render_info: &view::RenderInfo,
        events: &mut VecDeque<TerminalEvent>,
        run_tx: &Sender<RunWorkerCommand>,
    ) -> io::Result<bool> {
        let mut detail_layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        super::handle_terminal_events(
            app,
            &mut detail_layout,
            &mut drag,
            render_info,
            events,
            run_tx,
        )
    }

    fn handle_frontend_terminal_events(
        app: &mut WatchApp,
        render_info: &view::RenderInfo,
        events: &mut VecDeque<TerminalEvent>,
        run_tx: &Sender<RunWorkerCommand>,
        stress_setup: Option<StressSetupContext<'_>>,
        contest_switch: Option<&mut ContestSwitchController<'_>>,
        command_palette: &mut CommandPalette,
    ) -> io::Result<bool> {
        let mut detail_layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        super::handle_terminal_events_with_mouse_mode(
            app,
            &mut detail_layout,
            &mut drag,
            render_info,
            events,
            MouseMode::Cells,
            FrontendInputContext {
                terminal: TerminalInputContext::new(run_tx, stress_setup),
                contest_switch,
                command_palette: Some(command_palette),
                open_source: None,
            },
        )
    }

    #[derive(Debug)]
    struct RecordingSourceEditor {
        mode: EditorLaunchMode,
        resolve_error: Option<String>,
        launch_error: Option<String>,
        terminal_restore_error: Option<String>,
        resolve_calls: usize,
        external_targets: Vec<PathBuf>,
        terminal_targets: Vec<PathBuf>,
    }

    impl RecordingSourceEditor {
        fn new(mode: EditorLaunchMode) -> Self {
            Self {
                mode,
                resolve_error: None,
                launch_error: None,
                terminal_restore_error: None,
                resolve_calls: 0,
                external_targets: Vec::new(),
                terminal_targets: Vec::new(),
            }
        }
    }

    impl SourceEditorHost for RecordingSourceEditor {
        fn resolve(&mut self) -> Result<ResolvedEditor, String> {
            self.resolve_calls += 1;
            if let Some(error) = self.resolve_error.clone() {
                return Err(error);
            }
            Ok(ResolvedEditor {
                program: "test-editor".into(),
                args: Vec::new(),
                mode: self.mode,
                source: crate::editor::EditorSource::Config,
            })
        }

        fn launch_external(
            &mut self,
            _editor: &ResolvedEditor,
            target: &Path,
        ) -> Result<(), SourceLaunchError> {
            self.external_targets.push(target.to_path_buf());
            self.launch_error
                .clone()
                .map_or(Ok(()), |error| Err(SourceLaunchError::Recoverable(error)))
        }

        fn launch_terminal(
            &mut self,
            _editor: &ResolvedEditor,
            target: &Path,
        ) -> Result<(), SourceLaunchError> {
            self.terminal_targets.push(target.to_path_buf());
            if let Some(error) = self.terminal_restore_error.clone() {
                return Err(SourceLaunchError::TerminalRestore(error));
            }
            self.launch_error
                .clone()
                .map_or(Ok(()), |error| Err(SourceLaunchError::Recoverable(error)))
        }
    }

    #[derive(Debug)]
    struct RecordingSourceCreator {
        template: String,
        error: Option<String>,
        race_contents: Option<String>,
        calls: Vec<(PathBuf, String, Language)>,
    }

    impl RecordingSourceCreator {
        fn new(template: &str) -> Self {
            Self {
                template: template.to_string(),
                error: None,
                race_contents: None,
                calls: Vec::new(),
            }
        }
    }

    impl SourceCreator for RecordingSourceCreator {
        fn create(
            &mut self,
            destination: &Path,
            problem_index: &str,
            language: Language,
        ) -> Result<PathBuf, String> {
            self.calls.push((
                destination.to_path_buf(),
                problem_index.to_string(),
                language,
            ));
            if let Some(error) = self.error.clone() {
                return Err(error);
            }
            if let Some(contents) = self.race_contents.take() {
                let path = crate::workspace::source_file_path(destination, problem_index, language)
                    .map_err(|error| error.to_string())?;
                fs::write(path, contents).map_err(|error| error.to_string())?;
            }
            crate::workspace::create_source_file(
                destination,
                problem_index,
                language,
                &self.template,
            )
            .map_err(|error| error.to_string())
        }
    }

    fn handle_open_source_events(
        app: &mut WatchApp,
        render_info: &view::RenderInfo,
        events: &mut VecDeque<TerminalEvent>,
        controller: &mut OpenSourceController,
        editor: &mut dyn SourceEditorHost,
        creator: &mut dyn SourceCreator,
        command_palette: Option<&mut CommandPalette>,
    ) -> io::Result<bool> {
        let (run_tx, _run_rx) = mpsc::channel();
        let mut detail_layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        super::handle_terminal_events_with_mouse_mode(
            app,
            &mut detail_layout,
            &mut drag,
            render_info,
            events,
            MouseMode::Cells,
            FrontendInputContext {
                terminal: TerminalInputContext::new(&run_tx, None),
                contest_switch: None,
                command_palette,
                open_source: Some(OpenSourceInputContext {
                    controller,
                    editor,
                    creator,
                }),
            },
        )
    }

    fn workspace_context(root: &Path) -> AppContext {
        AppContext::Workspace {
            root: root.to_path_buf(),
        }
    }

    fn successful_create_task() -> ContestSwitchTask {
        Arc::new(|_, _| Ok(()))
    }

    fn failing_create_task() -> ContestSwitchTask {
        Arc::new(|_, _| Err(io::Error::other("contest fetch failed").into()))
    }

    fn wait_for_create_operation(controller: &mut ContestSwitchController<'_>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while controller.operation.active.is_some() && std::time::Instant::now() < deadline {
            controller.handle_operation_messages();
            thread::yield_now();
        }
        controller.handle_operation_messages();
        assert!(
            controller.operation.active.is_none(),
            "contest creation operation did not finish"
        );
    }

    fn set_displayed_contest_id(controller: &mut ContestSwitchController<'_>, contest_id: &str) {
        controller.modal.as_mut().unwrap().contest_id = contest_id.to_string();
        controller.refresh_resolution();
    }

    fn palette_labels(query: &str) -> Vec<&'static str> {
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = query.to_string();
        palette
            .filtered_actions()
            .into_iter()
            .map(FrontendAction::label)
            .collect()
    }

    #[test]
    fn command_palette_opens_closes_and_reopens_without_persisting_query() {
        let mut app = app();
        let (run_tx, _run_rx) = mpsc::channel();
        let mut palette = CommandPalette::default();
        let mut events = VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char(':'),
            KeyEventKind::Press,
        ))]);

        assert!(
            handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                None,
                &mut palette,
            )
            .unwrap()
        );
        assert!(palette.is_active());
        assert_eq!(palette.query, "");
        assert_eq!(palette.selected_action(), Some(FrontendAction::RunTests));

        assert_eq!(
            palette.handle_key(key(KeyCode::Char('q'), KeyEventKind::Press)),
            CommandPaletteKeyResult::Handled(true)
        );
        assert_eq!(palette.query, "q");
        assert_eq!(
            palette.handle_key(key(KeyCode::Escape, KeyEventKind::Press)),
            CommandPaletteKeyResult::Handled(true)
        );
        assert!(!palette.is_active());
        assert_eq!(palette.query, "");

        palette.open();
        assert!(palette.is_active());
        assert_eq!(palette.query, "");
        assert_eq!(palette.selected_action(), Some(FrontendAction::RunTests));

        palette.handle_key(key(KeyCode::Char('a'), KeyEventKind::Press));
        palette.handle_key(key(KeyCode::Char('\u{301}'), KeyEventKind::Press));
        assert_eq!(palette.query, "a\u{301}");
        assert_eq!(
            palette.handle_key(key(KeyCode::Backspace, KeyEventKind::Repeat)),
            CommandPaletteKeyResult::Handled(true)
        );
        assert_eq!(palette.query, "");
        assert_eq!(
            palette.handle_key(key(KeyCode::Char('x'), KeyEventKind::Release)),
            CommandPaletteKeyResult::Handled(false)
        );
        assert_eq!(palette.query, "");
    }

    #[test]
    fn command_palette_search_is_case_insensitive_word_prefix_matching() {
        for (query, expected) in [
            ("sw", vec!["Switch Contest"]),
            ("con", vec!["Switch Contest"]),
            ("sta", vec!["Start Stress"]),
            (
                "str",
                vec!["Start Stress", "Stop Stress", "Initialize Stress"],
            ),
            ("stop", vec!["Stop Stress"]),
            ("ini", vec!["Initialize Stress"]),
            ("deb", vec!["Toggle Debug"]),
            ("sam", vec!["Toggle Samples"]),
            ("tes", vec!["Run Tests"]),
            ("open", vec!["Open Source"]),
            ("sou", vec!["Open Source"]),
            ("Sw", vec!["Switch Contest"]),
            ("sW cOn", vec!["Switch Contest"]),
            ("to sam", vec!["Toggle Samples"]),
            ("does-not-match", vec![]),
        ] {
            assert_eq!(palette_labels(query), expected, "query {query:?}");
        }

        assert!(command_matches("Switch Contest", "  sw   con "));
        assert!(!command_matches("Switch Contest", "switching"));
    }

    #[test]
    fn command_palette_selection_wraps_resets_on_filter_and_never_auto_runs() {
        let mut palette = CommandPalette::default();
        palette.open();
        assert_eq!(palette.selected_action(), Some(FrontendAction::RunTests));

        assert_eq!(
            palette.handle_key(key(KeyCode::Down, KeyEventKind::Press)),
            CommandPaletteKeyResult::Handled(true)
        );
        assert_eq!(palette.selected_action(), Some(FrontendAction::OpenSource));
        palette.handle_key(key(KeyCode::Up, KeyEventKind::Press));
        assert_eq!(palette.selected_action(), Some(FrontendAction::RunTests));
        palette.handle_key(key(KeyCode::Up, KeyEventKind::Press));
        assert_eq!(
            palette.selected_action(),
            Some(FrontendAction::SwitchContest)
        );

        palette.open();
        for character in "str".chars() {
            assert_eq!(
                palette.handle_key(key(KeyCode::Char(character), KeyEventKind::Press)),
                CommandPaletteKeyResult::Handled(true)
            );
        }
        assert_eq!(
            palette.filtered_actions(),
            [
                FrontendAction::StartStress,
                FrontendAction::StopStress,
                FrontendAction::InitializeStress
            ]
        );
        assert_eq!(palette.selected_action(), Some(FrontendAction::StartStress));
        palette.handle_key(key(KeyCode::Down, KeyEventKind::Press));
        assert_eq!(palette.selected_action(), Some(FrontendAction::StopStress));
        palette.handle_key(key(KeyCode::Down, KeyEventKind::Press));
        assert_eq!(
            palette.selected_action(),
            Some(FrontendAction::InitializeStress)
        );
        palette.handle_key(key(KeyCode::Char(' '), KeyEventKind::Press));
        palette.handle_key(key(KeyCode::Char('i'), KeyEventKind::Press));
        assert_eq!(
            palette.filtered_actions(),
            [FrontendAction::InitializeStress]
        );
        assert_eq!(
            palette.selected_action(),
            Some(FrontendAction::InitializeStress)
        );
        assert!(palette.is_active(), "one result must not auto-execute");
        assert_eq!(
            palette.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            CommandPaletteKeyResult::ExecuteRequested(FrontendAction::InitializeStress)
        );
        assert!(
            palette.is_active(),
            "the caller closes only after availability"
        );

        palette.query = "nope".to_string();
        palette.reset_selection();
        assert!(palette.filtered_actions().is_empty());
        assert_eq!(
            palette.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            CommandPaletteKeyResult::Handled(false)
        );
        assert!(palette.is_active());
    }

    #[test]
    fn initial_shortcuts_and_palette_selection_resolve_to_the_same_frontend_actions() {
        for (index, action) in FrontendAction::ALL.into_iter().enumerate() {
            if let Some(shortcut) = action.shortcut() {
                let shortcut = shortcut.chars().next().unwrap();
                assert_eq!(
                    FrontendAction::from_shortcut(key(
                        KeyCode::Char(shortcut),
                        KeyEventKind::Press,
                    )),
                    Some(action)
                );
            } else {
                assert!(matches!(
                    action,
                    FrontendAction::OpenSource | FrontendAction::StopStress
                ));
            }

            let mut palette = CommandPalette::default();
            palette.open();
            for _ in 0..index {
                palette.handle_key(key(KeyCode::Down, KeyEventKind::Press));
            }
            assert_eq!(
                palette.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
                CommandPaletteKeyResult::ExecuteRequested(action)
            );
        }

        assert_eq!(FrontendAction::StopStress.label(), "Stop Stress");
        assert_eq!(FrontendAction::StopStress.shortcut(), None);
        assert_eq!(FrontendAction::OpenSource.label(), "Open Source");
        assert_eq!(FrontendAction::OpenSource.shortcut(), None);
    }

    #[test]
    fn open_source_action_order_availability_and_palette_activation_are_deterministic() {
        assert_eq!(
            FrontendAction::ALL,
            [
                FrontendAction::RunTests,
                FrontendAction::OpenSource,
                FrontendAction::ToggleDebug,
                FrontendAction::ToggleSamples,
                FrontendAction::StartStress,
                FrontendAction::StopStress,
                FrontendAction::InitializeStress,
                FrontendAction::SwitchContest,
            ]
        );

        let empty_contest = Contest {
            contest_id: "empty".to_string(),
            problems: Vec::new(),
        };
        let empty = WatchApp::new(&empty_contest, Vec::new()).unwrap();
        assert_eq!(
            FrontendAction::OpenSource.availability(&empty, false),
            FrontendActionAvailability::Unavailable("no selected problem")
        );

        let temp = tempfile::tempdir().unwrap();
        let mut app = app();
        assert_eq!(
            FrontendAction::OpenSource.availability(&app, false),
            FrontendActionAvailability::Available
        );
        let mut controller = OpenSourceController::new(temp.path(), Language::Cpp);
        let mut editor = RecordingSourceEditor::new(EditorLaunchMode::External);
        let mut creator = RecordingSourceCreator::new("template");
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "open sou".to_string();
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);

        assert!(
            handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &mut controller,
                &mut editor,
                &mut creator,
                Some(&mut palette),
            )
            .unwrap()
        );
        assert!(!palette.is_active());
        assert!(controller.modal_active());
        assert_eq!(controller.modal().unwrap().problem_index, "A");
    }

    #[test]
    fn open_source_initial_selection_uses_canonical_current_then_default_without_reordering() {
        let temp = tempfile::tempdir().unwrap();
        for (current, default, expected) in [
            (Some(Language::Cpp), Language::Python, Language::Cpp),
            (Some(Language::Python), Language::Cpp, Language::Python),
            (None, Language::Cpp, Language::Cpp),
            (None, Language::Python, Language::Python),
        ] {
            let mut app = app();
            if let Some(language) = current {
                let path = crate::workspace::source_file_path(temp.path(), "A", language).unwrap();
                app.source_changed(0, path, language);
            }
            let mut controller = OpenSourceController::new(temp.path(), default);

            assert!(controller.open(&app));
            let modal = controller.modal().unwrap();
            assert_eq!(Language::ALL, [Language::Cpp, Language::Python]);
            assert_eq!(modal.selected_language(), expected);
            assert_eq!(modal.current_language(&app), current);
            assert_eq!(
                modal.path_for(Language::Cpp).unwrap(),
                temp.path().join("A.cpp")
            );
            assert_eq!(
                modal.path_for(Language::Python).unwrap(),
                temp.path().join("A.py")
            );
        }
    }

    #[test]
    fn existing_source_open_switches_current_language_and_dispatches_exact_mode_and_path() {
        for (current, selected, mode) in [
            (Language::Cpp, Language::Python, EditorLaunchMode::External),
            (Language::Python, Language::Cpp, EditorLaunchMode::Terminal),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let cpp = temp.path().join("A.cpp");
            let python = temp.path().join("A.py");
            fs::write(&cpp, "cpp user source").unwrap();
            fs::write(&python, "python user source").unwrap();
            let current_path = match current {
                Language::Cpp => cpp.clone(),
                Language::Python => python.clone(),
            };
            let selected_path = match selected {
                Language::Cpp => cpp.clone(),
                Language::Python => python.clone(),
            };
            let mut app = app();
            app.source_changed(0, current_path, current);
            let mut controller = OpenSourceController::new(temp.path(), Language::Cpp);
            assert!(controller.open(&app));
            let mut editor = RecordingSourceEditor::new(mode);
            let mut creator = RecordingSourceCreator::new("unused");
            let mut events = VecDeque::from([
                TerminalEvent::Key(key(KeyCode::Down, KeyEventKind::Press)),
                TerminalEvent::Key(key(KeyCode::Char('i'), KeyEventKind::Press)),
                TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press)),
            ]);

            assert!(
                handle_open_source_events(
                    &mut app,
                    &view::RenderInfo::default(),
                    &mut events,
                    &mut controller,
                    &mut editor,
                    &mut creator,
                    None,
                )
                .unwrap()
            );
            assert!(!controller.modal_active());
            let source = app.current_problem().unwrap().source.as_ref().unwrap();
            assert_eq!(source.language, selected);
            assert_eq!(source.path, selected_path);
            match mode {
                EditorLaunchMode::External => {
                    assert_eq!(editor.external_targets, [selected_path]);
                    assert!(editor.terminal_targets.is_empty());
                }
                EditorLaunchMode::Terminal => {
                    assert_eq!(editor.terminal_targets, [selected_path]);
                    assert!(editor.external_targets.is_empty());
                }
            }
            assert_eq!(fs::read_to_string(cpp).unwrap(), "cpp user source");
            assert_eq!(fs::read_to_string(python).unwrap(), "python user source");
            assert!(creator.calls.is_empty());
        }
    }

    #[test]
    fn missing_source_enter_is_inert_and_create_open_resolves_before_safe_creation() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = app();
        let mut controller = OpenSourceController::new(temp.path(), Language::Python);
        assert!(controller.open(&app));
        let mut editor = RecordingSourceEditor::new(EditorLaunchMode::External);
        let mut creator = RecordingSourceCreator::new("print('created')\n");
        let mut enter =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);

        assert!(
            !handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut enter,
                &mut controller,
                &mut editor,
                &mut creator,
                None,
            )
            .unwrap()
        );
        assert!(!temp.path().join("A.py").exists());
        assert_eq!(editor.resolve_calls, 0);
        assert!(creator.calls.is_empty());

        let mut create = VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char('i'),
            KeyEventKind::Press,
        ))]);
        assert!(
            handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut create,
                &mut controller,
                &mut editor,
                &mut creator,
                None,
            )
            .unwrap()
        );
        let target = temp.path().join("A.py");
        assert_eq!(fs::read_to_string(&target).unwrap(), "print('created')\n");
        assert_eq!(editor.resolve_calls, 1);
        assert_eq!(
            creator.calls,
            [(temp.path().to_path_buf(), "A".into(), Language::Python)]
        );
        assert_eq!(
            editor.external_targets.as_slice(),
            std::slice::from_ref(&target)
        );
        let source = app.current_problem().unwrap().source.as_ref().unwrap();
        assert_eq!(source.path, target);
        assert_eq!(source.language, Language::Python);
        assert!(!controller.modal_active());
    }

    #[test]
    fn create_open_failures_preserve_filesystem_and_current_source_at_each_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let cpp = temp.path().join("A.cpp");
        fs::write(&cpp, "current cpp").unwrap();

        let mut app = app();
        app.source_changed(0, cpp.clone(), Language::Cpp);
        let mut controller = OpenSourceController::new(temp.path(), Language::Python);
        assert!(controller.open(&app));
        controller.modal.as_mut().unwrap().selected_language = Language::Python;
        let mut unresolved = RecordingSourceEditor::new(EditorLaunchMode::External);
        unresolved.resolve_error = Some("No editor configured.".to_string());
        let mut creator = RecordingSourceCreator::new("created");
        let mut events = VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char('i'),
            KeyEventKind::Press,
        ))]);
        assert!(
            handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &mut controller,
                &mut unresolved,
                &mut creator,
                None,
            )
            .unwrap()
        );
        assert!(!temp.path().join("A.py").exists());
        assert!(creator.calls.is_empty());
        assert_eq!(
            app.current_problem().unwrap().source.as_ref().unwrap().path,
            cpp
        );
        assert!(
            controller
                .modal()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("No editor")
        );

        controller.modal.as_mut().unwrap().error = None;
        let mut resolved = RecordingSourceEditor::new(EditorLaunchMode::External);
        creator.error = Some("template resolution failed".to_string());
        let mut events = VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char('i'),
            KeyEventKind::Press,
        ))]);
        assert!(
            handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &mut controller,
                &mut resolved,
                &mut creator,
                None,
            )
            .unwrap()
        );
        assert!(resolved.external_targets.is_empty());
        assert_eq!(
            app.current_problem().unwrap().source.as_ref().unwrap().path,
            cpp
        );

        creator.error = None;
        resolved.launch_error = Some("spawn failed".to_string());
        let mut events = VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char('i'),
            KeyEventKind::Press,
        ))]);
        assert!(
            handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &mut controller,
                &mut resolved,
                &mut creator,
                None,
            )
            .unwrap()
        );
        let python = temp.path().join("A.py");
        assert_eq!(fs::read_to_string(&python).unwrap(), "created");
        assert!(controller.modal_active());
        assert_eq!(
            controller.modal().unwrap().current_language(&app),
            Some(Language::Python)
        );
        assert_eq!(
            app.current_problem().unwrap().source.as_ref().unwrap().path,
            python
        );
        assert!(
            controller
                .modal()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("spawn failed")
        );
    }

    #[test]
    fn create_open_race_uses_no_clobber_core_and_never_launches_or_switches() {
        let temp = tempfile::tempdir().unwrap();
        let cpp = temp.path().join("A.cpp");
        fs::write(&cpp, "current cpp").unwrap();
        let mut app = app();
        app.source_changed(0, cpp.clone(), Language::Cpp);
        let mut controller = OpenSourceController::new(temp.path(), Language::Python);
        assert!(controller.open(&app));
        controller.modal.as_mut().unwrap().selected_language = Language::Python;
        assert!(!controller.selected_path().unwrap().exists());
        let mut editor = RecordingSourceEditor::new(EditorLaunchMode::External);
        let mut creator = RecordingSourceCreator::new("must not win");
        creator.race_contents = Some("race winner".to_string());
        let mut events = VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char('i'),
            KeyEventKind::Press,
        ))]);

        assert!(
            handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &mut controller,
                &mut editor,
                &mut creator,
                None,
            )
            .unwrap()
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("A.py")).unwrap(),
            "race winner"
        );
        assert!(editor.external_targets.is_empty());
        assert_eq!(
            app.current_problem().unwrap().source.as_ref().unwrap().path,
            cpp
        );
        assert!(controller.modal_active());
    }

    #[test]
    fn terminal_restore_failure_is_fatal_after_current_source_transition() {
        let temp = tempfile::tempdir().unwrap();
        let python = temp.path().join("A.py");
        fs::write(&python, "source").unwrap();
        let mut app = app();
        let mut controller = OpenSourceController::new(temp.path(), Language::Python);
        assert!(controller.open(&app));
        let mut editor = RecordingSourceEditor::new(EditorLaunchMode::Terminal);
        editor.terminal_restore_error = Some("failed to restore the TUI terminal".to_string());
        let mut creator = RecordingSourceCreator::new("unused");
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);

        let error = handle_open_source_events(
            &mut app,
            &view::RenderInfo::default(),
            &mut events,
            &mut controller,
            &mut editor,
            &mut creator,
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("restore the TUI terminal"));
        assert_eq!(
            app.current_problem().unwrap().source.as_ref().unwrap().path,
            python
        );
        assert!(controller.modal_active());
    }

    #[test]
    fn terminal_editor_discards_the_remainder_of_the_collected_input_batch() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("A.cpp");
        fs::write(&source_path, "source").unwrap();
        let mut app = app();
        let mut controller = OpenSourceController::new(temp.path(), Language::Cpp);
        assert!(controller.open(&app));
        let mut editor = RecordingSourceEditor::new(EditorLaunchMode::Terminal);
        let mut creator = RecordingSourceCreator::new("unused");
        let mut events = VecDeque::from([
            TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press)),
            TerminalEvent::Key(key(KeyCode::Char('d'), KeyEventKind::Press)),
            TerminalEvent::Pointer(pointer(PointerKind::ScrollDown, 5, 5)),
            TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
        ]);

        assert!(
            handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &mut controller,
                &mut editor,
                &mut creator,
                None,
            )
            .unwrap()
        );

        assert!(events.is_empty());
        assert!(!app.debug_enabled());
        assert_eq!(app.selected_case(), 0);
        assert!(!app.should_quit());
        assert_eq!(editor.terminal_targets, [source_path]);
        assert!(!controller.modal_active());
    }

    #[test]
    fn open_source_same_batch_quit_and_modal_input_follow_shared_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("abc123");
        let context = workspace_context(temp.path());
        let mut resolve = |_: &str| ContestSwitchResolution::rejected(None, "invalid".into());
        let contest_switch = ContestSwitchController::new(
            &context,
            &current,
            &mut resolve,
            successful_create_task(),
        );
        let mut app = app();
        let mut source = OpenSourceController::new(temp.path(), Language::Cpp);
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "open".to_string();
        let mut events = VecDeque::from([
            TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press)),
            TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
        ]);
        assert!(!contains_global_quit_event(
            &events,
            &app,
            &contest_switch,
            &palette,
            &source,
        ));
        let mut editor = RecordingSourceEditor::new(EditorLaunchMode::External);
        let mut creator = RecordingSourceCreator::new("unused");
        assert!(
            handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &mut source,
                &mut editor,
                &mut creator,
                Some(&mut palette),
            )
            .unwrap()
        );
        assert!(source.modal_active());
        assert!(!app.should_quit());

        let mut events = VecDeque::from([
            TerminalEvent::Key(key(KeyCode::Escape, KeyEventKind::Press)),
            TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
        ]);
        assert!(contains_global_quit_event(
            &events,
            &app,
            &contest_switch,
            &palette,
            &source,
        ));
        assert!(
            handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &mut source,
                &mut editor,
                &mut creator,
                Some(&mut palette),
            )
            .unwrap()
        );
        assert!(app.should_quit());
    }

    #[test]
    fn open_source_modal_consumes_global_actions_and_suppresses_underlying_pointers() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("A.cpp"), "source").unwrap();
        let mut app = app();
        let mut source = OpenSourceController::new(temp.path(), Language::Cpp);
        assert!(source.open(&app));
        let mut editor = RecordingSourceEditor::new(EditorLaunchMode::External);
        let mut creator = RecordingSourceCreator::new("unused");
        let mut keys = VecDeque::from(['r', 'S', ':', 'q'].map(|character| {
            TerminalEvent::Key(key(KeyCode::Char(character), KeyEventKind::Press))
        }));
        assert!(
            !handle_open_source_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut keys,
                &mut source,
                &mut editor,
                &mut creator,
                None,
            )
            .unwrap()
        );
        assert!(!app.should_quit());
        assert_eq!(
            app.current_problem().unwrap().run.phase,
            app::RunPhase::Idle
        );
        assert!(source.modal_active());

        let samples_info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };
        let mut pointers = VecDeque::from([
            TerminalEvent::Pointer(pointer(PointerKind::ScrollDown, 5, 5)),
            TerminalEvent::Pointer(pointer(PointerKind::Down(PointerButton::Left), 5, 5)),
            TerminalEvent::Pointer(pointer(PointerKind::Drag(PointerButton::Left), 5, 8)),
        ]);
        assert!(
            !handle_open_source_events(
                &mut app,
                &samples_info,
                &mut pointers,
                &mut source,
                &mut editor,
                &mut creator,
                None,
            )
            .unwrap()
        );
        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 0);
        assert!(source.modal_active());
    }

    #[test]
    fn command_availability_uses_only_current_frontend_state() {
        let mut app = app();
        assert_eq!(
            FrontendAction::RunTests.availability(&app, false),
            FrontendActionAvailability::Unavailable("no source file")
        );
        assert_eq!(
            FrontendAction::StartStress.availability(&app, false),
            FrontendActionAvailability::Unavailable("no source file")
        );
        assert_eq!(
            FrontendAction::InitializeStress.availability(&app, false),
            FrontendActionAvailability::Unavailable("stress initialization not required")
        );
        assert_eq!(
            FrontendAction::StopStress.availability(&app, false),
            FrontendActionAvailability::Unavailable("stress is not running")
        );
        assert_eq!(
            FrontendAction::SwitchContest.availability(&app, false),
            FrontendActionAvailability::Unavailable("not in a workspace")
        );
        assert_eq!(
            FrontendAction::ToggleDebug.availability(&app, false),
            FrontendActionAvailability::Available
        );

        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        assert_eq!(
            FrontendAction::RunTests.availability(&app, false),
            FrontendActionAvailability::Available
        );
        assert!(app.set_stress_setup_required(0, true, false));
        assert_eq!(
            FrontendAction::StartStress.availability(&app, false),
            FrontendActionAvailability::Unavailable("stress helpers not initialized")
        );
        assert_eq!(
            FrontendAction::InitializeStress.availability(&app, false),
            FrontendActionAvailability::Available
        );
        assert_eq!(
            FrontendAction::SwitchContest.availability(&app, true),
            FrontendActionAvailability::Available
        );

        let stress = app.queue_stress(0, 123).unwrap();
        assert_eq!(app.active_stress_identity(), Some((0, stress.run_id)));
        assert_eq!(
            FrontendAction::StopStress.availability(&app, false),
            FrontendActionAvailability::Available
        );
    }

    #[test]
    fn palette_enter_executes_available_action_and_keeps_unavailable_action_open() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let mut resolve = |_: &str| ContestSwitchResolution::rejected(None, "invalid".to_string());
        let standalone_context = AppContext::Standalone {
            launch_root: root.path().to_path_buf(),
        };
        let mut standalone = ContestSwitchController::new(
            &standalone_context,
            &current,
            &mut resolve,
            successful_create_task(),
        );
        let mut app = app();
        let (run_tx, _run_rx) = mpsc::channel();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "deb".to_string();
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);

        assert!(
            handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                Some(&mut standalone),
                &mut palette,
            )
            .unwrap()
        );
        assert!(app.debug_enabled());
        assert!(!palette.is_active());

        palette.open();
        palette.query = "sw".to_string();
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);
        assert!(
            !handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                Some(&mut standalone),
                &mut palette,
            )
            .unwrap()
        );
        assert!(palette.is_active());
        assert!(!standalone.modal_active());

        palette.query = "stop".to_string();
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);
        assert!(
            !handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                Some(&mut standalone),
                &mut palette,
            )
            .unwrap()
        );
        assert!(palette.is_active());
    }

    #[test]
    fn palette_stop_stress_targets_active_generation_independent_of_selection() {
        let mut app = app_with_problems(&[1, 1]);
        assert!(app.source_changed(1, PathBuf::from("B.py"), Language::Python));
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let stress = app.queue_stress(0, 123).unwrap();
        assert!(app.run_started(0, stress.run_id));
        assert!(app.select_problem(1));
        let (run_tx, run_rx) = mpsc::channel();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "stop".to_string();
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);

        assert!(
            handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                None,
                &mut palette,
            )
            .unwrap()
        );

        assert!(!palette.is_active());
        assert_eq!(app.selected_problem(), Some(1));
        assert_eq!(app.problems()[0].stress.phase, app::StressPhase::Cancelled);
        assert_eq!(app.problems()[1].stress.phase, app::StressPhase::Idle);
        assert_eq!(
            run_rx.try_recv().unwrap(),
            RunWorkerCommand::CancelStress {
                problem: 0,
                run_id: stress.run_id,
            }
        );
    }

    #[test]
    fn failed_stop_submission_does_not_cancel_logical_stress() {
        let mut app = app_with_problems(&[1]);
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let stress = app.queue_stress(0, 123).unwrap();
        let (run_tx, run_rx) = mpsc::channel();
        drop(run_rx);

        let error = execute_frontend_action(
            &mut app,
            FrontendAction::StopStress,
            TerminalInputContext::new(&run_tx, None),
            None,
            None,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(app.active_stress_identity(), Some((0, stress.run_id)));
        assert_eq!(app.problems()[0].stress.phase, app::StressPhase::Queued);
    }

    #[test]
    fn palette_enter_rechecks_current_availability_before_execution() {
        let mut app = app_with_problems(&[1, 1]);
        assert!(app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp));
        let (run_tx, run_rx) = mpsc::channel();
        let mut palette = CommandPalette::default();
        palette.open();
        palette.query = "tes".to_string();
        assert_eq!(
            FrontendAction::RunTests.availability(&app, false),
            FrontendActionAvailability::Available
        );

        assert!(app.select_problem(1));
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);
        assert!(
            !handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                None,
                &mut palette,
            )
            .unwrap()
        );
        assert!(palette.is_active());
        assert!(run_rx.try_recv().is_err());

        assert!(app.source_changed(1, PathBuf::from("B.py"), Language::Python));
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);
        assert!(
            handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                None,
                &mut palette,
            )
            .unwrap()
        );
        assert!(!palette.is_active());
        assert_eq!(received_run(&run_rx).problem, 1);
    }

    #[test]
    fn palette_and_contest_switch_enforce_modal_precedence_without_stacking() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let context = workspace_context(root.path());
        let mut resolve = |_: &str| ContestSwitchResolution::rejected(None, "invalid".to_string());
        let mut controller = ContestSwitchController::new(
            &context,
            &current,
            &mut resolve,
            successful_create_task(),
        );
        let mut app = app();
        let (run_tx, _run_rx) = mpsc::channel();
        let mut palette = CommandPalette::default();
        palette.open();
        let mut events =
            VecDeque::from(['q', 'c', ':', 'r', 'd', 's', 'S', 'i'].map(|character| {
                TerminalEvent::Key(key(KeyCode::Char(character), KeyEventKind::Press))
            }));

        assert!(
            handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                Some(&mut controller),
                &mut palette,
            )
            .unwrap()
        );
        assert_eq!(palette.query, "qc:rdsSi");
        assert!(!app.should_quit());
        assert!(!app.debug_enabled());
        assert!(!app.samples_pane_enabled());
        assert!(!controller.modal_active());

        palette.open();
        palette.query = "sw".to_string();
        let mut events =
            VecDeque::from([TerminalEvent::Key(key(KeyCode::Enter, KeyEventKind::Press))]);
        assert!(
            handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                Some(&mut controller),
                &mut palette,
            )
            .unwrap()
        );
        assert!(!palette.is_active());
        assert!(controller.modal_active());

        let mut events = VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char(':'),
            KeyEventKind::Press,
        ))]);
        assert!(
            handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                Some(&mut controller),
                &mut palette,
            )
            .unwrap()
        );
        assert_eq!(controller.modal().unwrap().contest_id, ":");
        assert!(!palette.is_active());
    }

    #[test]
    fn palette_state_simulation_keeps_same_batch_q_and_c_safe() {
        for character in ['q', 'c'] {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("abc123");
            let context = workspace_context(root.path());
            let mut resolve =
                |_: &str| ContestSwitchResolution::rejected(None, "invalid".to_string());
            let mut controller = ContestSwitchController::new(
                &context,
                &current,
                &mut resolve,
                successful_create_task(),
            );
            let mut app = app();
            let (run_tx, _run_rx) = mpsc::channel();
            let mut palette = CommandPalette::default();
            let source = OpenSourceController::new(Path::new("."), Language::Cpp);
            let mut events = VecDeque::from([
                TerminalEvent::Key(key(KeyCode::Char(':'), KeyEventKind::Press)),
                TerminalEvent::Key(key(KeyCode::Char(character), KeyEventKind::Press)),
            ]);

            assert!(!contains_global_quit_event(
                &events,
                &app,
                &controller,
                &palette,
                &source,
            ));
            assert!(
                handle_frontend_terminal_events(
                    &mut app,
                    &view::RenderInfo::default(),
                    &mut events,
                    &run_tx,
                    None,
                    Some(&mut controller),
                    &mut palette,
                )
                .unwrap()
            );
            assert!(palette.is_active());
            assert_eq!(palette.query, character.to_string());
            assert!(!app.should_quit());
            assert!(!controller.modal_active());
        }
    }

    #[test]
    fn escape_then_q_in_one_batch_restores_global_quit_behavior() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let context = workspace_context(root.path());
        let mut resolve = |_: &str| ContestSwitchResolution::rejected(None, "invalid".to_string());
        let mut controller = ContestSwitchController::new(
            &context,
            &current,
            &mut resolve,
            successful_create_task(),
        );
        let mut app = app();
        let (run_tx, _run_rx) = mpsc::channel();
        let mut palette = CommandPalette::default();
        let source = OpenSourceController::new(Path::new("."), Language::Cpp);
        palette.open();
        palette.query = "deb".to_string();
        let mut events = VecDeque::from([
            TerminalEvent::Key(key(KeyCode::Escape, KeyEventKind::Press)),
            TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
        ]);

        assert!(contains_global_quit_event(
            &events,
            &app,
            &controller,
            &palette,
            &source,
        ));
        assert!(
            handle_frontend_terminal_events(
                &mut app,
                &view::RenderInfo::default(),
                &mut events,
                &run_tx,
                None,
                Some(&mut controller),
                &mut palette,
            )
            .unwrap()
        );
        assert!(!palette.is_active());
        assert_eq!(palette.query, "");
        assert!(app.should_quit());
    }

    #[test]
    fn palette_suppresses_detail_scrollbar_and_samples_pointer_actions() {
        let mut app = foldable_app("actual body\n".repeat(1_000));
        let info = rendered_fold_info(&app, 100, 40);
        let header = *info
            .detail_section_headers
            .iter()
            .find(|target| target.kind == detail::DetailSectionKind::Actual)
            .unwrap();
        let scrollbar = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                PointerKind::Down(PointerButton::Left),
                header.area.x.saturating_add(1),
                header.area.y,
            )),
            TerminalEvent::Pointer(pointer(
                PointerKind::Down(PointerButton::Left),
                scrollbar.gutter.x,
                scrollbar.track_end_row().saturating_sub(1),
            )),
        ]);
        let (run_tx, _run_rx) = mpsc::channel();
        let mut palette = CommandPalette::default();
        palette.open();

        assert!(
            !handle_frontend_terminal_events(
                &mut app,
                &info,
                &mut events,
                &run_tx,
                None,
                None,
                &mut palette,
            )
            .unwrap()
        );
        assert!(
            !app.detail_fold_state()
                .is_collapsed(detail::DetailSectionKind::Actual)
        );
        assert_eq!(app.detail_scroll(), 0);

        let samples_info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };
        let mut events = VecDeque::from([TerminalEvent::Pointer(pointer(
            PointerKind::ScrollDown,
            5,
            5,
        ))]);
        assert!(
            !handle_frontend_terminal_events(
                &mut app,
                &samples_info,
                &mut events,
                &run_tx,
                None,
                None,
                &mut palette,
            )
            .unwrap()
        );
        assert_eq!(app.selected_case(), 0);
    }

    #[test]
    fn palette_open_cancels_pixel_drag_and_suppresses_later_pixel_events() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let generation = 17;
        let mode = pixel_mode(generation);
        let info = scrollbar_info_with_pixels(&app, 1_000, 0, 1, Some((20, generation)));
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let gutter_x = u32::from(geometry.gutter.x) * 10 + 5;
        let thumb_y = u32::from(geometry.thumb_start_row) * 20 + 10;

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Down(PointerButton::Left),
            gutter_x,
            thumb_y,
            Some(generation),
        ));
        assert!(drag.active.is_some());

        let (run_tx, _run_rx) = mpsc::channel();
        let mut palette = CommandPalette::default();
        let mut events = VecDeque::from([
            TerminalEvent::Key(key(KeyCode::Char(':'), KeyEventKind::Press)),
            TerminalEvent::Pointer(pixel_pointer(
                PointerKind::Drag(PointerButton::Left),
                gutter_x,
                thumb_y + 200,
                Some(generation),
            )),
            TerminalEvent::Pointer(pixel_pointer(
                PointerKind::ScrollDown,
                gutter_x,
                thumb_y,
                Some(generation),
            )),
        ]);

        assert!(
            super::handle_terminal_events_with_mouse_mode(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                mode,
                FrontendInputContext {
                    terminal: TerminalInputContext::new(&run_tx, None),
                    contest_switch: None,
                    command_palette: Some(&mut palette),
                    open_source: None,
                },
            )
            .unwrap()
        );
        assert!(events.is_empty());
        assert!(palette.is_active());
        assert!(drag.active.is_none());
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn contest_switch_modal_opens_and_cancels_only_in_workspace_context() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "incomplete".to_string());
        let mut controller = ContestSwitchController::new(
            &workspace_context(root.path()),
            &current,
            &mut resolve,
            successful_create_task(),
        );

        assert_eq!(
            controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert!(controller.modal_active());
        assert_eq!(
            controller.handle_key(key(KeyCode::Escape, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert!(!controller.modal_active());

        let standalone = AppContext::Standalone {
            launch_root: root.path().to_path_buf(),
        };
        let mut standalone = ContestSwitchController::new(
            &standalone,
            &current,
            &mut resolve,
            successful_create_task(),
        );
        assert_eq!(
            standalone.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press)),
            ContestSwitchKeyResult::NotHandled
        );
        assert!(!standalone.modal_active());
    }

    #[test]
    fn modal_accepts_chars_q_backspace_and_unicode_grapheme_backspace() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let mut resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "incomplete".to_string());
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(
            &context,
            &current,
            &mut resolve,
            successful_create_task(),
        );
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));

        for character in ['a', 'b', 'c', 'q', '7', '0', 'e', '\u{301}'] {
            assert_eq!(
                controller.handle_key(key(KeyCode::Char(character), KeyEventKind::Press)),
                ContestSwitchKeyResult::Handled
            );
        }
        assert_eq!(controller.modal().unwrap().contest_id, "abcq70e\u{301}");
        assert_eq!(
            controller.handle_key(key(KeyCode::Backspace, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert_eq!(controller.modal().unwrap().contest_id, "abcq70");
        assert!(!controller.switch_requested);
    }

    #[test]
    fn modal_enter_switches_only_for_an_accepted_different_destination() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let next = root.path().join("abc467");
        let mut resolve = |contest_id: &str| {
            if contest_id == "abc467" {
                ContestSwitchResolution::accepted(next.clone())
            } else {
                ContestSwitchResolution::rejected(None, "invalid contest".to_string())
            }
        };
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(
            &context,
            &current,
            &mut resolve,
            successful_create_task(),
        );
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        for character in "bad".chars() {
            controller.handle_key(key(KeyCode::Char(character), KeyEventKind::Press));
        }
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert!(controller.modal_active());
        assert!(!controller.switch_requested);

        set_displayed_contest_id(&mut controller, "abc467");
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::SwitchRequested
        );
        assert!(controller.switch_requested);
    }

    #[test]
    fn same_destination_is_a_noop_and_closes_the_modal() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let mut resolve = |_: &str| ContestSwitchResolution::accepted(current.clone());
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(
            &context,
            &current,
            &mut resolve,
            successful_create_task(),
        );
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc123");

        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert!(!controller.modal_active());
        assert!(!controller.switch_requested);
    }

    #[test]
    fn repair_required_preview_is_non_mutating_and_confirmation_starts_one_repair() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("ABC/abc470");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        let mut resolve = |_: &str| ContestSwitchResolution::repair_required(destination.clone());
        let (request_tx, request_rx) = mpsc::channel();
        let task: ContestSwitchTask = Arc::new(move |request, _| {
            request_tx.send(request).unwrap();
            Err(io::Error::other("injected repair failure").into())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);

        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");
        let modal = controller.modal().unwrap();
        assert_eq!(modal.destination, Some(destination.clone()));
        assert_eq!(modal.target, Some(ContestSwitchTarget::RepairRequired));
        assert!(controller.operation.active.is_none());
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        controller.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        assert!(!controller.modal_active());
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        let request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(request.mutation, ContestSwitchMutation::Repair);
        assert_eq!(request.contest_id, "abc470");
        assert_eq!(request.destination, destination);
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Repairing
        );
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        wait_for_create_operation(&mut controller);
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Failed
        );
        assert_eq!(
            controller.modal().unwrap().mutation,
            Some(ContestSwitchMutation::Repair)
        );
    }

    #[test]
    fn repair_confirmation_races_refresh_without_acting_until_reconfirmed() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let first = root.path().join("one/abc470");
        let second = root.path().join("two/abc470");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let phase = Cell::new(0);
        let mut resolve = |_: &str| match phase.get() {
            0 => ContestSwitchResolution::repair_required(first.clone()),
            _ => ContestSwitchResolution::repair_required(second.clone()),
        };
        let (request_tx, request_rx) = mpsc::channel();
        let task: ContestSwitchTask = Arc::new(move |request, _| {
            request_tx.send(request).unwrap();
            Err(io::Error::other("stop after capture").into())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");

        phase.set(1);
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert_eq!(
            controller.modal().unwrap().destination,
            Some(second.clone())
        );
        assert_eq!(
            controller.modal().unwrap().target,
            Some(ContestSwitchTarget::RepairRequired)
        );
        assert!(controller.operation.active.is_none());
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        let request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(request.mutation, ContestSwitchMutation::Repair);
        assert_eq!(request.destination, second);
        wait_for_create_operation(&mut controller);

        for changed_target in [ContestSwitchTarget::Existing, ContestSwitchTarget::Missing] {
            let destination = root.path().join(format!("changed-{changed_target:?}"));
            std::fs::create_dir(&destination).unwrap();
            let phase = Cell::new(false);
            let mut resolve = |_: &str| {
                if !phase.get() {
                    ContestSwitchResolution::repair_required(destination.clone())
                } else if changed_target == ContestSwitchTarget::Existing {
                    ContestSwitchResolution::accepted(destination.clone())
                } else {
                    ContestSwitchResolution::missing(destination.clone())
                }
            };
            let (request_tx, request_rx) = mpsc::channel();
            let task: ContestSwitchTask = Arc::new(move |request, _| {
                request_tx.send(request).unwrap();
                Err(io::Error::other("captured").into())
            });
            let mut controller =
                ContestSwitchController::new(&context, &current, &mut resolve, task);
            controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
            set_displayed_contest_id(&mut controller, "abc470");
            phase.set(true);

            assert_eq!(
                controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
                ContestSwitchKeyResult::Handled
            );
            assert_eq!(controller.modal().unwrap().target, Some(changed_target));
            assert!(!controller.switch_requested);
            assert!(controller.operation.active.is_none());
            assert!(matches!(
                request_rx.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));

            let result = controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
            if changed_target == ContestSwitchTarget::Existing {
                assert_eq!(result, ContestSwitchKeyResult::SwitchRequested);
                assert!(controller.switch_requested);
            } else {
                assert_eq!(result, ContestSwitchKeyResult::Handled);
                let request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                assert_eq!(request.mutation, ContestSwitchMutation::Create);
                wait_for_create_operation(&mut controller);
            }
        }
    }

    #[test]
    fn repair_target_becoming_an_error_starts_no_action() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let phase = Cell::new(false);
        let mut resolve = |_: &str| {
            if phase.get() {
                ContestSwitchResolution::rejected(
                    Some(destination.clone()),
                    "workspace mapping is ambiguous".to_string(),
                )
            } else {
                ContestSwitchResolution::repair_required(destination.clone())
            }
        };
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_starts = Arc::clone(&starts);
        let task: ContestSwitchTask = Arc::new(move |_, _| {
            task_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");

        phase.set(true);
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert_eq!(controller.modal().unwrap().target, None);
        assert_eq!(
            controller.modal().unwrap().error.as_deref(),
            Some("workspace mapping is ambiguous")
        );
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!controller.switch_requested);
    }

    #[test]
    fn active_destination_repair_is_deliberately_rejected() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let mut resolve = |_: &str| ContestSwitchResolution::repair_required(current.clone());
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_starts = Arc::clone(&starts);
        let task: ContestSwitchTask = Arc::new(move |_, _| {
            task_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc123");

        let modal = controller.modal().unwrap();
        assert_eq!(modal.destination, Some(current.clone()));
        assert_eq!(modal.target, None);
        assert_eq!(
            modal.error.as_deref(),
            Some("Cannot repair the active contest from Switch Contest yet.")
        );
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        assert!(controller.modal_active());
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(controller.operation.active.is_none());
        assert!(!controller.switch_requested);
    }

    #[test]
    fn active_destination_identity_error_blocks_repair_and_surfaces_the_failure() {
        fn identity_error(_: &Path, _: &Path) -> io::Result<bool> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "identity unavailable",
            ))
        }

        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let active_marker = current.join("active-session.txt");
        std::fs::write(&active_marker, "unchanged\n").unwrap();
        let mut resolve = |_: &str| ContestSwitchResolution::repair_required(destination.clone());
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_starts = Arc::clone(&starts);
        let task: ContestSwitchTask = Arc::new(move |_, _| {
            task_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new_with_identity_check(
            &context,
            &current,
            &mut resolve,
            task,
            identity_error,
        );
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");

        let modal = controller.modal().unwrap();
        assert_eq!(modal.destination, Some(destination.clone()));
        assert_eq!(modal.target, None);
        assert!(
            modal
                .error
                .as_deref()
                .unwrap()
                .contains("Cannot determine whether the repair target is the active contest")
        );
        assert!(
            modal
                .error
                .as_deref()
                .unwrap()
                .contains("identity unavailable")
        );

        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(controller.operation.active.is_none());
        assert!(!controller.switch_requested);
        assert_eq!(
            std::fs::read_to_string(active_marker).unwrap(),
            "unchanged\n"
        );
    }

    #[test]
    fn distinct_existing_destination_remains_repairable() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&destination).unwrap();
        assert!(!existing_destinations_have_same_identity(&current, &destination).unwrap());

        let mut resolve = |_: &str| ContestSwitchResolution::repair_required(destination.clone());
        let task: ContestSwitchTask = Arc::new(|_, _| Ok(()));
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");

        let modal = controller.modal().unwrap();
        assert_eq!(modal.destination, Some(destination.clone()));
        assert_eq!(modal.target, Some(ContestSwitchTarget::RepairRequired));
        assert!(modal.error.is_none());
        assert!(controller.operation.active.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn differently_cased_active_destination_alias_is_rejected_for_repair() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("ABC").join("abc123");
        std::fs::create_dir_all(&current).unwrap();
        let alias = root.path().join("abc").join("abc123");
        assert_ne!(current, alias);
        assert_eq!(
            std::fs::canonicalize(&current).unwrap(),
            std::fs::canonicalize(&alias).unwrap()
        );

        let mut resolve = |_: &str| ContestSwitchResolution::repair_required(alias.clone());
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_starts = Arc::clone(&starts);
        let task: ContestSwitchTask = Arc::new(move |_, _| {
            task_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc123");

        let modal = controller.modal().unwrap();
        assert_eq!(modal.destination, Some(alias.clone()));
        assert_eq!(modal.target, None);
        assert_eq!(
            modal.error.as_deref(),
            Some("Cannot repair the active contest from Switch Contest yet.")
        );
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(controller.operation.active.is_none());
        assert!(!controller.switch_requested);
    }

    #[test]
    fn standalone_context_cannot_enter_repair_and_switch() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        let mut resolve = |_: &str| ContestSwitchResolution::repair_required(destination.clone());
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_starts = Arc::clone(&starts);
        let task: ContestSwitchTask = Arc::new(move |_, _| {
            task_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let context = AppContext::Standalone {
            launch_root: root.path().to_path_buf(),
        };
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);

        assert_eq!(
            controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press)),
            ContestSwitchKeyResult::NotHandled
        );
        assert!(!controller.modal_active());
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn missing_destination_requires_confirmation_and_escape_before_it_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("ABC/abc470");
        let mut resolve = |_: &str| ContestSwitchResolution::missing(destination.clone());
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_starts = Arc::clone(&starts);
        let task: ContestSwitchTask = Arc::new(move |_, _| {
            task_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);

        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");

        assert_eq!(
            controller.modal().unwrap().destination,
            Some(destination.clone())
        );
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        controller.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        assert!(!controller.modal_active());
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn changed_missing_destination_refreshes_without_starting_creation() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let first_destination = root.path().join("one/abc470");
        let second_destination = root.path().join("two/abc470");
        let workspace_config = root.path().join(".atc-workspace.toml");
        fs::write(
            &workspace_config,
            "version = 1\npaths = [{ pattern = \"^abc\", path = \"one\" }]\n",
        )
        .unwrap();
        let mut resolve = |contest_id: &str| {
            ContestSwitchResolution::missing(
                crate::workspace::resolve_contest_path(root.path(), contest_id).unwrap(),
            )
        };
        let (request_tx, request_rx) = mpsc::channel();
        let task: ContestSwitchTask = Arc::new(move |request, _| {
            request_tx.send(request).unwrap();
            Err(io::Error::other("injected create failure").into())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");
        assert_eq!(
            controller.modal().unwrap().destination,
            Some(first_destination.clone())
        );

        fs::write(
            &workspace_config,
            "version = 1\npaths = [{ pattern = \"^abc\", path = \"two\" }]\n",
        )
        .unwrap();
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );

        let modal = controller.modal().unwrap();
        assert_eq!(modal.state, SwitchContestModalState::Input);
        assert_eq!(modal.target, Some(ContestSwitchTarget::Missing));
        assert_eq!(modal.destination, Some(second_destination.clone()));
        assert!(modal.error.is_none());
        assert!(controller.operation.active.is_none());
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(!root.path().join("one").exists());
        assert!(!root.path().join("two").exists());

        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        let request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(request.destination, second_destination);
        wait_for_create_operation(&mut controller);
    }

    #[test]
    fn existing_target_becoming_missing_requires_create_reconfirmation() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        let phase = Cell::new(0);
        let mut resolve = |_: &str| match phase.get() {
            0 => ContestSwitchResolution::accepted(destination.clone()),
            _ => ContestSwitchResolution::missing(destination.clone()),
        };
        let (request_tx, request_rx) = mpsc::channel();
        let task: ContestSwitchTask = Arc::new(move |request, _| {
            request_tx.send(request).unwrap();
            Err(io::Error::other("injected create failure").into())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");
        assert_eq!(
            controller.modal().unwrap().target,
            Some(ContestSwitchTarget::Existing)
        );

        phase.set(1);
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert!(!controller.switch_requested);
        assert!(controller.operation.active.is_none());
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert_eq!(
            controller.modal().unwrap().target,
            Some(ContestSwitchTarget::Missing)
        );

        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        let request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(request.destination, destination);
        wait_for_create_operation(&mut controller);
    }

    #[test]
    fn missing_target_becoming_existing_requires_switch_reconfirmation() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        let phase = Cell::new(0);
        let mut resolve = |_: &str| match phase.get() {
            0 => ContestSwitchResolution::missing(destination.clone()),
            _ => ContestSwitchResolution::accepted(destination.clone()),
        };
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_starts = Arc::clone(&starts);
        let task: ContestSwitchTask = Arc::new(move |_, _| {
            task_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");

        phase.set(1);
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        assert!(!controller.switch_requested);
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            controller.modal().unwrap().target,
            Some(ContestSwitchTarget::Existing)
        );

        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::SwitchRequested
        );
        assert!(controller.switch_requested);
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn error_introduced_before_enter_refreshes_without_starting_an_action() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        let phase = Cell::new(0);
        let mut resolve = |_: &str| match phase.get() {
            0 => ContestSwitchResolution::missing(destination.clone()),
            _ => ContestSwitchResolution::rejected(
                Some(destination.clone()),
                "contest metadata became invalid".to_string(),
            ),
        };
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_starts = Arc::clone(&starts);
        let task: ContestSwitchTask = Arc::new(move |_, _| {
            task_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");

        phase.set(1);
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );

        let modal = controller.modal().unwrap();
        assert_eq!(modal.state, SwitchContestModalState::Input);
        assert_eq!(modal.target, None);
        assert_eq!(modal.destination, Some(destination.clone()));
        assert_eq!(
            modal.error.as_deref(),
            Some("contest metadata became invalid")
        );
        assert!(!controller.switch_requested);
        assert!(controller.operation.active.is_none());
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!destination.exists());
    }

    #[test]
    fn failed_creation_character_input_resumes_editing_and_refreshes_preview() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let mut resolve =
            |contest_id: &str| ContestSwitchResolution::missing(root.path().join(contest_id));
        let context = workspace_context(root.path());
        let mut controller =
            ContestSwitchController::new(&context, &current, &mut resolve, failing_create_task());
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "anc");
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        wait_for_create_operation(&mut controller);

        assert!(
            controller
                .modal()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("contest fetch failed")
        );
        controller.handle_key(key(KeyCode::Char('x'), KeyEventKind::Press));

        let modal = controller.modal().unwrap();
        assert_eq!(modal.state, SwitchContestModalState::Input);
        assert_eq!(modal.contest_id, "ancx");
        assert_eq!(modal.destination, Some(root.path().join("ancx")));
        assert_eq!(modal.target, Some(ContestSwitchTarget::Missing));
        assert_eq!(modal.error, None);
        assert!(modal.progress.is_empty());
    }

    #[test]
    fn failed_creation_backspace_removes_last_grapheme_and_refreshes_preview() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let mut resolve =
            |contest_id: &str| ContestSwitchResolution::missing(root.path().join(contest_id));
        let context = workspace_context(root.path());
        let mut controller =
            ContestSwitchController::new(&context, &current, &mut resolve, failing_create_task());
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "anc\u{301}");
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        wait_for_create_operation(&mut controller);

        assert!(
            controller
                .modal()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("contest fetch failed")
        );
        controller.handle_key(key(KeyCode::Backspace, KeyEventKind::Press));

        let modal = controller.modal().unwrap();
        assert_eq!(modal.state, SwitchContestModalState::Input);
        assert_eq!(modal.contest_id, "an");
        assert_eq!(modal.destination, Some(root.path().join("an")));
        assert_eq!(modal.target, Some(ContestSwitchTarget::Missing));
        assert_eq!(modal.error, None);
        assert!(modal.progress.is_empty());
    }

    #[test]
    fn failed_creation_enter_retries_the_same_resolved_target() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("anc");
        let mut resolve = |_: &str| ContestSwitchResolution::missing(destination.clone());
        let (request_tx, request_rx) = mpsc::channel();
        let task: ContestSwitchTask = Arc::new(move |request, _| {
            request_tx.send(request).unwrap();
            Err(io::Error::other("contest fetch failed").into())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "anc");

        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        let first_request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_for_create_operation(&mut controller);
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Failed
        );

        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        let retry_request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert_eq!(retry_request, first_request);
        assert_eq!(retry_request.contest_id, "anc");
        assert_eq!(retry_request.destination, destination);
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Creating
        );
        wait_for_create_operation(&mut controller);
    }

    #[test]
    fn failed_creation_mapping_change_refreshes_without_retrying() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let first_destination = root.path().join("one/anc");
        let second_destination = root.path().join("two/anc");
        let phase = Cell::new(0);
        let mut resolve = |_: &str| match phase.get() {
            0 => ContestSwitchResolution::missing(first_destination.clone()),
            _ => ContestSwitchResolution::missing(second_destination.clone()),
        };
        let (request_tx, request_rx) = mpsc::channel();
        let task: ContestSwitchTask = Arc::new(move |request, _| {
            request_tx.send(request).unwrap();
            Err(io::Error::other("contest fetch failed").into())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "anc");

        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        let first_request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(first_request.destination, first_destination);
        wait_for_create_operation(&mut controller);
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Failed
        );

        phase.set(1);
        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );

        let modal = controller.modal().unwrap();
        assert_eq!(modal.state, SwitchContestModalState::Input);
        assert_eq!(modal.target, Some(ContestSwitchTarget::Missing));
        assert_eq!(modal.destination, Some(second_destination.clone()));
        assert!(modal.error.is_none());
        assert!(modal.progress.is_empty());
        assert!(controller.operation.active.is_none());
        assert!(matches!(
            request_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        let retry_request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(retry_request.destination, second_destination);
        wait_for_create_operation(&mut controller);
    }

    #[test]
    fn failed_creation_escape_dismisses_the_modal() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("anc");
        let mut resolve = |_: &str| ContestSwitchResolution::missing(destination.clone());
        let context = workspace_context(root.path());
        let mut controller =
            ContestSwitchController::new(&context, &current, &mut resolve, failing_create_task());
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "anc");
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        wait_for_create_operation(&mut controller);

        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Failed
        );
        controller.handle_key(key(KeyCode::Escape, KeyEventKind::Press));

        assert!(!controller.modal_active());
        assert!(!controller.switch_requested);
    }

    #[test]
    fn confirmed_creation_runs_off_thread_reports_progress_and_keeps_old_events_usable() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        let mut resolve = |_: &str| ContestSwitchResolution::missing(destination.clone());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let task_release = Arc::clone(&release_rx);
        let task: ContestSwitchTask = Arc::new(move |request, reporter| {
            started_tx.send(thread::current().id()).unwrap();
            reporter.report(Event::ContestFetching {
                contest_id: &request.contest_id,
            });
            reporter.report(Event::ContestFetched {
                contest_id: &request.contest_id,
                problems: 1,
            });
            reporter.report(Event::ProblemFetching {
                index: "A",
                current: 1,
                total: 1,
            });
            reporter.report(Event::ProblemFetchFailed {
                index: "A",
                error: "sample endpoint unavailable",
            });
            task_release.lock().unwrap().recv().unwrap();
            Err(io::Error::other("contest fetch failed").into())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");

        assert_eq!(
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            ContestSwitchKeyResult::Handled
        );
        let worker_thread = started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_ne!(worker_thread, thread::current().id());
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Creating
        );
        assert!(controller.operation.active.is_some());

        controller.handle_key(key(KeyCode::Char('x'), KeyEventKind::Press));
        assert_eq!(controller.modal().unwrap().contest_id, "abc470");
        controller.handle_key(key(KeyCode::Backspace, KeyEventKind::Press));
        assert_eq!(controller.modal().unwrap().contest_id, "abc470");
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Creating
        );

        let duplicate = controller.operation.start(ContestSwitchRequest {
            mutation: ContestSwitchMutation::Create,
            contest_id: "abc471".to_string(),
            destination: root.path().join("abc471"),
        });
        assert_eq!(duplicate.unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        controller.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        assert!(
            controller.modal_active(),
            "Escape must not cancel a running create"
        );

        let mut old_app = app();
        let (old_tx, old_rx) = mpsc::channel();
        let (run_tx, _run_rx) = mpsc::channel();
        old_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("old-session-A.cpp"),
                language: Language::Cpp,
            })
            .unwrap();
        assert!(handle_messages(&mut old_app, &old_rx, &run_tx).unwrap());
        assert_eq!(
            old_app
                .current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .path,
            PathBuf::from("old-session-A.cpp")
        );

        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while controller.operation.active.is_some() && std::time::Instant::now() < deadline {
            controller.handle_operation_messages();
            thread::yield_now();
        }
        controller.handle_operation_messages();

        let modal = controller.modal().unwrap();
        assert_eq!(modal.state, SwitchContestModalState::Failed);
        assert!(
            modal
                .error
                .as_deref()
                .unwrap()
                .contains("contest fetch failed")
        );
        assert!(modal.progress.iter().any(|progress| matches!(
            progress,
            ContestSwitchProgress::ContestFetching { contest_id } if contest_id == "abc470"
        )));
        assert!(modal.progress.iter().any(|progress| matches!(
            progress,
            ContestSwitchProgress::ProblemFetchFailed { index, error }
                if index == "A" && error == "sample endpoint unavailable"
        )));
        assert!(controller.operation.active.is_none());
        assert!(!controller.switch_requested);

        controller.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        assert!(!controller.modal_active());
    }

    #[test]
    fn confirmed_repair_runs_off_thread_reports_progress_and_keeps_old_events_usable() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let mut resolve = |_: &str| ContestSwitchResolution::repair_required(destination.clone());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let task_release = Arc::clone(&release_rx);
        let task: ContestSwitchTask = Arc::new(move |request, reporter| {
            started_tx.send(thread::current().id()).unwrap();
            reporter.report(Event::ContestFetching {
                contest_id: &request.contest_id,
            });
            reporter.report(Event::ProblemFetching {
                index: "A",
                current: 1,
                total: 1,
            });
            reporter.report(Event::WorkspaceRepaired {
                destination: &request.destination,
            });
            task_release.lock().unwrap().recv().unwrap();
            Err(io::Error::other("repair failed").into())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));

        let worker_thread = started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_ne!(worker_thread, thread::current().id());
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Repairing
        );
        assert!(controller.operation.active.is_some());

        controller.handle_key(key(KeyCode::Char('x'), KeyEventKind::Press));
        controller.handle_key(key(KeyCode::Backspace, KeyEventKind::Press));
        controller.handle_key(key(KeyCode::Escape, KeyEventKind::Press));
        assert_eq!(controller.modal().unwrap().contest_id, "abc470");
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Repairing
        );
        assert!(controller.modal_active());

        let duplicate = controller.operation.start(ContestSwitchRequest {
            mutation: ContestSwitchMutation::Create,
            contest_id: "abc471".to_string(),
            destination: root.path().join("abc471"),
        });
        assert_eq!(duplicate.unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        let mut old_app = app();
        let (old_tx, old_rx) = mpsc::channel();
        let (run_tx, _run_rx) = mpsc::channel();
        old_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("old-session-still-live.cpp"),
                language: Language::Cpp,
            })
            .unwrap();
        assert!(handle_messages(&mut old_app, &old_rx, &run_tx).unwrap());
        assert_eq!(
            old_app
                .current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .path,
            PathBuf::from("old-session-still-live.cpp")
        );

        release_tx.send(()).unwrap();
        wait_for_create_operation(&mut controller);
        let modal = controller.modal().unwrap();
        assert_eq!(modal.state, SwitchContestModalState::Failed);
        assert_eq!(modal.mutation, Some(ContestSwitchMutation::Repair));
        assert!(modal.error.as_deref().unwrap().contains("repair failed"));
        assert!(modal.progress.iter().any(|progress| matches!(
            progress,
            ContestSwitchProgress::ContestFetching { contest_id } if contest_id == "abc470"
        )));
        assert!(modal.progress.iter().any(|progress| matches!(
            progress,
            ContestSwitchProgress::WorkspaceRepaired { destination }
                if destination == &root.path().join("abc470")
        )));
        assert!(controller.operation.active.is_none());
        assert!(!controller.switch_requested);

        old_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("old-session-after-repair-failure.cpp"),
                language: Language::Cpp,
            })
            .unwrap();
        assert!(handle_messages(&mut old_app, &old_rx, &run_tx).unwrap());
        assert_eq!(
            old_app
                .current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .path,
            PathBuf::from("old-session-after-repair-failure.cpp")
        );
    }

    #[test]
    fn failed_repair_retries_only_an_unchanged_repair_target() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let mut resolve = |_: &str| ContestSwitchResolution::repair_required(destination.clone());
        let (request_tx, request_rx) = mpsc::channel();
        let task: ContestSwitchTask = Arc::new(move |request, _| {
            request_tx.send(request).unwrap();
            Err(io::Error::other("repair failed").into())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        let first = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_for_create_operation(&mut controller);

        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        let retry = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(retry, first);
        assert_eq!(retry.mutation, ContestSwitchMutation::Repair);
        assert_eq!(
            controller.modal().unwrap().state,
            SwitchContestModalState::Repairing
        );
        wait_for_create_operation(&mut controller);
    }

    #[test]
    fn failed_repair_refreshes_changed_health_or_destination_without_acting() {
        for change in ["mapping", "healthy", "missing"] {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("abc123");
            let first = root.path().join("one/abc470");
            let second = root.path().join("two/abc470");
            std::fs::create_dir(&current).unwrap();
            std::fs::create_dir_all(&first).unwrap();
            std::fs::create_dir_all(&second).unwrap();
            let phase = Cell::new(false);
            let mut resolve = |_: &str| {
                if !phase.get() {
                    ContestSwitchResolution::repair_required(first.clone())
                } else {
                    match change {
                        "mapping" => ContestSwitchResolution::repair_required(second.clone()),
                        "healthy" => ContestSwitchResolution::accepted(first.clone()),
                        "missing" => ContestSwitchResolution::missing(first.clone()),
                        _ => unreachable!(),
                    }
                }
            };
            let (request_tx, request_rx) = mpsc::channel();
            let task: ContestSwitchTask = Arc::new(move |request, _| {
                request_tx.send(request).unwrap();
                Err(io::Error::other("repair failed").into())
            });
            let context = workspace_context(root.path());
            let mut controller =
                ContestSwitchController::new(&context, &current, &mut resolve, task);
            controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
            set_displayed_contest_id(&mut controller, "abc470");
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
            request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            wait_for_create_operation(&mut controller);

            phase.set(true);
            controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
            let modal = controller.modal().unwrap();
            assert_eq!(modal.state, SwitchContestModalState::Input, "{change}");
            assert!(modal.error.is_none(), "{change}");
            assert!(modal.progress.is_empty(), "{change}");
            assert!(!controller.switch_requested, "{change}");
            assert!(controller.operation.active.is_none(), "{change}");
            assert!(matches!(
                request_rx.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            match change {
                "mapping" => {
                    assert_eq!(modal.destination, Some(second.clone()));
                    assert_eq!(modal.target, Some(ContestSwitchTarget::RepairRequired));
                }
                "healthy" => {
                    assert_eq!(modal.destination, Some(first.clone()));
                    assert_eq!(modal.target, Some(ContestSwitchTarget::Existing));
                }
                "missing" => {
                    assert_eq!(modal.destination, Some(first.clone()));
                    assert_eq!(modal.target, Some(ContestSwitchTarget::Missing));
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn editing_after_failed_repair_clears_stale_failure_and_re_resolves() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(root.path().join("anc")).unwrap();
        std::fs::create_dir(root.path().join("ancx")).unwrap();
        let mut resolve = |contest_id: &str| {
            ContestSwitchResolution::repair_required(root.path().join(contest_id))
        };
        let context = workspace_context(root.path());
        let mut controller =
            ContestSwitchController::new(&context, &current, &mut resolve, failing_create_task());
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "anc");
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        wait_for_create_operation(&mut controller);

        controller.handle_key(key(KeyCode::Char('x'), KeyEventKind::Press));
        let modal = controller.modal().unwrap();
        assert_eq!(modal.state, SwitchContestModalState::Input);
        assert_eq!(modal.contest_id, "ancx");
        assert_eq!(modal.destination, Some(root.path().join("ancx")));
        assert_eq!(modal.target, Some(ContestSwitchTarget::RepairRequired));
        assert_eq!(modal.error, None);
        assert!(modal.progress.is_empty());
        assert_eq!(modal.mutation, None);

        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        wait_for_create_operation(&mut controller);
        controller.handle_key(key(KeyCode::Backspace, KeyEventKind::Press));
        let modal = controller.modal().unwrap();
        assert_eq!(modal.state, SwitchContestModalState::Input);
        assert_eq!(modal.contest_id, "anc");
        assert_eq!(modal.destination, Some(root.path().join("anc")));
        assert_eq!(modal.error, None);
        assert!(modal.progress.is_empty());
    }

    #[test]
    fn successful_creation_is_joined_before_requesting_the_switch() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        let mut resolve = |_: &str| ContestSwitchResolution::missing(destination.clone());
        let task: ContestSwitchTask = Arc::new(|request, reporter| {
            reporter.report(Event::ContestFetching {
                contest_id: &request.contest_id,
            });
            Ok(())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !controller.switch_requested && std::time::Instant::now() < deadline {
            controller.handle_operation_messages();
            thread::yield_now();
        }

        assert!(controller.switch_requested);
        assert!(controller.operation.active.is_none());
    }

    #[test]
    fn successful_repair_is_joined_before_requesting_the_switch() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let destination = root.path().join("abc470");
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let mut resolve = |_: &str| ContestSwitchResolution::repair_required(destination.clone());
        let task: ContestSwitchTask = Arc::new(|request, reporter| {
            assert_eq!(request.mutation, ContestSwitchMutation::Repair);
            reporter.report(Event::WorkspaceRepaired {
                destination: &request.destination,
            });
            Ok(())
        });
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(&context, &current, &mut resolve, task);
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));
        set_displayed_contest_id(&mut controller, "abc470");
        controller.handle_key(key(KeyCode::Enter, KeyEventKind::Press));

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !controller.switch_requested && std::time::Instant::now() < deadline {
            controller.handle_operation_messages();
            thread::yield_now();
        }

        assert!(controller.switch_requested);
        assert!(controller.operation.active.is_none());
    }

    #[test]
    fn contest_switch_reporter_copies_relevant_borrowed_events() {
        let (tx, rx) = mpsc::channel();
        let mut reporter = ContestSwitchReporter { tx };
        let contest_id = String::from("abc470");
        let problem = String::from("A");
        let warning = String::from("samples unavailable");
        let destination = PathBuf::from("workspace/abc470");

        reporter.report(Event::ContestFetching {
            contest_id: &contest_id,
        });
        reporter.report(Event::ContestFetched {
            contest_id: &contest_id,
            problems: 1,
        });
        reporter.report(Event::ProblemFetching {
            index: &problem,
            current: 1,
            total: 1,
        });
        reporter.report(Event::ProblemFetched {
            index: &problem,
            samples: 3,
        });
        reporter.report(Event::ProblemFetchFailed {
            index: &problem,
            error: &warning,
        });
        reporter.report(Event::WorkspaceCreated {
            destination: &destination,
        });
        reporter.report(Event::WorkspaceRefreshed {
            destination: &destination,
        });
        reporter.report(Event::WorkspaceRepaired {
            destination: &destination,
        });
        drop(contest_id);
        drop(problem);
        drop(warning);
        drop(destination);

        let progress = std::iter::from_fn(|| match rx.try_recv() {
            Ok(ContestSwitchOperationMessage::Progress(progress)) => Some(progress),
            Ok(ContestSwitchOperationMessage::Finished(_)) | Err(_) => None,
        })
        .collect::<Vec<_>>();

        assert_eq!(
            progress,
            [
                ContestSwitchProgress::ContestFetching {
                    contest_id: "abc470".to_string(),
                },
                ContestSwitchProgress::ContestFetched {
                    contest_id: "abc470".to_string(),
                    problems: 1,
                },
                ContestSwitchProgress::ProblemFetching {
                    index: "A".to_string(),
                    current: 1,
                    total: 1,
                },
                ContestSwitchProgress::ProblemFetched {
                    index: "A".to_string(),
                    samples: 3,
                },
                ContestSwitchProgress::ProblemFetchFailed {
                    index: "A".to_string(),
                    error: "samples unavailable".to_string(),
                },
                ContestSwitchProgress::WorkspaceCreated {
                    destination: PathBuf::from("workspace/abc470"),
                },
                ContestSwitchProgress::WorkspaceRefreshed {
                    destination: PathBuf::from("workspace/abc470"),
                },
                ContestSwitchProgress::WorkspaceRepaired {
                    destination: PathBuf::from("workspace/abc470"),
                },
            ]
        );
    }

    #[test]
    fn dropping_frontend_operation_joins_its_worker() {
        let (worker_started_tx, worker_started_rx) = mpsc::channel();
        let (release_worker_tx, release_worker_rx) = mpsc::channel();
        let release_worker_rx = Arc::new(std::sync::Mutex::new(release_worker_rx));
        let task_release = Arc::clone(&release_worker_rx);
        let task: ContestSwitchTask = Arc::new(move |_, _| {
            worker_started_tx.send(()).unwrap();
            task_release.lock().unwrap().recv().unwrap();
            Ok(())
        });
        let mut operation = ContestSwitchOperation::new(task);
        operation
            .start(ContestSwitchRequest {
                mutation: ContestSwitchMutation::Repair,
                contest_id: "abc470".to_string(),
                destination: PathBuf::from("abc470"),
            })
            .unwrap();
        worker_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let (drop_started_tx, drop_started_rx) = mpsc::channel();
        let (drop_finished_tx, drop_finished_rx) = mpsc::channel();
        let drop_thread = thread::spawn(move || {
            drop_started_tx.send(()).unwrap();
            drop(operation);
            drop_finished_tx.send(()).unwrap();
        });
        drop_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        assert!(matches!(
            drop_finished_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        release_worker_tx.send(()).unwrap();
        drop_finished_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        drop_thread.join().unwrap();
    }

    #[test]
    fn modal_suppresses_global_shortcuts_and_queued_q_is_not_global_quit() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("abc123");
        let mut resolve = |_: &str| ContestSwitchResolution::rejected(None, "invalid".to_string());
        let context = workspace_context(root.path());
        let mut controller = ContestSwitchController::new(
            &context,
            &current,
            &mut resolve,
            successful_create_task(),
        );
        let mut app = app();
        controller.handle_key(key(KeyCode::Char('c'), KeyEventKind::Press));

        for code in [KeyCode::Char('q'), KeyCode::Char('d'), KeyCode::Char('j')] {
            assert_ne!(
                controller.handle_key(key(code, KeyEventKind::Press)),
                ContestSwitchKeyResult::NotHandled
            );
        }
        assert!(!app.should_quit());
        assert!(!app.debug_enabled());
        assert_eq!(app.selected_case(), 0);
        assert_eq!(controller.modal().unwrap().contest_id, "qdj");

        let queued = VecDeque::from([
            TerminalEvent::Key(key(KeyCode::Char('c'), KeyEventKind::Press)),
            TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
        ]);
        let mut fresh_resolve =
            |_: &str| ContestSwitchResolution::rejected(None, "invalid".to_string());
        let fresh = ContestSwitchController::new(
            &context,
            &current,
            &mut fresh_resolve,
            successful_create_task(),
        );
        let source = OpenSourceController::new(Path::new("."), Language::Cpp);
        assert!(!contains_global_quit_event(
            &queued,
            &app,
            &fresh,
            &CommandPalette::default(),
            &source,
        ));

        // The modal consumes these keys before the existing application handler can mutate app.
        assert!(!handle_key(
            &mut app,
            KeyCode::Char('x'),
            KeyEventKind::Press
        ));
    }

    #[test]
    fn frontend_preferences_survive_fresh_contest_state() {
        let mut first = app_with_problems(&[3]);
        first.toggle_debug();
        first.toggle_samples_pane();
        first.next_case();
        let mut preferences = FrontendPreferences::default();
        preferences.capture(&first);

        let mut next = app_with_problems(&[1]);
        preferences.apply(&mut next);

        assert!(next.debug_enabled());
        assert!(next.samples_pane_enabled());
        assert_eq!(next.selected_case(), 0);
        assert_eq!(next.contest_id(), "abc123");
    }

    #[test]
    fn old_session_messages_cannot_modify_a_new_session_app() {
        let mut old_app = app_with_problems(&[1]);
        let mut new_app = app_with_problems(&[1]);
        let (old_tx, old_rx) = mpsc::channel();
        let (_new_tx, new_rx) = mpsc::channel();
        let (run_tx, _run_rx) = mpsc::channel();
        old_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("old-session-A.py"),
                language: Language::Python,
            })
            .unwrap();

        assert!(!handle_messages(&mut new_app, &new_rx, &run_tx).unwrap());
        assert!(new_app.current_problem().unwrap().source.is_none());
        assert!(handle_messages(&mut old_app, &old_rx, &run_tx).unwrap());
        assert_eq!(
            old_app
                .current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .path,
            PathBuf::from("old-session-A.py")
        );
    }

    #[test]
    fn press_and_repeat_are_handled_but_release_is_ignored() {
        let mut app = app();

        handle_key(&mut app, KeyCode::Char('j'), KeyEventKind::Press);
        assert_eq!(app.selected_case(), 1);

        handle_key(&mut app, KeyCode::Down, KeyEventKind::Repeat);
        assert_eq!(app.selected_case(), 2);

        handle_key(&mut app, KeyCode::Char('q'), KeyEventKind::Release);
        assert!(!app.should_quit());
        handle_key(&mut app, KeyCode::Char('q'), KeyEventKind::Press);
        assert!(app.should_quit());
    }

    #[test]
    fn queued_quit_is_processed_before_later_events_without_intermediate_draws() {
        let mut app = app();
        let events = RefCell::new(VecDeque::from([
            TerminalEvent::Ignored,
            resize(80, 24),
            TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press)),
            TerminalEvent::Pointer(pointer(PointerKind::ScrollDown, 5, 5)),
        ]));
        let poll_waits = RefCell::new(Vec::new());

        let queued = read_terminal_events_with(
            TERMINAL_POLL_INTERVAL,
            |wait| {
                poll_waits.borrow_mut().push(wait);
                Ok(!events.borrow().is_empty())
            },
            || {
                events
                    .borrow_mut()
                    .pop_front()
                    .ok_or_else(|| io::Error::other("test event queue is empty"))
            },
        )
        .unwrap();

        assert!(contains_quit_event(&queued));
        app.quit();
        assert!(app.should_quit());
        assert_eq!(app.selected_case(), 0);
        assert_eq!(events.borrow().len(), 1);
        assert_eq!(
            poll_waits.into_inner(),
            [TERMINAL_POLL_INTERVAL, Duration::ZERO, Duration::ZERO,]
        );
    }

    #[test]
    fn ignored_events_preserve_the_raw_event_batch_cap_before_quit() {
        let quit = TerminalEvent::Key(key(KeyCode::Char('q'), KeyEventKind::Press));
        let mut source = vec![TerminalEvent::Ignored; MAX_TERMINAL_EVENTS_PER_TICK];
        source.push(quit);
        let events = RefCell::new(VecDeque::from(source));

        let queued = read_terminal_events_with(
            Duration::ZERO,
            |_| Ok(!events.borrow().is_empty()),
            || {
                events
                    .borrow_mut()
                    .pop_front()
                    .ok_or_else(|| io::Error::other("test event queue is empty"))
            },
        )
        .unwrap();

        assert_eq!(queued.len(), MAX_TERMINAL_EVENTS_PER_TICK);
        assert!(queued.iter().all(|event| *event == TerminalEvent::Ignored));
        assert!(!contains_quit_event(&queued));
        assert_eq!(events.borrow().len(), 1);
        assert_eq!(events.borrow().front(), Some(&quit));
    }

    #[test]
    fn leading_resizes_only_coalesce_across_contiguous_raw_events() {
        let mut events = VecDeque::from([
            resize(80, 24),
            resize(120, 40),
            TerminalEvent::Ignored,
            resize(160, 50),
            TerminalEvent::Key(key(KeyCode::Char('j'), KeyEventKind::Press)),
        ]);

        assert!(take_leading_resizes(&mut events));
        assert_eq!(
            events,
            VecDeque::from([
                TerminalEvent::Ignored,
                resize(160, 50),
                TerminalEvent::Key(key(KeyCode::Char('j'), KeyEventKind::Press)),
            ])
        );
    }

    #[test]
    fn quit_priority_requires_lowercase_press_but_ignores_modifiers() {
        let modified_q = TerminalEvent::Key(KeyEvent {
            code: KeyCode::Char('q'),
            kind: KeyEventKind::Press,
            modifiers: terminal::Modifiers {
                control: true,
                alt: true,
                ..terminal::Modifiers::default()
            },
        });

        assert!(is_quit_event(&modified_q));
        assert!(!is_quit_event(&TerminalEvent::Key(key(
            KeyCode::Char('Q'),
            KeyEventKind::Press,
        ))));
        assert!(!is_quit_event(&TerminalEvent::Key(key(
            KeyCode::Char('q'),
            KeyEventKind::Repeat,
        ))));
        assert!(!is_quit_event(&TerminalEvent::Key(key(
            KeyCode::Char('q'),
            KeyEventKind::Release,
        ))));
    }

    #[test]
    fn mouse_after_resize_waits_for_new_render_info() {
        let mut app = app();
        let (run_tx, _run_rx) = mpsc::channel();

        let old_info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        let mut events = VecDeque::from([
            resize(100, 40),
            TerminalEvent::Pointer(pointer(PointerKind::ScrollDown, 5, 5)),
        ]);

        assert!(handle_terminal_events(&mut app, &old_info, &mut events, &run_tx,).unwrap());

        assert_eq!(app.selected_case(), 0);
        assert_eq!(events.len(), 1);

        let new_info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 100, 40),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        assert!(handle_terminal_events(&mut app, &new_info, &mut events, &run_tx,).unwrap());

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 3);
    }

    #[test]
    fn repeat_does_not_toggle_debug_repeatedly() {
        let mut app = app();

        handle_key(&mut app, KeyCode::Char('d'), KeyEventKind::Press);
        assert!(app.debug_enabled());
        handle_key(&mut app, KeyCode::Char('d'), KeyEventKind::Repeat);
        assert!(app.debug_enabled());
    }

    #[test]
    fn up_and_k_move_to_the_previous_case() {
        let mut app = app();

        handle_key(&mut app, KeyCode::Up, KeyEventKind::Press);
        assert_eq!(app.selected_case(), 2);
        handle_key(&mut app, KeyCode::Char('k'), KeyEventKind::Press);
        assert_eq!(app.selected_case(), 1);
    }

    #[test]
    fn unknown_and_no_op_navigation_keys_are_not_dirty() {
        let mut app = app_with_problems(&[1]);

        assert!(!handle_key(
            &mut app,
            KeyCode::Char('x'),
            KeyEventKind::Press
        ));
        assert!(!handle_key(&mut app, KeyCode::Right, KeyEventKind::Press));
        assert!(!handle_key(&mut app, KeyCode::Down, KeyEventKind::Press));
    }

    #[test]
    fn source_messages_update_state_and_multiple_messages_use_the_latest_source() {
        let mut app = app_with_problems(&[3, 2]);
        app.previous_case();
        let (tx, rx) = mpsc::channel();
        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.cpp"),
            language: Language::Cpp,
        })
        .unwrap();
        tx.send(Message::SourceChanged {
            problem: 1,
            path: PathBuf::from("B.cpp"),
            language: Language::Cpp,
        })
        .unwrap();
        tx.send(Message::SourceChanged {
            problem: 1,
            path: PathBuf::from("B.py"),
            language: Language::Python,
        })
        .unwrap();

        let (run_tx, _run_rx) = mpsc::channel();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        let problem = app.current_problem().unwrap();
        assert_eq!(problem.index, "B");
        assert_eq!(app.selected_case(), 0);
        let source = problem.source.as_ref().unwrap();
        assert_eq!(source.path, Path::new("B.py"));
        assert_eq!(source.language, Language::Python);
    }

    #[test]
    fn message_drain_is_bounded_to_keep_input_responsive() {
        let mut app = app();

        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        // 最初の1tickで処理できる上限いっぱいまでC++の変更を積む
        for _ in 0..MAX_MESSAGES_PER_TICK {
            tx.send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("A.cpp"),
                language: Language::Cpp,
            })
            .unwrap();
        }

        // 257件目。これは最初のhandle_messagesでは処理されないはず
        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.py"),
            language: Language::Python,
        })
        .unwrap();

        // 1tick目
        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        assert_eq!(
            app.current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .language,
            Language::Cpp
        );

        // 256件だけRunRequestが作られている
        let first_requests = received_runs(&run_rx);

        assert_eq!(first_requests.len(), MAX_MESSAGES_PER_TICK);
        assert_eq!(first_requests[0].run_id, 1);
        assert_eq!(
            first_requests.last().unwrap().run_id,
            MAX_MESSAGES_PER_TICK as u64
        );

        // 2tick目で残っていたA.pyを処理
        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        assert_eq!(
            app.current_problem()
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .language,
            Language::Python
        );

        let second_requests = received_runs(&run_rx);

        assert_eq!(second_requests.len(), 1);
        assert_eq!(second_requests[0].run_id, MAX_MESSAGES_PER_TICK as u64 + 1);
        assert_eq!(second_requests[0].problem, 0);
        assert_eq!(second_requests[0].language, Language::Python);
    }

    #[test]
    fn watcher_failure_and_disconnected_channel_are_errors() {
        let mut app = app();

        let (tx, rx) = mpsc::channel();
        let (run_tx, _run_rx) = mpsc::channel();

        tx.send(Message::WatcherFailed(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "watch failed",
        )))
        .unwrap();

        let error = handle_messages(&mut app, &rx, &run_tx).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "watch failed");

        let (tx, rx) = mpsc::channel();
        tx.send(Message::WorkerFailed(io::Error::other("worker panicked")))
            .unwrap();
        let (run_tx, _run_rx) = mpsc::channel();

        let error = handle_messages(&mut app, &rx, &run_tx).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "worker panicked");

        // background側の全Senderが消えた場合
        let (tx, rx) = mpsc::channel::<Message>();
        drop(tx);

        let (run_tx, _run_rx) = mpsc::channel();

        let error = handle_messages(&mut app, &rx, &run_tx).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "background message channel disconnected");
    }

    #[test]
    fn detail_analysis_results_update_layout_and_disconnection_is_an_error() {
        let raw = "line\n".repeat(3_000);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);
        let mut layout = detail_layout::DetailLayout::default();
        let initial = layout.viewport(&document, 5, 80, 20, 0);
        assert_eq!(initial.max_scroll, None);
        layout.complete_structure_for_test(&document);
        layout.stage_analysis_command(&document);
        let Some(detail_layout::DetailAnalysisCommand::Count(request)) =
            layout.take_analysis_command()
        else {
            panic!("large detail must request background counting");
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
        let result = detail_layout::DetailAnalysisResult::Count(detail_layout::DetailCountResult {
            identity: request.identity,
            exact_layout_index: count.exact_layout_index,
            anchor: request.anchor,
            anchor_visual_row: count.anchor_visual_row,
            anchor_row_raw_start: count.anchor_row_raw_start,
        });
        let (tx, rx) = mpsc::channel();
        for _ in 0..=MAX_DETAIL_ANALYSIS_RESULTS_PER_TICK {
            tx.send(result.clone()).unwrap();
        }

        assert!(handle_detail_analysis_results(&mut layout, 5, &rx).unwrap());
        assert!(rx.try_recv().is_ok(), "result draining must stay bounded");
        assert_eq!(
            layout.viewport(&document, 5, 80, 20, 0).max_scroll,
            Some(2_981)
        );

        drop(tx);
        let error = handle_detail_analysis_results(&mut layout, 5, &rx).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            error.to_string(),
            "detail analysis worker result channel disconnected"
        );
    }

    #[test]
    fn detail_scroll_reconciliation_crosses_layout_app_boundary_once() {
        let raw = "a".repeat(100);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);
        let mut layout = detail_layout::DetailLayout::default();

        layout.viewport(&document, 1, 6, 2, 5);
        let anchored = layout.viewport(&document, 1, 11, 2, 5);
        let mut app = app_with_problems(&[1]);
        app.scroll_detail_down(5);

        assert!(apply_detail_scroll_reconciliation(&mut app, &mut layout));
        assert_eq!(app.detail_scroll(), anchored.effective_scroll);
        assert!(!apply_detail_scroll_reconciliation(&mut app, &mut layout));
    }

    #[test]
    fn mouse_scroll_cancels_pending_width_reconciliation_without_snap_back() {
        let raw = "long normal detail line\n".repeat(4_000);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);
        let mut layout = detail_layout::DetailLayout::default();

        layout.viewport(&document, 2, 100, 20, 0);
        layout.complete_structure_for_test(&document);
        layout.stage_analysis_command(&document);
        let Some(detail_layout::DetailAnalysisCommand::Count(initial_request)) =
            layout.take_analysis_command()
        else {
            panic!("completed lazy detail must stage its initial count");
        };
        let mut never_cancel = || false;
        let initial = initial_request
            .structure
            .count_chunks(
                &initial_request.snapshot,
                initial_request.identity.layout_width,
                initial_request.anchor,
                &mut never_cancel,
            )
            .unwrap();
        assert!(layout.apply_count_result(detail_layout::DetailCountResult {
            identity: initial_request.identity,
            exact_layout_index: initial.exact_layout_index,
            anchor: initial_request.anchor,
            anchor_visual_row: initial.anchor_visual_row,
            anchor_row_raw_start: initial.anchor_row_raw_start,
        }));

        let baseline_scroll = 500;
        layout.viewport(&document, 2, 100, 20, baseline_scroll);
        layout.viewport(&document, 2, 70, 20, baseline_scroll);
        layout.stage_analysis_command(&document);
        let Some(detail_layout::DetailAnalysisCommand::Count(anchored_request)) =
            layout.take_analysis_command()
        else {
            panic!("width transition must stage an anchored count");
        };
        assert!(anchored_request.anchor.is_some());
        let delayed = anchored_request
            .structure
            .count_chunks(
                &anchored_request.snapshot,
                anchored_request.identity.layout_width,
                anchored_request.anchor,
                &mut never_cancel,
            )
            .unwrap();

        let mut app = app_with_problems(&[1]);
        app.scroll_detail_down(baseline_scroll);
        let info = view::RenderInfo {
            max_detail_scroll: None,
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 70, 20),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };
        assert!(super::handle_pointer_event(
            &mut app,
            &mut layout,
            &mut DetailScrollbarDragState::default(),
            pointer(PointerKind::ScrollDown, 30, 5),
            &info,
        ));
        let user_scroll = baseline_scroll + DETAIL_SCROLL_LINES;
        assert_eq!(app.detail_scroll(), user_scroll);

        layout.viewport(&document, 2, 70, 20, app.detail_scroll());
        assert!(layout.apply_count_result(detail_layout::DetailCountResult {
            identity: anchored_request.identity,
            exact_layout_index: delayed.exact_layout_index,
            anchor: anchored_request.anchor,
            anchor_visual_row: delayed.anchor_visual_row,
            anchor_row_raw_start: delayed.anchor_row_raw_start,
        }));
        assert!(!apply_detail_scroll_reconciliation(&mut app, &mut layout));
        assert_eq!(app.detail_scroll(), user_scroll);
    }

    #[test]
    fn stale_structure_result_is_rejected_before_layout_prepares_the_new_document() {
        let raw = "line\n".repeat(100_000);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);
        let mut layout = detail_layout::DetailLayout::default();
        layout.viewport(&document, 5, 80, 20, 0);
        layout.stage_analysis_command(&document);
        let Some(detail_layout::DetailAnalysisCommand::BuildStructure(request)) =
            layout.take_analysis_command()
        else {
            panic!("large detail must request background structure discovery");
        };
        let structure =
            detail_layout::build_document_structure_cancellable(&request.snapshot, || false)
                .unwrap();
        let result = detail_layout::DetailAnalysisResult::StructureReady(
            detail_layout::DetailStructureResult {
                identity: request.identity,
                structure,
            },
        );
        let (tx, rx) = mpsc::channel();
        tx.send(result).unwrap();

        assert!(!handle_detail_analysis_results(&mut layout, 6, &rx).unwrap());
        assert_eq!(layout.viewport(&document, 5, 80, 20, 0).max_scroll, None);
    }

    #[test]
    fn key_and_source_message_processing_coexist() {
        let mut app = app_with_problems(&[3, 2]);

        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();
        assert!(
            handle_key_event(&mut app, key(KeyCode::Down, KeyEventKind::Press), &run_tx,).unwrap()
        );
        assert_eq!(app.selected_case(), 1);

        tx.send(Message::SourceChanged {
            problem: 1,
            path: PathBuf::from("B.py"),
            language: Language::Python,
        })
        .unwrap();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        assert_eq!(app.current_problem().unwrap().index, "B");
        assert_eq!(app.selected_case(), 0);

        // source変更からworkerへのRunRequestも作られている
        let request = received_run(&run_rx);

        assert_eq!(request.run_id, 1);
        assert_eq!(request.problem, 1);
        assert_eq!(request.language, Language::Python);
        assert!(!request.debug);

        // Message処理後もkeyboard操作できる
        assert!(
            handle_key_event(&mut app, key(KeyCode::Down, KeyEventKind::Press), &run_tx,).unwrap()
        );
        assert_eq!(app.selected_case(), 1);
    }

    #[test]
    fn stale_run_messages_in_the_same_drain_cannot_overwrite_a_newer_request() {
        let mut app = app();
        let (tx, rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.cpp"),
            language: Language::Cpp,
        })
        .unwrap();
        tx.send(Message::RunStarted {
            run_id: 1,
            problem: 0,
        })
        .unwrap();
        tx.send(Message::SourceChanged {
            problem: 0,
            path: PathBuf::from("A.py"),
            language: Language::Python,
        })
        .unwrap();
        tx.send(Message::RunEvent {
            run_id: 1,
            problem: 0,
            event: message::TestEvent::TestRunFinished {
                accepted: 3,
                total_cases: 3,
            },
        })
        .unwrap();
        tx.send(Message::RunCompleted {
            run_id: 1,
            problem: 0,
        })
        .unwrap();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());

        let requests = received_runs(&run_rx);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].run_id, 1);
        assert_eq!(requests[1].run_id, 2);
        assert_eq!(requests[1].language, Language::Python);

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.id, Some(2));
        assert_eq!(run.phase, app::RunPhase::Queued);
        assert_eq!(run.language, Some(Language::Python));
        assert_eq!(run.accepted, 0);
    }

    #[test]
    fn run_requeued_message_updates_state_and_only_marks_real_changes_dirty() {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));

        let (tx, rx) = mpsc::channel();
        let (run_tx, _run_rx) = mpsc::channel();
        tx.send(Message::RunRequeued {
            run_id: request.run_id,
            problem: 0,
        })
        .unwrap();

        assert!(handle_messages(&mut app, &rx, &run_tx).unwrap());
        assert_eq!(
            app.current_problem().unwrap().run.phase,
            app::RunPhase::Queued
        );

        let current = app.queue_run(0).unwrap();
        tx.send(Message::RunRequeued {
            run_id: request.run_id,
            problem: 0,
        })
        .unwrap();

        assert!(!handle_messages(&mut app, &rx, &run_tx).unwrap());
        assert_eq!(app.current_problem().unwrap().run.id, Some(current.run_id));
        assert_eq!(
            app.current_problem().unwrap().run.phase,
            app::RunPhase::Queued
        );
    }
    #[test]
    fn samples_pane_toggles_only_on_key_press() {
        let mut app = app();

        assert!(!app.samples_pane_enabled());

        assert!(handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyEventKind::Press
        ),);
        assert!(app.samples_pane_enabled());

        assert!(!handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyEventKind::Repeat
        ),);
        assert!(app.samples_pane_enabled());

        assert!(handle_key(
            &mut app,
            KeyCode::Char('s'),
            KeyEventKind::Press
        ),);
        assert!(!app.samples_pane_enabled());
    }
    fn pointer(kind: PointerKind, column: u16, row: u16) -> PointerEvent {
        PointerEvent {
            kind,
            position: terminal::PointerPosition::Cells { column, row },
            modifiers: terminal::Modifiers::default(),
            pixel_generation: None,
        }
    }

    fn pixel_metrics() -> TerminalPixelMetrics {
        TerminalPixelMetrics::validated(100, 40, 1_000, 800, 10, 20).unwrap()
    }

    fn pixel_mode(generation: u64) -> MouseMode {
        MouseMode::Pixels {
            metrics: pixel_metrics(),
            origin: mouse::PixelCoordinateOrigin::ZeroBased,
            generation,
        }
    }

    fn pixel_pointer(kind: PointerKind, x: u32, y: u32, generation: Option<u64>) -> PointerEvent {
        PointerEvent {
            kind,
            position: terminal::PointerPosition::AbsolutePixels { x, y },
            modifiers: terminal::Modifiers::default(),
            pixel_generation: generation,
        }
    }
    fn handle_pointer_event(
        app: &mut WatchApp,
        pointer: PointerEvent,
        render_info: &view::RenderInfo,
    ) -> bool {
        let mut detail_layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        super::handle_pointer_event(app, &mut detail_layout, &mut drag, pointer, render_info)
    }

    fn scrollbar_info(
        app: &WatchApp,
        max_scroll: usize,
        scroll: usize,
        layout_generation: u64,
    ) -> view::RenderInfo {
        scrollbar_info_with_pixels(app, max_scroll, scroll, layout_generation, None)
    }

    fn scrollbar_info_with_pixels(
        app: &WatchApp,
        max_scroll: usize,
        scroll: usize,
        layout_generation: u64,
        pixels: Option<(u32, u64)>,
    ) -> view::RenderInfo {
        let detail_area = ratatui::layout::Rect::new(20, 5, 40, 20);
        let geometry = detail_scrollbar::DetailScrollbarGeometry::new(
            detail_area,
            max_scroll,
            scroll,
            usize::from(detail_area.height),
            &[],
        )
        .unwrap();
        let pixel_geometry = pixels.and_then(|(cell_height_px, generation)| {
            detail_scrollbar::DetailScrollbarPixelGeometry::new(
                &geometry,
                cell_height_px,
                generation,
            )
        });
        let interaction = detail_scrollbar::DetailScrollbarInteraction::new(
            detail_layout::DetailExactLayoutIdentity {
                document_generation: 1,
                layout_generation,
                revision: app.detail_revision(),
            },
            geometry,
            pixel_geometry,
        )
        .unwrap();
        view::RenderInfo {
            max_detail_scroll: Some(max_scroll),
            samples_area: None,
            detail_area,
            detail_scrollbar: Some(interaction),
            detail_section_headers: Vec::new(),
        }
    }

    fn dispatch_mouse(
        app: &mut WatchApp,
        layout: &mut detail_layout::DetailLayout,
        drag: &mut DetailScrollbarDragState,
        info: &view::RenderInfo,
        kind: PointerKind,
        column: u16,
        row: u16,
    ) -> bool {
        super::handle_pointer_event(app, layout, drag, pointer(kind, column, row), info)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "test call sites name every raw pixel input needed by each scenario"
    )]
    fn dispatch_pixel(
        app: &mut WatchApp,
        layout: &mut detail_layout::DetailLayout,
        drag: &mut DetailScrollbarDragState,
        info: &view::RenderInfo,
        mode: MouseMode,
        kind: PointerKind,
        x: u32,
        y: u32,
        generation: Option<u64>,
    ) -> bool {
        super::handle_pointer_event_with_mouse_mode(
            app,
            layout,
            drag,
            pixel_pointer(kind, x, y, generation),
            info,
            mode,
        )
    }

    #[test]
    fn left_down_toggles_each_visible_semantic_header_and_drag_does_not_repeat() {
        for kind in [
            detail::DetailSectionKind::Input,
            detail::DetailSectionKind::Expected,
            detail::DetailSectionKind::Actual,
            detail::DetailSectionKind::Stderr,
        ] {
            let mut app = foldable_app("actual body".to_string());
            let info = rendered_fold_info(&app, 100, 40);
            let target = *info
                .detail_section_headers
                .iter()
                .find(|target| target.kind == kind)
                .unwrap();
            let mut layout = detail_layout::DetailLayout::default();
            let mut drag = DetailScrollbarDragState::default();

            assert!(dispatch_mouse(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                PointerKind::Down(PointerButton::Left),
                target.area.x.saturating_add(1),
                target.area.y,
            ));
            assert!(app.detail_fold_state().is_collapsed(kind));

            assert!(!dispatch_mouse(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                PointerKind::Drag(PointerButton::Left),
                target.area.x.saturating_add(1),
                target.area.y,
            ));
            assert!(app.detail_fold_state().is_collapsed(kind));
        }
    }

    #[test]
    fn body_non_left_and_wheel_events_do_not_toggle_fold_headers() {
        let mut app = foldable_app("actual body".to_string());
        let info = rendered_fold_info(&app, 100, 40);
        let target = *info
            .detail_section_headers
            .iter()
            .find(|target| target.kind == detail::DetailSectionKind::Input)
            .unwrap();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let column = target.area.x.saturating_add(1);

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            PointerKind::Down(PointerButton::Left),
            column,
            target.area.y.saturating_add(1),
        ));
        for kind in [
            PointerKind::Down(PointerButton::Right),
            PointerKind::Down(PointerButton::Middle),
            PointerKind::ScrollUp,
            PointerKind::ScrollDown,
        ] {
            dispatch_mouse(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                kind,
                column,
                target.area.y,
            );
        }
        assert!(
            !app.detail_fold_state()
                .is_collapsed(detail::DetailSectionKind::Input)
        );
    }

    #[test]
    fn stale_fold_header_targets_cannot_toggle_after_a_detail_revision_change() {
        let mut app = foldable_app("actual body".to_string());
        let info = rendered_fold_info(&app, 100, 40);
        let actual = *info
            .detail_section_headers
            .iter()
            .find(|target| target.kind == detail::DetailSectionKind::Actual)
            .unwrap();
        let expected = *info
            .detail_section_headers
            .iter()
            .find(|target| target.kind == detail::DetailSectionKind::Expected)
            .unwrap();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();

        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            PointerKind::Down(PointerButton::Left),
            actual.area.x.saturating_add(1),
            actual.area.y,
        ));
        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            PointerKind::Down(PointerButton::Left),
            expected.area.x.saturating_add(1),
            expected.area.y,
        ));
        assert!(
            app.detail_fold_state()
                .is_collapsed(detail::DetailSectionKind::Actual)
        );
        assert!(
            !app.detail_fold_state()
                .is_collapsed(detail::DetailSectionKind::Expected)
        );
    }

    #[test]
    fn scrollbar_gutter_has_priority_over_fold_headers_in_cells_and_pixels() {
        for pixels in [false, true] {
            let mut app = foldable_app("actual body\n".repeat(1_000));
            let info = rendered_fold_info(&app, 100, 40);
            let target = *info
                .detail_section_headers
                .iter()
                .find(|target| target.kind == detail::DetailSectionKind::Actual)
                .unwrap();
            let scrollbar = info.detail_scrollbar.as_ref().unwrap();
            let gutter = scrollbar.geometry.gutter.x;
            assert!(scrollbar.geometry.hit_test(gutter, target.area.y).is_some());
            let mut layout = detail_layout::DetailLayout::default();
            let mut drag = DetailScrollbarDragState::default();

            if pixels {
                dispatch_pixel(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    pixel_mode(7),
                    PointerKind::Down(PointerButton::Left),
                    u32::from(gutter) * 10 + 5,
                    u32::from(target.area.y) * 20 + 10,
                    Some(7),
                );
            } else {
                dispatch_mouse(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    PointerKind::Down(PointerButton::Left),
                    gutter,
                    target.area.y,
                );
            }
            assert!(
                !app.detail_fold_state()
                    .is_collapsed(detail::DetailSectionKind::Actual)
            );

            let mut app = foldable_app("actual body\n".repeat(1_000));
            let info = rendered_fold_info(&app, 100, 40);
            let target = *info
                .detail_section_headers
                .iter()
                .find(|target| target.kind == detail::DetailSectionKind::Actual)
                .unwrap();
            let mut layout = detail_layout::DetailLayout::default();
            let mut drag = DetailScrollbarDragState::default();
            if pixels {
                assert!(dispatch_pixel(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    pixel_mode(7),
                    PointerKind::Down(PointerButton::Left),
                    u32::from(target.area.x.saturating_add(1)) * 10 + 5,
                    u32::from(target.area.y) * 20 + 10,
                    Some(7),
                ));
            } else {
                assert!(dispatch_mouse(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    PointerKind::Down(PointerButton::Left),
                    target.area.x.saturating_add(1),
                    target.area.y,
                ));
            }
            assert!(
                app.detail_fold_state()
                    .is_collapsed(detail::DetailSectionKind::Actual)
            );
        }
    }

    fn pending_width_layout(
        document: &detail::DetailDocument<'_>,
    ) -> (
        detail_layout::DetailLayout,
        detail_layout::DetailCountResult,
    ) {
        let mut layout = detail_layout::DetailLayout::default();
        layout.viewport(document, 2, 100, 20, 0);
        layout.complete_structure_for_test(document);
        let initial = count_request_from_layout(&mut layout, document);
        assert!(layout.apply_count_result(real_count_result(initial)));
        layout.viewport(document, 2, 100, 20, 500);
        layout.viewport(document, 2, 70, 20, 500);
        let delayed = real_count_result(count_request_from_layout(&mut layout, document));
        assert!(layout.has_pending_width_anchor_for_test());
        (layout, delayed)
    }

    fn count_request_from_layout(
        layout: &mut detail_layout::DetailLayout,
        document: &detail::DetailDocument<'_>,
    ) -> detail_layout::DetailCountRequest {
        layout.stage_analysis_command(document);
        let Some(detail_layout::DetailAnalysisCommand::Count(request)) =
            layout.take_analysis_command()
        else {
            panic!("expected exact Detail count request");
        };
        request
    }

    fn real_count_result(
        request: detail_layout::DetailCountRequest,
    ) -> detail_layout::DetailCountResult {
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
        detail_layout::DetailCountResult {
            identity: request.identity,
            exact_layout_index: count.exact_layout_index,
            anchor,
            anchor_visual_row: count.anchor_visual_row,
            anchor_row_raw_start: count.anchor_row_raw_start,
        }
    }
    #[test]
    fn mouse_wheel_over_samples_changes_sample() {
        let mut app = app();

        app.scroll_detail_down(10);

        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        assert!(handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 5, 5),
            &info,
        ));

        assert_eq!(app.selected_case(), 1);
        assert_eq!(app.detail_scroll(), 0);
    }
    #[test]
    fn mouse_wheel_over_detail_scrolls_detail() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        assert!(handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 30, 5),
            &info,
        ));

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 3);
    }

    #[test]
    fn mouse_wheel_does_not_clamp_while_lazy_max_is_unknown() {
        let mut app = app();
        let info = view::RenderInfo {
            max_detail_scroll: None,
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 60, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        assert!(handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 30, 5),
            &info,
        ));
        assert_eq!(app.detail_scroll(), DETAIL_SCROLL_LINES);
    }
    #[test]
    fn mouse_wheel_at_detail_bottom_is_not_dirty() {
        let mut app = app();

        app.scroll_detail_down(10);

        let info = view::RenderInfo {
            max_detail_scroll: Some(10),
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 60, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        assert!(!handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 30, 5),
            &info,
        ));

        assert_eq!(app.detail_scroll(), 10);
    }

    #[test]
    fn mouse_wheel_at_detail_top_is_not_dirty() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: Some(10),
            samples_area: None,
            detail_area: ratatui::layout::Rect::new(0, 0, 60, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        assert!(!handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollUp, 30, 5),
            &info,
        ));

        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn samples_and_detail_rect_boundary_is_half_open() {
        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        let mut samples_app = app();
        assert!(handle_pointer_event(
            &mut samples_app,
            pointer(PointerKind::ScrollDown, 19, 5),
            &info,
        ));
        assert_eq!(samples_app.selected_case(), 1);
        assert_eq!(samples_app.detail_scroll(), 0);

        let mut detail_app = app();
        assert!(handle_pointer_event(
            &mut detail_app,
            pointer(PointerKind::ScrollDown, 20, 5),
            &info,
        ));
        assert_eq!(detail_app.selected_case(), 0);
        assert_eq!(detail_app.detail_scroll(), 3);
    }

    #[test]
    fn samples_wheel_with_one_case_does_not_mark_ui_dirty() {
        let mut app = app_with_problems(&[1]);
        let info = view::RenderInfo {
            max_detail_scroll: Some(0),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        assert!(!handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 5, 5),
            &info,
        ));
        assert_eq!(app.selected_case(), 0);
    }
    #[test]
    fn mouse_wheel_outside_content_is_ignored() {
        let mut app = app();

        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 5, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 5, 40, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };

        assert!(!handle_pointer_event(
            &mut app,
            pointer(PointerKind::ScrollDown, 5, 1),
            &info,
        ));

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn absolute_pixel_pointer_positions_are_not_projected_as_cells() {
        let mut app = app();
        let info = view::RenderInfo {
            max_detail_scroll: Some(20),
            samples_area: Some(ratatui::layout::Rect::new(0, 0, 20, 10)),
            detail_area: ratatui::layout::Rect::new(20, 0, 40, 10),
            detail_scrollbar: None,
            detail_section_headers: Vec::new(),
        };
        let pointer = PointerEvent {
            kind: PointerKind::ScrollDown,
            position: terminal::PointerPosition::AbsolutePixels { x: 30, y: 5 },
            modifiers: terminal::Modifiers::default(),
            pixel_generation: None,
        };

        assert!(!handle_pointer_event(&mut app, pointer, &info));
        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn thumb_down_starts_drag_and_track_down_seeks_without_dragging() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        ));
        assert!(drag.active.is_some());

        let track_row = geometry.track_end_row().saturating_sub(1);
        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            track_row,
        ));
        assert!(app.detail_scroll() > 0);
        assert!(drag.active.is_none());
    }

    #[test]
    fn cap_clicks_seek_exact_endpoints() {
        let mut app = app();
        app.set_detail_scroll_from_user(50);
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 50, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;

        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.top_cap_row.unwrap(),
        ));
        assert_eq!(app.detail_scroll(), 0);
        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.bottom_cap_row.unwrap(),
        ));
        assert_eq!(app.detail_scroll(), 100);
    }

    #[test]
    fn drag_preserves_grab_offset_and_continues_outside_the_gutter_and_pane() {
        let mut app = app();
        app.set_detail_scroll_from_user(5);
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 10, 5, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        assert!(geometry.thumb_len > 1);
        let grab_offset = geometry.thumb_len - 1;
        let pointer_row = geometry.thumb_start_row + grab_offset;

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            pointer_row,
        ));
        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Drag(MouseButton::Left),
            0,
            pointer_row,
        ));
        assert_eq!(app.detail_scroll(), 5);

        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Drag(MouseButton::Left),
            0,
            u16::MAX,
        ));
        assert_eq!(app.detail_scroll(), 10);
        assert!(dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Drag(MouseButton::Left),
            u16::MAX,
            0,
        ));
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn pixel_thumb_drag_preserves_sub_cell_grab_offset_and_adjacent_pixels() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 1_000_000, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let mode = pixel_mode(7);
        let x = u32::from(geometry.gutter.x) * 10 + 5;
        let thumb_top = u32::from(geometry.thumb_start_row) * 20;
        let grab_offset_px = 13;

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Down(PointerButton::Left),
            x,
            thumb_top + grab_offset_px,
            Some(7),
        ));
        assert!(matches!(
            drag.active,
            Some(DetailScrollbarDrag {
                coordinate: DragCoordinate::Pixels {
                    grab_offset_px: 13,
                    generation: 7,
                },
                ..
            })
        ));

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            0,
            thumb_top + grab_offset_px,
            Some(7),
        ));
        assert_eq!(app.detail_scroll(), 0);

        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            0,
            thumb_top + grab_offset_px + 1,
            Some(7),
        ));
        let first = app.detail_scroll();
        assert!(first > 0);
        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            0,
            thumb_top + grab_offset_px + 2,
            Some(7),
        ));
        assert!(app.detail_scroll() > first);

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Up(PointerButton::Left),
            u32::MAX,
            u32::MAX,
            None,
        ));
        assert!(drag.active.is_none());
    }

    #[test]
    fn fractional_pixel_down_hit_and_grab_use_the_published_exact_interval() {
        let maximum = 1_000;
        let generation = 31;
        let representable_scroll = (1..maximum)
            .find(|scroll| {
                let info =
                    scrollbar_info_with_pixels(&app(), maximum, *scroll, 1, Some((20, generation)));
                let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
                let projection = geometry.pixel_projection(20).unwrap();
                !projection.thumb_top_px().is_multiple_of(20)
                    && geometry.scroll_for_pixel_drag(
                        u32::try_from(projection.thumb_top_px()).unwrap(),
                        0,
                        20,
                    ) == *scroll
            })
            .unwrap();

        let mut app = app();
        app.set_detail_scroll_from_user(representable_scroll);
        let info = scrollbar_info_with_pixels(
            &app,
            maximum,
            representable_scroll,
            1,
            Some((20, generation)),
        );
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let projection = geometry.pixel_projection(20).unwrap();
        let gutter_x = u32::from(geometry.gutter.x) * 10 + 5;
        let top = u32::try_from(projection.thumb_top_px()).unwrap();
        let bottom = u32::try_from(projection.thumb_bottom_px()).unwrap();
        let top_row = u16::try_from(top / 20).unwrap();
        assert_eq!(top_row, u16::try_from((top - 1) / 20).unwrap());

        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(generation),
            PointerKind::Down(PointerButton::Left),
            gutter_x,
            top + 3,
            Some(generation),
        ));
        assert!(matches!(
            drag.active,
            Some(DetailScrollbarDrag {
                coordinate: DragCoordinate::Pixels {
                    grab_offset_px: 3,
                    generation: 31,
                },
                ..
            })
        ));

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(generation),
            PointerKind::Drag(PointerButton::Left),
            0,
            top + 3,
            Some(generation),
        ));
        assert_eq!(app.detail_scroll(), representable_scroll);

        dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(generation),
            PointerKind::Up(PointerButton::Left),
            gutter_x,
            top,
            Some(generation),
        );
        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(generation),
            PointerKind::Down(PointerButton::Left),
            gutter_x,
            bottom - 1,
            Some(generation),
        ));
        assert!(matches!(
            drag.active,
            Some(DetailScrollbarDrag {
                coordinate: DragCoordinate::Pixels { grab_offset_px, .. },
                ..
            }) if grab_offset_px == u64::from(bottom - top - 1)
        ));

        dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(generation),
            PointerKind::Up(PointerButton::Left),
            gutter_x,
            top,
            Some(generation),
        );
        dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(generation),
            PointerKind::Down(PointerButton::Left),
            gutter_x,
            top - 1,
            Some(generation),
        );
        assert!(drag.active.is_none());

        dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(generation + 1),
            PointerKind::Down(PointerButton::Left),
            gutter_x,
            top,
            Some(generation + 1),
        );
        assert!(drag.active.is_none());
    }

    #[test]
    fn cells_thumb_hit_remains_whole_cell_based_even_when_pixel_geometry_exists() {
        let mut app = app();
        app.set_detail_scroll_from_user(150);
        let info = scrollbar_info_with_pixels(&app, 10_000, 150, 1, Some((20, 5)));
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            PointerKind::Down(PointerButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        ));
        assert!(matches!(
            drag.active,
            Some(DetailScrollbarDrag {
                coordinate: DragCoordinate::Cells { grab_offset: 0 },
                ..
            })
        ));
    }

    #[test]
    fn pixel_drag_clamps_large_ranges_and_rejects_invalid_or_stale_mapping() {
        let mut app = app();
        app.set_detail_scroll_from_user(usize::MAX / 2);
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, usize::MAX, usize::MAX / 2, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let mode = pixel_mode(11);
        let x = u32::from(geometry.gutter.x) * 10 + 5;
        let thumb_y = u32::from(geometry.thumb_start_row) * 20 + 7;

        dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Down(PointerButton::Left),
            x,
            thumb_y,
            Some(11),
        );
        assert!(drag.active.is_some());

        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            x,
            0,
            Some(11),
        ));
        assert_eq!(app.detail_scroll(), 0);
        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Drag(PointerButton::Left),
            x,
            799,
            Some(11),
        ));
        assert_eq!(app.detail_scroll(), usize::MAX);

        let unchanged = app.detail_scroll();
        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseMode::Disabled,
            PointerKind::Drag(PointerButton::Left),
            x,
            200,
            None,
        ));
        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(12),
            PointerKind::Drag(PointerButton::Left),
            x,
            200,
            Some(11),
        ));
        assert_eq!(app.detail_scroll(), unchanged);
    }

    #[test]
    fn resize_cancels_pixel_drag_and_a_stale_report_cannot_restart_it() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 10_000, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let x = u32::from(geometry.gutter.x) * 10 + 5;
        let y = u32::from(geometry.thumb_start_row) * 20 + 5;

        dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(21),
            PointerKind::Down(PointerButton::Left),
            x,
            y,
            Some(21),
        );
        assert!(drag.active.is_some());

        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([resize(100, 40)]);
        assert!(
            super::handle_terminal_events_with_mouse_mode(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                MouseMode::Disabled,
                FrontendInputContext {
                    terminal: TerminalInputContext::new(&run_tx, None),
                    contest_switch: None,
                    command_palette: None,
                    open_source: None,
                },
            )
            .unwrap()
        );
        assert!(drag.active.is_none());

        assert!(!dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            pixel_mode(22),
            PointerKind::Drag(PointerButton::Left),
            x,
            y + 100,
            Some(21),
        ));
        assert_eq!(app.detail_scroll(), 0);
        assert!(drag.active.is_none());
    }

    #[test]
    fn pixel_wheel_caps_and_track_click_match_cell_semantics() {
        let mode = pixel_mode(3);
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 1_000, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let gutter_x = u32::from(geometry.gutter.x) * 10 + 5;

        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::ScrollDown,
            u32::from(info.detail_area.x) * 10 + 5,
            u32::from(info.detail_area.y) * 20 + 5,
            Some(3),
        ));
        assert_eq!(app.detail_scroll(), DETAIL_SCROLL_LINES);

        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Down(PointerButton::Left),
            gutter_x,
            u32::from(geometry.bottom_cap_row.unwrap()) * 20 + 10,
            Some(3),
        ));
        assert_eq!(app.detail_scroll(), 1_000);

        app.set_detail_scroll_from_user(0);
        let track_row = geometry.track_start_row + geometry.track_len / 2;
        let expected = geometry.scroll_for_track_click(track_row);
        assert!(dispatch_pixel(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            mode,
            PointerKind::Down(PointerButton::Left),
            gutter_x,
            u32::from(track_row) * 20 + 19,
            Some(3),
        ));
        assert_eq!(app.detail_scroll(), expected);
    }

    #[test]
    fn drag_without_start_and_pixel_down_are_ignored_but_up_terminates() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Drag(MouseButton::Left),
            geometry.gutter.x,
            geometry.track_end_row(),
        ));
        dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(drag.active.is_some());
        assert!(!super::handle_pointer_event(
            &mut app,
            &mut layout,
            &mut drag,
            PointerEvent {
                kind: PointerKind::Down(PointerButton::Right),
                position: terminal::PointerPosition::AbsolutePixels { x: 0, y: 0 },
                modifiers: terminal::Modifiers::default(),
                pixel_generation: None,
            },
            &info,
        ));
        assert!(drag.active.is_some());
        assert!(!super::handle_pointer_event(
            &mut app,
            &mut layout,
            &mut drag,
            PointerEvent {
                kind: PointerKind::Up(PointerButton::Right),
                position: terminal::PointerPosition::AbsolutePixels { x: 0, y: 0 },
                modifiers: terminal::Modifiers::default(),
                pixel_generation: None,
            },
            &info,
        ));
        assert!(drag.active.is_none());
    }

    #[test]
    fn scroll_only_redraw_preserves_drag_but_stable_identity_changes_invalidate_it() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );

        let moved = scrollbar_info(&app, 100, 70, 1);
        drag.reconcile_render_info(&moved);
        assert!(drag.active.is_some(), "thumb start is not stable identity");

        let resized = scrollbar_info(&app, 100, 70, 2);
        drag.reconcile_render_info(&resized);
        assert!(drag.active.is_none());
    }

    #[test]
    fn resize_disappearance_and_another_down_cancel_drag() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let start = |app: &mut WatchApp,
                     layout: &mut detail_layout::DetailLayout,
                     drag: &mut DetailScrollbarDragState| {
            dispatch_mouse(
                app,
                layout,
                drag,
                &info,
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            );
        };

        start(&mut app, &mut layout, &mut drag);
        drag.reconcile_render_info(&view::RenderInfo::default());
        assert!(drag.active.is_none());

        start(&mut app, &mut layout, &mut drag);
        dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Right),
            0,
            0,
        );
        assert!(drag.active.is_none());

        start(&mut app, &mut layout, &mut drag);
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([resize(100, 40)]);
        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert!(drag.active.is_none());
    }

    #[test]
    fn stale_drag_geometry_cannot_mutate_a_new_detail_revision() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let old_info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &old_info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &old_info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(app.next_case());
        assert_eq!(app.detail_scroll(), 0);

        assert!(!dispatch_mouse(
            &mut app,
            &mut layout,
            &mut drag,
            &old_info,
            MouseEventKind::Drag(MouseButton::Left),
            0,
            u16::MAX,
        ));
        assert_eq!(app.detail_scroll(), 0);
        assert!(drag.active.is_none());
    }

    #[test]
    fn one_event_batch_can_start_and_advance_a_valid_drag() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
            TerminalEvent::Pointer(pointer(MouseEventKind::Drag(MouseButton::Left), 0, 15)),
            TerminalEvent::Pointer(pointer(MouseEventKind::Drag(MouseButton::Left), 0, 20)),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert!(app.detail_scroll() > 0);
        assert!(drag.active.is_some());
    }

    #[test]
    fn non_drag_scroll_queues_later_pointer_geometry_until_redraw() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                MouseEventKind::ScrollDown,
                geometry.gutter.x.saturating_sub(1),
                geometry.thumb_start_row,
            )),
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert_eq!(app.detail_scroll(), DETAIL_SCROLL_LINES);
        assert_eq!(events.len(), 1);
        assert!(drag.active.is_none());
    }

    #[test]
    fn track_seek_queues_a_later_drag_until_redraw() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                PointerKind::Down(PointerButton::Left),
                geometry.gutter.x,
                geometry.track_end_row().saturating_sub(1),
            )),
            TerminalEvent::Pointer(pointer(
                PointerKind::Drag(PointerButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert!(app.detail_scroll() > 0);
        assert_eq!(events.len(), 1);
        assert!(drag.active.is_none());
    }

    #[test]
    fn a_new_pointer_interaction_after_drag_waits_for_redrawn_thumb_geometry() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
            TerminalEvent::Pointer(pointer(MouseEventKind::Drag(MouseButton::Left), 0, 18)),
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.thumb_start_row,
            )),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert!(app.detail_scroll() > 0);
        assert_eq!(events.len(), 1);
        assert!(drag.active.is_some());
    }

    #[test]
    fn layout_changing_key_stops_later_mouse_events_until_redraw() {
        let mut app = app();
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        let (run_tx, _run_rx) = mpsc::channel();
        let mut events = VecDeque::from([
            TerminalEvent::Key(key(KeyCode::Char('s'), KeyEventKind::Press)),
            TerminalEvent::Pointer(pointer(
                MouseEventKind::Down(MouseButton::Left),
                geometry.gutter.x,
                geometry.track_end_row().saturating_sub(1),
            )),
        ]);

        assert!(
            super::handle_terminal_events(
                &mut app,
                &mut layout,
                &mut drag,
                &info,
                &mut events,
                &run_tx,
            )
            .unwrap()
        );
        assert_eq!(events.len(), 1);
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn problem_case_and_detail_mode_revisions_invalidate_drag_identity() {
        let mut problem_app = app_with_problems(&[3, 3]);
        let mut layout = detail_layout::DetailLayout::default();
        let mut drag = DetailScrollbarDragState::default();
        let info = scrollbar_info(&problem_app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut problem_app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(problem_app.next_problem());
        drag.reconcile_render_info(&scrollbar_info(&problem_app, 100, 0, 1));
        assert!(drag.active.is_none());

        let mut case_app = app();
        let info = scrollbar_info(&case_app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut case_app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(case_app.next_case());
        drag.reconcile_render_info(&scrollbar_info(&case_app, 100, 0, 1));
        assert!(drag.active.is_none());

        let mut mode_app = app();
        mode_app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let info = scrollbar_info(&mode_app, 100, 0, 1);
        let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
        dispatch_mouse(
            &mut mode_app,
            &mut layout,
            &mut drag,
            &info,
            MouseEventKind::Down(MouseButton::Left),
            geometry.gutter.x,
            geometry.thumb_start_row,
        );
        assert!(mode_app.queue_stress(0, 1).is_some());
        drag.reconcile_render_info(&scrollbar_info(&mode_app, 100, 0, 1));
        assert!(drag.active.is_none());
    }

    #[test]
    fn track_seek_and_drag_cancel_width_reconciliation_but_keep_exact_result_useful() {
        let raw = "long normal detail line\n".repeat(4_000);
        let segments = [raw.as_str()];
        let document = detail::DetailDocument::from_borrowed_segments(&segments);

        for use_drag in [false, true] {
            let (mut layout, delayed) = pending_width_layout(&document);
            let mut app = app();
            app.set_detail_scroll_from_user(500);
            let info = scrollbar_info(&app, 10_000, 500, 1);
            let geometry = &info.detail_scrollbar.as_ref().unwrap().geometry;
            let mut drag = DetailScrollbarDragState::default();

            if use_drag {
                dispatch_mouse(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    MouseEventKind::Down(MouseButton::Left),
                    geometry.gutter.x,
                    geometry.thumb_start_row,
                );
                dispatch_mouse(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    MouseEventKind::Drag(MouseButton::Left),
                    0,
                    geometry.track_end_row(),
                );
            } else {
                dispatch_mouse(
                    &mut app,
                    &mut layout,
                    &mut drag,
                    &info,
                    MouseEventKind::Down(MouseButton::Left),
                    geometry.gutter.x,
                    geometry.track_end_row().saturating_sub(1),
                );
            }

            assert!(!layout.has_pending_width_anchor_for_test());
            assert!(layout.apply_count_result(delayed));
            assert!(layout.take_scroll_reconciliation().is_none());
            let viewport = layout.viewport(&document, 2, 70, 20, app.detail_scroll());
            assert!(viewport.exact_layout_identity.is_some());
        }
    }
    #[test]
    fn rerun_key_queues_current_source() {
        let mut app = app();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        let request = received_run(&run_rx);

        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(!request.debug);
    }

    #[test]
    fn stress_inspection_precedes_seed_generation_and_ready_request_is_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let contest = contest_with_problems(&[1]);
        let mut app = WatchApp::new(&contest, vec![1]).unwrap();
        let candidate = temp.path().join("A.cpp");
        fs::write(&candidate, b"candidate").unwrap();
        assert!(app.source_changed(0, candidate, Language::Cpp));
        app.toggle_debug();
        let (run_tx, run_rx) = mpsc::channel();
        let setup = StressSetupContext::new(temp.path(), &contest);
        let seed_calls = Cell::new(0);

        assert!(
            queue_problem_stress_with_seed(&mut app, 0, &run_tx, setup, || {
                seed_calls.set(seed_calls.get() + 1);
                Ok(987)
            })
            .unwrap()
        );
        assert_eq!(seed_calls.get(), 0);
        assert!(run_rx.try_recv().is_err());
        assert_eq!(
            app.current_problem().unwrap().stress_setup,
            app::StressSetupState::Required {
                generator_missing: true,
                brute_missing: true,
            }
        );

        fs::write(temp.path().join("A_gen.py"), b"generator").unwrap();
        fs::write(temp.path().join("A_brute.py"), b"brute").unwrap();
        assert!(
            queue_problem_stress_with_seed(&mut app, 0, &run_tx, setup, || {
                seed_calls.set(seed_calls.get() + 1);
                Ok(987)
            })
            .unwrap()
        );

        assert_eq!(seed_calls.get(), 1);
        let request = received_run(&run_rx);
        assert_eq!(request.run_id, 1);
        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(request.debug);
        assert!(matches!(
            request.kind,
            message::RunKind::Stress {
                base_seed: 987,
                count: None,
            }
        ));
        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn stress_key_queues_unbounded_stress_for_current_source() {
        let temp = tempfile::tempdir().unwrap();
        let contest = contest_with_problems(&[3]);
        let mut app = WatchApp::new(&contest, vec![3]).unwrap();
        let candidate = temp.path().join("A.cpp");
        fs::write(&candidate, b"candidate").unwrap();
        fs::write(temp.path().join("A_gen.py"), b"generator").unwrap();
        fs::write(temp.path().join("A_brute.py"), b"brute").unwrap();
        app.source_changed(0, candidate, Language::Cpp);
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );

        let request = received_run(&run_rx);
        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(matches!(
            request.kind,
            message::RunKind::Stress { count: None, .. }
        ));
        assert_eq!(
            app.current_problem().unwrap().detail_mode,
            app::DetailMode::Stress
        );
        assert_eq!(
            app.current_problem().unwrap().stress_setup,
            app::StressSetupState::None
        );
    }

    #[test]
    fn stress_key_reports_missing_helpers_before_candidate_requirements() {
        for (generator_exists, brute_exists, generator_missing, brute_missing) in [
            (false, false, true, true),
            (true, false, false, true),
            (false, true, true, false),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let contest = contest_with_problems(&[1]);
            let mut app = WatchApp::new(&contest, vec![1]).unwrap();
            if generator_exists {
                fs::write(temp.path().join("A_gen.py"), b"generator").unwrap();
            }
            if brute_exists {
                fs::write(temp.path().join("A_brute.py"), b"brute").unwrap();
            }
            let (run_tx, run_rx) = mpsc::channel();

            assert!(
                handle_stress_setup_key(
                    &mut app,
                    KeyCode::Char('S'),
                    temp.path(),
                    &contest,
                    &run_tx,
                )
                .unwrap()
            );

            assert!(run_rx.try_recv().is_err());
            let problem = app.current_problem().unwrap();
            assert_eq!(problem.detail_mode, app::DetailMode::Stress);
            assert_eq!(problem.stress.phase, app::StressPhase::Idle);
            assert_eq!(
                problem.stress_setup,
                app::StressSetupState::Required {
                    generator_missing,
                    brute_missing,
                }
            );
        }
    }

    #[test]
    fn stress_key_keeps_invalid_targets_local_to_the_tui() {
        let temp = tempfile::tempdir().unwrap();
        let contest = contest_with_problems(&[1]);
        let mut app = WatchApp::new(&contest, vec![1]).unwrap();
        fs::create_dir(temp.path().join("A_gen.py")).unwrap();
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );

        assert!(run_rx.try_recv().is_err());
        let problem = app.current_problem().unwrap();
        assert_eq!(problem.detail_mode, app::DetailMode::Stress);
        assert!(matches!(
            &problem.stress_setup,
            app::StressSetupState::Error { message }
                if message.contains("stress generator target is not a regular file")
        ));
        assert_eq!(problem.stress.phase, app::StressPhase::Idle);
    }

    #[test]
    fn ready_helpers_without_a_candidate_preserve_the_existing_no_request_behavior() {
        let temp = tempfile::tempdir().unwrap();
        let contest = contest_with_problems(&[1]);
        let mut app = WatchApp::new(&contest, vec![1]).unwrap();
        fs::write(temp.path().join("A_gen.py"), b"generator").unwrap();
        fs::write(temp.path().join("A_brute.py"), b"brute").unwrap();
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            !handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );

        assert!(run_rx.try_recv().is_err());
        assert_eq!(
            app.current_problem().unwrap().stress.phase,
            app::StressPhase::Idle
        );
        assert_eq!(
            app.current_problem().unwrap().stress_setup,
            app::StressSetupState::None
        );
    }

    #[test]
    fn initialize_key_uses_production_initialization_without_candidate_or_run_request() {
        for existing_generator in [None, Some(&b"user generator\0\xff"[..])] {
            let temp = tempfile::tempdir().unwrap();
            let contest = contest_with_problems(&[1]);
            let mut app = WatchApp::new(&contest, vec![1]).unwrap();
            let generator = temp.path().join("A_gen.py");
            let brute = temp.path().join("A_brute.py");
            if let Some(contents) = existing_generator {
                fs::write(&generator, contents).unwrap();
            }
            let (run_tx, run_rx) = mpsc::channel();

            assert!(
                handle_stress_setup_key(
                    &mut app,
                    KeyCode::Char('S'),
                    temp.path(),
                    &contest,
                    &run_tx,
                )
                .unwrap()
            );
            assert!(
                handle_stress_setup_key(
                    &mut app,
                    KeyCode::Char('i'),
                    temp.path(),
                    &contest,
                    &run_tx,
                )
                .unwrap()
            );

            assert_eq!(
                fs::read(&generator).unwrap(),
                existing_generator
                    .unwrap_or_else(|| { crate::template::stress_generator_template().as_bytes() })
            );
            assert_eq!(
                fs::read(&brute).unwrap(),
                crate::template::stress_brute_template().as_bytes()
            );
            assert!(run_rx.try_recv().is_err());
            let problem = app.current_problem().unwrap();
            assert!(problem.source.is_none());
            assert_eq!(problem.stress.phase, app::StressPhase::Idle);
            assert_eq!(problem.detail_mode, app::DetailMode::Stress);
            assert_eq!(problem.stress_setup, app::StressSetupState::Initialized);
        }
    }

    #[test]
    fn setup_actions_do_not_hide_or_replace_an_already_queued_stress() {
        let temp = tempfile::tempdir().unwrap();
        let contest = contest_with_problems(&[1]);
        let mut app = WatchApp::new(&contest, vec![1]).unwrap();
        let candidate = temp.path().join("A.py");
        let generator = temp.path().join("A_gen.py");
        let brute = temp.path().join("A_brute.py");
        fs::write(&candidate, b"candidate").unwrap();
        fs::write(&generator, b"generator").unwrap();
        fs::write(&brute, b"brute").unwrap();
        assert!(app.source_changed(0, candidate, Language::Python));
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        let queued = received_run(&run_rx);
        assert_eq!(
            app.current_problem().unwrap().stress.phase,
            app::StressPhase::Queued
        );

        fs::remove_file(&generator).unwrap();
        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        assert!(run_rx.try_recv().is_err());
        assert_eq!(
            app.current_problem().unwrap().stress.id,
            Some(queued.run_id)
        );
        assert!(matches!(
            app.current_problem().unwrap().stress_setup,
            app::StressSetupState::Required { .. }
        ));
        let required_detail: String = detail::DetailDocument::from_app(&app)
            .segments()
            .map(|segment| segment.text())
            .collect();
        assert!(required_detail.contains("STRESS QUEUED"));
        let queued_heading = required_detail.find("STRESS QUEUED").unwrap();
        let setup_heading = required_detail.find("STRESS SETUP REQUIRED").unwrap();
        assert!(queued_heading < setup_heading);

        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('i'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        assert!(run_rx.try_recv().is_err());
        assert_eq!(
            app.current_problem().unwrap().stress.id,
            Some(queued.run_id)
        );
        assert_eq!(
            app.current_problem().unwrap().stress_setup,
            app::StressSetupState::Initialized
        );
        let initialized_detail: String = detail::DetailDocument::from_app(&app)
            .segments()
            .map(|segment| segment.text())
            .collect();
        assert!(initialized_detail.contains("STRESS QUEUED"));
        assert!(!initialized_detail.contains("STRESS FILES INITIALIZED"));
    }

    #[test]
    fn initialize_key_is_a_no_op_outside_required_state_and_keeps_errors_local() {
        let temp = tempfile::tempdir().unwrap();
        let contest = contest_with_problems(&[1]);
        let mut app = WatchApp::new(&contest, vec![1]).unwrap();
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            !handle_stress_setup_key(&mut app, KeyCode::Char('i'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        assert!(!temp.path().join("A_gen.py").exists());
        assert!(!temp.path().join("A_brute.py").exists());

        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        fs::create_dir(temp.path().join("A_gen.py")).unwrap();
        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('i'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );

        assert!(run_rx.try_recv().is_err());
        assert!(matches!(
            &app.current_problem().unwrap().stress_setup,
            app::StressSetupState::Error { message }
                if message.contains("stress generator target is not a regular file")
        ));
        assert!(!temp.path().join("A_brute.py").exists());
        assert!(
            !handle_stress_setup_key(&mut app, KeyCode::Char('i'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
    }

    #[test]
    fn every_stress_key_press_rechecks_the_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let contest = contest_with_problems(&[1]);
        let mut app = WatchApp::new(&contest, vec![1]).unwrap();
        let candidate = temp.path().join("A.py");
        fs::write(&candidate, b"candidate").unwrap();
        app.source_changed(0, candidate, Language::Python);
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        fs::write(temp.path().join("A_gen.py"), b"generator").unwrap();
        fs::write(temp.path().join("A_brute.py"), b"brute").unwrap();
        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        assert!(matches!(
            received_run(&run_rx).kind,
            message::RunKind::Stress { count: None, .. }
        ));

        let mut app = WatchApp::new(&contest, vec![1]).unwrap();
        app.source_changed(0, temp.path().join("A.py"), Language::Python);
        fs::remove_file(temp.path().join("A_gen.py")).unwrap();
        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('i'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        assert_eq!(
            app.current_problem().unwrap().stress_setup,
            app::StressSetupState::Initialized
        );

        fs::remove_file(temp.path().join("A_gen.py")).unwrap();
        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        assert_eq!(
            app.current_problem().unwrap().stress_setup,
            app::StressSetupState::Required {
                generator_missing: true,
                brute_missing: false,
            }
        );

        fs::create_dir(temp.path().join("A_gen.py")).unwrap();
        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        assert!(matches!(
            app.current_problem().unwrap().stress_setup,
            app::StressSetupState::Error { .. }
        ));

        fs::remove_dir(temp.path().join("A_gen.py")).unwrap();
        fs::write(temp.path().join("A_gen.py"), b"repaired generator").unwrap();
        assert!(
            handle_stress_setup_key(&mut app, KeyCode::Char('S'), temp.path(), &contest, &run_tx,)
                .unwrap()
        );
        assert!(matches!(
            received_run(&run_rx).kind,
            message::RunKind::Stress { count: None, .. }
        ));
        assert_eq!(
            app.current_problem().unwrap().stress_setup,
            app::StressSetupState::None
        );
    }

    #[test]
    fn rerun_uses_only_the_selected_problems_confirmed_source() {
        let mut app = app_with_problems(&[1, 1]);
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        app.source_changed(1, PathBuf::from("B.py"), Language::Python);
        assert!(app.select_problem(0));

        let (run_tx, run_rx) = mpsc::channel();
        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        let request = received_run(&run_rx);
        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
    }

    #[test]
    fn rerun_key_without_source_is_no_op() {
        let mut app = app();
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            !handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn debug_and_rerun_repeat_do_not_change_state_or_queue_requests() {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );
        let first = received_run(&run_rx);
        assert!(first.debug);

        assert!(
            !handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Repeat),
                &run_tx,
            )
            .unwrap()
        );
        assert!(
            !handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Repeat),
                &run_tx,
            )
            .unwrap()
        );

        assert!(app.debug_enabled());
        assert_eq!(app.current_problem().unwrap().run.id, Some(first.run_id));
        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn rerun_request_channel_disconnect_is_a_fatal_error() {
        let mut app = app();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let (run_tx, run_rx) = mpsc::channel();
        drop(run_rx);
        let mut events = VecDeque::from([TerminalEvent::Key(key(
            KeyCode::Char('r'),
            KeyEventKind::Press,
        ))]);

        let error =
            handle_terminal_events(&mut app, &view::RenderInfo::default(), &mut events, &run_tx)
                .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "run worker request channel disconnected");
    }

    #[test]
    fn debug_toggle_reruns_cpp_with_new_debug_state_and_resets_all_folds() {
        let mut app = app();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let initial = app.queue_run(0).unwrap();
        assert!(app.run_started(0, initial.run_id));
        assert!(app.run_event(
            0,
            initial.run_id,
            message::TestEvent::TestRunStarted { total_cases: 3 },
        ));
        assert!(app.run_event(
            0,
            initial.run_id,
            message::TestEvent::TestCaseComparison {
                number: 1,
                expected: "expected".to_string(),
                actual: "actual".to_string(),
            },
        ));
        for kind in [
            detail::DetailSectionKind::Input,
            detail::DetailSectionKind::Expected,
            detail::DetailSectionKind::Actual,
            detail::DetailSectionKind::Stderr,
        ] {
            app.toggle_detail_section(kind);
        }

        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(app.debug_enabled());

        let request = received_run(&run_rx);

        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(request.debug);
        for kind in [
            detail::DetailSectionKind::Input,
            detail::DetailSectionKind::Expected,
            detail::DetailSectionKind::Actual,
            detail::DetailSectionKind::Stderr,
        ] {
            assert!(!app.detail_fold_state().is_collapsed(kind), "{kind:?}");
        }

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(!app.debug_enabled());

        let request = received_run(&run_rx);
        assert!(!request.debug);
    }
    #[test]
    fn debug_toggle_does_not_rerun_python() {
        let mut app = app();

        app.source_changed(0, PathBuf::from("A.py"), Language::Python);

        let (run_tx, run_rx) = mpsc::channel();

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        assert!(app.debug_enabled());
        assert!(run_rx.try_recv().is_err());
    }

    #[test]
    fn save_debug_rerun_and_save_keep_monotonic_run_ids_and_latest_state() {
        let mut app = app();
        let (message_tx, message_rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        message_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("A.cpp"),
                language: Language::Cpp,
            })
            .unwrap();
        assert!(handle_messages(&mut app, &message_rx, &run_tx).unwrap());

        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('d'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );
        assert!(
            handle_key_event(
                &mut app,
                key(KeyCode::Char('r'), KeyEventKind::Press),
                &run_tx,
            )
            .unwrap()
        );

        message_tx
            .send(Message::SourceChanged {
                problem: 0,
                path: PathBuf::from("A.py"),
                language: Language::Python,
            })
            .unwrap();
        assert!(handle_messages(&mut app, &message_rx, &run_tx).unwrap());

        let requests = received_runs(&run_rx);
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.run_id)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            requests
                .iter()
                .map(|request| (request.language, request.debug))
                .collect::<Vec<_>>(),
            [
                (Language::Cpp, false),
                (Language::Cpp, true),
                (Language::Cpp, true),
                (Language::Python, false),
            ]
        );

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.id, Some(4));
        assert_eq!(run.phase, app::RunPhase::Queued);
        assert_eq!(run.language, Some(Language::Python));
        assert!(app.debug_enabled());

        assert!(!app.run_failed(0, 3, "stale failure".to_string()));
        assert_eq!(app.current_problem().unwrap().run.id, Some(4));
        assert_eq!(
            app.current_problem().unwrap().run.phase,
            app::RunPhase::Queued
        );
    }

    #[test]
    fn rapid_save_debug_and_rerun_requests_keep_monotonic_latest_state() {
        let mut app = app();
        let (message_tx, message_rx) = mpsc::channel();
        let (run_tx, run_rx) = mpsc::channel();

        for operation in 0..300 {
            match operation % 3 {
                0 => {
                    message_tx
                        .send(Message::SourceChanged {
                            problem: 0,
                            path: PathBuf::from("A.cpp"),
                            language: Language::Cpp,
                        })
                        .unwrap();
                    assert!(handle_messages(&mut app, &message_rx, &run_tx).unwrap());
                }
                1 => assert!(
                    handle_key_event(
                        &mut app,
                        key(KeyCode::Char('d'), KeyEventKind::Press),
                        &run_tx,
                    )
                    .unwrap()
                ),
                _ => assert!(
                    handle_key_event(
                        &mut app,
                        key(KeyCode::Char('r'), KeyEventKind::Press),
                        &run_tx,
                    )
                    .unwrap()
                ),
            }
        }

        let requests = received_runs(&run_rx);
        assert_eq!(requests.len(), 300);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.run_id)
                .collect::<Vec<_>>(),
            (1..=300).collect::<Vec<_>>()
        );
        assert!(
            requests
                .iter()
                .all(|request| { request.problem == 0 && request.language == Language::Cpp })
        );

        let latest = requests.last().unwrap();
        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.id, Some(latest.run_id));
        assert_eq!(run.phase, app::RunPhase::Queued);
        assert_eq!(run.language, Some(latest.language));
        assert_eq!(latest.debug, app.debug_enabled());
    }
}
