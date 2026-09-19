use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::JoinHandle;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::terminal::{KeyCode, KeyEvent, KeyEventKind, TerminalEvent};
use crate::atcoder::{self, AuthenticationVerification};
use crate::auth::{self, AuthSnapshot};
use crate::paths::CookieLocation;

type Verifier = Arc<dyn Fn(Arc<AuthSnapshot>) -> AuthenticationVerification + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoredCredentialState {
    Inspecting,
    Missing,
    Configured,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VerificationState {
    NotStarted,
    Verifying,
    Authenticated { username: Option<String> },
    Rejected,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationState {
    Saving,
    Resetting,
}

#[derive(Debug)]
enum AuthenticationScreen {
    Main,
    SecretInput {
        input: RedactedSecretInput,
        error: Option<&'static str>,
    },
    ResetConfirmation,
}

#[derive(Default)]
struct RedactedSecretInput {
    value: String,
    cursor: usize,
}

impl fmt::Debug for RedactedSecretInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedactedSecretInput")
            .field("empty", &self.value.is_empty())
            .finish()
    }
}

impl RedactedSecretInput {
    fn previous_grapheme_boundary(&self) -> usize {
        self.value[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(start, _)| start)
    }

    fn next_grapheme_boundary(&self) -> usize {
        self.value[self.cursor..]
            .grapheme_indices(true)
            .nth(1)
            .map_or(self.value.len(), |(next, _)| self.cursor + next)
    }

