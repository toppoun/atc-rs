use std::io;
use std::path::PathBuf;

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
}

#[derive(Debug)]
pub struct WatchApp {
    should_quit: bool,
    debug: bool,

    contest_id: String,
    problems: Vec<ProblemState>,

    selected_problem: usize,
    selected_case: usize,
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
            })
            .collect();

        Ok(Self {
            should_quit: false,
            debug: false,
            contest_id: contest.contest_id.clone(),
            problems,
            selected_problem: 0,
            selected_case: 0,
        })
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn debug_enabled(&self) -> bool {
        self.debug
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

    pub fn select_problem(&mut self, index: usize) -> bool {
        if index >= self.problems.len() {
            return false;
        }

        self.selected_problem = index;
        self.selected_case = 0;
        true
    }

    pub fn next_problem(&mut self) {
        if self.problems.is_empty() {
            self.selected_problem = 0;
            self.selected_case = 0;
            return;
        }

        let next = (self.selected_problem + 1) % self.problems.len();
        self.select_problem(next);
    }

    pub fn previous_problem(&mut self) {
        if self.problems.is_empty() {
            self.selected_problem = 0;
            self.selected_case = 0;
            return;
        }

        let previous = if self.selected_problem == 0 {
            self.problems.len() - 1
        } else {
            self.selected_problem - 1
        };
        self.select_problem(previous);
    }

    pub fn next_case(&mut self) {
        let count = self.current_case_count();

        if count == 0 {
            self.selected_case = 0;
            return;
        }

        self.selected_case = (self.selected_case + 1) % count;
    }

    pub fn previous_case(&mut self) {
        let count = self.current_case_count();

        if count == 0 {
            self.selected_case = 0;
            return;
        }

        if self.selected_case == 0 {
            self.selected_case = count - 1;
        } else {
            self.selected_case -= 1;
        }
    }
    pub fn source_changed(&mut self, problem: usize, path: PathBuf, language: Language) -> bool {
        if problem >= self.problems.len() {
            return false;
        }

        self.problems[problem].source = Some(SourceState { path, language });

        self.select_problem(problem);

        true
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
}
