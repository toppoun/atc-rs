use super::*;

impl UserInputRunState {
    pub(crate) fn queued(
        run_id: RunId,
        snapshot: Arc<UserInputRunSnapshot>,
        language: Language,
    ) -> Self {
        Self {
            run_id,
            snapshot,
            language: Some(language),
            status: UserInputRunStatus::Queued,
            stdout: Arc::default(),
            stderr: Arc::default(),
            diagnostic: None,
            elapsed: None,
        }
    }

    pub(crate) fn started(&mut self) -> bool {
        if self.status != UserInputRunStatus::Queued {
            return false;
        }
        self.status = if self.language == Some(Language::Cpp) {
            UserInputRunStatus::Compiling
        } else {
            UserInputRunStatus::Running
        };
        true
    }

    pub(crate) fn failed(&mut self, error: &str) {
        self.status = UserInputRunStatus::Failed;
        self.diagnostic = Some(Arc::new(error.to_owned()));
    }

    pub(crate) fn apply_event(&mut self, event: UserInputRunEvent) {
        match event {
            UserInputRunEvent::Running => self.status = UserInputRunStatus::Running,
            UserInputRunEvent::Finished(result) => {
                self.status = result.status;
                self.stdout = Arc::new(result.stdout);
                self.stderr = Arc::new(result.stderr);
                self.elapsed = Some(result.elapsed);
            }
        }
    }
}

impl UserInputReadyState {
    pub(super) fn discard_draft_run(&mut self) {
        self.runs
            .retain(|target, _| !matches!(target, UserInputRunTarget::Draft(_)));
    }

    fn run_target(&self, selection: UserInputSelection) -> UserInputRunTarget {
        match selection {
            UserInputSelection::Persisted(id) => UserInputRunTarget::Persisted(id),
            UserInputSelection::Draft => UserInputRunTarget::Draft(self.draft_generation),
        }
    }

    pub fn last_run(&self, selection: UserInputSelection) -> Option<&UserInputRunState> {
        self.runs.get(&self.run_target(selection))
    }

    fn run_content(&self, target: UserInputRunTarget) -> Option<&str> {
        if let Some(edit) = &self.edit {
            match (target, edit.target) {
                (UserInputRunTarget::Draft(generation), UserInputEditTarget::Draft)
                    if generation == self.draft_generation =>
                {
                    return Some(&edit.buffer);
                }
                (UserInputRunTarget::Persisted(id), UserInputEditTarget::Persisted(edit_id))
                    if id == edit_id =>
                {
                    return Some(&edit.buffer);
                }
                _ => {}
            }
        }
        match target {
            UserInputRunTarget::Persisted(id) => self
                .persisted
                .iter()
                .find(|input| input.id == id)
                .map(|input| input.content.as_str()),
            UserInputRunTarget::Draft(_) => None,
        }
    }
}

impl ProblemState {
    pub(super) fn apply_user_input_results(
        &mut self,
        latest: RunId,
        runs: HashMap<UserInputRunTarget, UserInputRunState>,
    ) -> bool {
        let Some(ready) = self.user_inputs.ready_mut() else {
            return false;
        };
        let before = ready.runs.len();
        ready.runs.retain(|target, local| {
            local.run_id > latest
                || local.language.is_none()
                || runs
                    .get(target)
                    .is_some_and(|stored| stored.run_id == local.run_id)
        });
        let mut changed = ready.runs.len() != before;
        for run in runs.into_values() {
            changed |= self.apply_user_input_projection(run);
        }
        changed
    }

    fn apply_user_input_projection(&mut self, run: UserInputRunState) -> bool {
        let Some(ready) = self.user_inputs.ready_mut() else {
            return false;
        };
        let target = run.snapshot.target;
        let local = ready.runs.get(&target);
        let requires_local = self.own_request_id.is_some_and(|own| run.run_id <= own)
            || matches!(target, UserInputRunTarget::Draft(_));
        if run.snapshot.source_revision != self.source_revision
            || run.snapshot.problem_index != self.index
            || self
                .source
                .as_ref()
                .is_some_and(|source| Some(source.language) != run.language)
            || ready.run_content(target) != Some(run.snapshot.input.as_ref())
            || requires_local && !local.is_some_and(|local| local.run_id == run.run_id)
            || local == Some(&run)
        {
            return false;
        }
        ready.runs.insert(target, run);
        true
    }
}

