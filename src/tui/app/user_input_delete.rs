use super::*;

impl WatchApp {
    pub(in crate::tui) fn user_input_delete_armed(&self) -> Option<u64> {
        self.current_problem()?.user_input_delete_armed
    }

    pub(in crate::tui) fn can_delete_user_input(&self, id: u64) -> bool {
        self.current_problem()
            .and_then(|state| state.user_inputs.ready())
            .is_some_and(|ready| {
                ready.persisted.iter().any(|input| input.id == id)
                    && !ready
                        .edit
                        .as_ref()
                        .is_some_and(|edit| edit.target == UserInputEditTarget::Persisted(id))
            })
    }

    pub(in crate::tui) fn arm_user_input_delete(&mut self, id: u64) -> bool {
        if !self.can_delete_user_input(id) {
            return false;
        }
        self.problems[self.selected_problem].user_input_delete_armed = Some(id);
        // Appearance only: the fixed × / ×? hitbox and Detail identity are unchanged.
        // The input dispatcher requests redraw without invalidating Detail interactions.
        true
    }

    pub(in crate::tui) fn disarm_user_input_delete(&mut self) -> bool {
        let mut changed = false;
        for state in &mut self.problems {
            changed |= state.user_input_delete_armed.take().is_some();
        }
        changed
    }

    pub(in crate::tui) fn complete_user_input_delete(
        &mut self,
        id: u64,
        result: Result<(), String>,
    ) {
        self.disarm_user_input_delete();
        if !self.can_delete_user_input(id) {
            return;
        }
        let problem = self.selected_problem;
        let previous = self.displayed_detail_case();
        let state = &mut self.problems[problem];
        match result {
            Ok(()) => {
                let ready = state
                    .user_inputs
                    .ready_mut()
                    .expect("checked delete target");
                let position = ready.persisted.iter().position(|input| input.id == id);
                ready.persisted.retain(|input| input.id != id);
                ready.runs.remove(&UserInputRunTarget::Persisted(id));
                self.pending_user_input_runs.retain(|request| {
                    request.problem != problem || !matches!(
                        &request.kind,
                        RunKind::UserInput(snapshot) if snapshot.target == UserInputRunTarget::Persisted(id)
                    )
                });
                state.user_input_sync_notice = None;
                let fallback = (self.case_selection
                    == Some(CaseSelection::UserInput(UserInputSelection::Persisted(id))))
                .then_some(position)
                .flatten();
                self.reconcile_user_input_selection(problem, fallback);
                self.reset_folds_if_displayed_case_changed(previous);
                if fallback.is_some() {
                    self.reset_detail_scroll();
                }
            }
            Err(error) => {
                state.user_input_sync_notice =
                    Some(Arc::new(format!("Could not delete User Input: {error}")));
            }
        }
        self.invalidate_detail();
    }

    // Shared by external sync, direct Run reconciliation, and explicit Delete.
    // A removed selection keeps its packed position, then falls back to the previous
    // persisted row, then the normal case selection policy. IDs are never ordinals.
    pub(super) fn reconcile_user_input_selection(
        &mut self,
        problem: usize,
        position: Option<usize>,
    ) {
        if self.selected_problem != problem {
            return;
        }
        if let Some(position) = position {
            self.case_selection = self.problems[problem]
                .user_inputs
                .ready()
                .and_then(|ready| {
                    ready
                        .persisted
                        .get(position.min(ready.persisted.len().saturating_sub(1)))
                })
                .map(|input| CaseSelection::UserInput(UserInputSelection::Persisted(input.id)));
        }
        self.reconcile_case_selection(problem);
    }

    pub(in crate::tui) fn select_case(&mut self, selection: CaseSelection) -> bool {
        let disarmed = self.disarm_user_input_delete();
        let Some(problem) = self.current_problem() else {
            return disarmed;
        };
        if !problem.contains_case_selection(selection)
            || (self.case_selection == Some(selection)
                && problem.detail_mode == DetailMode::Samples)
        {
            return disarmed;
        }
        let previous = self.displayed_detail_case();
        self.problems[self.selected_problem].detail_mode = DetailMode::Samples;
        self.case_selection = Some(selection);
        self.reset_folds_if_displayed_case_changed(previous);
        self.reset_detail_scroll();
        self.invalidate_detail();
        true
    }
}
