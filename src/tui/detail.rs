use std::sync::Arc;
use std::time::Duration;

use super::app::{
    CaseVerdict, DetailFoldState, DetailMode, ProblemState, RunPhase, StressPhase,
    StressSetupState, WatchApp,
};

// Byte position in the virtual concatenation of all detail segments. Segment
// boundaries contribute no bytes and are not logical-line boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RawOffset(pub(super) usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum DetailSectionKind {
    Input,
    Expected,
    Actual,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetailSectionAnchor {
    pub(super) kind: DetailSectionKind,
    pub(super) raw_position: RawOffset,
}

#[derive(Debug)]
pub(super) struct DetailSegment<'a> {
    text: DetailSegmentText<'a>,
}

#[derive(Debug)]
enum DetailSegmentText<'a> {
    Static(&'static str),
    Owned(String),
    Shared(&'a Arc<String>),
    #[cfg(test)]
    SharedOwned(Arc<String>),
}

impl DetailSegmentText<'_> {
    fn text(&self) -> &str {
        match self {
            Self::Static(text) => text,
            Self::Owned(text) => text,
            Self::Shared(text) => text.as_str(),
            #[cfg(test)]
            Self::SharedOwned(text) => text.as_str(),
        }
    }
}

impl DetailSegment<'_> {
    pub(super) fn text(&self) -> &str {
        self.text.text()
    }
}

pub(super) trait DetailTextSource {
    fn segment_count(&self) -> usize;
    fn segment_text(&self, index: usize) -> Option<&str>;
    fn section_anchors(&self) -> &[DetailSectionAnchor];
}

