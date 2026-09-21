use crate::language::{Language, PythonRuntime};
use crate::settings::{
    EditorModeValue, SettingInput, SettingKey, SettingValue, SettingsCategory, SettingsDocument,
};
use crate::settings_store::{PostCommitState, SaveOutcome, SettingsSaveError, SettingsStore};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminalEvent};

const LIFECYCLE_NOTICE: &str =
    "Saved changes apply when you reopen a Contest. Refresh Contest does not reload settings.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTransition {
    None,
    Back,
    OpenEditor,
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsScreen {
    store: SettingsStore,
    selected: usize,
    scroll: usize,
    editor: Option<EditModal>,
    notice: Option<String>,
    confirmation: Option<ConfirmationModal>,
    recovery: Option<RecoveryModal>,
}

#[derive(Debug, Clone)]
pub(crate) enum SettingsPage {
    Ready(Box<SettingsScreen>),
    Invalid {
        path: PathBuf,
        message: String,
        selected: usize,
    },
}

impl SettingsPage {
    pub(crate) fn load(path: impl Into<PathBuf>) -> Self {
        Self::load_with_selection(path.into(), 0)
    }

    fn load_with_selection(path: PathBuf, selected: usize) -> Self {
        match SettingsStore::load(path.clone()) {
            Ok(store) => {
                let mut screen = SettingsScreen::new(store);
                screen.selected = selected.min(SettingKey::ALL.len() - 1);
                Self::Ready(Box::new(screen))
            }
            Err(error) => Self::Invalid {
                path,
                message: error.to_string(),
                selected,
            },
        }
    }

    pub(crate) fn handle_event(&mut self, event: TerminalEvent) -> SettingsTransition {
        match self {
            Self::Ready(screen) => screen.handle_event(event),
            Self::Invalid {
                path,
                message,
                selected,
            } => {
                let TerminalEvent::Key(key) = event else {
                    return SettingsTransition::None;
                };
                if key.kind != KeyEventKind::Press
                    || key.modifiers.control
                    || key.modifiers.alt
                    || key.modifiers.super_key
                {
                    return SettingsTransition::None;
                }
                match key.code {
                    KeyCode::Escape => SettingsTransition::Back,
                    KeyCode::Char('e') => SettingsTransition::OpenEditor,
                    KeyCode::Char('r') => {
                        let reloaded = Self::load_with_selection(path.clone(), *selected);
                        if let Self::Invalid {
                            message: new_message,
                            ..
                        } = &reloaded
                        {
                            *message = format!("Reload failed: {new_message}");
                        } else {
                            *self = reloaded;
                        }
                        SettingsTransition::None
                    }
                    _ => SettingsTransition::None,
                }
            }
        }
    }

    pub(crate) fn reload_after_editor(&mut self) {
        let (path, selected) = match self {
            Self::Ready(screen) => (screen.store.path().to_path_buf(), screen.selected),
            Self::Invalid { path, selected, .. } => (path.clone(), *selected),
        };
        *self = Self::load_with_selection(path, selected);
        if let Self::Ready(screen) = self {
            screen.notice = Some("Reloaded Global Config after the editor closed.".to_string());
        }
    }
}

impl SettingsScreen {
    pub(crate) fn new(store: SettingsStore) -> Self {
        Self {
            store,
            selected: 0,
            scroll: 0,
            editor: None,
            notice: None,
            confirmation: None,
            recovery: None,
        }
    }

    #[cfg(test)]
    fn store(&self) -> &SettingsStore {
        &self.store
    }

    #[cfg(test)]
    fn store_mut(&mut self) -> &mut SettingsStore {
        &mut self.store
    }

    pub(crate) fn selected_key(&self) -> SettingKey {
        SettingKey::ALL[self.selected]
    }

