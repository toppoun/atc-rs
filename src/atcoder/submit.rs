use super::{
    AtCoderClient, BASE_URL, HttpSource, MAX_429_RETRIES, Source, retry_wait, wait_for_request_slot,
};
use crate::language::Language;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, LOCATION};
use scraper::{ElementRef, Html, Selector};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::thread;
use std::time::Duration;

#[derive(Clone, PartialEq, Eq)]
struct CsrfToken(String);

impl fmt::Debug for CsrfToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmitLanguage {
    id: String,
    label: String,
}

impl SubmitLanguage {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmitTask {
    task_id: String,
    languages: Vec<SubmitLanguage>,
}

impl SubmitTask {
    pub(crate) fn task_id(&self) -> &str {
        &self.task_id
    }

    pub(crate) fn languages(&self) -> &[SubmitLanguage] {
        &self.languages
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmitPage {
    form_action: String,
    csrf_token: CsrfToken,
    tasks: BTreeMap<String, SubmitTask>,
}

impl SubmitPage {
    pub(crate) fn form_action(&self) -> &str {
        &self.form_action
    }

    pub(crate) fn csrf_token(&self) -> &str {
        &self.csrf_token.0
    }

    pub(crate) fn tasks(&self) -> impl ExactSizeIterator<Item = &SubmitTask> {
        self.tasks.values()
    }

    pub(crate) fn resolve_language(
        &self,
        task_id: &str,
        language: Language,
    ) -> Result<&SubmitLanguage, SubmitPageError> {
        let task = self
            .tasks
            .get(task_id)
            .ok_or_else(|| SubmitPageError::TaskUnavailable {
                task_id: task_id.to_string(),
            })?;

        let selected = match language {
            Language::Cpp => select_latest(&task.languages, classify_cpp),
            Language::Python => select_latest(&task.languages, classify_python),
        };

        match selected {
            RankedSelection::Selected(language) => Ok(language),
            RankedSelection::Unavailable => Err(SubmitPageError::LanguageUnavailable {
                task_id: task_id.to_string(),
                language,
            }),
            RankedSelection::Ambiguous => Err(SubmitPageError::LanguageAmbiguous {
                task_id: task_id.to_string(),
                language,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubmitPageError {
    SubmitUnavailable,
    MalformedPage(&'static str),
    TaskUnavailable { task_id: String },
    LanguageUnavailable { task_id: String, language: Language },
    LanguageAmbiguous { task_id: String, language: Language },
}

impl fmt::Display for SubmitPageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SubmitUnavailable => formatter.write_str("AtCoder submit form is unavailable"),
            Self::MalformedPage(message) => {
                write!(formatter, "malformed AtCoder submit page: {message}")
            }
            Self::TaskUnavailable { task_id } => {
                write!(formatter, "task {task_id:?} is unavailable for submission")
            }
            Self::LanguageUnavailable { task_id, language } => write!(
                formatter,
                "{language:?} is unavailable for task {task_id:?}"
            ),
            Self::LanguageAmbiguous { task_id, language } => write!(
                formatter,
                "AtCoder language for {language:?} is ambiguous for task {task_id:?}"
            ),
        }
    }
}

impl std::error::Error for SubmitPageError {}

pub(crate) fn parse_submit_page(
    expected_contest_id: &str,
    html: &str,
) -> Result<SubmitPage, SubmitPageError> {
    if !is_valid_identifier(expected_contest_id) {
        return Err(SubmitPageError::MalformedPage(
            "invalid expected contest ID",
        ));
    }

    let document = Html::parse_document(html);
    let form_selector = selector("form");
    let submit_marker_selector =
        selector("select[name='data.TaskScreenName'], #select-lang, textarea[name='sourceCode']");
    let mut submit_forms = document.select(&form_selector).filter(|form| {
        form.value()
            .attr("action")
            .is_some_and(action_looks_like_submit)
            || form.select(&submit_marker_selector).next().is_some()
    });

    let form = submit_forms
        .next()
        .ok_or(SubmitPageError::SubmitUnavailable)?;
    if submit_forms.next().is_some() {
        return Err(SubmitPageError::MalformedPage(
            "multiple submit forms found",
        ));
    }

    validate_form(&form, expected_contest_id)?;
    let csrf_token = parse_csrf_token(&form)?;
    let task_ids = parse_task_ids(&form)?;
    let tasks = parse_task_languages(&form, &task_ids)?;

    Ok(SubmitPage {
        form_action: format!("/contests/{expected_contest_id}/submit"),
        csrf_token: CsrfToken(csrf_token),
        tasks,
    })
}

fn validate_form(form: &ElementRef<'_>, expected_contest_id: &str) -> Result<(), SubmitPageError> {
    if !form
        .value()
        .attr("method")
        .is_some_and(|method| method.eq_ignore_ascii_case("post"))
    {
        return Err(SubmitPageError::MalformedPage(
            "submit form method is not POST",
        ));
    }

    let expected_action = format!("/contests/{expected_contest_id}/submit");
    if form.value().attr("action") != Some(expected_action.as_str()) {
        return Err(SubmitPageError::MalformedPage(
            "submit form action does not match the expected contest",
        ));
    }

    let source_selector = selector("textarea[name='sourceCode']");
    let mut source_controls = form.select(&source_selector);
    source_controls
        .next()
        .ok_or(SubmitPageError::MalformedPage(
            "sourceCode textarea is missing",
        ))?;
    if source_controls.next().is_some() {
        return Err(SubmitPageError::MalformedPage(
            "multiple sourceCode textareas found",
        ));
    }

    Ok(())
}

fn parse_csrf_token(form: &ElementRef<'_>) -> Result<String, SubmitPageError> {
    let csrf_selector = selector("input[name='csrf_token']");
    let mut inputs = form.select(&csrf_selector);
    let input = inputs
        .next()
        .ok_or(SubmitPageError::MalformedPage("CSRF token is missing"))?;
    if inputs.next().is_some() {
        return Err(SubmitPageError::MalformedPage("multiple CSRF tokens found"));
    }

    let value = input
        .value()
        .attr("value")
        .ok_or(SubmitPageError::MalformedPage("CSRF token is empty"))?;
    if value.trim().is_empty() {
        return Err(SubmitPageError::MalformedPage("CSRF token is empty"));
    }

    Ok(value.to_string())
}

fn parse_task_ids(form: &ElementRef<'_>) -> Result<Vec<String>, SubmitPageError> {
    let task_selector = selector("select[name='data.TaskScreenName']");
    let mut selectors = form.select(&task_selector);
    let tasks = selectors
        .next()
        .ok_or(SubmitPageError::MalformedPage("task selector is missing"))?;
    if selectors.next().is_some() {
        return Err(SubmitPageError::MalformedPage(
            "multiple task selectors found",
        ));
    }

    let option_selector = selector("option");
    let mut task_ids = Vec::new();
    let mut seen = BTreeSet::new();
    for option in tasks.select(&option_selector) {
        let task_id = option
            .value()
            .attr("value")
            .ok_or(SubmitPageError::MalformedPage("task ID is missing"))?;
        if !is_valid_identifier(task_id) {
            return Err(SubmitPageError::MalformedPage("task ID is invalid"));
        }
        if !seen.insert(task_id.to_string()) {
            return Err(SubmitPageError::MalformedPage("duplicate task ID"));
        }
        task_ids.push(task_id.to_string());
    }

    if task_ids.is_empty() {
        return Err(SubmitPageError::MalformedPage("task selector has no tasks"));
    }

    Ok(task_ids)
}

fn parse_task_languages(
    form: &ElementRef<'_>,
    task_ids: &[String],
) -> Result<BTreeMap<String, SubmitTask>, SubmitPageError> {
    let language_root_selector = selector("#select-lang");
    let mut roots = form.select(&language_root_selector);
    let root = roots.next().ok_or(SubmitPageError::MalformedPage(
        "language container is missing",
    ))?;
    if roots.next().is_some() {
        return Err(SubmitPageError::MalformedPage(
            "multiple language containers found",
        ));
    }
    if root.value().attr("data-name") != Some("data.LanguageId") {
        return Err(SubmitPageError::MalformedPage(
            "language container role is invalid",
        ));
    }

    let id_selector = selector("[id]");
    let select_selector = selector("select");
    let option_selector = selector("option");
    let mut tasks = BTreeMap::new();

    for task_id in task_ids {
        let expected_container_id = format!("select-lang-{task_id}");
        let mut containers = root
            .select(&id_selector)
            .filter(|element| element.value().attr("id") == Some(expected_container_id.as_str()));
        let container = containers.next().ok_or(SubmitPageError::MalformedPage(
            "task language container is missing",
        ))?;
        if containers.next().is_some() {
            return Err(SubmitPageError::MalformedPage(
                "duplicate task language container",
            ));
        }

        let mut language_selects = container.select(&select_selector);
        let language_select = language_selects
            .next()
            .ok_or(SubmitPageError::MalformedPage(
                "task language select is missing",
            ))?;
        if language_selects.next().is_some() {
            return Err(SubmitPageError::MalformedPage(
                "multiple task language selects found",
            ));
        }

        let mut languages = Vec::new();
        let mut language_ids = BTreeSet::new();
        for (option_index, option) in language_select.select(&option_selector).enumerate() {
            let label = normalized_text(&option);
            let id = option.value().attr("value");
            if is_language_placeholder(option_index, id, &label) {
                continue;
            }

            let id = id.ok_or(SubmitPageError::MalformedPage(
                "language option ID is missing",
            ))?;
            if id.is_empty() || id.trim() != id {
                return Err(SubmitPageError::MalformedPage(
                    "language option ID is empty or invalid",
                ));
            }
            if !language_ids.insert(id.to_string()) {
                return Err(SubmitPageError::MalformedPage(
                    "duplicate language option ID",
                ));
            }

            if label.is_empty() {
                return Err(SubmitPageError::MalformedPage(
                    "language option label is empty",
                ));
            }
            languages.push(SubmitLanguage {
                id: id.to_string(),
                label,
            });
        }

        tasks.insert(
            task_id.clone(),
            SubmitTask {
                task_id: task_id.clone(),
                languages,
            },
        );
    }

    Ok(tasks)
}

fn is_language_placeholder(option_index: usize, id: Option<&str>, label: &str) -> bool {
    option_index == 0 && label.is_empty() && id.is_none_or(str::is_empty)
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("static submit-page selector should be valid")
}

fn action_looks_like_submit(action: &str) -> bool {
    action
        .split(['?', '#'])
        .next()
        .is_some_and(|path| path.ends_with("/submit"))
}

fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn normalized_text(element: &ElementRef<'_>) -> String {
    element
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NumericVersion(Vec<u64>);

impl NumericVersion {
    fn parse(value: &str) -> Option<Self> {
        if value.is_empty() {
            return None;
        }

        let mut components = value
            .split('.')
            .map(|component| {
                if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
                    None
                } else {
                    component.parse::<u64>().ok()
                }
            })
            .collect::<Option<Vec<_>>>()?;

        while components.len() > 1 && components.last() == Some(&0) {
            components.pop();
        }

        Some(Self(components))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CppRank {
    standard: u64,
    gcc: NumericVersion,
}

enum RankClassification<R> {
    NotCandidate,
    Candidate(R),
    UnrankableCandidate,
}

enum RankedSelection<'a> {
    Selected(&'a SubmitLanguage),
    Unavailable,
    Ambiguous,
}

fn select_latest<'a, R: Ord>(
    languages: &'a [SubmitLanguage],
    classify: impl Fn(&str) -> RankClassification<R>,
) -> RankedSelection<'a> {
    let mut candidates = Vec::new();
    let mut has_unrankable_candidate = false;

    for language in languages {
        match classify(&language.label) {
            RankClassification::NotCandidate => {}
            RankClassification::Candidate(rank) => candidates.push((rank, language)),
            RankClassification::UnrankableCandidate => has_unrankable_candidate = true,
        }
    }

    if has_unrankable_candidate {
        return RankedSelection::Ambiguous;
    }

    let Some(max_rank) = candidates.iter().map(|(rank, _)| rank).max() else {
        return RankedSelection::Unavailable;
    };
    let mut newest = candidates
        .iter()
        .filter(|(rank, _)| rank == max_rank)
        .map(|(_, language)| *language);
    let selected = newest
        .next()
        .expect("a maximum rank requires at least one candidate");

    if newest.next().is_some() {
        RankedSelection::Ambiguous
    } else {
        RankedSelection::Selected(selected)
    }
}

fn classify_cpp(label: &str) -> RankClassification<CppRank> {
    if !label.starts_with("C++") {
        return RankClassification::NotCandidate;
    }

    let lowercase = label.to_ascii_lowercase();
    if lowercase.contains("ioi-style") || lowercase.contains("ioi style") {
        return RankClassification::NotCandidate;
    }
    if !lowercase.contains("gcc") {
        return RankClassification::NotCandidate;
    }

    let Some(rest) = label.strip_prefix("C++") else {
        return RankClassification::UnrankableCandidate;
    };
    let Some((standard, gcc)) = rest.split_once(" (GCC ") else {
        return RankClassification::UnrankableCandidate;
    };
    let Some(gcc) = gcc.strip_suffix(')') else {
        return RankClassification::UnrankableCandidate;
    };
    let Some(standard) = parse_ascii_u64(standard) else {
        return RankClassification::UnrankableCandidate;
    };
    let Some(gcc) = NumericVersion::parse(gcc) else {
        return RankClassification::UnrankableCandidate;
    };

    RankClassification::Candidate(CppRank { standard, gcc })
}

fn classify_python(label: &str) -> RankClassification<NumericVersion> {
    if !label.starts_with("Python") {
        return RankClassification::NotCandidate;
    }
    if !label.contains("CPython") {
        return RankClassification::NotCandidate;
    }

    let Some(version) = label
        .strip_prefix("Python (CPython ")
        .and_then(|version| version.strip_suffix(')'))
        .and_then(NumericVersion::parse)
    else {
        return RankClassification::UnrankableCandidate;
    };

    RankClassification::Candidate(version)
}

fn parse_ascii_u64(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        None
    } else {
        value.parse().ok()
    }
}

// ============================================================
// Submit Backend
// ============================================================

pub(crate) struct SubmitRequest {
    contest_id: String,
    task_id: String,
    language: Language,
    source: String,
}

impl SubmitRequest {
    pub(crate) fn new(
        contest_id: String,
        task_id: String,
        language: Language,
        source: String,
    ) -> Self {
        Self {
            contest_id,
            task_id,
            language,
            source,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_parts(&self) -> (&str, &str, Language, &str) {
        (&self.contest_id, &self.task_id, self.language, &self.source)
    }
}

impl fmt::Debug for SubmitRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubmitRequest")
            .field("contest_id", &self.contest_id)
            .field("task_id", &self.task_id)
            .field("language", &self.language)
            .field("source", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitOutcome {
    Accepted,
    UnknownSubmissionOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubmitError {
    InvalidRequestIdentity { kind: &'static str },
    AuthenticationRequired,
    SubmitUnavailable,
    SubmitClientInitializationFailed,
    SubmitPage(SubmitPageError),
    SubmitPageFetchFailed,
    SubmissionRejected,
    UnexpectedRedirect,
    RateLimited,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequestIdentity { kind } => {
                write!(formatter, "invalid submit request {kind}")
            }
            Self::AuthenticationRequired => {
                formatter.write_str("AtCoder authentication is required")
            }
            Self::SubmitUnavailable => formatter.write_str("AtCoder submit is unavailable"),
            Self::SubmitClientInitializationFailed => {
                formatter.write_str("failed to initialize the AtCoder submit client")
            }
            Self::SubmitPage(error) => error.fmt(formatter),
            Self::SubmitPageFetchFailed => {
                formatter.write_str("failed to fetch the AtCoder submit page")
            }
            Self::SubmissionRejected => formatter.write_str("AtCoder rejected the submission"),
            Self::UnexpectedRedirect => {
                formatter.write_str("AtCoder returned an unexpected submit redirect")
            }
            Self::RateLimited => formatter.write_str("AtCoder submit was rate limited"),
        }
    }
}

impl std::error::Error for SubmitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SubmitPage(error) => Some(error),
            Self::InvalidRequestIdentity { .. }
            | Self::AuthenticationRequired
            | Self::SubmitUnavailable
            | Self::SubmitClientInitializationFailed
            | Self::SubmitPageFetchFailed
            | Self::SubmissionRejected
            | Self::UnexpectedRedirect
            | Self::RateLimited => None,
        }
    }
}

impl AtCoderClient {
    pub(crate) fn submit(&self, request: SubmitRequest) -> Result<SubmitOutcome, SubmitError> {
        match &self.source {
            Source::Http(http) => {
                let mut transport = HttpSubmitTransport { http };
                submit_with_transport(&mut transport, request)
            }
            Source::Fixture(_) => Err(SubmitError::SubmitUnavailable),
        }
    }
}

struct SubmitForm<'a> {
    fields: [(&'static str, &'a str); 4],
}

impl<'a> SubmitForm<'a> {
    fn new(request: &'a SubmitRequest, language_id: &'a str, csrf_token: &'a str) -> Self {
        Self {
            fields: [
                ("data.TaskScreenName", &request.task_id),
                ("data.LanguageId", language_id),
                ("sourceCode", &request.source),
                ("csrf_token", csrf_token),
            ],
        }
    }

    fn as_pairs(&self) -> &[(&'static str, &'a str); 4] {
        &self.fields
    }
}

struct SubmitHttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
    final_url: Option<reqwest::Url>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitTransportFailure {
    ClientInitialization,
    Timeout,
    ConnectionReset,
    Other,
}

trait SubmitTransport {
    fn get_submit_page(&mut self, path: &str)
    -> Result<SubmitHttpResponse, SubmitTransportFailure>;

    fn post_submit_form(
        &mut self,
        path: &str,
        form: &SubmitForm<'_>,
    ) -> Result<SubmitHttpResponse, SubmitTransportFailure>;

    fn wait_before_get_retry(&mut self, duration: Duration);
}

struct HttpSubmitTransport<'a> {
    http: &'a HttpSource,
}

impl HttpSubmitTransport<'_> {
    fn submit_page_client(&self) -> &Client {
        &self.http.client
    }

    fn submit_post_client(&self) -> Result<Client, SubmitTransportFailure> {
        self.http
            .submit_client
            .get()
            .map_err(|_| SubmitTransportFailure::ClientInitialization)
    }
}

impl SubmitTransport for HttpSubmitTransport<'_> {
    fn get_submit_page(
        &mut self,
        path: &str,
    ) -> Result<SubmitHttpResponse, SubmitTransportFailure> {
        wait_for_request_slot(self.http);
        let response = self
            .submit_page_client()
            .get(format!("{BASE_URL}{path}"))
            .send()
            .map_err(classify_transport_failure)?;

        collect_submit_page_response(response, path)
    }

