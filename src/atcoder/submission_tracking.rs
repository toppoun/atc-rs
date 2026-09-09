use super::{AtCoderClient, AtCoderError, BASE_URL, HttpSource, Source};

use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::thread;
use std::time::Duration;
use time::{Date, Month, OffsetDateTime, PlainDateTime, Time, UtcOffset};

const DISCOVERY_ATTEMPTS: usize = 3;
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(1);
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const STATUS_POLL_ATTEMPTS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SubmissionId(u64);

impl SubmissionId {
    #[cfg(test)]
    pub(crate) fn for_test(value: u64) -> Self {
        assert!(value > 0);
        Self(value)
    }
}

impl fmt::Display for SubmissionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Accepted,
    WrongAnswer,
    TimeLimitExceeded,
    MemoryLimitExceeded,
    RuntimeError,
    CompilationError,
    QueryLimitExceeded,
    OutputLimitExceeded,
    InternalError,
}

impl Verdict {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "AC" => Some(Self::Accepted),
            "WA" => Some(Self::WrongAnswer),
            "TLE" => Some(Self::TimeLimitExceeded),
            "MLE" => Some(Self::MemoryLimitExceeded),
            "RE" => Some(Self::RuntimeError),
            "CE" => Some(Self::CompilationError),
            "QLE" => Some(Self::QueryLimitExceeded),
            "OLE" => Some(Self::OutputLimitExceeded),
            "IE" => Some(Self::InternalError),
            _ => None,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Accepted => "AC",
            Self::WrongAnswer => "WA",
            Self::TimeLimitExceeded => "TLE",
            Self::MemoryLimitExceeded => "MLE",
            Self::RuntimeError => "RE",
            Self::CompilationError => "CE",
            Self::QueryLimitExceeded => "QLE",
            Self::OutputLimitExceeded => "OLE",
            Self::InternalError => "IE",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubmissionResult {
    pub(crate) verdict: Verdict,
    pub(crate) execution_time_ms: Option<u64>,
    pub(crate) memory_kib: Option<u64>,
}

impl SubmissionResult {
    pub(crate) const fn new(verdict: Verdict) -> Self {
        Self {
            verdict,
            execution_time_ms: None,
            memory_kib: None,
        }
    }

