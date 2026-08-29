use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::detail::DetailSectionKind;
use super::message::{RunId, RunKind, RunRequest, StressEvent, TestEvent};
use crate::language::Language;
use crate::model::{Contest, Sample};
use crate::stress::CandidateFailureKind;

#[derive(Debug)]
pub struct SourceState {
    pub path: PathBuf,
    pub language: Language,
}

#[derive(Debug, Clone)]
pub struct SavedStressCaseState {
    pub input: Arc<String>,
    pub expected: Arc<String>,
}

impl From<Sample> for SavedStressCaseState {
    fn from(sample: Sample) -> Self {
        Self {
            input: Arc::new(sample.input),
            expected: Arc::new(sample.output),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedUserInputState {
    pub id: u64,
    pub content: String,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserInputEditTarget {
    Draft,
    Persisted(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserInputSelection {
    Persisted(u64),
    Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseSelection {
    Test(usize),
    UserInput(UserInputSelection),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum DraftReturnSelection {
    #[default]
    None,
    Sample(usize),
    SavedStress,
    PersistedUserInput(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInputEditState {
    target: UserInputEditTarget,
    buffer: String,
    cursor: usize,
    preferred_column: Option<usize>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl UserInputEditState {
    pub fn target(&self) -> UserInputEditTarget {
        self.target
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn replace_buffer(&mut self, buffer: String) {
        self.buffer = buffer;
        self.cursor = self.buffer.len();
        self.preferred_column = None;
    }

    pub fn insert(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        debug_assert!(self.buffer.is_char_boundary(self.cursor));
        self.buffer.insert_str(self.cursor, text);
        self.cursor = self.cursor.saturating_add(text.len());
        if self.cursor > 0
            && self.buffer.as_bytes().get(self.cursor - 1) == Some(&b'\r')
            && self.buffer.as_bytes().get(self.cursor) == Some(&b'\n')
        {
            self.cursor = self.cursor.saturating_add(1);
        }
        self.preferred_column = None;
        debug_assert!(self.buffer.is_char_boundary(self.cursor));
        true
    }

    pub fn move_left(&mut self) -> bool {
        if self
            .buffer
            .as_bytes()
            .get(self.cursor.saturating_sub(2)..self.cursor)
            == Some(b"\r\n".as_slice())
        {
            self.cursor = self.cursor.saturating_sub(2);
            self.preferred_column = None;
            debug_assert!(self.buffer.is_char_boundary(self.cursor));
            return true;
        }
        let Some((previous, _)) = self.buffer[..self.cursor].char_indices().next_back() else {
            return false;
        };
        self.cursor = previous;
        self.preferred_column = None;
        debug_assert!(self.buffer.is_char_boundary(self.cursor));
        true
    }

    pub fn move_right(&mut self) -> bool {
        if self
            .buffer
            .as_bytes()
            .get(self.cursor..self.cursor.saturating_add(2))
            == Some(b"\r\n".as_slice())
        {
            self.cursor = self.cursor.saturating_add(2);
            self.preferred_column = None;
            debug_assert!(self.buffer.is_char_boundary(self.cursor));
            return true;
        }
        let Some(character) = self.buffer[self.cursor..].chars().next() else {
            return false;
        };
        self.cursor = self.cursor.saturating_add(character.len_utf8());
        self.preferred_column = None;
        debug_assert!(self.buffer.is_char_boundary(self.cursor));
        true
    }

    pub fn move_home(&mut self) -> bool {
        let start = logical_line_start(&self.buffer, self.cursor);
        if start == self.cursor {
            return false;
        }
        self.cursor = start;
        self.preferred_column = None;
        true
    }

    pub fn move_end(&mut self) -> bool {
        let end = logical_line_end(&self.buffer, self.cursor);
        if end == self.cursor {
            return false;
        }
        self.cursor = end;
        self.preferred_column = None;
        true
    }

    pub fn move_up(&mut self) -> bool {
        let current_start = logical_line_start(&self.buffer, self.cursor);
        if current_start == 0 {
            return false;
        }
        let column = self
            .preferred_column
            .unwrap_or_else(|| self.buffer[current_start..self.cursor].chars().count());
        let previous_newline = current_start.saturating_sub(1);
        let previous_end = previous_newline.saturating_sub(usize::from(
            previous_newline > 0
                && self.buffer.as_bytes().get(previous_newline - 1) == Some(&b'\r'),
        ));
        let previous_start = logical_line_start(&self.buffer, previous_end);
        self.cursor = byte_at_scalar_column(&self.buffer, previous_start, previous_end, column);
        self.preferred_column = Some(column);
        true
    }

    pub fn move_down(&mut self) -> bool {
        let Some(next_newline) = self.buffer[self.cursor..].find('\n') else {
            return false;
        };
        let current_start = logical_line_start(&self.buffer, self.cursor);
        let column = self
            .preferred_column
            .unwrap_or_else(|| self.buffer[current_start..self.cursor].chars().count());
        let next_start = self.cursor.saturating_add(next_newline).saturating_add(1);
        let next_end = logical_line_end(&self.buffer, next_start);
        self.cursor = byte_at_scalar_column(&self.buffer, next_start, next_end, column);
        self.preferred_column = Some(column);
        true
    }

    pub fn backspace(&mut self) -> bool {
        if self
            .buffer
            .as_bytes()
            .get(self.cursor.saturating_sub(2)..self.cursor)
            == Some(b"\r\n".as_slice())
        {
            let start = self.cursor.saturating_sub(2);
            self.buffer.drain(start..self.cursor);
            self.cursor = start;
            self.preferred_column = None;
            debug_assert!(self.buffer.is_char_boundary(self.cursor));
            return true;
        }
        let Some((previous, _)) = self.buffer[..self.cursor].char_indices().next_back() else {
            return false;
        };
        self.buffer.drain(previous..self.cursor);
        self.cursor = previous;
        self.move_before_crlf_if_between();
        self.preferred_column = None;
        debug_assert!(self.buffer.is_char_boundary(self.cursor));
        true
    }

    pub fn delete(&mut self) -> bool {
        if self
            .buffer
            .as_bytes()
            .get(self.cursor..self.cursor.saturating_add(2))
            == Some(b"\r\n".as_slice())
        {
            let end = self.cursor.saturating_add(2);
            self.buffer.drain(self.cursor..end);
            self.preferred_column = None;
            debug_assert!(self.buffer.is_char_boundary(self.cursor));
            return true;
        }
        let Some(character) = self.buffer[self.cursor..].chars().next() else {
            return false;
        };
        let end = self.cursor.saturating_add(character.len_utf8());
        self.buffer.drain(self.cursor..end);
        self.move_before_crlf_if_between();
        self.preferred_column = None;
        debug_assert!(self.buffer.is_char_boundary(self.cursor));
        true
    }

    fn move_before_crlf_if_between(&mut self) {
        if self.cursor > 0
            && self.buffer.as_bytes().get(self.cursor - 1) == Some(&b'\r')
            && self.buffer.as_bytes().get(self.cursor) == Some(&b'\n')
        {
            self.cursor -= 1;
        }
    }
}

fn logical_line_start(buffer: &str, cursor: usize) -> usize {
    debug_assert!(buffer.is_char_boundary(cursor));
    buffer[..cursor]
        .rfind('\n')
        .map_or(0, |newline| newline.saturating_add(1))
}

fn logical_line_end(buffer: &str, cursor: usize) -> usize {
    debug_assert!(buffer.is_char_boundary(cursor));
    let newline = buffer[cursor..]
        .find('\n')
        .map_or(buffer.len(), |newline| cursor.saturating_add(newline));
    if newline > cursor && buffer.as_bytes().get(newline - 1) == Some(&b'\r') {
        newline - 1
    } else {
        newline
    }
}

fn byte_at_scalar_column(buffer: &str, start: usize, end: usize, column: usize) -> usize {
    debug_assert!(start <= end);
    debug_assert!(buffer.is_char_boundary(start));
    debug_assert!(buffer.is_char_boundary(end));
    buffer[start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(offset, _)| start.saturating_add(offset))
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserInputEditStartError {
    AlreadyEditing,
    PersistedInputNotFound(u64),
    Unavailable,
}

impl fmt::Display for UserInputEditStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyEditing => formatter.write_str("a user input edit is already active"),
            Self::PersistedInputNotFound(id) => {
                write!(formatter, "persisted user input {id} was not found")
            }
            Self::Unavailable => formatter.write_str("User Inputs are unavailable"),
        }
    }
}

impl std::error::Error for UserInputEditStartError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInputReadyState {
    persisted: Vec<PersistedUserInputState>,
    edit: Option<UserInputEditState>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl UserInputReadyState {
    pub(crate) fn new(mut persisted: Vec<PersistedUserInputState>) -> Self {
        persisted.sort_by_key(|input| input.id);
        Self {
            persisted,
            edit: None,
        }
    }

    pub fn persisted(&self) -> &[PersistedUserInputState] {
        &self.persisted
    }

    pub fn edit(&self) -> Option<&UserInputEditState> {
        self.edit.as_ref()
    }

    pub fn edit_mut(&mut self) -> Option<&mut UserInputEditState> {
        self.edit.as_mut()
    }

    pub fn begin_draft(&mut self) -> Result<(), UserInputEditStartError> {
        if self.edit.is_some() {
            return Err(UserInputEditStartError::AlreadyEditing);
        }
        self.edit = Some(UserInputEditState {
            target: UserInputEditTarget::Draft,
            buffer: String::new(),
            cursor: 0,
            preferred_column: None,
        });
        Ok(())
    }

    pub fn begin_persisted_edit(&mut self, id: u64) -> Result<(), UserInputEditStartError> {
        if self.edit.is_some() {
            return Err(UserInputEditStartError::AlreadyEditing);
        }
        let content = self
            .persisted
            .iter()
            .find(|input| input.id == id)
            .map(|input| input.content.clone())
            .ok_or(UserInputEditStartError::PersistedInputNotFound(id))?;
        self.edit = Some(UserInputEditState {
            target: UserInputEditTarget::Persisted(id),
            cursor: content.len(),
            buffer: content,
            preferred_column: None,
        });
        Ok(())
    }

    pub fn edit_is_dirty(&self) -> Option<bool> {
        let edit = self.edit.as_ref()?;
        Some(match edit.target {
            UserInputEditTarget::Draft => true,
            UserInputEditTarget::Persisted(id) => {
                let persisted = self
                    .persisted
                    .iter()
                    .find(|input| input.id == id)
                    .expect("an active persisted User Input edit must retain its target");
                edit.buffer != persisted.content
            }
        })
    }

    pub fn cancel_edit(&mut self) -> bool {
        self.edit.take().is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInputState {
    Ready(UserInputReadyState),
    Error { message: Arc<String> },
}

impl Default for UserInputState {
    fn default() -> Self {
        Self::Ready(UserInputReadyState::default())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl UserInputState {
    pub(crate) fn loaded(persisted: Vec<PersistedUserInputState>) -> Self {
        Self::Ready(UserInputReadyState::new(persisted))
    }

    pub(crate) fn load_error(message: String) -> Self {
        Self::Error {
            message: Arc::new(message),
        }
    }

    pub fn ready(&self) -> Option<&UserInputReadyState> {
        match self {
            Self::Ready(state) => Some(state),
            Self::Error { .. } => None,
        }
    }

    pub fn ready_mut(&mut self) -> Option<&mut UserInputReadyState> {
        match self {
            Self::Ready(state) => Some(state),
            Self::Error { .. } => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Ready(_) => None,
            Self::Error { message } => Some(message),
        }
    }
}

#[derive(Debug)]
pub struct ProblemState {
    pub index: String,
    pub title: String,
    pub sample_cases: usize,
    pub total_cases: usize,
    pub saved_stress_case: Option<SavedStressCaseState>,
    pub source: Option<SourceState>,
    pub run: RunState,
    pub stress: StressState,
    pub stress_setup: StressSetupState,
    #[cfg_attr(not(test), allow(dead_code))]
    pub user_inputs: UserInputState,
    selection_before_draft: DraftReturnSelection,
    pub detail_mode: DetailMode,
}

impl ProblemState {
    fn user_input_selections(&self) -> impl Iterator<Item = CaseSelection> + '_ {
        let ready = self.user_inputs.ready();
        let persisted = ready.into_iter().flat_map(|ready| {
            ready
                .persisted()
                .iter()
                .map(|input| CaseSelection::UserInput(UserInputSelection::Persisted(input.id)))
        });
        let draft = ready
            .and_then(UserInputReadyState::edit)
            .filter(|edit| edit.target() == UserInputEditTarget::Draft)
            .map(|_| CaseSelection::UserInput(UserInputSelection::Draft));

        persisted.chain(draft)
    }

    fn case_selections(&self) -> Vec<CaseSelection> {
        (0..self.total_cases)
            .map(CaseSelection::Test)
            .chain(self.user_input_selections())
            .collect()
    }

    fn first_case_selection(&self) -> Option<CaseSelection> {
        if self.total_cases > 0 {
            Some(CaseSelection::Test(0))
        } else {
            self.user_input_selections().next()
        }
    }

    fn contains_case_selection(&self, selection: CaseSelection) -> bool {
        match selection {
            CaseSelection::Test(index) => index < self.total_cases,
            CaseSelection::UserInput(_) => {
                self.user_input_selections().any(|item| item == selection)
            }
        }
    }

    fn draft_return_selection(&self, selection: Option<CaseSelection>) -> DraftReturnSelection {
        match selection {
            Some(CaseSelection::Test(index)) if index < self.sample_cases => {
                DraftReturnSelection::Sample(index)
            }
            Some(CaseSelection::Test(index))
                if self.saved_stress_case.is_some() && index == self.sample_cases =>
            {
                DraftReturnSelection::SavedStress
            }
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(id)))
                if self.contains_case_selection(CaseSelection::UserInput(
                    UserInputSelection::Persisted(id),
                )) =>
            {
                DraftReturnSelection::PersistedUserInput(id)
            }
            Some(CaseSelection::Test(_))
            | Some(CaseSelection::UserInput(UserInputSelection::Draft))
            | Some(CaseSelection::UserInput(UserInputSelection::Persisted(_)))
            | None => DraftReturnSelection::None,
        }
    }

    fn resolve_draft_return_selection(
        &self,
        selection: DraftReturnSelection,
    ) -> Option<CaseSelection> {
        match selection {
            DraftReturnSelection::Sample(index) if index < self.sample_cases => {
                Some(CaseSelection::Test(index))
            }
            DraftReturnSelection::SavedStress if self.saved_stress_case.is_some() => {
                Some(CaseSelection::Test(self.sample_cases))
            }
            DraftReturnSelection::PersistedUserInput(id)
                if self.contains_case_selection(CaseSelection::UserInput(
                    UserInputSelection::Persisted(id),
                )) =>
            {
                Some(CaseSelection::UserInput(UserInputSelection::Persisted(id)))
            }
            DraftReturnSelection::None
            | DraftReturnSelection::Sample(_)
            | DraftReturnSelection::SavedStress
            | DraftReturnSelection::PersistedUserInput(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct WatchApp {
    should_quit: bool,
    debug: bool,
    samples_pane_enabled: bool,

    contest_id: String,
    problems: Vec<ProblemState>,

    selected_problem: usize,
    case_selection: Option<CaseSelection>,

    detail_scroll: usize,
    detail_revision: u64,
    detail_folds: DetailFoldState,

    next_run_id: RunId,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct DetailFoldState {
    input_collapsed: bool,
    expected_collapsed: bool,
    actual_collapsed: bool,
    stderr_collapsed: bool,
}

impl DetailFoldState {
    pub(super) fn is_collapsed(self, kind: DetailSectionKind) -> bool {
        match kind {
            DetailSectionKind::Input => self.input_collapsed,
            DetailSectionKind::Expected => self.expected_collapsed,
            DetailSectionKind::Actual => self.actual_collapsed,
            DetailSectionKind::Stderr => self.stderr_collapsed,
        }
    }

    fn toggle(&mut self, kind: DetailSectionKind) {
        let collapsed = match kind {
            DetailSectionKind::Input => &mut self.input_collapsed,
            DetailSectionKind::Expected => &mut self.expected_collapsed,
            DetailSectionKind::Actual => &mut self.actual_collapsed,
            DetailSectionKind::Stderr => &mut self.stderr_collapsed,
        };
        *collapsed = !*collapsed;
    }

    fn expand(&mut self, kind: DetailSectionKind) {
        match kind {
            DetailSectionKind::Input => self.input_collapsed = false,
            DetailSectionKind::Expected => self.expected_collapsed = false,
            DetailSectionKind::Actual => self.actual_collapsed = false,
            DetailSectionKind::Stderr => self.stderr_collapsed = false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayedDetailCase {
    problem: usize,
    mode: DetailMode,
    case: Option<DisplayedNormalCase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayedNormalCase {
    Sample(usize),
    SavedStress,
    UserInput(UserInputSelection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailMode {
    Samples,
    Stress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StressPhase {
    Idle,
    Queued,
    Compiling,
    Running,
    Failed,
    Finished,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StressSetupState {
    #[default]
    None,
    Required {
        generator_missing: bool,
        brute_missing: bool,
    },
    Initialized,
    Error {
        message: Arc<String>,
    },
}

#[derive(Debug, Clone)]
pub struct StressFailureState {
    pub kind: CandidateFailureKind,
    pub case_number: u64,
    pub seed: u64,
    pub input: Arc<String>,
    pub expected: Arc<String>,
    pub actual: Arc<String>,
    pub stderr: Arc<String>,
    pub candidate_elapsed: Duration,
    pub saved_to: PathBuf,
}

#[derive(Debug)]
pub struct StressState {
    pub id: Option<RunId>,
    pub phase: StressPhase,
    pub language: Option<Language>,
    pub base_seed: Option<u64>,
    pub case_limit: Option<u64>,
    pub case_number: u64,
    pub seed: Option<u64>,
    pub passed: u64,
    pub elapsed: Duration,
    pub cases_per_second: f64,
    pub failure: Option<StressFailureState>,
    pub error: Option<Arc<String>>,
}

impl Default for StressState {
    fn default() -> Self {
        Self {
            id: None,
            phase: StressPhase::Idle,
            language: None,
            base_seed: None,
            case_limit: None,
            case_number: 0,
            seed: None,
            passed: 0,
            elapsed: Duration::ZERO,
            cases_per_second: 0.0,
            failure: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    Idle,
    Queued,
    Compiling,
    Running,
    Finished,
    CompileError,
    CompileTimedOut,
    NoSamples,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseVerdict {
    Pending,
    Accepted,
    WrongAnswer,
    RuntimeError,
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct CaseState {
    pub verdict: CaseVerdict,
    pub elapsed: Option<Duration>,
    pub input: Option<Arc<String>>,
    pub expected: Option<Arc<String>>,
    pub actual: Option<Arc<String>>,
    pub stderr: Option<Arc<String>>,
}

impl Default for CaseState {
    fn default() -> Self {
        Self {
            verdict: CaseVerdict::Pending,
            elapsed: None,
            input: None,
            expected: None,
            actual: None,
            stderr: None,
        }
    }
}

#[derive(Debug)]
pub struct RunState {
    pub id: Option<RunId>,
    pub phase: RunPhase,
    pub language: Option<Language>,
    pub debug: bool,
    test_run_started: bool,

    pub accepted: usize,
    pub total_cases: usize,
    pub error: Option<Arc<String>>,
    pub cases: Vec<CaseState>,
}

impl Default for RunState {
    fn default() -> Self {
        Self {
            id: None,
            phase: RunPhase::Idle,
            language: None,
            debug: false,
            test_run_started: false,

            accepted: 0,
            total_cases: 0,
            error: None,

            cases: Vec::new(),
        }
    }
}

fn case_mut(run: &mut RunState, number: usize) -> Option<&mut CaseState> {
    let index = number.checked_sub(1)?;
    run.cases.get_mut(index)
}

impl WatchApp {
    #[cfg(test)]
    pub fn new(contest: &Contest, sample_counts: Vec<usize>) -> io::Result<Self> {
        let stress_cases = vec![None; contest.problems.len()];
        Self::new_with_stress_cases(contest, sample_counts, stress_cases)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new_with_stress_cases(
        contest: &Contest,
        sample_counts: Vec<usize>,
        stress_cases: Vec<Option<Sample>>,
    ) -> io::Result<Self> {
        let user_inputs = vec![UserInputState::default(); contest.problems.len()];
        Self::new_with_session_data(contest, sample_counts, stress_cases, user_inputs)
    }

    pub(crate) fn new_with_session_data(
        contest: &Contest,
        sample_counts: Vec<usize>,
        stress_cases: Vec<Option<Sample>>,
        user_inputs: Vec<UserInputState>,
    ) -> io::Result<Self> {
        if contest.problems.len() != sample_counts.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "problem and sample count lengths differ: {} problems, {} sample counts",
                    contest.problems.len(),
                    sample_counts.len()
                ),
            ));
        }
        if contest.problems.len() != stress_cases.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "problem and stress case lengths differ: {} problems, {} stress cases",
                    contest.problems.len(),
                    stress_cases.len()
                ),
            ));
        }
        if contest.problems.len() != user_inputs.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "problem and user input state lengths differ: {} problems, {} user input states",
                    contest.problems.len(),
                    user_inputs.len()
                ),
            ));
        }

        let problems = contest
            .problems
            .iter()
            .zip(sample_counts)
            .zip(stress_cases)
            .zip(user_inputs)
            .map(|(((problem, sample_cases), stress_case), user_inputs)| {
                let saved_stress_case = stress_case.map(SavedStressCaseState::from);
                ProblemState {
                    index: problem.index.clone(),
                    title: problem.title.clone(),
                    sample_cases,
                    total_cases: sample_cases + if saved_stress_case.is_some() { 1 } else { 0 },
                    saved_stress_case,
                    source: None,
                    run: RunState::default(),
                    stress: StressState::default(),
                    stress_setup: StressSetupState::None,
                    user_inputs,
                    selection_before_draft: DraftReturnSelection::None,
                    detail_mode: DetailMode::Samples,
                }
            })
            .collect::<Vec<_>>();
        let case_selection = problems
            .first()
            .and_then(ProblemState::first_case_selection);

        Ok(Self {
            should_quit: false,
            debug: false,
            samples_pane_enabled: false,
            contest_id: contest.contest_id.clone(),
            problems,
            selected_problem: 0,
            case_selection,
            detail_scroll: 0,
            detail_revision: 0,
            detail_folds: DetailFoldState::default(),
            next_run_id: 1,
        })
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn debug_enabled(&self) -> bool {
        self.debug
    }
    pub fn samples_pane_enabled(&self) -> bool {
        self.samples_pane_enabled
    }

    pub fn toggle_samples_pane(&mut self) {
        self.samples_pane_enabled = !self.samples_pane_enabled;
    }

    pub fn contest_id(&self) -> &str {
        &self.contest_id
    }

    pub fn current_problem(&self) -> Option<&ProblemState> {
        self.problems.get(self.selected_problem)
    }

    pub fn case_selection(&self) -> Option<CaseSelection> {
        self.case_selection
    }

    pub fn selected_test_case(&self) -> Option<usize> {
        match self.case_selection {
            Some(CaseSelection::Test(index)) => Some(index),
            Some(CaseSelection::UserInput(_)) | None => None,
        }
    }

    pub fn selected_user_input(&self) -> Option<UserInputSelection> {
        match self.case_selection {
            Some(CaseSelection::UserInput(selection)) => Some(selection),
            Some(CaseSelection::Test(_)) | None => None,
        }
    }

    pub fn active_user_input_edit(&self) -> Option<&UserInputEditState> {
        self.current_problem()?.user_inputs.ready()?.edit()
    }

    pub fn selected_user_input_edit(&self) -> Option<&UserInputEditState> {
        let selection = self.selected_user_input()?;
        let edit = self.active_user_input_edit()?;
        let matches_selection = matches!(
            (selection, edit.target()),
            (UserInputSelection::Draft, UserInputEditTarget::Draft)
                | (
                    UserInputSelection::Persisted(_),
                    UserInputEditTarget::Persisted(_)
                )
        ) && match (selection, edit.target()) {
            (UserInputSelection::Persisted(selected), UserInputEditTarget::Persisted(target)) => {
                selected == target
            }
            _ => true,
        };
        (self
            .current_problem()
            .is_some_and(|problem| problem.detail_mode == DetailMode::Samples)
            && matches_selection)
            .then_some(edit)
    }

    pub fn user_input_editor_active(&self) -> bool {
        self.selected_user_input_edit().is_some()
    }

    pub fn begin_new_user_input(&mut self) -> Result<bool, UserInputEditStartError> {
        let problem_index = self
            .selected_problem()
            .ok_or(UserInputEditStartError::Unavailable)?;
        let existing_target = self.problems[problem_index]
            .user_inputs
            .ready()
            .ok_or(UserInputEditStartError::Unavailable)?
            .edit()
            .map(UserInputEditState::target);

        match existing_target {
            Some(UserInputEditTarget::Persisted(_)) => {
                return Err(UserInputEditStartError::AlreadyEditing);
            }
            Some(UserInputEditTarget::Draft) => {}
            None => {
                let selection_before_draft =
                    self.problems[problem_index].draft_return_selection(self.case_selection);
                self.problems[problem_index]
                    .user_inputs
                    .ready_mut()
                    .expect("checked ready User Input state must remain ready")
                    .begin_draft()?;
                self.problems[problem_index].selection_before_draft = selection_before_draft;
            }
        }

        let draft = CaseSelection::UserInput(UserInputSelection::Draft);
        let mode_changed = self.problems[problem_index].detail_mode != DetailMode::Samples;
        if self.case_selection == Some(draft) && !mode_changed {
            self.detail_folds.expand(DetailSectionKind::Input);
            return Ok(existing_target.is_none());
        }

        let previous = self.displayed_detail_case();
        self.problems[problem_index].detail_mode = DetailMode::Samples;
        self.case_selection = Some(draft);
        self.reset_folds_if_displayed_case_changed(previous);
        self.detail_folds.expand(DetailSectionKind::Input);
        self.reset_detail_scroll();
        self.invalidate_detail();
        Ok(true)
    }

    pub fn begin_selected_user_input_edit(&mut self) -> Result<bool, UserInputEditStartError> {
        let UserInputSelection::Persisted(id) = self
            .selected_user_input()
            .ok_or(UserInputEditStartError::Unavailable)?
        else {
            return Err(UserInputEditStartError::Unavailable);
        };
        let problem_index = self
            .selected_problem()
            .ok_or(UserInputEditStartError::Unavailable)?;
        self.problems[problem_index]
            .user_inputs
            .ready_mut()
            .ok_or(UserInputEditStartError::Unavailable)?
            .begin_persisted_edit(id)?;
        self.detail_folds.expand(DetailSectionKind::Input);
        self.invalidate_detail();
        Ok(true)
    }

    pub fn cancel_user_input_edit(&mut self) -> bool {
        let Some(problem_index) = self.selected_problem() else {
            return false;
        };
        let Some(target) = self.problems[problem_index]
            .user_inputs
            .ready()
            .and_then(UserInputReadyState::edit)
            .map(UserInputEditState::target)
        else {
            return false;
        };
        let previous = self.displayed_detail_case();
        let cancelled = self.problems[problem_index]
            .user_inputs
            .ready_mut()
            .is_some_and(UserInputReadyState::cancel_edit);
        if !cancelled {
            return false;
        }

        if target == UserInputEditTarget::Draft {
            let return_selection =
                std::mem::take(&mut self.problems[problem_index].selection_before_draft);
            self.case_selection =
                self.problems[problem_index].resolve_draft_return_selection(return_selection);
            self.reconcile_case_selection(problem_index);
            self.reset_folds_if_displayed_case_changed(previous);
            self.reset_detail_scroll();
        }
        self.detail_folds.expand(DetailSectionKind::Input);
        self.invalidate_detail();
        true
    }

    pub fn edit_user_input_insert(&mut self, text: &str) -> bool {
        let changed = self
            .selected_user_input_edit_mut()
            .is_some_and(|edit| edit.insert(text));
        if changed {
            self.invalidate_detail();
        }
        changed
    }

    pub fn edit_user_input_backspace(&mut self) -> bool {
        let changed = self
            .selected_user_input_edit_mut()
            .is_some_and(UserInputEditState::backspace);
        if changed {
            self.invalidate_detail();
        }
        changed
    }

    pub fn edit_user_input_delete(&mut self) -> bool {
        let changed = self
            .selected_user_input_edit_mut()
            .is_some_and(UserInputEditState::delete);
        if changed {
            self.invalidate_detail();
        }
        changed
    }

    pub fn edit_user_input_left(&mut self) -> bool {
        self.selected_user_input_edit_mut()
            .is_some_and(UserInputEditState::move_left)
    }

    pub fn edit_user_input_right(&mut self) -> bool {
        self.selected_user_input_edit_mut()
            .is_some_and(UserInputEditState::move_right)
    }

    pub fn edit_user_input_up(&mut self) -> bool {
        self.selected_user_input_edit_mut()
            .is_some_and(UserInputEditState::move_up)
    }

    pub fn edit_user_input_down(&mut self) -> bool {
        self.selected_user_input_edit_mut()
            .is_some_and(UserInputEditState::move_down)
    }

    pub fn edit_user_input_home(&mut self) -> bool {
        self.selected_user_input_edit_mut()
            .is_some_and(UserInputEditState::move_home)
    }

    pub fn edit_user_input_end(&mut self) -> bool {
        self.selected_user_input_edit_mut()
            .is_some_and(UserInputEditState::move_end)
    }

    fn selected_user_input_edit_mut(&mut self) -> Option<&mut UserInputEditState> {
        if !self.user_input_editor_active() {
            return None;
        }
        let problem = self.problems.get_mut(self.selected_problem)?;
        problem.user_inputs.ready_mut()?.edit_mut()
    }

    #[cfg(test)]
    pub fn selected_case(&self) -> usize {
        self.selected_test_case().unwrap_or(0)
    }

    pub fn selected_problem(&self) -> Option<usize> {
        if self.problems.is_empty() {
            None
        } else {
            Some(self.selected_problem)
        }
    }

    pub fn detail_scroll(&self) -> usize {
        self.detail_scroll
    }

    pub(super) fn detail_revision(&self) -> u64 {
        self.detail_revision
    }

    pub(super) fn detail_fold_state(&self) -> DetailFoldState {
        self.detail_folds
    }

    pub(super) fn toggle_detail_section(&mut self, kind: DetailSectionKind) {
        if kind == DetailSectionKind::Input && self.user_input_editor_active() {
            return;
        }
        self.detail_folds.toggle(kind);
        self.invalidate_detail();
    }

    pub(super) fn invalidate_detail_animation(&mut self) {
        self.invalidate_detail();
    }

    #[cfg(test)]
    pub fn scroll_detail_up(&mut self, lines: usize) -> bool {
        let previous = self.detail_scroll;

        self.detail_scroll = self.detail_scroll.saturating_sub(lines);

        self.detail_scroll != previous
    }

    #[cfg(test)]
    pub fn scroll_detail_down(&mut self, lines: usize) -> bool {
        let previous = self.detail_scroll;

        self.detail_scroll = self.detail_scroll.saturating_add(lines);

        self.detail_scroll != previous
    }

    pub(super) fn set_detail_scroll_from_user(&mut self, absolute_row: usize) -> bool {
        let previous = self.detail_scroll;
        self.detail_scroll = absolute_row;
        self.detail_scroll != previous
    }

    pub fn clamp_detail_scroll(&mut self, max: usize) {
        self.detail_scroll = self.detail_scroll.min(max);
    }

    pub(super) fn reconcile_detail_scroll(&mut self, absolute_row: usize) -> bool {
        let previous = self.detail_scroll;
        self.detail_scroll = absolute_row;
        self.detail_scroll != previous
    }

    fn reset_detail_scroll(&mut self) {
        self.detail_scroll = 0;
    }

    fn invalidate_detail(&mut self) {
        self.detail_revision = self.detail_revision.wrapping_add(1);
    }

    fn displayed_detail_case(&self) -> Option<DisplayedDetailCase> {
        let problem = self.current_problem()?;
        let case = if problem.detail_mode == DetailMode::Samples {
            match self.case_selection {
                Some(CaseSelection::Test(index)) if index < problem.sample_cases => {
                    Some(DisplayedNormalCase::Sample(index))
                }
                Some(CaseSelection::Test(index))
                    if problem.saved_stress_case.is_some() && index == problem.sample_cases =>
                {
                    Some(DisplayedNormalCase::SavedStress)
                }
                Some(CaseSelection::UserInput(selection)) => {
                    Some(DisplayedNormalCase::UserInput(selection))
                }
                Some(CaseSelection::Test(_)) | None => None,
            }
        } else {
            None
        };
        Some(DisplayedDetailCase {
            problem: self.selected_problem,
            mode: problem.detail_mode,
            case,
        })
    }

    fn reset_detail_folds(&mut self) {
        self.detail_folds = DetailFoldState::default();
    }

    fn reset_folds_if_displayed_case_changed(&mut self, previous: Option<DisplayedDetailCase>) {
        if self.displayed_detail_case() != previous {
            self.reset_detail_folds();
        }
    }

    fn selected_test_detail_for(&self, problem: usize) -> Option<usize> {
        (self.selected_problem == problem
            && self
                .problems
                .get(problem)
                .is_some_and(|problem| problem.detail_mode == DetailMode::Samples))
        .then(|| self.selected_test_case())
        .flatten()
    }

    fn displays_attempt_detail(&self, problem: usize, mode: DetailMode) -> bool {
        self.selected_problem == problem
            && self
                .problems
                .get(problem)
                .is_some_and(|state| state.detail_mode == mode)
            && (mode == DetailMode::Stress || self.selected_user_input().is_none())
    }

    fn reconcile_case_selection(&mut self, problem: usize) {
        if self.selected_problem != problem {
            return;
        }
        let Some(problem) = self.problems.get(problem) else {
            self.case_selection = None;
            return;
        };
        if self
            .case_selection
            .is_some_and(|selection| problem.contains_case_selection(selection))
        {
            return;
        }
        self.case_selection = problem.first_case_selection();
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn toggle_debug(&mut self) {
        self.debug = !self.debug;
    }

    pub fn current_source_language(&self) -> Option<Language> {
        self.current_problem()?
            .source
            .as_ref()
            .map(|source| source.language)
    }

    pub fn stress_setup_required(&self, problem: usize) -> bool {
        self.problems.get(problem).is_some_and(|problem| {
            matches!(&problem.stress_setup, StressSetupState::Required { .. })
        })
    }

    pub fn set_stress_setup_required(
        &mut self,
        problem: usize,
        generator_missing: bool,
        brute_missing: bool,
    ) -> bool {
        self.set_stress_setup_state(
            problem,
            StressSetupState::Required {
                generator_missing,
                brute_missing,
            },
        )
    }

    pub fn set_stress_setup_initialized(&mut self, problem: usize) -> bool {
        self.set_stress_setup_state(problem, StressSetupState::Initialized)
    }

    pub fn set_stress_setup_error(&mut self, problem: usize, message: String) -> bool {
        self.set_stress_setup_state(
            problem,
            StressSetupState::Error {
                message: Arc::new(message),
            },
        )
    }

    pub fn clear_stress_setup(&mut self, problem: usize) -> bool {
        let Some(problem_state) = self.problems.get_mut(problem) else {
            return false;
        };
        if problem_state.stress_setup == StressSetupState::None {
            return false;
        }

        problem_state.stress_setup = StressSetupState::None;
        if self.selected_problem == problem {
            self.reset_detail_scroll();
            self.reset_detail_folds();
            self.invalidate_detail();
        }
        true
    }

    fn set_stress_setup_state(&mut self, problem: usize, state: StressSetupState) -> bool {
        let Some(problem_state) = self.problems.get_mut(problem) else {
            return false;
        };
        let changed =
            problem_state.detail_mode != DetailMode::Stress || problem_state.stress_setup != state;
        if !changed {
            return false;
        }

        problem_state.detail_mode = DetailMode::Stress;
        problem_state.stress_setup = state;
        if self.selected_problem == problem {
            self.reset_detail_scroll();
            self.reset_detail_folds();
            self.invalidate_detail();
        }
        true
    }

    pub fn select_problem(&mut self, index: usize) -> bool {
        if index >= self.problems.len() || index == self.selected_problem {
            return false;
        }

        let previous_detail_case = self.displayed_detail_case();
        self.selected_problem = index;
        self.case_selection = self.problems[index].first_case_selection();
        self.reset_folds_if_displayed_case_changed(previous_detail_case);
        self.reset_detail_scroll();
        self.invalidate_detail();
        true
    }

    pub fn next_problem(&mut self) -> bool {
        if self.problems.is_empty() {
            self.selected_problem = 0;
            self.case_selection = None;
            return false;
        }

        let next = (self.selected_problem + 1) % self.problems.len();
        self.select_problem(next)
    }

    pub fn previous_problem(&mut self) -> bool {
        if self.problems.is_empty() {
            self.selected_problem = 0;
            self.case_selection = None;
            return false;
        }

        let previous = if self.selected_problem == 0 {
            self.problems.len() - 1
        } else {
            self.selected_problem - 1
        };
        self.select_problem(previous)
    }

    pub fn next_case(&mut self) -> bool {
        let Some(problem) = self.current_problem() else {
            self.case_selection = None;
            return false;
        };
        let selections = problem.case_selections();
        let Some(next) = self
            .case_selection
            .and_then(|selected| selections.iter().position(|item| *item == selected))
            .map(|index| selections[(index + 1) % selections.len()])
            .or_else(|| selections.first().copied())
        else {
            self.case_selection = None;
            return false;
        };
        let mode_changed = self.problems[self.selected_problem].detail_mode != DetailMode::Samples;
        if Some(next) == self.case_selection && !mode_changed {
            return false;
        }
        let previous_detail_case = self.displayed_detail_case();
        self.problems[self.selected_problem].detail_mode = DetailMode::Samples;
        self.case_selection = Some(next);
        self.reset_folds_if_displayed_case_changed(previous_detail_case);
        self.reset_detail_scroll();
        self.invalidate_detail();
        true
    }

    pub fn previous_case(&mut self) -> bool {
        let Some(problem) = self.current_problem() else {
            self.case_selection = None;
            return false;
        };
        let selections = problem.case_selections();
        let Some(previous) = self
            .case_selection
            .and_then(|selected| selections.iter().position(|item| *item == selected))
            .map(|index| selections[(index + selections.len() - 1) % selections.len()])
            .or_else(|| selections.last().copied())
        else {
            self.case_selection = None;
            return false;
        };
        let mode_changed = self.problems[self.selected_problem].detail_mode != DetailMode::Samples;
        if Some(previous) == self.case_selection && !mode_changed {
            return false;
        }
        let previous_detail_case = self.displayed_detail_case();
        self.problems[self.selected_problem].detail_mode = DetailMode::Samples;
        self.case_selection = Some(previous);
        self.reset_folds_if_displayed_case_changed(previous_detail_case);
        self.reset_detail_scroll();
        self.invalidate_detail();
        true
    }
    pub fn source_changed(&mut self, problem: usize, path: PathBuf, language: Language) -> bool {
        if problem >= self.problems.len() {
            return false;
        }

        let previous_detail_case = self.displayed_detail_case();
        let source = SourceState { path, language };
        debug_assert_eq!(
            source.path.extension(),
            Some(OsStr::new(source.language.extension()))
        );
        self.problems[problem].source = Some(source);
        self.problems[problem].detail_mode = DetailMode::Samples;
        self.selected_problem = problem;
        self.case_selection = self.problems[problem].first_case_selection();
        self.reset_folds_if_displayed_case_changed(previous_detail_case);
        self.reset_detail_scroll();
        self.invalidate_detail();

        true
    }
    fn retire_other_stress_requests(&mut self, keep_problem: usize) {
        for (index, problem) in self.problems.iter_mut().enumerate() {
            if index == keep_problem {
                continue;
            }

            if problem.stress.id.is_some()
                && matches!(
                    problem.stress.phase,
                    StressPhase::Queued | StressPhase::Compiling | StressPhase::Running
                )
            {
                problem.stress.id = None;
                problem.stress.phase = StressPhase::Cancelled;
            }
        }
    }

    /// `queue_stress` is the only transition that creates an active stress generation, and it
    /// retires every other active stress before installing the new one. All event transitions
    /// preserve that identity or move it to a terminal phase, so valid app state has at most one
    /// active stress. Still require a unique, well-formed identity here so cancellation fails
    /// closed instead of choosing an arbitrary generation if that invariant is ever violated.
    pub fn active_stress_identity(&self) -> Option<(usize, RunId)> {
        let mut active = self
            .problems
            .iter()
            .enumerate()
            .filter_map(|(problem, state)| {
                matches!(
                    state.stress.phase,
                    StressPhase::Queued | StressPhase::Compiling | StressPhase::Running
                )
                .then_some((problem, state.stress.id))
            });
        let (problem, run_id) = active.next()?;
        if active.next().is_some() {
            return None;
        }
        run_id.map(|run_id| (problem, run_id))
    }

    pub fn cancel_stress(&mut self, problem: usize, run_id: RunId) -> bool {
        let affects_current_detail = self.selected_problem == problem
            && self
                .problems
                .get(problem)
                .is_some_and(|problem| problem.detail_mode == DetailMode::Stress);
        let Some(stress) = self.current_stress_mut(problem, run_id) else {
            return false;
        };
        if !matches!(
            stress.phase,
            StressPhase::Queued | StressPhase::Compiling | StressPhase::Running
        ) {
            return false;
        }

        stress.phase = StressPhase::Cancelled;
        self.problems[problem].stress_setup = StressSetupState::None;
        if affects_current_detail {
            self.invalidate_detail();
        }
        true
    }

    pub fn queue_run(&mut self, problem: usize) -> Option<RunRequest> {
        let (language, total_cases) = {
            let problem_state = self.problems.get(problem)?;
            let source = problem_state.source.as_ref()?;
            (source.language, problem_state.total_cases)
        };
        self.retire_other_stress_requests(problem);
        let debug = self.debug && language == Language::Cpp;

        let run_id = self.next_run_id;
        self.next_run_id += 1;

        let previous_detail_case = self.displayed_detail_case();
        self.problems[problem].detail_mode = DetailMode::Samples;
        if self.problems[problem].stress.id.is_some()
            && matches!(
                self.problems[problem].stress.phase,
                StressPhase::Queued | StressPhase::Compiling | StressPhase::Running
            )
        {
            self.problems[problem].stress.phase = StressPhase::Cancelled;
        }
        self.problems[problem].stress.id = None;
        self.problems[problem].run = RunState {
            id: Some(run_id),
            phase: RunPhase::Queued,
            language: Some(language),
            debug,
            test_run_started: false,
            accepted: 0,
            total_cases,
            error: None,
            cases: vec![CaseState::default(); total_cases],
        };
        if self.selected_problem == problem {
            let displayed_case_changed = self.displayed_detail_case() != previous_detail_case;
            if displayed_case_changed || self.selected_user_input().is_none() {
                self.reset_detail_scroll();
                self.reset_folds_if_displayed_case_changed(previous_detail_case);
                if !displayed_case_changed {
                    self.reset_detail_folds();
                }
                self.invalidate_detail();
            }
        }

        Some(RunRequest {
            run_id,
            problem,
            language,
            debug,
            kind: RunKind::Samples,
        })
    }

    pub fn queue_stress(&mut self, problem: usize, base_seed: u64) -> Option<RunRequest> {
        let language = self.problems.get(problem)?.source.as_ref()?.language;
        self.retire_other_stress_requests(problem);
        let debug = self.debug && language == Language::Cpp;

        let run_id = self.next_run_id;
        self.next_run_id += 1;

        self.problems[problem].detail_mode = DetailMode::Stress;
        if self.problems[problem].run.id.is_some()
            && matches!(
                self.problems[problem].run.phase,
                RunPhase::Queued | RunPhase::Compiling | RunPhase::Running
            )
        {
            self.problems[problem].run.id = None;
            self.problems[problem].run.phase = RunPhase::Cancelled;
        } else {
            self.problems[problem].run.id = None;
        }
        self.problems[problem].stress = StressState {
            id: Some(run_id),
            phase: StressPhase::Queued,
            language: Some(language),
            base_seed: Some(base_seed),
            case_limit: None,
            case_number: 0,
            seed: None,
            passed: 0,
            elapsed: Duration::ZERO,
            cases_per_second: 0.0,
            failure: None,
            error: None,
        };
        self.problems[problem].stress_setup = StressSetupState::None;
        self.reset_detail_scroll();
        if self.selected_problem == problem {
            self.reset_detail_folds();
            self.invalidate_detail();
        }

        Some(RunRequest {
            run_id,
            problem,
            language,
            debug,
            kind: RunKind::Stress {
                base_seed,
                count: None,
            },
        })
    }

    fn current_run_mut(&mut self, problem: usize, run_id: RunId) -> Option<&mut RunState> {
        let run = &mut self.problems.get_mut(problem)?.run;

        if run.id != Some(run_id) {
            return None;
        }

        Some(run)
    }

    fn current_stress_mut(&mut self, problem: usize, run_id: RunId) -> Option<&mut StressState> {
        let stress = &mut self.problems.get_mut(problem)?.stress;

        if stress.id != Some(run_id) {
            return None;
        }

        Some(stress)
    }

    fn attempt_mode(&self, problem: usize, run_id: RunId) -> Option<DetailMode> {
        let problem = self.problems.get(problem)?;
        if problem.run.id == Some(run_id) {
            Some(DetailMode::Samples)
        } else if problem.stress.id == Some(run_id) {
            Some(DetailMode::Stress)
        } else {
            None
        }
    }

    pub fn run_started(&mut self, problem: usize, run_id: RunId) -> bool {
        let Some(mode) = self.attempt_mode(problem, run_id) else {
            return false;
        };
        let affects_current_detail = self.displays_attempt_detail(problem, mode);

        let changed = match mode {
            DetailMode::Samples => {
                let run = self
                    .current_run_mut(problem, run_id)
                    .expect("attempt mode was checked above");
                if run.phase != RunPhase::Queued {
                    false
                } else {
                    run.phase = match run.language {
                        Some(Language::Cpp) => RunPhase::Compiling,
                        _ => RunPhase::Running,
                    };
                    true
                }
            }
            DetailMode::Stress => {
                let stress = self
                    .current_stress_mut(problem, run_id)
                    .expect("attempt mode was checked above");
                if stress.phase != StressPhase::Queued {
                    false
                } else {
                    stress.phase = match stress.language {
                        Some(Language::Cpp) => StressPhase::Compiling,
                        _ => StressPhase::Running,
                    };
                    true
                }
            }
        };

        if changed && mode == DetailMode::Stress {
            self.problems[problem].stress_setup = StressSetupState::None;
        }

        if changed && affects_current_detail {
            self.invalidate_detail();
        }

        changed
    }
    pub fn run_requeued(&mut self, problem: usize, run_id: RunId) -> bool {
        let affects_current_detail = self.displays_attempt_detail(problem, DetailMode::Samples);
        let Some(total_cases) = self
            .problems
            .get(problem)
            .map(|problem| problem.total_cases)
        else {
            return false;
        };
        let Some(run) = self.current_run_mut(problem, run_id) else {
            return false;
        };

        if !matches!(run.phase, RunPhase::Compiling | RunPhase::Running) {
            return false;
        }

        // run_id/language are logical-request state and survive preemption. Everything below is
        // physical-attempt state and must be fresh before the same logical run starts again.
        run.phase = RunPhase::Queued;
        run.test_run_started = false;
        run.accepted = 0;
        run.total_cases = total_cases;
        run.error = None;
        run.cases = vec![CaseState::default(); total_cases];

        if self.selected_problem == problem {
            self.reconcile_case_selection(problem);
        }
        if affects_current_detail {
            self.reset_detail_scroll();
            self.reset_detail_folds();
            self.invalidate_detail();
        }

        true
    }
    fn apply_test_case_layout(
        &mut self,
        problem: usize,
        run_id: RunId,
        sample_cases: usize,
        stress_case: Option<Sample>,
    ) -> bool {
        let previous_detail_case = self.displayed_detail_case();
        let affects_current_detail = self.displays_attempt_detail(problem, DetailMode::Samples);
        let Some(problem_state) = self.problems.get_mut(problem) else {
            return false;
        };
        if problem_state.run.id != Some(run_id)
            || problem_state.run.test_run_started
            || !matches!(
                problem_state.run.phase,
                RunPhase::Compiling | RunPhase::Running
            )
        {
            return false;
        }

        problem_state.sample_cases = sample_cases;
        problem_state.saved_stress_case = stress_case.map(SavedStressCaseState::from);
        let total_cases = sample_cases
            + if problem_state.saved_stress_case.is_some() {
                1
            } else {
                0
            };
        problem_state.total_cases = total_cases;
        problem_state.run.total_cases = total_cases;
        problem_state.run.cases = vec![CaseState::default(); total_cases];

        if self.selected_problem == problem {
            self.reconcile_case_selection(problem);
        }
        if affects_current_detail {
            self.reset_detail_scroll();
            self.reset_folds_if_displayed_case_changed(previous_detail_case);
            self.invalidate_detail();
        }

        true
    }

    pub fn run_event(&mut self, problem: usize, run_id: RunId, event: TestEvent) -> bool {
        let event = match event {
            TestEvent::TestCaseLayout {
                sample_cases,
                stress_case,
            } => {
                return self.apply_test_case_layout(problem, run_id, sample_cases, stress_case);
            }
            event => event,
        };

        let affects_current_detail = match &event {
            TestEvent::TestCaseAccepted { number, .. }
            | TestEvent::TestCaseComparison { number, .. }
            | TestEvent::TestCaseWrongAnswer { number, .. }
            | TestEvent::TestCaseRuntimeError { number, .. }
            | TestEvent::TestCaseTimedOut { number, .. }
            | TestEvent::TestCaseStderr { number, .. } => {
                number.checked_sub(1) == self.selected_test_detail_for(problem)
            }
            _ => self.displays_attempt_detail(problem, DetailMode::Samples),
        };

        let updated_total_cases = match &event {
            TestEvent::TestRunStarted { total_cases } => Some(*total_cases),
            TestEvent::NoSamples => Some(0),
            _ => None,
        };
        let previous_detail_case = self.displayed_detail_case();

        let Some(run) = self.current_run_mut(problem, run_id) else {
            return false;
        };

        let changed = match event {
            TestEvent::NoSamples
                if !run.test_run_started
                    && matches!(run.phase, RunPhase::Compiling | RunPhase::Running) =>
            {
                run.phase = RunPhase::NoSamples;
                run.accepted = 0;
                run.total_cases = 0;
                run.cases.clear();
                true
            }

            TestEvent::CompileFailed { stderr } if run.phase == RunPhase::Compiling => {
                run.phase = RunPhase::CompileError;
                run.error = Some(Arc::new(stderr));
                true
            }

            TestEvent::CompileTimedOut { .. } if run.phase == RunPhase::Compiling => {
                run.phase = RunPhase::CompileTimedOut;
                true
            }

            TestEvent::TestRunStarted { total_cases }
                if !run.test_run_started
                    && matches!(run.phase, RunPhase::Compiling | RunPhase::Running) =>
            {
                run.phase = RunPhase::Running;
                run.test_run_started = true;
                run.accepted = 0;
                run.total_cases = total_cases;
                run.cases = vec![CaseState::default(); total_cases];
                true
            }

            TestEvent::TestRunFinished {
                accepted,
                total_cases,
            } if run.phase == RunPhase::Running
                && run.test_run_started
                && total_cases == run.cases.len()
                && accepted <= total_cases =>
            {
                run.phase = RunPhase::Finished;
                run.accepted = accepted;
                run.total_cases = total_cases;
                true
            }

            TestEvent::TestCaseAccepted { number, elapsed }
                if run.phase == RunPhase::Running && run.test_run_started =>
            {
                let Some(case) = case_mut(run, number) else {
                    return false;
                };
                if case.verdict != CaseVerdict::Pending {
                    return false;
                }

                case.verdict = CaseVerdict::Accepted;
                case.elapsed = Some(elapsed);

                true
            }

            TestEvent::TestCaseWrongAnswer { number, elapsed }
                if run.phase == RunPhase::Running && run.test_run_started =>
            {
                let Some(case) = case_mut(run, number) else {
                    return false;
                };
                if case.verdict != CaseVerdict::Pending {
                    return false;
                }

                case.verdict = CaseVerdict::WrongAnswer;
                case.elapsed = Some(elapsed);

                true
            }
            TestEvent::TestCaseComparison {
                number,
                input,
                expected,
                actual,
            } if run.phase == RunPhase::Running && run.test_run_started => {
                let Some(case) = case_mut(run, number) else {
                    return false;
                };

                if case.input.is_some() || case.expected.is_some() || case.actual.is_some() {
                    return false;
                }

                case.input = Some(Arc::new(input));
                case.expected = Some(Arc::new(expected));
                case.actual = Some(Arc::new(actual));

                true
            }

            TestEvent::TestCaseRuntimeError { number, elapsed }
                if run.phase == RunPhase::Running && run.test_run_started =>
            {
                let Some(case) = case_mut(run, number) else {
                    return false;
                };
                if case.verdict != CaseVerdict::Pending {
                    return false;
                }

                case.verdict = CaseVerdict::RuntimeError;
                case.elapsed = Some(elapsed);

                true
            }

            TestEvent::TestCaseTimedOut { number, elapsed }
                if run.phase == RunPhase::Running && run.test_run_started =>
            {
                let Some(case) = case_mut(run, number) else {
                    return false;
                };
                if case.verdict != CaseVerdict::Pending {
                    return false;
                }

                case.verdict = CaseVerdict::TimedOut;
                case.elapsed = Some(elapsed);

                true
            }

            TestEvent::TestCaseStderr { number, stderr }
                if run.phase == RunPhase::Running && run.test_run_started =>
            {
                let Some(case) = case_mut(run, number) else {
                    return false;
                };
                if case.stderr.is_some() {
                    return false;
                }

                case.stderr = Some(Arc::new(stderr));

                true
            }

            _ => false,
        };

        if changed && let Some(total_cases) = updated_total_cases {
            self.problems[problem].total_cases = total_cases;
            if total_cases == 0 {
                self.problems[problem].sample_cases = 0;
                self.problems[problem].saved_stress_case = None;
            }
            if self.selected_problem == problem {
                self.reconcile_case_selection(problem);
                let displayed_case_changed = self.displayed_detail_case() != previous_detail_case;
                if affects_current_detail || displayed_case_changed {
                    self.reset_folds_if_displayed_case_changed(previous_detail_case);
                    self.reset_detail_scroll();
                }
            }
        }

        if changed
            && (affects_current_detail || self.displayed_detail_case() != previous_detail_case)
        {
            self.invalidate_detail();
        }

        changed
    }
    #[allow(clippy::too_many_arguments)]
    fn stress_failed(
        &mut self,
        problem: usize,
        run_id: RunId,
        kind: CandidateFailureKind,
        case_number: u64,
        base_seed: u64,
        seed: u64,
        input: String,
        expected: String,
        actual: String,
        stderr: String,
        candidate_elapsed: Duration,
        elapsed: Duration,
        saved_to: PathBuf,
    ) -> bool {
        let previous_detail_case = self.displayed_detail_case();
        let affects_current_detail = self.selected_problem == problem
            && self.problems.get(problem).is_some_and(|state| {
                state.detail_mode == DetailMode::Stress
                    || (state.detail_mode == DetailMode::Samples
                        && state.saved_stress_case.is_some()
                        && self.selected_test_case() == Some(state.sample_cases))
            });
        let Some(problem_state) = self.problems.get_mut(problem) else {
            return false;
        };
        if problem_state.stress.id != Some(run_id)
            || !matches!(
                problem_state.stress.phase,
                StressPhase::Running | StressPhase::Compiling
            )
        {
            return false;
        }

        let input = Arc::new(input);
        let expected = Arc::new(expected);
        let actual = Arc::new(actual);
        let stderr = Arc::new(stderr);

        problem_state.stress.phase = StressPhase::Failed;
        problem_state.stress_setup = StressSetupState::None;
        problem_state.stress.base_seed = Some(base_seed);
        problem_state.stress.case_number = case_number;
        problem_state.stress.seed = Some(seed);
        problem_state.stress.elapsed = elapsed;
        problem_state.stress.failure = Some(StressFailureState {
            kind,
            case_number,
            seed,
            input: Arc::clone(&input),
            expected: Arc::clone(&expected),
            actual: Arc::clone(&actual),
            stderr: Arc::clone(&stderr),
            candidate_elapsed,
            saved_to,
        });

        problem_state.saved_stress_case = Some(SavedStressCaseState {
            input,
            expected: Arc::clone(&expected),
        });
        problem_state.total_cases = problem_state.sample_cases + 1;
        problem_state.run.total_cases = problem_state.total_cases;
        problem_state
            .run
            .cases
            .resize_with(problem_state.total_cases, CaseState::default);

        let stress_index = problem_state.sample_cases;
        if problem_state
            .run
            .cases
            .get(stress_index)
            .is_some_and(|case| case.verdict == CaseVerdict::Accepted)
        {
            problem_state.run.accepted = problem_state.run.accepted.saturating_sub(1);
        }
        if let Some(case) = problem_state.run.cases.get_mut(stress_index) {
            case.verdict = match kind {
                CandidateFailureKind::WrongAnswer => CaseVerdict::WrongAnswer,
                CandidateFailureKind::RuntimeError => CaseVerdict::RuntimeError,
                CandidateFailureKind::TimedOut => CaseVerdict::TimedOut,
            };
            case.elapsed = Some(candidate_elapsed);
            case.expected = Some(expected);
            case.actual = Some(actual);
            case.stderr = (!stderr.is_empty()).then_some(stderr);
        }

        if self.selected_problem == problem {
            self.reconcile_case_selection(problem);
        }
        let displayed_case_changed = self.displayed_detail_case() != previous_detail_case;
        if affects_current_detail || displayed_case_changed {
            self.reset_detail_folds();
            if displayed_case_changed {
                self.reset_detail_scroll();
            }
            self.invalidate_detail();
        }

        true
    }

    pub fn stress_event(&mut self, problem: usize, run_id: RunId, event: StressEvent) -> bool {
        let event = match event {
            StressEvent::Failed {
                kind,
                case_number,
                base_seed,
                seed,
                input,
                expected,
                actual,
                stderr,
                candidate_elapsed,
                elapsed,
                saved_to,
            } => {
                return self.stress_failed(
                    problem,
                    run_id,
                    kind,
                    case_number,
                    base_seed,
                    seed,
                    input,
                    expected,
                    actual,
                    stderr,
                    candidate_elapsed,
                    elapsed,
                    saved_to,
                );
            }
            event => event,
        };

        let affects_current_detail = self.selected_problem == problem
            && self
                .problems
                .get(problem)
                .is_some_and(|problem| problem.detail_mode == DetailMode::Stress);

        let Some(stress) = self.current_stress_mut(problem, run_id) else {
            return false;
        };

        let changed = match event {
            StressEvent::Started {
                base_seed,
                case_limit,
            } if matches!(
                stress.phase,
                StressPhase::Queued | StressPhase::Compiling | StressPhase::Running
            ) =>
            {
                stress.phase = StressPhase::Running;
                stress.base_seed = Some(base_seed);
                stress.case_limit = case_limit;
                stress.case_number = 0;
                stress.seed = None;
                stress.passed = 0;
                stress.elapsed = Duration::ZERO;
                stress.cases_per_second = 0.0;
                stress.failure = None;
                stress.error = None;
                true
            }

            StressEvent::Progress {
                case_number,
                seed,
                passed,
                elapsed,
                cases_per_second,
            } if stress.phase == StressPhase::Running => {
                stress.case_number = case_number;
                stress.seed = Some(seed);
                stress.passed = passed;
                stress.elapsed = elapsed;
                stress.cases_per_second = cases_per_second;
                true
            }

            StressEvent::Finished { cases, elapsed }
                if matches!(stress.phase, StressPhase::Running | StressPhase::Compiling) =>
            {
                stress.phase = StressPhase::Finished;
                stress.passed = cases;
                stress.elapsed = elapsed;
                true
            }

            StressEvent::Cancelled { cases, elapsed }
                if matches!(
                    stress.phase,
                    StressPhase::Queued | StressPhase::Compiling | StressPhase::Running
                ) =>
            {
                stress.phase = StressPhase::Cancelled;
                stress.passed = cases;
                stress.elapsed = elapsed;
                true
            }

            _ => false,
        };

        if changed {
            self.problems[problem].stress_setup = StressSetupState::None;
        }

        if changed && affects_current_detail {
            self.invalidate_detail();
        }

        changed
    }

    pub fn run_completed(&mut self, problem: usize, run_id: RunId) -> bool {
        let Some(mode) = self.attempt_mode(problem, run_id) else {
            return false;
        };
        let affects_current_detail = self.displays_attempt_detail(problem, mode);

        let changed = match mode {
            DetailMode::Samples => {
                let run = self
                    .current_run_mut(problem, run_id)
                    .expect("attempt mode was checked above");
                match run.phase {
                    RunPhase::Queued | RunPhase::Compiling | RunPhase::Running => {
                        run.phase = RunPhase::Finished;
                        true
                    }
                    RunPhase::Idle
                    | RunPhase::Finished
                    | RunPhase::CompileError
                    | RunPhase::CompileTimedOut
                    | RunPhase::NoSamples
                    | RunPhase::Cancelled
                    | RunPhase::Failed => false,
                }
            }
            DetailMode::Stress => {
                let stress = self
                    .current_stress_mut(problem, run_id)
                    .expect("attempt mode was checked above");
                match stress.phase {
                    StressPhase::Queued | StressPhase::Compiling | StressPhase::Running => {
                        stress.phase = StressPhase::Finished;
                        true
                    }
                    StressPhase::Idle
                    | StressPhase::Failed
                    | StressPhase::Finished
                    | StressPhase::Cancelled
                    | StressPhase::Error => false,
                }
            }
        };

        if changed && mode == DetailMode::Stress {
            self.problems[problem].stress_setup = StressSetupState::None;
        }

        if changed && affects_current_detail {
            self.invalidate_detail();
        }

        changed
    }

    pub fn run_failed(&mut self, problem: usize, run_id: RunId, error: String) -> bool {
        let Some(mode) = self.attempt_mode(problem, run_id) else {
            return false;
        };
        let affects_current_detail = self.displays_attempt_detail(problem, mode);

        let changed = match mode {
            DetailMode::Samples => {
                let run = self
                    .current_run_mut(problem, run_id)
                    .expect("attempt mode was checked above");
                if !matches!(
                    run.phase,
                    RunPhase::Queued | RunPhase::Compiling | RunPhase::Running
                ) {
                    false
                } else {
                    run.phase = RunPhase::Failed;
                    run.error = Some(Arc::new(error));
                    true
                }
            }
            DetailMode::Stress => {
                let stress = self
                    .current_stress_mut(problem, run_id)
                    .expect("attempt mode was checked above");
                if !matches!(
                    stress.phase,
                    StressPhase::Queued | StressPhase::Compiling | StressPhase::Running
                ) {
                    false
                } else {
                    stress.phase = StressPhase::Error;
                    stress.error = Some(Arc::new(error));
                    true
                }
            }
        };

        if changed && mode == DetailMode::Stress {
            self.problems[problem].stress_setup = StressSetupState::None;
        }

        if changed && affects_current_detail {
            self.invalidate_detail();
        }

        changed
    }

    pub fn problems(&self) -> &[ProblemState] {
        &self.problems
    }

    pub fn selected_case_state(&self) -> Option<&CaseState> {
        self.current_problem()?
            .run
            .cases
            .get(self.selected_test_case()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Problem;
    use crate::tui::detail::DetailDocument;

    fn contest(problem_count: usize) -> Contest {
        Contest {
            contest_id: "abc123".to_string(),
            problems: (0..problem_count)
                .map(|index| Problem {
                    index: char::from(b'A' + index as u8).to_string(),
                    title: format!("Problem {index}"),
                    task_id: format!("abc123_{index}"),
                    url: format!("https://example.invalid/{index}"),
                    sample_count: 0,
                })
                .collect(),
        }
    }

    fn assert_selection_invariant(app: &WatchApp) {
        if app.problems.is_empty() {
            assert_eq!(app.selected_problem, 0);
            assert_eq!(app.case_selection, None);
            return;
        }

        assert!(app.selected_problem < app.problems.len());
        let problem = app.current_problem().unwrap();
        match app.case_selection {
            Some(selection) => assert!(problem.contains_case_selection(selection)),
            None => assert!(problem.first_case_selection().is_none()),
        }
    }

    fn queued_cpp_app(total_cases: usize) -> (WatchApp, RunId) {
        let mut app = WatchApp::new(&contest(1), vec![total_cases]).unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp));
        let request = app.queue_run(0).unwrap();
        (app, request.run_id)
    }

    fn detail_text(app: &WatchApp) -> String {
        DetailDocument::from_app(app)
            .segments()
            .map(|segment| segment.text())
            .collect()
    }

    fn assert_all_folds_expanded(app: &WatchApp) {
        for kind in [
            DetailSectionKind::Input,
            DetailSectionKind::Expected,
            DetailSectionKind::Actual,
            DetailSectionKind::Stderr,
        ] {
            assert!(!app.detail_fold_state().is_collapsed(kind), "{kind:?}");
        }
    }

    fn loaded_user_inputs(inputs: &[(u64, &str)]) -> UserInputState {
        UserInputState::loaded(
            inputs
                .iter()
                .map(|(id, content)| PersistedUserInputState {
                    id: *id,
                    content: (*content).to_string(),
                })
                .collect(),
        )
    }

    fn assert_case_selection(app: &WatchApp, expected: Option<CaseSelection>) {
        assert_eq!(app.case_selection(), expected);
        assert_selection_invariant(app);
    }

    #[test]
    fn user_input_draft_is_one_unsaved_empty_edit_and_cancel_discards_it() {
        let mut state = UserInputReadyState::new(vec![PersistedUserInputState {
            id: 1,
            content: "persisted\n".to_string(),
        }]);

        state.begin_draft().unwrap();

        let edit = state.edit().unwrap();
        assert_eq!(edit.target(), UserInputEditTarget::Draft);
        assert_eq!(edit.buffer(), "");
        assert_eq!(state.edit_is_dirty(), Some(true));
        assert_eq!(
            state.begin_draft(),
            Err(UserInputEditStartError::AlreadyEditing)
        );
        assert_eq!(state.persisted()[0].content, "persisted\n");

        assert!(state.cancel_edit());
        assert!(state.edit().is_none());
        assert_eq!(state.edit_is_dirty(), None);
        assert!(!state.cancel_edit());
    }

    #[test]
    fn persisted_user_input_edit_is_exact_derived_and_cancel_preserves_content() {
        let original = "alpha\r\n\r\nomega\r\n";
        let mut state = UserInputReadyState::new(vec![PersistedUserInputState {
            id: 3,
            content: original.to_string(),
        }]);

        state.begin_persisted_edit(3).unwrap();
        assert_eq!(
            state.edit().unwrap().target(),
            UserInputEditTarget::Persisted(3)
        );
        assert_eq!(state.edit().unwrap().buffer(), original);
        assert_eq!(state.edit_is_dirty(), Some(false));

        state
            .edit_mut()
            .unwrap()
            .replace_buffer("alpha\r\n\r\nomega\n".to_string());
        assert_eq!(state.edit_is_dirty(), Some(true));

        state
            .edit_mut()
            .unwrap()
            .replace_buffer(original.to_string());
        assert_eq!(state.edit_is_dirty(), Some(false));

        assert!(state.cancel_edit());
        assert_eq!(state.persisted()[0].content, original);
    }

    #[test]
    fn editor_insert_delete_and_horizontal_movement_are_utf8_safe() {
        let mut state = UserInputReadyState::default();
        state.begin_draft().unwrap();
        let edit = state.edit_mut().unwrap();

        assert!(edit.insert("a界🙂"));
        assert_eq!(edit.buffer(), "a界🙂");
        assert_eq!(edit.cursor(), "a界🙂".len());
        assert!(edit.move_left());
        assert_eq!(edit.cursor(), "a界".len());
        assert!(edit.backspace());
        assert_eq!(edit.buffer(), "a🙂");
        assert_eq!(edit.cursor(), 1);
        assert!(edit.delete());
        assert_eq!(edit.buffer(), "a");
        assert_eq!(edit.cursor(), 1);
        assert!(edit.move_left());
        assert_eq!(edit.cursor(), 0);
        assert!(edit.move_right());
        assert_eq!(edit.cursor(), 1);
        assert!(edit.buffer().is_char_boundary(edit.cursor()));
    }

    #[test]
    fn editor_horizontal_movement_never_enters_a_crlf_pair() {
        let mut state = UserInputReadyState::default();
        state.begin_draft().unwrap();
        let edit = state.edit_mut().unwrap();
        assert!(edit.insert("a\r\nb\r\n"));

        for expected in [4, 3, 1, 0] {
            assert!(edit.move_left());
            assert_eq!(edit.cursor(), expected);
            assert!(edit.buffer().is_char_boundary(edit.cursor()));
            assert!(!matches!(
                edit.buffer().as_bytes().get(edit.cursor()),
                Some(b'\n')
            ));
        }
        assert!(!edit.move_left());

        for expected in [1, 3, 4, 6] {
            assert!(edit.move_right());
            assert_eq!(edit.cursor(), expected);
            assert!(edit.buffer().is_char_boundary(edit.cursor()));
            assert!(!matches!(
                edit.buffer().as_bytes().get(edit.cursor()),
                Some(b'\n')
            ));
        }
        assert!(!edit.move_right());
        assert_eq!(edit.buffer(), "a\r\nb\r\n");

        edit.replace_buffer("界\r\n🙂\r\n".to_string());
        assert!(edit.move_left());
        assert_eq!(edit.cursor(), "界\r\n🙂".len());
        assert!(edit.buffer().is_char_boundary(edit.cursor()));
        assert!(edit.move_left());
        assert_eq!(edit.cursor(), "界\r\n".len());
        assert!(edit.move_left());
        assert_eq!(edit.cursor(), "界".len());
        assert!(edit.buffer().is_char_boundary(edit.cursor()));
        assert!(edit.move_right());
        assert_eq!(edit.cursor(), "界\r\n".len());
        assert!(edit.buffer().is_char_boundary(edit.cursor()));
    }

    #[test]
    fn editor_backspace_and_delete_remove_crlf_as_one_logical_newline() {
        let mut edit = UserInputEditState {
            target: UserInputEditTarget::Draft,
            buffer: "a\r\nb\r\n".to_string(),
            cursor: "a\r\n".len(),
            preferred_column: None,
        };
        assert!(edit.backspace());
        assert_eq!(edit.buffer(), "ab\r\n");
        assert_eq!(edit.cursor(), 1);
        assert!(edit.buffer().is_char_boundary(edit.cursor()));

        edit.replace_buffer("a\r\nb\r\n".to_string());
        edit.cursor = 1;
        assert!(edit.delete());
        assert_eq!(edit.buffer(), "ab\r\n");
        assert_eq!(edit.cursor(), 1);
        assert!(edit.buffer().is_char_boundary(edit.cursor()));

        edit.replace_buffer("界\r\n🙂".to_string());
        edit.cursor = "界\r\n".len();
        assert!(edit.backspace());
        assert_eq!(edit.buffer(), "界🙂");
        assert_eq!(edit.cursor(), "界".len());
        assert!(edit.buffer().is_char_boundary(edit.cursor()));

        edit.replace_buffer("界\r\n🙂".to_string());
        edit.cursor = "界".len();
        assert!(edit.delete());
        assert_eq!(edit.buffer(), "界🙂");
        assert_eq!(edit.cursor(), "界".len());
        assert!(edit.buffer().is_char_boundary(edit.cursor()));
    }

    #[test]
    fn editor_leaves_lone_carriage_return_and_line_feed_independent() {
        for newline in ['\r', '\n'] {
            let mut edit = UserInputEditState {
                target: UserInputEditTarget::Draft,
                buffer: format!("a{newline}b"),
                cursor: 1,
                preferred_column: None,
            };
            assert!(edit.move_right());
            assert_eq!(edit.cursor(), 2);
            assert!(edit.buffer().is_char_boundary(edit.cursor()));
            assert!(edit.move_left());
            assert_eq!(edit.cursor(), 1);
            assert!(edit.delete());
            assert_eq!(edit.buffer(), "ab");
            assert_eq!(edit.cursor(), 1);
            assert!(edit.buffer().is_char_boundary(edit.cursor()));

            edit.replace_buffer(format!("a{newline}b"));
            edit.cursor = 2;
            assert!(edit.backspace());
            assert_eq!(edit.buffer(), "ab");
            assert_eq!(edit.cursor(), 1);
            assert!(edit.buffer().is_char_boundary(edit.cursor()));
        }
    }

    #[test]
    fn editor_mutations_that_form_crlf_do_not_leave_the_cursor_between_the_pair() {
        let mut edit = UserInputEditState {
            target: UserInputEditTarget::Draft,
            buffer: "\n".to_string(),
            cursor: 0,
            preferred_column: None,
        };
        assert!(edit.insert("\r"));
        assert_eq!(edit.buffer(), "\r\n");
        assert_eq!(edit.cursor(), 2);
        assert!(edit.buffer().is_char_boundary(edit.cursor()));

        edit.replace_buffer("\rX\n".to_string());
        edit.cursor = 2;
        assert!(edit.backspace());
        assert_eq!(edit.buffer(), "\r\n");
        assert_eq!(edit.cursor(), 0);
        assert!(edit.buffer().is_char_boundary(edit.cursor()));

        edit.replace_buffer("\rX\n".to_string());
        edit.cursor = 1;
        assert!(edit.delete());
        assert_eq!(edit.buffer(), "\r\n");
        assert_eq!(edit.cursor(), 0);
        assert!(edit.buffer().is_char_boundary(edit.cursor()));
    }

    #[test]
    fn editor_enter_home_end_and_vertical_movement_preserve_blank_and_trailing_lines() {
        let mut state = UserInputReadyState::default();
        state.begin_draft().unwrap();
        let edit = state.edit_mut().unwrap();
        assert!(edit.insert("abc\n界\n\nxyz\n"));
        assert_eq!(edit.cursor(), edit.buffer().len());

        assert!(edit.move_left());
        assert_eq!(edit.cursor(), "abc\n界\n\nxyz".len());
        assert!(edit.move_up());
        assert_eq!(edit.cursor(), "abc\n界\n".len());
        assert!(edit.move_up());
        assert_eq!(edit.cursor(), "abc\n界".len());
        assert!(edit.move_home());
        assert_eq!(edit.cursor(), "abc\n".len());
        assert!(edit.move_end());
        assert_eq!(edit.cursor(), "abc\n界".len());
        assert!(edit.move_down());
        assert_eq!(edit.cursor(), "abc\n界\n".len());
        assert_eq!(edit.buffer(), "abc\n界\n\nxyz\n");
    }

    #[test]
    fn editor_exact_insert_never_normalizes_existing_or_pasted_crlf() {
        let original = "a\r\nb\r\n";
        let mut state = UserInputReadyState::new(vec![PersistedUserInputState {
            id: 7,
            content: original.to_string(),
        }]);
        state.begin_persisted_edit(7).unwrap();
        let edit = state.edit_mut().unwrap();
        assert!(edit.insert(" \r\n\n  tail\r\n"));
        assert_eq!(edit.buffer(), "a\r\nb\r\n \r\n\n  tail\r\n");
        assert_eq!(state.persisted()[0].content, original);
    }

    #[test]
    fn editor_line_movement_treats_crlf_as_one_logical_line_break() {
        let mut state = UserInputReadyState::default();
        state.begin_draft().unwrap();
        let edit = state.edit_mut().unwrap();
        assert!(edit.insert("ab\r\n界x\r\n"));

        assert!(edit.move_up());
        assert_eq!(edit.cursor(), "ab\r\n".len());
        assert!(edit.move_end());
        assert_eq!(edit.cursor(), "ab\r\n界x".len());
        assert!(edit.move_up());
        assert_eq!(edit.cursor(), "ab".len());
        assert!(edit.move_down());
        assert_eq!(edit.cursor(), "ab\r\n界x".len());
        assert_eq!(edit.buffer(), "ab\r\n界x\r\n");
    }

    #[test]
    fn new_user_input_creates_selects_and_edits_one_memory_only_draft() {
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![2],
            vec![None],
            vec![loaded_user_inputs(&[(3, "persisted\n")])],
        )
        .unwrap();
        let total_cases = app.current_problem().unwrap().total_cases;
        let run_cases = app.current_problem().unwrap().run.cases.len();

        assert!(app.begin_new_user_input().unwrap());
        assert_eq!(
            app.case_selection(),
            Some(CaseSelection::UserInput(UserInputSelection::Draft))
        );
        assert!(app.user_input_editor_active());
        assert_eq!(app.selected_user_input_edit().unwrap().buffer(), "");
        assert_eq!(app.selected_user_input_edit().unwrap().cursor(), 0);
        assert_eq!(app.current_problem().unwrap().total_cases, total_cases);
        assert_eq!(app.current_problem().unwrap().run.cases.len(), run_cases);
        assert_eq!(
            app.current_problem()
                .unwrap()
                .user_input_selections()
                .filter(|selection| {
                    *selection == CaseSelection::UserInput(UserInputSelection::Draft)
                })
                .count(),
            1
        );
    }

    #[test]
    fn reopening_an_existing_draft_preserves_buffer_cursor_and_single_edit_invariant() {
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![1],
            vec![None],
            vec![loaded_user_inputs(&[])],
        )
        .unwrap();
        app.begin_new_user_input().unwrap();
        assert!(app.edit_user_input_insert("123\n456"));
        assert!(app.edit_user_input_left());
        let cursor = app.selected_user_input_edit().unwrap().cursor();
        assert!(
            app.next_case(),
            "navigation may leave an active edit selected elsewhere"
        );
        assert!(!app.user_input_editor_active());

        assert!(app.begin_new_user_input().unwrap());
        let edit = app.selected_user_input_edit().unwrap();
        assert_eq!(edit.buffer(), "123\n456");
        assert_eq!(edit.cursor(), cursor);
        assert_eq!(edit.target(), UserInputEditTarget::Draft);
    }

    #[test]
    fn new_user_input_never_silently_discards_a_persisted_edit() {
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![0],
            vec![None],
            vec![loaded_user_inputs(&[(3, "original\r\n")])],
        )
        .unwrap();
        app.begin_selected_user_input_edit().unwrap();
        assert!(app.edit_user_input_insert("edited"));
        let buffer = app.selected_user_input_edit().unwrap().buffer().to_string();

        assert_eq!(
            app.begin_new_user_input(),
            Err(UserInputEditStartError::AlreadyEditing)
        );
        assert_eq!(app.selected_user_input_edit().unwrap().buffer(), buffer);
        assert_eq!(
            app.selected_user_input_edit().unwrap().target(),
            UserInputEditTarget::Persisted(3)
        );
        assert_eq!(
            app.current_problem()
                .unwrap()
                .user_inputs
                .ready()
                .unwrap()
                .persisted()[0]
                .content,
            "original\r\n"
        );
    }

    #[test]
    fn draft_cancel_restores_valid_selection_and_reconciles_a_removed_target() {
        let mut app = WatchApp::new(&contest(1), vec![2]).unwrap();
        assert!(app.next_case());
        assert_eq!(app.case_selection(), Some(CaseSelection::Test(1)));
        app.begin_new_user_input().unwrap();
        assert!(app.edit_user_input_insert("discard me"));
        assert!(app.cancel_user_input_edit());
        assert_eq!(app.case_selection(), Some(CaseSelection::Test(1)));
        assert!(app.active_user_input_edit().is_none());

        app.begin_new_user_input().unwrap();
        app.problems[0].sample_cases = 1;
        app.problems[0].total_cases = 1;
        assert!(app.cancel_user_input_edit());
        assert_eq!(app.case_selection(), Some(CaseSelection::Test(0)));
        assert_selection_invariant(&app);
    }

    #[test]
    fn draft_cancel_restores_sample_saved_stress_persisted_and_none_semantically() {
        let stress = || Sample {
            input: "stress input\n".to_string(),
            output: "stress output\n".to_string(),
        };

        let mut sample = WatchApp::new(&contest(1), vec![2]).unwrap();
        assert!(sample.next_case());
        sample.begin_new_user_input().unwrap();
        assert!(sample.cancel_user_input_edit());
        assert_case_selection(&sample, Some(CaseSelection::Test(1)));

        let mut saved = WatchApp::new_with_session_data(
            &contest(1),
            vec![2],
            vec![Some(stress())],
            vec![loaded_user_inputs(&[])],
        )
        .unwrap();
        assert!(saved.next_case());
        assert!(saved.next_case());
        saved.begin_new_user_input().unwrap();
        assert!(saved.cancel_user_input_edit());
        assert_case_selection(&saved, Some(CaseSelection::Test(2)));

        let mut persisted = WatchApp::new_with_session_data(
            &contest(1),
            vec![0],
            vec![None],
            vec![loaded_user_inputs(&[(41, "input\n")])],
        )
        .unwrap();
        persisted.begin_new_user_input().unwrap();
        assert!(persisted.cancel_user_input_edit());
        assert_case_selection(
            &persisted,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(41))),
        );

        let mut none = WatchApp::new_with_session_data(
            &contest(1),
            vec![0],
            vec![None],
            vec![loaded_user_inputs(&[])],
        )
        .unwrap();
        assert_case_selection(&none, None);
        none.begin_new_user_input().unwrap();
        assert!(none.cancel_user_input_edit());
        assert_case_selection(&none, None);
    }

    #[test]
    fn draft_cancel_re_resolves_saved_stress_after_sample_count_changes() {
        let stress = || SavedStressCaseState {
            input: Arc::new("stress input\n".to_string()),
            expected: Arc::new("stress output\n".to_string()),
        };

        for (initial_samples, changed_samples) in [(2, 3), (3, 1)] {
            let mut app = WatchApp::new_with_session_data(
                &contest(1),
                vec![initial_samples],
                vec![Some(Sample {
                    input: "stress input\n".to_string(),
                    output: "stress output\n".to_string(),
                })],
                vec![loaded_user_inputs(&[])],
            )
            .unwrap();
            app.case_selection = Some(CaseSelection::Test(initial_samples));
            app.begin_new_user_input().unwrap();

            app.problems[0].sample_cases = changed_samples;
            app.problems[0].total_cases = changed_samples + 1;
            app.problems[0].saved_stress_case = Some(stress());

            assert!(app.cancel_user_input_edit());
            assert_case_selection(&app, Some(CaseSelection::Test(changed_samples)));
        }
    }

    #[test]
    fn draft_cancel_falls_back_instead_of_aliasing_a_disappeared_semantic_target() {
        let mut sample = WatchApp::new(&contest(1), vec![2]).unwrap();
        sample.case_selection = Some(CaseSelection::Test(1));
        sample.begin_new_user_input().unwrap();
        sample.problems[0].sample_cases = 1;
        sample.problems[0].total_cases = 1;
        assert!(sample.cancel_user_input_edit());
        assert_case_selection(&sample, Some(CaseSelection::Test(0)));

        let mut saved = WatchApp::new_with_session_data(
            &contest(1),
            vec![1],
            vec![Some(Sample {
                input: "stress input\n".to_string(),
                output: "stress output\n".to_string(),
            })],
            vec![loaded_user_inputs(&[])],
        )
        .unwrap();
        saved.case_selection = Some(CaseSelection::Test(1));
        saved.begin_new_user_input().unwrap();
        saved.problems[0].saved_stress_case = None;
        saved.problems[0].total_cases = 1;
        assert!(saved.cancel_user_input_edit());
        assert_case_selection(&saved, Some(CaseSelection::Test(0)));

        let mut persisted = WatchApp::new_with_session_data(
            &contest(1),
            vec![1],
            vec![None],
            vec![loaded_user_inputs(&[(73, "input\n")])],
        )
        .unwrap();
        persisted.case_selection =
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(73)));
        persisted.begin_new_user_input().unwrap();
        persisted.problems[0]
            .user_inputs
            .ready_mut()
            .unwrap()
            .persisted
            .clear();
        assert!(persisted.cancel_user_input_edit());
        assert_case_selection(&persisted, Some(CaseSelection::Test(0)));
    }

    #[test]
    fn persisted_cancel_keeps_selection_and_exact_backend_loaded_content() {
        let original = "4\r\n1 2 3 4\r\n";
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![0],
            vec![None],
            vec![loaded_user_inputs(&[(9, original)])],
        )
        .unwrap();
        app.begin_selected_user_input_edit().unwrap();
        assert_eq!(app.selected_user_input_edit().unwrap().buffer(), original);
        assert!(app.edit_user_input_insert("x"));
        assert!(app.cancel_user_input_edit());
        assert_eq!(
            app.case_selection(),
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(9)))
        );
        assert_eq!(
            app.current_problem()
                .unwrap()
                .user_inputs
                .ready()
                .unwrap()
                .persisted()[0]
                .content,
            original
        );
    }

    #[test]
    fn edit_state_is_problem_local_and_survives_navigation_and_run_events() {
        let mut app = WatchApp::new_with_session_data(
            &contest(2),
            vec![1, 0],
            vec![None, None],
            vec![
                loaded_user_inputs(&[(3, "A original\n")]),
                loaded_user_inputs(&[(8, "B original\n")]),
            ],
        )
        .unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let request = app.queue_run(0).unwrap();
        assert!(app.next_case());
        app.begin_selected_user_input_edit().unwrap();
        assert!(app.edit_user_input_insert("A edit\n"));
        assert!(app.edit_user_input_left());
        let selection = app.case_selection();
        let buffer = app.selected_user_input_edit().unwrap().buffer().to_string();
        let cursor = app.selected_user_input_edit().unwrap().cursor();

        assert!(app.run_started(0, request.run_id));
        assert!(app.run_requeued(0, request.run_id));
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_completed(0, request.run_id));
        assert_eq!(app.case_selection(), selection);
        assert_eq!(app.selected_user_input_edit().unwrap().buffer(), buffer);
        assert_eq!(app.selected_user_input_edit().unwrap().cursor(), cursor);

        let failed_request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, failed_request.run_id));
        assert!(app.run_failed(0, failed_request.run_id, "runner failed".to_string()));
        assert_eq!(app.case_selection(), selection);
        assert_eq!(app.selected_user_input_edit().unwrap().buffer(), buffer);
        assert_eq!(app.selected_user_input_edit().unwrap().cursor(), cursor);

        assert!(app.next_problem());
        app.begin_selected_user_input_edit().unwrap();
        assert!(app.edit_user_input_insert("B edit"));
        assert!(app.previous_problem());
        assert_eq!(app.active_user_input_edit().unwrap().buffer(), buffer);
        assert_eq!(app.active_user_input_edit().unwrap().cursor(), cursor);
        assert_eq!(
            app.problems[1]
                .user_inputs
                .ready()
                .unwrap()
                .edit()
                .unwrap()
                .buffer(),
            "B original\nB edit"
        );
    }

    #[test]
    fn edit_content_mutation_keeps_detail_identity_scroll_and_non_input_folds() {
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![0],
            vec![None],
            vec![loaded_user_inputs(&[(3, "original")])],
        )
        .unwrap();
        app.begin_selected_user_input_edit().unwrap();
        app.detail_folds.expected_collapsed = true;
        assert!(app.scroll_detail_down(17));
        let identity = app.displayed_detail_case();
        let revision = app.detail_revision();

        assert!(app.edit_user_input_insert(" changed"));
        assert_eq!(app.displayed_detail_case(), identity);
        assert_eq!(app.detail_scroll(), 17);
        assert!(app.detail_folds.expected_collapsed);
        assert!(!app.detail_folds.input_collapsed);
        assert_ne!(app.detail_revision(), revision);
        app.toggle_detail_section(DetailSectionKind::Input);
        assert!(
            !app.detail_folds.input_collapsed,
            "editing Input stays expanded"
        );
    }

    #[test]
    fn missing_persisted_user_input_edit_has_an_explicit_result() {
        let mut state = UserInputReadyState::new(vec![PersistedUserInputState {
            id: 1,
            content: "one".to_string(),
        }]);

        assert_eq!(
            state.begin_persisted_edit(3),
            Err(UserInputEditStartError::PersistedInputNotFound(3))
        );
        assert!(state.edit().is_none());
    }

    #[test]
    fn user_input_runtime_state_is_problem_local() {
        let mut app = WatchApp::new_with_session_data(
            &contest(2),
            vec![0, 0],
            vec![None, None],
            vec![
                loaded_user_inputs(&[(1, "A persisted\n")]),
                loaded_user_inputs(&[(3, "B persisted\r\n")]),
            ],
        )
        .unwrap();

        app.problems[0]
            .user_inputs
            .ready_mut()
            .unwrap()
            .begin_draft()
            .unwrap();
        app.problems[1]
            .user_inputs
            .ready_mut()
            .unwrap()
            .begin_persisted_edit(3)
            .unwrap();
        app.problems[1]
            .user_inputs
            .ready_mut()
            .unwrap()
            .edit_mut()
            .unwrap()
            .replace_buffer("B edited\r\n".to_string());

        let a = app.problems[0].user_inputs.ready().unwrap();
        assert_eq!(a.edit().unwrap().target(), UserInputEditTarget::Draft);
        assert_eq!(a.persisted()[0].content, "A persisted\n");
        let b = app.problems[1].user_inputs.ready().unwrap();
        assert_eq!(
            b.edit().unwrap().target(),
            UserInputEditTarget::Persisted(3)
        );
        assert_eq!(b.persisted()[0].content, "B persisted\r\n");
        assert_eq!(b.edit_is_dirty(), Some(true));
    }

    #[test]
    fn fresh_watch_app_does_not_retain_user_input_draft_or_edit_state() {
        let contest = contest(2);
        let session_inputs = || {
            vec![
                loaded_user_inputs(&[(1, "A persisted\n")]),
                loaded_user_inputs(&[(3, "B persisted\r\n")]),
            ]
        };
        let mut old = WatchApp::new_with_session_data(
            &contest,
            vec![0, 0],
            vec![None, None],
            session_inputs(),
        )
        .unwrap();
        old.problems[0]
            .user_inputs
            .ready_mut()
            .unwrap()
            .begin_draft()
            .unwrap();
        old.problems[1]
            .user_inputs
            .ready_mut()
            .unwrap()
            .begin_persisted_edit(3)
            .unwrap();

        let fresh = WatchApp::new_with_session_data(
            &contest,
            vec![0, 0],
            vec![None, None],
            session_inputs(),
        )
        .unwrap();

        assert!(
            fresh.problems.iter().all(|problem| problem
                .user_inputs
                .ready()
                .unwrap()
                .edit()
                .is_none())
        );
        assert_eq!(
            fresh.problems[0].user_inputs.ready().unwrap().persisted()[0].content,
            "A persisted\n"
        );
        assert_eq!(
            fresh.problems[1].user_inputs.ready().unwrap().persisted()[0].content,
            "B persisted\r\n"
        );
    }

    #[test]
    fn samples_stress_and_user_inputs_share_semantic_navigation_without_changing_test_counts() {
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![2],
            vec![Some(Sample {
                input: "stress input\n".to_string(),
                output: "stress output\n".to_string(),
            })],
            vec![loaded_user_inputs(&[(1, "one\n"), (3, "three\n")])],
        )
        .unwrap();

        assert_eq!(app.problems[0].total_cases, 3);
        assert_eq!(app.problems[0].run.cases.len(), 0);
        assert_case_selection(&app, Some(CaseSelection::Test(0)));
        assert_eq!(app.problems[0].detail_mode, DetailMode::Samples);

        assert!(app.next_case());
        assert_case_selection(&app, Some(CaseSelection::Test(1)));
        assert!(app.next_case());
        assert_case_selection(&app, Some(CaseSelection::Test(2)));
        assert!(app.next_case());
        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(1))),
        );
        assert!(app.next_case());
        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(3))),
        );
        assert!(app.next_case());
        assert_case_selection(&app, Some(CaseSelection::Test(0)));

