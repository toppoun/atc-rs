use crate::model::Contest;

#[derive(Debug)]
pub struct ProblemState {
    pub index: String,
    pub title: String,
    pub total_cases: usize,
}

#[derive(Debug)]
pub struct WatchApp {
    pub should_quit: bool,
    pub debug: bool,

    pub contest_id: String,
    pub problems: Vec<ProblemState>,

    pub selected_problem: usize,
    pub selected_case: usize,
}

impl WatchApp {
    pub fn new(contest: &Contest, sample_counts: Vec<usize>) -> Self {
        let problems = contest
            .problems
            .iter()
            .zip(sample_counts)
            .map(|(problem, total_cases)| ProblemState {
                index: problem.index.clone(),
                title: problem.title.clone(),
                total_cases,
            })
            .collect();

        Self {
            should_quit: false,
            debug: false,
            contest_id: contest.contest_id.clone(),
            problems,
            selected_problem: 0,
            selected_case: 0,
        }
    }
    fn current_case_count(&self) -> usize {
        self.problems
            .get(self.selected_problem)
            .map(|problem| problem.total_cases)
            .unwrap_or(0)
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn toggle_debug(&mut self) {
        self.debug = !self.debug;
    }
    pub fn next_problem(&mut self) {
        self.selected_problem = (self.selected_problem + 1) % self.problems.len();
    }

    pub fn previous_problem(&mut self) {
        self.selected_problem =
            (self.selected_problem + self.problems.len() - 1) % self.problems.len();
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
}