#[derive(Debug)]
#[allow(dead_code)]
enum DetailSnapshotSegment {
    Static(&'static str),
    Owned(String),
    Shared(Arc<String>),
}

impl DetailSnapshotSegment {
    #[allow(dead_code)]
    fn text(&self) -> &str {
        match self {
            Self::Static(text) => text,
            Self::Owned(text) => text.as_str(),
            Self::Shared(text) => text.as_str(),
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct DetailSnapshot {
    segments: Vec<DetailSnapshotSegment>,
    section_anchors: Vec<DetailSectionAnchor>,
}

impl DetailTextSource for DetailSnapshot {
    fn segment_count(&self) -> usize {
        self.segments.len()
    }

    fn segment_text(&self, index: usize) -> Option<&str> {
        self.segments.get(index).map(DetailSnapshotSegment::text)
    }

    fn section_anchors(&self) -> &[DetailSectionAnchor] {
        &self.section_anchors
    }
}

#[cfg(test)]
impl DetailSnapshot {
    pub(super) fn shares_buffer(&self, expected: &Arc<String>) -> bool {
        self.segments.iter().any(|segment| {
            matches!(segment, DetailSnapshotSegment::Shared(shared) if Arc::ptr_eq(shared, expected))
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct DetailDocument<'a> {
    segments: Vec<DetailSegment<'a>>,
    section_anchors: Vec<DetailSectionAnchor>,
    raw_len: usize,
}

impl<'a> DetailDocument<'a> {
    pub(super) fn from_app(app: &'a WatchApp) -> Self {
        let mut document = Self::default();

        let Some(problem) = app.current_problem() else {
            document.push_static("No problems");
            return document;
        };

        document.push_owned(format!("{} - {}\n\n", problem.index, problem.title));
        document.push_problem_detail(app, problem);
        document
    }

    #[cfg(test)]
    pub(super) fn segments(&self) -> impl Iterator<Item = &DetailSegment<'a>> {
        self.segments.iter()
    }

    #[allow(dead_code)]
    pub(super) fn snapshot(&self) -> DetailSnapshot {
        DetailSnapshot {
            segments: self
                .segments
                .iter()
                .map(|segment| match &segment.text {
                    DetailSegmentText::Static(text) => DetailSnapshotSegment::Static(text),
                    DetailSegmentText::Owned(text) => DetailSnapshotSegment::Owned(text.clone()),
                    DetailSegmentText::Shared(text) => {
                        DetailSnapshotSegment::Shared(Arc::clone(text))
                    }
                    #[cfg(test)]
                    DetailSegmentText::SharedOwned(text) => {
                        DetailSnapshotSegment::Shared(Arc::clone(text))
                    }
                })
                .collect(),
            section_anchors: self.section_anchors.clone(),
        }
    }

    fn push_problem_detail(&mut self, app: &'a WatchApp, problem: &'a ProblemState) {
        match problem.detail_mode {
            DetailMode::Samples => self.push_sample_run_detail(app, problem),
            DetailMode::Stress => self.push_stress_detail(app, problem),
        }
    }

    fn push_sample_run_detail(&mut self, app: &'a WatchApp, problem: &'a ProblemState) {
        let run = &problem.run;

        if run.id.is_none()
            && problem.saved_stress_case.is_some()
            && app.selected_case() >= problem.sample_cases
        {
            self.push_saved_stress_case_detail(app, problem);
            return;
        }

        match run.phase {
            RunPhase::Idle => {
                self.push_static("Waiting for a source change...");
            }

            RunPhase::Queued => {
                self.push_static("Queued...");
            }

            RunPhase::Compiling => {
                self.push_owned(format!("Compiling {}...", problem.index));
            }

            RunPhase::CompileError => {
                self.push_static("Compile Error");

                if let Some(error) = run.error.as_ref() {
                    self.push_shared_section("compiler output", error);
                }
            }

            RunPhase::CompileTimedOut => {
                self.push_static("Compile Timed Out");
            }

            RunPhase::NoSamples => {
                self.push_static("No samples");
            }

            RunPhase::Failed => {
                self.push_static("Run Failed");

                if let Some(error) = run.error.as_ref() {
                    self.push_shared_section("error", error);
                }
            }

            RunPhase::Running | RunPhase::Finished | RunPhase::Cancelled => {
                self.push_sample_detail(app, problem);
            }
        }
    }

    fn push_stress_detail(&mut self, app: &'a WatchApp, problem: &'a ProblemState) {
        let stress = &problem.stress;

        match stress.phase {
            StressPhase::Idle => {
                if !self.push_stress_setup_detail(problem) {
                    self.push_static("Stress has not been started.");
                }
                return;
            }
            StressPhase::Queued => {
                self.push_static("STRESS QUEUED");
                if let Some(seed) = stress.base_seed {
                    self.push_owned(format!("\n\nseed       {seed}"));
                }
            }
            StressPhase::Compiling => {
                self.push_static("STRESS COMPILING");
                if let Some(seed) = stress.base_seed {
                    self.push_owned(format!("\n\nseed       {seed}"));
                }
            }
            StressPhase::Running => {
                self.push_static("STRESS RUNNING");
                self.push_owned(format!("\n\ncases      {}", stress.passed));
                if stress.case_number > 0 {
                    self.push_owned(format!("\ncase       {}", stress.case_number));
                }
                if let Some(seed) = stress.seed.or(stress.base_seed) {
                    self.push_owned(format!("\nseed       {seed}"));
                }
                self.push_owned(format!(
                    "\nelapsed    {}",
                    stress_elapsed_label(stress.elapsed)
                ));
                self.push_owned(format!(
                    "\nrate       {:.1} cases/s",
                    stress.cases_per_second
                ));
            }
            StressPhase::Failed => {
                let Some(failure) = stress.failure.as_ref() else {
                    self.push_static("STRESS FAILED");
                    return;
                };

                self.push_owned(format!(
                    "STRESS {}   case {}   seed {}",
                    failure.kind.as_str(),
                    failure.case_number,
                    failure.seed,
                ));
                self.push_owned(format!(
                    "\n\nelapsed    {}\ncandidate  {}",
                    stress_elapsed_label(stress.elapsed),
                    stress_elapsed_label(failure.candidate_elapsed),
                ));
                self.push_semantic_shared_section(
                    DetailSectionKind::Input,
                    "Input",
                    &failure.input,
                    app.detail_fold_state(),
                );
                self.push_semantic_shared_section(
                    DetailSectionKind::Expected,
                    "Expected",
                    &failure.expected,
                    app.detail_fold_state(),
                );
                self.push_semantic_shared_section(
                    DetailSectionKind::Actual,
                    "Actual",
                    &failure.actual,
                    app.detail_fold_state(),
                );
                if !failure.stderr.is_empty() {
                    self.push_semantic_shared_section(
                        DetailSectionKind::Stderr,
                        "Stderr",
                        &failure.stderr,
                        app.detail_fold_state(),
                    );
                }
                self.push_owned(format!("\n\nsaved\n{}", failure.saved_to.display()));
            }
            StressPhase::Finished => {
                self.push_owned(format!(
                    "STRESS FINISHED\n\n{} cases passed\nelapsed    {}",
                    stress.passed,
                    stress_elapsed_label(stress.elapsed),
                ));
            }
            StressPhase::Cancelled => {
                self.push_owned(format!(
                    "STRESS CANCELLED\n\n{} cases passed\nelapsed    {}",
                    stress.passed,
                    stress_elapsed_label(stress.elapsed),
                ));
            }
            StressPhase::Error => {
                self.push_static("STRESS ERROR");
                if let Some(error) = stress.error.as_ref() {
                    self.push_shared_section("error", error);
                }
            }
        }

        if matches!(
            &problem.stress_setup,
            StressSetupState::Required { .. } | StressSetupState::Error { .. }
        ) {
            self.push_static("\n\n");
            self.push_stress_setup_detail(problem);
        }
    }

    fn push_stress_setup_detail(&mut self, problem: &'a ProblemState) -> bool {
        match &problem.stress_setup {
            StressSetupState::Required {
                generator_missing,
                brute_missing,
            } => {
                let mut text = String::from("STRESS SETUP REQUIRED\n\nMissing:");
                if *generator_missing {
                    text.push_str(&format!("\n  {}_gen.py", problem.index));
                }
                if *brute_missing {
                    text.push_str(&format!("\n  {}_brute.py", problem.index));
                }
                text.push_str("\n\nPress i to initialize.");
                self.push_owned(text);
                true
            }
            StressSetupState::Initialized => {
                self.push_owned(format!(
                    "STRESS FILES INITIALIZED\n\n{}_gen.py\n{}_brute.py\n\nEdit the files, then press S to run stress.",
                    problem.index, problem.index,
                ));
                true
            }
            StressSetupState::Error { message } => {
                self.push_owned(format!("STRESS SETUP ERROR\n\n{message}"));
                true
            }
            StressSetupState::None => false,
        }
    }

    fn push_sample_detail(&mut self, app: &'a WatchApp, problem: &'a ProblemState) {
        let run = &problem.run;
        let total = problem.total_cases;

        if total == 0 {
            self.push_static("Running samples...");
            return;
        }

        if app.selected_case() >= problem.sample_cases {
            self.push_saved_stress_case_detail(app, problem);
            return;
        }

        let Some(case) = app.selected_case_state() else {
            self.push_owned(format!(
                "sample {} / {}\n\nPending...",
                app.selected_case() + 1,
                problem.sample_cases,
            ));
            return;
        };

        self.push_owned(format!(
            "sample {} / {}   {}{}",
            app.selected_case() + 1,
            problem.sample_cases,
            verdict_label(case.verdict),
            elapsed_label(case.elapsed),
        ));

        match case.verdict {
            CaseVerdict::Pending => {
                self.push_static("\n\nPending...");
                return;
            }

            CaseVerdict::Accepted if !run.debug => {
                self.push_static("\n\nAccepted");
            }

            CaseVerdict::Accepted => {}

            CaseVerdict::WrongAnswer => {}

            CaseVerdict::RuntimeError => {
                self.push_static("\n\nRuntime Error");
            }

            CaseVerdict::TimedOut => {
                self.push_static("\n\nTime Limit Exceeded");
            }
        }

        if run.debug {
            self.push_optional_semantic_shared_section(
                DetailSectionKind::Input,
                "Input",
                case.input.as_ref(),
                app.detail_fold_state(),
            );
        }

        self.push_optional_semantic_shared_section(
            DetailSectionKind::Expected,
            "Expected",
            case.expected.as_ref(),
            app.detail_fold_state(),
        );

        self.push_optional_semantic_shared_section(
            DetailSectionKind::Actual,
            "Actual",
            case.actual.as_ref(),
            app.detail_fold_state(),
        );

        if let Some(stderr) = case.stderr.as_ref() {
            self.push_semantic_shared_section(
                DetailSectionKind::Stderr,
                "Stderr",
                stderr,
                app.detail_fold_state(),
            );
        }
    }

    fn push_saved_stress_case_detail(&mut self, app: &'a WatchApp, problem: &'a ProblemState) {
        let Some(saved) = problem.saved_stress_case.as_ref() else {
            self.push_static("stress 1 / 1\n\nPending...");
            return;
        };

        let case = app.selected_case_state();
        let verdict = case
            .map(|case| case.verdict)
            .unwrap_or(CaseVerdict::Pending);
        let elapsed = case.and_then(|case| case.elapsed);

        self.push_owned(format!(
            "stress 1 / 1   {}{}",
            verdict_label(verdict),
            elapsed_label(elapsed),
        ));
        self.push_semantic_shared_section(
            DetailSectionKind::Input,
            "Input",
            &saved.input,
            app.detail_fold_state(),
        );
        self.push_semantic_shared_section(
            DetailSectionKind::Expected,
            "Expected",
            &saved.expected,
            app.detail_fold_state(),
        );

        match verdict {
            CaseVerdict::Pending => self.push_static("\n\nPending..."),
            CaseVerdict::Accepted | CaseVerdict::WrongAnswer => {}
            CaseVerdict::RuntimeError => self.push_static("\n\nRuntime Error"),
            CaseVerdict::TimedOut => self.push_static("\n\nTime Limit Exceeded"),
        }

        if let Some(case) = case {
            self.push_optional_semantic_shared_section(
                DetailSectionKind::Actual,
                "Actual",
                case.actual.as_ref(),
                app.detail_fold_state(),
            );
            if let Some(stderr) = case.stderr.as_ref() {
                self.push_semantic_shared_section(
                    DetailSectionKind::Stderr,
                    "Stderr",
                    stderr,
                    app.detail_fold_state(),
                );
            }
        }
    }

    fn push_optional_semantic_shared_section(
        &mut self,
        kind: DetailSectionKind,
        label: &'static str,
        content: Option<&'a Arc<String>>,
        folds: DetailFoldState,
    ) {
        if let Some(content) = content {
            self.push_semantic_shared_section(kind, label, content, folds);
        }
    }

    fn push_semantic_shared_section(
        &mut self,
        kind: DetailSectionKind,
        label: &'static str,
        content: &'a Arc<String>,
        folds: DetailFoldState,
    ) {
        self.push_semantic_section_gap();
        debug_assert!(
            !self
                .section_anchors
                .iter()
                .any(|anchor| anchor.kind == kind),
            "semantic Detail sections must be unique"
        );
        self.section_anchors.push(DetailSectionAnchor {
            kind,
            raw_position: RawOffset(self.raw_len),
        });
        if folds.is_collapsed(kind) {
            self.push_static("▶ ");
        } else {
            self.push_static("▼ ");
        }
        self.push_static(label);
        if folds.is_collapsed(kind) {
            return;
        }

        self.push_static("\n");
        if content.is_empty() {
            self.push_static("(empty)");
        } else {
            self.push_shared(content);
        }
    }

    fn push_semantic_section_gap(&mut self) {
        // A trailing line feed already terminates the preceding body line, so only one more
        // is needed to render the single blank row between semantic sections. The shared body
        // segment itself remains untouched.
        if self
            .segments
            .last()
            .is_some_and(|segment| segment.text().ends_with('\n'))
        {
            self.push_static("\n");
        } else {
            self.push_static("\n\n");
        }
    }

    fn push_shared_section(&mut self, label: &'static str, content: &'a Arc<String>) {
        self.push_static("\n\n");
        self.push_static(label);
        self.push_static("\n");

        if content.is_empty() {
            self.push_static("(empty)");
        } else {
            self.push_shared(content);
        }
    }

    fn push_static(&mut self, text: &'static str) {
        self.raw_len = self
            .raw_len
            .checked_add(text.len())
            .expect("detail document byte length must fit in usize");
        self.segments.push(DetailSegment {
            text: DetailSegmentText::Static(text),
        });
    }

    fn push_shared(&mut self, text: &'a Arc<String>) {
        self.raw_len = self
            .raw_len
            .checked_add(text.len())
            .expect("detail document byte length must fit in usize");
        self.segments.push(DetailSegment {
            text: DetailSegmentText::Shared(text),
        });
    }

    fn push_owned(&mut self, text: String) {
        self.raw_len = self
            .raw_len
            .checked_add(text.len())
            .expect("detail document byte length must fit in usize");
        self.segments.push(DetailSegment {
            text: DetailSegmentText::Owned(text),
        });
    }

    #[cfg(test)]
    pub(super) fn from_borrowed_segments(segments: &'a [&'a str]) -> Self {
        let raw_len = segments
            .iter()
            .try_fold(0usize, |total, text| total.checked_add(text.len()))
            .expect("detail document byte length must fit in usize");
        Self {
            segments: segments
                .iter()
                .map(|text| DetailSegment {
                    text: DetailSegmentText::SharedOwned(Arc::new((*text).to_string())),
                })
                .collect(),
            section_anchors: Vec::new(),
            raw_len,
        }
    }

    #[cfg(test)]
    pub(super) fn from_borrowed_segments_with_anchors(
        segments: &'a [&'a str],
        section_anchors: &[DetailSectionAnchor],
    ) -> Self {
        let mut document = Self::from_borrowed_segments(segments);
        debug_assert!(
            section_anchors
                .windows(2)
                .all(|anchors| anchors[0].raw_position <= anchors[1].raw_position)
        );
        debug_assert!(
            section_anchors
                .iter()
                .all(|anchor| anchor.raw_position.0 <= document.raw_len)
        );
        document.section_anchors = section_anchors.to_vec();
        document
    }

    #[cfg(test)]
    pub(super) fn from_shared_segments(segments: &'a [&'a Arc<String>]) -> Self {
        let raw_len = segments
            .iter()
            .try_fold(0usize, |total, text| total.checked_add(text.len()))
            .expect("detail document byte length must fit in usize");
        Self {
            segments: segments
                .iter()
                .map(|text| DetailSegment {
                    text: DetailSegmentText::Shared(text),
                })
                .collect(),
            section_anchors: Vec::new(),
            raw_len,
        }
    }
}

impl DetailTextSource for DetailDocument<'_> {
    fn segment_count(&self) -> usize {
        self.segments.len()
    }

    fn segment_text(&self, index: usize) -> Option<&str> {
        self.segments.get(index).map(DetailSegment::text)
    }

    fn section_anchors(&self) -> &[DetailSectionAnchor] {
        &self.section_anchors
    }
}

fn verdict_label(verdict: CaseVerdict) -> &'static str {
    match verdict {
        CaseVerdict::Pending => "Pending",
        CaseVerdict::Accepted => "AC",
        CaseVerdict::WrongAnswer => "WA",
        CaseVerdict::RuntimeError => "RE",
        CaseVerdict::TimedOut => "TLE",
    }
}

fn stress_elapsed_label(elapsed: Duration) -> String {
    if elapsed.as_secs() >= 1 {
        format!("{:.2}s", elapsed.as_secs_f64())
    } else {
        format!("{:.1} ms", elapsed.as_secs_f64() * 1000.0)
    }
}

fn elapsed_label(elapsed: Option<Duration>) -> String {
    let Some(elapsed) = elapsed else {
        return String::new();
    };

    format!("   {:.1} ms", elapsed.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;
    use crate::model::{Contest, Problem};
    use crate::stress::CandidateFailureKind;
    use crate::tui::message::{StressEvent, TestEvent};
    use std::path::PathBuf;

    fn contest() -> Contest {
        Contest {
            contest_id: "abc123".to_string(),
            problems: vec![Problem {
                index: "A".to_string(),
                title: "Problem A".to_string(),
                task_id: "abc123_a".to_string(),
                url: "https://example.invalid/a".to_string(),
                sample_count: 0,
            }],
        }
    }

    fn running_app() -> (WatchApp, u64) {
        let mut app = WatchApp::new(&contest(), vec![1]).unwrap();
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        (app, request.run_id)
    }

    fn running_cpp_app(debug: bool) -> (WatchApp, u64) {
        let mut app = WatchApp::new(&contest(), vec![1]).unwrap();
        if debug {
            app.toggle_debug();
        }
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let request = app.queue_run(0).unwrap();
        assert_eq!(request.debug, debug);
        assert!(app.run_started(0, request.run_id));
        assert!(app.run_event(
            0,
            request.run_id,
            TestEvent::TestRunStarted { total_cases: 1 },
        ));
        (app, request.run_id)
    }

    fn compiling_app() -> (WatchApp, u64) {
        let mut app = WatchApp::new(&contest(), vec![1]).unwrap();
        app.source_changed(0, PathBuf::from("A.cpp"), Language::Cpp);
        let request = app.queue_run(0).unwrap();
        assert!(app.run_started(0, request.run_id));
        (app, request.run_id)
    }

    fn source_text(source: &impl DetailTextSource) -> String {
        (0..source.segment_count())
            .map(|index| source.segment_text(index).unwrap())
            .collect()
    }

    fn document_text(document: &DetailDocument<'_>) -> String {
        source_text(document)
    }

    fn assert_snapshot_matches(document: &DetailDocument<'_>) {
        let snapshot = document.snapshot();
        assert_eq!(snapshot.segment_count(), document.segment_count());
        assert_eq!(source_text(&snapshot), source_text(document));

        for index in 0..document.segment_count() {
            assert_eq!(snapshot.segment_text(index), document.segment_text(index));
        }
        assert_eq!(snapshot.section_anchors(), document.section_anchors());
    }

    fn expected_anchor(text: &str, label: &str) -> RawOffset {
        let header = format!("▼ {label}");
        RawOffset(
            text.find(&header)
                .unwrap_or_else(|| panic!("missing expected section header {label}")),
        )
    }

    fn assert_snapshot_shares(document: &DetailDocument<'_>, state: &Arc<String>) {
        let owners = Arc::strong_count(state);
        let snapshot = document.snapshot();
        let shared = snapshot
            .segments
            .iter()
            .find_map(|segment| match segment {
                DetailSnapshotSegment::Shared(shared) if Arc::ptr_eq(shared, state) => Some(shared),
                _ => None,
            })
            .expect("snapshot must share the raw state Arc");

        assert_eq!(shared.as_ptr(), state.as_ptr());
        assert_eq!(Arc::strong_count(state), owners + 1);
    }

    #[test]
    fn stress_setup_required_document_lists_only_missing_canonical_files() {
        for (generator_missing, brute_missing, expected_missing) in [
            (true, true, "  A_gen.py\n  A_brute.py"),
            (true, false, "  A_gen.py"),
            (false, true, "  A_brute.py"),
        ] {
            let mut app = WatchApp::new(&contest(), vec![1]).unwrap();
            assert!(app.set_stress_setup_required(0, generator_missing, brute_missing));

            assert_eq!(
                document_text(&DetailDocument::from_app(&app)),
                format!(
                    "A - Problem A\n\nSTRESS SETUP REQUIRED\n\nMissing:\n{expected_missing}\n\nPress i to initialize."
                )
            );
        }
    }

    #[test]
    fn initialized_and_setup_error_documents_are_distinct_from_real_stress_errors() {
        let mut app = WatchApp::new(&contest(), vec![1]).unwrap();
        assert!(app.set_stress_setup_initialized(0));
        assert_eq!(
            document_text(&DetailDocument::from_app(&app)),
            concat!(
                "A - Problem A\n\n",
                "STRESS FILES INITIALIZED\n\n",
                "A_gen.py\n",
                "A_brute.py\n\n",
                "Edit the files, then press S to run stress."
            )
        );

        assert!(app.set_stress_setup_error(0, "A_gen.py is a directory".to_string()));
        assert_eq!(
            document_text(&DetailDocument::from_app(&app)),
            "A - Problem A\n\nSTRESS SETUP ERROR\n\nA_gen.py is a directory"
        );
        assert!(!document_text(&DetailDocument::from_app(&app)).contains("STRESS ERROR"));
    }

    #[test]
    fn real_stress_queue_and_error_precede_later_setup_presentation() {
        let mut app = WatchApp::new(&contest(), vec![1]).unwrap();
        assert!(app.source_changed(0, PathBuf::from("A.py"), Language::Python));
        let request = app.queue_stress(0, 123).unwrap();
        assert!(app.set_stress_setup_initialized(0));

        let queued = document_text(&DetailDocument::from_app(&app));
        assert!(queued.contains("STRESS QUEUED"));
        assert!(!queued.contains("STRESS FILES INITIALIZED"));

        assert!(app.run_started(0, request.run_id));
        assert!(app.run_failed(0, request.run_id, "candidate source failed".to_string(),));
        assert!(app.set_stress_setup_error(0, "invalid helper target".to_string()));

        let failed = document_text(&DetailDocument::from_app(&app));
        assert!(failed.contains("STRESS ERROR"));
        assert!(failed.contains("candidate source failed"));
        let real_error = failed.find("STRESS ERROR").unwrap();
        let setup_error = failed.find("STRESS SETUP ERROR").unwrap();
        assert!(real_error < setup_error);
        assert!(failed.contains("invalid helper target"));
    }

    #[test]
    fn wrong_answer_document_preserves_order_whitespace_unicode_and_borrows_raw_outputs() {
        let (mut app, run_id) = running_app();
        let expected = "  58\n\n日本語 e\u{301} 👩‍💻\n".to_string();
        let actual = "58 \n".to_string();
        let stderr = " debug \n\n".to_string();

        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseComparison {
                number: 1,
                input: "sample input\n".to_string(),
                expected,
                actual,
            },
        ));
        assert!(app.run_event(0, run_id, TestEvent::TestCaseStderr { number: 1, stderr },));

        let case = &app.current_problem().unwrap().run.cases[0];
        let expected = case.expected.as_deref().unwrap();
        let actual = case.actual.as_deref().unwrap();
        let stderr = case.stderr.as_deref().unwrap();
        let expected_ptr = expected.as_ptr();
        let actual_ptr = actual.as_ptr();
        let stderr_ptr = stderr.as_ptr();
        let document = DetailDocument::from_app(&app);

        assert_eq!(
            document_text(&document),
            concat!(
                "A - Problem A\n\n",
                "sample 1 / 1   WA   1.0 ms",
                "\n\n▼ Expected\n  58\n\n日本語 e\u{301} 👩‍💻\n",
                "\n▼ Actual\n58 \n",
                "\n▼ Stderr\n debug \n\n",
            )
        );
        assert_snapshot_matches(&document);

        let text = document_text(&document);
        assert_eq!(
            document.section_anchors(),
            [
                DetailSectionAnchor {
                    kind: DetailSectionKind::Expected,
                    raw_position: expected_anchor(&text, "Expected\n"),
                },
                DetailSectionAnchor {
                    kind: DetailSectionKind::Actual,
                    raw_position: expected_anchor(&text, "Actual\n"),
                },
                DetailSectionAnchor {
                    kind: DetailSectionKind::Stderr,
                    raw_position: expected_anchor(&text, "Stderr\n"),
                },
            ]
        );

        let mut segment_start = 0usize;
        for segment in document.segments() {
            if segment.text() == "▼ " {
                assert!(
                    document
                        .section_anchors()
                        .iter()
                        .any(|anchor| anchor.raw_position == RawOffset(segment_start))
                );
            }
            segment_start = segment_start.checked_add(segment.text().len()).unwrap();
        }

        for (raw, pointer) in [
            (expected, expected_ptr),
            (actual, actual_ptr),
            (stderr, stderr_ptr),
        ] {
            let segment = document
                .segments()
                .find(|segment| segment.text().as_ptr() == pointer)
                .expect("raw output must remain a document segment");
            assert_eq!(segment.text(), raw);
            assert!(matches!(segment.text, DetailSegmentText::Shared(_)));
        }
    }

