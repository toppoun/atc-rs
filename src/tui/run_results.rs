use std::collections::{HashMap, VecDeque};
use std::io;
use std::mem::size_of;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::app::{CaseState, ExecutionState, UserInputRunState};
use super::message::{
    Message, RunId, RunKind, RunRequest, RunScope, RunTarget, StressEvent, TestEvent,
    UserInputRunStatus, UserInputRunTarget,
};

const MAX_COMPLETED_TASKS: usize = 64;
const MAX_COMPLETED_BYTES: usize = 64 * 1024 * 1024;
const MAX_USER_INPUT_RESULTS_PER_TASK: usize = 8;

#[derive(Clone, Default)]
pub(crate) struct RunResultStore(Arc<Mutex<Store>>);

#[derive(Default)]
struct Store {
    scopes: HashMap<RunScope, ScopeResults>,
    completed: VecDeque<(RunTarget, usize)>,
    completed_bytes: usize,
}

#[derive(Default)]
struct ScopeResults {
    tasks: HashMap<Arc<str>, TaskResult>,
    view: Option<Subscriber>,
    watch_warning: Option<String>,
}

struct Subscriber {
    token: Arc<()>,
    sender: Sender<Message>,
    dirty: HashMap<Arc<str>, RunId>,
    debug: bool,
}

#[derive(Clone)]
pub(super) struct TaskResult {
    pub(super) run_id: RunId,
    pub(super) execution: ExecutionState,
    pub(super) user_inputs: HashMap<UserInputRunTarget, UserInputRunState>,
    pub(super) layout_known: bool,
    pub(super) requeued: u64,
    in_flight: bool,
}

pub(super) struct ExecutionUpdate {
    pub(super) task_id: Arc<str>,
    pub(super) run_id: RunId,
    pub(super) result: Option<TaskResult>,
}

pub(crate) struct RunView {
    store: RunResultStore,
    scope: RunScope,
    token: Arc<()>,
}

impl std::fmt::Debug for RunView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunView")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(crate) enum RunResults {
    Tracked {
        store: RunResultStore,
        scope: RunScope,
    },
    #[cfg(test)]
    Raw(Arc<Mutex<Option<Sender<Message>>>>),
}

impl RunResultStore {
    pub(crate) fn register(&self, scope: &RunScope) -> RunResults {
        let mut store = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            store
                .scopes
                .insert(scope.clone(), ScopeResults::default())
                .is_none()
        );
        RunResults::Tracked {
            store: self.clone(),
            scope: scope.clone(),
        }
    }
}

impl RunResults {
    pub(crate) fn is_registered(&self) -> bool {
        let mut registered = false;
        self.with_store(|store, scope| registered = store.scopes.contains_key(scope));
        registered
    }