    pub(crate) const fn with_metrics(
        verdict: Verdict,
        execution_time_ms: Option<u64>,
        memory_kib: Option<u64>,
    ) -> Self {
        Self {
            verdict,
            execution_time_ms,
            memory_kib,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmissionStatus {
    WaitingForJudge,
    WaitingForRejudge,
    Judging,
    JudgingProgress {
        judged: u32,
        total: u32,
        provisional: Option<Verdict>,
    },
    Finished(SubmissionResult),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubmissionDiscovery {
    pub(crate) submission_id: SubmissionId,
    pub(crate) submitted_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmissionTrackingErrorKind {
    Unavailable,
    HttpStatus,
    HttpTransport,
    RateLimited,
    Fetch,
    InvalidIdentity,
    MalformedSubmissionList,
    SubmissionNotFound,
    AmbiguousSubmissionIds,
    Cancelled,
    StatusPollingTimedOut,
    MalformedStatusJson,
    TargetStatusMissing,
    StatusHtmlMissing,
    StatusCellMissing,
    MultipleStatusCells,
    InvalidStatus,
}

impl fmt::Display for SubmissionTrackingErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "Unavailable",
            Self::HttpStatus => "HttpStatus",
            Self::HttpTransport => "HttpTransport",
            Self::RateLimited => "RateLimited",
            Self::Fetch => "Fetch",
            Self::InvalidIdentity => "InvalidIdentity",
            Self::MalformedSubmissionList => "MalformedSubmissionList",
            Self::SubmissionNotFound => "SubmissionNotFound",
            Self::AmbiguousSubmissionIds => "AmbiguousSubmissionIds",
            Self::Cancelled => "Cancelled",
            Self::StatusPollingTimedOut => "StatusPollingTimedOut",
            Self::MalformedStatusJson => "MalformedStatusJson",
            Self::TargetStatusMissing => "TargetStatusMissing",
            Self::StatusHtmlMissing => "StatusHtmlMissing",
            Self::StatusCellMissing => "StatusCellMissing",
            Self::MultipleStatusCells => "MultipleStatusCells",
            Self::InvalidStatus => "InvalidStatus",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubmissionDiagnostic {
    LanguageResolved {
        language_id: String,
    },
    BaselineStarted,
    BaselineSucceeded {
        existing_ids: Vec<SubmissionId>,
    },
    BaselineFailed {
        kind: SubmissionTrackingErrorKind,
        message: String,
    },
    PostAccepted,
    SubmitFailed {
        after_baseline: bool,
        kind: String,
        message: String,
    },
    PostUnknown,
    DiscoveryAttempt {
        attempt: usize,
        visible_ids: Vec<SubmissionId>,
        new_ids: Vec<SubmissionId>,
        observed_union: Vec<SubmissionId>,
    },
    DiscoveryResolved {
        submission_id: SubmissionId,
    },
    DiscoveryFailed {
        attempt: Option<usize>,
        kind: SubmissionTrackingErrorKind,
        message: String,
        observed_new_ids: Vec<SubmissionId>,
    },
    StatusObserved {
        attempt: usize,
        submission_id: SubmissionId,
        status: SubmissionStatus,
    },
    StatusFailed {
        attempt: usize,
        submission_id: SubmissionId,
        kind: SubmissionTrackingErrorKind,
        message: String,
    },
    Cancelled {
        stage: &'static str,
    },
    RawCaptureLimited {
        filename: String,
        original_bytes: usize,
        captured_bytes: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawCaptureKind {
    Baseline,
    Discovery { attempt: usize },
    Status { attempt: usize },
}

impl RawCaptureKind {
    pub(crate) fn filename(self) -> String {
        match self {
            Self::Baseline => "baseline.html".to_string(),
            Self::Discovery { attempt } => format!("discover-{attempt:03}.html"),
            Self::Status { attempt } => format!("status-{attempt:03}.json"),
        }
    }
}

pub(crate) trait SubmissionDiagnosticObserver {
    fn observe(&mut self, event: SubmissionDiagnostic);
    fn capture_raw(&mut self, kind: RawCaptureKind, text: &str);
}

#[allow(dead_code)]
struct NoopSubmissionDiagnosticObserver;

impl SubmissionDiagnosticObserver for NoopSubmissionDiagnosticObserver {
    fn observe(&mut self, _event: SubmissionDiagnostic) {}

    fn capture_raw(&mut self, _kind: RawCaptureKind, _text: &str) {}
}

#[derive(Debug)]
pub(crate) enum SubmissionTrackingError {
    Unavailable,
    Fetch(AtCoderError),
    InvalidIdentity(&'static str),
    MalformedSubmissionList(&'static str),
    SubmissionNotFound,
    AmbiguousSubmissionIds,
    Cancelled,
    StatusPollingTimedOut,
    MalformedStatusJson(serde_json::Error),
    TargetStatusMissing,
    StatusHtmlMissing,
    StatusCellMissing,
    MultipleStatusCells,
    InvalidStatus,
}

impl fmt::Display for SubmissionTrackingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("submission tracking is unavailable"),
            Self::Fetch(error) => write!(formatter, "tracking request failed: {error}"),
            Self::InvalidIdentity(kind) => write!(formatter, "invalid tracking {kind}"),
            Self::MalformedSubmissionList(message) => {
                write!(formatter, "malformed submission list: {message}")
            }
            Self::SubmissionNotFound => formatter.write_str("new submission ID was not found"),
            Self::AmbiguousSubmissionIds => {
                formatter.write_str("multiple new submission IDs were found")
            }
            Self::Cancelled => formatter.write_str("submission tracking was cancelled"),
            Self::StatusPollingTimedOut => {
                formatter.write_str("submission status polling timed out")
            }
            Self::MalformedStatusJson(error) => {
                write!(formatter, "malformed submission status JSON: {error}")
            }
            Self::TargetStatusMissing => formatter.write_str("target submission status is missing"),
            Self::StatusHtmlMissing => formatter.write_str("submission status HTML is missing"),
            Self::StatusCellMissing => formatter.write_str("submission status cell is missing"),
            Self::MultipleStatusCells => {
                formatter.write_str("multiple submission status cells were found")
            }
            Self::InvalidStatus => formatter.write_str("unrecognized submission status"),
        }
    }
}

impl std::error::Error for SubmissionTrackingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fetch(error) => Some(error),
            Self::MalformedStatusJson(error) => Some(error),
            Self::Unavailable
            | Self::InvalidIdentity(_)
            | Self::MalformedSubmissionList(_)
            | Self::SubmissionNotFound
            | Self::AmbiguousSubmissionIds
            | Self::Cancelled
            | Self::StatusPollingTimedOut
            | Self::TargetStatusMissing
            | Self::StatusHtmlMissing
            | Self::StatusCellMissing
            | Self::MultipleStatusCells
            | Self::InvalidStatus => None,
        }
    }
}

impl SubmissionTrackingError {
    pub(crate) fn diagnostic_kind(&self) -> SubmissionTrackingErrorKind {
        match self {
            Self::Unavailable => SubmissionTrackingErrorKind::Unavailable,
            Self::Fetch(AtCoderError::Http(error)) if error.status().is_some() => {
                SubmissionTrackingErrorKind::HttpStatus
            }
            Self::Fetch(AtCoderError::Http(_)) => SubmissionTrackingErrorKind::HttpTransport,
            Self::Fetch(AtCoderError::RateLimited { .. }) => {
                SubmissionTrackingErrorKind::RateLimited
            }
            Self::Fetch(_) => SubmissionTrackingErrorKind::Fetch,
            Self::InvalidIdentity(_) => SubmissionTrackingErrorKind::InvalidIdentity,
            Self::MalformedSubmissionList(_) => {
                SubmissionTrackingErrorKind::MalformedSubmissionList
            }
            Self::SubmissionNotFound => SubmissionTrackingErrorKind::SubmissionNotFound,
            Self::AmbiguousSubmissionIds => SubmissionTrackingErrorKind::AmbiguousSubmissionIds,
            Self::Cancelled => SubmissionTrackingErrorKind::Cancelled,
            Self::StatusPollingTimedOut => SubmissionTrackingErrorKind::StatusPollingTimedOut,
            Self::MalformedStatusJson(_) => SubmissionTrackingErrorKind::MalformedStatusJson,
            Self::TargetStatusMissing => SubmissionTrackingErrorKind::TargetStatusMissing,
            Self::StatusHtmlMissing => SubmissionTrackingErrorKind::StatusHtmlMissing,
            Self::StatusCellMissing => SubmissionTrackingErrorKind::StatusCellMissing,
            Self::MultipleStatusCells => SubmissionTrackingErrorKind::MultipleStatusCells,
            Self::InvalidStatus => SubmissionTrackingErrorKind::InvalidStatus,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmissionBaseline {
    contest_id: String,
    task_id: String,
    language_id: String,
    ids: BTreeSet<SubmissionId>,
}

impl AtCoderClient {
    #[allow(dead_code)]
    pub(crate) fn capture_submission_baseline(
        &self,
        contest_id: &str,
        task_id: &str,
        language_id: &str,
    ) -> Result<SubmissionBaseline, SubmissionTrackingError> {
        self.capture_submission_baseline_until(contest_id, task_id, language_id, &|| true)
    }

    #[allow(dead_code)]
    pub(crate) fn capture_submission_baseline_until(
        &self,
        contest_id: &str,
        task_id: &str,
        language_id: &str,
        should_continue: &dyn Fn() -> bool,
    ) -> Result<SubmissionBaseline, SubmissionTrackingError> {
        let mut observer = NoopSubmissionDiagnosticObserver;
        self.capture_submission_baseline_until_observed(
            contest_id,
            task_id,
            language_id,
            should_continue,
            &mut observer,
        )
    }

    pub(crate) fn capture_submission_baseline_until_observed(
        &self,
        contest_id: &str,
        task_id: &str,
        language_id: &str,
        should_continue: &dyn Fn() -> bool,
        observer: &mut dyn SubmissionDiagnosticObserver,
    ) -> Result<SubmissionBaseline, SubmissionTrackingError> {
        match &self.source {
            Source::Http(http) => capture_baseline_with_transport_until_observed(
                &mut HttpTrackingTransport { http },
                contest_id,
                task_id,
                language_id,
                should_continue,
                observer,
            ),
            Source::Fixture(_) => {
                observer.observe(SubmissionDiagnostic::BaselineStarted);
                let error = SubmissionTrackingError::Unavailable;
                observe_baseline_error(observer, &error);
                Err(error)
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn discover_submission(
        &self,
        baseline: &SubmissionBaseline,
    ) -> Result<SubmissionDiscovery, SubmissionTrackingError> {
        self.discover_submission_until(baseline, &|| true)
    }

    #[allow(dead_code)]
    pub(crate) fn discover_submission_until(
        &self,
        baseline: &SubmissionBaseline,
        should_continue: &dyn Fn() -> bool,
    ) -> Result<SubmissionDiscovery, SubmissionTrackingError> {
        let mut observer = NoopSubmissionDiagnosticObserver;
        self.discover_submission_until_observed(baseline, should_continue, &mut observer)
    }

    pub(crate) fn discover_submission_until_observed(
        &self,
        baseline: &SubmissionBaseline,
        should_continue: &dyn Fn() -> bool,
        observer: &mut dyn SubmissionDiagnosticObserver,
    ) -> Result<SubmissionDiscovery, SubmissionTrackingError> {
        match &self.source {
            Source::Http(http) => discover_submission_with_transport_until_observed(
                &mut HttpTrackingTransport { http },
                baseline,
                should_continue,
                observer,
            ),
            Source::Fixture(_) => {
                let error = SubmissionTrackingError::Unavailable;
                observe_discovery_error(observer, None, &BTreeSet::new(), &error);
                Err(error)
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn watch_submission(
        &self,
        contest_id: &str,
        submission_id: SubmissionId,
        on_status: &mut dyn FnMut(&SubmissionStatus) -> bool,
    ) -> Result<(), SubmissionTrackingError> {
        self.watch_submission_until(contest_id, submission_id, on_status, &|| true)
    }

    #[allow(dead_code)]
    pub(crate) fn watch_submission_until(
        &self,
        contest_id: &str,
        submission_id: SubmissionId,
        on_status: &mut dyn FnMut(&SubmissionStatus) -> bool,
        should_continue: &dyn Fn() -> bool,
    ) -> Result<(), SubmissionTrackingError> {
        let mut observer = NoopSubmissionDiagnosticObserver;
        self.watch_submission_until_observed(
            contest_id,
            submission_id,
            on_status,
            should_continue,
            &mut observer,
        )
    }

    pub(crate) fn watch_submission_until_observed(
        &self,
        contest_id: &str,
        submission_id: SubmissionId,
        on_status: &mut dyn FnMut(&SubmissionStatus) -> bool,
        should_continue: &dyn Fn() -> bool,
        observer: &mut dyn SubmissionDiagnosticObserver,
    ) -> Result<(), SubmissionTrackingError> {
        match &self.source {
            Source::Http(http) => watch_submission_with_transport_until_observed(
                &mut HttpTrackingTransport { http },
                contest_id,
                submission_id,
                on_status,
                should_continue,
                observer,
            ),
            Source::Fixture(_) => {
                let error = SubmissionTrackingError::Unavailable;
                observe_status_error(observer, 1, submission_id, &error);
                Err(error)
            }
        }
    }
}

trait TrackingTransport {
    fn get_text(&mut self, path: &str) -> Result<String, SubmissionTrackingError>;
    fn wait(&mut self, duration: Duration);

    fn get_text_until(
        &mut self,
        path: &str,
        should_continue: &dyn Fn() -> bool,
    ) -> Result<String, SubmissionTrackingError> {
        if !should_continue() {
            return Err(SubmissionTrackingError::Cancelled);
        }
        let text = self.get_text(path)?;
        if !should_continue() {
            return Err(SubmissionTrackingError::Cancelled);
        }
        Ok(text)
    }

    fn wait_while(&mut self, duration: Duration, should_continue: &dyn Fn() -> bool) -> bool {
        if !should_continue() {
            return false;
        }
        self.wait(duration);
        should_continue()
    }
}

struct HttpTrackingTransport<'a> {
    http: &'a HttpSource,
}

impl HttpTrackingTransport<'_> {
    #[cfg(test)]
    fn client(&self) -> &reqwest::blocking::Client {
        &self.http.client
    }
}

impl TrackingTransport for HttpTrackingTransport<'_> {
    fn get_text(&mut self, path: &str) -> Result<String, SubmissionTrackingError> {
        AtCoderClient::get_text(self.http, &format!("{BASE_URL}{path}"))
            .map_err(SubmissionTrackingError::Fetch)
    }

    fn wait(&mut self, duration: Duration) {
        thread::sleep(duration);
    }

    fn get_text_until(
        &mut self,
        path: &str,
        should_continue: &dyn Fn() -> bool,
    ) -> Result<String, SubmissionTrackingError> {
        AtCoderClient::get_text_until(self.http, &format!("{BASE_URL}{path}"), should_continue)
            .map_err(SubmissionTrackingError::Fetch)?
            .ok_or(SubmissionTrackingError::Cancelled)
    }

    fn wait_while(&mut self, duration: Duration, should_continue: &dyn Fn() -> bool) -> bool {
        const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);
        let deadline = std::time::Instant::now() + duration;
        while should_continue() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return true;
            }
            thread::sleep(remaining.min(CANCEL_POLL_INTERVAL));
        }
        false
    }
}

#[cfg(test)]
fn capture_baseline_with_transport(
    transport: &mut impl TrackingTransport,
    contest_id: &str,
    task_id: &str,
    language_id: &str,
) -> Result<SubmissionBaseline, SubmissionTrackingError> {
    capture_baseline_with_transport_until(transport, contest_id, task_id, language_id, &|| true)
}

#[allow(dead_code)]
fn capture_baseline_with_transport_until(
    transport: &mut impl TrackingTransport,
    contest_id: &str,
    task_id: &str,
    language_id: &str,
    should_continue: &dyn Fn() -> bool,
) -> Result<SubmissionBaseline, SubmissionTrackingError> {
    let mut observer = NoopSubmissionDiagnosticObserver;
    capture_baseline_with_transport_until_observed(
        transport,
        contest_id,
        task_id,
        language_id,
        should_continue,
        &mut observer,
    )
}

fn capture_baseline_with_transport_until_observed(
    transport: &mut impl TrackingTransport,
    contest_id: &str,
    task_id: &str,
    language_id: &str,
    should_continue: &dyn Fn() -> bool,
    observer: &mut dyn SubmissionDiagnosticObserver,
) -> Result<SubmissionBaseline, SubmissionTrackingError> {
    observer.observe(SubmissionDiagnostic::BaselineStarted);
    let result = (|| {
        if !should_continue() {
            return Err(SubmissionTrackingError::Cancelled);
        }
        validate_identifier(contest_id, "contest ID")?;
        validate_identifier(task_id, "task ID")?;
        validate_identifier(language_id, "language ID")?;
        let html = transport.get_text_until(
            &submission_list_path(contest_id, task_id, language_id),
            should_continue,
        )?;
        observer.capture_raw(RawCaptureKind::Baseline, &html);
        let ids = parse_submission_list(contest_id, task_id, language_id, &html)?;

        Ok(SubmissionBaseline {
            contest_id: contest_id.to_string(),
            task_id: task_id.to_string(),
            language_id: language_id.to_string(),
            ids,
        })
    })();

    match &result {
        Ok(baseline) => observer.observe(SubmissionDiagnostic::BaselineSucceeded {
            existing_ids: baseline.ids.iter().copied().collect(),
        }),
        Err(error) => observe_baseline_error(observer, error),
    }
    result
}

#[cfg(test)]
fn discover_submission_with_transport(
    transport: &mut impl TrackingTransport,
    baseline: &SubmissionBaseline,
) -> Result<SubmissionDiscovery, SubmissionTrackingError> {
    discover_submission_with_transport_until(transport, baseline, &|| true)
}

#[allow(dead_code)]
fn discover_submission_with_transport_until(
    transport: &mut impl TrackingTransport,
    baseline: &SubmissionBaseline,
    should_continue: &dyn Fn() -> bool,
) -> Result<SubmissionDiscovery, SubmissionTrackingError> {
    let mut observer = NoopSubmissionDiagnosticObserver;
    discover_submission_with_transport_until_observed(
        transport,
        baseline,
        should_continue,
        &mut observer,
    )
}

fn discover_submission_with_transport_until_observed(
    transport: &mut impl TrackingTransport,
    baseline: &SubmissionBaseline,
    should_continue: &dyn Fn() -> bool,
    observer: &mut dyn SubmissionDiagnosticObserver,
) -> Result<SubmissionDiscovery, SubmissionTrackingError> {
    let mut observed_new_ids = BTreeSet::new();
    let mut discovery_html = Vec::with_capacity(DISCOVERY_ATTEMPTS);

    for attempt in 0..DISCOVERY_ATTEMPTS {
        let attempt_number = attempt + 1;
        if !should_continue() {
            let error = SubmissionTrackingError::Cancelled;
            observe_discovery_error(observer, Some(attempt_number), &observed_new_ids, &error);
            return Err(error);
        }
        let html = match transport.get_text_until(
            &submission_list_path(
                &baseline.contest_id,
                &baseline.task_id,
                &baseline.language_id,
            ),
            should_continue,
        ) {
            Ok(html) => html,
            Err(error) => {
                observe_discovery_error(observer, Some(attempt_number), &observed_new_ids, &error);
                return Err(error);
            }
        };
        observer.capture_raw(
            RawCaptureKind::Discovery {
                attempt: attempt_number,
            },
            &html,
        );
        let ids = match parse_submission_list(
            &baseline.contest_id,
            &baseline.task_id,
            &baseline.language_id,
            &html,
        ) {
            Ok(ids) => ids,
            Err(error) => {
                observe_discovery_error(observer, Some(attempt_number), &observed_new_ids, &error);
                return Err(error);
            }
        };
        discovery_html.push(html);
        let new_ids = ids
            .difference(&baseline.ids)
            .copied()
            .collect::<BTreeSet<_>>();
        observed_new_ids.extend(&new_ids);
        observer.observe(SubmissionDiagnostic::DiscoveryAttempt {
            attempt: attempt_number,
            visible_ids: ids.into_iter().collect(),
            new_ids: new_ids.into_iter().collect(),
            observed_union: observed_new_ids.iter().copied().collect(),
        });

        if attempt + 1 < DISCOVERY_ATTEMPTS
            && !transport.wait_while(DISCOVERY_INTERVAL, should_continue)
        {
            let error = SubmissionTrackingError::Cancelled;
            observe_discovery_error(observer, Some(attempt_number), &observed_new_ids, &error);
            return Err(error);
        }
    }

    // Task and language are exact correlations available from the submit form. Code size is not
    // used: there is no verified contract that AtCoder's displayed value equals the UTF-8 byte
    // length of our snapshot. Submission time is likewise not a reliable ownership token. Even the
    // exact task and language cannot prove ownership if another process posts the same pair and
    // only that ID becomes visible during this bounded window. Accumulating every observed ID
    // catches delayed races without ever narrowing an ambiguous observation back to a singleton.
    let mut candidates = observed_new_ids.into_iter();
    match (candidates.next(), candidates.next()) {
        (Some(id), None) => {
            observer.observe(SubmissionDiagnostic::DiscoveryResolved { submission_id: id });
            let submitted_at = discovery_html
                .iter()
                .rev()
                .find_map(|html| parse_submission_timestamp(html, id));
            Ok(SubmissionDiscovery {
                submission_id: id,
                submitted_at,
            })
        }
        (Some(first), Some(second)) => {
            let observed = std::iter::once(first)
                .chain(std::iter::once(second))
                .chain(candidates)
                .collect::<BTreeSet<_>>();
            let error = SubmissionTrackingError::AmbiguousSubmissionIds;
            observe_discovery_error(observer, Some(DISCOVERY_ATTEMPTS), &observed, &error);
            Err(error)
        }
        (None, _) => {
            let error = SubmissionTrackingError::SubmissionNotFound;
            observe_discovery_error(observer, Some(DISCOVERY_ATTEMPTS), &BTreeSet::new(), &error);
            Err(error)
        }
    }
}

#[cfg(test)]
fn watch_submission_with_transport(
    transport: &mut impl TrackingTransport,
    contest_id: &str,
    submission_id: SubmissionId,
    on_status: &mut dyn FnMut(&SubmissionStatus) -> bool,
) -> Result<(), SubmissionTrackingError> {
    watch_submission_with_transport_until(transport, contest_id, submission_id, on_status, &|| true)
}

#[allow(dead_code)]
fn watch_submission_with_transport_until(
    transport: &mut impl TrackingTransport,
    contest_id: &str,
    submission_id: SubmissionId,
    on_status: &mut dyn FnMut(&SubmissionStatus) -> bool,
    should_continue: &dyn Fn() -> bool,
) -> Result<(), SubmissionTrackingError> {
    let mut observer = NoopSubmissionDiagnosticObserver;
    watch_submission_with_transport_until_observed(
        transport,
        contest_id,
        submission_id,
        on_status,
        should_continue,
        &mut observer,
    )
}

fn watch_submission_with_transport_until_observed(
    transport: &mut impl TrackingTransport,
    contest_id: &str,
    submission_id: SubmissionId,
    on_status: &mut dyn FnMut(&SubmissionStatus) -> bool,
    should_continue: &dyn Fn() -> bool,
    observer: &mut dyn SubmissionDiagnosticObserver,
) -> Result<(), SubmissionTrackingError> {
    if let Err(error) = validate_identifier(contest_id, "contest ID") {
        observe_status_error(observer, 1, submission_id, &error);
        return Err(error);
    }
    let mut previous = None;

    for attempt in 0..STATUS_POLL_ATTEMPTS {
        let attempt_number = attempt + 1;
        if !should_continue() {
            let error = SubmissionTrackingError::Cancelled;
            observe_status_error(observer, attempt_number, submission_id, &error);
            return Err(error);
        }
        let json = match transport
            .get_text_until(&status_path(contest_id, submission_id), should_continue)
        {
            Ok(json) => json,
            Err(error) => {
                observe_status_error(observer, attempt_number, submission_id, &error);
                return Err(error);
            }
        };
        observer.capture_raw(
            RawCaptureKind::Status {
                attempt: attempt_number,
            },
            &json,
        );
        let status = match parse_status_response(submission_id, &json) {
            Ok(status) => status,
            Err(error) => {
                observe_status_error(observer, attempt_number, submission_id, &error);
                return Err(error);
            }
        };
        observer.observe(SubmissionDiagnostic::StatusObserved {
            attempt: attempt_number,
            submission_id,
            status,
        });
        let finished = matches!(status, SubmissionStatus::Finished(_));

        if previous != Some(status) {
            if !on_status(&status) {
                return Ok(());
            }
            previous = Some(status);
        }

        if finished {
            return Ok(());
        }

        if attempt + 1 < STATUS_POLL_ATTEMPTS
            && !transport.wait_while(STATUS_POLL_INTERVAL, should_continue)
        {
            let error = SubmissionTrackingError::Cancelled;
            observe_status_error(observer, attempt_number, submission_id, &error);
            return Err(error);
        }
    }

    let error = SubmissionTrackingError::StatusPollingTimedOut;
    observe_status_error(observer, STATUS_POLL_ATTEMPTS, submission_id, &error);
    Err(error)
}

fn observe_baseline_error(
    observer: &mut dyn SubmissionDiagnosticObserver,
    error: &SubmissionTrackingError,
) {
    observer.observe(SubmissionDiagnostic::BaselineFailed {
        kind: error.diagnostic_kind(),
        message: error.to_string(),
    });
}

fn observe_discovery_error(
    observer: &mut dyn SubmissionDiagnosticObserver,
    attempt: Option<usize>,
    observed_new_ids: &BTreeSet<SubmissionId>,
    error: &SubmissionTrackingError,
) {
    observer.observe(SubmissionDiagnostic::DiscoveryFailed {
        attempt,
        kind: error.diagnostic_kind(),
        message: error.to_string(),
        observed_new_ids: observed_new_ids.iter().copied().collect(),
    });
}

fn observe_status_error(
    observer: &mut dyn SubmissionDiagnosticObserver,
    attempt: usize,
    submission_id: SubmissionId,
    error: &SubmissionTrackingError,
) {
    observer.observe(SubmissionDiagnostic::StatusFailed {
        attempt,
        submission_id,
        kind: error.diagnostic_kind(),
        message: error.to_string(),
    });
}

fn submission_list_path(contest_id: &str, task_id: &str, language_id: &str) -> String {
    let mut url = reqwest::Url::parse(BASE_URL).expect("AtCoder base URL should be valid");
    url.set_path(&format!("/contests/{contest_id}/submissions/me"));
    url.query_pairs_mut()
        .append_pair("f.Language", language_id)
        .append_pair("f.Task", task_id);
    format!(
        "{}?{}",
        url.path(),
        url.query()
            .expect("submission list query should be present")
    )
}

fn status_path(contest_id: &str, submission_id: SubmissionId) -> String {
    format!("/contests/{contest_id}/submissions/me/status/json?sids[]={submission_id}")
}

fn validate_identifier(value: &str, kind: &'static str) -> Result<(), SubmissionTrackingError> {
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(SubmissionTrackingError::InvalidIdentity(kind))
    }
}

fn parse_submission_list(
    contest_id: &str,
    task_id: &str,
    language_id: &str,
    html: &str,
) -> Result<BTreeSet<SubmissionId>, SubmissionTrackingError> {
    validate_identifier(contest_id, "contest ID")?;
    validate_identifier(task_id, "task ID")?;
    validate_identifier(language_id, "language ID")?;

    let document = Html::parse_document(html);
    let table_selector = selector("table.table-bordered.table-striped");
    let mut tables = document.select(&table_selector);
    let Some(table) = tables.next() else {
        if has_canonical_empty_submission_state(&document) {
            return Ok(BTreeSet::new());
        }
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "submission table is missing",
        ));
    };
    if tables.next().is_some() {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "multiple submission tables were found",
        ));
    }
    let body_selector = selector("tbody");
    let mut bodies = table.select(&body_selector);
    let Some(body) = bodies.next() else {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "submission table body is missing",
        ));
    };
    if bodies.next().is_some() {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "multiple submission table bodies were found",
        ));
    }

    let row_selector = selector("tr");
    let score_selector = selector("td.submission-score");
    let detail_selector = selector("a.submission-details-link");
    let task_selector = selector("a[href*='/tasks/']");
    let link_selector = selector("a[href]");
    let expected_task_href = format!("/contests/{contest_id}/tasks/{task_id}");
    let mut ids = BTreeSet::new();

    for row in body.select(&row_selector) {
        let score_id = required_score_id(&row, &score_selector)?;
        let detail_id = required_detail_id(&row, &detail_selector, contest_id)?;
        if score_id != detail_id {
            return Err(SubmissionTrackingError::MalformedSubmissionList(
                "submission ID sources do not match",
            ));
        }

        require_exact_task_link(&row, &task_selector, &expected_task_href)?;
        require_exact_language_link(&row, &link_selector, contest_id, task_id, language_id)?;

        if !ids.insert(score_id) {
            return Err(SubmissionTrackingError::MalformedSubmissionList(
                "duplicate submission ID",
            ));
        }
    }

    Ok(ids)
}