    #[test]
    fn runtime_error_and_timed_out_documents_preserve_current_detail_text() {
        let (mut runtime_app, run_id) = running_app();
        assert!(runtime_app.run_event(
            0,
            run_id,
            TestEvent::TestCaseRuntimeError {
                number: 1,
                elapsed: Duration::from_millis(2),
            },
        ));
        assert!(runtime_app.run_event(
            0,
            run_id,
            TestEvent::TestCaseStderr {
                number: 1,
                stderr: "runtime stderr\n".to_string(),
            },
        ));
        assert_eq!(
            document_text(&DetailDocument::from_app(&runtime_app)),
            concat!(
                "A - Problem A\n\n",
                "sample 1 / 1   RE   2.0 ms",
                "\n\nRuntime Error",
                "\n\n▼ Stderr\nruntime stderr\n",
            )
        );
        assert_snapshot_matches(&DetailDocument::from_app(&runtime_app));

        let (mut timed_out_app, run_id) = running_app();
        assert!(timed_out_app.run_event(
            0,
            run_id,
            TestEvent::TestCaseTimedOut {
                number: 1,
                elapsed: Duration::from_millis(3),
            },
        ));
        assert_eq!(
            document_text(&DetailDocument::from_app(&timed_out_app)),
            concat!(
                "A - Problem A\n\n",
                "sample 1 / 1   TLE   3.0 ms",
                "\n\nTime Limit Exceeded",
            )
        );
        assert_snapshot_matches(&DetailDocument::from_app(&timed_out_app));
    }

