use super::submission_tracking::{
    RawCaptureKind, SubmissionDiagnostic, SubmissionDiagnosticObserver, SubmissionId,
    SubmissionStatus,
};
use crate::language::{PythonRuntime, SubmissionTarget};
use crate::paths;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(crate) const RAW_CAPTURE_LIMIT_BYTES: usize = 2 * 1024 * 1024;

static NEXT_ATTEMPT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct RawCapture {
    filename: String,
    bytes: Vec<u8>,
    flushed: bool,
}

pub(crate) struct AttemptDiagnostics {
    enabled: bool,
    raw_enabled: bool,
    started: Instant,
    directory: Option<PathBuf>,
    directory_ready: bool,
    filesystem_disabled: bool,
    trace: Vec<String>,
    flushed_trace_lines: usize,
    raw: Vec<RawCapture>,
    raw_bytes: usize,
    safe_to_flush: bool,
    #[cfg(test)]
    flush_probe: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

impl AttemptDiagnostics {
    pub(crate) fn from_env(contest_id: &str, task_id: &str, target: SubmissionTarget) -> Self {
        let raw_enabled = env_flag("ATC_SUBMISSION_TRACE_RAW");
        let enabled = raw_enabled || env_flag("ATC_SUBMISSION_TRACE");
        let directory = enabled
            .then(|| paths::submission_tracking_dir().ok())
            .flatten()
            .map(|root| root.join(attempt_directory_name()));
        Self::new(enabled, raw_enabled, directory, contest_id, task_id, target)
    }

    fn new(
        enabled: bool,
        raw_enabled: bool,
        directory: Option<PathBuf>,
        contest_id: &str,
        task_id: &str,
        target: SubmissionTarget,
    ) -> Self {
        let mut diagnostics = Self {
            enabled,
            raw_enabled,
            started: Instant::now(),
            directory,
            directory_ready: false,
            filesystem_disabled: false,
            trace: Vec::new(),
            flushed_trace_lines: 0,
            raw: Vec::new(),
            raw_bytes: 0,
            safe_to_flush: false,
            #[cfg(test)]
            flush_probe: None,
        };
        if enabled {
            diagnostics
                .trace
                .push("atc-rs submission trace v1".to_string());
            diagnostics
                .trace
                .push(format!("atc-rs-version={}", env!("CARGO_PKG_VERSION")));
            diagnostics.trace.push(format!(
                "attempt={}",
                diagnostics
                    .directory
                    .as_deref()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .unwrap_or("path-unavailable")
            ));
            diagnostics
                .trace
                .push(format!("contest={}", safe_field(contest_id)));
            diagnostics
                .trace
                .push(format!("task={}", safe_field(task_id)));
            diagnostics
                .trace
                .push(format!("selected-policy={}", target_policy(target)));
            diagnostics.trace.push(format!(
                "raw-capture={}",
                if raw_enabled { "enabled" } else { "disabled" }
            ));
            if raw_enabled {
                diagnostics.trace.push(
                    "raw-warning=contains private logged-in AtCoder data; do not publish or commit unsanitized"
                        .to_string(),
                );
            }
            diagnostics.push_timed("attempt started".to_string());
        }
        diagnostics
    }

    pub(crate) fn arm_safe_flush(&mut self) {
        self.safe_to_flush = true;
    }

    pub(crate) fn finish(&mut self, result: &str) {
        if !self.enabled {
            return;
        }
        self.push_timed(format!("result={}", safe_field(result)));
        self.flush();
    }