    fn post_submit_form(
        &mut self,
        path: &str,
        form: &SubmitForm<'_>,
    ) -> Result<SubmitHttpResponse, SubmitTransportFailure> {
        wait_for_request_slot(self.http);
        let response = self
            .submit_post_client()?
            .post(format!("{BASE_URL}{path}"))
            .form(form.as_pairs())
            .send()
            .map_err(classify_transport_failure)?;

        Ok(collect_submit_response_without_body(response))
    }

    fn wait_before_get_retry(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

fn collect_submit_page_response(
    response: reqwest::blocking::Response,
    expected_path: &str,
) -> Result<SubmitHttpResponse, SubmitTransportFailure> {
    let status = response.status();
    let headers = response.headers().clone();
    let final_url = response.url().clone();
    let body = if status == StatusCode::OK && is_expected_submit_page_url(&final_url, expected_path)
    {
        response.text().map_err(classify_transport_failure)?
    } else {
        String::new()
    };

    Ok(SubmitHttpResponse {
        status,
        headers,
        body,
        final_url: Some(final_url),
    })
}

fn collect_submit_response_without_body(
    response: reqwest::blocking::Response,
) -> SubmitHttpResponse {
    SubmitHttpResponse {
        status: response.status(),
        headers: response.headers().clone(),
        body: String::new(),
        final_url: None,
    }
}

fn classify_transport_failure(error: reqwest::Error) -> SubmitTransportFailure {
    if error.is_timeout() {
        SubmitTransportFailure::Timeout
    } else if error.is_connect() {
        SubmitTransportFailure::ConnectionReset
    } else {
        SubmitTransportFailure::Other
    }
}

fn submit_with_transport(
    transport: &mut impl SubmitTransport,
    request: SubmitRequest,
) -> Result<SubmitOutcome, SubmitError> {
    if !is_valid_identifier(&request.contest_id) {
        return Err(SubmitError::InvalidRequestIdentity { kind: "contest ID" });
    }
    if !is_valid_identifier(&request.task_id) {
        return Err(SubmitError::InvalidRequestIdentity { kind: "task ID" });
    }

    let submit_path = format!("/contests/{}/submit", request.contest_id);
    let response = fetch_fresh_submit_page(transport, &submit_path)?;
    let page = parse_submit_page(&request.contest_id, &response.body).map_err(|error| {
        if error == SubmitPageError::SubmitUnavailable {
            SubmitError::SubmitUnavailable
        } else {
            SubmitError::SubmitPage(error)
        }
    })?;
    let language = page
        .resolve_language(&request.task_id, request.language)
        .map_err(SubmitError::SubmitPage)?;
    let form = SubmitForm::new(&request, language.id(), page.csrf_token());

    let response = match transport.post_submit_form(page.form_action(), &form) {
        Ok(response) => response,
        Err(SubmitTransportFailure::ClientInitialization) => {
            return Err(SubmitError::SubmitClientInitializationFailed);
        }
        Err(_) => return Ok(SubmitOutcome::UnknownSubmissionOutcome),
    };

    classify_submit_response(&request.contest_id, response)
}

fn fetch_fresh_submit_page(
    transport: &mut impl SubmitTransport,
    submit_path: &str,
) -> Result<SubmitHttpResponse, SubmitError> {
    for retry_count in 0..=MAX_429_RETRIES {
        let response = transport
            .get_submit_page(submit_path)
            .map_err(|_| SubmitError::SubmitPageFetchFailed)?;

        if let Some(final_url) = response.final_url.as_ref() {
            if is_login_url(final_url) {
                return Err(SubmitError::AuthenticationRequired);
            }
            if !is_expected_submit_page_url(final_url, submit_path) {
                return Err(SubmitError::SubmitUnavailable);
            }
        }

        if response.status == StatusCode::TOO_MANY_REQUESTS {
            if retry_count == MAX_429_RETRIES {
                return Err(SubmitError::RateLimited);
            }
            transport.wait_before_get_retry(retry_wait(&response.headers));
            continue;
        }

        if is_login_redirect(response.status, &response.headers) {
            return Err(SubmitError::AuthenticationRequired);
        }
        if response.status != StatusCode::OK {
            return Err(SubmitError::SubmitUnavailable);
        }

        return Ok(response);
    }

    Err(SubmitError::RateLimited)
}

fn classify_submit_response(
    contest_id: &str,
    response: SubmitHttpResponse,
) -> Result<SubmitOutcome, SubmitError> {
    if response.status == StatusCode::TOO_MANY_REQUESTS {
        return Err(SubmitError::RateLimited);
    }
    if response.status.is_server_error() {
        return Ok(SubmitOutcome::UnknownSubmissionOutcome);
    }
    if is_login_redirect(response.status, &response.headers) {
        return Err(SubmitError::AuthenticationRequired);
    }
    if response.status == StatusCode::OK {
        return Err(SubmitError::SubmissionRejected);
    }
    if response.status.is_success() {
        return Ok(SubmitOutcome::UnknownSubmissionOutcome);
    }
    if matches!(response.status, StatusCode::FOUND | StatusCode::SEE_OTHER) {
        return if unique_location(&response.headers)
            .is_some_and(|location| is_expected_submission_location(contest_id, location))
        {
            Ok(SubmitOutcome::Accepted)
        } else {
            Err(SubmitError::UnexpectedRedirect)
        };
    }
    if response.status.is_redirection() {
        return Err(SubmitError::UnexpectedRedirect);
    }

    Err(SubmitError::SubmissionRejected)
}

fn is_login_redirect(status: StatusCode, headers: &HeaderMap) -> bool {
    status.is_redirection()
        && unique_location(headers)
            .and_then(parse_atcoder_location)
            .is_some_and(|url| is_login_url(&url))
}

fn is_login_url(url: &reqwest::Url) -> bool {
    is_same_atcoder_origin(url) && url.path() == "/login" && url.fragment().is_none()
}

fn is_expected_submit_page_url(url: &reqwest::Url, expected_path: &str) -> bool {
    url.as_str() == format!("{BASE_URL}{expected_path}")
}

fn unique_location(headers: &HeaderMap) -> Option<&str> {
    let mut locations = headers.get_all(LOCATION).iter();
    let location = locations.next()?;
    if locations.next().is_some() {
        return None;
    }
    location.to_str().ok()
}

fn is_expected_submission_location(contest_id: &str, location: &str) -> bool {
    let expected_path = format!("/contests/{contest_id}/submissions/me");
    let expected_absolute = format!("{BASE_URL}{expected_path}");

    location == expected_path || location == expected_absolute
}

fn parse_atcoder_location(location: &str) -> Option<reqwest::Url> {
    if location.is_empty() || location.trim() != location || location.starts_with("//") {
        return None;
    }

    if location.starts_with('/') {
        reqwest::Url::parse(BASE_URL).ok()?.join(location).ok()
    } else {
        reqwest::Url::parse(location).ok()
    }
}

fn is_same_atcoder_origin(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("atcoder.jp")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderValue, RETRY_AFTER};
    use std::collections::VecDeque;

    const CURRENT_SUBMIT_PAGE: &str = include_str!("../../fixtures/submit/abc466.html");

    fn current_page() -> SubmitPage {
        parse_submit_page("abc466", CURRENT_SUBMIT_PAGE)
            .expect("current submit-page fixture should parse")
    }

    fn replace_once(html: &str, from: &str, to: &str) -> String {
        assert_eq!(
            html.matches(from).count(),
            1,
            "fixture mutation must be unique"
        );
        html.replacen(from, to, 1)
    }

    fn malformed(html: &str) -> SubmitPageError {
        parse_submit_page("abc466", html).expect_err("submit page should be rejected")
    }

    fn submit_page_with_language_options(options: &str) -> String {
        format!(
            r#"<!doctype html>
<html><body>
  <form method="POST" action="/contests/abc466/submit">
    <input type="hidden" name="csrf_token" value="dummy-submit-csrf-token">
    <select name="data.TaskScreenName">
      <option value="abc466_a">A</option>
    </select>
    <div id="select-lang" data-name="data.LanguageId">
      <div id="select-lang-abc466_a">
        <select>{options}</select>
      </div>
    </div>
    <textarea name="sourceCode"></textarea>
  </form>
</body></html>"#
        )
    }

    fn page_with_languages(languages: &[(&str, &str)]) -> SubmitPage {
        let task_id = "contest_task".to_string();
        let languages = languages
            .iter()
            .map(|(id, label)| SubmitLanguage {
                id: (*id).to_string(),
                label: (*label).to_string(),
            })
            .collect();
        let task = SubmitTask {
            task_id: task_id.clone(),
            languages,
        };

        SubmitPage {
            form_action: "/contests/contest/submit".to_string(),
            csrf_token: CsrfToken("dummy-unit-test-token".to_string()),
            tasks: BTreeMap::from([(task_id, task)]),
        }
    }

    struct ExpectedSubmitForm {
        task_id: String,
        language_id: String,
        source: String,
        csrf_token: String,
    }

    impl ExpectedSubmitForm {
        fn matches(&self, form: &SubmitForm<'_>) {
            let fields = form.as_pairs();
            let expected = [
                ("data.TaskScreenName", self.task_id.as_str()),
                ("data.LanguageId", self.language_id.as_str()),
                ("sourceCode", self.source.as_str()),
                ("csrf_token", self.csrf_token.as_str()),
            ];

            for (actual, expected) in fields.iter().zip(expected) {
                assert_eq!(actual.0, expected.0, "scripted POST field name mismatch");
                assert!(actual.1 == expected.1, "scripted POST field value mismatch");
            }
        }
    }

    enum ScriptStep {
        Get {
            path: String,
            result: Result<SubmitHttpResponse, SubmitTransportFailure>,
        },
        Post {
            path: String,
            form: ExpectedSubmitForm,
            result: Result<SubmitHttpResponse, SubmitTransportFailure>,
        },
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ObservedRequest {
        method: &'static str,
        path: String,
    }

    struct ScriptedSubmitTransport {
        steps: VecDeque<ScriptStep>,
        requests: Vec<ObservedRequest>,
        get_retry_waits: usize,
    }

    impl ScriptedSubmitTransport {
        fn new(steps: Vec<ScriptStep>) -> Self {
            Self {
                steps: steps.into(),
                requests: Vec::new(),
                get_retry_waits: 0,
            }
        }

        fn get_count(&self) -> usize {
            self.requests
                .iter()
                .filter(|request| request.method == "GET")
                .count()
        }

        fn post_count(&self) -> usize {
            self.requests
                .iter()
                .filter(|request| request.method == "POST")
                .count()
        }

        fn assert_complete(&self) {
            assert!(self.steps.is_empty(), "scripted transport has unused steps");
        }
    }

    impl SubmitTransport for ScriptedSubmitTransport {
        fn get_submit_page(
            &mut self,
            path: &str,
        ) -> Result<SubmitHttpResponse, SubmitTransportFailure> {
            self.requests.push(ObservedRequest {
                method: "GET",
                path: path.to_string(),
            });
            let step = self
                .steps
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected scripted GET"));
            let ScriptStep::Get {
                path: expected_path,
                result,
            } = step
            else {
                panic!("script expected POST but backend sent GET");
            };
            assert_eq!(path, expected_path, "scripted GET path mismatch");
            result
        }

        fn post_submit_form(
            &mut self,
            path: &str,
            form: &SubmitForm<'_>,
        ) -> Result<SubmitHttpResponse, SubmitTransportFailure> {
            self.requests.push(ObservedRequest {
                method: "POST",
                path: path.to_string(),
            });
            let step = self
                .steps
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected scripted POST"));
            let ScriptStep::Post {
                path: expected_path,
                form: expected_form,
                result,
            } = step
            else {
                panic!("script expected GET but backend sent POST");
            };
            assert_eq!(path, expected_path, "scripted POST path mismatch");
            expected_form.matches(form);
            result
        }

        fn wait_before_get_retry(&mut self, _duration: Duration) {
            self.get_retry_waits += 1;
        }
    }

    fn submit_request(language: Language, source: &str) -> SubmitRequest {
        SubmitRequest::new(
            "abc466".to_string(),
            "abc466_a".to_string(),
            language,
            source.to_string(),
        )
    }

    fn expected_form(language_id: &str, source: &str) -> ExpectedSubmitForm {
        ExpectedSubmitForm {
            task_id: "abc466_a".to_string(),
            language_id: language_id.to_string(),
            source: source.to_string(),
            csrf_token: "dummy-submit-csrf-token".to_string(),
        }
    }

    fn response(status: StatusCode, location: Option<&str>, body: &str) -> SubmitHttpResponse {
        let mut headers = HeaderMap::new();
        if let Some(location) = location {
            headers.insert(
                LOCATION,
                HeaderValue::from_str(location).expect("test Location should be valid"),
            );
        }
        SubmitHttpResponse {
            status,
            headers,
            body: body.to_string(),
            final_url: None,
        }
    }

    fn submit_page_response(html: &str) -> SubmitHttpResponse {
        response(StatusCode::OK, None, html)
    }

    fn accepted_response(status: StatusCode, location: &str) -> SubmitHttpResponse {
        response(status, Some(location), "dummy redirect body")
    }

    fn get_step(html: &str) -> ScriptStep {
        ScriptStep::Get {
            path: "/contests/abc466/submit".to_string(),
            result: Ok(submit_page_response(html)),
        }
    }

    fn post_step(
        language_id: &str,
        source: &str,
        result: Result<SubmitHttpResponse, SubmitTransportFailure>,
    ) -> ScriptStep {
        ScriptStep::Post {
            path: "/contests/abc466/submit".to_string(),
            form: expected_form(language_id, source),
            result,
        }
    }

    #[test]
    fn current_live_style_dom_with_placeholders_parses() {
        let page = current_page();

        assert_eq!(page.tasks().len(), 2);
        assert!(!page.csrf_token().is_empty());
    }

    #[test]
    fn language_select_without_name_parses_via_parent_role() {
        assert!(
            CURRENT_SUBMIT_PAGE.contains("<div id=\"select-lang\" data-name=\"data.LanguageId\">")
        );
        assert!(!CURRENT_SUBMIT_PAGE.contains("<select name=\"data.LanguageId\""));
        assert_eq!(CURRENT_SUBMIT_PAGE.matches("<option></option>").count(), 2);

        let page = current_page();
        // The value-less blank placeholder is not a SubmitLanguage.
        assert_eq!(page.tasks().next().unwrap().languages().len(), 8);
    }

    #[test]
    fn multiple_tasks_are_mapped_by_task_id() {
        let page = current_page();
        let tasks = page.tasks().map(|task| task.task_id()).collect::<Vec<_>>();

        assert_eq!(tasks, ["abc466_a", "abc466_b"]);
    }

    #[test]
    fn parsed_language_sets_remain_task_local() {
        let page = current_page();
        let task_a = page.tasks.get("abc466_a").unwrap();
        let task_b = page.tasks.get("abc466_b").unwrap();

        assert!(
            task_a
                .languages()
                .iter()
                .any(|language| language.label() == "C++23 (GCC 15.2.0)")
        );
        assert!(
            task_b
                .languages()
                .iter()
                .all(|language| language.label() != "C++23 (GCC 15.2.0)")
        );
    }

    #[test]
    fn canonical_form_action_is_preserved() {
        assert_eq!(current_page().form_action(), "/contests/abc466/submit");
    }

    #[test]
    fn wrong_contest_action_is_rejected() {
        let error = parse_submit_page("abc999", CURRENT_SUBMIT_PAGE)
            .expect_err("another contest action must be rejected");

        assert!(matches!(error, SubmitPageError::MalformedPage(_)));
    }

    #[test]
    fn external_submit_action_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "action=\"/contests/abc466/submit\"",
            "action=\"https://evil.example/contests/abc466/submit\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn scheme_relative_submit_action_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "action=\"/contests/abc466/submit\"",
            "action=\"//evil.example/contests/abc466/submit\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn canonical_looking_action_with_query_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "action=\"/contests/abc466/submit\"",
            "action=\"/contests/abc466/submit?x=1\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn canonical_looking_action_with_fragment_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "action=\"/contests/abc466/submit\"",
            "action=\"/contests/abc466/submit#x\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn structurally_identifiable_form_with_non_submit_action_is_malformed() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "action=\"/contests/abc466/submit\"",
            "action=\"/contests/abc466/tasks\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn missing_submit_form_is_reported_as_unavailable() {
        let error = parse_submit_page("abc466", "<html><body></body></html>")
            .expect_err("missing submit form must be rejected");

        assert_eq!(error, SubmitPageError::SubmitUnavailable);
    }

    #[test]
    fn duplicate_submit_form_is_rejected() {
        let html = format!(
            "{CURRENT_SUBMIT_PAGE}<form method=\"POST\" action=\"/contests/abc466/submit\"></form>"
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn non_post_submit_form_is_rejected() {
        let html = replace_once(CURRENT_SUBMIT_PAGE, "method=\"POST\"", "method=\"GET\"");

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn current_fixture_has_exactly_one_valid_source_code_textarea() {
        let page = current_page();

        assert_eq!(page.tasks().len(), 2);
    }

    #[test]
    fn missing_source_code_textarea_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "    <textarea id=\"plain-textarea\" name=\"sourceCode\"></textarea>\n",
            "",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn duplicate_source_code_textarea_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "    <textarea id=\"plain-textarea\" name=\"sourceCode\"></textarea>",
            "    <textarea id=\"plain-textarea\" name=\"sourceCode\"></textarea>\n    <textarea name=\"sourceCode\"></textarea>",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn input_source_code_control_does_not_satisfy_textarea_contract() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "<textarea id=\"plain-textarea\" name=\"sourceCode\"></textarea>",
            "<input name=\"sourceCode\" value=\"not-read\">",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn missing_csrf_token_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "    <input type=\"hidden\" name=\"csrf_token\" value=\"dummy-submit-csrf-token\">\n",
            "",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn duplicate_csrf_token_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "    <select id=\"select-task\"",
            "    <input type=\"hidden\" name=\"csrf_token\" value=\"another-dummy\">\n    <select id=\"select-task\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn empty_csrf_token_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "value=\"dummy-submit-csrf-token\"",
            "value=\"\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn duplicate_task_id_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "      <option value=\"abc466_b\">B - Unfortunate 2</option>",
            "      <option value=\"abc466_a\">B - Unfortunate 2</option>",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn empty_task_id_is_rejected() {
        let html = replace_once(CURRENT_SUBMIT_PAGE, "value=\"abc466_b\"", "value=\"\"");

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn missing_task_selector_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "name=\"data.TaskScreenName\"",
            "name=\"not-the-task-field\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn missing_task_language_container_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "id=\"select-lang-abc466_b\"",
            "id=\"select-lang-unrelated_task\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn invalid_language_container_role_is_rejected() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "data-name=\"data.LanguageId\"",
            "data-name=\"unexpected\"",
        );

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn malformed_language_option_is_rejected() {
        let html = replace_once(CURRENT_SUBMIT_PAGE, "value=\"5001\"", "value=\"\"");

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn placeholder_only_has_no_language_and_resolution_fails_closed() {
        let html = submit_page_with_language_options("<option></option>");
        let page = parse_submit_page("abc466", &html).unwrap();
        let task = page.tasks().next().unwrap();

        assert!(task.languages().is_empty());
        for language in Language::ALL {
            assert!(matches!(
                page.resolve_language("abc466_a", language),
                Err(SubmitPageError::LanguageUnavailable {
                    task_id,
                    language: unavailable,
                }) if task_id == "abc466_a" && unavailable == language
            ));
        }
    }

    #[test]
    fn blank_first_empty_value_placeholder_is_ignored() {
        let html = submit_page_with_language_options(concat!(
            "<option value=\"\"></option>",
            "<option value=\"6017\">C++23 (GCC 15.2.0)</option>"
        ));
        let page = parse_submit_page("abc466", &html).unwrap();
        let selected = page.resolve_language("abc466_a", Language::Cpp).unwrap();

        assert_eq!(page.tasks().next().unwrap().languages().len(), 1);
        assert_eq!(selected.id(), "6017");
    }

    #[test]
    fn real_language_candidate_without_an_id_is_still_malformed() {
        let html = submit_page_with_language_options(concat!(
            "<option></option>",
            "<option>C++23 (GCC 15.2.0)</option>"
        ));

        assert_eq!(
            malformed(&html),
            SubmitPageError::MalformedPage("language option ID is missing")
        );
    }

    #[test]
    fn value_less_blank_option_after_the_placeholder_is_malformed() {
        let html = submit_page_with_language_options(concat!(
            "<option></option>",
            "<option></option>",
            "<option value=\"6017\">C++23 (GCC 15.2.0)</option>"
        ));

        assert_eq!(
            malformed(&html),
            SubmitPageError::MalformedPage("language option ID is missing")
        );
    }

    #[test]
    fn duplicate_language_id_within_a_task_is_rejected() {
        let html = replace_once(CURRENT_SUBMIT_PAGE, "value=\"6017\"", "value=\"5001\"");

        assert!(matches!(
            malformed(&html),
            SubmitPageError::MalformedPage(_)
        ));
    }

    #[test]
    fn latest_normal_gcc_cpp_is_selected() {
        let page = current_page();
        let selected = page.resolve_language("abc466_a", Language::Cpp).unwrap();

        assert_eq!(selected.label(), "C++23 (GCC 15.2.0)");
        assert_eq!(selected.id(), "6017");
    }

    #[test]
    fn clang_cpp_is_ignored() {
        let page = page_with_languages(&[
            ("gcc", "C++20 (GCC 12.2.0)"),
            ("clang", "C++26 (Clang 22.0.0)"),
        ]);

        assert_eq!(
            page.resolve_language("contest_task", Language::Cpp)
                .unwrap()
                .id(),
            "gcc"
        );
    }

    #[test]
    fn ioi_style_gcc_cpp_is_ignored() {
        let page = page_with_languages(&[
            ("normal", "C++20 (GCC 12.2.0)"),
            ("ioi", "C++26 (GCC 20.1.0) [IOI-Style]"),
        ]);

        assert_eq!(
            page.resolve_language("contest_task", Language::Cpp)
                .unwrap()
                .id(),
            "normal"
        );
    }

    #[test]
    fn cpp23_ranks_above_cpp20() {
        let page = page_with_languages(&[
            ("cpp20", "C++20 (GCC 99.0.0)"),
            ("cpp23", "C++23 (GCC 10.0.0)"),
        ]);

        assert_eq!(
            page.resolve_language("contest_task", Language::Cpp)
                .unwrap()
                .id(),
            "cpp23"
        );
    }

    #[test]
    fn gcc15_ranks_above_gcc14() {
        let page = page_with_languages(&[
            ("gcc14", "C++23 (GCC 14.9.0)"),
            ("gcc15", "C++23 (GCC 15.0.0)"),
        ]);

        assert_eq!(
            page.resolve_language("contest_task", Language::Cpp)
                .unwrap()
                .id(),
            "gcc15"
        );
    }

    #[test]
    fn gcc_patch_versions_are_compared_numerically() {
        let page = page_with_languages(&[
            ("older", "C++23 (GCC 15.2.9)"),
            ("newer", "C++23 (GCC 15.2.10)"),
        ]);

        assert_eq!(
            page.resolve_language("contest_task", Language::Cpp)
                .unwrap()
                .id(),
            "newer"
        );
    }

    #[test]
    fn cpp_resolution_uses_only_the_requested_task() {
        let page = current_page();

        assert_eq!(
            page.resolve_language("abc466_b", Language::Cpp)
                .unwrap()
                .id(),
            "b-cpp20"
        );
    }

    #[test]
    fn no_normal_gcc_candidate_is_an_error() {
        let page = page_with_languages(&[
            ("clang", "C++23 (Clang 20.1.8)"),
            ("ioi", "C++23 (GCC 15.2.0) [IOI-Style]"),
        ]);

        assert!(matches!(
            page.resolve_language("contest_task", Language::Cpp),
            Err(SubmitPageError::LanguageUnavailable { .. })
        ));
    }

    #[test]
    fn tied_latest_gcc_candidates_are_ambiguous() {
        let page =
            page_with_languages(&[("one", "C++23 (GCC 15.2)"), ("two", "C++23 (GCC 15.2.0)")]);

        assert!(matches!(
            page.resolve_language("contest_task", Language::Cpp),
            Err(SubmitPageError::LanguageAmbiguous { .. })
        ));
    }

    #[test]
    fn latest_cpython_is_selected() {
        let page = current_page();
        let selected = page.resolve_language("abc466_a", Language::Python).unwrap();

        assert_eq!(selected.label(), "Python (CPython 3.13.7)");
        assert_eq!(selected.id(), "6082");
    }

    #[test]
    fn pypy_is_ignored() {
        let page = page_with_languages(&[
            ("cpython", "Python (CPython 3.10.1)"),
            ("pypy", "Python (PyPy 9.99.0)"),
        ]);

        assert_eq!(
            page.resolve_language("contest_task", Language::Python)
                .unwrap()
                .id(),
            "cpython"
        );
    }

    #[test]
    fn codon_is_ignored() {
        let page = page_with_languages(&[
            ("cpython", "Python (CPython 3.10.1)"),
            ("codon", "Python (Codon 99.0.0)"),
        ]);

        assert_eq!(
            page.resolve_language("contest_task", Language::Python)
                .unwrap()
                .id(),
            "cpython"
        );
    }

    #[test]
    fn python_3_13_ranks_above_3_9_numerically() {
        let page = page_with_languages(&[
            ("old", "Python (CPython 3.9.99)"),
            ("new", "Python (CPython 3.13.0)"),
        ]);

        assert_eq!(
            page.resolve_language("contest_task", Language::Python)
                .unwrap()
                .id(),
            "new"
        );
    }

    #[test]
    fn no_cpython_candidate_is_an_error() {
        let page = page_with_languages(&[
            ("pypy", "Python (PyPy 3.11-v7.3.19)"),
            ("codon", "Python (Codon 0.19.3)"),
        ]);

        assert!(matches!(
            page.resolve_language("contest_task", Language::Python),
            Err(SubmitPageError::LanguageUnavailable { .. })
        ));
    }

    #[test]
    fn tied_latest_cpython_candidates_are_ambiguous() {
        let page = page_with_languages(&[
            ("one", "Python (CPython 3.13)"),
            ("two", "Python (CPython 3.13.0)"),
        ]);

        assert!(matches!(
            page.resolve_language("contest_task", Language::Python),
            Err(SubmitPageError::LanguageAmbiguous { .. })
        ));
    }

    #[test]
    fn unknown_task_is_an_explicit_error() {
        let page = current_page();

        assert!(matches!(
            page.resolve_language("abc466_missing", Language::Cpp),
            Err(SubmitPageError::TaskUnavailable { task_id }) if task_id == "abc466_missing"
        ));
    }

    #[test]
    fn language_id_is_parsed_and_returned_as_an_opaque_string() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "value=\"6017\"",
            "value=\"gcc-opaque-id\"",
        );
        let page = parse_submit_page("abc466", &html).unwrap();

        assert_eq!(
            page.resolve_language("abc466_a", Language::Cpp)
                .unwrap()
                .id(),
            "gcc-opaque-id"
        );
    }

    #[test]
    fn unrankable_relevant_label_fails_closed() {
        let page = page_with_languages(&[
            ("known", "C++23 (GCC 15.2.0)"),
            ("unknown", "C++next (GCC rolling)"),
        ]);

        assert!(matches!(
            page.resolve_language("contest_task", Language::Cpp),
            Err(SubmitPageError::LanguageAmbiguous { .. })
        ));
    }

    #[test]
    fn malformed_future_gcc_candidate_prevents_selecting_an_older_gcc() {
        let page = page_with_languages(&[
            ("known", "C++23 (GCC 15.2.0)"),
            ("future", "C++26 (GCC ???)"),
        ]);

        assert!(matches!(
            page.resolve_language("contest_task", Language::Cpp),
            Err(SubmitPageError::LanguageAmbiguous { .. })
        ));
    }

    #[test]
    fn malformed_future_cpython_candidate_prevents_selecting_an_older_cpython() {
        let page = page_with_languages(&[
            ("known", "Python (CPython 3.13.7)"),
            ("future", "Python (CPython ???)"),
        ]);

        assert!(matches!(
            page.resolve_language("contest_task", Language::Python),
            Err(SubmitPageError::LanguageAmbiguous { .. })
        ));
    }

    #[test]
    fn current_style_ioi_language_is_not_a_normal_gcc_candidate() {
        let page = page_with_languages(&[("ioi", "C++ IOI-Style(GNU++20) (GCC 14.2.0)")]);

        assert!(matches!(
            page.resolve_language("contest_task", Language::Cpp),
            Err(SubmitPageError::LanguageUnavailable { .. })
        ));
    }

    #[test]
    fn submit_page_debug_redacts_csrf_token() {
        let page = current_page();
        let debug = format!("{page:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(page.csrf_token()));
    }

    #[test]
    fn errors_do_not_contain_csrf_or_html() {
        let secret = current_page().csrf_token().to_string();
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "data-name=\"data.LanguageId\"",
            "data-name=\"broken\"",
        );
        let error = malformed(&html);
        let display = error.to_string();
        let debug = format!("{error:?}");

        assert!(!display.contains(&secret));
        assert!(!debug.contains(&secret));
        assert!(!display.contains("<!doctype html>"));
        assert!(!debug.contains("<!doctype html>"));
    }

    #[test]
    fn production_transport_separates_normal_get_and_one_shot_post_clients() {
        let http = super::super::HttpSource::new(None)
            .expect("normal HTTP source should construct without making a request");
        let transport = HttpSubmitTransport { http: &http };

        assert!(std::ptr::eq(transport.submit_page_client(), &http.client));
        assert!(
            http.submit_client
                .client
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        );

        let post_client = transport
            .submit_post_client()
            .expect("POST client should initialize without making a request");

        assert!(!std::ptr::eq(transport.submit_page_client(), &post_client));
        assert!(
            http.submit_client
                .client
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        );
    }

    #[test]
    fn backend_gets_fresh_page_and_posts_exact_cpp_form_once() {
        let source = "dummy cpp source\n";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(accepted_response(
                    StatusCode::FOUND,
                    "/contests/abc466/submissions/me",
                )),
            ),
        ]);

        let outcome =
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source)).unwrap();