    pub(crate) fn handle_event(&mut self, event: TerminalEvent) -> SettingsTransition {
        if self.recovery.is_some() {
            return self.handle_recovery_event(event);
        }
        if self.confirmation.is_some() {
            return self.handle_confirmation_event(event);
        }
        if let Some(mut editor) = self.editor.take() {
            match editor.handle_event(event, self.store.document()) {
                EditTransition::Keep => self.editor = Some(editor),
                EditTransition::Cancel => {}
                EditTransition::Save(value) => {
                    if let Err(error) = self.save_value(editor.key(), value) {
                        if !error.candidate_present() {
                            editor.set_error(error.to_string());
                            self.editor = Some(editor);
                        }
                        if let SaveAttemptError::Store(error) = error {
                            self.recovery =
                                Some(RecoveryModal::from_save_error(error, self.editor.is_some()));
                        }
                    }
                }
            }
            return SettingsTransition::None;
        }
        match event {
            TerminalEvent::Key(key) => self.handle_key(key),
            TerminalEvent::Paste(_)
            | TerminalEvent::Pointer(_)
            | TerminalEvent::Resize(_)
            | TerminalEvent::Ignored => SettingsTransition::None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> SettingsTransition {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || key.modifiers.control
            || key.modifiers.alt
            || key.modifiers.super_key
        {
            return SettingsTransition::None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                SettingsTransition::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(SettingKey::ALL.len() - 1);
                SettingsTransition::None
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                self.begin_edit();
                SettingsTransition::None
            }
            KeyCode::Char('r') if key.kind == KeyEventKind::Press => {
                self.reset_selected();
                SettingsTransition::None
            }
            KeyCode::Char('e') if key.kind == KeyEventKind::Press => SettingsTransition::OpenEditor,
            KeyCode::Escape if key.kind == KeyEventKind::Press => SettingsTransition::Back,
            _ => SettingsTransition::None,
        }
    }

    fn begin_edit(&mut self) {
        let key = self.selected_key();
        if let Some(reason) = self.store.read_only_reason() {
            self.notice = Some(format!("Read-only: {reason}"));
            return;
        }
        if !self.store.document().editable(key) {
            self.notice = Some(format!(
                "{} cannot be edited until editor.command is configured.",
                key.path()
            ));
            return;
        }
        self.notice = None;
        self.editor = Some(EditModal::for_key(key, self.store.document()));
    }

    fn reset_selected(&mut self) {
        let key = self.selected_key();
        if let Some(reason) = self.store.read_only_reason() {
            self.notice = Some(format!("Read-only: {reason}"));
            return;
        }
        if key == SettingKey::EditorCommand && self.store.document().is_modified(key) {
            self.confirmation = Some(ConfirmationModal::ResetEditor);
            self.notice = None;
            return;
        }
        self.apply_reset(key);
    }

    fn apply_reset(&mut self, key: SettingKey) {
        let mut candidate = self.store.clone();
        match candidate.document_mut().reset(key) {
            Ok(_) => match self.commit_candidate(candidate) {
                Ok(SaveOutcome::Saved) => self.notice = Some(format!("Reset {}.", key.path())),
                Ok(SaveOutcome::Unchanged) => {
                    self.notice = Some(format!("{} already uses the default.", key.path()))
                }
                Err(error) => {
                    self.notice = Some(error.to_string());
                    self.recovery = Some(RecoveryModal::from_save_error(error, false));
                }
            },
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn save_value(&mut self, key: SettingKey, value: SettingValue) -> Result<(), SaveAttemptError> {
        let mut candidate = self.store.clone();
        candidate
            .document_mut()
            .set(key, value)
            .map_err(|error| SaveAttemptError::Document(error.to_string()))?;
        match self.commit_candidate(candidate) {
            Ok(SaveOutcome::Saved) => {
                self.notice = Some(format!("Saved {}.", key.path()));
                Ok(())
            }
            Ok(SaveOutcome::Unchanged) => {
                self.notice = Some(format!("{} is unchanged.", key.path()));
                Ok(())
            }
            Err(error) => Err(SaveAttemptError::Store(error)),
        }
    }

    // The live store always describes the last disk-confirmed document. A failed
    // pre-commit attempt must not leave a hidden edit that a later save can commit.
    fn commit_candidate(
        &mut self,
        mut candidate: SettingsStore,
    ) -> Result<SaveOutcome, SettingsSaveError> {
        let result = candidate.save();
        self.reconcile_save_result(candidate, result)
    }

    fn reconcile_save_result(
        &mut self,
        candidate: SettingsStore,
        result: Result<SaveOutcome, SettingsSaveError>,
    ) -> Result<SaveOutcome, SettingsSaveError> {
        match result {
            Ok(outcome) => {
                self.store = candidate;
                Ok(outcome)
            }
            Err(error) => {
                if matches!(
                    error,
                    SettingsSaveError::PostCommit {
                        state: PostCommitState::CandidatePresent | PostCommitState::BaselinePresent,
                        ..
                    }
                ) {
                    // SettingsStore re-read and reconciled the confirmed disk bytes.
                    self.store = candidate;
                }
                Err(error)
            }
        }
    }

    fn handle_confirmation_event(&mut self, event: TerminalEvent) -> SettingsTransition {
        let TerminalEvent::Key(key) = event else {
            return SettingsTransition::None;
        };
        if key.kind != KeyEventKind::Press {
            return SettingsTransition::None;
        }
        match key.code {
            KeyCode::Escape => self.confirmation = None,
            KeyCode::Enter => {
                let confirmation = self
                    .confirmation
                    .take()
                    .expect("confirmation must remain active");
                match confirmation {
                    ConfirmationModal::ResetEditor => self.apply_reset(SettingKey::EditorCommand),
                }
            }
            _ => {}
        }
        SettingsTransition::None
    }

    fn handle_recovery_event(&mut self, event: TerminalEvent) -> SettingsTransition {
        let TerminalEvent::Key(key) = event else {
            return SettingsTransition::None;
        };
        if key.kind != KeyEventKind::Press {
            return SettingsTransition::None;
        }
        match key.code {
            KeyCode::Escape => {
                if self
                    .recovery
                    .as_ref()
                    .is_some_and(|recovery| recovery.must_reload)
                {
                    return SettingsTransition::Back;
                }
                self.recovery = None;
            }
            KeyCode::Char('r') => match self.store.reload() {
                Ok(()) => {
                    self.recovery = None;
                    self.notice = Some(if self.editor.is_some() {
                        "Reloaded Global Config. Review the retained draft before saving or cancel it."
                            .to_string()
                    } else {
                        "Reloaded Global Config. No previous change was reapplied.".to_string()
                    });
                }
                Err(error) => {
                    if let Some(recovery) = self.recovery.as_mut() {
                        recovery.message = format!("Reload failed: {error}");
                        recovery.must_reload = true;
                    }
                }
            },
            _ => {}
        }
        SettingsTransition::None
    }
}

#[derive(Debug)]
enum SaveAttemptError {
    Document(String),
    Store(SettingsSaveError),
}

impl SaveAttemptError {
    fn candidate_present(&self) -> bool {
        matches!(
            self,
            Self::Store(SettingsSaveError::PostCommit {
                state: PostCommitState::CandidatePresent,
                ..
            })
        )
    }
}

impl std::fmt::Display for SaveAttemptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Document(message) => formatter.write_str(message),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ConfirmationModal {
    ResetEditor,
}

#[derive(Debug, Clone)]
struct RecoveryModal {
    title: &'static str,
    message: String,
    draft_retained: bool,
    must_reload: bool,
}

impl RecoveryModal {
    fn from_save_error(error: SettingsSaveError, draft_retained: bool) -> Self {
        let title = match &error {
            SettingsSaveError::Conflict(_) => "Conflict",
            SettingsSaveError::PostCommit {
                state: PostCommitState::CandidatePresent,
                ..
            } => "Saved; Finalization Failed",
            SettingsSaveError::PostCommit {
                state: PostCommitState::BaselinePresent,
                ..
            } => "Previous Settings Confirmed",
            SettingsSaveError::PostCommit {
                state: PostCommitState::Unknown,
                ..
            } => "Save State Uncertain",
            SettingsSaveError::BeforeCommit(_) => "Save Failed",
            SettingsSaveError::ReadOnly(_) => "Read-only",
        };
        Self {
            title,
            message: error.to_string(),
            draft_retained,
            must_reload: matches!(
                error,
                SettingsSaveError::PostCommit {
                    state: PostCommitState::Unknown,
                    ..
                }
            ),
        }
    }
}

#[derive(Debug, Clone)]
enum EditModal {
    Enum(EnumEditor),
    Text(TextEditor),
    List(ListEditor),
}

impl EditModal {
    fn for_key(key: SettingKey, document: &SettingsDocument) -> Self {
        match key.input() {
            SettingInput::Enum => Self::Enum(EnumEditor::new(key, document)),
            SettingInput::String | SettingInput::Number => {
                Self::Text(TextEditor::new(key, document))
            }
            SettingInput::StringList => Self::List(ListEditor::new(key, document)),
        }
    }

    fn key(&self) -> SettingKey {
        match self {
            Self::Enum(editor) => editor.key,
            Self::Text(editor) => editor.key,
            Self::List(editor) => editor.key,
        }
    }

    fn set_error(&mut self, error: String) {
        match self {
            Self::Enum(editor) => editor.error = Some(error),
            Self::Text(editor) => editor.error = Some(error),
            Self::List(editor) => editor.error = Some(error),
        }
    }

    fn handle_event(
        &mut self,
        event: TerminalEvent,
        document: &SettingsDocument,
    ) -> EditTransition {
        match self {
            Self::Enum(editor) => editor.handle_event(event),
            Self::Text(editor) => editor.handle_event(event, document),
            Self::List(editor) => editor.handle_event(event, document),
        }
    }
}

enum EditTransition {
    Keep,
    Cancel,
    Save(SettingValue),
}

#[derive(Debug, Clone)]
struct EnumChoice {
    label: &'static str,
    value: SettingValue,
}

#[derive(Debug, Clone)]
struct EnumEditor {
    key: SettingKey,
    choices: Vec<EnumChoice>,
    selected: usize,
    error: Option<String>,
}

impl EnumEditor {
    fn new(key: SettingKey, document: &SettingsDocument) -> Self {
        let choices = match key {
            SettingKey::DefaultLanguage => vec![
                EnumChoice {
                    label: "C++",
                    value: SettingValue::Language(Language::Cpp),
                },
                EnumChoice {
                    label: "Python",
                    value: SettingValue::Language(Language::Python),
                },
            ],
            SettingKey::SubmitPythonRuntime => vec![
                EnumChoice {
                    label: "CPython",
                    value: SettingValue::PythonRuntime(PythonRuntime::CPython),
                },
                EnumChoice {
                    label: "PyPy",
                    value: SettingValue::PythonRuntime(PythonRuntime::PyPy),
                },
            ],
            SettingKey::EditorMode => vec![
                EnumChoice {
                    label: "Auto",
                    value: SettingValue::EditorMode(EditorModeValue::Auto),
                },
                EnumChoice {
                    label: "Terminal",
                    value: SettingValue::EditorMode(EditorModeValue::Terminal),
                },
                EnumChoice {
                    label: "External",
                    value: SettingValue::EditorMode(EditorModeValue::External),
                },
            ],
            _ => unreachable!("only enum settings create an enum editor"),
        };
        let current = document.effective_value(key);
        let selected = choices
            .iter()
            .position(|choice| choice.value == current)
            .unwrap_or(0);
        Self {
            key,
            choices,
            selected,
            error: None,
        }
    }

    fn handle_event(&mut self, event: TerminalEvent) -> EditTransition {
        let TerminalEvent::Key(key) = event else {
            return EditTransition::Keep;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || key.modifiers.control
            || key.modifiers.alt
            || key.modifiers.super_key
        {
            return EditTransition::Keep;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.error = None;
                EditTransition::Keep
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.choices.len() - 1);
                self.error = None;
                EditTransition::Keep
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                EditTransition::Save(self.choices[self.selected].value.clone())
            }
            KeyCode::Escape if key.kind == KeyEventKind::Press => EditTransition::Cancel,
            _ => EditTransition::Keep,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct SingleLineInput {
    value: String,
    cursor: usize,
}

impl SingleLineInput {
    fn new(value: String) -> Self {
        let cursor = value.len();
        Self { value, cursor }
    }

    fn insert(&mut self, text: &str) -> Result<(), &'static str> {
        if text.contains(['\r', '\n']) {
            return Err("Multiline input is not allowed.");
        }
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
        Ok(())
    }

    fn move_left(&mut self) {
        if let Some((index, _)) = self.value[..self.cursor].grapheme_indices(true).next_back() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.value.len() {
            let length = self.value[self.cursor..]
                .graphemes(true)
                .next()
                .map_or(0, str::len);
            self.cursor += length;
        }
    }

    fn backspace(&mut self) {
        let end = self.cursor;
        self.move_left();
        if self.cursor < end {
            self.value.drain(self.cursor..end);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.value.len() {
            let end = self.cursor
                + self.value[self.cursor..]
                    .graphemes(true)
                    .next()
                    .map_or(0, str::len);
            self.value.drain(self.cursor..end);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || key.modifiers.control
            || key.modifiers.alt
            || key.modifiers.super_key
        {
            return false;
        }
        match key.code {
            KeyCode::Char(character) => {
                let mut buffer = [0; 4];
                let _ = self.insert(character.encode_utf8(&mut buffer));
            }
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            _ => return false,
        }
        true
    }

    fn rendered_with_cursor(&self) -> String {
        format!(
            "{}│{}",
            &self.value[..self.cursor],
            &self.value[self.cursor..]
        )
    }
}

#[derive(Debug, Clone)]
struct TextEditor {
    key: SettingKey,
    input: SingleLineInput,
    error: Option<String>,
}

impl TextEditor {
    fn new(key: SettingKey, document: &SettingsDocument) -> Self {
        let initial = if key == SettingKey::EditorCommand && !document.editor_configured() {
            String::new()
        } else {
            document.effective_value(key).display()
        };
        Self {
            key,
            input: SingleLineInput::new(initial),
            error: None,
        }
    }

    fn handle_event(
        &mut self,
        event: TerminalEvent,
        document: &SettingsDocument,
    ) -> EditTransition {
        match event {
            TerminalEvent::Paste(text) => {
                self.error = self.input.insert(&text).err().map(str::to_string);
                EditTransition::Keep
            }
            TerminalEvent::Key(key)
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Escape =>
            {
                EditTransition::Cancel
            }
            TerminalEvent::Key(key)
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter =>
            {
                let value = match self.key.input() {
                    SettingInput::String => SettingValue::String(self.input.value.clone()),
                    SettingInput::Number => match self.input.value.parse::<f64>() {
                        Ok(value) => SettingValue::Number(value),
                        Err(_) => {
                            self.error = Some("Enter a valid number of seconds.".to_string());
                            return EditTransition::Keep;
                        }
                    },
                    _ => unreachable!("text editor handles string and number settings"),
                };
                match document.validate_set(self.key, value.clone()) {
                    Ok(()) => EditTransition::Save(value),
                    Err(error) => {
                        self.error = Some(error.to_string());
                        EditTransition::Keep
                    }
                }
            }
            TerminalEvent::Key(key) => {
                if self.input.handle_key(key) {
                    self.error = None;
                }
                EditTransition::Keep
            }
            TerminalEvent::Pointer(_) | TerminalEvent::Resize(_) | TerminalEvent::Ignored => {
                EditTransition::Keep
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ElementEditor {
    index: Option<usize>,
    input: SingleLineInput,
}

#[derive(Debug, Clone)]
struct ListEditor {
    key: SettingKey,
    items: Vec<String>,
    selected: usize,
    element: Option<ElementEditor>,
    error: Option<String>,
}

impl ListEditor {
    fn new(key: SettingKey, document: &SettingsDocument) -> Self {
        let SettingValue::StringList(items) = document.effective_value(key) else {
            unreachable!("list editor only handles string-list settings");
        };
        Self {
            key,
            items,
            selected: 0,
            element: None,
            error: None,
        }
    }

    fn handle_event(
        &mut self,
        event: TerminalEvent,
        document: &SettingsDocument,
    ) -> EditTransition {
        if self.element.is_some() {
            return self.handle_element_event(event, document);
        }
        let TerminalEvent::Key(key) = event else {
            return EditTransition::Keep;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || key.modifiers.control
            || key.modifiers.alt
            || key.modifiers.super_key
        {
            return EditTransition::Keep;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press && !self.items.is_empty() => {
                self.element = Some(ElementEditor {
                    index: Some(self.selected),
                    input: SingleLineInput::new(self.items[self.selected].clone()),
                });
            }
            KeyCode::Char('a') if key.kind == KeyEventKind::Press => {
                self.element = Some(ElementEditor {
                    index: None,
                    input: SingleLineInput::default(),
                });
            }
            KeyCode::Char('d') if key.kind == KeyEventKind::Press && !self.items.is_empty() => {
                self.items.remove(self.selected);
                self.selected = self.selected.min(self.items.len().saturating_sub(1));
                self.error = None;
            }
            KeyCode::Char('J') if key.kind == KeyEventKind::Press => {
                if self.selected + 1 < self.items.len() {
                    self.items.swap(self.selected, self.selected + 1);
                    self.selected += 1;
                }
            }
            KeyCode::Char('K') if key.kind == KeyEventKind::Press => {
                if self.selected > 0 {
                    self.items.swap(self.selected, self.selected - 1);
                    self.selected -= 1;
                }
            }
            KeyCode::Char('s') if key.kind == KeyEventKind::Press => {
                let value = SettingValue::StringList(self.items.clone());
                return match document.validate_set(self.key, value.clone()) {
                    Ok(()) => EditTransition::Save(value),
                    Err(error) => {
                        self.error = Some(error.to_string());
                        EditTransition::Keep
                    }
                };
            }
            KeyCode::Escape if key.kind == KeyEventKind::Press => return EditTransition::Cancel,
            _ => {}
        }
        EditTransition::Keep
    }

    fn handle_element_event(
        &mut self,
        event: TerminalEvent,
        document: &SettingsDocument,
    ) -> EditTransition {
        let element = self
            .element
            .as_mut()
            .expect("element editor must be active");
        match event {
            TerminalEvent::Paste(text) => {
                self.error = element.input.insert(&text).err().map(str::to_string);
            }
            TerminalEvent::Key(key)
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Escape =>
            {
                self.element = None;
                self.error = None;
            }
            TerminalEvent::Key(key)
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter =>
            {
                let mut proposed = self.items.clone();
                match element.index {
                    Some(index) => proposed[index] = element.input.value.clone(),
                    None => proposed.push(element.input.value.clone()),
                }
                match document.validate_set(self.key, SettingValue::StringList(proposed.clone())) {
                    Ok(()) => {
                        self.items = proposed;
                        self.selected = element.index.unwrap_or(self.items.len() - 1);
                        self.element = None;
                        self.error = None;
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            TerminalEvent::Key(key) => {
                if element.input.handle_key(key) {
                    self.error = None;
                }
            }
            TerminalEvent::Pointer(_) | TerminalEvent::Resize(_) | TerminalEvent::Ignored => {}
        }
        EditTransition::Keep
    }
}

pub(crate) fn render(frame: &mut Frame<'_>, screen: &mut SettingsScreen) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let outer = Block::default()
        .title(" Global Settings ")
        .borders(Borders::ALL);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    if let Some(recovery) = screen.recovery.as_ref()
        && recovery.must_reload
    {
        frame.render_widget(
            Paragraph::new("The current Global Config is unconfirmed. No previous settings are shown as current."),
            inner,
        );
        render_recovery(frame, recovery);
        return;
    }

    let show_detail = inner.width >= 64 && inner.height >= 17;
    let detail_height = if show_detail { 7 } else { 0 };
    let footer_height = inner.height.min(2);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(detail_height),
            Constraint::Length(footer_height),
        ])
        .split(inner);

    render_list(frame, sections[0], screen);
    if show_detail {
        render_detail(frame, sections[1], screen);
    }
    render_footer(frame, sections[2], screen.notice.as_deref());
    if let Some(editor) = screen.editor.as_ref() {
        render_edit_modal(frame, editor);
    }
    if let Some(confirmation) = screen.confirmation.as_ref() {
        render_confirmation(frame, confirmation);
    }
    if let Some(recovery) = screen.recovery.as_ref() {
        render_recovery(frame, recovery);
    }
}

pub(crate) fn render_page(frame: &mut Frame<'_>, page: &mut SettingsPage) {
    match page {
        SettingsPage::Ready(screen) => render(frame, screen),
        SettingsPage::Invalid { path, message, .. } => {
            let area = frame.area();
            let block = Block::default()
                .title(" Global Settings — Invalid Config ")
                .borders(Borders::ALL);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            if inner.width > 0 && inner.height > 0 {
                frame.render_widget(
                    Paragraph::new(Text::from(vec![
                        Line::styled(
                            "The current Global Config could not be loaded.",
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ),
                        Line::raw("No previous settings are shown as current."),
                        Line::raw(""),
                        Line::raw(truncate_end(
                            path.to_string_lossy().as_ref(),
                            usize::from(inner.width),
                        )),
                        Line::raw(""),
                        Line::raw(message.clone()),
                        Line::raw(""),
                        Line::raw("r Reload    e Open TOML in Editor    Esc Back"),
                    ]))
                    .wrap(Wrap { trim: false }),
                    inner,
                );
            }
        }
    }
}

fn render_confirmation(frame: &mut Frame<'_>, confirmation: &ConfirmationModal) {
    let area = centered_modal(frame.area(), 66, 10);
    let title = match confirmation {
        ConfirmationModal::ResetEditor => " Reset Editor Configuration ",
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width > 0 && inner.height > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::raw("Resetting editor.command removes the complete [editor] section."),
                Line::raw("editor.args and editor.mode will also be removed."),
                Line::raw(""),
                Line::raw("Enter Reset    Esc Cancel"),
            ]))
            .wrap(Wrap { trim: false }),
            inner,
        );
    }
}

fn render_recovery(frame: &mut Frame<'_>, recovery: &RecoveryModal) {
    let area = centered_modal(frame.area(), 72, 12);
    let block = Block::default()
        .title(format!(" {} ", recovery.title))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width > 0 && inner.height > 0 {
        let draft_message = if recovery.draft_retained {
            "The edit draft is retained. Reload will not apply or save it."
        } else {
            "No unsaved change is queued for a later save."
        };
        let action_message = if recovery.must_reload {
            "r Reload from disk    Esc Leave Settings"
        } else {
            "r Reload from disk    Esc Dismiss"
        };
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::raw(recovery.message.clone()),
                Line::raw(""),
                Line::raw(draft_message),
                Line::raw("No automatic merge, overwrite, or retry will be performed."),
                Line::raw(""),
                Line::raw(action_message),
            ]))
            .wrap(Wrap { trim: false }),
            inner,
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum DisplayRow {
    Category(SettingsCategory),
    Setting { index: usize, key: SettingKey },
}

fn display_rows() -> Vec<DisplayRow> {
    let mut rows = Vec::with_capacity(14);
    let mut previous_category = None;
    for (index, key) in SettingKey::ALL.into_iter().enumerate() {
        let category = key.category();
        if previous_category != Some(category) {
            rows.push(DisplayRow::Category(category));
            previous_category = Some(category);
        }
        rows.push(DisplayRow::Setting { index, key });
    }
    rows
}

fn selected_row(rows: &[DisplayRow], selected: usize) -> usize {
    rows.iter()
        .position(|row| matches!(row, DisplayRow::Setting { index, .. } if *index == selected))
        .expect("every setting has a display row")
}

fn render_list(frame: &mut Frame<'_>, area: Rect, screen: &mut SettingsScreen) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let block = Block::default().borders(Borders::BOTTOM);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rows = display_rows();
    let selected = selected_row(&rows, screen.selected);
    let visible = usize::from(inner.height);
    if selected < screen.scroll {
        screen.scroll = selected;
    } else if selected >= screen.scroll.saturating_add(visible) {
        screen.scroll = selected + 1 - visible;
    }
    screen.scroll = screen.scroll.min(rows.len().saturating_sub(visible));

    let lines = rows
        .iter()
        .skip(screen.scroll)
        .take(visible)
        .map(|row| match row {
            DisplayRow::Category(category) => Line::from(Span::styled(
                format!("  {}", category.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            DisplayRow::Setting { index, key } => {
                setting_line(screen, *index, *key, usize::from(inner.width))
            }
        });
    frame.render_widget(Paragraph::new(Text::from_iter(lines)), inner);
}

fn setting_line(
    screen: &SettingsScreen,
    index: usize,
    key: SettingKey,
    width: usize,
) -> Line<'static> {
    let selected = index == screen.selected;
    let marker = if selected { "> " } else { "  " };
    let status = if screen.store.document().is_modified(key) {
        "Modified"
    } else {
        "Default"
    };
    let label = key.label();
    let value = screen.store.document().effective_value(key).display();
    let status_width = UnicodeWidthStr::width(status);
    let fixed = 3 + status_width;
    let available = width.saturating_sub(fixed);
    let label_width = if width >= 58 {
        29.min(available)
    } else {
        ((available * 2) / 5).max(8).min(available)
    };
    let value_width = available.saturating_sub(label_width);
    let label = pad_end_display(&truncate_end(label, label_width), label_width);
    let value = pad_end_display(&truncate_end(&value, value_width), value_width);
    let text = format!("{marker}{label} {value}{status}");
    let style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if !screen.store.document().editable(key) {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    Line::styled(text, style)
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, screen: &SettingsScreen) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let key = screen.selected_key();
    let document = screen.store.document();
    let configured = document
        .configured_value(key)
        .map_or_else(|| "(not set)".to_string(), |value| value.display());
    let mut lines = vec![
        Line::styled(
            format!(" {} ", key.path()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(key.description()),
        Line::raw(format!(
            "Built-in: {}",
            crate::settings::SettingsDocument::built_in_value(key).display()
        )),
        Line::raw(format!("Configured: {configured}")),
    ];
    if !document.editable(key) {
        lines.push(Line::styled(
            "Set editor.command before editing this setting.",
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(reason) = screen.store.read_only_reason() {
        lines.push(Line::styled(
            format!("Read-only: {reason}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(notice) = screen.notice.as_ref() {
        lines.push(Line::styled(
            notice.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::BOTTOM))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, notice: Option<&str>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut lines = Vec::new();
    if area.height >= 2 {
        if let Some(notice) = notice {
            lines.push(Line::styled(
                truncate_end(notice, usize::from(area.width)),
                Style::default().fg(Color::Yellow),
            ));
        } else {
            lines.push(Line::from(vec![
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(" Edit  "),
                Span::styled("r", Style::default().fg(Color::Yellow)),
                Span::raw(" Reset  "),
                Span::styled("e", Style::default().fg(Color::Yellow)),
                Span::raw(" Open TOML  "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw(" Back"),
            ]));
        }
    }
    lines.push(Line::styled(
        truncate_end(LIFECYCLE_NOTICE, usize::from(area.width)),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_edit_modal(frame: &mut Frame<'_>, editor: &EditModal) {
    let desired_height = match editor {
        EditModal::Enum(editor) => editor.choices.len() as u16 + 5,
        EditModal::Text(_) => 9,
        EditModal::List(editor) => (editor.items.len() as u16 + 9).clamp(11, 18),
    };
    let area = centered_modal(frame.area(), 72, desired_height);
    let key = editor.key();
    let block = Block::default()
        .title(format!(" Edit {} ", key.path()))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match editor {
        EditModal::Enum(editor) => {
            let mut lines = editor
                .choices
                .iter()
                .enumerate()
                .map(|(index, choice)| {
                    let style = if index == editor.selected {
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    } else {
                        Style::default()
                    };
                    Line::styled(
                        format!(
                            "{} {}",
                            if index == editor.selected { ">" } else { " " },
                            choice.label
                        ),
                        style,
                    )
                })
                .collect::<Vec<_>>();
            if let Some(error) = editor.error.as_ref() {
                lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
            }
            lines.push(Line::raw(""));
            lines.push(Line::raw("Enter Save    Esc Cancel"));
            frame.render_widget(Paragraph::new(Text::from(lines)), inner);
        }
        EditModal::Text(editor) => {
            let kind = if editor.key.input() == SettingInput::Number {
                "Seconds"
            } else {
                "Value"
            };
            let mut lines = vec![
                Line::raw(kind),
                Line::styled(
                    input_window(&editor.input, usize::from(inner.width)),
                    Style::default().fg(Color::Yellow),
                ),
                Line::raw(""),
            ];
            if let Some(error) = editor.error.as_ref() {
                lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
            }
            lines.push(Line::raw("Enter Save    Esc Cancel"));
            frame.render_widget(
                Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
                inner,
            );
        }
        EditModal::List(editor) => {
            let reserved =
                if editor.element.is_some() { 4 } else { 2 } + usize::from(editor.error.is_some());
            let visible = usize::from(inner.height).saturating_sub(reserved).max(1);
            let start = editor
                .selected
                .saturating_add(1)
                .saturating_sub(visible)
                .min(editor.items.len().saturating_sub(visible));
            let mut lines = if editor.items.is_empty() {
                vec![Line::styled(
                    "(empty list)",
                    Style::default().fg(Color::DarkGray),
                )]
            } else {
                editor
                    .items
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(visible)
                    .map(|(index, item)| {
                        let selected = index == editor.selected;
                        let style = if selected {
                            Style::default().fg(Color::Black).bg(Color::Yellow)
                        } else {
                            Style::default()
                        };
                        Line::styled(
                            format!(
                                "{} {}",
                                if selected { ">" } else { " " },
                                truncate_end(item, usize::from(inner.width.saturating_sub(2)))
                            ),
                            style,
                        )
                    })
                    .collect()
            };
            lines.push(Line::raw(""));
            if let Some(element) = editor.element.as_ref() {
                lines.push(Line::raw(if element.index.is_some() {
                    "Edit element"
                } else {
                    "Add element"
                }));
                lines.push(Line::styled(
                    input_window(&element.input, usize::from(inner.width)),
                    Style::default().fg(Color::Yellow),
                ));
                lines.push(Line::raw("Enter Apply element    Esc Cancel element"));
            } else {
                lines.push(Line::raw(
                    "Enter Edit  a Add  d Delete  J/K Move  s Save list  Esc Cancel",
                ));
            }
            if let Some(error) = editor.error.as_ref() {
                lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
            }
            frame.render_widget(
                Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
                inner,
            );
        }
    }
}

fn centered_modal(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn input_window(input: &SingleLineInput, width: usize) -> String {
    let rendered = input.rendered_with_cursor();
    if UnicodeWidthStr::width(rendered.as_str()) <= width {
        return rendered;
    }
    if input.cursor == 0 {
        return truncate_end(&rendered, width);
    }
    let before = &input.value[..input.cursor];
    let after = format!("│{}", &input.value[input.cursor..]);
    let after_width = UnicodeWidthStr::width(after.as_str()).min(width / 2);
    let before_width = width.saturating_sub(after_width + 1);
    let before = truncate_start(before, before_width);
    truncate_end(&format!("{before}{after}"), width)
}

fn truncate_start(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let target = width - 1;
    let mut suffix = Vec::new();
    let mut used = 0;
    for grapheme in value.graphemes(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > target {
            break;
        }
        suffix.push(grapheme);
        used += grapheme_width;
    }
    suffix.reverse();
    format!("…{}", suffix.concat())
}

fn truncate_end(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut rendered = String::new();
    let target = width - 1;
    for grapheme in value.graphemes(true) {
        if UnicodeWidthStr::width(rendered.as_str()) + UnicodeWidthStr::width(grapheme) > target {
            break;
        }
        rendered.push_str(grapheme);
    }
    rendered.push('…');
    rendered
}

fn pad_end_display(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(value));
    format!("{value}{}", " ".repeat(padding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{SettingKey, SettingValue};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use std::fs;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            kind: KeyEventKind::Press,
            modifiers: Default::default(),
        }
    }

    fn event(code: KeyCode) -> TerminalEvent {
        TerminalEvent::Key(key(code))
    }

    fn move_to(screen: &mut SettingsScreen, key: SettingKey) {
        while screen.selected_key() != key {
            screen.handle_event(event(KeyCode::Down));
        }
    }

    fn screen(contents: &str) -> (tempfile::TempDir, SettingsScreen) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, contents).unwrap();
        let store = SettingsStore::load(path).unwrap();
        (temp, SettingsScreen::new(store))
    }

    fn draw(screen: &mut SettingsScreen, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, screen)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn draw_page(page: &mut SettingsPage, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render_page(frame, page)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        buffer
            .content()
            .chunks(usize::from(buffer.area.width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn full_view_renders_all_categories_settings_provenance_and_details() {
        let (_temp, mut screen) = screen("[defaults]\nlanguage = \"cpp\"\n");
        let rendered = buffer_text(&draw(&mut screen, 110, 34));

        for category in ["Defaults", "Runner", "Submit", "Editor"] {
            assert!(rendered.contains(category), "missing category {category}");
        }
        for key in SettingKey::ALL {
            assert!(rendered.contains(key.label()), "missing {}", key.path());
        }
        assert!(rendered.contains("Language"));
        assert!(rendered.contains("C++"));
        assert!(rendered.contains("Modified"));
        assert!(rendered.contains("Built-in: C++"));
        assert!(rendered.contains("Configured: C++"));
        assert!(rendered.contains(LIFECYCLE_NOTICE));
    }

    #[test]
    fn explicit_default_and_empty_list_are_modified_while_absent_values_are_default() {
        let (_temp, mut screen) =
            screen("[defaults]\nlanguage = \"cpp\"\n[runner]\ncpp_flags = []\n");
        let rendered = buffer_text(&draw(&mut screen, 100, 32));

        assert!(rendered.contains("Language"));
        assert!(rendered.contains("C++ flags"));
        assert!(rendered.contains("[]"));
        assert!(rendered.matches("Modified").count() >= 2);
        assert!(rendered.matches("Default").count() >= 8);
    }

    #[test]
    fn small_terminal_scrolls_to_every_setting_and_omits_details() {
        let (_temp, mut screen) = screen("");
        for _ in 0..20 {
            screen.handle_key(key(KeyCode::Down));
        }
        let rendered = buffer_text(&draw(&mut screen, 42, 9));

        assert_eq!(screen.selected_key(), SettingKey::EditorMode);
        assert!(rendered.contains("Editor mode"), "{rendered}");
        assert!(!rendered.contains("Built-in:"));
    }

    #[test]
    fn navigation_never_selects_category_headers_and_actions_target_current_setting() {
        let (_temp, mut screen) = screen("");
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(screen.selected_key(), SettingKey::RunnerPython);
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            SettingsTransition::None
        );
        assert!(matches!(screen.editor, Some(EditModal::Text(_))));
        screen.handle_event(event(KeyCode::Escape));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('r'))),
            SettingsTransition::None
        );
        assert_eq!(screen.selected_key(), SettingKey::RunnerPython);
    }

    #[test]
    fn long_values_are_truncated_in_list_but_complete_in_detail() {
        let (_temp, mut screen) = screen("");
        let long = "python-非常に長い-custom-command-with-a-complete-value";
        screen
            .store_mut()
            .document_mut()
            .set(
                SettingKey::RunnerPython,
                SettingValue::String(long.to_string()),
            )
            .unwrap();
        screen.handle_key(key(KeyCode::Down));

        let rendered = buffer_text(&draw(&mut screen, 80, 24));

        assert!(rendered.contains('…'));
        assert!(rendered.contains("Modified"), "{rendered}");
        assert!(rendered.contains("Configured: python-"), "{rendered}");
        assert!(rendered.contains("complete-value"), "{rendered}");
    }

    #[test]
    fn enum_editor_saves_canonical_value_and_keeps_selection() {
        let (temp, mut screen) = screen("");
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Down));
        screen.handle_event(event(KeyCode::Enter));

        assert!(screen.editor.is_none());
        assert_eq!(screen.selected_key(), SettingKey::DefaultLanguage);
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::DefaultLanguage),
            SettingValue::Language(Language::Python)
        );
        assert!(
            screen
                .store()
                .document()
                .is_modified(SettingKey::DefaultLanguage)
        );
        assert!(
            fs::read_to_string(temp.path().join("config.toml"))
                .unwrap()
                .contains("language = \"python\"")
        );
    }

    #[test]
    fn string_input_supports_unicode_paste_cursor_edits_and_escape() {
        let (temp, mut screen) = screen("");
        move_to(&mut screen, SettingKey::RunnerPython);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Home));
        screen.handle_event(TerminalEvent::Paste("日本".to_string()));
        screen.handle_event(event(KeyCode::Left));
        screen.handle_event(event(KeyCode::Backspace));

        let Some(EditModal::Text(editor)) = screen.editor.as_ref() else {
            panic!("text editor should remain open");
        };
        assert_eq!(editor.input.value, "本python");
        screen.handle_event(event(KeyCode::Escape));

        assert!(screen.editor.is_none());
        assert_eq!(
            fs::read_to_string(temp.path().join("config.toml")).unwrap(),
            ""
        );
        assert!(
            !screen
                .store()
                .document()
                .is_modified(SettingKey::RunnerPython)
        );
    }

    #[test]
    fn multiline_paste_is_rejected_without_joining_lines() {
        let (_temp, mut screen) = screen("");
        move_to(&mut screen, SettingKey::RunnerPython);
        screen.handle_event(event(KeyCode::Enter));
        let before = match screen.editor.as_ref() {
            Some(EditModal::Text(editor)) => editor.input.value.clone(),
            _ => panic!("text editor should be open"),
        };

        screen.handle_event(TerminalEvent::Paste("one\ntwo".to_string()));

        let Some(EditModal::Text(editor)) = screen.editor.as_ref() else {
            panic!("text editor should remain open");
        };
        assert_eq!(editor.input.value, before);
        assert_eq!(
            editor.error.as_deref(),
            Some("Multiline input is not allowed.")
        );
    }

    #[test]
    fn number_editor_rejects_zero_and_saves_a_positive_finite_value() {
        let (temp, mut screen) = screen("");
        move_to(&mut screen, SettingKey::RunnerTimeoutSeconds);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Home));
        screen.handle_event(event(KeyCode::Delete));
        screen.handle_event(event(KeyCode::Char('0')));
        screen.handle_event(event(KeyCode::Enter));
        assert!(matches!(screen.editor, Some(EditModal::Text(_))));

        screen.handle_event(event(KeyCode::Home));
        screen.handle_event(event(KeyCode::Delete));
        screen.handle_event(TerminalEvent::Paste("3.5".to_string()));
        screen.handle_event(event(KeyCode::Enter));

        assert!(screen.editor.is_none());
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerTimeoutSeconds),
            SettingValue::Number(3.5)
        );
        assert!(
            fs::read_to_string(temp.path().join("config.toml"))
                .unwrap()
                .contains("timeout_seconds = 3.5")
        );
    }

    #[test]
    fn list_changes_remain_local_until_save_and_preserve_order() {
        let (temp, mut screen) = screen("");
        let path = temp.path().join("config.toml");
        move_to(&mut screen, SettingKey::RunnerCppFlags);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Char('a')));
        screen.handle_event(TerminalEvent::Paste("-g".to_string()));
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Char('K')));

        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        let Some(EditModal::List(editor)) = screen.editor.as_ref() else {
            panic!("list editor should remain open");
        };
        assert_eq!(editor.items[editor.items.len() - 2], "-g");

        screen.handle_event(event(KeyCode::Char('s')));

        assert!(screen.editor.is_none());
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("cpp_flags"));
        assert!(saved.find("-g").unwrap() < saved.find("-Wextra").unwrap());
    }

    #[test]
    fn list_editor_allows_empty_array_and_distinguishes_element_cancel() {
        let (temp, mut screen) = screen("[runner]\ncpp_flags = [\"-O2\"]\n");
        let path = temp.path().join("config.toml");
        let original = fs::read_to_string(&path).unwrap();
        move_to(&mut screen, SettingKey::RunnerCppFlags);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Char('a')));
        screen.handle_event(TerminalEvent::Paste("draft".to_string()));
        screen.handle_event(event(KeyCode::Escape));

        assert!(matches!(screen.editor, Some(EditModal::List(_))));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        screen.handle_event(event(KeyCode::Char('d')));
        screen.handle_event(event(KeyCode::Char('s')));

        assert!(screen.editor.is_none());
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerCppFlags),
            SettingValue::StringList(Vec::new())
        );
        assert!(
            screen
                .store()
                .document()
                .is_modified(SettingKey::RunnerCppFlags)
        );
    }