    pub(crate) fn attach(&self, sender: Sender<Message>) -> io::Result<RunView> {
        match self {
            Self::Tracked { store, scope } => {
                let mut state = store
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let entry = state.scopes.get_mut(scope).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "run generation was invalidated")
                })?;
                if entry.view.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "run generation already has a view",
                    ));
                }
                let token = Arc::new(());
                let dirty = entry
                    .tasks
                    .iter()
                    .map(|(task_id, result)| (task_id.clone(), result.run_id))
                    .collect::<HashMap<_, _>>();
                // The initial dirty set and registration share the same lock as publication.
                if !dirty.is_empty() {
                    sender
                        .send(Message::ExecutionChanged)
                        .map_err(io::Error::other)?;
                }
                if let Some(warning) = &entry.watch_warning {
                    sender
                        .send(Message::WorkspaceWatchWarning(Some(warning.clone())))
                        .map_err(io::Error::other)?;
                }
                entry.view = Some(Subscriber {
                    token: token.clone(),
                    sender,
                    dirty,
                    debug: false,
                });
                Ok(RunView {
                    store: store.clone(),
                    scope: scope.clone(),
                    token,
                })
            }
            #[cfg(test)]
            Self::Raw(_) => Err(io::Error::other(
                "raw test results do not support subscriptions",
            )),
        }
    }

    pub(crate) fn invalidate(&self) {
        match self {
            Self::Tracked { store, scope } => {
                let mut state = store
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.scopes.remove(scope);
                let mut removed_bytes = 0;
                state.completed.retain(|(target, bytes)| {
                    if &target.scope != scope {
                        return true;
                    }
                    removed_bytes += bytes;
                    false
                });
                state.completed_bytes -= removed_bytes;
            }
            #[cfg(test)]
            Self::Raw(sender) => {
                *sender
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            }
        }
    }

    fn with_store(&self, action: impl FnOnce(&mut Store, &RunScope)) {
        match self {
            Self::Tracked { store, scope } => action(
                &mut store
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                scope,
            ),
            #[cfg(test)]
            Self::Raw(_) => {}
        }
    }

    pub(crate) fn discard_completed(&self) {
        self.with_store(|state, scope| {
            let mut discarded = Vec::new();
            state.completed.retain(|(target, bytes)| {
                if &target.scope == scope {
                    discarded.push((target.clone(), *bytes));
                    false
                } else {
                    true
                }
            });
            for (target, size) in discarded {
                state.evict(&target, size);
            }
        });
    }

    pub(crate) fn admitted(&self, request: &RunRequest, sample_count: usize) {
        self.with_store(|state, scope| {
            if scope == &request.target.scope {
                state.admitted(request, sample_count);
            }
        });
    }

    pub(crate) fn debug_enabled(&self) -> bool {
        let mut debug = false;
        self.with_store(|store, scope| {
            debug = store
                .scopes
                .get(scope)
                .and_then(|results| results.view.as_ref())
                .is_some_and(|view| view.debug);
        });
        debug
    }

    pub(crate) fn source_removed(&self, target: &RunTarget) {
        self.with_store(|state, scope| {
            if scope == &target.scope
                && let Some(entry) = state.scopes.get_mut(scope)
                && let Some(task) = entry.tasks.get_mut(&target.task_id)
            {
                task.execution.reset_source();
                task.user_inputs.clear();
                let run_id = task.run_id;
                state.changed(target, run_id);
            }
        });
    }

    pub(crate) fn cancelled(&self, request: &RunRequest) {
        self.cancel(&request.target, request.run_id);
    }

    pub(crate) fn cancel(&self, target: &RunTarget, run_id: RunId) {
        self.with_store(|state, scope| {
            state.apply(
                scope,
                Message::RunCancelled {
                    target: target.clone(),
                    run_id,
                },
            );
        });
    }

    pub(crate) fn send(&self, message: Message) -> io::Result<()> {
        match self {
            Self::Tracked { store, scope } => {
                store
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .apply(scope, message);
                Ok(())
            }
            #[cfg(test)]
            Self::Raw(sender) => {
                if let Some(sender) = sender
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    sender.send(message).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "TUI message receiver disconnected from run worker",
                        )
                    })?;
                }
                Ok(())
            }
        }
    }
}

// Direct event receivers let scheduler and executor tests inspect their existing protocol.
#[cfg(test)]
impl From<Sender<Message>> for RunResults {
    fn from(sender: Sender<Message>) -> Self {
        Self::Raw(Arc::new(Mutex::new(Some(sender))))
    }
}

#[cfg(test)]
impl Default for RunResults {
    fn default() -> Self {
        Self::Raw(Arc::default())
    }
}

impl RunView {
    pub(super) fn scope(&self) -> &RunScope {
        &self.scope
    }

    pub(super) fn set_debug(&self, debug: bool) {
        let mut store = self
            .store
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(view) = store
            .scopes
            .get_mut(&self.scope)
            .and_then(|entry| entry.view.as_mut())
            && Arc::ptr_eq(&self.token, &view.token)
        {
            view.debug = debug;
        }
    }

