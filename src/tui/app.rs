use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use super::message::{RunId, RunRequest, TestEvent};
use crate::language::Language;
use crate::model::Contest;

#[derive(Debug)]
pub struct SourceState {
    pub path: PathBuf,
    pub language: Language,
}

#[derive(Debug)]
pub struct ProblemState {
    pub index: String,
    pub title: String,
    pub total_cases: usize,
    pub source: Option<SourceState>,
    pub run: RunState,
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
    pub expected: Option<String>,
    pub actual: Option<String>,
    pub stderr: Option<String>,
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
    pub error: Option<String>,
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

        let problems = contest
            .problems
            .iter()
            .zip(sample_counts)
            .map(|(problem, total_cases)| ProblemState {
                index: problem.index.clone(),
                title: problem.title.clone(),
                total_cases,
                source: None,
                run: RunState::default(),
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
        self.reset_detail_scroll();
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
        if next == self.selected_case {
            return false;
        }
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
        if previous == self.selected_case {
            return false;
        }
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
        self.selected_problem = problem;
        self.selected_case = 0;
        self.reset_detail_scroll();
        self.invalidate_detail();

        true
    }
    pub fn queue_run(&mut self, problem: usize) -> Option<RunRequest> {
        let problem_state = self.problems.get(problem)?;
        let source = problem_state.source.as_ref()?;

        let language = source.language;
        let total_cases = problem_state.total_cases;

        let debug = self.debug && language == Language::Cpp;

        let run_id = self.next_run_id;
        self.next_run_id += 1;

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
        })
    }
    fn current_run_mut(&mut self, problem: usize, run_id: RunId) -> Option<&mut RunState> {
        let run = &mut self.problems.get_mut(problem)?.run;

        if run.id != Some(run_id) {
            return None;
        }

        Some(run)
    }
    pub fn run_started(&mut self, problem: usize, run_id: RunId) -> bool {
        let affects_current_detail = self.selected_problem == problem;
        let Some(run) = self.current_run_mut(problem, run_id) else {
            return false;
        };

        if run.phase != RunPhase::Queued {
            return false;
        }

        run.phase = match run.language {
            Some(Language::Cpp) => RunPhase::Compiling,
            _ => RunPhase::Running,
        };

        if affects_current_detail {
            self.invalidate_detail();
        }

        true
    }
    pub fn run_event(&mut self, problem: usize, run_id: RunId, event: TestEvent) -> bool {
        let affects_current_detail = self.selected_problem == problem
            && match &event {
                TestEvent::TestCaseAccepted { number, .. }
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
                run.error = Some(stderr);
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
                case.expected = None;
                case.actual = None;

                true
            }

            TestEvent::TestCaseWrongAnswer {
                number,
                expected,
                actual,
                elapsed,
            } if run.phase == RunPhase::Running && run.test_run_started => {
                let Some(case) = case_mut(run, number) else {
                    return false;
                };
                if case.verdict != CaseVerdict::Pending {
                    return false;
                }

                case.verdict = CaseVerdict::WrongAnswer;
                case.elapsed = Some(elapsed);
                case.expected = Some(expected);
                case.actual = Some(actual);

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
                case.expected = None;
                case.actual = None;

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
                case.expected = None;
                case.actual = None;

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

                case.stderr = Some(stderr);

                true
            }

            _ => false,
        };

        if changed && let Some(total_cases) = updated_total_cases {
            self.problems[problem].total_cases = total_cases;
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
    pub fn run_completed(&mut self, problem: usize, run_id: RunId) -> bool {
        let affects_current_detail = self.selected_problem == problem;
        let Some(run) = self.current_run_mut(problem, run_id) else {
            return false;
        };

        let changed = match run.phase {
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
        };

        if changed && affects_current_detail {
            self.invalidate_detail();
        }

        changed
    }
    pub fn run_failed(&mut self, problem: usize, run_id: RunId, error: String) -> bool {
        let affects_current_detail = self.selected_problem == problem;
        let Some(run) = self.current_run_mut(problem, run_id) else {
            return false;
        };

        if !matches!(
            run.phase,
            RunPhase::Queued | RunPhase::Compiling | RunPhase::Running
        ) {
            return false;
        }

        run.phase = RunPhase::Failed;
        run.error = Some(error);

        if affects_current_detail {
            self.invalidate_detail();
        }

        true
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

        assert_eq!(run.error.as_deref(), Some("compile error"));
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
        assert_eq!(run.error.as_deref(), Some("runner failed"));
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
                expected: "Yes\n".to_string(),
                actual: "No\n".to_string(),
                elapsed: Duration::from_millis(6),
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
        assert_eq!(case.expected.as_deref(), Some("Yes\n"));
        assert_eq!(case.actual.as_deref(), Some("No\n"));
        assert_eq!(case.stderr.as_deref(), Some("debug: answer = No\n"));
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
                expected: "expected".to_string(),
                actual: "actual".to_string(),
                elapsed: Duration::from_millis(5),
            },
        ));

        let case = &app.current_problem().unwrap().run.cases[0];
        assert_eq!(case.verdict, CaseVerdict::Accepted);
        assert_eq!(case.stderr.as_deref(), Some("debug first\n"));
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
                expected: "Yes\n".to_string(),
                actual: "No\n".to_string(),
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

        app.toggle_samples_pane();
        assert!(app.samples_pane_enabled());

        app.next_problem();
        assert!(app.samples_pane_enabled());

        app.toggle_samples_pane();
        assert!(!app.samples_pane_enabled());
    }
    #[test]
    fn selected_problem_returns_current_problem_index() {
        let mut app = WatchApp::new(&contest(2), vec![1, 1]).unwrap();

        assert_eq!(app.selected_problem(), Some(0));

        app.next_problem();

        assert_eq!(app.selected_problem(), Some(1));
    }
}