        assert!(app.previous_case());
        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(3))),
        );
        assert_eq!(app.problems[0].total_cases, 3);
        assert!(app.problems[0].run.cases.is_empty());

        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        assert!(app.queue_stress(0, 1).is_some());
        assert_eq!(app.problems[0].detail_mode, DetailMode::Stress);
        assert!(app.next_case());
        assert_eq!(app.problems[0].detail_mode, DetailMode::Samples);
        assert_case_selection(&app, Some(CaseSelection::Test(1)));
    }

    #[test]
    fn stress_setup_state_is_per_problem_and_does_not_allocate_a_run() {
        let mut app = WatchApp::new(&contest(2), vec![1, 1]).unwrap();

        assert!(app.set_stress_setup_required(0, true, false));
        assert_eq!(app.problems[0].detail_mode, DetailMode::Stress);
        assert_eq!(app.problems[0].stress.id, None);
        assert_eq!(app.problems[0].stress.phase, StressPhase::Idle);
        assert_eq!(app.problems[1].stress_setup, StressSetupState::None);

        assert!(app.select_problem(1));
        assert_eq!(
            app.current_problem().unwrap().stress_setup,
            StressSetupState::None
        );
        assert!(app.select_problem(0));
        assert_eq!(
            app.current_problem().unwrap().stress_setup,
            StressSetupState::Required {
                generator_missing: true,
                brute_missing: false,
            }
        );
    }

    #[test]
    fn queuing_real_stress_clears_setup_guidance_and_uses_the_first_real_run_id() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        assert!(app.set_stress_setup_initialized(0));
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));

        let request = app.queue_stress(0, 123).unwrap();

        assert_eq!(request.run_id, 1);
        assert_eq!(app.problems[0].stress_setup, StressSetupState::None);
        assert_eq!(app.problems[0].stress.phase, StressPhase::Queued);
    }

    fn collapse_all_folds(app: &mut WatchApp) {
        for kind in [
            DetailSectionKind::Input,
            DetailSectionKind::Expected,
            DetailSectionKind::Actual,
            DetailSectionKind::Stderr,
        ] {
            app.toggle_detail_section(kind);
            assert!(app.detail_fold_state().is_collapsed(kind), "{kind:?}");
        }
    }

    fn foldable_sample_app(language: Language) -> WatchApp {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        let path = match language {
            Language::Cpp => PathBuf::from("A.cpp"),
            Language::Python => PathBuf::from("A.py"),
        };
        assert!(app.source_changed(0, path, language));
        let request = app.queue_run(0).unwrap();
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
                input: "input".to_string(),
                expected: "expected".to_string(),
                actual: "actual".to_string(),
            },
        ));
        app
    }

    #[test]
    fn fold_state_starts_expanded() {
        let app = WatchApp::new(&contest(1), vec![2]).unwrap();
        assert_all_folds_expanded(&app);
    }

    #[test]
    fn explicit_rerun_replaces_the_displayed_result_and_resets_all_folds() {
        let mut app = foldable_sample_app(Language::Python);
        collapse_all_folds(&mut app);

        assert!(app.queue_run(0).is_some());

        assert_all_folds_expanded(&app);
        assert!(detail_text(&app).contains("Queued..."));
    }

    #[test]
    fn source_save_rerun_resets_all_folds_when_it_replaces_the_current_result() {
        let mut app = foldable_sample_app(Language::Python);
        collapse_all_folds(&mut app);
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        assert!(
            app.detail_fold_state()
                .is_collapsed(DetailSectionKind::Actual)
        );

        assert!(app.queue_run(0).is_some());

        assert_all_folds_expanded(&app);
    }

    #[test]
    fn sample_problem_and_samples_stress_navigation_reset_all_folds() {
        let mut app = WatchApp::new_with_stress_cases(
            &contest(2),
            vec![2, 1],
            vec![
                Some(Sample {
                    input: "persisted input".to_string(),
                    output: "persisted output".to_string(),
                }),
                None,
            ],
        )
        .unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));

        collapse_all_folds(&mut app);
        assert!(app.next_case());
        assert_all_folds_expanded(&app);

        collapse_all_folds(&mut app);
        assert!(app.select_problem(1));
        assert_all_folds_expanded(&app);

        assert!(app.select_problem(0));
        collapse_all_folds(&mut app);
        assert!(app.queue_stress(0, 123).is_some());
        assert_all_folds_expanded(&app);

        collapse_all_folds(&mut app);
        assert!(app.next_case());
        assert_all_folds_expanded(&app);

        collapse_all_folds(&mut app);
        assert!(app.previous_case());
        assert_all_folds_expanded(&app);
        assert_eq!(app.selected_case(), 0);

        collapse_all_folds(&mut app);
        assert!(app.previous_case());
        assert_eq!(app.selected_case(), 2, "persisted Stress case is last");
        assert_all_folds_expanded(&app);

        collapse_all_folds(&mut app);
        assert!(app.next_case());
        assert_eq!(app.selected_case(), 0);
        assert_all_folds_expanded(&app);
    }

    #[test]
    fn problem_navigation_wraps_in_both_directions() {
        let mut app = WatchApp::new(&contest(3), vec![1, 1, 1]).unwrap();

        app.previous_problem();
        assert_eq!(app.current_problem().unwrap().index, "C");
        app.next_problem();
        assert_eq!(app.current_problem().unwrap().index, "A");
        app.next_problem();
        assert_eq!(app.current_problem().unwrap().index, "B");
        assert_selection_invariant(&app);
    }

    #[test]
    fn case_navigation_wraps_in_both_directions() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();

        app.previous_case();
        assert_eq!(app.selected_case(), 2);
        app.next_case();
        assert_eq!(app.selected_case(), 0);
        app.next_case();
        assert_eq!(app.selected_case(), 1);
        assert_selection_invariant(&app);
    }

    #[test]
    fn saved_stress_case_is_part_of_case_navigation() {
        let mut app = WatchApp::new_with_stress_cases(
            &contest(1),
            vec![2],
            vec![Some(Sample {
                input: "1 2\n".to_string(),
                output: "3\n".to_string(),
            })],
        )
        .unwrap();

        assert_eq!(app.problems[0].sample_cases, 2);
        assert_eq!(app.problems[0].total_cases, 3);
        app.previous_case();
        assert_eq!(app.selected_case(), 2);
        app.next_case();
        assert_eq!(app.selected_case(), 0);
    }

    #[test]
    fn samples_and_non_contiguous_user_input_ids_navigate_in_both_directions() {
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![1],
            vec![None],
            vec![loaded_user_inputs(&[
                (8, "eight"),
                (1, "one"),
                (3, "three"),
            ])],
        )
        .unwrap();

        for expected in [
            CaseSelection::UserInput(UserInputSelection::Persisted(1)),
            CaseSelection::UserInput(UserInputSelection::Persisted(3)),
            CaseSelection::UserInput(UserInputSelection::Persisted(8)),
            CaseSelection::Test(0),
        ] {
            assert!(app.next_case());
            assert_case_selection(&app, Some(expected));
        }

        for expected in [
            CaseSelection::UserInput(UserInputSelection::Persisted(8)),
            CaseSelection::UserInput(UserInputSelection::Persisted(3)),
            CaseSelection::UserInput(UserInputSelection::Persisted(1)),
            CaseSelection::Test(0),
        ] {
            assert!(app.previous_case());
            assert_case_selection(&app, Some(expected));
        }
    }

    #[test]
    fn zero_samples_selects_user_input_and_no_selectable_cases_selects_nothing() {
        let mut user_input_only = WatchApp::new_with_session_data(
            &contest(1),
            vec![0],
            vec![None],
            vec![loaded_user_inputs(&[(3, "three")])],
        )
        .unwrap();
        assert_case_selection(
            &user_input_only,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(3))),
        );
        assert!(!user_input_only.next_case());
        assert!(!user_input_only.previous_case());

        let mut empty = WatchApp::new(&contest(1), vec![0]).unwrap();
        assert_case_selection(&empty, None);
        assert!(!empty.next_case());
        assert!(!empty.previous_case());
        assert_case_selection(&empty, None);
    }

    #[test]
    fn draft_is_the_last_semantic_item_without_becoming_a_test_case() {
        let mut ready = UserInputReadyState::new(vec![PersistedUserInputState {
            id: 3,
            content: "three".to_string(),
        }]);
        ready.begin_draft().unwrap();
        ready
            .edit_mut()
            .unwrap()
            .replace_buffer("draft\r\nbody\r\n".to_string());
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![1],
            vec![None],
            vec![UserInputState::Ready(ready)],
        )
        .unwrap();

        assert!(app.previous_case());
        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Draft)),
        );
        assert_eq!(app.current_problem().unwrap().total_cases, 1);
        assert!(app.current_problem().unwrap().run.cases.is_empty());
    }

    #[test]
    fn problem_switch_selects_the_destination_first_semantic_item() {
        let mut app = WatchApp::new_with_session_data(
            &contest(3),
            vec![2, 0, 0],
            vec![None, None, None],
            vec![
                UserInputState::default(),
                loaded_user_inputs(&[(8, "eight")]),
                UserInputState::default(),
            ],
        )
        .unwrap();
        assert!(app.next_case());

        assert!(app.select_problem(1));
        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(8))),
        );
        assert!(app.select_problem(2));
        assert_case_selection(&app, None);
        assert!(app.select_problem(0));
        assert_case_selection(&app, Some(CaseSelection::Test(0)));
    }

    #[test]
    fn changing_problem_resets_selected_case() {
        let mut app = WatchApp::new(&contest(2), vec![3, 5]).unwrap();

        app.previous_case();
        assert_eq!(app.selected_case(), 2);

        app.next_problem();

        assert_eq!(app.current_problem().unwrap().index, "B");
        assert_eq!(app.selected_case(), 0);
        assert_selection_invariant(&app);
    }

    #[test]
    fn changing_to_a_problem_without_samples_resets_the_case() {
        let mut app = WatchApp::new(&contest(2), vec![3, 0]).unwrap();
        app.previous_case();

        assert!(app.select_problem(1));
        assert_eq!(app.selected_case(), 0);
        app.next_case();
        app.previous_case();
        assert_eq!(app.selected_case(), 0);
        assert_selection_invariant(&app);
    }

    #[test]
    fn no_problems_is_safe_for_all_navigation() {
        let mut app = WatchApp::new(&contest(0), vec![]).unwrap();

        app.next_problem();
        app.previous_problem();
        app.next_case();
        app.previous_case();

        assert!(app.current_problem().is_none());
        assert!(!app.select_problem(0));
        assert_selection_invariant(&app);
    }

    #[test]
    fn one_problem_and_one_sample_remain_selected() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();

        app.next_problem();
        app.previous_problem();
        app.next_case();
        app.previous_case();

        assert_eq!(app.current_problem().unwrap().index, "A");
        assert_eq!(app.selected_case(), 0);
        assert_selection_invariant(&app);
    }

    #[test]
    fn sample_count_length_mismatch_is_rejected() {
        for sample_counts in [vec![1], vec![1, 2, 3]] {
            let error = WatchApp::new(&contest(2), sample_counts).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("lengths differ"));
        }
    }

    #[test]
    fn debug_toggle_and_quit_update_only_their_state() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();

        assert!(!app.debug_enabled());
        app.toggle_debug();
        assert!(app.debug_enabled());
        app.toggle_debug();
        assert!(!app.debug_enabled());

        assert!(!app.should_quit());
        app.quit();
        assert!(app.should_quit());
        assert_selection_invariant(&app);
    }
    #[test]
    fn source_change_selects_problem_and_resets_case() {
        let mut app = WatchApp::new(&contest(2), vec![3, 3]).unwrap();

        app.previous_case();
        assert_eq!(app.selected_case(), 2);

        assert!(app.source_changed(1, PathBuf::from("B.cpp"), Language::Cpp,));

        assert_eq!(app.current_problem().unwrap().index, "B");
        assert_eq!(app.selected_case(), 0);

        let source = app.current_problem().unwrap().source.as_ref().unwrap();

        assert_eq!(source.path, PathBuf::from("B.cpp"));
        assert_eq!(source.language, Language::Cpp);

        assert_selection_invariant(&app);
    }

    #[test]
    fn source_change_on_the_current_problem_still_resets_case() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();
        app.previous_case();
        assert_eq!(app.selected_case(), 2);

        assert!(app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp));

        assert_eq!(app.selected_case(), 0);
        assert_selection_invariant(&app);
    }

    #[test]
    fn latest_source_change_replaces_previous_source() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();

        assert!(app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp,));

        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python,));

        let source = app.current_problem().unwrap().source.as_ref().unwrap();

        assert_eq!(source.path, PathBuf::from("A.py"));
        assert_eq!(source.language, Language::Python);
    }

    #[test]
    fn invalid_source_change_is_ignored() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();

        assert!(!app.source_changed(100, PathBuf::from("Z.cpp"), Language::Cpp,));

        assert_eq!(app.current_problem().unwrap().index, "A");
        assert!(app.current_problem().unwrap().source.is_none());
        assert_selection_invariant(&app);
    }
    #[test]
    fn queue_run_uses_latest_source_and_marks_problem_queued() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let request = app.queue_run(0).unwrap();

        assert_eq!(request.run_id, 1);
        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(!request.debug);

        let run = &app.current_problem().unwrap().run;

        assert_eq!(run.id, Some(1));
        assert_eq!(run.phase, RunPhase::Queued);
        assert_eq!(run.language, Some(Language::Cpp));
    }

    #[test]
    fn queue_run_assigns_increasing_run_ids() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let first = app.queue_run(0).unwrap();
        let second = app.queue_run(0).unwrap();

        assert_eq!(first.run_id, 1);
        assert_eq!(second.run_id, 2);

        assert_eq!(app.current_problem().unwrap().run.id, Some(2));
    }

    #[test]
    fn queue_run_enables_debug_only_for_cpp() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();

        app.toggle_debug();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let cpp = app.queue_run(0).unwrap();
        assert!(cpp.debug);
        assert!(app.current_problem().unwrap().run.debug);

        app.source_changed(0, PathBuf::from("A.py"), Language::Python);

        let python = app.queue_run(0).unwrap();
        assert!(!python.debug);
        assert!(!app.current_problem().unwrap().run.debug);
    }

    #[test]
    fn queue_run_without_source_returns_none() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();

        assert!(app.queue_run(0).is_none());
        assert_eq!(app.current_problem().unwrap().run.phase, RunPhase::Idle);
    }
    #[test]
    fn run_messages_advance_cpp_run_to_finished() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let request = app.queue_run(0).unwrap();

        assert!(app.run_started(0, request.run_id));

        assert_eq!(
            app.current_problem().unwrap().run.phase,
            RunPhase::Compiling
        );

        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 3 },
        ));

        assert_eq!(app.current_problem().unwrap().run.phase, RunPhase::Running);

        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunFinished {
                accepted: 3,
                total_cases: 3,
            },
        ));

        let run = &app.current_problem().unwrap().run;

        assert_eq!(run.phase, RunPhase::Finished);
        assert_eq!(run.accepted, 3);
        assert_eq!(run.total_cases, 3);
    }
    #[test]
    fn stale_run_messages_are_ignored() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let old = app.queue_run(0).unwrap();
        let current = app.queue_run(0).unwrap();

        assert!(!app.run_started(0, old.run_id));

        assert_eq!(app.current_problem().unwrap().run.id, Some(current.run_id));

        assert_eq!(app.current_problem().unwrap().run.phase, RunPhase::Queued);

        assert!(!app.run_event(
            0,
            old.run_id,
            TestEvent::TestRunFinished {
                accepted: 3,
                total_cases: 3,
            },
        ));
        assert!(!app.run_completed(0, old.run_id));
        assert!(!app.run_failed(0, old.run_id, "old failure".to_string()));

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.id, Some(current.run_id));
        assert_eq!(run.phase, RunPhase::Queued);
        assert!(run.error.is_none());
    }
    #[test]
    fn completed_does_not_overwrite_compile_error() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let request = app.queue_run(0).unwrap();

        app.run_started(0, request.run_id);

        app.run_event(
            0,
            request.run_id,
            TestEvent::CompileFailed {
                stderr: "compile error".to_string(),
            },
        );

        assert!(!app.run_completed(0, request.run_id));

        let run = &app.current_problem().unwrap().run;

        assert_eq!(run.phase, RunPhase::CompileError);

        assert_eq!(
            run.error.as_ref().map(|text| text.as_str()),
            Some("compile error")
        );
    }

    #[test]
    fn terminal_run_states_ignore_late_messages() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::CompileTimedOut {
                elapsed: std::time::Duration::from_secs(1),
            },
        ));

        assert!(!app.run_started(0, request.run_id));
        assert!(!app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 3 },
        ));
        assert!(!app.run_completed(0, request.run_id));
        assert!(!app.run_failed(0, request.run_id, "late failure".to_string()));

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.phase, RunPhase::CompileTimedOut);
        assert!(run.error.is_none());
    }

    #[test]
    fn fatal_run_error_transitions_only_an_active_run_to_failed() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_run(0).unwrap();

        assert!(app.run_started(0, request.run_id));
        assert!(app.run_failed(0, request.run_id, "runner failed".to_string()));
        assert!(!app.run_completed(0, request.run_id));

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.phase, RunPhase::Failed);
        assert_eq!(
            run.error.as_ref().map(|text| text.as_str()),
            Some("runner failed")
        );
    }

    #[test]
    fn no_samples_is_terminal_and_is_not_overwritten_by_completed() {
        let mut app = WatchApp::new(&contest(1), vec![2]).unwrap();
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_run(0).unwrap();
        app.previous_case();
        app.scroll_detail_down(10);

        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(0, request.run_id, TestEvent::NoSamples));
        assert!(!app.run_completed(0, request.run_id));

        let run = &app.current_problem().unwrap().run;
        assert_eq!(run.phase, RunPhase::NoSamples);
        assert_eq!(run.accepted, 0);
        assert_eq!(run.total_cases, 0);
        assert!(run.cases.is_empty());
        assert_eq!(app.current_problem().unwrap().total_cases, 0);
        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 0);
    }
    #[test]
    fn case_events_are_stored_for_sample_detail() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let request = app.queue_run(0).unwrap();

        assert!(app.run_started(0, request.run_id));

        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 3 },
        ));

        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 2,
                elapsed: Duration::from_millis(6),
            },
        ));

        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseComparison {
                number: 2,
                input: "1\n".to_owned(),
                expected: "Yes\n".to_owned(),
                actual: "No\n".to_owned(),
            },
        ));

        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseStderr {
                number: 2,
                stderr: "debug: answer = No\n".to_string(),
            },
        ));

        let case = &app.current_problem().unwrap().run.cases[1];

        assert_eq!(case.verdict, CaseVerdict::WrongAnswer);
        assert_eq!(case.elapsed, Some(Duration::from_millis(6)));
        assert_eq!(case.input.as_ref().map(|text| text.as_str()), Some("1\n"));
        assert_eq!(
            case.expected.as_ref().map(|text| text.as_str()),
            Some("Yes\n")
        );
        assert_eq!(case.actual.as_ref().map(|text| text.as_str()), Some("No\n"));
        assert_eq!(
            case.stderr.as_ref().map(|text| text.as_str()),
            Some("debug: answer = No\n")
        );
    }

    #[test]
    fn event_strings_move_into_shared_raw_state_without_copying_their_buffers() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));

        let expected = "expected ".repeat(10_000);
        let actual = "actual ".repeat(10_000);
        let stderr = "stderr ".repeat(10_000);
        let input = "input ".repeat(10_000);
        let input_ptr = input.as_ptr();
        let expected_ptr = expected.as_ptr();
        let actual_ptr = actual.as_ptr();
        let stderr_ptr = stderr.as_ptr();

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
                input,
                expected,
                actual,
            },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseStderr { number: 1, stderr },
        ));

        let case = &app.current_problem().unwrap().run.cases[0];
        let input: &Arc<String> = case.input.as_ref().unwrap();
        let expected: &Arc<String> = case.expected.as_ref().unwrap();
        let actual: &Arc<String> = case.actual.as_ref().unwrap();
        let stderr: &Arc<String> = case.stderr.as_ref().unwrap();
        assert_eq!(input.as_ptr(), input_ptr);
        assert_eq!(expected.as_ptr(), expected_ptr);
        assert_eq!(actual.as_ptr(), actual_ptr);
        assert_eq!(stderr.as_ptr(), stderr_ptr);

        let mut compile_app = WatchApp::new(&contest(1), vec![1]).unwrap();
        compile_app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let compile = compile_app.queue_run(0).unwrap();
        assert!(compile_app.run_started(0, compile.run_id));
        let compiler_output = "compiler output ".repeat(10_000);
        let compiler_output_ptr = compiler_output.as_ptr();
        assert!(compile_app.run_event(
            0,
            compile.run_id,
            TestEvent::CompileFailed {
                stderr: compiler_output,
            },
        ));
        assert_eq!(
            compile_app
                .current_problem()
                .unwrap()
                .run
                .error
                .as_ref()
                .unwrap()
                .as_ptr(),
            compiler_output_ptr
        );

        let mut failed_app = WatchApp::new(&contest(1), vec![1]).unwrap();
        failed_app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let failed = failed_app.queue_run(0).unwrap();
        let error = "run error ".repeat(10_000);
        let error_ptr = error.as_ptr();
        assert!(failed_app.run_failed(0, failed.run_id, error));
        assert_eq!(
            failed_app
                .current_problem()
                .unwrap()
                .run
                .error
                .as_ref()
                .unwrap()
                .as_ptr(),
            error_ptr
        );
    }

    #[test]
    fn stderr_before_verdict_is_preserved_and_duplicate_verdict_does_not_overwrite() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));

        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseStderr {
                number: 1,
                stderr: "debug first\n".to_string(),
            },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseAccepted {
                number: 1,
                elapsed: Duration::from_millis(4),
            },
        ));
        assert!(!app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::from_millis(5),
            },
        ));

        let case = &app.current_problem().unwrap().run.cases[0];
        assert_eq!(case.verdict, CaseVerdict::Accepted);
        assert_eq!(
            case.stderr.as_ref().map(|text| text.as_str()),
            Some("debug first\n")
        );
        assert!(case.expected.is_none());
        assert!(case.actual.is_none());
    }

    #[test]
    fn duplicate_test_run_started_cannot_clear_case_results() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseTimedOut {
                number: 1,
                elapsed: Duration::from_secs(2),
            },
        ));

        assert!(!app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert_eq!(
            app.current_problem().unwrap().run.cases[0].verdict,
            CaseVerdict::TimedOut
        );
    }

    #[test]
    fn test_run_started_synchronizes_case_navigation_with_the_worker_count() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();
        app.previous_case();
        app.scroll_detail_down(20);
        assert_eq!(app.selected_case(), 2);

        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        app.previous_case();
        assert_eq!(app.selected_case(), 2);
        app.scroll_detail_down(20);

        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));

        let problem = app.current_problem().unwrap();
        assert_eq!(problem.total_cases, 1);
        assert_eq!(problem.run.total_cases, 1);
        assert_eq!(problem.run.cases.len(), 1);
        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn case_events_before_test_run_started_or_after_finish_are_ignored() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));

        let accepted = TestEvent::TestCaseAccepted {
            number: 1,
            elapsed: Duration::from_millis(1),
        };
        assert!(!app.run_event(0, request.run_id, accepted.clone()));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunFinished {
                accepted: 0,
                total_cases: 1,
            },
        ));
        assert!(!app.run_event(0, request.run_id, accepted));
        assert_eq!(
            app.current_problem().unwrap().run.cases[0].verdict,
            CaseVerdict::Pending
        );
    }
    #[test]
    fn queueing_new_run_clears_previous_case_results() {
        let mut app = WatchApp::new(&contest(1), vec![2]).unwrap();

        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);

        let first = app.queue_run(0).unwrap();

        app.run_started(0, first.run_id);

        app.run_event(
            0,
            first.run_id,
            TestEvent::TestRunStarted { total_cases: 2 },
        );

        app.run_event(
            0,
            first.run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::from_millis(5),
            },
        );

        assert_eq!(
            app.current_problem().unwrap().run.cases[0].verdict,
            CaseVerdict::WrongAnswer
        );

        let second = app.queue_run(0).unwrap();

        let run = &app.current_problem().unwrap().run;

        assert_eq!(run.id, Some(second.run_id));
        assert_eq!(run.cases.len(), 2);

        assert!(
            run.cases
                .iter()
                .all(|case| case.verdict == CaseVerdict::Pending)
        );
    }
    #[test]
    fn detail_scroll_moves_and_saturates_at_zero() {
        let mut app = WatchApp::new(&contest(1), vec![3]).unwrap();

        assert_eq!(app.detail_scroll(), 0);

        assert!(app.scroll_detail_down(3));
        assert_eq!(app.detail_scroll(), 3);

        assert!(app.scroll_detail_down(4));
        assert_eq!(app.detail_scroll(), 7);

        assert!(app.scroll_detail_up(2));
        assert_eq!(app.detail_scroll(), 5);

        assert!(app.scroll_detail_up(100));
        assert_eq!(app.detail_scroll(), 0);

        assert!(!app.scroll_detail_up(1));
        assert_eq!(app.detail_scroll(), 0);

        assert!(app.scroll_detail_down(100_000));
        assert_eq!(app.detail_scroll(), 100_000);

        assert!(app.scroll_detail_down(usize::MAX));
        assert_eq!(app.detail_scroll(), usize::MAX);
        assert!(!app.scroll_detail_down(1));
    }
    #[test]
    fn navigation_and_new_run_reset_detail_scroll() {
        let mut app = WatchApp::new(&contest(2), vec![3, 3]).unwrap();

        app.scroll_detail_down(10);
        assert_eq!(app.detail_scroll(), 10);

        assert!(app.next_case());
        assert_eq!(app.detail_scroll(), 0);

        app.scroll_detail_down(10);

        assert!(app.next_problem());
        assert_eq!(app.detail_scroll(), 0);

        app.scroll_detail_down(10);

        app.source_changed(1, PathBuf::from("B.cpp"), Language::Cpp);

        assert_eq!(app.detail_scroll(), 0);

        app.scroll_detail_down(10);

        app.queue_run(1).unwrap();

        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn detail_revision_tracks_visible_detail_changes_but_not_scroll_or_background_cases() {
        let mut app = WatchApp::new(&contest(2), vec![2, 1]).unwrap();
        let initial = app.detail_revision();

        app.scroll_detail_down(10);
        app.toggle_samples_pane();
        app.toggle_debug();
        assert_eq!(app.detail_revision(), initial);

        app.source_changed(1, PathBuf::from("B.py"), Language::Python);
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let selected_revision = app.detail_revision();
        assert!(selected_revision > initial);

        let background = app.queue_run(1).unwrap();
        assert_eq!(app.detail_revision(), selected_revision);
        assert!(app.run_started(1, background.run_id));
        assert!(app.run_event(
            1,
            background.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert!(app.run_event(
            1,
            background.run_id,
            TestEvent::TestCaseAccepted {
                number: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert_eq!(app.detail_revision(), selected_revision);

        let selected = app.queue_run(0).unwrap();
        let queued_revision = app.detail_revision();
        assert!(queued_revision > selected_revision);
        assert!(app.run_started(0, selected.run_id));
        let started_revision = app.detail_revision();
        assert!(started_revision > queued_revision);
        assert!(app.run_event(
            0,
            selected.run_id,
            TestEvent::TestRunStarted { total_cases: 2 },
        ));
        let test_revision = app.detail_revision();
        assert!(test_revision > started_revision);

        assert!(app.run_event(
            0,
            selected.run_id,
            TestEvent::TestCaseAccepted {
                number: 2,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert_eq!(app.detail_revision(), test_revision);

        assert!(app.run_event(
            0,
            selected.run_id,
            TestEvent::TestCaseAccepted {
                number: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert!(app.detail_revision() > test_revision);

        let case_revision = app.detail_revision();
        assert!(app.next_case());
        assert!(app.detail_revision() > case_revision);

        let problem_revision = app.detail_revision();
        assert!(app.next_problem());
        assert!(app.detail_revision() > problem_revision);

        let failed_revision = app.detail_revision();
        assert!(app.run_failed(1, background.run_id, "failed".to_string()));
        assert!(app.detail_revision() > failed_revision);

        let completed = app.queue_run(1).unwrap();
        let queued_revision = app.detail_revision();
        assert!(app.run_completed(1, completed.run_id));
        assert!(app.detail_revision() > queued_revision);
    }
    #[test]
    fn samples_pane_toggle_is_persistent_ui_state() {
        let mut app = WatchApp::new(&contest(2), vec![3, 3]).unwrap();

        assert!(!app.samples_pane_enabled());
        app.scroll_detail_down(37);

        app.toggle_samples_pane();
        assert!(app.samples_pane_enabled());
        assert_eq!(app.detail_scroll(), 37);

        app.next_problem();
        assert!(app.samples_pane_enabled());

        app.toggle_samples_pane();
        assert!(!app.samples_pane_enabled());
    }

    #[test]
    fn detail_scroll_reconciliation_is_an_explicit_absolute_update() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        app.scroll_detail_down(10);

        assert!(app.reconcile_detail_scroll(42));
        assert_eq!(app.detail_scroll(), 42);
        assert!(!app.reconcile_detail_scroll(42));
    }
    #[test]
    fn selected_problem_returns_current_problem_index() {
        let mut app = WatchApp::new(&contest(2), vec![1, 1]).unwrap();

        assert_eq!(app.selected_problem(), Some(0));

        app.next_problem();

        assert_eq!(app.selected_problem(), Some(1));
    }

    #[test]
    fn compiling_run_can_be_requeued_without_changing_logical_identity() {
        let (mut app, run_id) = queued_cpp_app(3);
        assert!(app.run_started(0, run_id));
        assert_eq!(app.problems[0].run.phase, RunPhase::Compiling);

        assert!(app.run_requeued(0, run_id));

        let run = &app.problems[0].run;
        assert_eq!(run.id, Some(run_id));
        assert_eq!(run.language, Some(Language::Cpp));
        assert_eq!(run.phase, RunPhase::Queued);
        assert!(!run.test_run_started);
        assert_eq!(run.total_cases, 3);
        assert_eq!(run.cases.len(), 3);
        assert!(
            run.cases
                .iter()
                .all(|case| case.verdict == CaseVerdict::Pending)
        );
    }

    #[test]
    fn running_run_can_restart_with_the_same_run_id_after_requeue() {
        let (mut app, run_id) = queued_cpp_app(2);
        assert!(app.run_started(0, run_id));
        assert!(app.run_event(0, run_id, TestEvent::TestRunStarted { total_cases: 2 },));
        assert_eq!(app.problems[0].run.phase, RunPhase::Running);

        assert!(app.run_requeued(0, run_id));
        assert!(app.run_started(0, run_id));
        assert!(app.run_event(0, run_id, TestEvent::TestRunStarted { total_cases: 2 },));

        let run = &app.problems[0].run;
        assert_eq!(run.id, Some(run_id));
        assert_eq!(run.phase, RunPhase::Running);
        assert!(run.test_run_started);
    }

    #[test]
    fn replacement_by_a_requeued_execution_attempt_resets_all_folds() {
        let (mut app, run_id) = queued_cpp_app(1);
        assert!(app.run_started(0, run_id));
        assert!(app.run_event(0, run_id, TestEvent::TestRunStarted { total_cases: 1 },));
        collapse_all_folds(&mut app);

        assert!(app.run_requeued(0, run_id));

        assert_all_folds_expanded(&app);
    }

    #[test]
    fn requeue_clears_all_partial_attempt_state_and_invalidates_selected_detail() {
        let (mut app, run_id) = queued_cpp_app(2);
        assert!(app.run_started(0, run_id));
        assert!(app.run_event(0, run_id, TestEvent::TestRunStarted { total_cases: 2 },));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseAccepted {
                number: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 2,
                elapsed: Duration::from_millis(2),
            },
        ));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseComparison {
                number: 2,
                input: "input".to_owned(),
                expected: "...".to_owned(),
                actual: "...".to_owned(),
            },
        ));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseStderr {
                number: 2,
                stderr: "old stderr\n".to_string(),
            },
        ));
        app.problems[0].run.accepted = 1;
        app.problems[0].run.error = Some(Arc::new("old error".to_string()));
        assert!(app.next_case());
        assert!(app.scroll_detail_down(50_000));
        let revision = app.detail_revision();
        assert!(detail_text(&app).contains("▼ Actual\n"));

        assert!(app.run_requeued(0, run_id));

        let run = &app.problems[0].run;
        assert_eq!(run.phase, RunPhase::Queued);
        assert!(!run.test_run_started);
        assert_eq!(run.accepted, 0);
        assert!(run.error.is_none());
        assert_eq!(run.total_cases, 2);
        assert!(run.cases.iter().all(|case| {
            case.verdict == CaseVerdict::Pending
                && case.elapsed.is_none()
                && case.input.is_none()
                && case.expected.is_none()
                && case.actual.is_none()
                && case.stderr.is_none()
        }));
        assert_eq!(app.selected_case(), 1);
        assert_eq!(app.detail_scroll(), 0);
        assert!(app.detail_revision() > revision);
        let detail = detail_text(&app);
        assert!(detail.contains("Queued..."));
        assert!(!detail.contains("▼ Actual\n"));
        assert!(!detail.contains("old stderr\n"));
    }

    #[test]
    fn stale_requeue_cannot_overwrite_a_newer_logical_run() {
        let (mut app, stale_id) = queued_cpp_app(2);
        assert!(app.run_started(0, stale_id));
        let current = app.queue_run(0).unwrap();
        let revision = app.detail_revision();

        assert!(!app.run_requeued(0, stale_id));

        let run = &app.problems[0].run;
        assert_eq!(run.id, Some(current.run_id));
        assert_eq!(run.phase, RunPhase::Queued);
        assert_eq!(run.language, Some(current.language));
        assert_eq!(app.detail_revision(), revision);
    }

    #[test]
    fn terminal_runs_cannot_be_requeued() {
        for phase in [
            RunPhase::Finished,
            RunPhase::CompileError,
            RunPhase::CompileTimedOut,
            RunPhase::NoSamples,
            RunPhase::Cancelled,
            RunPhase::Failed,
        ] {
            let (mut app, run_id) = queued_cpp_app(1);
            app.problems[0].run.phase = phase;
            app.problems[0].run.accepted = 1;
            let revision = app.detail_revision();

            assert!(!app.run_requeued(0, run_id), "phase {phase:?}");
            assert_eq!(app.problems[0].run.phase, phase);
            assert_eq!(app.problems[0].run.accepted, 1);
            assert_eq!(app.detail_revision(), revision);
        }
    }

    #[test]
    fn requeueing_nonselected_problem_does_not_change_selected_presentation_state() {
        let mut app = WatchApp::new(&contest(2), vec![3, 2]).unwrap();
        assert!(app.source_changed(1, PathBuf::from("B.py"), Language::Python));
        let request = app.queue_run(1).unwrap();
        assert!(app.run_started(1, request.run_id));
        assert!(app.run_event(
            1,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 2 },
        ));
        assert!(app.select_problem(0));
        assert!(app.next_case());
        assert!(app.scroll_detail_down(77));
        let revision = app.detail_revision();

        assert!(app.run_requeued(1, request.run_id));

        assert_eq!(app.problems[1].run.phase, RunPhase::Queued);
        assert_eq!(app.selected_problem(), Some(0));
        assert_eq!(app.selected_case(), 1);
        assert_eq!(app.detail_scroll(), 77);
        assert_eq!(app.detail_revision(), revision);
    }

    #[test]
    fn queue_stress_uses_selected_source_and_switches_detail_mode() {
        let mut app = WatchApp::new(&contest(1), vec![2]).unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp));

        let request = app.queue_stress(0, 1234).unwrap();

        assert_eq!(request.problem, 0);
        assert_eq!(request.language, Language::Cpp);
        assert!(matches!(
            request.kind,
            RunKind::Stress {
                base_seed: 1234,
                count: None,
            }
        ));
        assert_eq!(app.problems[0].detail_mode, DetailMode::Stress);
        assert_eq!(app.problems[0].stress.phase, StressPhase::Queued);
        assert_eq!(app.problems[0].stress.base_seed, Some(1234));
    }

    #[test]
    fn active_stress_identity_uses_logical_phase_and_run_id() {
        for (phase, expected_active) in [
            (StressPhase::Queued, true),
            (StressPhase::Compiling, true),
            (StressPhase::Running, true),
            (StressPhase::Finished, false),
            (StressPhase::Failed, false),
            (StressPhase::Cancelled, false),
            (StressPhase::Error, false),
            (StressPhase::Idle, false),
        ] {
            let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
            assert!(app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp));
            let stress = app.queue_stress(0, 123).unwrap();
            app.problems[0].stress.phase = phase;

            assert_eq!(
                app.active_stress_identity(),
                expected_active.then_some((0, stress.run_id)),
                "phase {phase:?}"
            );
        }
    }

    #[test]
    fn active_stress_identity_is_unique_across_public_transitions_and_fails_closed() {
        let mut app = WatchApp::new(&contest(2), vec![1, 1]).unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        assert!(app.source_changed(1, PathBuf::from("B.py"), Language::Python));

        let first = app.queue_stress(0, 100).unwrap();
        assert_eq!(app.active_stress_identity(), Some((0, first.run_id)));

        let second = app.queue_stress(1, 200).unwrap();
        assert_eq!(app.problems[0].stress.phase, StressPhase::Cancelled);
        assert_eq!(app.problems[0].stress.id, None);
        assert_eq!(app.active_stress_identity(), Some((1, second.run_id)));

        // Even corrupted internal state must never make Stop Stress choose an arbitrary target.
        app.problems[0].stress.id = Some(first.run_id);
        app.problems[0].stress.phase = StressPhase::Running;
        assert_eq!(app.active_stress_identity(), None);
    }

    #[test]
    fn cancelling_active_stress_is_selection_independent_terminal_and_restartable() {
        let mut app = WatchApp::new(&contest(2), vec![1, 1]).unwrap();
        assert!(app.source_changed(1, PathBuf::from("B.py"), Language::Python));
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let first = app.queue_stress(0, 100).unwrap();
        assert!(app.run_started(0, first.run_id));
        assert!(app.stress_event(
            0,
            first.run_id,
            StressEvent::Started {
                base_seed: 100,
                case_limit: None,
            },
        ));
        assert!(app.select_problem(1));

        assert_eq!(app.active_stress_identity(), Some((0, first.run_id)));
        assert!(app.cancel_stress(0, first.run_id));
        assert_eq!(app.selected_problem(), Some(1));
        assert_eq!(app.problems[0].stress.phase, StressPhase::Cancelled);
        assert_eq!(app.problems[1].stress.phase, StressPhase::Idle);
        assert_eq!(app.active_stress_identity(), None);

        assert!(!app.run_started(0, first.run_id));
        assert!(!app.stress_event(
            0,
            first.run_id,
            StressEvent::Started {
                base_seed: 100,
                case_limit: None,
            },
        ));
        assert!(!app.stress_event(
            0,
            first.run_id,
            StressEvent::Progress {
                case_number: 1,
                seed: 100,
                passed: 1,
                elapsed: Duration::from_millis(1),
                cases_per_second: 1_000.0,
            },
        ));
        assert!(!app.stress_event(
            0,
            first.run_id,
            StressEvent::Failed {
                kind: CandidateFailureKind::WrongAnswer,
                case_number: 1,
                base_seed: 100,
                seed: 100,
                input: "late input".to_string(),
                expected: "late expected".to_string(),
                actual: "late actual".to_string(),
                stderr: "late stderr".to_string(),
                candidate_elapsed: Duration::from_millis(1),
                elapsed: Duration::from_millis(1),
                saved_to: PathBuf::from(".atc/stress/A"),
            },
        ));
        assert!(!app.stress_event(
            0,
            first.run_id,
            StressEvent::Finished {
                cases: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert!(!app.stress_event(
            0,
            first.run_id,
            StressEvent::Cancelled {
                cases: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert!(!app.run_completed(0, first.run_id));
        assert!(!app.run_failed(0, first.run_id, "late failure".to_string()));
        assert_eq!(app.problems[0].stress.phase, StressPhase::Cancelled);
        assert_eq!(app.problems[0].stress.passed, 0);
        assert!(app.problems[0].stress.failure.is_none());
        assert!(app.problems[0].stress.error.is_none());

        let sample = app.queue_run(1).unwrap();
        assert!(matches!(sample.kind, RunKind::Samples));
        assert!(sample.run_id > first.run_id);
        assert!(app.run_started(1, sample.run_id));
        assert!(app.run_completed(1, sample.run_id));

        let second = app.queue_stress(0, 200).unwrap();
        assert!(second.run_id > sample.run_id);
        assert_eq!(app.active_stress_identity(), Some((0, second.run_id)));
        assert_eq!(app.selected_problem(), Some(1));
        assert!(app.run_started(0, second.run_id));
        assert_eq!(app.problems[0].stress.phase, StressPhase::Running);
    }

    #[test]
    fn stress_attempt_error_transitions_to_error_detail() {
        for (source, language) in [
            (PathBuf::from("A.cpp"), Language::Cpp),
            (PathBuf::from("A.py"), Language::Python),
        ] {
            let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
            assert!(app.source_changed(0, source, language));
            let stress = app.queue_stress(0, 100).unwrap();
            assert!(app.run_started(0, stress.run_id));

            assert!(app.run_failed(0, stress.run_id, "reference program failed".to_string(),));

            assert_eq!(app.problems[0].stress.phase, StressPhase::Error);
            assert_eq!(
                app.problems[0].stress.error.as_deref().map(String::as_str),
                Some("reference program failed")
            );
            let detail = detail_text(&app);
            assert!(detail.contains("STRESS ERROR"));
            assert!(detail.contains("reference program failed"));
        }
    }

    #[test]
    fn switching_between_samples_and_stress_rejects_late_cross_mode_events() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));

        let sample = app.queue_run(0).unwrap();
        assert!(app.run_started(0, sample.run_id));
        let stress = app.queue_stress(0, 100).unwrap();

        assert_eq!(app.problems[0].run.id, None);
        assert_eq!(app.problems[0].run.phase, RunPhase::Cancelled);

        assert!(!app.run_event(
            0,
            sample.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert_eq!(app.problems[0].detail_mode, DetailMode::Stress);

        let next_sample = app.queue_run(0).unwrap();
        assert_eq!(app.problems[0].stress.id, None);
        assert_eq!(app.problems[0].stress.phase, StressPhase::Cancelled);
        assert!(!app.stress_event(
            0,
            stress.run_id,
            StressEvent::Progress {
                case_number: 1,
                seed: 100,
                passed: 1,
                elapsed: Duration::from_millis(10),
                cases_per_second: 100.0,
            },
        ));
        assert_eq!(app.problems[0].detail_mode, DetailMode::Samples);
        assert_eq!(app.problems[0].run.id, Some(next_sample.run_id));
    }

    #[test]
    fn stress_failure_is_owned_and_shown_without_destroying_sample_state() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let sample = app.queue_run(0).unwrap();
        assert!(app.run_started(0, sample.run_id));
        assert!(app.run_event(
            0,
            sample.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert!(app.run_event(
            0,
            sample.run_id,
            TestEvent::TestCaseAccepted {
                number: 1,
                elapsed: Duration::from_millis(3),
            },
        ));

        let stress = app.queue_stress(0, 100).unwrap();
        assert!(app.run_started(0, stress.run_id));
        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Started {
                base_seed: 100,
                case_limit: None,
            },
        ));
        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Failed {
                kind: CandidateFailureKind::WrongAnswer,
                case_number: 14,
                base_seed: 100,
                seed: 113,
                input: "2\n1 2\n".to_string(),
                expected: "No\n".to_string(),
                actual: "Yes\n".to_string(),
                stderr: String::new(),
                candidate_elapsed: Duration::from_millis(4),
                elapsed: Duration::from_millis(80),
                saved_to: PathBuf::from(".atc/stress/A"),
            },
        ));

        assert_eq!(app.problems[0].run.cases[0].verdict, CaseVerdict::Accepted);
        assert_eq!(
            app.problems[0].run.cases[1].verdict,
            CaseVerdict::WrongAnswer
        );
        assert_eq!(app.problems[0].sample_cases, 1);
        assert_eq!(app.problems[0].total_cases, 2);
        assert_eq!(
            app.problems[0]
                .saved_stress_case
                .as_ref()
                .map(|case| (case.input.as_str(), case.expected.as_str())),
            Some(("2\n1 2\n", "No\n"))
        );
        assert_eq!(app.problems[0].stress.phase, StressPhase::Failed);
        let detail = detail_text(&app);
        assert!(detail.contains("STRESS WA   case 14   seed 113"));
        assert!(detail.contains("▼ Input\n2\n1 2\n"));
        assert!(detail.contains("▼ Expected\nNo\n"));
        assert!(detail.contains("▼ Actual\nYes\n"));
    }

    #[test]
    fn normal_test_layout_promotes_saved_stress_case_as_last_case() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseLayout {
                sample_cases: 1,
                stress_case: Some(Sample {
                    input: "9\n".to_string(),
                    output: "10\n".to_string(),
                }),
            },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 2 },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseAccepted {
                number: 1,
                elapsed: Duration::from_millis(2),
            },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 2,
                elapsed: Duration::from_millis(3),
            },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseComparison {
                number: 2,
                input: "9\n".to_string(),
                expected: "10\n".to_string(),
                actual: "11\n".to_string(),
            },
        ));

        assert_eq!(app.problems[0].sample_cases, 1);
        assert_eq!(app.problems[0].total_cases, 2);
        assert!(app.next_case());
        let detail = detail_text(&app);
        assert!(detail.contains("stress 1 / 1   WA"));
        assert!(detail.contains("▼ Input\n9\n"));
        assert!(detail.contains("▼ Expected\n10\n"));
        assert!(detail.contains("▼ Actual\n11\n"));
    }

    #[test]
    fn case_navigation_switches_from_live_stress_to_the_only_saved_case() {
        let mut app = WatchApp::new_with_stress_cases(
            &contest(1),
            vec![0],
            vec![Some(Sample {
                input: "saved input\n".to_string(),
                output: "saved expected\n".to_string(),
            })],
        )
        .unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let stress = app.queue_stress(0, 100).unwrap();
        assert!(app.run_started(0, stress.run_id));
        assert_eq!(app.problems[0].detail_mode, DetailMode::Stress);

        assert!(app.next_case());

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.problems[0].detail_mode, DetailMode::Samples);
        let detail = detail_text(&app);
        assert!(detail.contains("stress 1 / 1   Pending"));
        assert!(detail.contains("▼ Input\nsaved input\n"));
        assert!(detail.contains("▼ Expected\nsaved expected\n"));
        assert!(!detail.contains("STRESS RUNNING"));
    }

    #[test]
    fn replacing_an_accepted_stress_case_updates_the_preserved_run_summary() {
        let mut app = WatchApp::new_with_stress_cases(
            &contest(1),
            vec![1],
            vec![Some(Sample {
                input: "old input\n".to_string(),
                output: "old expected\n".to_string(),
            })],
        )
        .unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let normal = app.queue_run(0).unwrap();
        assert!(app.run_started(0, normal.run_id));
        assert!(app.run_event(
            0,
            normal.run_id,
            TestEvent::TestRunStarted { total_cases: 2 },
        ));
        for number in 1..=2 {
            assert!(app.run_event(
                0,
                normal.run_id,
                TestEvent::TestCaseAccepted {
                    number,
                    elapsed: Duration::from_millis(1),
                },
            ));
        }
        assert!(app.run_event(
            0,
            normal.run_id,
            TestEvent::TestRunFinished {
                accepted: 2,
                total_cases: 2,
            },
        ));

        let stress = app.queue_stress(0, 200).unwrap();
        assert!(app.run_started(0, stress.run_id));
        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Started {
                base_seed: 200,
                case_limit: None,
            },
        ));
        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Failed {
                kind: CandidateFailureKind::WrongAnswer,
                case_number: 1,
                base_seed: 200,
                seed: 200,
                input: "new input\n".to_string(),
                expected: "new expected\n".to_string(),
                actual: "new actual\n".to_string(),
                stderr: String::new(),
                candidate_elapsed: Duration::from_millis(2),
                elapsed: Duration::from_millis(5),
                saved_to: PathBuf::from(".atc/stress/A"),
            },
        ));

        let problem = &app.problems[0];
        assert_eq!(problem.run.accepted, 1);
        assert_eq!(problem.run.total_cases, 2);
        assert_eq!(problem.run.cases[1].verdict, CaseVerdict::WrongAnswer);
    }

    #[test]
    fn live_stress_result_and_same_domain_rerun_each_reset_all_folds() {
        let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let stress = app.queue_stress(0, 200).unwrap();
        assert!(app.run_started(0, stress.run_id));
        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Started {
                base_seed: 200,
                case_limit: None,
            },
        ));
        collapse_all_folds(&mut app);

        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Failed {
                kind: CandidateFailureKind::WrongAnswer,
                case_number: 1,
                base_seed: 200,
                seed: 201,
                input: "input\n".to_string(),
                expected: "expected\n".to_string(),
                actual: "actual\n".to_string(),
                stderr: "stderr\n".to_string(),
                candidate_elapsed: Duration::from_millis(2),
                elapsed: Duration::from_millis(5),
                saved_to: PathBuf::from(".atc/stress/A"),
            },
        ));
        assert_all_folds_expanded(&app);

        collapse_all_folds(&mut app);
        assert!(app.queue_stress(0, 300).is_some());
        assert_all_folds_expanded(&app);
    }

    #[test]
    fn replacing_selected_stress_case_invalidates_equal_shape_detail() {
        let mut app = WatchApp::new_with_stress_cases(
            &contest(1),
            vec![0],
            vec![Some(Sample {
                input: "old input\n".to_string(),
                output: "old expected\n".to_string(),
            })],
        )
        .unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));

        let normal = app.queue_run(0).unwrap();
        assert!(app.run_started(0, normal.run_id));
        assert!(app.run_event(
            0,
            normal.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert!(app.run_event(
            0,
            normal.run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::from_millis(2),
            },
        ));
        assert!(app.run_event(
            0,
            normal.run_id,
            TestEvent::TestCaseComparison {
                number: 1,
                input: "old input\n".to_string(),
                expected: "old expected\n".to_string(),
                actual: "old actual\n".to_string(),
            },
        ));
        assert!(app.run_event(
            0,
            normal.run_id,
            TestEvent::TestRunFinished {
                accepted: 0,
                total_cases: 1,
            },
        ));

        let stress = app.queue_stress(0, 200).unwrap();
        assert!(app.run_started(0, stress.run_id));
        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Started {
                base_seed: 200,
                case_limit: None,
            },
        ));
        assert!(app.next_case());
        assert_eq!(app.problems[0].detail_mode, DetailMode::Samples);

        let before_lengths = DetailDocument::from_app(&app)
            .segments()
            .map(|segment| segment.text().len())
            .collect::<Vec<_>>();
        let revision = app.detail_revision();

        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Failed {
                kind: CandidateFailureKind::WrongAnswer,
                case_number: 1,
                base_seed: 200,
                seed: 200,
                input: "new input\n".to_string(),
                expected: "new expected\n".to_string(),
                actual: "new actual\n".to_string(),
                stderr: String::new(),
                candidate_elapsed: Duration::from_millis(2),
                elapsed: Duration::from_millis(5),
                saved_to: PathBuf::from(".atc/stress/A"),
            },
        ));

        let after_lengths = DetailDocument::from_app(&app)
            .segments()
            .map(|segment| segment.text().len())
            .collect::<Vec<_>>();
        assert_eq!(after_lengths, before_lengths);
        assert_ne!(app.detail_revision(), revision);
        let detail = detail_text(&app);
        assert!(detail.contains("▼ Input\nnew input\n"));
        assert!(detail.contains("▼ Expected\nnew expected\n"));
        assert!(detail.contains("▼ Actual\nnew actual\n"));
        assert!(!detail.contains("old input"));
    }

    #[test]
    fn promoted_only_case_replaces_obsolete_no_samples_detail() {
        let mut app = WatchApp::new(&contest(1), vec![0]).unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));

        let normal = app.queue_run(0).unwrap();
        assert!(app.run_started(0, normal.run_id));
        assert!(app.run_event(0, normal.run_id, TestEvent::NoSamples));
        assert_eq!(app.problems[0].run.phase, RunPhase::NoSamples);

        let stress = app.queue_stress(0, 300).unwrap();
        assert!(app.run_started(0, stress.run_id));
        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Started {
                base_seed: 300,
                case_limit: None,
            },
        ));
        assert!(app.stress_event(
            0,
            stress.run_id,
            StressEvent::Failed {
                kind: CandidateFailureKind::RuntimeError,
                case_number: 1,
                base_seed: 300,
                seed: 300,
                input: "promoted input\n".to_string(),
                expected: "trusted expected\n".to_string(),
                actual: String::new(),
                stderr: "runtime error\n".to_string(),
                candidate_elapsed: Duration::from_millis(2),
                elapsed: Duration::from_millis(5),
                saved_to: PathBuf::from(".atc/stress/A"),
            },
        ));

        assert!(app.next_case());
        let detail = detail_text(&app);
        assert!(detail.contains("stress 1 / 1   RE"));
        assert!(detail.contains("▼ Input\npromoted input\n"));
        assert!(detail.contains("▼ Expected\ntrusted expected\n"));
        assert!(!detail.contains("No samples"));
    }

    #[test]
    fn test_layout_and_case_events_preserve_user_input_selection_and_detail_state() {
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![2],
            vec![Some(Sample {
                input: "old stress\n".to_string(),
                output: "old expected\n".to_string(),
            })],
            vec![loaded_user_inputs(&[(3, "user\r\ninput\r\n")])],
        )
        .unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.next_case());
        assert!(app.next_case());
        assert!(app.next_case());
        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(3))),
        );
        app.toggle_detail_section(DetailSectionKind::Input);
        assert!(app.scroll_detail_down(27));
        let revision = app.detail_revision();

        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseLayout {
                sample_cases: 1,
                stress_case: None,
            },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestCaseAccepted {
                number: 1,
                elapsed: Duration::from_millis(2),
            },
        ));

        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(3))),
        );
        assert_eq!(app.current_problem().unwrap().sample_cases, 1);
        assert!(app.current_problem().unwrap().saved_stress_case.is_none());
        assert_eq!(app.current_problem().unwrap().total_cases, 1);
        assert_eq!(app.current_problem().unwrap().run.cases.len(), 1);
        assert_eq!(app.detail_scroll(), 27);
        assert!(
            app.detail_fold_state()
                .is_collapsed(DetailSectionKind::Input)
        );
        assert_eq!(app.detail_revision(), revision);
    }

    #[test]
    fn run_lifecycle_preserves_user_input_selection_fold_scroll_and_revision() {
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![1],
            vec![None],
            vec![loaded_user_inputs(&[(3, "user input\r\n")])],
        )
        .unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        assert!(app.next_case());

        let selection = Some(CaseSelection::UserInput(UserInputSelection::Persisted(3)));
        assert_case_selection(&app, selection);
        app.toggle_detail_section(DetailSectionKind::Input);
        assert!(app.scroll_detail_down(27));
        let folds = app.detail_fold_state();
        let scroll = app.detail_scroll();
        let revision = app.detail_revision();

        let assert_preserved = |app: &WatchApp, operation: &str| {
            assert_eq!(app.case_selection(), selection, "{operation}: selection");
            assert_eq!(app.detail_fold_state(), folds, "{operation}: folds");
            assert_eq!(app.detail_scroll(), scroll, "{operation}: scroll");
            assert_eq!(app.detail_revision(), revision, "{operation}: revision");
            assert_selection_invariant(app);
        };

        let first = app.queue_run(0).unwrap();
        assert_preserved(&app, "queue_run");

        assert!(app.run_started(0, first.run_id));
        assert_preserved(&app, "run_started");

        assert!(app.run_requeued(0, first.run_id));
        assert_preserved(&app, "run_requeued");

        assert!(app.run_started(0, first.run_id));
        assert_preserved(&app, "run_started after requeue");

        assert!(app.run_completed(0, first.run_id));
        assert_preserved(&app, "run_completed");

        let second = app.queue_run(0).unwrap();
        assert_preserved(&app, "queue_run before failure");
        assert!(app.run_started(0, second.run_id));
        assert_preserved(&app, "run_started before failure");
        assert!(app.run_failed(0, second.run_id, "runner failed".to_string()));
        assert_preserved(&app, "run_failed");
    }

    #[test]
    fn saved_stress_failure_does_not_replace_or_invalidate_user_input_detail() {
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![0],
            vec![None],
            vec![loaded_user_inputs(&[(8, "kept user input\r\n")])],
        )
        .unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let request = app.queue_stress(0, 99).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.stress_event(
            0,
            request.run_id,
            StressEvent::Started {
                base_seed: 99,
                case_limit: None,
            },
        ));
        assert!(app.next_case(), "case navigation exits live Stress mode");
        assert_eq!(
            app.current_problem().unwrap().detail_mode,
            DetailMode::Samples
        );
        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(8))),
        );
        app.toggle_detail_section(DetailSectionKind::Input);
        assert!(app.scroll_detail_down(19));
        let revision = app.detail_revision();

        assert!(app.stress_event(
            0,
            request.run_id,
            StressEvent::Failed {
                kind: CandidateFailureKind::WrongAnswer,
                case_number: 4,
                base_seed: 99,
                seed: 102,
                input: "saved stress input\n".to_string(),
                expected: "expected\n".to_string(),
                actual: "actual\n".to_string(),
                stderr: String::new(),
                candidate_elapsed: Duration::from_millis(3),
                elapsed: Duration::from_millis(12),
                saved_to: PathBuf::from(".atc/stress/A"),
            },
        ));

        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Persisted(8))),
        );
        assert_eq!(app.current_problem().unwrap().total_cases, 1);
        assert_eq!(app.current_problem().unwrap().run.cases.len(), 1);
        assert_eq!(app.detail_scroll(), 19);
        assert!(
            app.detail_fold_state()
                .is_collapsed(DetailSectionKind::Input)
        );
        assert_eq!(app.detail_revision(), revision);
        assert!(detail_text(&app).contains("▶ Input"));
        assert!(!detail_text(&app).contains("saved stress input"));
    }

    #[test]
    fn persisted_and_draft_user_input_details_preserve_exact_content_and_fold_identity() {
        let persisted = "alpha\r\n\r\nomega\r\n";
        let mut ready = UserInputReadyState::new(vec![PersistedUserInputState {
            id: 3,
            content: persisted.to_string(),
        }]);
        ready.begin_draft().unwrap();
        let draft = "draft\r\n\r\nbody\n";
        ready.edit_mut().unwrap().replace_buffer(draft.to_string());
        let mut app = WatchApp::new_with_session_data(
            &contest(1),
            vec![0],
            vec![None],
            vec![UserInputState::Ready(ready)],
        )
        .unwrap();

        let persisted_document = DetailDocument::from_app(&app);
        assert_eq!(
            persisted_document.section_body(DetailSectionKind::Input),
            Some(persisted)
        );
        assert!(detail_text(&app).contains("▼ Input\nalpha\r\n\r\nomega\r\n"));

        app.toggle_detail_section(DetailSectionKind::Input);
        assert!(detail_text(&app).contains("▶ Input"));
        assert_eq!(
            DetailDocument::from_app(&app).section_body(DetailSectionKind::Input),
            Some(persisted)
        );

        assert!(app.next_case());
        assert_case_selection(
            &app,
            Some(CaseSelection::UserInput(UserInputSelection::Draft)),
        );
        assert!(
            !app.detail_fold_state()
                .is_collapsed(DetailSectionKind::Input),
            "changing semantic detail identity resets folds"
        );
        assert_eq!(
            DetailDocument::from_app(&app).section_body(DetailSectionKind::Input),
            Some(draft)
        );
    }

    #[test]
    fn selected_case_is_clamped_to_current_problem_sample_count_on_requeue() {
        let (mut app, run_id) = queued_cpp_app(3);
        assert!(app.run_started(0, run_id));
        assert!(app.run_event(0, run_id, TestEvent::TestRunStarted { total_cases: 3 },));
        app.case_selection = Some(CaseSelection::Test(2));
        app.problems[0].total_cases = 1;

        assert!(app.run_requeued(0, run_id));

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.problems[0].run.total_cases, 1);
        assert_eq!(app.problems[0].run.cases.len(), 1);
        assert_selection_invariant(&app);
    }
}