impl WatchApp {
    // This explicit refresh is scoped to the clicked read-only ID. In particular it must not
    // replace another row's persisted content, which is the active editor's Save baseline.
    pub(in crate::tui) fn refresh_user_input_run_target(
        &mut self,
        problem: usize,
        id: u64,
        loaded: Result<Option<PersistedUserInputState>, String>,
    ) -> bool {
        let previous = self.displayed_detail_case();
        let state = &mut self.problems[problem];
        let Some(ready) = state.user_inputs.ready_mut() else {
            return false;
        };
        let Some(position) = ready.persisted.iter().position(|input| input.id == id) else {
            return false;
        };
        debug_assert!(
            !ready
                .edit
                .as_ref()
                .is_some_and(|edit| edit.target == UserInputEditTarget::Persisted(id))
        );
        let mut fallback_position = None;
        let available = match loaded {
            Ok(Some(input)) => {
                debug_assert_eq!(input.id, id);
                if ready.persisted[position] != input {
                    ready.runs.remove(&UserInputRunTarget::Persisted(id));
                    ready.persisted[position] = input;
                }
                state.user_input_sync_notice = None;
                true
            }
            Ok(None) => {
                ready.persisted.remove(position);
                ready.runs.remove(&UserInputRunTarget::Persisted(id));
                state.user_input_sync_notice = Some(Arc::new(format!(
                    "User Input {} was removed externally.",
                    position + 1
                )));
                if self.case_selection
                    == Some(CaseSelection::UserInput(UserInputSelection::Persisted(id)))
                {
                    fallback_position = Some(position);
                }
                false
            }
            Err(error) => {
                state.user_input_sync_notice =
                    Some(Arc::new(format!("Could not refresh User Inputs: {error}")));
                false
            }
        };
        self.reconcile_user_input_selection(problem, fallback_position);
        self.reset_folds_if_displayed_case_changed(previous);
        self.invalidate_detail();
        available
    }

    // Called only after read-only sync, or directly for an editor's immutable buffer snapshot.
    pub(in crate::tui) fn enqueue_selected_user_input(&mut self) -> bool {
        self.disarm_user_input_delete();
        let Some(problem) = self.selected_problem() else {
            return false;
        };
        let Some(selection) = self.selected_user_input() else {
            return false;
        };
        let Some(ready) = self.problems[problem].user_inputs.ready() else {
            return false;
        };
        let target = ready.run_target(selection);
        let Some(input) = ready.run_content(target).map(str::to_owned) else {
            return false;
        };
        self.enqueue_user_input_snapshot(problem, target, &input)
    }