        assert_eq!(outcome, SubmitOutcome::Accepted);
        assert_eq!(transport.get_count(), 1);
        assert_eq!(transport.post_count(), 1);
        assert_eq!(
            transport.requests,
            [
                ObservedRequest {
                    method: "GET",
                    path: "/contests/abc466/submit".to_string(),
                },
                ObservedRequest {
                    method: "POST",
                    path: "/contests/abc466/submit".to_string(),
                },
            ]
        );
        transport.assert_complete();
    }

    #[test]
    fn adt_submit_keeps_contest_and_stable_task_identities_independent() {
        let contest_id = "adt_easy_20260826_1";
        let task_id = "abc430_a";
        let other_task_id = "abc343_a";
        let source = "dummy snapshot";
        let submit_path = "/contests/adt_easy_20260826_1/submit";
        let html = CURRENT_SUBMIT_PAGE
            .replace("/contests/abc466/submit", submit_path)
            .replace("abc466_a", other_task_id)
            .replace("abc466_b", task_id);
        assert!(html.contains(task_id));
        assert!(html.contains(other_task_id));
        assert!(!html.contains("adt_easy_20260826_1_a"));

        let expected_form = ExpectedSubmitForm {
            task_id: task_id.to_string(),
            language_id: "b-cpp20".to_string(),
            source: source.to_string(),
            csrf_token: "dummy-submit-csrf-token".to_string(),
        };
        let mut transport = ScriptedSubmitTransport::new(vec![
            ScriptStep::Get {
                path: submit_path.to_string(),
                result: Ok(submit_page_response(&html)),
            },
            ScriptStep::Post {
                path: submit_path.to_string(),
                form: expected_form,
                result: Ok(accepted_response(
                    StatusCode::FOUND,
                    "/contests/adt_easy_20260826_1/submissions/me",
                )),
            },
        ]);
        let request = SubmitRequest::new(
            contest_id.to_string(),
            task_id.to_string(),
            Language::Cpp,
            source.to_string(),
        );

        assert_eq!(
            submit_with_transport(&mut transport, request).unwrap(),
            SubmitOutcome::Accepted
        );
        assert_eq!(
            transport.requests,
            [
                ObservedRequest {
                    method: "GET",
                    path: submit_path.to_string(),
                },
                ObservedRequest {
                    method: "POST",
                    path: submit_path.to_string(),
                },
            ]
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn backend_resolves_latest_cpython_before_posting() {
        let source = "print('dummy')\n";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6082",
                source,
                Ok(accepted_response(
                    StatusCode::FOUND,
                    "/contests/abc466/submissions/me",
                )),
            ),
        ]);

        let outcome =
            submit_with_transport(&mut transport, submit_request(Language::Python, source))
                .unwrap();

        assert_eq!(outcome, SubmitOutcome::Accepted);
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn every_attempt_fetches_and_uses_a_fresh_csrf_token() {
        let source = "dummy source";
        let second_page = replace_once(
            CURRENT_SUBMIT_PAGE,
            "value=\"dummy-submit-csrf-token\"",
            "value=\"second-dummy-csrf-token\"",
        );
        let second_form = ExpectedSubmitForm {
            csrf_token: "second-dummy-csrf-token".to_string(),
            ..expected_form("6017", source)
        };
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(accepted_response(
                    StatusCode::FOUND,
                    "/contests/abc466/submissions/me",
                )),
            ),
            get_step(&second_page),
            ScriptStep::Post {
                path: "/contests/abc466/submit".to_string(),
                form: second_form,
                result: Ok(accepted_response(
                    StatusCode::FOUND,
                    "/contests/abc466/submissions/me",
                )),
            },
        ]);

        for _ in 0..2 {
            assert_eq!(
                submit_with_transport(&mut transport, submit_request(Language::Cpp, source),)
                    .unwrap(),
                SubmitOutcome::Accepted
            );
        }

        assert_eq!(transport.get_count(), 2);
        assert_eq!(transport.post_count(), 2);
        transport.assert_complete();
    }

    fn assert_source_snapshot_is_submitted_exactly(source: &str) {
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(accepted_response(
                    StatusCode::FOUND,
                    "/contests/abc466/submissions/me",
                )),
            ),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),).unwrap(),
            SubmitOutcome::Accepted
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn owned_source_snapshot_is_not_trimmed() {
        assert_source_snapshot_is_submitted_exactly("  dummy source  ");
    }

    #[test]
    fn source_crlf_is_preserved() {
        assert_source_snapshot_is_submitted_exactly("line one\r\nline two\r\n");
    }

    #[test]
    fn source_trailing_newline_is_preserved() {
        assert_source_snapshot_is_submitted_exactly("dummy source\n");
    }

    #[test]
    fn source_unicode_is_preserved() {
        assert_source_snapshot_is_submitted_exactly("こんにちは🦀\n");
    }

    #[test]
    fn status_303_with_relative_submission_location_is_accepted() {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(accepted_response(
                    StatusCode::SEE_OTHER,
                    "/contests/abc466/submissions/me",
                )),
            ),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),).unwrap(),
            SubmitOutcome::Accepted
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn absolute_same_origin_submission_location_is_accepted() {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(accepted_response(
                    StatusCode::FOUND,
                    "https://atcoder.jp/contests/abc466/submissions/me",
                )),
            ),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),).unwrap(),
            SubmitOutcome::Accepted
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    fn assert_unexpected_post_redirect(location: &str) {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(accepted_response(StatusCode::FOUND, location)),
            ),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),),
            Err(SubmitError::UnexpectedRedirect)
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn http_200_is_a_definite_rejection_and_does_not_leak_source() {
        let source = "dummy potentially sensitive source";
        let ignored_body = "x".repeat(1_000_000);
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(response(StatusCode::OK, None, &ignored_body)),
            ),
        ]);

        let error = submit_with_transport(&mut transport, submit_request(Language::Cpp, source))
            .unwrap_err();

        assert_eq!(error, SubmitError::SubmissionRejected);
        assert!(!error.to_string().contains(source));
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    fn assert_unexpected_success_status_is_unknown(status: StatusCode) {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step("6017", source, Ok(response(status, None, "ignored body"))),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source)).unwrap(),
            SubmitOutcome::UnknownSubmissionOutcome
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn http_201_is_unknown_and_is_never_retried() {
        assert_unexpected_success_status_is_unknown(StatusCode::CREATED);
    }

    #[test]
    fn http_202_is_unknown_and_is_never_retried() {
        assert_unexpected_success_status_is_unknown(StatusCode::ACCEPTED);
    }

    #[test]
    fn http_204_is_unknown_and_is_never_retried() {
        assert_unexpected_success_status_is_unknown(StatusCode::NO_CONTENT);
    }

    #[test]
    fn http_206_is_unknown_and_is_never_retried() {
        assert_unexpected_success_status_is_unknown(StatusCode::PARTIAL_CONTENT);
    }

    #[test]
    fn external_submit_redirect_is_rejected() {
        assert_unexpected_post_redirect("https://evil.example/contests/abc466/submissions/me");
    }

    #[test]
    fn noncanonical_dot_segment_submission_redirects_are_rejected() {
        for location in [
            "/contests/abc466/submissions/x/../me",
            "/contests/abc466/submissions/x/%2e%2e/me",
            "/contests/abc466/submissions/x/%2E%2E/me",
        ] {
            assert_unexpected_post_redirect(location);
        }
    }

    #[test]
    fn backslash_and_extra_path_submission_redirects_are_rejected() {
        for location in [
            "/contests/abc466/submissions\\me",
            "/contests/abc466/submissions/me/extra",
        ] {
            assert_unexpected_post_redirect(location);
        }
    }

    #[test]
    fn noncanonical_absolute_submission_origins_are_rejected() {
        for location in [
            "//evil.example/contests/abc466/submissions/me",
            "https://atcoder.jp.evil.example/contests/abc466/submissions/me",
            "https://atcoder.jp@evil.example/contests/abc466/submissions/me",
            "https://atcoder.jp:8443/contests/abc466/submissions/me",
            "http://atcoder.jp/contests/abc466/submissions/me",
        ] {
            assert_unexpected_post_redirect(location);
        }
    }

    #[test]
    fn wrong_contest_submit_redirect_is_rejected() {
        assert_unexpected_post_redirect("/contests/abc999/submissions/me");
    }

    #[test]
    fn submit_page_redirect_is_not_accepted() {
        assert_unexpected_post_redirect("/contests/abc466/submit");
    }

    #[test]
    fn unexpected_same_origin_redirect_is_rejected() {
        assert_unexpected_post_redirect("/contests/abc466/submissions");
    }

    #[test]
    fn submission_redirect_with_query_or_fragment_is_rejected() {
        for location in [
            "/contests/abc466/submissions/me?x=1",
            "/contests/abc466/submissions/me#x",
        ] {
            assert_unexpected_post_redirect(location);
        }
    }

    #[test]
    fn multiple_submission_locations_are_rejected() {
        let source = "dummy";
        let mut redirect = accepted_response(StatusCode::FOUND, "/contests/abc466/submissions/me");
        redirect.headers.append(
            LOCATION,
            HeaderValue::from_static("/contests/abc466/submissions/me"),
        );
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step("6017", source, Ok(redirect)),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source)),
            Err(SubmitError::UnexpectedRedirect)
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn post_login_redirect_requires_authentication() {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(accepted_response(
                    StatusCode::FOUND,
                    "/login?continue=%2Fcontests%2Fabc466%2Fsubmit",
                )),
            ),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),),
            Err(SubmitError::AuthenticationRequired)
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn submit_page_get_login_redirect_requires_authentication_without_post() {
        let mut transport = ScriptedSubmitTransport::new(vec![ScriptStep::Get {
            path: "/contests/abc466/submit".to_string(),
            result: Ok(accepted_response(
                StatusCode::FOUND,
                "/login?continue=%2Fcontests%2Fabc466%2Fsubmit",
            )),
        }]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, "dummy"),),
            Err(SubmitError::AuthenticationRequired)
        );
        assert_eq!(transport.post_count(), 0);
        transport.assert_complete();
    }

    #[test]
    fn followed_submit_page_login_url_requires_authentication_without_post() {
        let mut login_page = response(StatusCode::OK, None, "ignored login body");
        login_page.final_url = Some(
            reqwest::Url::parse("https://atcoder.jp/login?continue=%2Fcontests%2Fabc466%2Fsubmit")
                .unwrap(),
        );
        let mut transport = ScriptedSubmitTransport::new(vec![ScriptStep::Get {
            path: "/contests/abc466/submit".to_string(),
            result: Ok(login_page),
        }]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, "dummy")),
            Err(SubmitError::AuthenticationRequired)
        );
        assert_eq!(transport.post_count(), 0);
        transport.assert_complete();
    }

    #[test]
    fn post_client_initialization_failure_remains_submit_specific() {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Err(SubmitTransportFailure::ClientInitialization),
            ),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source)),
            Err(SubmitError::SubmitClientInitializationFailed)
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn post_timeout_is_unknown_and_is_never_retried() {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step("6017", source, Err(SubmitTransportFailure::Timeout)),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),).unwrap(),
            SubmitOutcome::UnknownSubmissionOutcome
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn post_connection_reset_is_unknown_and_is_never_retried() {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step("6017", source, Err(SubmitTransportFailure::ConnectionReset)),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),).unwrap(),
            SubmitOutcome::UnknownSubmissionOutcome
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn post_429_is_rate_limited_and_is_never_retried() {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(response(
                    StatusCode::TOO_MANY_REQUESTS,
                    None,
                    "rate limited",
                )),
            ),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),),
            Err(SubmitError::RateLimited)
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn post_500_is_unknown_and_is_never_retried() {
        let source = "dummy";
        let mut transport = ScriptedSubmitTransport::new(vec![
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    "server error",
                )),
            ),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),).unwrap(),
            SubmitOutcome::UnknownSubmissionOutcome
        );
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn get_429_retries_then_posts_exactly_once() {
        let source = "dummy";
        let mut rate_limited = response(StatusCode::TOO_MANY_REQUESTS, None, "rate limited");
        rate_limited
            .headers
            .insert(RETRY_AFTER, HeaderValue::from_static("0"));
        let mut transport = ScriptedSubmitTransport::new(vec![
            ScriptStep::Get {
                path: "/contests/abc466/submit".to_string(),
                result: Ok(rate_limited),
            },
            get_step(CURRENT_SUBMIT_PAGE),
            post_step(
                "6017",
                source,
                Ok(accepted_response(
                    StatusCode::FOUND,
                    "/contests/abc466/submissions/me",
                )),
            ),
        ]);

        assert_eq!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, source),).unwrap(),
            SubmitOutcome::Accepted
        );
        assert_eq!(transport.get_count(), 2);
        assert_eq!(transport.get_retry_waits, 1);
        assert_eq!(transport.post_count(), 1);
        transport.assert_complete();
    }

    #[test]
    fn task_unavailable_fails_before_post() {
        let mut transport = ScriptedSubmitTransport::new(vec![get_step(CURRENT_SUBMIT_PAGE)]);
        let request = SubmitRequest::new(
            "abc466".to_string(),
            "abc466_missing".to_string(),
            Language::Cpp,
            "dummy".to_string(),
        );

        assert!(matches!(
            submit_with_transport(&mut transport, request),
            Err(SubmitError::SubmitPage(
                SubmitPageError::TaskUnavailable { .. }
            ))
        ));
        assert_eq!(transport.post_count(), 0);
        transport.assert_complete();
    }

    #[test]
    fn language_unavailable_fails_before_post() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            ">C++20 (GCC 12.2.0)<",
            ">C++20 (Clang 12.2.0)<",
        );
        let html = replace_once(&html, ">C++23 (GCC 15.2.0)<", ">C++23 (Clang 15.2.0)<");
        let mut transport = ScriptedSubmitTransport::new(vec![get_step(&html)]);

        assert!(matches!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, "dummy"),),
            Err(SubmitError::SubmitPage(
                SubmitPageError::LanguageUnavailable { .. }
            ))
        ));
        assert_eq!(transport.post_count(), 0);
        transport.assert_complete();
    }

    #[test]
    fn ambiguous_language_fails_before_post() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "          <option value=\"6017\">C++23 (GCC 15.2.0)</option>",
            "          <option value=\"6017\">C++23 (GCC 15.2.0)</option>\n          <option value=\"other-latest\">C++23 (GCC 15.2.0)</option>",
        );
        let mut transport = ScriptedSubmitTransport::new(vec![get_step(&html)]);

        assert!(matches!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, "dummy"),),
            Err(SubmitError::SubmitPage(
                SubmitPageError::LanguageAmbiguous { .. }
            ))
        ));
        assert_eq!(transport.post_count(), 0);
        transport.assert_complete();
    }

    #[test]
    fn malformed_submit_page_fails_before_post() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "data-name=\"data.LanguageId\"",
            "data-name=\"broken\"",
        );
        let mut transport = ScriptedSubmitTransport::new(vec![get_step(&html)]);

        assert!(matches!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, "dummy"),),
            Err(SubmitError::SubmitPage(SubmitPageError::MalformedPage(_)))
        ));
        assert_eq!(transport.post_count(), 0);
        transport.assert_complete();
    }

    #[test]
    fn missing_csrf_fails_before_post() {
        let html = replace_once(
            CURRENT_SUBMIT_PAGE,
            "    <input type=\"hidden\" name=\"csrf_token\" value=\"dummy-submit-csrf-token\">\n",
            "",
        );
        let mut transport = ScriptedSubmitTransport::new(vec![get_step(&html)]);

        assert!(matches!(
            submit_with_transport(&mut transport, submit_request(Language::Cpp, "dummy"),),
            Err(SubmitError::SubmitPage(SubmitPageError::MalformedPage(_)))
        ));
        assert_eq!(transport.post_count(), 0);
        transport.assert_complete();
    }

    #[test]
    fn wrong_contest_form_identity_fails_before_post() {
        let mut transport = ScriptedSubmitTransport::new(vec![ScriptStep::Get {
            path: "/contests/abc999/submit".to_string(),
            result: Ok(submit_page_response(CURRENT_SUBMIT_PAGE)),
        }]);
        let request = SubmitRequest::new(
            "abc999".to_string(),
            "abc466_a".to_string(),
            Language::Cpp,
            "dummy".to_string(),
        );

        assert!(matches!(
            submit_with_transport(&mut transport, request),
            Err(SubmitError::SubmitPage(SubmitPageError::MalformedPage(_)))
        ));
        assert_eq!(transport.post_count(), 0);
        transport.assert_complete();
    }

    #[test]
    fn fixture_client_submit_is_unavailable_without_live_network() {
        let client = AtCoderClient::fixture("unused-submit-fixture-root");

        assert_eq!(
            client.submit(submit_request(Language::Cpp, "dummy")),
            Err(SubmitError::SubmitUnavailable)
        );
    }

    #[test]
    fn submit_request_debug_redacts_source_snapshot() {
        let source = "dummy sensitive source";
        let debug = format!("{:?}", submit_request(Language::Cpp, source));

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(source));
    }
}
