use super::*;

/// Test state and case layout shared by the execution store and its display projection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionState {
    pub sample_cases: usize,
    pub total_cases: usize,
    pub saved_stress_case: Option<SavedStressCaseState>,
    pub run: RunState,
    pub stress: StressState,
}

impl ExecutionState {
    pub(super) fn new(
        sample_cases: usize,
        saved_stress_case: Option<SavedStressCaseState>,
    ) -> Self {
        Self {
            sample_cases,
            total_cases: sample_cases + usize::from(saved_stress_case.is_some()),
            saved_stress_case,
            ..Self::default()
        }
    }

    pub(crate) fn queue_samples(&mut self, run_id: RunId, language: Language, debug: bool) {
        let total_cases = self.total_cases;
        self.cancel_stress();
        self.stress.id = None;
        self.run = RunState {
            id: Some(run_id),
            phase: RunPhase::Queued,
            language: Some(language),
            debug,
            test_run_started: false,
            accepted: 0,
            total_cases,
            error: None,
            warning: None,
            compile_diagnostics: None,
            cases: vec![CaseState::default(); total_cases],
        };
    }

    pub(crate) fn queue_stress(
        &mut self,
        run_id: RunId,
        language: Language,
        base_seed: u64,
        count: Option<u64>,
    ) {
        self.cancel_samples();
        self.run.id = None;
        self.stress = StressState {
            id: Some(run_id),
            phase: StressPhase::Queued,
            language: Some(language),
            base_seed: Some(base_seed),
            case_limit: count,
            case_number: 0,
            seed: None,
            passed: 0,
            elapsed: Duration::ZERO,
            cases_per_second: 0.0,
            failure: None,
            error: None,
            compile_diagnostics: None,
        };
    }

    pub(crate) fn queue_user_input(&mut self) {
        self.cancel_samples();
        if self.run.phase == RunPhase::Cancelled {
            self.run.id = None;
        }
        if self.cancel_stress() {
            self.stress.id = None;
        }
    }

    pub(crate) fn cancel_samples(&mut self) {
        if matches!(
            self.run.phase,
            RunPhase::Queued | RunPhase::Compiling | RunPhase::Running
        ) {
            self.run.phase = RunPhase::Cancelled;
        }
    }

    pub(crate) fn cancel_stress(&mut self) -> bool {
        if !matches!(
            self.stress.phase,
            StressPhase::Queued | StressPhase::Compiling | StressPhase::Running
        ) {
            return false;
        }
        self.stress.phase = StressPhase::Cancelled;
        true
    }

    pub(crate) fn reset_source(&mut self) {
        self.run = RunState {
            total_cases: self.total_cases,
            cases: vec![CaseState::default(); self.total_cases],
            ..RunState::default()
        };
        self.cancel_stress();
        self.stress.id = None;
    }

    fn current_run_mut(&mut self, run_id: RunId) -> Option<&mut RunState> {
        let run = &mut self.run;

        if run.id != Some(run_id) {
            return None;
        }

        Some(run)
    }

    fn current_stress_mut(&mut self, run_id: RunId) -> Option<&mut StressState> {
        let stress = &mut self.stress;

        if stress.id != Some(run_id) {
            return None;
        }

        Some(stress)
    }

    pub(super) fn attempt_mode(&self, run_id: RunId) -> Option<DetailMode> {
        if self.run.id == Some(run_id) {
            Some(DetailMode::Samples)
        } else if self.stress.id == Some(run_id) {
            Some(DetailMode::Stress)
        } else {
            None
        }
    }

    pub(crate) fn compile_diagnostics(&mut self, run_id: RunId, stderr: String) -> bool {
        let Some(mode) = self.attempt_mode(run_id) else {
            return false;
        };
        let diagnostics = match mode {
            DetailMode::Samples if self.run.phase == RunPhase::Compiling => {
                &mut self.run.compile_diagnostics
            }
            DetailMode::Stress if self.stress.phase == StressPhase::Compiling => {
                &mut self.stress.compile_diagnostics
            }
            _ => return false,
        };
        *diagnostics = Some(Arc::new(stderr));
        true
    }