    fn handle_key(&mut self, key: KeyEvent) {
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
                self.value.insert(self.cursor, character);
                self.cursor += character.len_utf8();
            }
            _ => {}
        }
    }

    fn insert_paste(&mut self, text: &str) {
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerPurpose {
    Inspection,
    Verification,
    Write,
    Reset,
}

struct WorkerMessage {
    generation: u64,
    purpose: WorkerPurpose,
    snapshot: Arc<AuthSnapshot>,
    verification: Option<AuthenticationVerification>,
    operation_error: Option<&'static str>,
}

struct WorkerTask {
    generation: u64,
    purpose: WorkerPurpose,
    location: CookieLocation,
    receiver: Receiver<WorkerMessage>,
    handle: JoinHandle<()>,
    recovery: bool,
}

impl fmt::Debug for WorkerTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerTask")
            .field("generation", &self.generation)
            .field("purpose", &self.purpose)
            .field("recovery", &self.recovery)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerTestStage {
    BeforeWork,
    BeforeFreshInspection,
}

type WorkerTestHook = Arc<dyn Fn(WorkerPurpose, WorkerTestStage) + Send + Sync>;

fn call_worker_hook(hook: &Option<WorkerTestHook>, purpose: WorkerPurpose, stage: WorkerTestStage) {
    if let Some(hook) = hook {
        hook(purpose, stage);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthenticationTransition {
    Consumed,
    Close,
}

pub(crate) struct AuthenticationModalController {
    active: bool,
    path: String,
    location: Option<CookieLocation>,
    stored: StoredCredentialState,
    verification: VerificationState,
    mutation: Option<MutationState>,
    screen: AuthenticationScreen,
    guidance: Option<String>,
    warnings: Vec<auth::AuthPersistenceWarning>,
    generation: u64,
    worker: Option<WorkerTask>,
    verifier: Verifier,
    worker_test_hook: Option<WorkerTestHook>,
}

impl fmt::Debug for AuthenticationModalController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticationModalController")
            .field("active", &self.active)
            .field("path", &self.path)
            .field("stored", &self.stored)
            .field("verification", &self.verification)
            .field("mutation", &self.mutation)
            .field("screen", &self.screen)
            .field("guidance", &self.guidance)
            .field("warnings", &self.warnings)
            .field("worker", &self.worker)
            .finish()
    }
}

impl Default for AuthenticationModalController {
    fn default() -> Self {
        Self::new(Arc::new(atcoder::verify_authentication))
    }
}

impl AuthenticationModalController {
    fn new(verifier: Verifier) -> Self {
        Self {
            active: false,
            path: "unavailable".to_string(),
            location: None,
            stored: StoredCredentialState::Inspecting,
            verification: VerificationState::NotStarted,
            mutation: None,
            screen: AuthenticationScreen::Main,
            guidance: None,
            warnings: Vec::new(),
            generation: 0,
            worker: None,
            verifier,
            worker_test_hook: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_verifier(
        verifier: impl Fn(Arc<AuthSnapshot>) -> AuthenticationVerification + Send + Sync + 'static,
    ) -> Self {
        Self::new(Arc::new(verifier))
    }

    #[cfg(test)]
    pub(crate) fn set_worker_hook_for_test(
        &mut self,
        hook: impl Fn(WorkerPurpose, WorkerTestStage) + Send + Sync + 'static,
    ) {
        self.worker_test_hook = Some(Arc::new(hook));
    }

    #[cfg(test)]
    fn start_sender_loss_for_test(&mut self, purpose: WorkerPurpose) {
        let location = self
            .location
            .clone()
            .expect("sender-loss test requires an authentication location");
        self.worker = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let (sender, receiver) = mpsc::channel();
        let handle = std::thread::spawn(move || drop(sender));
        self.worker = Some(WorkerTask {
            generation,
            purpose,
            location,
            receiver,
            handle,
            recovery: false,
        });
        match purpose {
            WorkerPurpose::Inspection => self.stored = StoredCredentialState::Inspecting,
            WorkerPurpose::Verification => self.verification = VerificationState::Verifying,
            WorkerPurpose::Write => self.mutation = Some(MutationState::Saving),
            WorkerPurpose::Reset => self.mutation = Some(MutationState::Resetting),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn open(&mut self, target: Result<(PathBuf, CookieLocation), String>) {
        self.worker = None;
        self.generation = self.generation.wrapping_add(1);
        self.active = true;
        self.location = None;
        self.stored = StoredCredentialState::Inspecting;
        self.verification = VerificationState::NotStarted;
        self.mutation = None;
        self.screen = AuthenticationScreen::Main;
        self.guidance = None;
        self.warnings.clear();

        let (path, location) = match target {
            Ok(target) => target,
            Err(error) => {
                self.path = "unavailable".to_string();
                self.stored = StoredCredentialState::Invalid;
                self.guidance = Some(error);
                return;
            }
        };
        self.path = path.to_string_lossy().into_owned();
        self.location = Some(location.clone());
        self.start_inspection(location);
    }

    pub(crate) fn handle_event(&mut self, event: TerminalEvent) -> AuthenticationTransition {
        if !self.active {
            return AuthenticationTransition::Consumed;
        }

        match event {
            TerminalEvent::Key(key) => self.handle_key(key),
            TerminalEvent::Paste(text) => {
                if let AuthenticationScreen::SecretInput { input, error } = &mut self.screen {
                    input.insert_paste(&text);
                    *error = None;
                }
                AuthenticationTransition::Consumed
            }
            TerminalEvent::Resize(_) | TerminalEvent::Pointer(_) | TerminalEvent::Ignored => {
                AuthenticationTransition::Consumed
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> AuthenticationTransition {
        match &mut self.screen {
            AuthenticationScreen::SecretInput { input, error } => {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Escape {
                    self.screen = AuthenticationScreen::Main;
                    return AuthenticationTransition::Consumed;
                }
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
                    let AuthenticationScreen::SecretInput { input, .. } =
                        std::mem::replace(&mut self.screen, AuthenticationScreen::Main)
                    else {
                        unreachable!("secret input screen must remain active")
                    };
                    match auth::parse_pasted_credential(&input.value) {
                        Ok(credential) => self.start_write(credential),
                        Err(_) => {
                            self.screen = AuthenticationScreen::SecretInput {
                                input,
                                error: Some(
                                    "Enter one cookie value without spaces, attributes, or extra cookies.",
                                ),
                            };
                        }
                    }
                    return AuthenticationTransition::Consumed;
                }
                input.handle_key(key);
                *error = None;
                AuthenticationTransition::Consumed
            }
            AuthenticationScreen::ResetConfirmation => {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Escape {
                    self.screen = AuthenticationScreen::Main;
                } else if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
                    self.start_reset();
                }
                AuthenticationTransition::Consumed
            }
            AuthenticationScreen::Main => {
                if key.kind != KeyEventKind::Press {
                    return AuthenticationTransition::Consumed;
                }
                if key.code == KeyCode::Escape {
                    if self.mutation.is_some() {
                        return AuthenticationTransition::Consumed;
                    }
                    self.close();
                    return AuthenticationTransition::Close;
                }
                if key.modifiers.control || key.modifiers.alt || key.modifiers.super_key {
                    return AuthenticationTransition::Consumed;
                }
                if self.mutation.is_some() {
                    return AuthenticationTransition::Consumed;
                }
                if self.stored == StoredCredentialState::Inspecting {
                    return AuthenticationTransition::Consumed;
                }
                match key.code {
                    KeyCode::Char('p') if self.location.is_some() => {
                        self.screen = AuthenticationScreen::SecretInput {
                            input: RedactedSecretInput::default(),
                            error: None,
                        };
                    }
                    KeyCode::Char('r')
                        if self.location.is_some()
                            && self.stored != StoredCredentialState::Missing =>
                    {
                        self.screen = AuthenticationScreen::ResetConfirmation;
                    }
                    _ => {}
                }
                AuthenticationTransition::Consumed
            }
        }
    }

    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(worker) = self.worker.as_ref() {
            let finished_without_message = match worker.receiver.try_recv() {
                Ok(message) => {
                    self.worker = None;
                    if message.generation == self.generation && self.active {
                        self.apply_worker_message(message);
                        changed = true;
                    }
                    continue;
                }
                Err(TryRecvError::Empty) => worker.handle.is_finished(),
                Err(TryRecvError::Disconnected) => true,
            };
            if !finished_without_message {
                break;
            }

            let worker = self
                .worker
                .take()
                .expect("observed authentication worker must remain present");
            let _ = worker.handle.join();
            match worker.receiver.try_recv() {
                Ok(message) if message.generation == self.generation && self.active => {
                    self.apply_worker_message(message);
                    changed = true;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected)
                    if worker.generation == self.generation && self.active =>
                {
                    if worker.recovery {
                        self.apply_unrecoverable_worker_failure(worker.purpose);
                        changed = true;
                    } else {
                        self.start_worker_recovery(worker.purpose, worker.location);
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            }
        }
        changed
    }

    fn close(&mut self) {
        debug_assert!(self.mutation.is_none());
        self.worker = None;
        self.generation = self.generation.wrapping_add(1);
        self.active = false;
        self.location = None;
        self.verification = VerificationState::NotStarted;
        self.mutation = None;
        self.screen = AuthenticationScreen::Main;
        self.guidance = None;
        self.warnings.clear();
    }

    fn apply_snapshot(&mut self, snapshot: &AuthSnapshot) {
        match snapshot {
            AuthSnapshot::Missing => {
                self.stored = StoredCredentialState::Missing;
                self.verification = VerificationState::NotStarted;
                self.guidance = None;
            }
            AuthSnapshot::Configured { .. } => {
                self.stored = StoredCredentialState::Configured;
                self.guidance = None;
            }
            AuthSnapshot::Invalid(error) => {
                self.stored = StoredCredentialState::Invalid;
                self.verification = VerificationState::NotStarted;
                self.guidance = Some(error.to_string());
            }
        }
    }

    fn start_inspection(&mut self, location: CookieLocation) {
        self.stored = StoredCredentialState::Inspecting;
        self.verification = VerificationState::NotStarted;
        self.spawn_worker(
            WorkerPurpose::Inspection,
            location.clone(),
            move |generation, _| WorkerMessage {
                generation,
                purpose: WorkerPurpose::Inspection,
                snapshot: AuthSnapshot::load_from(&location),
                verification: None,
                operation_error: None,
            },
        );
    }

    fn start_verification(&mut self, snapshot: Arc<AuthSnapshot>, location: CookieLocation) {
        self.verification = VerificationState::Verifying;
        self.warnings.clear();
        let verifier = Arc::clone(&self.verifier);
        self.spawn_worker(
            WorkerPurpose::Verification,
            location.clone(),
            move |generation, hook| {
                let verification = verifier(snapshot);
                call_worker_hook(
                    &hook,
                    WorkerPurpose::Verification,
                    WorkerTestStage::BeforeFreshInspection,
                );
                let fresh = AuthSnapshot::load_from(&location);
                WorkerMessage {
                    generation,
                    purpose: WorkerPurpose::Verification,
                    snapshot: fresh,
                    verification: Some(verification),
                    operation_error: None,
                }
            },
        );
    }

    fn start_write(&mut self, credential: auth::Credential) {
        let Some(location) = self.location.clone() else {
            self.guidance = Some("Authentication cookie path is unavailable.".to_string());
            return;
        };
        self.worker = None;
        self.mutation = Some(MutationState::Saving);
        self.verification = VerificationState::NotStarted;
        self.guidance = None;
        self.warnings.clear();
        self.screen = AuthenticationScreen::Main;
        let verifier = Arc::clone(&self.verifier);
        self.spawn_worker(WorkerPurpose::Write, location.clone(), move |generation, hook| {
            let (snapshot, operation_error) = match auth::write_explicit_credential(
                &location,
                &credential,
            ) {
                Ok(snapshot) => (snapshot, None),
                Err(_) => (
                    AuthSnapshot::load_from(&location),
                    Some(
                        "The cookie could not be saved safely. The displayed state was refreshed from disk.",
                    ),
                ),
            };
            call_worker_hook(
                &hook,
                WorkerPurpose::Write,
                WorkerTestStage::BeforeFreshInspection,
            );
            let verification = matches!(snapshot.as_ref(), AuthSnapshot::Configured { .. })
                .then(|| verifier(Arc::clone(&snapshot)));
            let fresh = AuthSnapshot::load_from(&location);
            WorkerMessage {
                generation,
                purpose: WorkerPurpose::Write,
                snapshot: fresh,
                verification,
                operation_error,
            }
        });
    }

    fn start_reset(&mut self) {
        let Some(location) = self.location.clone() else {
            self.screen = AuthenticationScreen::Main;
            self.guidance = Some("Authentication cookie path is unavailable.".to_string());
            return;
        };
        self.worker = None;
        self.mutation = Some(MutationState::Resetting);
        self.verification = VerificationState::NotStarted;
        self.guidance = None;
        self.warnings.clear();
        self.screen = AuthenticationScreen::Main;
        self.spawn_worker(
            WorkerPurpose::Reset,
            location.clone(),
            move |generation, hook| {
                let operation_error = auth::reset_explicit_credential(&location).err().map(|_| {
                "The cookie could not be reset safely. The displayed state was refreshed from disk."
            });
                call_worker_hook(
                    &hook,
                    WorkerPurpose::Reset,
                    WorkerTestStage::BeforeFreshInspection,
                );
                let fresh = AuthSnapshot::load_from(&location);
                WorkerMessage {
                    generation,
                    purpose: WorkerPurpose::Reset,
                    snapshot: fresh,
                    verification: None,
                    operation_error,
                }
            },
        );
    }

    fn spawn_worker(
        &mut self,
        purpose: WorkerPurpose,
        location: CookieLocation,
        work: impl FnOnce(u64, Option<WorkerTestHook>) -> WorkerMessage + Send + 'static,
    ) {
        debug_assert!(self.worker.is_none());
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let (sender, receiver) = mpsc::channel();
        let hook = self.worker_test_hook.clone();
        let work_hook = hook.clone();
        let handle = std::thread::spawn(move || {
            call_worker_hook(&hook, purpose, WorkerTestStage::BeforeWork);
            let _ = sender.send(work(generation, work_hook));
        });
        self.worker = Some(WorkerTask {
            generation,
            purpose,
            location,
            receiver,
            handle,
            recovery: false,
        });
    }

    fn worker_failure_guidance(purpose: WorkerPurpose) -> &'static str {
        match purpose {
            WorkerPurpose::Inspection => {
                "Authentication state inspection stopped unexpectedly. The displayed state was refreshed from disk."
            }
            WorkerPurpose::Verification => "Authentication verification stopped unexpectedly.",
            WorkerPurpose::Write => {
                "The cookie save operation stopped unexpectedly. The displayed state was refreshed from disk."
            }
            WorkerPurpose::Reset => {
                "The cookie reset operation stopped unexpectedly. The displayed state was refreshed from disk."
            }
        }
    }

    fn start_worker_recovery(&mut self, purpose: WorkerPurpose, location: CookieLocation) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let (sender, receiver) = mpsc::channel();
        let recovery_location = location.clone();
        let handle = std::thread::spawn(move || {
            let _ = sender.send(WorkerMessage {
                generation,
                purpose,
                snapshot: AuthSnapshot::load_from(&recovery_location),
                verification: None,
                operation_error: Some(Self::worker_failure_guidance(purpose)),
            });
        });
        self.worker = Some(WorkerTask {
            generation,
            purpose,
            location,
            receiver,
            handle,
            recovery: true,
        });
    }

    fn apply_unrecoverable_worker_failure(&mut self, purpose: WorkerPurpose) {
        self.apply_worker_message(WorkerMessage {
            generation: self.generation,
            purpose,
            snapshot: Arc::new(AuthSnapshot::Invalid(auth::AuthLoadError::from_io(
                &std::io::Error::other("authentication worker recovery failed"),
            ))),
            verification: None,
            operation_error: Some(
                "The authentication operation stopped and the current stored state could not be inspected safely.",
            ),
        });
    }

    fn apply_worker_message(&mut self, message: WorkerMessage) {
        self.mutation = None;
        self.apply_snapshot(&message.snapshot);
        self.guidance =
            if message.operation_error.is_some() && self.stored == StoredCredentialState::Invalid {
                Some(
                "The operation failed and the current credential state could not be read safely."
                    .to_string(),
            )
            } else {
                message.operation_error.map(str::to_string)
            }
            .or_else(|| {
                if self.stored == StoredCredentialState::Invalid {
                    match message.snapshot.as_ref() {
                        AuthSnapshot::Invalid(error) => Some(error.to_string()),
                        _ => None,
                    }
                } else {
                    None
                }
            });

        if matches!(message.purpose, WorkerPurpose::Inspection) {
            if message.operation_error.is_none()
                && self.stored == StoredCredentialState::Configured
                && let Some(location) = self.location.clone()
            {
                self.start_verification(Arc::clone(&message.snapshot), location);
            }
            return;
        }

        if self.stored != StoredCredentialState::Configured {
            self.verification = VerificationState::NotStarted;
            self.warnings.clear();
            return;
        }

        if message
            .verification
            .as_ref()
            .is_some_and(|verification| !verification.matches_snapshot(&message.snapshot))
        {
            self.verification = VerificationState::NotStarted;
            self.warnings.clear();
            return;
        }

        match message.verification {
            Some(AuthenticationVerification::Authenticated {
                username, warnings, ..
            }) => {
                self.verification = VerificationState::Authenticated { username };
                self.warnings = warnings;
            }
            Some(AuthenticationVerification::Rejected { warnings, .. }) => {
                self.verification = VerificationState::Rejected;
                self.warnings = warnings;
            }
            Some(AuthenticationVerification::Unavailable { warnings, .. }) => {
                self.verification = VerificationState::Unavailable;
                self.warnings = warnings;
            }
            None if matches!(message.purpose, WorkerPurpose::Reset) => {
                self.verification = VerificationState::NotStarted;
            }
            None => self.verification = VerificationState::Unavailable,
        }
    }

    fn status_label(&self) -> &'static str {
        if let Some(mutation) = self.mutation {
            return match mutation {
                MutationState::Saving => "Saving...",
                MutationState::Resetting => "Resetting...",
            };
        }
        match self.stored {
            StoredCredentialState::Inspecting => "Inspecting...",
            StoredCredentialState::Missing => "Not configured",
            StoredCredentialState::Invalid => "Invalid",
            StoredCredentialState::Configured => match self.verification {
                VerificationState::NotStarted => "Verification unavailable",
                VerificationState::Verifying => "Verifying...",
                VerificationState::Authenticated { .. } => "Authenticated",
                VerificationState::Rejected => "Not authenticated",
                VerificationState::Unavailable => "Verification unavailable",
            },
        }
    }

    fn account_label(&self) -> &str {
        match &self.verification {
            VerificationState::Authenticated {
                username: Some(username),
            } if self.stored == StoredCredentialState::Configured => username,
            _ => "—",
        }
    }

    fn paste_label(&self) -> &'static str {
        match self.stored {
            StoredCredentialState::Inspecting => "Paste Cookie",
            StoredCredentialState::Missing => "Paste Cookie",
            StoredCredentialState::Configured => "Replace Cookie",
            StoredCredentialState::Invalid => "Repair Cookie",
        }
    }
}

pub(crate) fn render(frame: &mut Frame<'_>, controller: &AuthenticationModalController) {
    match &controller.screen {
        AuthenticationScreen::Main => render_main(frame, controller),
        AuthenticationScreen::SecretInput { input, error } => {
            render_secret_input(frame, controller.paste_label(), !input.is_empty(), *error)
        }
        AuthenticationScreen::ResetConfirmation => render_reset_confirmation(frame),
    }
}

fn render_main(frame: &mut Frame<'_>, controller: &AuthenticationModalController) {
    let warning_lines = controller.warnings.len().min(2) as u16;
    let guidance_lines = u16::from(controller.guidance.is_some());
    let height = 11_u16
        .saturating_add(warning_lines)
        .saturating_add(guidance_lines.saturating_mul(2));
    let area = centered_rect(frame.area(), 72, height);
    let block = Block::default()
        .title(" Authentication ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let width = usize::from(inner.width);
    let mut lines = vec![
        Line::raw(format!("Status   {}", controller.status_label())),
        Line::raw(format!("Account  {}", controller.account_label())),
        Line::raw(labeled_line("Path     ", &controller.path, width)),
        Line::raw(""),
    ];
    if controller.mutation.is_none()
        && controller.location.is_some()
        && controller.stored != StoredCredentialState::Inspecting
    {
        lines.push(action_line("p", controller.paste_label()));
        if controller.stored != StoredCredentialState::Missing {
            lines.push(action_line("r", "Reset Authentication"));
        }
    }
    if let Some(guidance) = controller.guidance.as_deref() {
        lines.push(Line::raw(""));
        lines.push(Line::raw(guidance));
    }
    for warning in controller.warnings.iter().take(2) {
        lines.push(Line::raw(warning.to_string()));
    }
    lines.push(Line::raw(""));
    lines.push(action_line("Esc", "Close"));
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

fn render_secret_input(
    frame: &mut Frame<'_>,
    title: &str,
    nonempty: bool,
    error: Option<&'static str>,
) {
    let area = centered_rect(frame.area(), 72, if error.is_some() { 12 } else { 10 });
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let value = if nonempty { "<hidden>" } else { "<empty>" };
    let mut lines = vec![
        Line::raw("Paste the REVEL_SESSION value."),
        Line::raw("The \"REVEL_SESSION=\" prefix is optional."),
        Line::raw(""),
        Line::raw(format!("REVEL_SESSION={value}")),
    ];
    if let Some(error) = error {
        lines.push(Line::raw(""));
        lines.push(Line::styled(error, Style::default().fg(Color::Red)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::raw("  Save       "),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::raw("  Cancel"),
    ]));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_reset_confirmation(frame: &mut Frame<'_>) {
    let area = centered_rect(frame.area(), 62, 9);
    let block = Block::default()
        .title(" Reset Authentication ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::raw("Delete the stored authentication cookie?"),
            Line::raw(""),
            Line::raw("This resets the local authentication state."),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw("  Reset       "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw("  Cancel"),
            ]),
        ])),
        inner,
    );
}

fn action_line(key: &'static str, label: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("[{key}]"), Style::default().fg(Color::Yellow)),
        Span::raw(format!(" {label}")),
    ])
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

fn clip_start(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut tail = Vec::new();
    let mut tail_width = 0_usize;
    for grapheme in text.graphemes(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if tail_width.saturating_add(grapheme_width) > width - 1 {
            break;
        }
        tail.push(grapheme);
        tail_width = tail_width.saturating_add(grapheme_width);
    }
    format!("…{}", tail.into_iter().rev().collect::<String>())
}

fn labeled_line(label: &str, value: &str, width: usize) -> String {
    let label_width = UnicodeWidthStr::width(label);
    if width <= label_width {
        return label.chars().take(width).collect();
    }
    format!("{label}{}", clip_start(value, width - label_width))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use super::*;
    use crate::tui::terminal::Modifiers;

    fn key(code: KeyCode) -> TerminalEvent {
        TerminalEvent::Key(KeyEvent {
            code,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::default(),
        })
    }

    fn location(root: &std::path::Path) -> CookieLocation {
        let platform_base = root.join("platform-state");
        let state_dir = platform_base.join("atc").join("state");
        CookieLocation {
            platform_base,
            file: state_dir.join("cookie"),
            state_dir,
        }
    }

    fn authenticated(snapshot: Arc<AuthSnapshot>) -> AuthenticationVerification {
        AuthenticationVerification::authenticated_for_test(&snapshot, Some("test_user".to_string()))
    }

    fn draw(controller: &AuthenticationModalController) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, controller)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &Buffer) -> String {
        buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("")
    }

    fn wait_for_worker(controller: &mut AuthenticationModalController) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if controller.poll() {
                return;
            }
            std::thread::yield_now();
        }
        panic!("authentication worker did not complete");
    }

    fn blocking_gate() -> (Arc<(Mutex<bool>, Condvar)>, Arc<AtomicBool>) {
        (
            Arc::new((Mutex::new(false), Condvar::new())),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn wait_until_started(started: &AtomicBool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if started.load(Ordering::SeqCst) {
                return;
            }
            std::thread::yield_now();
        }
        panic!("authentication test worker did not start");
    }

    fn release_gate(gate: &(Mutex<bool>, Condvar)) {
        let (released, wake) = gate;
        *released.lock().unwrap() = true;
        wake.notify_all();
    }

    fn wait_on_gate(gate: &(Mutex<bool>, Condvar), started: &AtomicBool) {
        started.store(true, Ordering::SeqCst);
        let (released, wake) = gate;
        let mut released = released.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
    }

    #[test]
    fn secret_input_edits_without_debugging_or_rendering_secret_or_length() {
        let marker = "distinctive-secret-19f0";
        let mut input = RedactedSecretInput::default();
        input.insert_paste(marker);
        input.handle_key(KeyEvent {
            code: KeyCode::Home,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::default(),
        });
        input.handle_key(KeyEvent {
            code: KeyCode::Right,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::default(),
        });
        input.handle_key(KeyEvent {
            code: KeyCode::Delete,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::default(),
        });
        input.handle_key(KeyEvent {
            code: KeyCode::Backspace,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::default(),
        });
        input.handle_key(KeyEvent {
            code: KeyCode::Char('v'),
            kind: KeyEventKind::Press,
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
        });
        input.handle_key(KeyEvent {
            code: KeyCode::End,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::default(),
        });
        assert!(!format!("{input:?}").contains(marker));
        assert!(!format!("{input:?}").contains(&marker.len().to_string()));
        let event = TerminalEvent::Paste(marker.to_string());
        assert_eq!(format!("{event:?}"), "Paste(<redacted>)");

        let snapshot = AuthSnapshot::configured_for_test("REVEL_SESSION=lineage-debug-secret");
        let verification = AuthenticationVerification::authenticated_for_test(&snapshot, None);
        assert!(!format!("{verification:?}").contains("lineage-debug-secret"));
    }

    #[test]
    fn paste_modal_renders_only_empty_or_hidden() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let mut controller = AuthenticationModalController::new_with_verifier(authenticated);
        controller.open(Ok((location.file.clone(), location)));
        wait_for_worker(&mut controller);
        controller.handle_event(key(KeyCode::Char('p')));
        let empty = text(&draw(&controller));
        assert!(empty.contains("REVEL_SESSION=<empty>"));

        let secret = "render-secret-4c23";
        controller.handle_event(TerminalEvent::Paste(secret.to_string()));
        let hidden = text(&draw(&controller));
        assert!(hidden.contains("REVEL_SESSION=<hidden>"));
        assert!(!hidden.contains(secret));
        assert!(!format!("{controller:?}").contains(secret));
    }

    #[test]
    fn write_verify_and_reset_follow_the_shared_controller_flow() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let mut controller = AuthenticationModalController::new_with_verifier(authenticated);
        controller.open(Ok((location.file.clone(), location.clone())));
        wait_for_worker(&mut controller);
        assert_eq!(controller.status_label(), "Not configured");

        controller.handle_event(key(KeyCode::Char('p')));
        controller.handle_event(TerminalEvent::Paste(
            "REVEL_SESSION=controller-a".to_string(),
        ));
        controller.handle_event(key(KeyCode::Enter));
        assert_eq!(controller.mutation, Some(MutationState::Saving));
        wait_for_worker(&mut controller);
        assert_eq!(controller.status_label(), "Authenticated");
        assert_eq!(controller.account_label(), "test_user");
        assert!(location.file.is_file());

        controller.handle_event(key(KeyCode::Char('r')));
        assert!(matches!(
            controller.screen,
            AuthenticationScreen::ResetConfirmation
        ));
        let reset = text(&draw(&controller));
        assert!(reset.contains("Delete the stored authentication cookie?"));
        assert!(reset.contains("This resets the local authentication state."));
        controller.handle_event(key(KeyCode::Enter));
        wait_for_worker(&mut controller);
        assert_eq!(controller.status_label(), "Not configured");
        assert!(!location.file.exists());
    }

    #[test]
    fn configured_open_is_one_shot_verifying_then_authenticated() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        std::fs::create_dir_all(&location.state_dir).unwrap();
        std::fs::write(&location.file, "REVEL_SESSION=configured-a").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&location.file, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let mut controller = AuthenticationModalController::new_with_verifier(authenticated);

        controller.open(Ok((location.file.clone(), location)));

        assert_eq!(controller.status_label(), "Inspecting...");
        let inspecting = text(&draw(&controller));
        assert!(inspecting.contains("Status   Inspecting..."));
        wait_for_worker(&mut controller);
        assert_eq!(controller.status_label(), "Verifying...");
        let verifying = text(&draw(&controller));
        assert!(verifying.contains("[p] Replace Cookie"));
        assert!(verifying.contains("[r] Reset Authentication"));
        wait_for_worker(&mut controller);
        assert_eq!(controller.status_label(), "Authenticated");
        assert_eq!(controller.account_label(), "test_user");
        assert!(!controller.poll());
    }

    #[test]
    fn cancel_and_modal_keys_do_not_escape_to_the_home() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let mut controller = AuthenticationModalController::new_with_verifier(authenticated);
        controller.open(Ok((location.file.clone(), location)));
        wait_for_worker(&mut controller);
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char('q'),
            KeyCode::Enter,
            KeyCode::Char('r'),
        ] {
            assert_eq!(
                controller.handle_event(key(code)),
                AuthenticationTransition::Consumed
            );
            assert!(controller.is_active());
        }
        controller.handle_event(key(KeyCode::Char('p')));
        controller.handle_event(TerminalEvent::Paste("cancelled-secret".to_string()));
        controller.handle_event(key(KeyCode::Escape));
        assert!(matches!(controller.screen, AuthenticationScreen::Main));
        assert!(!format!("{controller:?}").contains("cancelled-secret"));
        assert_eq!(
            controller.handle_event(key(KeyCode::Escape)),
            AuthenticationTransition::Close
        );
        assert!(!controller.is_active());
    }

    #[test]
    fn initial_inspection_is_async_and_stale_generation_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let (gate, started) = blocking_gate();
        let inspections = Arc::new(AtomicUsize::new(0));
        let hook_gate = Arc::clone(&gate);
        let hook_started = Arc::clone(&started);
        let hook_inspections = Arc::clone(&inspections);
        let mut controller = AuthenticationModalController::new_with_verifier(authenticated);
        controller.set_worker_hook_for_test(move |purpose, stage| {
            if purpose == WorkerPurpose::Inspection
                && stage == WorkerTestStage::BeforeWork
                && hook_inspections.fetch_add(1, Ordering::SeqCst) == 0
            {
                wait_on_gate(&hook_gate, &hook_started);
            }
        });

        controller.open(Ok((location.file.clone(), location.clone())));
        wait_until_started(&started);
        assert_eq!(controller.status_label(), "Inspecting...");
        assert_eq!(
            controller.handle_event(TerminalEvent::Resize(
                super::super::terminal::TerminalSize {
                    columns: 90,
                    rows: 28,
                },
            )),
            AuthenticationTransition::Consumed
        );
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char('p'),
            KeyCode::Char('r'),
            KeyCode::Char('q'),
            KeyCode::Enter,
        ] {
            assert_eq!(
                controller.handle_event(key(code)),
                AuthenticationTransition::Consumed
            );
            assert!(matches!(controller.screen, AuthenticationScreen::Main));
        }
        assert_eq!(
            controller.handle_event(TerminalEvent::Paste("isolated-secret".to_string())),
            AuthenticationTransition::Consumed
        );
        assert!(!format!("{controller:?}").contains("isolated-secret"));
        assert_eq!(
            controller.handle_event(key(KeyCode::Escape)),
            AuthenticationTransition::Close
        );

        controller.open(Ok((location.file.clone(), location)));
        wait_for_worker(&mut controller);
        assert_eq!(controller.status_label(), "Not configured");
        release_gate(&gate);
        std::thread::sleep(Duration::from_millis(10));
        assert!(!controller.poll());
        assert_eq!(controller.status_label(), "Not configured");
    }

    #[test]
    fn write_verifies_returned_snapshot_and_discards_result_for_external_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let saw_written_snapshot = Arc::new(AtomicBool::new(false));
        let verifier_saw_written = Arc::clone(&saw_written_snapshot);
        let mut controller = AuthenticationModalController::new_with_verifier(move |snapshot| {
            verifier_saw_written.store(
                snapshot
                    .credential()
                    .is_some_and(|credential| credential.as_str() == "REVEL_SESSION=lineage-a"),
                Ordering::SeqCst,
            );
            AuthenticationVerification::authenticated_for_test(
                &snapshot,
                Some("lineage_a_user".to_string()),
            )
        });
        let replacement_location = location.clone();
        controller.set_worker_hook_for_test(move |purpose, stage| {
            if purpose == WorkerPurpose::Write && stage == WorkerTestStage::BeforeFreshInspection {
                let replacement = auth::parse_pasted_credential("lineage-x").unwrap();
                auth::write_explicit_credential(&replacement_location, &replacement).unwrap();
            }
        });

        controller.open(Ok((location.file.clone(), location.clone())));
        wait_for_worker(&mut controller);
        controller.handle_event(key(KeyCode::Char('p')));
        controller.handle_event(TerminalEvent::Paste("lineage-a".to_string()));
        controller.handle_event(key(KeyCode::Enter));
        wait_for_worker(&mut controller);

        assert!(saw_written_snapshot.load(Ordering::SeqCst));
        assert_eq!(controller.stored, StoredCredentialState::Configured);
        assert_eq!(controller.verification, VerificationState::NotStarted);
        assert_eq!(controller.account_label(), "—");
        let fresh = AuthSnapshot::load_from(&location);
        assert!(
            fresh
                .credential()
                .is_some_and(|credential| credential.as_str() == "REVEL_SESSION=lineage-x")
        );
    }

    #[test]
    fn reset_discards_old_verification_even_when_external_credential_appears() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let initial = auth::parse_pasted_credential("reset-lineage-a").unwrap();
        auth::write_explicit_credential(&location, &initial).unwrap();
        let mut controller = AuthenticationModalController::new_with_verifier(authenticated);
        controller.open(Ok((location.file.clone(), location.clone())));
        wait_for_worker(&mut controller);
        wait_for_worker(&mut controller);
        assert_eq!(controller.account_label(), "test_user");

        let replacement_location = location.clone();
        controller.set_worker_hook_for_test(move |purpose, stage| {
            if purpose == WorkerPurpose::Reset && stage == WorkerTestStage::BeforeFreshInspection {
                let replacement = auth::parse_pasted_credential("reset-lineage-x").unwrap();
                auth::write_explicit_credential(&replacement_location, &replacement).unwrap();
            }
        });
        controller.handle_event(key(KeyCode::Char('r')));
        controller.handle_event(key(KeyCode::Enter));
        assert_eq!(controller.account_label(), "—");
        wait_for_worker(&mut controller);

        assert_eq!(controller.stored, StoredCredentialState::Configured);
        assert_eq!(controller.verification, VerificationState::NotStarted);
        assert_eq!(controller.account_label(), "—");
    }

    #[test]
    fn legitimate_rotation_applies_with_persisted_successor_or_bootstrap_fallback() {
        for persistence_fails in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let location = location(temp.path());
            let initial = auth::parse_pasted_credential("rotation-lineage-a").unwrap();
            auth::write_explicit_credential(&location, &initial).unwrap();
            if persistence_fails {
                std::fs::remove_file(location.state_dir.join(".cookie.lock")).unwrap();
                std::fs::create_dir(location.state_dir.join(".cookie.lock")).unwrap();
            }

            let mut controller =
                AuthenticationModalController::new_with_verifier(move |snapshot| {
                    let session = auth::SessionAuth::from_snapshot(&snapshot);
                    let header = reqwest::header::HeaderValue::from_static(
                        "REVEL_SESSION=rotation-lineage-b; Path=/",
                    );
                    session.observe_set_cookie_batch(
                        std::iter::once(&header),
                        &reqwest::Url::parse("https://atcoder.jp/settings").unwrap(),
                    );
                    let warnings = session.persist_pending();
                    AuthenticationVerification::authenticated_for_session_test(
                        &session,
                        Some("rotation_user".to_string()),
                        warnings,
                    )
                });
            controller.open(Ok((location.file.clone(), location.clone())));
            wait_for_worker(&mut controller);
            wait_for_worker(&mut controller);

            assert_eq!(controller.status_label(), "Authenticated");
            assert_eq!(controller.account_label(), "rotation_user");
            let fresh = AuthSnapshot::load_from(&location);
            let expected = if persistence_fails {
                "REVEL_SESSION=rotation-lineage-a"
            } else {
                "REVEL_SESSION=rotation-lineage-b"
            };
            assert!(
                fresh
                    .credential()
                    .is_some_and(|credential| credential.as_str() == expected)
            );
        }
    }

    #[test]
    fn explicit_mutations_block_close_until_the_worker_completes() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let (save_gate, save_started) = blocking_gate();
        let hook_gate = Arc::clone(&save_gate);
        let hook_started = Arc::clone(&save_started);
        let mut controller = AuthenticationModalController::new_with_verifier(authenticated);
        controller.set_worker_hook_for_test(move |purpose, stage| {
            if purpose == WorkerPurpose::Write && stage == WorkerTestStage::BeforeWork {
                wait_on_gate(&hook_gate, &hook_started);
            }
        });
        controller.open(Ok((location.file.clone(), location.clone())));
        wait_for_worker(&mut controller);
        controller.handle_event(key(KeyCode::Char('p')));
        controller.handle_event(TerminalEvent::Paste("close-safe-a".to_string()));
        controller.handle_event(key(KeyCode::Enter));
        wait_until_started(&save_started);
        for code in [KeyCode::Escape, KeyCode::Char('q'), KeyCode::Char('a')] {
            assert_eq!(
                controller.handle_event(key(code)),
                AuthenticationTransition::Consumed
            );
            assert!(controller.is_active());
        }
        release_gate(&save_gate);
        wait_for_worker(&mut controller);
        assert_eq!(controller.status_label(), "Authenticated");

        let (reset_gate, reset_started) = blocking_gate();
        let hook_gate = Arc::clone(&reset_gate);
        let hook_started = Arc::clone(&reset_started);
        controller.set_worker_hook_for_test(move |purpose, stage| {
            if purpose == WorkerPurpose::Reset && stage == WorkerTestStage::BeforeWork {
                wait_on_gate(&hook_gate, &hook_started);
            }
        });
        controller.handle_event(key(KeyCode::Char('r')));
        controller.handle_event(key(KeyCode::Enter));
        wait_until_started(&reset_started);
        for code in [KeyCode::Escape, KeyCode::Char('q'), KeyCode::Char('a')] {
            assert_eq!(
                controller.handle_event(key(code)),
                AuthenticationTransition::Consumed
            );
            assert!(controller.is_active());
        }
        release_gate(&reset_gate);
        wait_for_worker(&mut controller);
        assert_eq!(controller.status_label(), "Not configured");
    }

    #[test]
    fn worker_panics_and_sender_loss_leave_no_permanent_busy_state() {
        for purpose in [
            WorkerPurpose::Inspection,
            WorkerPurpose::Verification,
            WorkerPurpose::Write,
            WorkerPurpose::Reset,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let location = location(temp.path());
            if matches!(purpose, WorkerPurpose::Verification | WorkerPurpose::Reset) {
                let credential = auth::parse_pasted_credential("panic-safe-a").unwrap();
                auth::write_explicit_credential(&location, &credential).unwrap();
            }
            let mut controller = AuthenticationModalController::new_with_verifier(authenticated);
            controller.set_worker_hook_for_test(move |actual, stage| {
                if actual == purpose && stage == WorkerTestStage::BeforeWork {
                    panic!("injected authentication worker panic");
                }
            });
            controller.open(Ok((location.file.clone(), location.clone())));
            wait_for_worker(&mut controller);

            match purpose {
                WorkerPurpose::Inspection => {}
                WorkerPurpose::Verification => wait_for_worker(&mut controller),
                WorkerPurpose::Write => {
                    controller.handle_event(key(KeyCode::Char('p')));
                    controller.handle_event(TerminalEvent::Paste("panic-secret".to_string()));
                    controller.handle_event(key(KeyCode::Enter));
                    wait_for_worker(&mut controller);
                }
                WorkerPurpose::Reset => {
                    wait_for_worker(&mut controller);
                    controller.handle_event(key(KeyCode::Char('r')));
                    controller.handle_event(key(KeyCode::Enter));
                    wait_for_worker(&mut controller);
                }
            }
            assert!(controller.mutation.is_none());
            assert!(!matches!(
                controller.verification,
                VerificationState::Verifying
            ));
            assert_ne!(controller.status_label(), "Inspecting...");
            assert!(!format!("{controller:?}").contains("panic-secret"));
            assert!(controller.is_active());
        }

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let credential = auth::parse_pasted_credential("sender-loss-a").unwrap();
        auth::write_explicit_credential(&location, &credential).unwrap();
        let mut controller = AuthenticationModalController::new_with_verifier(authenticated);
        controller.open(Ok((location.file.clone(), location)));
        wait_for_worker(&mut controller);
        wait_for_worker(&mut controller);
        controller.start_sender_loss_for_test(WorkerPurpose::Verification);
        wait_for_worker(&mut controller);
        assert_eq!(controller.verification, VerificationState::Unavailable);
        assert!(controller.is_active());
    }
}