    pub(crate) fn flush(&mut self) {
        if !self.enabled || !self.safe_to_flush || self.filesystem_disabled {
            return;
        }
        #[cfg(test)]
        if let Some(probe) = &self.flush_probe {
            probe.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        if !self.ensure_directory() {
            return;
        }
        let Some(directory) = self.directory.as_deref() else {
            return;
        };

        if self.flushed_trace_lines < self.trace.len() {
            let pending = self.trace[self.flushed_trace_lines..].join("\n") + "\n";
            let trace_path = directory.join("trace.log");
            let write_result = OpenOptions::new()
                .create(true)
                .append(true)
                .open(trace_path)
                .and_then(|mut file| file.write_all(pending.as_bytes()));
            if write_result.is_ok() {
                self.flushed_trace_lines = self.trace.len();
            }
        }

        for capture in &mut self.raw {
            if capture.flushed {
                continue;
            }
            if fs::write(directory.join(&capture.filename), &capture.bytes).is_ok() {
                capture.flushed = true;
            }
        }
    }

    fn ensure_directory(&mut self) -> bool {
        if self.directory_ready {
            return true;
        }
        let Some(directory) = self.directory.as_deref() else {
            self.filesystem_disabled = true;
            return false;
        };
        let Some(parent) = directory.parent() else {
            self.filesystem_disabled = true;
            return false;
        };
        if fs::create_dir_all(parent).is_err() || fs::create_dir(directory).is_err() {
            self.filesystem_disabled = true;
            return false;
        }
        self.directory_ready = true;
        true
    }

    fn push_timed(&mut self, message: String) {
        if self.enabled {
            self.trace.push(format!(
                "+{:06}ms {message}",
                self.started.elapsed().as_millis()
            ));
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        directory: Option<PathBuf>,
        enabled: bool,
        raw_enabled: bool,
        contest_id: &str,
        task_id: &str,
        target: SubmissionTarget,
    ) -> Self {
        Self::new(enabled, raw_enabled, directory, contest_id, task_id, target)
    }

    #[cfg(test)]
    pub(crate) fn set_flush_probe(
        &mut self,
        probe: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        self.flush_probe = Some(probe);
    }

    #[cfg(test)]
    pub(crate) fn trace_text(&self) -> String {
        self.trace.join("\n")
    }
}

impl SubmissionDiagnosticObserver for AttemptDiagnostics {
    fn observe(&mut self, event: SubmissionDiagnostic) {
        if !self.enabled {
            return;
        }
        let flush_after = matches!(
            &event,
            SubmissionDiagnostic::DiscoveryAttempt { .. }
                | SubmissionDiagnostic::DiscoveryResolved { .. }
                | SubmissionDiagnostic::DiscoveryFailed { .. }
                | SubmissionDiagnostic::StatusObserved { .. }
                | SubmissionDiagnostic::StatusFailed { .. }
        );
        let message = match event {
            SubmissionDiagnostic::LanguageResolved { language_id } => {
                format!("language resolved id={}", safe_field(&language_id))
            }
            SubmissionDiagnostic::BaselineStarted => "baseline started".to_string(),
            SubmissionDiagnostic::BaselineSucceeded { existing_ids } => {
                format!("baseline ok ids={}", ids(&existing_ids))
            }
            SubmissionDiagnostic::BaselineFailed { kind, message } => format!(
                "baseline failed kind={kind} message={}",
                safe_field(&message)
            ),
            SubmissionDiagnostic::PostAccepted => "post accepted".to_string(),
            SubmissionDiagnostic::SubmitFailed {
                after_baseline,
                kind,
                message,
            } => {
                let phase = if kind == "SubmissionRejected" && after_baseline {
                    "post rejected"
                } else if after_baseline {
                    "post failed"
                } else {
                    "submission failed before post"
                };
                format!(
                    "{phase} kind={} message={}",
                    safe_field(&kind),
                    safe_field(&message)
                )
            }
            SubmissionDiagnostic::PostUnknown => "post outcome=unknown".to_string(),
            SubmissionDiagnostic::DiscoveryAttempt {
                attempt,
                visible_ids,
                new_ids,
                observed_union,
            } => format!(
                "discovery attempt={attempt} visible={} new={} observed_union={}",
                ids(&visible_ids),
                ids(&new_ids),
                ids(&observed_union)
            ),
            SubmissionDiagnostic::DiscoveryResolved { submission_id } => {
                format!("resolved_submission_id={submission_id}")
            }
            SubmissionDiagnostic::DiscoveryFailed {
                attempt,
                kind,
                message,
                observed_new_ids,
            } => format!(
                "discovery failed attempt={} kind={kind} message={} observed_new_ids={}",
                attempt.map_or_else(|| "none".to_string(), |value| value.to_string()),
                safe_field(&message),
                ids(&observed_new_ids)
            ),
            SubmissionDiagnostic::StatusObserved {
                attempt,
                submission_id,
                status,
            } => format!(
                "status attempt={attempt} submission_id={submission_id} status={}",
                status_text(status)
            ),
            SubmissionDiagnostic::StatusFailed {
                attempt,
                submission_id,
                kind,
                message,
            } => format!(
                "status failed attempt={attempt} submission_id={submission_id} kind={kind} message={}",
                safe_field(&message)
            ),
            SubmissionDiagnostic::Cancelled { stage } => {
                format!("cancelled {stage}")
            }
            SubmissionDiagnostic::RawCaptureLimited {
                filename,
                original_bytes,
                captured_bytes,
            } => format!(
                "raw capture limited file={} original_bytes={original_bytes} captured_bytes={captured_bytes}",
                safe_field(&filename)
            ),
        };
        self.push_timed(message);
        if flush_after {
            // These are all post-outcome tracking safe points. Incremental flushes keep a long
            // judging run useful even if the process later panics or is terminated abruptly.
            self.flush();
        }
    }

    fn capture_raw(&mut self, kind: RawCaptureKind, text: &str) {
        if !self.enabled || !self.raw_enabled {
            return;
        }
        let filename = kind.filename();
        let original_bytes = text.len();
        let remaining = RAW_CAPTURE_LIMIT_BYTES.saturating_sub(self.raw_bytes);
        let captured_bytes = original_bytes.min(remaining);
        if captured_bytes > 0 {
            self.push_timed(format!(
                "raw captured file={} bytes={captured_bytes}",
                safe_field(&filename)
            ));
            self.raw.push(RawCapture {
                filename: filename.clone(),
                bytes: text.as_bytes()[..captured_bytes].to_vec(),
                flushed: false,
            });
            self.raw_bytes += captured_bytes;
        }
        if captured_bytes < original_bytes {
            self.observe(SubmissionDiagnostic::RawCaptureLimited {
                filename,
                original_bytes,
                captured_bytes,
            });
        }
    }
}

impl Drop for AttemptDiagnostics {
    fn drop(&mut self) {
        if self.enabled && std::thread::panicking() {
            // Unwinding cannot resume the submit transport, so a physical POST that has not
            // started can no longer start. It is therefore safe to persist an immediately-before-
            // POST baseline here, while also preserving diagnostics for a panic during send.
            self.safe_to_flush = true;
            self.push_timed("result=worker_panicked".to_string());
        }
        self.flush();
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| value == "1")
}

fn attempt_directory_name() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = NEXT_ATTEMPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp}-p{}-a{sequence}", std::process::id())
}

fn target_policy(target: SubmissionTarget) -> &'static str {
    match target {
        SubmissionTarget::Cpp => "cpp/latest-compatible",
        SubmissionTarget::Python(PythonRuntime::CPython) => "python/cpython-latest-compatible",
        SubmissionTarget::Python(PythonRuntime::PyPy) => "python/pypy-current-compatible",
    }
}