    pub(super) fn enqueue_user_input_snapshot(
        &mut self,
        problem: usize,
        target: UserInputRunTarget,
        input: &str,
    ) -> bool {
        if self.watch_generation_invalid {
            return false;
        }
        let state = &self.problems[problem];
        let language = state.source.as_ref().map(|source| source.language);
        let snapshot = Arc::new(UserInputRunSnapshot {
            problem_index: state.index.clone(),
            target,
            input: Arc::from(input),
            source_revision: state.source_revision,
            start_gate: state.user_input_start_gate.clone(),
        });
        let Some(language) = language else {
            // Match the sample runner's source-selection prerequisite, but make failure visible
            // on this input rather than misreporting it as a User Input storage sync failure.
            let run_id = self.run_generation.next_run_id();
            self.problems[problem]
                .user_inputs
                .ready_mut()
                .expect("run target must be ready")
                .runs
                .insert(
                    target,
                    UserInputRunState {
                        run_id,
                        snapshot,
                        language: None,
                        status: UserInputRunStatus::Failed,
                        stdout: Arc::default(),
                        stderr: Arc::default(),
                        diagnostic: Some(Arc::new(
                            "No source is selected. Open or save a source file before running User Input."
                                .to_string(),
                        )),
                        elapsed: None,
                    },
                );
            self.invalidate_detail();
            return true;
        };
        self.retire_user_input_runs();
        // Follow the existing one-shot stress policy. Same-problem sample work is obsolete;
        // different-problem sample work remains eligible for the scheduler's normal requeue.
        self.retire_other_stress_requests(problem);
        let state = &mut self.problems[problem];
        state.execution.queue_user_input();
        let run_id = self.run_generation.next_run_id();
        state.stage_execution(run_id);
        state
            .user_inputs
            .ready_mut()
            .expect("run target must be ready")
            .runs
            .insert(
                target,
                UserInputRunState::queued(run_id, Arc::clone(&snapshot), language),
            );
        self.pending_user_input_runs.push_back(RunRequest {
            run_id,
            target: self.run_target(problem),
            language,
            debug: self.debug && language == Language::Cpp,
            kind: RunKind::UserInput(snapshot),
        });
        // Never reset selection, editor cursor, input scroll, or folds on a result update.
        self.invalidate_detail();
        true
    }

    pub(in crate::tui) fn take_user_input_run_request(&mut self) -> Option<RunRequest> {
        self.pending_user_input_runs.pop_front()
    }

    pub(super) fn clear_selected_user_input_run(&mut self) {
        let Some(selection) = self.selected_user_input() else {
            return;
        };
        let Some(ready) = self.problems[self.selected_problem].user_inputs.ready_mut() else {
            return;
        };
        ready.runs.remove(&ready.run_target(selection));
    }

    pub(super) fn retire_user_input_runs(&mut self) {
        let mut changed = false;
        for state in &mut self.problems {
            if let Some(ready) = state.user_inputs.ready_mut() {
                for run in ready.runs.values_mut().filter(|run| run.status.is_active()) {
                    run.status = UserInputRunStatus::Cancelled;
                    changed = true;
                }
            }
        }
        if changed {
            self.invalidate_detail();
        }
    }

    fn current_user_input_run_mut(
        &mut self,
        problem: usize,
        run_id: RunId,
    ) -> Option<&mut UserInputRunState> {
        let state = self.problems.get_mut(problem)?;
        let ready = state.user_inputs.ready_mut()?;
        let target = ready.runs.iter().find_map(|(target, run)| {
            (run.run_id == run_id
                && run.status.is_active()
                && run.snapshot.source_revision == state.source_revision
                && run.snapshot.problem_index == state.index
                && state.source.as_ref().map(|source| source.language) == run.language
                && ready.run_content(*target) == Some(run.snapshot.input.as_ref()))
            .then_some(*target)
        })?;
        ready.runs.get_mut(&target)
    }

    pub(super) fn user_input_run_started(&mut self, problem: usize, run_id: RunId) -> bool {
        let Some(run) = self.current_user_input_run_mut(problem, run_id) else {
            return false;
        };
        if !run.started() {
            return false;
        }
        self.invalidate_detail();
        true
    }

    pub(super) fn user_input_run_failed(
        &mut self,
        problem: usize,
        run_id: RunId,
        error: &str,
    ) -> bool {
        let Some(run) = self.current_user_input_run_mut(problem, run_id) else {
            return false;
        };
        run.failed(error);
        self.invalidate_detail();
        true
    }

    pub fn user_input_run_event(
        &mut self,
        problem: usize,
        run_id: RunId,
        snapshot: &UserInputRunSnapshot,
        event: UserInputRunEvent,
    ) -> bool {
        let Some(run) = self.current_user_input_run_mut(problem, run_id) else {
            return false;
        };
        if run.snapshot.as_ref() != snapshot {
            return false;
        }
        run.apply_event(event);
        self.invalidate_detail();
        true
    }
}