    pub(super) fn take_updates(&self) -> Vec<ExecutionUpdate> {
        let mut store = self
            .store
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = store.scopes.get_mut(&self.scope) else {
            return Vec::new();
        };
        let Some(view) = entry
            .view
            .as_mut()
            .filter(|view| Arc::ptr_eq(&self.token, &view.token))
        else {
            return Vec::new();
        };
        view.dirty
            .drain()
            .map(|(task_id, run_id)| {
                let result = entry.tasks.get(&task_id).cloned();
                ExecutionUpdate {
                    task_id,
                    run_id,
                    result,
                }
            })
            .collect()
    }
}

impl Drop for RunView {
    fn drop(&mut self) {
        let mut store = self
            .store
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = store.scopes.get_mut(&self.scope)
            && entry
                .view
                .as_ref()
                .is_some_and(|view| Arc::ptr_eq(&self.token, &view.token))
        {
            entry.view = None;
        }
    }
}

impl Store {
    fn admitted(&mut self, request: &RunRequest, sample_count: usize) {
        let Some(entry) = self.scopes.get_mut(&request.target.scope) else {
            return;
        };
        if entry
            .tasks
            .get(&request.target.task_id)
            .is_some_and(|task| task.run_id >= request.run_id)
        {
            return;
        }
        let task = entry
            .tasks
            .entry(request.target.task_id.clone())
            .or_insert_with(|| TaskResult {
                run_id: request.run_id,
                execution: ExecutionState {
                    sample_cases: sample_count,
                    total_cases: sample_count,
                    ..ExecutionState::default()
                },
                user_inputs: HashMap::new(),
                layout_known: false,
                requeued: 0,
                in_flight: false,
            });
        task.run_id = request.run_id;
        task.requeued = 0;
        task.in_flight = true;
        task.cancel_user_input();
        match &request.kind {
            RunKind::Samples => {
                task.execution
                    .queue_samples(request.run_id, request.language, request.debug)
            }
            RunKind::Stress { base_seed, count } => {
                task.execution.queue_stress(
                    request.run_id,
                    request.language,
                    *base_seed,
                    count.map(|count| count.get()),
                );
            }
            RunKind::UserInput(snapshot) => {
                task.execution.queue_user_input();
                task.user_inputs.insert(
                    snapshot.target,
                    UserInputRunState::queued(request.run_id, snapshot.clone(), request.language),
                );
                if task.user_inputs.len() > MAX_USER_INPUT_RESULTS_PER_TASK {
                    let oldest = task
                        .user_inputs
                        .iter()
                        .min_by_key(|(_, run)| run.run_id)
                        .map(|(target, _)| *target)
                        .expect("User Input retention limit was exceeded");
                    task.user_inputs.remove(&oldest);
                }
            }
        }
        self.changed(&request.target, request.run_id);
    }

    fn apply(&mut self, scope: &RunScope, message: Message) {
        let Some(entry) = self.scopes.get_mut(scope) else {
            return;
        };
        let (target, run_id) = match &message {
            Message::WorkspaceSource { target, .. } if &target.scope == scope => {
                if let Some(view) = &entry.view {
                    let _ = view.sender.send(message);
                }
                return;
            }
            Message::WorkspaceWatchWarning(warning) => {
                if entry.watch_warning != *warning {
                    entry.watch_warning = warning.clone();
                    if let Some(view) = &entry.view {
                        let _ = view.sender.send(message);
                    }
                }
                return;
            }
            Message::RunStarted { target, run_id }
            | Message::RunRequeued { target, run_id }
            | Message::RunCancelled { target, run_id }
            | Message::RunCompleted { target, run_id }
            | Message::RunFailed { target, run_id, .. }
            | Message::CompileDiagnostics { target, run_id, .. }
            | Message::RunEvent { target, run_id, .. }
            | Message::StressEvent { target, run_id, .. }
            | Message::UserInputRunEvent { target, run_id, .. } => (target.clone(), *run_id),
            Message::WorkerFailed(_) | Message::WorkspaceWatchInvalidated => {
                if let Some(view) = &entry.view {
                    let _ = view.sender.send(message);
                }
                return;
            }
            _ => return,
        };
        if &target.scope != scope {
            return;
        }
        let Some(task) = entry
            .tasks
            .get_mut(&target.task_id)
            .filter(|task| task.run_id == run_id && task.in_flight)
        else {
            return;
        };
        if task.apply(message) {
            self.changed(&target, run_id);
        }
    }

