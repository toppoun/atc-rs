use std::sync::Arc;
use std::time::Duration;

use super::app::{CaseVerdict, ProblemState, RunPhase, WatchApp};

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
}

impl DetailTextSource for DetailSnapshot {
    fn segment_count(&self) -> usize {
        self.segments.len()
    }

    fn segment_text(&self, index: usize) -> Option<&str> {
        self.segments.get(index).map(DetailSnapshotSegment::text)
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
        }
    }

    fn push_problem_detail(&mut self, app: &'a WatchApp, problem: &'a ProblemState) {
        let run = &problem.run;

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

            RunPhase::Running | RunPhase::Finished => {
                self.push_sample_detail(app, problem);
            }
        }
    }

    fn push_sample_detail(&mut self, app: &'a WatchApp, problem: &'a ProblemState) {
        let total = problem.run.total_cases;

        if total == 0 {
            self.push_static("Running samples...");
            return;
        }

        let Some(case) = app.selected_case_state() else {
            self.push_owned(format!(
                "sample {} / {}\n\nPending...",
                app.selected_case() + 1,
                total,
            ));
            return;
        };

        self.push_owned(format!(
            "sample {} / {}   {}{}",
            app.selected_case() + 1,
            total,
            verdict_label(case.verdict),
            elapsed_label(case.elapsed),
        ));

        match case.verdict {
            CaseVerdict::Pending => {
                self.push_static("\n\nPending...");
                return;
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

        self.push_optional_shared_section("expected", case.expected.as_ref());

        self.push_optional_shared_section("actual", case.actual.as_ref());

        if let Some(stderr) = case.stderr.as_ref() {
            self.push_shared_section("stderr", stderr);
        }
    }

    fn push_optional_shared_section(
        &mut self,
        label: &'static str,
        content: Option<&'a Arc<String>>,
    ) {
        if let Some(content) = content {
            self.push_shared_section(label, content);
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
        self.segments.push(DetailSegment {
            text: DetailSegmentText::Static(text),
        });
    }

    fn push_shared(&mut self, text: &'a Arc<String>) {
        self.segments.push(DetailSegment {
            text: DetailSegmentText::Shared(text),
        });
    }

    fn push_owned(&mut self, text: String) {
        self.segments.push(DetailSegment {
            text: DetailSegmentText::Owned(text),
        });
    }

    #[cfg(test)]
    pub(super) fn from_borrowed_segments(segments: &'a [&'a str]) -> Self {
        Self {
            segments: segments
                .iter()
                .map(|text| DetailSegment {
                    text: DetailSegmentText::SharedOwned(Arc::new((*text).to_string())),
                })
                .collect(),
        }
    }

    #[cfg(test)]
    pub(super) fn from_shared_segments(segments: &'a [&'a Arc<String>]) -> Self {
        Self {
            segments: segments
                .iter()
                .map(|text| DetailSegment {
                    text: DetailSegmentText::Shared(text),
                })
                .collect(),
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
    use crate::tui::message::TestEvent;
    use std::path::PathBuf;

    fn contest() -> Contest {
        Contest {
            contest_id: "abc123".to_string(),
            problems: vec![Problem {
                index: "A".to_string(),
                title: "Problem A".to_string(),
                task_id: "abc123_a".to_string(),
                url: "https://example.invalid/a".to_string(),
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
                "\n\nexpected\n  58\n\n日本語 e\u{301} 👩‍💻\n",
                "\n\nactual\n58 \n",
                "\n\nstderr\n debug \n\n",
            )
        );
        assert_snapshot_matches(&document);

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
                "\n\nstderr\nruntime stderr\n",
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
    fn compile_error_and_accepted_stderr_documents_preserve_current_sections() {
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

        let (mut accepted_app, run_id) = running_app();
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
                "\n\nstderr\naccepted stderr",
            )
        );
        assert_snapshot_matches(&DetailDocument::from_app(&accepted_app));
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
                expected: String::new(),
                actual: String::new(),
            },
        ));

        assert_eq!(
            document_text(&DetailDocument::from_app(&app)),
            concat!(
                "A - Problem A\n\n",
                "sample 1 / 1   WA   0.0 ms",
                "\n\nexpected\n(empty)",
                "\n\nactual\n(empty)",
            )
        );
        assert_snapshot_matches(&DetailDocument::from_app(&app));
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
