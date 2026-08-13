use std::borrow::Cow;
use std::time::Duration;

use super::app::{CaseVerdict, ProblemState, RunPhase, WatchApp};

#[derive(Debug)]
pub(super) struct DetailSegment<'a> {
    text: Cow<'a, str>,
}

impl DetailSegment<'_> {
    pub(super) fn text(&self) -> &str {
        &self.text
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

    pub(super) fn segments(&self) -> impl Iterator<Item = &DetailSegment<'a>> {
        self.segments.iter()
    }

    pub(super) fn segment_text(&self, index: usize) -> Option<&str> {
        self.segments.get(index).map(DetailSegment::text)
    }

    pub(super) fn segment_count(&self) -> usize {
        self.segments.len()
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

                if let Some(error) = run.error.as_deref() {
                    self.push_section("compiler output", error);
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

                if let Some(error) = run.error.as_deref() {
                    self.push_section("error", error);
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
            }

            CaseVerdict::Accepted => {
                self.push_static("\n\nAccepted");

                if let Some(stderr) = case.stderr.as_deref() {
                    self.push_section("stderr", stderr);
                }
            }

            CaseVerdict::WrongAnswer => {
                self.push_section("expected", case.expected.as_deref().unwrap_or(""));
                self.push_section("actual", case.actual.as_deref().unwrap_or(""));

                if let Some(stderr) = case.stderr.as_deref() {
                    self.push_section("stderr", stderr);
                }
            }

            CaseVerdict::RuntimeError => {
                self.push_static("\n\nRuntime Error");

                if let Some(stderr) = case.stderr.as_deref() {
                    self.push_section("stderr", stderr);
                }
            }

            CaseVerdict::TimedOut => {
                self.push_static("\n\nTime Limit Exceeded");

                if let Some(stderr) = case.stderr.as_deref() {
                    self.push_section("stderr", stderr);
                }
            }
        }
    }

    fn push_section(&mut self, label: &'static str, content: &'a str) {
        self.push_static("\n\n");
        self.push_static(label);
        self.push_static("\n");

        if content.is_empty() {
            self.push_static("(empty)");
        } else {
            self.push_borrowed(content);
        }
    }

    fn push_static(&mut self, text: &'static str) {
        self.segments.push(DetailSegment {
            text: Cow::Borrowed(text),
        });
    }

    fn push_borrowed(&mut self, text: &'a str) {
        self.segments.push(DetailSegment {
            text: Cow::Borrowed(text),
        });
    }

    fn push_owned(&mut self, text: String) {
        self.segments.push(DetailSegment {
            text: Cow::Owned(text),
        });
    }

    #[cfg(test)]
    pub(super) fn from_borrowed_segments(segments: &'a [&'a str]) -> Self {
        Self {
            segments: segments
                .iter()
                .map(|text| DetailSegment {
                    text: Cow::Borrowed(*text),
                })
                .collect(),
        }
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

    fn document_text(document: &DetailDocument<'_>) -> String {
        document.segments().map(DetailSegment::text).collect()
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
                expected,
                actual,
                elapsed: Duration::from_millis(1),
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
            assert!(matches!(segment.text, Cow::Borrowed(_)));
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
        assert!(matches!(compile_segment.text, Cow::Borrowed(_)));

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
        let error_segment = document
            .segments()
            .find(|segment| segment.text().as_ptr() == error.as_ptr())
            .expect("run error must remain borrowed");
        assert!(matches!(error_segment.text, Cow::Borrowed(_)));
    }

    #[test]
    fn empty_wrong_answer_outputs_keep_the_empty_placeholder() {
        let (mut app, run_id) = running_app();
        assert!(app.run_event(
            0,
            run_id,
            TestEvent::TestCaseWrongAnswer {
                number: 1,
                expected: String::new(),
                actual: String::new(),
                elapsed: Duration::ZERO,
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
    }
}