    #[test]
    fn compile_error_and_debug_off_accepted_stderr_preserve_current_sections() {
        let (mut compile_app, run_id) = compiling_app();
        assert!(compile_app.run_event(
            0,
            run_id,
            TestEvent::CompileFailed {
                stderr: "compiler output\n\n".to_string(),
            },
        ));
        assert_eq!(
            document_text(&DetailDocument::from_app(&compile_app)),
            concat!(
                "A - Problem A\n\n",
                "Compile Error",
                "\n\ncompiler output\ncompiler output\n\n",
            )
        );
        assert_snapshot_matches(&DetailDocument::from_app(&compile_app));
        assert!(
            DetailDocument::from_app(&compile_app)
                .section_anchors()
                .is_empty(),
            "compiler output is not a semantic Detail section"
        );

        let compile_error = compile_app
            .current_problem()
            .unwrap()
            .run
            .error
            .as_deref()
            .unwrap();
        let compile_document = DetailDocument::from_app(&compile_app);
        let compile_segment = compile_document
            .segments()
            .find(|segment| segment.text().as_ptr() == compile_error.as_ptr())
            .expect("compiler output must remain borrowed");
        assert!(matches!(compile_segment.text, DetailSegmentText::Shared(_)));
        assert_snapshot_shares(
            &compile_document,
            compile_app
                .current_problem()
                .unwrap()
                .run
                .error
                .as_ref()
                .unwrap(),
        );

        let (mut accepted_app, run_id) = running_cpp_app(false);
        assert!(accepted_app.run_event(
            0,
            run_id,
            TestEvent::TestCaseAccepted {
                number: 1,
                elapsed: Duration::from_millis(4),
            },
        ));
        assert!(accepted_app.run_event(
            0,
            run_id,
            TestEvent::TestCaseStderr {
                number: 1,
                stderr: "accepted stderr".to_string(),
            },
        ));
        assert_eq!(
            document_text(&DetailDocument::from_app(&accepted_app)),
            concat!(
                "A - Problem A\n\n",
                "sample 1 / 1   AC   4.0 ms",
                "\n\nAccepted",
                "\n\n▼ Stderr\naccepted stderr",
            )
        );
        assert_snapshot_matches(&DetailDocument::from_app(&accepted_app));
    }