    fn changed(&mut self, target: &RunTarget, run_id: RunId) {
        if let Some(position) = self.completed.iter().position(|(key, _)| key == target) {
            self.completed_bytes -= self.completed.remove(position).unwrap().1;
        }
        let entry = self
            .scopes
            .get_mut(&target.scope)
            .expect("result scope is registered");
        let task = entry
            .tasks
            .get_mut(&target.task_id)
            .expect("result task exists");
        if !task.in_flight {
            let mut bytes = task.retained_bytes();
            if bytes > MAX_COMPLETED_BYTES {
                task.clear_details();
                bytes = task.retained_bytes();
            }
            self.completed_bytes += bytes;
            self.completed.push_back((target.clone(), bytes));
        }
        entry.mark_dirty(&target.task_id, run_id);
        while self.completed.len() > MAX_COMPLETED_TASKS
            || self.completed_bytes > MAX_COMPLETED_BYTES
        {
            let (evicted, size) = self
                .completed
                .pop_front()
                .expect("the completed budget is exceeded");
            self.evict(&evicted, size);
        }
    }

    fn evict(&mut self, target: &RunTarget, size: usize) {
        self.completed_bytes -= size;
        let entry = self.scopes.get_mut(&target.scope).expect("retained scope");
        let task = entry.tasks.remove(&target.task_id).expect("retained task");
        debug_assert!(!task.in_flight);
        if entry.tasks.is_empty() {
            entry.tasks.shrink_to_fit();
        }
        entry.mark_dirty(&target.task_id, task.run_id);
    }
}

impl ScopeResults {
    fn mark_dirty(&mut self, task_id: &Arc<str>, run_id: RunId) {
        if let Some(view) = self.view.as_mut() {
            let notify = view.dirty.is_empty();
            view.dirty.insert(task_id.clone(), run_id);
            if notify && view.sender.send(Message::ExecutionChanged).is_err() {
                self.view = None;
            }
        }
    }
}

impl TaskResult {
    fn cancel_user_input(&mut self) {
        for run in self
            .user_inputs
            .values_mut()
            .filter(|run| run.status.is_active())
        {
            run.status = UserInputRunStatus::Cancelled;
        }
    }

    fn apply(&mut self, message: Message) -> bool {
        let run_id = self.run_id;
        let user_input = self
            .user_inputs
            .values_mut()
            .find(|run| run.run_id == run_id && run.status.is_active());
        match message {
            Message::RunStarted { .. } => match user_input {
                Some(run) => run.started(),
                None => self.execution.started(run_id),
            },
            Message::RunRequeued { .. } => {
                if !self.execution.requeue_joined(run_id) {
                    return false;
                }
                self.requeued += 1;
                true
            }
            Message::CompileDiagnostics { stderr, .. } => {
                self.execution.compile_diagnostics(run_id, stderr)
            }
            Message::RunEvent { event, .. } => {
                let layout_known = matches!(
                    &event,
                    TestEvent::TestCaseLayout { .. } | TestEvent::NoSamples
                );
                let changed = self.execution.run_event(run_id, event);
                self.layout_known |= changed && layout_known;
                changed
            }
            Message::StressEvent { event, .. } => {
                let failed = matches!(&event, StressEvent::Failed { .. });
                let changed = self.execution.stress_event(run_id, event);
                self.layout_known |= changed && failed;
                changed
            }
            Message::UserInputRunEvent {
                snapshot, event, ..
            } => {
                let Some(run) = user_input.filter(|run| run.snapshot == snapshot) else {
                    return false;
                };
                run.apply_event(event);
                true
            }
            Message::RunCancelled { .. } => {
                self.cancel_user_input();
                if self.execution.run.id == Some(run_id) {
                    self.execution.cancel_samples();
                }
                if self.execution.stress.id == Some(run_id) {
                    self.execution.cancel_stress();
                }
                self.in_flight = false;
                true
            }
            Message::RunCompleted { .. } => {
                if let Some(run) = user_input {
                    run.failed("User Input ended without a result");
                }
                self.execution.completed(run_id);
                self.in_flight = false;
                true
            }
            Message::RunFailed { error, .. } => {
                if let Some(run) = user_input {
                    run.failed(&error);
                } else {
                    self.execution.failed(run_id, error);
                }
                self.in_flight = false;
                true
            }
            _ => false,
        }
    }