fn ids(values: &[SubmissionId]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn status_text(status: SubmissionStatus) -> String {
    match status {
        SubmissionStatus::WaitingForJudge => "WJ".to_string(),
        SubmissionStatus::WaitingForRejudge => "WR".to_string(),
        SubmissionStatus::Judging => "Judging".to_string(),
        SubmissionStatus::JudgingProgress {
            judged,
            total,
            provisional,
        } => provisional.map_or_else(
            || format!("{judged}/{total}"),
            |verdict| format!("{judged}/{total} {verdict}"),
        ),
        SubmissionStatus::Finished(result) => result.verdict.to_string(),
    }
}

fn safe_field(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect(),
            '\t' => "\\t".chars().collect(),
            character if character.is_control() => "?".chars().collect(),
            character => vec![character],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atcoder::submission_tracking::{
        RawCaptureKind, SubmissionDiagnostic, SubmissionResult, SubmissionTrackingErrorKind,
        Verdict,
    };

    fn diagnostics(directory: &Path, raw_enabled: bool) -> AttemptDiagnostics {
        AttemptDiagnostics::for_test(
            Some(directory.to_path_buf()),
            true,
            raw_enabled,
            "abc473",
            "abc473_c",
            SubmissionTarget::Cpp,
        )
    }

    #[test]
    fn disabled_diagnostics_never_create_the_attempt_directory() {
        let temp = tempfile::tempdir().unwrap();
        let attempt = temp.path().join("disabled-attempt");
        let mut diagnostics = AttemptDiagnostics::for_test(
            Some(attempt.clone()),
            false,
            false,
            "abc473",
            "abc473_c",
            SubmissionTarget::Cpp,
        );

        diagnostics.observe(SubmissionDiagnostic::PostAccepted);
        diagnostics.arm_safe_flush();
        diagnostics.finish("finished");

        assert!(!attempt.exists());
        assert!(diagnostics.trace_text().is_empty());
    }

    #[test]
    fn safe_trace_records_sequence_without_raw_or_sensitive_values() {
        let temp = tempfile::tempdir().unwrap();
        let attempt = temp.path().join("safe-attempt");
        let mut diagnostics = diagnostics(&attempt, false);
        diagnostics.observe(SubmissionDiagnostic::LanguageResolved {
            language_id: "6017".to_string(),
        });
        diagnostics.observe(SubmissionDiagnostic::BaselineStarted);
        diagnostics.observe(SubmissionDiagnostic::BaselineSucceeded {
            existing_ids: vec![SubmissionId::for_test(10)],
        });
        diagnostics.capture_raw(
            RawCaptureKind::Baseline,
            "COOKIE=secret csrf_token source-text",
        );
        diagnostics.observe(SubmissionDiagnostic::PostAccepted);
        diagnostics.observe(SubmissionDiagnostic::DiscoveryAttempt {
            attempt: 1,
            visible_ids: vec![SubmissionId::for_test(10)],
            new_ids: vec![],
            observed_union: vec![],
        });
        diagnostics.observe(SubmissionDiagnostic::DiscoveryAttempt {
            attempt: 2,
            visible_ids: vec![SubmissionId::for_test(10), SubmissionId::for_test(11)],
            new_ids: vec![SubmissionId::for_test(11)],
            observed_union: vec![SubmissionId::for_test(11)],
        });
        diagnostics.observe(SubmissionDiagnostic::DiscoveryResolved {
            submission_id: SubmissionId::for_test(11),
        });
        for (attempt_number, status) in [
            (1, SubmissionStatus::WaitingForJudge),
            (
                2,
                SubmissionStatus::JudgingProgress {
                    judged: 14,
                    total: 50,
                    provisional: None,
                },
            ),
            (
                3,
                SubmissionStatus::Finished(SubmissionResult::new(Verdict::Accepted)),
            ),
        ] {
            diagnostics.observe(SubmissionDiagnostic::StatusObserved {
                attempt: attempt_number,
                submission_id: SubmissionId::for_test(11),
                status,
            });
        }
        diagnostics.arm_safe_flush();
        diagnostics.finish("finished");

        let trace = fs::read_to_string(attempt.join("trace.log")).unwrap();
        for expected in [
            "atc-rs submission trace v1",
            "contest=abc473",
            "task=abc473_c",
            "selected-policy=cpp/latest-compatible",
            "language resolved id=6017",
            "baseline ok ids=[10]",
            "post accepted",
            "discovery attempt=1 visible=[10] new=[] observed_union=[]",
            "discovery attempt=2 visible=[10,11] new=[11] observed_union=[11]",
            "resolved_submission_id=11",
            "status attempt=1 submission_id=11 status=WJ",
            "status attempt=2 submission_id=11 status=14/50",
            "status attempt=3 submission_id=11 status=AC",
            "result=finished",
        ] {
            assert!(trace.contains(expected), "missing {expected:?} in {trace}");
        }
        for forbidden in ["COOKIE=secret", "csrf_token", "source-text"] {
            assert!(!trace.contains(forbidden), "safe trace leaked {forbidden}");
        }
        assert_eq!(fs::read_dir(&attempt).unwrap().count(), 1);
    }

    #[test]
    fn raw_capture_writes_exact_fixture_text_only_when_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let attempt = temp.path().join("raw-attempt");
        let baseline = "<html>private baseline</html>";
        let status = r#"{"Result":{"1":{"Html":"WJ"}}}"#;
        let mut diagnostics = diagnostics(&attempt, true);

        diagnostics.capture_raw(RawCaptureKind::Baseline, baseline);
        diagnostics.capture_raw(RawCaptureKind::Discovery { attempt: 1 }, baseline);
        diagnostics.capture_raw(RawCaptureKind::Status { attempt: 1 }, status);
        diagnostics.arm_safe_flush();
        diagnostics.finish("finished");

        assert_eq!(
            fs::read(attempt.join("baseline.html")).unwrap(),
            baseline.as_bytes()
        );
        assert_eq!(
            fs::read(attempt.join("discover-001.html")).unwrap(),
            baseline.as_bytes()
        );
        assert_eq!(
            fs::read(attempt.join("status-001.json")).unwrap(),
            status.as_bytes()
        );
        let trace = fs::read_to_string(attempt.join("trace.log")).unwrap();
        assert!(trace.contains("raw-warning="));
        assert!(!trace.contains("private baseline"));
    }

    #[test]
    fn raw_limit_truncates_capture_without_stopping_later_trace_events() {
        let temp = tempfile::tempdir().unwrap();
        let attempt = temp.path().join("limited-attempt");
        let oversized = "x".repeat(RAW_CAPTURE_LIMIT_BYTES + 137);
        let mut diagnostics = diagnostics(&attempt, true);

        diagnostics.capture_raw(RawCaptureKind::Baseline, &oversized);
        diagnostics.observe(SubmissionDiagnostic::StatusObserved {
            attempt: 1,
            submission_id: SubmissionId::for_test(42),
            status: SubmissionStatus::Finished(SubmissionResult::new(Verdict::Accepted)),
        });
        diagnostics.arm_safe_flush();
        diagnostics.finish("finished");

        assert_eq!(
            fs::metadata(attempt.join("baseline.html")).unwrap().len(),
            RAW_CAPTURE_LIMIT_BYTES as u64
        );
        let trace = fs::read_to_string(attempt.join("trace.log")).unwrap();
        assert!(trace.contains("raw capture limited file=baseline.html"));
        assert!(trace.contains("captured_bytes=2097152"));
        assert!(trace.contains("status attempt=1 submission_id=42 status=AC"));
        assert!(trace.contains("result=finished"));
    }

    #[test]
    fn filesystem_failure_is_swallowed() {
        let temp = tempfile::tempdir().unwrap();
        let parent_file = temp.path().join("not-a-directory");
        fs::write(&parent_file, b"file").unwrap();
        let mut diagnostics = diagnostics(&parent_file.join("attempt"), true);

        diagnostics.observe(SubmissionDiagnostic::BaselineFailed {
            kind: SubmissionTrackingErrorKind::HttpStatus,
            message: "tracking request failed: HTTP 403".to_string(),
        });
        diagnostics.capture_raw(RawCaptureKind::Baseline, "private");
        diagnostics.arm_safe_flush();
        diagnostics.finish("tracking_unavailable_baseline");

        assert!(parent_file.is_file());
        assert!(
            diagnostics
                .trace_text()
                .contains("baseline failed kind=HttpStatus")
        );
    }

    #[test]
    fn flush_is_inert_until_the_post_safe_point_is_armed() {
        let temp = tempfile::tempdir().unwrap();
        let attempt = temp.path().join("timing-attempt");
        let probe = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut diagnostics = diagnostics(&attempt, true);
        diagnostics.set_flush_probe(std::sync::Arc::clone(&probe));

        diagnostics.observe(SubmissionDiagnostic::BaselineStarted);
        diagnostics.capture_raw(RawCaptureKind::Baseline, "baseline");
        diagnostics.flush();

        assert_eq!(probe.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!attempt.exists());

        diagnostics.arm_safe_flush();
        diagnostics.flush();
        assert_eq!(probe.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(attempt.join("baseline.html")).unwrap(),
            b"baseline"
        );
    }

    #[test]
    fn panic_after_the_post_safe_point_flushes_buffered_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let attempt = temp.path().join("panic-attempt");
        let panic_attempt = attempt.clone();

        let result = std::panic::catch_unwind(move || {
            let mut diagnostics = diagnostics(&panic_attempt, true);
            diagnostics.observe(SubmissionDiagnostic::PostAccepted);
            diagnostics.capture_raw(RawCaptureKind::Status { attempt: 1 }, "private status");
            diagnostics.arm_safe_flush();
            panic!("scripted worker panic");
        });

        assert!(result.is_err());
        let trace = fs::read_to_string(attempt.join("trace.log")).unwrap();
        assert!(trace.contains("post accepted"));
        assert!(trace.contains("result=worker_panicked"));
        assert_eq!(
            fs::read(attempt.join("status-001.json")).unwrap(),
            b"private status"
        );
    }

    #[test]
    fn unwinding_before_the_post_safe_point_flushes_only_after_post_can_no_longer_start() {
        let temp = tempfile::tempdir().unwrap();
        let attempt = temp.path().join("pre-arm-panic-attempt");
        let panic_attempt = attempt.clone();

        let result = std::panic::catch_unwind(move || {
            let mut diagnostics = diagnostics(&panic_attempt, true);
            diagnostics.observe(SubmissionDiagnostic::BaselineStarted);
            diagnostics.capture_raw(RawCaptureKind::Baseline, "private baseline");
            panic!("scripted panic before submit returns");
        });

        assert!(result.is_err());
        let trace = fs::read_to_string(attempt.join("trace.log")).unwrap();
        assert!(trace.contains("baseline started"));
        assert!(trace.contains("result=worker_panicked"));
        assert_eq!(
            fs::read(attempt.join("baseline.html")).unwrap(),
            b"private baseline"
        );
    }
}