    #[test]
    fn list_element_validation_matches_the_existing_empty_string_contract() {
        let (_temp, mut screen) = screen("");
        move_to(&mut screen, SettingKey::RunnerCppFlags);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Char('a')));
        screen.handle_event(event(KeyCode::Enter));

        let Some(EditModal::List(editor)) = screen.editor.as_ref() else {
            panic!("list editor should remain open");
        };
        assert!(editor.element.is_none());
        assert!(editor.error.is_none());
        assert_eq!(editor.items.last().map(String::as_str), Some(""));
    }

    #[test]
    fn list_editor_scrolls_to_the_selected_element_in_a_small_modal() {
        let items = (0..20)
            .map(|index| format!("\"flag{index}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let (_temp, mut screen) = screen(&format!("[runner]\ncpp_flags = [{items}]\n"));
        move_to(&mut screen, SettingKey::RunnerCppFlags);
        screen.handle_event(event(KeyCode::Enter));
        for _ in 0..19 {
            screen.handle_event(event(KeyCode::Down));
        }

        let rendered = buffer_text(&draw(&mut screen, 50, 12));

        assert!(rendered.contains("flag19"), "{rendered}");
    }

    #[test]
    fn modal_input_does_not_propagate_to_settings_navigation() {
        let (_temp, mut screen) = screen("");
        move_to(&mut screen, SettingKey::RunnerPython);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Down));
        screen.handle_event(event(KeyCode::Char('j')));

        assert_eq!(screen.selected_key(), SettingKey::RunnerPython);
        let Some(EditModal::Text(editor)) = screen.editor.as_ref() else {
            panic!("text editor should remain open");
        };
        assert!(editor.input.value.ends_with('j'));
    }

    #[test]
    fn all_ten_settings_select_the_expected_editor_when_dependencies_are_satisfied() {
        let document = SettingsDocument::parse(
            "[editor]\ncommand = \"code\"\nargs = []\nmode = \"external\"\n",
        )
        .unwrap();
        for key in SettingKey::ALL {
            let editor = EditModal::for_key(key, &document);
            match key.input() {
                SettingInput::Enum => assert!(matches!(editor, EditModal::Enum(_))),
                SettingInput::String | SettingInput::Number => {
                    assert!(matches!(editor, EditModal::Text(_)))
                }
                SettingInput::StringList => assert!(matches!(editor, EditModal::List(_))),
            }
        }
    }

    #[test]
    fn resetting_editor_command_requires_confirmation_and_removes_the_whole_section() {
        let (temp, mut screen) =
            screen("[editor]\ncommand = \"code\"\nargs = [\"--wait\"]\nmode = \"external\"\n");
        let path = temp.path().join("config.toml");
        let original = fs::read_to_string(&path).unwrap();
        move_to(&mut screen, SettingKey::EditorCommand);

        screen.handle_event(event(KeyCode::Char('r')));
        assert!(matches!(
            screen.confirmation,
            Some(ConfirmationModal::ResetEditor)
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        screen.handle_event(event(KeyCode::Escape));
        assert!(screen.confirmation.is_none());

        screen.handle_event(event(KeyCode::Char('r')));
        screen.handle_event(event(KeyCode::Enter));

        assert!(screen.confirmation.is_none());
        assert!(!screen.store().document().editor_configured());
        let saved = fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("[editor]"));
        assert!(!saved.contains("--wait"));
        assert!(!saved.contains("external"));
    }

    #[test]
    fn conflict_reload_keeps_draft_without_reapplying_or_saving_it() {
        let (temp, mut screen) = screen("");
        let path = temp.path().join("config.toml");
        move_to(&mut screen, SettingKey::RunnerPython);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Home));
        screen.handle_event(TerminalEvent::Paste("draft-".to_string()));
        fs::write(&path, "[runner]\npython = \"external\"\n").unwrap();

        screen.handle_event(event(KeyCode::Enter));

        assert_eq!(
            screen.recovery.as_ref().map(|modal| modal.title),
            Some("Conflict")
        );
        assert!(screen.editor.is_some());
        assert_eq!(screen.store().document().candidate(), "");
        let Some(EditModal::Text(editor)) = screen.editor.as_ref() else {
            panic!("text draft should remain open");
        };
        assert_eq!(editor.input.value, "draft-python");
        assert!(fs::read_to_string(&path).unwrap().contains("external"));

        screen.handle_event(event(KeyCode::Escape));
        assert!(screen.recovery.is_none());
        assert!(screen.editor.is_some());
        assert_eq!(screen.store().document().candidate(), "");

        screen.handle_event(event(KeyCode::Enter));
        assert!(screen.recovery.is_some());
        screen.handle_event(event(KeyCode::Char('r')));

        assert!(screen.recovery.is_none());
        let Some(EditModal::Text(editor)) = screen.editor.as_ref() else {
            panic!("Reload must retain the text draft for review");
        };
        assert_eq!(editor.input.value, "draft-python");
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerPython),
            SettingValue::String("external".to_string())
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[runner]\npython = \"external\"\n"
        );
        screen.handle_event(event(KeyCode::Escape));
        assert!(screen.editor.is_none());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[runner]\npython = \"external\"\n"
        );
        assert_eq!(screen.selected_key(), SettingKey::RunnerPython);
    }

    #[test]
    fn conflict_reload_requires_an_explicit_save_to_apply_the_retained_draft() {
        let (temp, mut screen) = screen("");
        let path = temp.path().join("config.toml");
        move_to(&mut screen, SettingKey::RunnerPython);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Home));
        screen.handle_event(TerminalEvent::Paste("draft-".to_string()));
        fs::write(&path, "[runner]\npython = \"external\"\n").unwrap();
        screen.handle_event(event(KeyCode::Enter));
        assert!(screen.recovery.is_some());

        screen.handle_event(event(KeyCode::Char('r')));
        assert!(screen.editor.is_some());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[runner]\npython = \"external\"\n"
        );
        screen.handle_event(event(KeyCode::Enter));
        assert!(screen.editor.is_none());
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerPython),
            SettingValue::String("draft-python".to_string())
        );
        assert!(fs::read_to_string(&path).unwrap().contains("draft-python"));
    }

    #[test]
    fn cancelled_string_after_failed_save_does_not_leak_into_next_save() {
        let (temp, mut screen) = screen("");
        let path = temp.path().join("config.toml");
        move_to(&mut screen, SettingKey::RunnerPython);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Home));
        screen.handle_event(TerminalEvent::Paste("draft-".to_string()));
        fs::write(&path, "# external edit\n").unwrap();
        screen.handle_event(event(KeyCode::Enter));

        assert!(screen.recovery.is_some());
        assert!(
            !screen
                .store()
                .document()
                .is_modified(SettingKey::RunnerPython)
        );
        let rendered = buffer_text(&draw(&mut screen, 100, 30));
        assert!(!rendered.contains("Configured: draft-python"));

        screen.handle_event(event(KeyCode::Escape)); // recovery
        screen.handle_event(event(KeyCode::Escape)); // edit
        fs::write(&path, "").unwrap(); // restore the exact baseline in-place
        move_to(&mut screen, SettingKey::RunnerCppCompiler);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Enter));

        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("cpp_compiler"), "{saved}");
        assert!(!saved.contains("draft-python"), "{saved}");
        assert!(
            !screen
                .store()
                .document()
                .is_modified(SettingKey::RunnerPython)
        );
    }

    #[test]
    fn cancelled_reset_after_failed_save_does_not_leak_into_next_save() {
        let original = "[runner]\npython = \"custom-python\"\n";
        let (temp, mut screen) = screen(original);
        let path = temp.path().join("config.toml");
        move_to(&mut screen, SettingKey::RunnerPython);
        fs::write(&path, "# external edit\n").unwrap();
        screen.handle_event(event(KeyCode::Char('r')));

        assert!(screen.recovery.is_some());
        assert!(
            screen
                .store()
                .document()
                .is_modified(SettingKey::RunnerPython)
        );
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerPython),
            SettingValue::String("custom-python".to_string())
        );

        screen.handle_event(event(KeyCode::Escape));
        fs::write(&path, original).unwrap(); // restore the exact baseline in-place
        move_to(&mut screen, SettingKey::RunnerCppCompiler);
        screen.handle_event(event(KeyCode::Enter));
        screen.handle_event(event(KeyCode::Enter));

        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("python = \"custom-python\""), "{saved}");
        assert!(saved.contains("cpp_compiler"), "{saved}");
    }

    #[test]
    fn post_commit_candidate_present_promotes_only_the_disk_confirmed_document() {
        let (temp, mut screen) = screen("[runner]\npython = \"old-python\"\n");
        let path = temp.path().join("config.toml");
        let mut candidate = screen.store().clone();
        candidate
            .document_mut()
            .set(
                SettingKey::RunnerPython,
                SettingValue::String("saved-python".to_string()),
            )
            .unwrap();
        let result = candidate
            .save_existing_with_test_hooks(crate::safe_file::replace_file, |_| {
                Err(std::io::Error::other("injected directory sync failure"))
            });

        let error = screen.reconcile_save_result(candidate, result).unwrap_err();

        assert!(matches!(
            error,
            SettingsSaveError::PostCommit {
                state: PostCommitState::CandidatePresent,
                ..
            }
        ));
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerPython),
            SettingValue::String("saved-python".to_string())
        );
        assert!(
            screen
                .store()
                .document()
                .is_modified(SettingKey::RunnerPython)
        );
        screen
            .save_value(
                SettingKey::RunnerCppCompiler,
                SettingValue::String("clang++".to_string()),
            )
            .unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("python = \"saved-python\""), "{saved}");
        assert!(saved.contains("cpp_compiler = \"clang++\""), "{saved}");
    }

    #[test]
    fn pre_commit_replace_failure_keeps_the_saved_document_clean() {
        let original = "[runner]\npython = \"old-python\"\n";
        let (temp, mut screen) = screen(original);
        let path = temp.path().join("config.toml");
        let mut candidate = screen.store().clone();
        candidate
            .document_mut()
            .set(
                SettingKey::RunnerPython,
                SettingValue::String("unsaved-python".to_string()),
            )
            .unwrap();
        let result = candidate.save_existing_with_test_hooks(
            |_, _| Err(std::io::Error::other("injected replace failure")),
            |_| Ok(()),
        );
        let error = screen.reconcile_save_result(candidate, result).unwrap_err();
        assert!(matches!(error, SettingsSaveError::BeforeCommit(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        screen
            .save_value(
                SettingKey::RunnerCppCompiler,
                SettingValue::String("clang++".to_string()),
            )
            .unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("python = \"old-python\""), "{saved}");
        assert!(saved.contains("cpp_compiler = \"clang++\""), "{saved}");
        assert!(!saved.contains("unsaved-python"), "{saved}");
    }

    #[test]
    fn post_commit_old_bytes_keep_old_state_and_do_not_carry_unsaved_draft() {
        let original = "[runner]\npython = \"old-python\"\n";
        let (temp, mut screen) = screen(original);
        let path = temp.path().join("config.toml");
        let mut candidate = screen.store().clone();
        candidate
            .document_mut()
            .set(
                SettingKey::RunnerPython,
                SettingValue::String("unsaved-python".to_string()),
            )
            .unwrap();
        let result = candidate.save_existing_with_test_hooks(
            |staging, target| {
                crate::safe_file::replace_file(staging, target)?;
                fs::write(target, original)
            },
            |_| Err(std::io::Error::other("injected directory sync failure")),
        );

        let error = screen.reconcile_save_result(candidate, result).unwrap_err();
        assert!(matches!(
            error,
            SettingsSaveError::PostCommit {
                state: PostCommitState::BaselinePresent,
                ..
            }
        ));
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerPython),
            SettingValue::String("old-python".to_string())
        );
        screen
            .save_value(
                SettingKey::RunnerCppCompiler,
                SettingValue::String("clang++".to_string()),
            )
            .unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("python = \"old-python\""), "{saved}");
        assert!(saved.contains("cpp_compiler = \"clang++\""), "{saved}");
        assert!(!saved.contains("unsaved-python"), "{saved}");
    }

    #[test]
    fn post_commit_other_bytes_require_reload_before_next_save() {
        let (temp, mut screen) = screen("[runner]\npython = \"old-python\"\n");
        let path = temp.path().join("config.toml");
        let mut candidate = screen.store().clone();
        candidate
            .document_mut()
            .set(
                SettingKey::RunnerPython,
                SettingValue::String("unsaved-python".to_string()),
            )
            .unwrap();
        let result = candidate.save_existing_with_test_hooks(
            |staging, target| {
                crate::safe_file::replace_file(staging, target)?;
                fs::write(target, "[runner]\npython = \"external-python\"\n")
            },
            |_| Err(std::io::Error::other("injected directory sync failure")),
        );

        let error = screen.reconcile_save_result(candidate, result).unwrap_err();
        screen.recovery = Some(RecoveryModal::from_save_error(error, true));

        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerPython),
            SettingValue::String("old-python".to_string())
        );
        let rendered = buffer_text(&draw(&mut screen, 100, 30));
        assert!(rendered.contains("No previous settings are shown as current."));
        assert!(!rendered.contains("Python command"));
        assert!(screen.recovery.is_some());
        screen.handle_event(event(KeyCode::Char('r')));
        assert!(screen.recovery.is_none());
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerPython),
            SettingValue::String("external-python".to_string())
        );
        screen
            .save_value(
                SettingKey::RunnerCppCompiler,
                SettingValue::String("clang++".to_string()),
            )
            .unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("python = \"external-python\""), "{saved}");
        assert!(saved.contains("cpp_compiler = \"clang++\""), "{saved}");
        assert!(!saved.contains("unsaved-python"), "{saved}");
    }

    #[test]
    fn unknown_save_state_escape_leaves_settings_instead_of_exposing_stale_values() {
        let (_temp, mut screen) = screen("");
        screen.recovery = Some(RecoveryModal::from_save_error(
            SettingsSaveError::PostCommit {
                source: std::io::Error::other("injected unreadable replacement"),
                state: PostCommitState::Unknown,
            },
            false,
        ));

        assert_eq!(
            screen.handle_event(event(KeyCode::Escape)),
            SettingsTransition::Back
        );
        assert!(screen.recovery.is_some());
    }

    #[test]
    fn invalid_page_never_displays_previous_values_and_can_reload() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[unknown]\nvalue = \"stale-secret\"\n").unwrap();
        let mut page = SettingsPage::load(&path);

        let rendered = buffer_text(&draw_page(&mut page, 80, 24));
        assert!(rendered.contains("Invalid Config"));
        assert!(rendered.contains("No previous settings are shown as current."));
        assert!(!rendered.contains("stale-secret"));

        fs::write(&path, "[runner]\npython = \"fresh\"\n").unwrap();
        page.handle_event(event(KeyCode::Char('r')));

        let SettingsPage::Ready(screen) = page else {
            panic!("valid reload should restore the settings list");
        };
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerPython),
            SettingValue::String("fresh".to_string())
        );
    }

    #[test]
    fn external_editor_reload_switches_to_invalid_state_instead_of_showing_stale_values() {
        let temp = tempfile::tempdir().unwrap();
        // The invalid page includes the path. A generic "old" assertion also
        // matches macOS's /var/folders temp root (and this filename).
        let path = temp.path().join("folder-config.toml");
        let stale = "stale-python-before-editor";
        fs::write(&path, format!("[runner]\npython = \"{stale}\"\n")).unwrap();
        let mut page = SettingsPage::load(&path);
        let SettingsPage::Ready(screen) = &page else {
            panic!("the original config should load before the external edit");
        };
        assert_eq!(
            screen
                .store()
                .document()
                .effective_value(SettingKey::RunnerPython),
            SettingValue::String(stale.to_string())
        );
        fs::write(&path, "not valid toml = [").unwrap();

        page.reload_after_editor();

        assert!(matches!(page, SettingsPage::Invalid { .. }));
        let rendered = buffer_text(&draw_page(&mut page, 80, 24));
        assert!(rendered.contains("Invalid Config"));
        assert!(!rendered.contains(stale));
    }

    #[test]
    fn read_only_document_blocks_structured_edit_but_keeps_open_editor_action() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runner]\npython = \"python3\"").unwrap();
        let mut page = SettingsPage::load(&path);
        let SettingsPage::Ready(screen) = &mut page else {
            panic!("valid non-roundtrippable config should remain viewable");
        };
        move_to(screen, SettingKey::RunnerPython);
        assert!(screen.store().read_only_reason().is_some());

        assert_eq!(
            screen.handle_event(event(KeyCode::Enter)),
            SettingsTransition::None
        );
        assert!(screen.editor.is_none());
        assert_eq!(
            screen.handle_event(event(KeyCode::Char('e'))),
            SettingsTransition::OpenEditor
        );
    }
}