    #[test]
    fn debug_accepted_detail_starts_with_input_and_omits_redundant_status() {
        let (mut app, run_id) = running_cpp_app(true);
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseAccepted {
                number: 1,
                elapsed: Duration::from_millis(4),
            },
        ));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseComparison {
                number: 1,
                input: "sample input\n".to_string(),
                expected: "expected\n".to_string(),
                actual: "actual\n".to_string(),
            },
        ));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseStderr {
                number: 1,
                stderr: "debug output\n".to_string(),
            },
        ));

        let document = DetailDocument::from_app(&app);
        let text = document_text(&document);
        assert_eq!(
            text,
            concat!(
                "A - Problem A\n\n",
                "sample 1 / 1   AC   4.0 ms",
                "\n\n▼ Input\nsample input\n",
                "\n▼ Expected\nexpected\n",
                "\n▼ Actual\nactual\n",
                "\n▼ Stderr\ndebug output\n",
            )
        );
        assert!(!text.contains("\n\nAccepted"));
        assert_eq!(
            document
                .section_anchors()
                .iter()
                .map(|anchor| anchor.kind)
                .collect::<Vec<_>>(),
            [
                DetailSectionKind::Input,
                DetailSectionKind::Expected,
                DetailSectionKind::Actual,
                DetailSectionKind::Stderr,
            ]
        );
    }

    #[test]
    fn fatal_run_error_remains_a_borrowed_error_section() {
        let (mut app, run_id) = running_app();
        assert!(app.run_failed(0, run_id, "fatal runner error\n".to_string()));
        let error = app.current_problem().unwrap().run.error.as_deref().unwrap();
        let document = DetailDocument::from_app(&app);

        assert_eq!(
            document_text(&document),
            "A - Problem A\n\nRun Failed\n\nerror\nfatal runner error\n"
        );
        assert_snapshot_matches(&document);
        let error_segment = document
            .segments()
            .find(|segment| segment.text().as_ptr() == error.as_ptr())
            .expect("run error must remain borrowed");
        assert!(matches!(error_segment.text, DetailSegmentText::Shared(_)));
        assert_snapshot_shares(
            &document,
            app.current_problem().unwrap().run.error.as_ref().unwrap(),
        );
    }

    #[test]
    fn empty_wrong_answer_outputs_keep_the_empty_placeholder() {
        let (mut app, run_id) = running_app();
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::ZERO,
            },
        ));

        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseComparison {
                number: 1,
                input: String::new(),
                expected: String::new(),
                actual: String::new(),
            },
        ));

        assert_eq!(
            document_text(&DetailDocument::from_app(&app)),
            concat!(
                "A - Problem A\n\n",
                "sample 1 / 1   WA   0.0 ms",
                "\n\n▼ Expected\n(empty)",
                "\n\n▼ Actual\n(empty)",
            )
        );
        assert_snapshot_matches(&DetailDocument::from_app(&app));
        let document = DetailDocument::from_app(&app);
        let text = document_text(&document);
        assert_eq!(
            document.section_anchors(),
            [
                DetailSectionAnchor {
                    kind: DetailSectionKind::Expected,
                    raw_position: expected_anchor(&text, "Expected\n"),
                },
                DetailSectionAnchor {
                    kind: DetailSectionKind::Actual,
                    raw_position: expected_anchor(&text, "Actual\n"),
                },
            ]
        );
    }

    #[test]
    fn semantic_section_gaps_preserve_internal_lines_and_add_no_trailing_separator() {
        let expected = Arc::new("a\n\nb\n".to_string());
        let actual = Arc::new("No\n".to_string());
        let stderr = Arc::new("diagnostic".to_string());
        let mut document = DetailDocument::default();
        document.push_static("header");
        for (kind, label, body) in [
            (DetailSectionKind::Expected, "Expected", &expected),
            (DetailSectionKind::Actual, "Actual", &actual),
            (DetailSectionKind::Stderr, "Stderr", &stderr),
        ] {
            document.push_semantic_shared_section(kind, label, body, DetailFoldState::default());
        }

        assert_eq!(
            document_text(&document),
            concat!(
                "header",
                "\n\n▼ Expected\na\n\nb\n",
                "\n▼ Actual\nNo\n",
                "\n▼ Stderr\ndiagnostic",
            )
        );
        assert_eq!(expected.as_str(), "a\n\nb\n");
        assert_eq!(actual.as_str(), "No\n");
        assert_eq!(stderr.as_str(), "diagnostic");
        assert_snapshot_matches(&document);
    }

    #[test]
    fn folded_semantic_sections_keep_one_blank_line_between_headers() {
        let mut app = WatchApp::new(&contest(), vec![1]).unwrap();
        app.toggle_detail_section(DetailSectionKind::Expected);
        app.toggle_detail_section(DetailSectionKind::Stderr);
        let expected = Arc::new("hidden expected\n".to_string());
        let actual = Arc::new("No\n".to_string());
        let stderr = Arc::new("hidden stderr\n".to_string());
        let mut document = DetailDocument::default();
        document.push_static("header");
        for (kind, label, body) in [
            (DetailSectionKind::Expected, "Expected", &expected),
            (DetailSectionKind::Actual, "Actual", &actual),
            (DetailSectionKind::Stderr, "Stderr", &stderr),
        ] {
            document.push_semantic_shared_section(kind, label, body, app.detail_fold_state());
        }

        assert_eq!(
            document_text(&document),
            concat!(
                "header",
                "\n\n▶ Expected",
                "\n\n▼ Actual\nNo\n",
                "\n▶ Stderr",
            )
        );
        assert_snapshot_matches(&document);
    }

    #[test]
    fn live_stress_failure_records_all_semantic_sections_in_render_order() {
        let mut app = WatchApp::new(&contest(), vec![1]).unwrap();
        app.source_changed(0, PathBuf::from("A.py"), Language::Python);
        let request = app.queue_stress(0, 123).unwrap();
        assert!(app.run_started(0, request.run_id));
        assert!(app.stress_event(
            0,
            request.run_id,
            StressEvent::Started {
                base_seed: 123,
                case_limit: None,
            },
        ));
        assert!(app.stress_event(
            0,
            request.run_id,
            StressEvent::Failed {
                kind: CandidateFailureKind::WrongAnswer,
                case_number: 1,
                base_seed: 123,
                seed: 456,
                input: "stress input\n".to_string(),
                expected: "No\n".to_string(),
                actual: "No\n".to_string(),
                stderr: "diagnostic".to_string(),
                candidate_elapsed: Duration::from_millis(1),
                elapsed: Duration::from_millis(2),
                saved_to: PathBuf::from(".atc/stress/A"),
            },
        ));

        let document = DetailDocument::from_app(&app);
        let text = document_text(&document);
        let expected = [
            (DetailSectionKind::Input, "Input\n"),
            (DetailSectionKind::Expected, "Expected\n"),
            (DetailSectionKind::Actual, "Actual\n"),
            (DetailSectionKind::Stderr, "Stderr\n"),
        ];
        assert_eq!(document.section_anchors().len(), expected.len());
        for (anchor, (kind, label)) in document.section_anchors().iter().zip(expected) {
            assert_eq!(anchor.kind, kind);
            assert_eq!(anchor.raw_position, expected_anchor(&text, label));
        }
        assert!(text.contains(concat!(
            "▼ Input\nstress input\n",
            "\n▼ Expected\nNo\n",
            "\n▼ Actual\nNo\n",
            "\n▼ Stderr\ndiagnostic",
        )));
        assert_snapshot_matches(&document);
    }

    #[test]
    fn collapsing_omits_the_shared_body_but_keeps_header_anchor_and_expand_restores_it() {
        let (mut app, run_id) = running_app();
        let actual = (0..4_000)
            .map(|line| format!("unique-actual-line-{line}\n"))
            .collect::<String>();
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseComparison {
                number: 1,
                input: "input\n".to_string(),
                expected: "trusted\n".to_string(),
                actual,
            },
        ));

        let actual_state = app.current_problem().unwrap().run.cases[0]
            .actual
            .as_ref()
            .unwrap();
        let actual_pointer = actual_state.as_ptr();
        let expanded = DetailDocument::from_app(&app);
        let expanded_text = document_text(&expanded);
        assert!(expanded_text.contains("▼ Actual\nunique-actual-line-0\n"));
        assert!(
            expanded
                .segments()
                .any(|segment| segment.text().as_ptr() == actual_pointer)
        );
        let expanded_len = expanded.raw_len;
        drop(expanded);

        app.toggle_detail_section(DetailSectionKind::Actual);
        let collapsed = DetailDocument::from_app(&app);
        let collapsed_text = document_text(&collapsed);
        assert!(collapsed_text.contains("▶ Actual"));
        assert!(!collapsed_text.contains("unique-actual-line-0"));
        assert!(collapsed.raw_len < expanded_len);
        assert!(
            !collapsed
                .segments()
                .any(|segment| segment.text().as_ptr() == actual_pointer)
        );
        let actual_anchor = collapsed
            .section_anchors()
            .iter()
            .find(|anchor| anchor.kind == DetailSectionKind::Actual)
            .unwrap();
        assert_eq!(
            actual_anchor.raw_position,
            RawOffset(collapsed_text.find("▶ Actual").unwrap())
        );
        drop(collapsed);

        let rebuilt_while_collapsed = DetailDocument::from_app(&app);
        assert!(document_text(&rebuilt_while_collapsed).contains("▶ Actual"));
        assert!(!document_text(&rebuilt_while_collapsed).contains("unique-actual-line-0"));
        assert!(
            app.detail_fold_state()
                .is_collapsed(DetailSectionKind::Actual)
        );
        drop(rebuilt_while_collapsed);

        app.toggle_detail_section(DetailSectionKind::Actual);
        let restored = DetailDocument::from_app(&app);
        assert_eq!(document_text(&restored), expanded_text);
        assert!(
            restored
                .segments()
                .any(|segment| segment.text().as_ptr() == actual_pointer)
        );
    }

    #[test]
    fn multiple_fold_toggles_preserve_unrelated_folds_across_document_rebuilds() {
        let (mut app, run_id) = running_app();
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseComparison {
                number: 1,
                input: "input\n".to_string(),
                expected: "trusted\n".to_string(),
                actual: "candidate\n".to_string(),
            },
        ));

        app.toggle_detail_section(DetailSectionKind::Expected);
        let expected_collapsed = DetailDocument::from_app(&app);
        assert!(document_text(&expected_collapsed).contains("▶ Expected"));
        drop(expected_collapsed);

        app.toggle_detail_section(DetailSectionKind::Actual);
        let both_collapsed = DetailDocument::from_app(&app);
        let both_text = document_text(&both_collapsed);
        assert!(both_text.contains("▶ Expected"));
        assert!(both_text.contains("▶ Actual"));
        drop(both_collapsed);

        app.toggle_detail_section(DetailSectionKind::Actual);
        let actual_expanded = DetailDocument::from_app(&app);
        let actual_expanded_text = document_text(&actual_expanded);
        assert!(actual_expanded_text.contains("▶ Expected"));
        assert!(actual_expanded_text.contains("▼ Actual\ncandidate\n"));
        assert!(
            app.detail_fold_state()
                .is_collapsed(DetailSectionKind::Expected)
        );
        assert!(
            !app.detail_fold_state()
                .is_collapsed(DetailSectionKind::Actual)
        );
    }

    #[test]
    fn snapshot_shares_raw_arc_buffers_and_remains_send_static() {
        fn assert_send_static<T: Send + 'static>() {}
        assert_send_static::<DetailSnapshot>();

        let (mut app, run_id) = running_app();
        let expected = "expected ".repeat(10_000);
        let actual = "actual ".repeat(10_000);
        let stderr = "stderr ".repeat(10_000);
        let expected_buffer = expected.as_ptr();
        let actual_buffer = actual.as_ptr();
        let stderr_buffer = stderr.as_ptr();
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                elapsed: Duration::from_millis(1),
            },
        ));
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseComparison {
                number: 1,
                input: "input\n".to_string(),
                expected,
                actual,
            },
        ));
        assert!(app.run_event(0, run_id, TestEvent::TestCaseStderr { number: 1, stderr },));

        let case = &app.current_problem().unwrap().run.cases[0];
        let states = [
            (case.expected.as_ref().unwrap(), expected_buffer),
            (case.actual.as_ref().unwrap(), actual_buffer),
            (case.stderr.as_ref().unwrap(), stderr_buffer),
        ];
        let owners_before_snapshot = states.map(|(state, _)| Arc::strong_count(state));
        let document = DetailDocument::from_app(&app);
        for ((state, buffer), owners) in states.iter().zip(owners_before_snapshot) {
            assert_eq!(state.as_ptr(), *buffer);
            let document_shared = document
                .segments()
                .find_map(|segment| match &segment.text {
                    DetailSegmentText::Shared(shared) if Arc::ptr_eq(shared, state) => {
                        Some(*shared)
                    }
                    _ => None,
                })
                .expect("document must borrow the state Arc");
            assert_eq!(document_shared.as_ptr(), *buffer);
            assert_eq!(Arc::strong_count(state), owners);
        }

        let snapshot = document.snapshot();
        for ((state, buffer), owners) in states.iter().zip(owners_before_snapshot) {
            let snapshot_shared = snapshot
                .segments
                .iter()
                .find_map(|segment| match segment {
                    DetailSnapshotSegment::Shared(shared) if Arc::ptr_eq(shared, state) => {
                        Some(shared)
                    }
                    _ => None,
                })
                .expect("snapshot must clone the state Arc");
            assert_eq!(snapshot_shared.as_ptr(), *buffer);
            assert_eq!(Arc::strong_count(state), owners + 1);
        }

        drop(snapshot);
        for ((state, buffer), owners) in states.iter().zip(owners_before_snapshot) {
            assert_eq!(Arc::strong_count(state), owners);
            assert_eq!(state.as_ptr(), *buffer);
            assert!(!state.is_empty());
        }
    }
}