    // Count retained allocations conservatively, including repeated references to the same text.
    fn retained_bytes(&self) -> usize {
        let text = |value: &Option<Arc<String>>| value.as_ref().map_or(0, |value| value.capacity());
        let run = &self.execution.run;
        let stress = &self.execution.stress;
        let mut bytes = size_of::<Self>()
            + run.cases.capacity() * size_of::<CaseState>()
            + text(&run.error)
            + text(&run.warning)
            + text(&run.compile_diagnostics)
            + text(&stress.error)
            + text(&stress.compile_diagnostics);
        for case in &run.cases {
            bytes +=
                text(&case.input) + text(&case.expected) + text(&case.actual) + text(&case.stderr);
        }
        if let Some(saved) = &self.execution.saved_stress_case {
            bytes += saved.input.capacity() + saved.expected.capacity();
        }
        if let Some(failure) = &stress.failure {
            bytes += failure.input.capacity()
                + failure.expected.capacity()
                + failure.actual.capacity()
                + failure.stderr.capacity()
                + failure.saved_to.capacity();
        }
        for run in self.user_inputs.values() {
            bytes += size_of::<UserInputRunState>()
                + run.stdout.capacity()
                + run.stderr.capacity()
                + text(&run.diagnostic)
                + run.snapshot.input.len()
                + run.snapshot.problem_index.capacity();
        }
        bytes
    }

