use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
    pub detail_mode: DetailMode,
}

#[derive(Debug)]
pub struct WatchApp {
    should_quit: bool,
    debug: bool,
    samples_pane_enabled: bool,

    contest_id: String,
    problems: Vec<ProblemState>,

    selected_problem: usize,
    selected_case: usize,

    detail_scroll: usize,
    detail_revision: u64,

    next_run_id: RunId,
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

#[derive(Debug, Clone)]
pub struct StressFailureState {
    pub kind: CandidateFailureKind,
    pub case_number: u64,
    pub base_seed: u64,
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
    pub expected: Option<Arc<String>>,
    pub actual: Option<Arc<String>>,
    pub stderr: Option<Arc<String>>,
}

impl Default for CaseState {
    fn default() -> Self {
        Self {
            verdict: CaseVerdict::Pending,
            elapsed: None,
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
    pub fn new(contest: &Contest, sample_counts: Vec<usize>) -> io::Result<Self> {
        let stress_cases = vec![None; contest.problems.len()];
        Self::new_with_stress_cases(contest, sample_counts, stress_cases)
    }

    pub fn new_with_stress_cases(
        contest: &Contest,
        sample_counts: Vec<usize>,
        stress_cases: Vec<Option<Sample>>,
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

        let problems = contest
            .problems
            .iter()
            .zip(sample_counts)
            .zip(stress_cases)
            .map(|((problem, sample_cases), stress_case)| {
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
                    detail_mode: DetailMode::Samples,
                }
            })
            .collect();

        Ok(Self {
            should_quit: false,
            debug: false,
            samples_pane_enabled: false,
            contest_id: contest.contest_id.clone(),
            problems,
            selected_problem: 0,
            selected_case: 0,
            detail_scroll: 0,
            detail_revision: 0,
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

    pub fn selected_case(&self) -> usize {
        self.selected_case
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

    pub fn scroll_detail_up(&mut self, lines: usize) -> bool {
        let previous = self.detail_scroll;

        self.detail_scroll = self.detail_scroll.saturating_sub(lines);

        self.detail_scroll != previous
    }

    pub fn scroll_detail_down(&mut self, lines: usize) -> bool {
        let previous = self.detail_scroll;

        self.detail_scroll = self.detail_scroll.saturating_add(lines);

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

    fn current_case_count(&self) -> usize {
        self.current_problem()
            .map(|problem| problem.total_cases)
            .unwrap_or(0)
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

    pub fn select_problem(&mut self, index: usize) -> bool {
        if index >= self.problems.len() || index == self.selected_problem {
            return false;
        }

        self.selected_problem = index;
        self.selected_case = 0;
        self.reset_detail_scroll();
        self.invalidate_detail();
        true
    }

    pub fn next_problem(&mut self) -> bool {
        if self.problems.is_empty() {
            self.selected_problem = 0;
            self.selected_case = 0;
            return false;
        }

        let next = (self.selected_problem + 1) % self.problems.len();
        self.select_problem(next)
    }

    pub fn previous_problem(&mut self) -> bool {
        if self.problems.is_empty() {
            self.selected_problem = 0;
            self.selected_case = 0;
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
        let count = self.current_case_count();

        if count == 0 {
            self.selected_case = 0;
            return false;
        }

        let next = (self.selected_case + 1) % count;
        let mode_changed = self.problems[self.selected_problem].detail_mode != DetailMode::Samples;
        if next == self.selected_case && !mode_changed {
            return false;
        }
        self.problems[self.selected_problem].detail_mode = DetailMode::Samples;
        self.selected_case = next;
        self.reset_detail_scroll();
        self.invalidate_detail();
        true
    }

    pub fn previous_case(&mut self) -> bool {
        let count = self.current_case_count();

        if count == 0 {
            self.selected_case = 0;
            return false;
        }

        let previous = if self.selected_case == 0 {
            count - 1
        } else {
            self.selected_case - 1
        };
        let mode_changed = self.problems[self.selected_problem].detail_mode != DetailMode::Samples;
        if previous == self.selected_case && !mode_changed {
            return false;
        }
        self.problems[self.selected_problem].detail_mode = DetailMode::Samples;
        self.selected_case = previous;
        self.reset_detail_scroll();
        self.invalidate_detail();
        true
    }
    pub fn source_changed(&mut self, problem: usize, path: PathBuf, language: Language) -> bool {
        if problem >= self.problems.len() {
            return false;
        }

        let source = SourceState { path, language };
        debug_assert_eq!(
            source.path.extension(),
            Some(OsStr::new(source.language.extension()))
        );
        self.problems[problem].source = Some(source);
        self.problems[problem].detail_mode = DetailMode::Samples;
        self.selected_problem = problem;
        self.selected_case = 0;
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

        self.problems[problem].detail_mode = DetailMode::Samples;
        self.problems[problem].stress.id = None;
        self.problems[problem].run = RunState {
            id: Some(run_id),
            phase: RunPhase::Queued,
            language: Some(language),
            test_run_started: false,
            accepted: 0,
            total_cases,
            error: None,
            cases: vec![CaseState::default(); total_cases],
        };

        self.reset_detail_scroll();
        if self.selected_problem == problem {
            self.invalidate_detail();
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
        self.problems[problem].run.id = None;
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

        self.reset_detail_scroll();
        if self.selected_problem == problem {
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
        let affects_current_detail = self.selected_problem == problem;
        let Some(mode) = self.attempt_mode(problem, run_id) else {
            return false;
        };

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

        if changed && affects_current_detail {
            self.invalidate_detail();
        }

        changed
    }
    pub fn run_requeued(&mut self, problem: usize, run_id: RunId) -> bool {
        let affects_current_detail = self.selected_problem == problem;
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

        if affects_current_detail {
            if total_cases == 0 || self.selected_case >= total_cases {
                self.selected_case = 0;
            }
            self.reset_detail_scroll();
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
        let affects_current_detail = self.selected_problem == problem;
        let Some(problem_state) = self.problems.get_mut(problem) else {
            return false;
        };
        if problem_state.run.id != Some(run_id)
            || problem_state.run.test_run_started
            || !matches!(problem_state.run.phase, RunPhase::Compiling | RunPhase::Running)
        {
            return false;
        }

        problem_state.sample_cases = sample_cases;
        problem_state.saved_stress_case = stress_case.map(SavedStressCaseState::from);
        let total_cases = sample_cases + if problem_state.saved_stress_case.is_some() { 1 } else { 0 };
        problem_state.total_cases = total_cases;
        problem_state.run.total_cases = total_cases;
        problem_state.run.cases = vec![CaseState::default(); total_cases];

        if affects_current_detail {
            if total_cases == 0 || self.selected_case >= total_cases {
                self.selected_case = 0;
            }
            self.reset_detail_scroll();
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

        let affects_current_detail = self.selected_problem == problem
            && match &event {
                TestEvent::TestCaseAccepted { number, .. }
                | TestEvent::TestCaseComparison { number, .. }
                | TestEvent::TestCaseWrongAnswer { number, .. }
                | TestEvent::TestCaseRuntimeError { number, .. }
                | TestEvent::TestCaseTimedOut { number, .. }
                | TestEvent::TestCaseStderr { number, .. } => {
                    number.checked_sub(1) == Some(self.selected_case)
                }
                _ => true,
            };

        let updated_total_cases = match &event {
            TestEvent::TestRunStarted { total_cases } => Some(*total_cases),
            TestEvent::NoSamples => Some(0),
            _ => None,
        };

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
                expected,
                actual,
            } if run.phase == RunPhase::Running && run.test_run_started => {
                let Some(case) = case_mut(run, number) else {
                    return false;
                };

                if case.expected.is_some() || case.actual.is_some() {
                    return false;
                }

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
            if self.selected_problem == problem
                && (total_cases == 0 || self.selected_case >= total_cases)
            {
                self.selected_case = 0;
            }
            self.reset_detail_scroll();
        }

        if changed && affects_current_detail {
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
        let affects_current_detail = self.selected_problem == problem
            && self
                .problems
                .get(problem)
                .is_some_and(|problem| problem.detail_mode == DetailMode::Stress);
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
        problem_state.stress.base_seed = Some(base_seed);
        problem_state.stress.case_number = case_number;
        problem_state.stress.seed = Some(seed);
        problem_state.stress.elapsed = elapsed;
        problem_state.stress.failure = Some(StressFailureState {
            kind,
            case_number,
            base_seed,
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

        if affects_current_detail {
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
            } if matches!(stress.phase, StressPhase::Queued | StressPhase::Compiling | StressPhase::Running) => {
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

        if changed && affects_current_detail {
            self.invalidate_detail();
        }

        changed
    }

    pub fn run_completed(&mut self, problem: usize, run_id: RunId) -> bool {
        let detail_mode = self.problems.get(problem).map(|problem| problem.detail_mode);
        let affects_current_detail = self.selected_problem == problem;
        let Some(mode) = self.attempt_mode(problem, run_id) else {
            return false;
        };

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

        if changed && affects_current_detail && detail_mode == Some(mode) {
            self.invalidate_detail();
        }

        changed
    }

    pub fn run_failed(&mut self, problem: usize, run_id: RunId, error: String) -> bool {
        let detail_mode = self.problems.get(problem).map(|problem| problem.detail_mode);
        let affects_current_detail = self.selected_problem == problem;
        let Some(mode) = self.attempt_mode(problem, run_id) else {
            return false;
        };

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

        if changed && affects_current_detail && detail_mode == Some(mode) {
            self.invalidate_detail();
        }

        changed
    }

    pub fn problems(&self) -> &[ProblemState] {
        &self.problems
    }

    pub fn selected_case_state(&self) -> Option<&CaseState> {
        self.current_problem()?.run.cases.get(self.selected_case)
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
                })
                .collect(),
        }
    }

    fn assert_selection_invariant(app: &WatchApp) {
        if app.problems.is_empty() {
            assert_eq!(app.selected_problem, 0);
            assert_eq!(app.selected_case, 0);
            return;
        }

        assert!(app.selected_problem < app.problems.len());
        let total_cases = app.current_case_count();
        if total_cases == 0 {
            assert_eq!(app.selected_case, 0);
        } else {
            assert!(app.selected_case < total_cases);
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

        app.source_changed(0, PathBuf::from("A.py"), Language::Python);

        let python = app.queue_run(0).unwrap();
        assert!(!python.debug);
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
        let expected: &Arc<String> = case.expected.as_ref().unwrap();
        let actual: &Arc<String> = case.actual.as_ref().unwrap();
        let stderr: &Arc<String> = case.stderr.as_ref().unwrap();
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
        assert!(detail_text(&app).contains("actual\n"));

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
                && case.expected.is_none()
                && case.actual.is_none()
                && case.stderr.is_none()
        }));
        assert_eq!(app.selected_case(), 1);
        assert_eq!(app.detail_scroll(), 0);
        assert!(app.detail_revision() > revision);
        let detail = detail_text(&app);
        assert!(detail.contains("Queued..."));
        assert!(!detail.contains("actual\n"));
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
    fn stress_attempt_error_transitions_to_error_detail() {
        for (source, language) in [
            (PathBuf::from("A.cpp"), Language::Cpp),
            (PathBuf::from("A.py"), Language::Python),
        ] {
            let mut app = WatchApp::new(&contest(1), vec![1]).unwrap();
            assert!(app.source_changed(0, source, language));
            let stress = app.queue_stress(0, 100).unwrap();
            assert!(app.run_started(0, stress.run_id));

            assert!(app.run_failed(
                0,
                stress.run_id,
                "reference program failed".to_string(),
            ));

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

        assert!(!app.run_event(
            0,
            sample.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        assert_eq!(app.problems[0].detail_mode, DetailMode::Stress);

        let next_sample = app.queue_run(0).unwrap();
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
        assert_eq!(app.problems[0].run.cases[1].verdict, CaseVerdict::WrongAnswer);
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
        assert!(detail.contains("input\n2\n1 2\n"));
        assert!(detail.contains("expected\nNo\n"));
        assert!(detail.contains("actual\nYes\n"));
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
                expected: "10\n".to_string(),
                actual: "11\n".to_string(),
            },
        ));

        assert_eq!(app.problems[0].sample_cases, 1);
        assert_eq!(app.problems[0].total_cases, 2);
        assert!(app.next_case());
        let detail = detail_text(&app);
        assert!(detail.contains("stress 1 / 1   WA"));
        assert!(detail.contains("input\n9\n"));
        assert!(detail.contains("expected\n10\n"));
        assert!(detail.contains("actual\n11\n"));
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
        assert!(detail.contains("input\nsaved input\n"));
        assert!(detail.contains("expected\nsaved expected\n"));
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
    fn selected_case_is_clamped_to_current_problem_sample_count_on_requeue() {
        let (mut app, run_id) = queued_cpp_app(3);
        assert!(app.run_started(0, run_id));
        assert!(app.run_event(0, run_id, TestEvent::TestRunStarted { total_cases: 3 },));
        app.selected_case = 2;
        app.problems[0].total_cases = 1;

        assert!(app.run_requeued(0, run_id));

        assert_eq!(app.selected_case(), 0);
        assert_eq!(app.problems[0].run.total_cases, 1);
        assert_eq!(app.problems[0].run.cases.len(), 1);
        assert_selection_invariant(&app);
    }
}