    pub(crate) fn started(&mut self, run_id: RunId) -> bool {
        let Some(mode) = self.attempt_mode(run_id) else {
            return false;
        };
        match mode {
            DetailMode::Samples => {
                let run = self
                    .current_run_mut(run_id)
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
                    .current_stress_mut(run_id)
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
        }
    }
    pub(crate) fn completed(&mut self, run_id: RunId) -> bool {
        let Some(mode) = self.attempt_mode(run_id) else {
            return false;
        };
        match mode {
            DetailMode::Samples => {
                let run = self
                    .current_run_mut(run_id)
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
                    .current_stress_mut(run_id)
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
        }
    }
    pub(crate) fn failed(&mut self, run_id: RunId, error: String) -> bool {
        let Some(mode) = self.attempt_mode(run_id) else {
            return false;
        };
        match mode {
            DetailMode::Samples => {
                let run = self
                    .current_run_mut(run_id)
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
                    .current_stress_mut(run_id)
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
        }
    }

    pub(crate) fn requeued(&mut self, run_id: RunId) -> bool {
        if !matches!(self.run.phase, RunPhase::Compiling | RunPhase::Running) {
            return false;
        }
        self.requeue_joined(run_id)
    }

    pub(crate) fn requeue_joined(&mut self, run_id: RunId) -> bool {
        let total_cases = self.total_cases;
        let Some(run) = self.current_run_mut(run_id) else {
            return false;
        };

        // run_id/language are logical-request state and survive preemption. Everything below is
        // physical-attempt state and must be fresh before the same logical run starts again.
        run.phase = RunPhase::Queued;
        run.test_run_started = false;
        run.accepted = 0;
        run.total_cases = total_cases;
        run.error = None;
        run.warning = None;
        run.compile_diagnostics = None;
        run.cases = vec![CaseState::default(); total_cases];

        true
    }

    fn test_case_layout(
        &mut self,
        run_id: RunId,
        sample_cases: usize,
        stress_case: Option<Sample>,
    ) -> bool {
        if self.run.id != Some(run_id)
            || self.run.test_run_started
            || !matches!(self.run.phase, RunPhase::Compiling | RunPhase::Running)
        {
            return false;
        }

        self.sample_cases = sample_cases;
        self.saved_stress_case = stress_case.map(SavedStressCaseState::from);
        let total_cases = sample_cases
            + if self.saved_stress_case.is_some() {
                1
            } else {
                0
            };
        self.total_cases = total_cases;
        self.run.total_cases = total_cases;
        self.run.cases = vec![CaseState::default(); total_cases];

        true
    }

    pub(crate) fn run_event(&mut self, run_id: RunId, event: TestEvent) -> bool {
        let updated_total_cases = match &event {
            TestEvent::TestRunStarted { total_cases } => Some(*total_cases),
            TestEvent::NoSamples => Some(0),
            _ => None,
        };
        let event = match event {
            TestEvent::TestCaseLayout {
                sample_cases,
                stress_case,
            } => return self.test_case_layout(run_id, sample_cases, stress_case),
            event => event,
        };
        let Some(run) = self.current_run_mut(run_id) else {
            return false;
        };
        let changed = match event {
            TestEvent::SavedStressCaseSkipped { reason }
                if !run.test_run_started
                    && matches!(run.phase, RunPhase::Compiling | RunPhase::Running) =>
            {
                run.warning = Some(Arc::new(format!("Saved Stress case skipped: {reason}")));
                true
            }
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
            self.total_cases = total_cases;
            if total_cases == 0 {
                self.sample_cases = 0;
                self.saved_stress_case = None;
            }
        }
        changed
    }

    #[allow(clippy::too_many_arguments)]
    fn stress_failed(
        &mut self,
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
        if self.stress.id != Some(run_id)
            || !matches!(
                self.stress.phase,
                StressPhase::Running | StressPhase::Compiling
            )
        {
            return false;
        }

        let input = Arc::new(input);
        let expected = Arc::new(expected);
        let actual = Arc::new(actual);
        let stderr = Arc::new(stderr);

        self.stress.phase = StressPhase::Failed;
        self.stress.base_seed = Some(base_seed);
        self.stress.case_number = case_number;
        self.stress.seed = Some(seed);
        self.stress.elapsed = elapsed;
        self.stress.failure = Some(StressFailureState {
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

        self.saved_stress_case = Some(SavedStressCaseState {
            input,
            expected: Arc::clone(&expected),
        });
        self.total_cases = self.sample_cases + 1;
        self.run.total_cases = self.total_cases;
        self.run
            .cases
            .resize_with(self.total_cases, CaseState::default);

        let stress_index = self.sample_cases;
        if self
            .run
            .cases
            .get(stress_index)
            .is_some_and(|case| case.verdict == CaseVerdict::Accepted)
        {
            self.run.accepted = self.run.accepted.saturating_sub(1);
        }
        if let Some(case) = self.run.cases.get_mut(stress_index) {
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

        true
    }

    pub(crate) fn stress_event(&mut self, run_id: RunId, event: StressEvent) -> bool {
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

        let Some(stress) = self.current_stress_mut(run_id) else {
            return false;
        };
        match event {
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
        }
    }
}
fn case_mut(run: &mut RunState, number: usize) -> Option<&mut CaseState> {
    let index = number.checked_sub(1)?;
    run.cases.get_mut(index)
}