    fn clear_details(&mut self) {
        let run = &mut self.execution.run;
        for case in &mut run.cases {
            *case = CaseState {
                verdict: case.verdict,
                elapsed: case.elapsed,
                ..CaseState::default()
            };
        }
        run.compile_diagnostics = None;
        let stress = &mut self.execution.stress;
        stress.compile_diagnostics = None;
        if let Some(failure) = &mut stress.failure {
            failure.input = Arc::default();
            failure.expected = Arc::default();
            failure.actual = Arc::default();
            failure.stderr = Arc::default();
        }
        if let Some(saved) = &mut self.execution.saved_stress_case {
            saved.input = Arc::default();
            saved.expected = Arc::default();
        }
        for input in self.user_inputs.values_mut() {
            input.stdout = Arc::default();
            input.stderr = Arc::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;
    use crate::tui::app::RunPhase;
    use std::sync::{Barrier, mpsc};
    use std::time::Duration;

    fn sample(scope: &RunScope, task: &str, run_id: RunId) -> RunRequest {
        RunRequest {
            run_id,
            target: scope.target(task),
            language: Language::Cpp,
            debug: false,
            kind: RunKind::Samples,
        }
    }

    fn start(results: &RunResults, request: &RunRequest) {
        results
            .send(Message::RunStarted {
                target: request.target.clone(),
                run_id: request.run_id,
            })
            .unwrap();
    }

    fn complete(results: &RunResults, request: &RunRequest) {
        results
            .send(Message::RunCompleted {
                target: request.target.clone(),
                run_id: request.run_id,
            })
            .unwrap();
    }

    #[test]
    fn subscription_and_concurrent_publication_restore_latest_state_once() {
        let store = RunResultStore::default();
        let scope = RunScope::new("abc483");
        let results = store.register(&scope);
        let request = sample(&scope, "task", 1);
        results.admitted(&request, 0);
        start(&results, &request);

        let (tx, rx) = mpsc::channel();
        let barrier = Barrier::new(2);
        let view = std::thread::scope(|threads| {
            threads.spawn(|| {
                barrier.wait();
                complete(&results, &request);
            });
            barrier.wait();
            results.attach(tx).unwrap()
        });

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::ExecutionChanged
        ));
        let updates = view.take_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].result.as_ref().unwrap().execution.run.phase,
            RunPhase::Finished
        );
        assert!(view.take_updates().is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn old_runs_and_generations_cannot_replace_or_recreate_results() {
        let store = RunResultStore::default();
        let old_scope = RunScope::new("abc483");
        let other_scope = RunScope::new("abc480");
        let old = store.register(&old_scope);
        let other = store.register(&other_scope);
        let first = sample(&old_scope, "same", 1);
        let second = sample(&old_scope, "same", 2);
        let other_request = sample(&other_scope, "same", 1);
        old.admitted(&first, 0);
        start(&old, &first);
        old.admitted(&second, 0);
        complete(&old, &first);
        other.admitted(&other_request, 0);
        complete(&other, &other_request);

        let (tx, _rx) = mpsc::channel();
        let old_view = old.attach(tx).unwrap();
        let latest = old_view.take_updates().pop().unwrap().result.unwrap();
        assert_eq!(latest.run_id, 2);
        assert_eq!(latest.execution.run.phase, RunPhase::Queued);
        old.invalidate();

        let new_scope = RunScope::new("abc483");
        let new = store.register(&new_scope);
        let replacement = sample(&new_scope, "same", 1);
        new.admitted(&replacement, 0);
        complete(&old, &second);
        old.admitted(&second, 0);
        new.send(Message::RunCompleted {
            target: first.target,
            run_id: 1,
        })
        .unwrap();

        assert!(old_view.take_updates().is_empty());
        assert!(old.attach(mpsc::channel().0).is_err());
        let state = store.0.lock().unwrap();
        assert!(!state.scopes.contains_key(&old_scope));
        assert_eq!(
            state.scopes[&new_scope].tasks["same"].execution.run.phase,
            RunPhase::Queued
        );
        assert_eq!(
            state.scopes[&other_scope].tasks["same"].execution.run.phase,
            RunPhase::Finished
        );
    }

    #[test]
    fn watcher_projection_rejects_an_old_snapshot_and_a_subscription_for_another_generation() {
        use crate::model::{Contest, Problem};
        use crate::tui::app::{UserInputState, WatchApp};
        use crate::tui::message::RunGeneration;
        use std::path::PathBuf;

        let contest = Contest {
            contest_id: "abc483".into(),
            problems: vec![Problem {
                index: "A".into(),
                title: "A".into(),
                task_id: "task".into(),
                url: String::new(),
                sample_count: 1,
            }],
        };
        let generation = RunGeneration::new(&contest.contest_id);
        let store = RunResultStore::default();
        let results = store.register(generation.scope());
        let mut app = WatchApp::new_with_run_generation(
            &contest,
            generation,
            vec![1],
            vec![None],
            vec![UserInputState::default()],
        )
        .unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp));
        let first = app.queue_run(0).unwrap();
        let latest = app.queue_run(0).unwrap();
        results.admitted(&first, 1);
        complete(&results, &first);

        let old_scope = RunScope::new(&contest.contest_id);
        let old = store.register(&old_scope);
        let (tx, _rx) = mpsc::channel();
        assert!(app.attach_run_view(old.attach(tx).unwrap()).is_err());
        let (tx, _rx) = mpsc::channel();
        app.attach_run_view(results.attach(tx).unwrap()).unwrap();
        assert_eq!(app.current_problem().unwrap().run.id, Some(latest.run_id));
        assert_eq!(app.current_problem().unwrap().run.phase, RunPhase::Queued);

        results.admitted(&latest, 1);
        start(&results, &latest);
        complete(&results, &first);
        assert!(app.apply_execution_updates());
        assert_eq!(
            app.current_problem().unwrap().run.phase,
            RunPhase::Compiling
        );
        assert!(!app.apply_execution_updates());
    }

    #[test]
    fn workspace_result_count_is_sparse_and_eviction_protects_active_and_pending_tasks() {
        let store = RunResultStore::default();
        let scopes: Vec<_> = (0..1000)
            .map(|index| {
                let scope = RunScope::new(&format!("abc{index}"));
                let results = store.register(&scope);
                (scope, results)
            })
            .collect();
        assert!(
            store
                .0
                .lock()
                .unwrap()
                .scopes
                .values()
                .all(|scope| scope.tasks.is_empty())
        );
        let (scope, results) = &scopes[0];
        let active = sample(scope, "active", 1);
        results.admitted(&active, 1);
        start(results, &active);
        let pending = sample(scope, "pending", 2);
        results.admitted(&pending, 1);

        let (tx, rx) = mpsc::channel();
        let view = results.attach(tx).unwrap();
        assert!(matches!(rx.recv().unwrap(), Message::ExecutionChanged));
        assert_eq!(view.take_updates().len(), 2);
        for (scope, results) in scopes.iter().take(MAX_COMPLETED_TASKS + 1) {
            let request = sample(scope, "completed", 3);
            results.admitted(&request, 1);
            complete(results, &request);
        }

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::ExecutionChanged
        ));
        let updates = view.take_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].task_id.as_ref(), "completed");
        assert!(updates[0].result.is_none());
        let state = store.0.lock().unwrap();
        assert_eq!(state.completed.len(), MAX_COMPLETED_TASKS);
        assert_eq!(
            state
                .scopes
                .values()
                .map(|scope| scope.tasks.len())
                .sum::<usize>(),
            MAX_COMPLETED_TASKS + 2
        );
        assert!(state.scopes[scope].tasks["active"].in_flight);
        assert!(state.scopes[scope].tasks["pending"].in_flight);
    }

    #[test]
    fn completed_outputs_obey_byte_budget_and_oversized_results_keep_verdicts() {
        let store = RunResultStore::default();
        let scope = RunScope::new("abc483");
        let results = store.register(&scope);
        let output = Arc::new("x".repeat(8 * 1024 * 1024));
        // The normal runner caps each stream at 8 MiB. Multiple completed runs still need a
        // Workspace-wide cap, and a single test can include much larger input/expected text.
        for index in 0..9 {
            let request = sample(&scope, &format!("task{index}"), index + 1);
            results.admitted(&request, 1);
            start(&results, &request);
            results
                .send(Message::CompileDiagnostics {
                    target: request.target.clone(),
                    run_id: request.run_id,
                    stderr: (*output).clone(),
                })
                .unwrap();
            complete(&results, &request);
        }
        {
            let state = store.0.lock().unwrap();
            assert!(state.completed_bytes <= MAX_COMPLETED_BYTES);
            assert!(!state.scopes[&scope].tasks.contains_key("task0"));
        }
        let request = sample(&scope, "large", 99);
        results.admitted(&request, 1);
        start(&results, &request);
        for event in [
            TestEvent::TestRunStarted { total_cases: 1 },
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::ZERO,
            },
            TestEvent::TestCaseComparison {
                number: 1,
                input: "x".repeat(MAX_COMPLETED_BYTES),
                expected: String::new(),
                actual: "actual".into(),
            },
            TestEvent::TestRunFinished {
                accepted: 0,
                total_cases: 1,
            },
        ] {
            results
                .send(Message::RunEvent {
                    target: request.target.clone(),
                    run_id: request.run_id,
                    event,
                })
                .unwrap();
        }
        complete(&results, &request);
        let state = store.0.lock().unwrap();
        let result = &state.scopes[&scope].tasks["large"];
        assert_eq!(result.execution.run.phase, RunPhase::Finished);
        assert_eq!(
            result.execution.run.cases[0].verdict,
            super::super::app::CaseVerdict::WrongAnswer
        );
        assert!(result.execution.run.cases[0].input.is_none());
        assert!(state.completed_bytes <= MAX_COMPLETED_BYTES);
    }
}