fn has_canonical_empty_submission_state(document: &Html) -> bool {
    let panel_selector = selector(".panel.panel-submission");
    let mut panels = document.select(&panel_selector);
    if panels.next().is_none() || panels.next().is_some() {
        return false;
    }

    let body_selector = selector(".panel.panel-submission > .panel-body");
    let mut bodies = document.select(&body_selector);
    let Some(body) = bodies.next() else {
        return false;
    };
    bodies.next().is_none() && normalized_text(&body) == "No Submissions"
}

fn required_score_id(
    row: &ElementRef<'_>,
    selector: &Selector,
) -> Result<SubmissionId, SubmissionTrackingError> {
    let mut cells = row.select(selector);
    let Some(cell) = cells.next() else {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "submission score cell is missing",
        ));
    };
    if cells.next().is_some() {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "multiple submission score cells were found",
        ));
    }

    cell.value()
        .attr("data-id")
        .and_then(parse_submission_id)
        .ok_or(SubmissionTrackingError::MalformedSubmissionList(
            "submission score data-id is missing or malformed",
        ))
}

fn required_detail_id(
    row: &ElementRef<'_>,
    selector: &Selector,
    contest_id: &str,
) -> Result<SubmissionId, SubmissionTrackingError> {
    let mut links = row.select(selector);
    let Some(link) = links.next() else {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "submission detail link is missing",
        ));
    };
    if links.next().is_some() {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "multiple submission detail links were found",
        ));
    }
    let Some(href) = link.value().attr("href") else {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "submission detail href is missing",
        ));
    };
    let expected_prefix = format!("/contests/{contest_id}/submissions/");
    let Some(value) = href.strip_prefix(&expected_prefix) else {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "submission detail href is malformed",
        ));
    };

    parse_submission_id(value).ok_or(SubmissionTrackingError::MalformedSubmissionList(
        "submission detail ID is malformed",
    ))
}

fn require_exact_task_link(
    row: &ElementRef<'_>,
    selector: &Selector,
    expected_href: &str,
) -> Result<(), SubmissionTrackingError> {
    let mut links = row.select(selector);
    let Some(link) = links.next() else {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "submission task link is missing",
        ));
    };
    if links.next().is_some() || link.value().attr("href") != Some(expected_href) {
        return Err(SubmissionTrackingError::MalformedSubmissionList(
            "submission task href is malformed",
        ));
    }
    Ok(())
}

fn require_exact_language_link(
    row: &ElementRef<'_>,
    selector: &Selector,
    contest_id: &str,
    task_id: &str,
    language_id: &str,
) -> Result<(), SubmissionTrackingError> {
    let expected_path = format!("/contests/{contest_id}/submissions/me");
    let mut matching_links = 0;

    for link in row.select(selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Ok(url) = reqwest::Url::parse(BASE_URL).and_then(|base| base.join(href)) else {
            continue;
        };
        if url.scheme() != "https"
            || url.host_str() != Some("atcoder.jp")
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != expected_path
            || url.fragment().is_some()
        {
            continue;
        }

        let pairs: Vec<_> = url.query_pairs().collect();
        if pairs.len() == 2
            && pairs
                .iter()
                .any(|(key, value)| key == "f.Language" && value.as_ref() == language_id)
            && pairs
                .iter()
                .any(|(key, value)| key == "f.Task" && value.as_ref() == task_id)
        {
            matching_links += 1;
        }
    }

    if matching_links == 1 {
        Ok(())
    } else {
        Err(SubmissionTrackingError::MalformedSubmissionList(
            "submission language link is missing, ambiguous, or mismatched",
        ))
    }
}

fn parse_submission_id(value: &str) -> Option<SubmissionId> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = value.parse::<u64>().ok()?;
    if number == 0 || number.to_string() != value {
        return None;
    }
    Some(SubmissionId(number))
}

fn parse_submission_timestamp(html: &str, submission_id: SubmissionId) -> Option<OffsetDateTime> {
    let document = Html::parse_document(html);
    let table_selector = selector("table.table-bordered.table-striped");
    let row_selector = selector("tbody tr");
    let score_selector = selector("td.submission-score");
    let timestamp_selector = selector("time.fixtime-second");
    let mut matching_rows = document.select(&table_selector).flat_map(|table| {
        table.select(&row_selector).filter(|row| {
            let mut cells = row.select(&score_selector);
            let matches = cells.next().is_some_and(|cell| {
                cell.value().attr("data-id").and_then(parse_submission_id) == Some(submission_id)
            });
            matches && cells.next().is_none()
        })
    });
    let row = matching_rows.next()?;
    if matching_rows.next().is_some() {
        return None;
    }
    let mut timestamps = row.select(&timestamp_selector);
    let timestamp = timestamps.next()?;
    if timestamps.next().is_some() {
        return None;
    }
    parse_official_submission_timestamp(&normalized_text(&timestamp))
}

fn parse_official_submission_timestamp(value: &str) -> Option<OffsetDateTime> {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b' '
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !matches!(bytes[19], b'+' | b'-')
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u32> {
        let digits = bytes.get(range)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        digits.iter().try_fold(0_u32, |value, digit| {
            value.checked_mul(10)?.checked_add(u32::from(*digit - b'0'))
        })
    };
    let year = i32::try_from(number(0..4)?).ok()?;
    let month = Month::try_from(u8::try_from(number(5..7)?).ok()?).ok()?;
    let day = u8::try_from(number(8..10)?).ok()?;
    let hour = u8::try_from(number(11..13)?).ok()?;
    let minute = u8::try_from(number(14..16)?).ok()?;
    let second = u8::try_from(number(17..19)?).ok()?;
    let offset_hour = i8::try_from(number(20..22)?).ok()?;
    let offset_minute = i8::try_from(number(22..24)?).ok()?;
    let sign = if bytes[19] == b'-' { -1 } else { 1 };
    let date = Date::from_calendar_date(year, month, day).ok()?;
    let time = Time::from_hms(hour, minute, second).ok()?;
    let offset = UtcOffset::from_hms(sign * offset_hour, sign * offset_minute, 0).ok()?;
    Some(PlainDateTime::new(date, time).assume_offset(offset))
}

#[derive(Deserialize)]
struct StatusEnvelope {
    #[serde(rename = "Result")]
    result: BTreeMap<String, StatusEntry>,
}

#[derive(Deserialize)]
struct StatusEntry {
    #[serde(rename = "Html")]
    html: Option<String>,
}

fn parse_status_response(
    submission_id: SubmissionId,
    json: &str,
) -> Result<SubmissionStatus, SubmissionTrackingError> {
    let envelope: StatusEnvelope =
        serde_json::from_str(json).map_err(SubmissionTrackingError::MalformedStatusJson)?;
    let entry = envelope
        .result
        .get(&submission_id.to_string())
        .ok_or(SubmissionTrackingError::TargetStatusMissing)?;
    let html = entry
        .html
        .as_deref()
        .ok_or(SubmissionTrackingError::StatusHtmlMissing)?;
    parse_status_html(html)
}

fn parse_status_html(fragment: &str) -> Result<SubmissionStatus, SubmissionTrackingError> {
    let document = Html::parse_document(&format!(
        "<table><tbody><tr>{fragment}</tr></tbody></table>"
    ));
    let status_selector = selector("td.text-center");
    let mut cells = document.select(&status_selector);
    let cell = cells
        .next()
        .ok_or(SubmissionTrackingError::StatusCellMissing)?;
    if cells.next().is_some() {
        return Err(SubmissionTrackingError::MultipleStatusCells);
    }
    let status = parse_status_text(&normalized_text(&cell))?;
    let SubmissionStatus::Finished(result) = status else {
        return Ok(status);
    };
    let metric_selector = selector("td.text-right");
    let metric_values = document
        .select(&metric_selector)
        .map(|cell| normalized_text(&cell))
        .collect::<Vec<_>>();
    let execution_time_ms = metric_values
        .iter()
        .find_map(|value| parse_metric(value, "ms"));
    let memory_kib = metric_values
        .iter()
        .find_map(|value| parse_metric(value, "KiB"));
    Ok(SubmissionStatus::Finished(SubmissionResult::with_metrics(
        result.verdict,
        execution_time_ms,
        memory_kib,
    )))
}

fn parse_status_text(value: &str) -> Result<SubmissionStatus, SubmissionTrackingError> {
    match value {
        "WJ" => return Ok(SubmissionStatus::WaitingForJudge),
        "WR" => return Ok(SubmissionStatus::WaitingForRejudge),
        "Judging" => return Ok(SubmissionStatus::Judging),
        _ => {}
    }

    if let Some(verdict) = Verdict::parse(value) {
        return Ok(SubmissionStatus::Finished(SubmissionResult::new(verdict)));
    }

    let mut fields = value.split_ascii_whitespace();
    let progress = fields
        .next()
        .ok_or(SubmissionTrackingError::InvalidStatus)?;
    let provisional = match fields.next() {
        Some(value) => Some(Verdict::parse(value).ok_or(SubmissionTrackingError::InvalidStatus)?),
        None => None,
    };
    if fields.next().is_some() {
        return Err(SubmissionTrackingError::InvalidStatus);
    }
    let (judged, total) = progress
        .split_once('/')
        .ok_or(SubmissionTrackingError::InvalidStatus)?;
    if total.contains('/') {
        return Err(SubmissionTrackingError::InvalidStatus);
    }
    let judged = parse_ascii_u32(judged).ok_or(SubmissionTrackingError::InvalidStatus)?;
    let total = parse_ascii_u32(total).ok_or(SubmissionTrackingError::InvalidStatus)?;
    if total == 0 || judged > total {
        return Err(SubmissionTrackingError::InvalidStatus);
    }

    Ok(SubmissionStatus::JudgingProgress {
        judged,
        total,
        provisional,
    })
}

fn parse_metric(value: &str, expected_unit: &str) -> Option<u64> {
    let mut fields = value.split_ascii_whitespace();
    let amount = fields.next()?;
    if fields.next()? != expected_unit || fields.next().is_some() {
        return None;
    }
    if amount.is_empty() || !amount.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    amount.parse().ok()
}

fn parse_ascii_u32(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        None
    } else {
        value.parse().ok()
    }
}

fn normalized_text(element: &ElementRef<'_>) -> String {
    element
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("static submission-tracking selector should be valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;

    const EMPTY_LIST: &str =
        include_str!("../../fixtures/submission_tracking/submissions_empty.html");
    const LIVE_EMPTY_LIST: &str =
        include_str!("../../fixtures/submission_tracking/submissions_empty_live.html");
    const SINGLE_LIST: &str =
        include_str!("../../fixtures/submission_tracking/submissions_single.html");
    const STATUS_WJ: &str = include_str!("../../fixtures/submission_tracking/status_wj.json");
    const STATUS_WJ_WA: &str = include_str!("../../fixtures/submission_tracking/status_wj_wa.json");
    const STATUS_JUDGING: &str =
        include_str!("../../fixtures/submission_tracking/status_judging_1_of_50.json");
    const STATUS_PROVISIONAL_WA: &str =
        include_str!("../../fixtures/submission_tracking/status_judging_3_of_36_wa.json");
    const STATUS_AC: &str = include_str!("../../fixtures/submission_tracking/status_ac.json");
    const STATUS_WA: &str = include_str!("../../fixtures/submission_tracking/status_wa.json");

    struct ScriptedTransport {
        responses: VecDeque<Result<String, SubmissionTrackingError>>,
        paths: Vec<String>,
        waits: Vec<Duration>,
    }

    impl ScriptedTransport {
        fn new<I, S>(responses: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: Into<String>,
        {
            Self {
                responses: responses
                    .into_iter()
                    .map(|response| Ok(response.into()))
                    .collect(),
                paths: Vec::new(),
                waits: Vec::new(),
            }
        }

        fn assert_complete(&self) {
            assert!(self.responses.is_empty(), "unused scripted responses");
        }
    }

    impl TrackingTransport for ScriptedTransport {
        fn get_text(&mut self, path: &str) -> Result<String, SubmissionTrackingError> {
            self.paths.push(path.to_string());
            self.responses
                .pop_front()
                .expect("unexpected tracking request")
        }

        fn wait(&mut self, duration: Duration) {
            self.waits.push(duration);
        }
    }

    #[derive(Default)]
    struct RecordingObserver {
        events: Vec<SubmissionDiagnostic>,
        raw: Vec<(RawCaptureKind, String)>,
    }

    impl SubmissionDiagnosticObserver for RecordingObserver {
        fn observe(&mut self, event: SubmissionDiagnostic) {
            self.events.push(event);
        }

        fn capture_raw(&mut self, kind: RawCaptureKind, text: &str) {
            self.raw.push((kind, text.to_string()));
        }
    }

    fn row_with_links(
        data_id: Option<&str>,
        detail_href: Option<&str>,
        task_href: Option<&str>,
        language_href: Option<&str>,
    ) -> String {
        let data_id = data_id
            .map(|id| format!(" data-id=\"{id}\""))
            .unwrap_or_default();
        let detail = detail_href
            .map(|href| format!("<a class=\"submission-details-link\" href=\"{href}\">Detail</a>"))
            .unwrap_or_default();
        let task = task_href
            .map(|href| format!("<a href=\"{href}\">A</a>"))
            .unwrap_or_default();
        let language = language_href
            .map(|href| format!("<a href=\"{href}\">C++</a>"))
            .unwrap_or_default();
        format!(
            "<tr><td>{task}</td><td>{language}</td><td class=\"submission-score\"{data_id}>0</td><td>{detail}</td></tr>"
        )
    }

    fn row(data_id: Option<&str>, detail_href: Option<&str>, task_href: &str) -> String {
        let (contest_id, task_id) = task_href
            .strip_prefix("/contests/")
            .and_then(|rest| rest.split_once("/tasks/"))
            .unwrap_or(("abc473", "abc473_c"));
        let language_href =
            format!("/contests/{contest_id}/submissions/me?f.Language=6017&amp;f.Task={task_id}");
        row_with_links(data_id, detail_href, Some(task_href), Some(&language_href))
    }

    fn row_with_timestamp(
        data_id: Option<&str>,
        detail_href: Option<&str>,
        task_href: &str,
        timestamp: &str,
    ) -> String {
        row(data_id, detail_href, task_href).replacen(
            "<tr>",
            &format!("<tr><td><time class=\"fixtime fixtime-second\">{timestamp}</time></td>"),
            1,
        )
    }

    fn list(rows: &str) -> String {
        format!(
            "<table class=\"table table-bordered table-striped small th-center\"><tbody>{rows}</tbody></table>"
        )
    }

    #[test]
    fn empty_submission_list_is_a_valid_baseline() {
        assert_eq!(
            parse_submission_list("abc473", "abc473_c", "6017", EMPTY_LIST).unwrap(),
            BTreeSet::new()
        );
    }

    #[test]
    fn live_empty_submission_panel_is_a_valid_baseline() {
        assert_eq!(
            parse_submission_list("abc466", "abc466_a", "6017", LIVE_EMPTY_LIST).unwrap(),
            BTreeSet::new()
        );
    }

    #[test]
    fn submission_panel_without_canonical_empty_state_fails_closed() {
        let html = r#"
            <div class="panel panel-default panel-submission">
                <div class="panel-body">Submission history unavailable</div>
            </div>
        "#;
        assert!(matches!(
            parse_submission_list("abc466", "abc466_a", "6017", html),
            Err(SubmissionTrackingError::MalformedSubmissionList(
                "submission table is missing"
            ))
        ));
    }

    #[test]
    fn unrelated_no_submissions_text_is_not_an_empty_submission_state() {
        let html = r#"
            <p>No Submissions</p>
            <div class="panel panel-default panel-submission">
                <div class="panel-body">Unexpected response</div>
            </div>
        "#;
        assert!(matches!(
            parse_submission_list("abc466", "abc466_a", "6017", html),
            Err(SubmissionTrackingError::MalformedSubmissionList(
                "submission table is missing"
            ))
        ));
    }

    #[test]
    fn canonical_empty_panel_does_not_bypass_malformed_table_validation() {
        let html = format!(
            "{LIVE_EMPTY_LIST}{}",
            list(&row(
                Some("10"),
                Some("/contests/abc466/submissions/11"),
                "/contests/abc466/tasks/abc466_a",
            ))
        );
        assert!(matches!(
            parse_submission_list("abc466", "abc466_a", "6017", &html),
            Err(SubmissionTrackingError::MalformedSubmissionList(
                "submission ID sources do not match"
            ))
        ));
    }

    #[test]
    fn measured_single_row_requires_matching_id_sources() {
        assert_eq!(
            parse_submission_list("abc473", "abc473_c", "6017", SINGLE_LIST).unwrap(),
            BTreeSet::from([SubmissionId(78777605)])
        );
    }

    #[test]
    fn multiple_existing_ids_are_preserved() {
        let rows = [
            row(
                Some("10"),
                Some("/contests/abc473/submissions/10"),
                "/contests/abc473/tasks/abc473_c",
            ),
            row(
                Some("20"),
                Some("/contests/abc473/submissions/20"),
                "/contests/abc473/tasks/abc473_c",
            ),
        ]
        .join("");
        assert_eq!(
            parse_submission_list("abc473", "abc473_c", "6017", &list(&rows)).unwrap(),
            BTreeSet::from([SubmissionId(10), SubmissionId(20)])
        );
    }

    #[test]
    fn mismatched_id_sources_do_not_produce_a_candidate() {
        let html = list(&row(
            Some("10"),
            Some("/contests/abc473/submissions/11"),
            "/contests/abc473/tasks/abc473_c",
        ));
        assert!(matches!(
            parse_submission_list("abc473", "abc473_c", "6017", &html),
            Err(SubmissionTrackingError::MalformedSubmissionList(_))
        ));
    }

    #[test]
    fn both_current_id_sources_are_required() {
        let task = "/contests/abc473/tasks/abc473_c";
        let missing_data = list(&row(None, Some("/contests/abc473/submissions/10"), task));
        let missing_detail = list(&row(Some("11"), None, task));
        for html in [missing_data, missing_detail] {
            assert!(matches!(
                parse_submission_list("abc473", "abc473_c", "6017", &html),
                Err(SubmissionTrackingError::MalformedSubmissionList(_))
            ));
        }
    }

    #[test]
    fn malformed_or_wrong_contest_detail_href_rejects_the_row() {
        for href in [
            "/contests/abc473/submissions/not-a-number",
            "/contests/abc999/submissions/10",
            "/contests/abc473/submissions/10/extra",
        ] {
            let html = list(&row(
                Some("10"),
                Some(href),
                "/contests/abc473/tasks/abc473_c",
            ));
            assert!(
                matches!(
                    parse_submission_list("abc473", "abc473_c", "6017", &html),
                    Err(SubmissionTrackingError::MalformedSubmissionList(_))
                ),
                "{href}"
            );
        }
    }

    #[test]
    fn malformed_task_schema_fails_the_whole_submission_table() {
        let valid_other = row(
            Some("10"),
            Some("/contests/abc473/submissions/10"),
            "/contests/abc473/tasks/abc473_c",
        );
        let language = "/contests/abc473/submissions/me?f.Language=6017&amp;f.Task=abc473_c";
        let malformed_rows = [
            row_with_links(
                Some("11"),
                Some("/contests/abc473/submissions/11"),
                None,
                Some(language),
            ),
            row_with_links(
                Some("11"),
                Some("/contests/abc473/submissions/11"),
                Some("/contests/abc473/task/abc473_c"),
                Some(language),
            ),
            row_with_links(
                Some("11"),
                Some("/contests/abc473/submissions/11"),
                Some("/contests/abc473/tasks/abc473_d"),
                Some(language),
            ),
            row_with_links(
                Some("11"),
                Some("/contests/abc473/submissions/11"),
                Some("/contests/abc473/tasks/abc473_c/extra"),
                Some(language),
            ),
        ];

        for malformed in malformed_rows {
            let html = list(&format!("{valid_other}{malformed}"));
            assert!(matches!(
                parse_submission_list("abc473", "abc473_c", "6017", &html),
                Err(SubmissionTrackingError::MalformedSubmissionList(_))
            ));
        }
    }

    #[test]
    fn resolved_language_id_must_match_the_submission_row() {
        let row = row(
            Some("10"),
            Some("/contests/abc473/submissions/10"),
            "/contests/abc473/tasks/abc473_c",
        );
        assert!(matches!(
            parse_submission_list("abc473", "abc473_c", "9999", &list(&row)),
            Err(SubmissionTrackingError::MalformedSubmissionList(_))
        ));
    }

    #[test]
    fn unrelated_tables_are_not_treated_as_submission_rows() {
        let html = format!(
            "<table><tbody><tr><td>navigation</td></tr></tbody></table>{}",
            list("")
        );
        assert_eq!(
            parse_submission_list("abc473", "abc473_c", "6017", &html).unwrap(),
            BTreeSet::new()
        );
    }

    #[test]
    fn missing_submission_table_is_not_mistaken_for_an_empty_list() {
        assert!(matches!(
            parse_submission_list("abc473", "abc473_c", "6017", "<html><body></body></html>"),
            Err(SubmissionTrackingError::MalformedSubmissionList(_))
        ));
    }

    #[test]
    fn duplicate_rows_fail_closed() {
        let row = row(
            Some("10"),
            Some("/contests/abc473/submissions/10"),
            "/contests/abc473/tasks/abc473_c",
        );
        assert!(matches!(
            parse_submission_list("abc473", "abc473_c", "6017", &list(&format!("{row}{row}"))),
            Err(SubmissionTrackingError::MalformedSubmissionList(_))
        ));
    }

    #[test]
    fn stable_task_id_is_used_without_deriving_it_from_adt_contest_id() {
        let html = list(&row(
            Some("99"),
            Some("/contests/adt_easy_20260826_1/submissions/99"),
            "/contests/adt_easy_20260826_1/tasks/abc430_a",
        ));
        let mut transport = ScriptedTransport::new([html]);
        let baseline = capture_baseline_with_transport(
            &mut transport,
            "adt_easy_20260826_1",
            "abc430_a",
            "6017",
        )
        .unwrap();

        assert_eq!(baseline.ids, BTreeSet::from([SubmissionId(99)]));
        assert_eq!(
            transport.paths,
            ["/contests/adt_easy_20260826_1/submissions/me?f.Language=6017&f.Task=abc430_a"]
        );
    }

    #[test]
    fn singleton_seen_throughout_the_settling_window_is_selected() {
        let after = SINGLE_LIST.replace("78777605", "78777606");
        let mut transport =
            ScriptedTransport::new([SINGLE_LIST.to_string(), after.clone(), after.clone(), after]);
        let baseline =
            capture_baseline_with_transport(&mut transport, "abc473", "abc473_c", "6017").unwrap();
        let id = discover_submission_with_transport(&mut transport, &baseline).unwrap();

        assert_eq!(id.submission_id, SubmissionId(78777606));
        assert_eq!(transport.paths.len(), 4);
        assert_eq!(transport.waits, [DISCOVERY_INTERVAL; 2]);
        transport.assert_complete();
    }

    #[test]
    fn zero_then_singleton_is_selected_after_the_settling_window() {
        let mut transport =
            ScriptedTransport::new([EMPTY_LIST, EMPTY_LIST, SINGLE_LIST, SINGLE_LIST]);
        let baseline =
            capture_baseline_with_transport(&mut transport, "abc473", "abc473_c", "6017").unwrap();
        let id = discover_submission_with_transport(&mut transport, &baseline).unwrap();

        assert_eq!(id.submission_id, SubmissionId(78777605));
        let submitted_at = id
            .submitted_at
            .expect("the measured row has an official time");
        assert_eq!(submitted_at.year(), 2026);
        assert_eq!(u8::from(submitted_at.month()), 9);
        assert_eq!(submitted_at.day(), 7);
        assert_eq!(submitted_at.hour(), 20);
        assert_eq!(submitted_at.minute(), 36);
        assert_eq!(submitted_at.second(), 23);
        assert_eq!(submitted_at.offset(), UtcOffset::from_hms(9, 0, 0).unwrap());
        assert_eq!(transport.waits, [DISCOVERY_INTERVAL; 2]);
        transport.assert_complete();
    }

    #[test]
    fn malformed_official_timestamp_does_not_fail_id_discovery() {
        let malformed = SINGLE_LIST.replace("2026-09-07 20:36:23+0900", "future format");
        let mut transport = ScriptedTransport::new([
            EMPTY_LIST.to_string(),
            malformed.clone(),
            malformed.clone(),
            malformed,
        ]);
        let baseline =
            capture_baseline_with_transport(&mut transport, "abc473", "abc473_c", "6017").unwrap();

        let discovery = discover_submission_with_transport(&mut transport, &baseline).unwrap();

        assert_eq!(discovery.submission_id, SubmissionId(78777605));
        assert_eq!(discovery.submitted_at, None);
        transport.assert_complete();
    }

    #[test]
    fn resolved_id_never_adopts_another_rows_timestamp() {
        let task = "/contests/abc473/tasks/abc473_c";
        let existing = row_with_timestamp(
            Some("10"),
            Some("/contests/abc473/submissions/10"),
            task,
            "2026-09-07 20:36:22+0900",
        );
        let new = row(Some("11"), Some("/contests/abc473/submissions/11"), task);
        let baseline_html = list(&existing);
        let after = list(&format!("{existing}{new}"));
        let mut transport =
            ScriptedTransport::new([baseline_html, after.clone(), after.clone(), after]);
        let baseline =
            capture_baseline_with_transport(&mut transport, "abc473", "abc473_c", "6017").unwrap();

        let discovery = discover_submission_with_transport(&mut transport, &baseline).unwrap();

        assert_eq!(discovery.submission_id, SubmissionId(11));
        assert_eq!(discovery.submitted_at, None);
        transport.assert_complete();
    }

    #[test]
    fn first_submission_is_discovered_from_live_empty_baseline() {
        let mut transport =
            ScriptedTransport::new([LIVE_EMPTY_LIST, SINGLE_LIST, SINGLE_LIST, SINGLE_LIST]);
        let baseline =
            capture_baseline_with_transport(&mut transport, "abc473", "abc473_c", "6017").unwrap();
        let id = discover_submission_with_transport(&mut transport, &baseline).unwrap();

        assert!(baseline.ids.is_empty());
        assert_eq!(id.submission_id, SubmissionId(78777605));
        assert_eq!(transport.waits, [DISCOVERY_INTERVAL; 2]);
        transport.assert_complete();
    }

    #[test]
    fn observed_live_empty_baseline_succeeds_with_empty_diagnostic_ids() {
        let mut transport = ScriptedTransport::new([LIVE_EMPTY_LIST]);
        let mut observer = RecordingObserver::default();

        let baseline = capture_baseline_with_transport_until_observed(
            &mut transport,
            "abc466",
            "abc466_a",
            "6017",
            &|| true,
            &mut observer,
        )
        .unwrap();

        assert!(baseline.ids.is_empty());
        assert_eq!(
            observer.events,
            [
                SubmissionDiagnostic::BaselineStarted,
                SubmissionDiagnostic::BaselineSucceeded {
                    existing_ids: vec![]
                }
            ]
        );
        assert!(observer.events.iter().all(|event| !matches!(
            event,
            SubmissionDiagnostic::BaselineFailed {
                kind: SubmissionTrackingErrorKind::MalformedSubmissionList,
                ..
            }
        )));
        transport.assert_complete();
    }

    #[test]
    fn live_empty_baseline_is_logged_as_success_when_diagnostics_are_enabled() {
        let mut transport = ScriptedTransport::new([LIVE_EMPTY_LIST]);
        let mut diagnostics = crate::atcoder::submission_diagnostics::AttemptDiagnostics::for_test(
            None,
            true,
            false,
            "abc466",
            "abc466_a",
            crate::language::SubmissionTarget::Cpp,
        );

        let baseline = capture_baseline_with_transport_until_observed(
            &mut transport,
            "abc466",
            "abc466_a",
            "6017",
            &|| true,
            &mut diagnostics,
        )
        .unwrap();

        assert!(baseline.ids.is_empty());
        let trace = diagnostics.trace_text();
        assert!(trace.contains("baseline ok ids=[]"), "{trace}");
        assert!(!trace.contains("MalformedSubmissionList"), "{trace}");
        transport.assert_complete();
    }

    #[test]
    fn observed_tracking_records_every_discovery_and_status_poll_including_duplicates() {
        let after = SINGLE_LIST.to_string();
        let status_wj = STATUS_WJ.replace("78905773", "78777605");
        let status_judging = STATUS_JUDGING.replace("78905773", "78777605");
        let status_ac = STATUS_AC.replace("78905773", "78777605");
        let mut transport = ScriptedTransport::new([
            EMPTY_LIST.to_string(),
            EMPTY_LIST.to_string(),
            after.clone(),
            after,
            status_wj.clone(),
            status_wj,
            status_judging,
            status_ac,
        ]);
        let mut observer = RecordingObserver::default();

        let baseline = capture_baseline_with_transport_until_observed(
            &mut transport,
            "abc473",
            "abc473_c",
            "6017",
            &|| true,
            &mut observer,
        )
        .unwrap();
        let id = discover_submission_with_transport_until_observed(
            &mut transport,
            &baseline,
            &|| true,
            &mut observer,
        )
        .unwrap();
        let mut ui_statuses = Vec::new();
        watch_submission_with_transport_until_observed(
            &mut transport,
            "abc473",
            id.submission_id,
            &mut |status| {
                ui_statuses.push(*status);
                true
            },
            &|| true,
            &mut observer,
        )
        .unwrap();

        assert_eq!(id.submission_id, SubmissionId(78777605));
        let attempts = observer
            .events
            .iter()
            .filter_map(|event| match event {
                SubmissionDiagnostic::DiscoveryAttempt {
                    attempt,
                    visible_ids,
                    new_ids,
                    observed_union,
                } => Some((
                    *attempt,
                    visible_ids.clone(),
                    new_ids.clone(),
                    observed_union.clone(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            attempts,
            [
                (1, vec![], vec![], vec![]),
                (
                    2,
                    vec![SubmissionId(78777605)],
                    vec![SubmissionId(78777605)],
                    vec![SubmissionId(78777605)]
                ),
                (
                    3,
                    vec![SubmissionId(78777605)],
                    vec![SubmissionId(78777605)],
                    vec![SubmissionId(78777605)]
                ),
            ]
        );
        assert!(
            observer
                .events
                .contains(&SubmissionDiagnostic::DiscoveryResolved {
                    submission_id: SubmissionId(78777605)
                })
        );
        let diagnostic_statuses = observer
            .events
            .iter()
            .filter_map(|event| match event {
                SubmissionDiagnostic::StatusObserved { status, .. } => Some(*status),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(diagnostic_statuses.len(), 4);
        assert_eq!(
            diagnostic_statuses[..2],
            [SubmissionStatus::WaitingForJudge; 2]
        );
        assert_eq!(
            ui_statuses.len(),
            3,
            "duplicate UI statuses must stay suppressed"
        );
        assert_eq!(
            observer
                .raw
                .iter()
                .map(|(kind, _)| *kind)
                .collect::<Vec<_>>(),
            [
                RawCaptureKind::Baseline,
                RawCaptureKind::Discovery { attempt: 1 },
                RawCaptureKind::Discovery { attempt: 2 },
                RawCaptureKind::Discovery { attempt: 3 },
                RawCaptureKind::Status { attempt: 1 },
                RawCaptureKind::Status { attempt: 2 },
                RawCaptureKind::Status { attempt: 3 },
                RawCaptureKind::Status { attempt: 4 },
            ]
        );
        transport.assert_complete();
    }

    #[test]
    fn missing_candidate_exhausts_the_bounded_discovery_window() {
        let mut transport =
            ScriptedTransport::new([EMPTY_LIST, EMPTY_LIST, EMPTY_LIST, EMPTY_LIST]);
        let baseline =
            capture_baseline_with_transport(&mut transport, "abc473", "abc473_c", "6017").unwrap();

        assert!(matches!(
            discover_submission_with_transport(&mut transport, &baseline),
            Err(SubmissionTrackingError::SubmissionNotFound)
        ));
        assert_eq!(transport.waits, [DISCOVERY_INTERVAL; 2]);
        transport.assert_complete();
    }

    #[test]
    fn observed_not_found_preserves_all_three_empty_poll_snapshots() {
        let mut transport = ScriptedTransport::new([EMPTY_LIST, EMPTY_LIST, EMPTY_LIST]);
        let baseline = SubmissionBaseline {
            contest_id: "abc473".to_string(),
            task_id: "abc473_c".to_string(),
            language_id: "6017".to_string(),
            ids: BTreeSet::new(),
        };
        let mut observer = RecordingObserver::default();

        let error = discover_submission_with_transport_until_observed(
            &mut transport,
            &baseline,
            &|| true,
            &mut observer,
        )
        .unwrap_err();

        assert!(matches!(error, SubmissionTrackingError::SubmissionNotFound));
        assert_eq!(
            observer
                .events
                .iter()
                .filter(|event| matches!(event, SubmissionDiagnostic::DiscoveryAttempt { .. }))
                .count(),
            3
        );
        assert!(matches!(
            observer.events.last(),
            Some(SubmissionDiagnostic::DiscoveryFailed {
                kind: SubmissionTrackingErrorKind::SubmissionNotFound,
                observed_new_ids,
                ..
            }) if observed_new_ids.is_empty()
        ));
    }

    #[test]
    fn observed_discovery_malformed_row_preserves_poll_number_and_exact_error() {
        let malformed = list(&row(
            Some("10"),
            Some("/contests/abc473/submissions/11"),
            "/contests/abc473/tasks/abc473_c",
        ));
        let mut transport = ScriptedTransport::new([malformed.clone()]);
        let baseline = SubmissionBaseline {
            contest_id: "abc473".to_string(),
            task_id: "abc473_c".to_string(),
            language_id: "6017".to_string(),
            ids: BTreeSet::new(),
        };
        let mut observer = RecordingObserver::default();

        let error = discover_submission_with_transport_until_observed(
            &mut transport,
            &baseline,
            &|| true,
            &mut observer,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SubmissionTrackingError::MalformedSubmissionList("submission ID sources do not match")
        ));
        assert!(matches!(
            observer.events.last(),
            Some(SubmissionDiagnostic::DiscoveryFailed {
                attempt: Some(1),
                kind: SubmissionTrackingErrorKind::MalformedSubmissionList,
                message,
                observed_new_ids,
            }) if message.contains("submission ID sources do not match") && observed_new_ids.is_empty()
        ));
        assert_eq!(
            observer.raw,
            [(RawCaptureKind::Discovery { attempt: 1 }, malformed)]
        );
    }

    #[test]
    fn singleton_then_two_ids_is_ambiguous() {
        let singleton = list(&row(
            Some("10"),
            Some("/contests/abc473/submissions/10"),
            "/contests/abc473/tasks/abc473_c",
        ));
        let rows = [
            row(
                Some("10"),
                Some("/contests/abc473/submissions/10"),
                "/contests/abc473/tasks/abc473_c",
            ),
            row(
                Some("11"),
                Some("/contests/abc473/submissions/11"),
                "/contests/abc473/tasks/abc473_c",
            ),
        ]
        .join("");
        let two = list(&rows);
        let mut transport =
            ScriptedTransport::new([EMPTY_LIST.to_string(), singleton, two.clone(), two]);
        let baseline =
            capture_baseline_with_transport(&mut transport, "abc473", "abc473_c", "6017").unwrap();

        assert!(matches!(
            discover_submission_with_transport(&mut transport, &baseline),
            Err(SubmissionTrackingError::AmbiguousSubmissionIds)
        ));
        assert_eq!(transport.waits, [DISCOVERY_INTERVAL; 2]);
        transport.assert_complete();
    }

    #[test]
    fn observed_ambiguity_reports_the_complete_new_id_union() {
        let rows = [
            row(
                Some("10"),
                Some("/contests/abc473/submissions/10"),
                "/contests/abc473/tasks/abc473_c",
            ),
            row(
                Some("11"),
                Some("/contests/abc473/submissions/11"),
                "/contests/abc473/tasks/abc473_c",
            ),
        ]
        .join("");
        let two = list(&rows);
        let mut transport = ScriptedTransport::new([two.clone(), two.clone(), two]);
        let baseline = SubmissionBaseline {
            contest_id: "abc473".to_string(),
            task_id: "abc473_c".to_string(),
            language_id: "6017".to_string(),
            ids: BTreeSet::new(),
        };
        let mut observer = RecordingObserver::default();

        let error = discover_submission_with_transport_until_observed(
            &mut transport,
            &baseline,
            &|| true,
            &mut observer,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SubmissionTrackingError::AmbiguousSubmissionIds
        ));
        assert!(matches!(
            observer.events.last(),
            Some(SubmissionDiagnostic::DiscoveryFailed {
                kind: SubmissionTrackingErrorKind::AmbiguousSubmissionIds,
                observed_new_ids,
                ..
            }) if observed_new_ids == &vec![SubmissionId(10), SubmissionId(11)]
        ));
    }

    #[test]
    fn observed_baseline_parse_failure_keeps_its_exact_error_and_raw_html() {
        let malformed = "<html><body>logged-in page without submissions</body></html>";
        let mut transport = ScriptedTransport::new([malformed]);
        let mut observer = RecordingObserver::default();

        let error = capture_baseline_with_transport_until_observed(
            &mut transport,
            "abc473",
            "abc473_c",
            "6017",
            &|| true,
            &mut observer,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SubmissionTrackingError::MalformedSubmissionList("submission table is missing")
        ));
        assert!(matches!(
            observer.events.last(),
            Some(SubmissionDiagnostic::BaselineFailed {
                kind: SubmissionTrackingErrorKind::MalformedSubmissionList,
                message,
            }) if message.contains("submission table is missing")
        ));
        assert_eq!(
            observer.raw,
            [(RawCaptureKind::Baseline, malformed.to_string())]
        );
    }

    #[test]
    fn ambiguity_is_never_narrowed_back_to_a_singleton() {
        let singleton = list(&row(
            Some("11"),
            Some("/contests/abc473/submissions/11"),
            "/contests/abc473/tasks/abc473_c",
        ));
        let two = list(&format!(
            "{}{}",
            row(
                Some("10"),
                Some("/contests/abc473/submissions/10"),
                "/contests/abc473/tasks/abc473_c",
            ),
            row(
                Some("11"),
                Some("/contests/abc473/submissions/11"),
                "/contests/abc473/tasks/abc473_c",
            )
        ));
        let mut transport =
            ScriptedTransport::new([EMPTY_LIST.to_string(), two, singleton.clone(), singleton]);
        let baseline =
            capture_baseline_with_transport(&mut transport, "abc473", "abc473_c", "6017").unwrap();

        assert!(matches!(
            discover_submission_with_transport(&mut transport, &baseline),
            Err(SubmissionTrackingError::AmbiguousSubmissionIds)
        ));
        transport.assert_complete();
    }

    #[test]
    fn external_submission_visible_before_ours_remains_ambiguous() {
        let existing = list(&row(
            Some("9"),
            Some("/contests/abc473/submissions/9"),
            "/contests/abc473/tasks/abc473_c",
        ));
        let external = list(&format!(
            "{}{}",
            row(
                Some("9"),
                Some("/contests/abc473/submissions/9"),
                "/contests/abc473/tasks/abc473_c",
            ),
            row(
                Some("10"),
                Some("/contests/abc473/submissions/10"),
                "/contests/abc473/tasks/abc473_c",
            )
        ));
        let both = list(&format!(
            "{}{}{}",
            row(
                Some("9"),
                Some("/contests/abc473/submissions/9"),
                "/contests/abc473/tasks/abc473_c",
            ),
            row(
                Some("10"),
                Some("/contests/abc473/submissions/10"),
                "/contests/abc473/tasks/abc473_c",
            ),
            row(
                Some("11"),
                Some("/contests/abc473/submissions/11"),
                "/contests/abc473/tasks/abc473_c",
            )
        ));
        let ours_only = list(&format!(
            "{}{}",
            row(
                Some("9"),
                Some("/contests/abc473/submissions/9"),
                "/contests/abc473/tasks/abc473_c",
            ),
            row(
                Some("11"),
                Some("/contests/abc473/submissions/11"),
                "/contests/abc473/tasks/abc473_c",
            )
        ));
        let mut transport = ScriptedTransport::new([existing, external, both, ours_only]);
        let baseline =
            capture_baseline_with_transport(&mut transport, "abc473", "abc473_c", "6017").unwrap();

        assert!(matches!(
            discover_submission_with_transport(&mut transport, &baseline),
            Err(SubmissionTrackingError::AmbiguousSubmissionIds)
        ));
        transport.assert_complete();
    }

    #[test]
    fn measured_status_fixtures_parse() {
        let cases = [
            (STATUS_WJ, SubmissionStatus::WaitingForJudge),
            (
                STATUS_JUDGING,
                SubmissionStatus::JudgingProgress {
                    judged: 1,
                    total: 50,
                    provisional: None,
                },
            ),
            (
                STATUS_PROVISIONAL_WA,
                SubmissionStatus::JudgingProgress {
                    judged: 3,
                    total: 36,
                    provisional: Some(Verdict::WrongAnswer),
                },
            ),
            (
                STATUS_AC,
                SubmissionStatus::Finished(SubmissionResult::with_metrics(
                    Verdict::Accepted,
                    Some(234),
                    Some(33_348),
                )),
            ),
            (
                STATUS_WA,
                SubmissionStatus::Finished(SubmissionResult::with_metrics(
                    Verdict::WrongAnswer,
                    Some(266),
                    Some(297_700),
                )),
            ),
        ];

        for (json, expected) in cases {
            let id = if json.contains("78905773") {
                SubmissionId(78905773)
            } else {
                SubmissionId(78905741)
            };
            assert_eq!(parse_status_response(id, json).unwrap(), expected);
        }
    }

    #[test]
    fn waiting_rejudge_plain_judging_and_all_known_final_verdicts_parse() {
        assert_eq!(
            parse_status_text("WR").unwrap(),
            SubmissionStatus::WaitingForRejudge
        );
        assert_eq!(
            parse_status_text("Judging").unwrap(),
            SubmissionStatus::Judging
        );
        for text in ["AC", "WA", "TLE", "MLE", "RE", "CE", "QLE", "OLE", "IE"] {
            assert!(matches!(
                parse_status_text(text),
                Ok(SubmissionStatus::Finished(_))
            ));
        }
    }

    #[test]
    fn judging_colspan_statuses_do_not_require_final_metric_cells() {
        for (text, judged, total, provisional) in [
            ("16/77", 16, 77, None),
            ("48/77", 48, 77, None),
            ("55/72 RE", 55, 72, Some(Verdict::RuntimeError)),
        ] {
            assert_eq!(
                parse_status_html(&format!(
                    "<td colspan='3' class='text-center waiting-judge'><span>{text}</span></td>"
                ))
                .unwrap(),
                SubmissionStatus::JudgingProgress {
                    judged,
                    total,
                    provisional,
                }
            );
        }
    }

    #[test]
    fn final_runtime_and_memory_parse_for_failure_verdicts() {
        for verdict in ["WA", "RE"] {
            assert_eq!(
                parse_status_html(&format!(
                    "<td class='text-center'><span>{verdict}</span></td>\
                     <td class='text-right'>234 ms</td>\
                     <td class='text-right'>33348 KiB</td>"
                ))
                .unwrap(),
                SubmissionStatus::Finished(SubmissionResult::with_metrics(
                    Verdict::parse(verdict).unwrap(),
                    Some(234),
                    Some(33_348),
                ))
            );
        }
    }

    #[test]
    fn malformed_final_metrics_are_independently_best_effort() {
        let cases = [
            ("future runtime", "33348 KiB", None, Some(33_348)),
            ("234 ms", "future memory", Some(234), None),
            ("234 us", "33348 KB", None, None),
            (
                "18446744073709551616 ms",
                "18446744073709551616 KiB",
                None,
                None,
            ),
        ];
        for (runtime, memory, execution_time_ms, memory_kib) in cases {
            assert_eq!(
                parse_status_html(&format!(
                    "<td class='text-center'><span>AC</span></td>\
                     <td class='text-right'>{runtime}</td>\
                     <td class='text-right'>{memory}</td>"
                ))
                .unwrap(),
                SubmissionStatus::Finished(SubmissionResult::with_metrics(
                    Verdict::Accepted,
                    execution_time_ms,
                    memory_kib,
                )),
                "runtime={runtime:?} memory={memory:?}"
            );
        }
    }

    #[test]
    fn malformed_progress_and_unknown_statuses_fail_closed() {
        for text in [
            "",
            "x/50",
            "1/y",
            "51/50",
            "0/0",
            "1/50 WA garbage",
            "1/50 FUTURE",
            "FUTURE",
        ] {
            assert!(
                matches!(
                    parse_status_text(text),
                    Err(SubmissionTrackingError::InvalidStatus)
                ),
                "{text:?}"
            );
        }
    }

    #[test]
    fn missing_json_fields_and_status_html_fail_closed() {
        let id = SubmissionId(1);
        assert!(matches!(
            parse_status_response(id, "{}"),
            Err(SubmissionTrackingError::MalformedStatusJson(_))
        ));
        assert!(matches!(
            parse_status_response(id, r#"{"Result":{}}"#),
            Err(SubmissionTrackingError::TargetStatusMissing)
        ));
        assert!(matches!(
            parse_status_response(id, r#"{"Result":{"1":{"Score":"0"}}}"#),
            Err(SubmissionTrackingError::StatusHtmlMissing)
        ));
        assert!(matches!(
            parse_status_response(id, r#"{"Result":{"1":{"Html":"<div>WJ</div>"}}}"#),
            Err(SubmissionTrackingError::StatusCellMissing)
        ));
    }

    #[test]
    fn observed_status_failure_includes_attempt_submission_id_and_exact_parser_error() {
        let malformed = r#"{"Result":{"42":{"Html":"<div>WJ</div>"}}}"#;
        let mut transport = ScriptedTransport::new([malformed]);
        let mut observer = RecordingObserver::default();

        let error = watch_submission_with_transport_until_observed(
            &mut transport,
            "abc473",
            SubmissionId(42),
            &mut |_| true,
            &|| true,
            &mut observer,
        )
        .unwrap_err();

        assert!(matches!(error, SubmissionTrackingError::StatusCellMissing));
        assert!(matches!(
            observer.events.last(),
            Some(SubmissionDiagnostic::StatusFailed {
                attempt: 1,
                submission_id: SubmissionId(42),
                kind: SubmissionTrackingErrorKind::StatusCellMissing,
                message,
            }) if message == "submission status cell is missing"
        ));
        assert_eq!(
            observer.raw,
            [(RawCaptureKind::Status { attempt: 1 }, malformed.to_string())]
        );
    }

    #[test]
    fn http_status_fetch_error_has_a_distinct_diagnostic_category_and_status_message() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let error = reqwest::blocking::Client::new()
            .get(format!("http://{address}/contests/abc473/submissions/me"))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap_err();
        server.join().unwrap();
        let error = SubmissionTrackingError::Fetch(AtCoderError::Http(error));

        assert_eq!(
            error.diagnostic_kind(),
            SubmissionTrackingErrorKind::HttpStatus
        );
        assert!(error.to_string().contains("403"));
        assert!(error.to_string().contains("submissions/me"));
    }

    #[test]
    fn score_is_optional_and_extra_json_fields_are_ignored() {
        let json = r#"{
            "Result": {"1": {"Html": "<td class='text-center'><span>AC</span></td>", "Future": true}},
            "Extra": "ignored"
        }"#;
        assert_eq!(
            parse_status_response(SubmissionId(1), json).unwrap(),
            SubmissionStatus::Finished(SubmissionResult::new(Verdict::Accepted))
        );
    }

    #[test]
    fn polling_emits_only_changes_and_stops_at_final_verdict() {
        let mut transport =
            ScriptedTransport::new([STATUS_WJ_WA, STATUS_WJ_WA, STATUS_PROVISIONAL_WA, STATUS_WA]);
        let mut observed = Vec::new();
        watch_submission_with_transport(
            &mut transport,
            "abc473",
            SubmissionId(78905741),
            &mut |status| {
                observed.push(*status);
                true
            },
        )
        .unwrap();

        assert_eq!(
            observed,
            [
                SubmissionStatus::WaitingForJudge,
                SubmissionStatus::JudgingProgress {
                    judged: 3,
                    total: 36,
                    provisional: Some(Verdict::WrongAnswer),
                },
                SubmissionStatus::Finished(SubmissionResult::with_metrics(
                    Verdict::WrongAnswer,
                    Some(266),
                    Some(297_700),
                )),
            ]
        );
        assert_eq!(transport.waits, [STATUS_POLL_INTERVAL; 3]);
        assert_eq!(
            transport.paths,
            vec![status_path("abc473", SubmissionId(78905741)); 4]
        );
        transport.assert_complete();
    }

    #[test]
    fn waiting_for_judge_forever_stops_at_the_poll_limit() {
        let mut transport =
            ScriptedTransport::new(vec![STATUS_WJ.to_string(); STATUS_POLL_ATTEMPTS]);
        let mut observed = Vec::new();
        let result = watch_submission_with_transport(
            &mut transport,
            "abc473",
            SubmissionId(78905773),
            &mut |status| {
                observed.push(*status);
                true
            },
        );

        assert!(matches!(
            result,
            Err(SubmissionTrackingError::StatusPollingTimedOut)
        ));
        assert_eq!(observed, [SubmissionStatus::WaitingForJudge]);
        assert_eq!(transport.paths.len(), STATUS_POLL_ATTEMPTS);
        assert_eq!(transport.waits.len(), STATUS_POLL_ATTEMPTS - 1);
        transport.assert_complete();
    }

    #[test]
    fn unchanged_judging_progress_forever_stops_at_the_poll_limit() {
        let mut transport =
            ScriptedTransport::new(vec![STATUS_JUDGING.to_string(); STATUS_POLL_ATTEMPTS]);
        let mut observed = Vec::new();
        let result = watch_submission_with_transport(
            &mut transport,
            "abc473",
            SubmissionId(78905773),
            &mut |status| {
                observed.push(*status);
                true
            },
        );

        assert!(matches!(
            result,
            Err(SubmissionTrackingError::StatusPollingTimedOut)
        ));
        assert_eq!(
            observed,
            [SubmissionStatus::JudgingProgress {
                judged: 1,
                total: 50,
                provisional: None,
            }]
        );
        assert_eq!(transport.paths.len(), STATUS_POLL_ATTEMPTS);
        assert_eq!(transport.waits.len(), STATUS_POLL_ATTEMPTS - 1);
        transport.assert_complete();
    }

    #[test]
    fn observer_can_stop_polling_without_turning_it_into_a_tracking_error() {
        let mut transport = ScriptedTransport::new([STATUS_WJ, STATUS_AC]);
        watch_submission_with_transport(
            &mut transport,
            "abc473",
            SubmissionId(78905773),
            &mut |_| false,
        )
        .unwrap();

        assert_eq!(transport.paths.len(), 1);
        assert!(transport.waits.is_empty());
    }

    #[test]
    fn cancellation_is_checked_between_unchanged_status_polls() {
        use std::cell::Cell;

        let mut transport = ScriptedTransport::new([STATUS_WJ, STATUS_WJ]);
        let checks = Cell::new(0);
        let should_continue = || {
            let next = checks.get() + 1;
            checks.set(next);
            next < 5
        };
        let mut observed = Vec::new();
        let result = watch_submission_with_transport_until(
            &mut transport,
            "abc473",
            SubmissionId(78905773),
            &mut |status| {
                observed.push(*status);
                true
            },
            &should_continue,
        );

        assert!(matches!(result, Err(SubmissionTrackingError::Cancelled)));
        assert_eq!(observed, [SubmissionStatus::WaitingForJudge]);
        assert_eq!(transport.paths.len(), 1);
        assert_eq!(transport.waits, [STATUS_POLL_INTERVAL]);
    }

    #[test]
    fn production_tracking_transport_uses_only_the_normal_get_client() {
        let http = HttpSource::new(None)
            .expect("normal HTTP source should construct without making a request");
        let transport = HttpTrackingTransport { http: &http };

        assert!(std::ptr::eq(transport.client(), &http.client));
        assert!(
            http.submit_client
                .client
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        );
    }
}
