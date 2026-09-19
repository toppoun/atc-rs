use crate::auth;
use crate::model::Sample;

use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::cookie::CookieStore;
use reqwest::header::{HeaderValue, RETRY_AFTER};

use scraper::{Html, Selector};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// Submit parsing and the one-attempt backend are shared with the one-shot CLI command.
pub(crate) mod submission_diagnostics;
pub(crate) mod submission_tracking;
#[allow(dead_code)]
pub(crate) mod submit;

const BASE_URL: &str = "https://atcoder.jp";

// 正常時も短時間に連打しない
const REQUEST_INTERVAL: Duration = Duration::from_millis(500);

// 429を受けたとき、Retry-Afterが無い場合の待機時間
const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(2);

// A malformed or overly defensive server value must not suspend the CLI indefinitely.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

// 最初のリクエストとは別に何回retryするか
const MAX_429_RETRIES: usize = 3;

#[derive(Debug)]
pub enum AtCoderError {
    Http(reqwest::Error),
    Auth(auth::AuthLoadError),
    UnexpectedAuthenticationStatus(StatusCode),
    Fixture {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(String),
    InvalidIdentifier {
        kind: &'static str,
        value: String,
    },
    InvalidProblemUrl(String),

    // 429がretryしても解消しなかった
    RateLimited {
        url: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationStatus {
    NotConfigured,
    Authenticated,
    Unauthenticated,
}

#[derive(Debug, Clone)]
pub(crate) enum AuthenticationVerification {
    Authenticated {
        username: Option<String>,
        warnings: Vec<auth::AuthPersistenceWarning>,
        lineage: auth::AuthenticationLineage,
    },
    Rejected {
        warnings: Vec<auth::AuthPersistenceWarning>,
        lineage: auth::AuthenticationLineage,
    },
    Unavailable {
        warnings: Vec<auth::AuthPersistenceWarning>,
        lineage: auth::AuthenticationLineage,
    },
}

impl AuthenticationVerification {
    pub(crate) fn matches_snapshot(&self, snapshot: &auth::AuthSnapshot) -> bool {
        let lineage = match self {
            Self::Authenticated { lineage, .. }
            | Self::Rejected { lineage, .. }
            | Self::Unavailable { lineage, .. } => lineage,
        };
        lineage.matches_snapshot(snapshot)
    }

    #[cfg(test)]
    pub(crate) fn authenticated_for_test(
        snapshot: &auth::AuthSnapshot,
        username: Option<String>,
    ) -> Self {
        let session = auth::SessionAuth::from_snapshot(snapshot);
        Self::Authenticated {
            username,
            warnings: Vec::new(),
            lineage: session
                .authentication_lineage()
                .expect("test verification snapshot must be configured"),
        }
    }

    #[cfg(test)]
    pub(crate) fn authenticated_for_session_test(
        session: &auth::SessionAuth,
        username: Option<String>,
        warnings: Vec<auth::AuthPersistenceWarning>,
    ) -> Self {
        Self::Authenticated {
            username,
            warnings,
            lineage: session
                .authentication_lineage()
                .expect("test verification session must have a configured lineage"),
        }
    }
}

pub(crate) fn verify_authentication(
    snapshot: Arc<auth::AuthSnapshot>,
) -> AuthenticationVerification {
    let session = auth::SessionAuth::from_snapshot(&snapshot);
    let lineage = session
        .authentication_lineage()
        .expect("authentication verification requires a configured snapshot");
    let Ok(client) = AtCoderClient::from_session_auth(session) else {
        return AuthenticationVerification::Unavailable {
            warnings: Vec::new(),
            lineage,
        };
    };
    client.verify_authentication()
}

pub fn authentication_status() -> Result<AuthenticationStatus, AtCoderError> {
    let auth = auth::AuthSnapshot::load();
    match auth.as_ref() {
        auth::AuthSnapshot::Missing => return Ok(AuthenticationStatus::NotConfigured),
        auth::AuthSnapshot::Invalid(error) => return Err(AtCoderError::Auth(*error)),
        auth::AuthSnapshot::Configured { .. } => {}
    };

    let client = AtCoderClient::from_session_auth(auth::SessionAuth::from_snapshot(&auth))?;
    let Source::Http(http) = &client.source else {
        unreachable!("authentication status always uses HTTP")
    };
    let response = http
        .send(http.client.get(format!("{BASE_URL}/settings")))?
        .error_for_status()?;

    classify_authentication_response(response.status(), response.url())
}

fn classify_authentication_response(
    status: StatusCode,
    url: &reqwest::Url,
) -> Result<AuthenticationStatus, AtCoderError> {
    if !status.is_success() {
        return Err(AtCoderError::UnexpectedAuthenticationStatus(status));
    }

    Ok(if is_authenticated_settings_url(url) {
        AuthenticationStatus::Authenticated
    } else {
        AuthenticationStatus::Unauthenticated
    })
}

fn is_authenticated_settings_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("atcoder.jp")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/settings"
}

impl fmt::Display for AtCoderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => write!(formatter, "HTTP request failed: {error}"),
            Self::Auth(error) => {
                write!(formatter, "failed to load authentication cookie: {error}")
            }

            Self::UnexpectedAuthenticationStatus(status) => {
                write!(
                    formatter,
                    "authentication check returned unexpected HTTP status {status}"
                )
            }
            Self::Fixture { path, source } => {
                write!(
                    formatter,
                    "failed to read fixture {}: {source}",
                    path.display()
                )
            }
            Self::Parse(message) => write!(formatter, "failed to parse AtCoder HTML: {message}"),
            Self::InvalidIdentifier { kind, value } => {
                write!(formatter, "invalid AtCoder {kind}: {value:?}")
            }
            Self::InvalidProblemUrl(url) => write!(formatter, "invalid AtCoder problem URL: {url}"),
            Self::RateLimited { url } => {
                write!(formatter, "rate limit persisted after retries: {url}")
            }
        }
    }
}

impl std::error::Error for AtCoderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::Auth(error) => Some(error),
            Self::Fixture { source, .. } => Some(source),
            Self::UnexpectedAuthenticationStatus(_)
            | Self::Parse(_)
            | Self::InvalidIdentifier { .. }
            | Self::InvalidProblemUrl(_)
            | Self::RateLimited { .. } => None,
        }
    }
}

impl From<reqwest::Error> for AtCoderError {
    fn from(err: reqwest::Error) -> Self {
        AtCoderError::Http(err)
    }
}

enum Source {
    Http(HttpSource),
    Fixture(PathBuf),
}

struct HttpSource {
    client: Client,
    submit_client: LazySubmitClient,
    auth: Arc<auth::SessionAuth>,
    last_request: Mutex<Option<Instant>>,
}

enum AuthenticatedSendResult {
    AuthenticationRequired,
    CancelledBeforePost,
    Sent(Result<Response, reqwest::Error>),
}

struct LazySubmitClient {
    provider: Arc<SessionCookieProvider>,
    client: Mutex<Option<Client>>,
}

impl LazySubmitClient {
    fn new(provider: Arc<SessionCookieProvider>) -> Self {
        Self {
            provider,
            client: Mutex::new(None),
        }
    }

    fn get(&self) -> Result<Client, AtCoderError> {
        self.get_or_try_init_with(build_submit_http_client)
    }

    fn get_or_try_init_with(
        &self,
        build: impl FnOnce(Arc<SessionCookieProvider>) -> Result<Client, AtCoderError>,
    ) -> Result<Client, AtCoderError> {
        let mut cached = self
            .client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = cached.as_ref() {
            return Ok(client.clone());
        }

        let client = build(Arc::clone(&self.provider))?;
        *cached = Some(client.clone());
        Ok(client)
    }
}

impl HttpSource {
    fn new(auth: Arc<auth::SessionAuth>) -> Result<Self, AtCoderError> {
        let provider = Arc::new(SessionCookieProvider::for_atcoder(Arc::clone(&auth)));
        Self::new_with_provider(auth, provider)
    }

    fn new_with_provider(
        auth: Arc<auth::SessionAuth>,
        provider: Arc<SessionCookieProvider>,
    ) -> Result<Self, AtCoderError> {
        Ok(Self {
            client: build_http_client(Arc::clone(&provider))?,
            submit_client: LazySubmitClient::new(provider),
            auth,
            last_request: Mutex::new(None),
        })
    }

    #[cfg(test)]
    fn new_for_test_origin(
        auth: Arc<auth::SessionAuth>,
        origin: &reqwest::Url,
    ) -> Result<Self, AtCoderError> {
        let provider = Arc::new(SessionCookieProvider::for_test_origin(
            Arc::clone(&auth),
            origin,
        ));
        Self::new_with_provider(auth, provider)
    }

    #[cfg(test)]
    fn new_for_test_origin_with_timeout(
        auth: Arc<auth::SessionAuth>,
        origin: &reqwest::Url,
        timeout: Duration,
    ) -> Result<Self, AtCoderError> {
        let provider = Arc::new(SessionCookieProvider::for_test_origin(
            Arc::clone(&auth),
            origin,
        ));
        Ok(Self {
            client: http_client_builder(Arc::clone(&provider))
                .timeout(timeout)
                .build()?,
            submit_client: LazySubmitClient::new(provider),
            auth,
            last_request: Mutex::new(None),
        })
    }

    fn send(&self, request: RequestBuilder) -> Result<Response, reqwest::Error> {
        self.exchange(|| request.send())
    }

    fn send_with_warning_sink(
        &self,
        request: RequestBuilder,
        warning_sink: impl FnMut(auth::AuthPersistenceWarning),
    ) -> Result<Response, reqwest::Error> {
        self.exchange_with_warning_sink(|| request.send(), warning_sink)
    }

    fn exchange<T>(&self, exchange: impl FnOnce() -> T) -> T {
        self.exchange_with_warning_sink(exchange, emit_auth_persistence_warning)
    }

    fn exchange_with_warning_sink<T>(
        &self,
        exchange: impl FnOnce() -> T,
        mut warning_sink: impl FnMut(auth::AuthPersistenceWarning),
    ) -> T {
        self.auth.with_exchange(|| {
            let result = exchange();
            for warning in self.auth.persist_pending() {
                warning_sink(warning);
            }
            result
        })
    }

    fn send_authenticated_once(
        &self,
        request: RequestBuilder,
        try_begin_post: &dyn Fn() -> bool,
    ) -> AuthenticatedSendResult {
        self.exchange(|| {
            if self.auth.kind() != auth::SessionAuthKind::Configured {
                return AuthenticatedSendResult::AuthenticationRequired;
            }
            if !try_begin_post() {
                return AuthenticatedSendResult::CancelledBeforePost;
            }
            AuthenticatedSendResult::Sent(request.send())
        })
    }
}

fn emit_auth_persistence_warning(warning: auth::AuthPersistenceWarning) {
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    write_auth_persistence_warning(&mut stderr, warning);
}

fn write_auth_persistence_warning(
    writer: &mut impl std::io::Write,
    warning: auth::AuthPersistenceWarning,
) {
    let _ = writeln!(writer, "warning: {warning}");
}

struct SessionCookieProvider {
    auth: Arc<auth::SessionAuth>,
    origin: TrustedHttpOrigin,
}

struct TrustedHttpOrigin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl TrustedHttpOrigin {
    fn atcoder() -> Self {
        Self {
            scheme: "https".to_string(),
            host: "atcoder.jp".to_string(),
            port: None,
        }
    }

    #[cfg(test)]
    fn from_url(url: &reqwest::Url) -> Self {
        Self {
            scheme: url.scheme().to_string(),
            host: url
                .host_str()
                .expect("test origin must have a host")
                .to_string(),
            port: url.port(),
        }
    }

    fn matches(&self, url: &reqwest::Url) -> bool {
        url.scheme() == self.scheme
            && url.host_str() == Some(self.host.as_str())
            && url.port() == self.port
            && url.username().is_empty()
            && url.password().is_none()
    }
}

impl SessionCookieProvider {
    fn for_atcoder(auth: Arc<auth::SessionAuth>) -> Self {
        Self {
            auth,
            origin: TrustedHttpOrigin::atcoder(),
        }
    }

    #[cfg(test)]
    fn for_test_origin(auth: Arc<auth::SessionAuth>, origin: &reqwest::Url) -> Self {
        Self {
            auth,
            origin: TrustedHttpOrigin::from_url(origin),
        }
    }
}

impl fmt::Debug for SessionCookieProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionCookieProvider(<redacted>)")
    }
}

impl CookieStore for SessionCookieProvider {
    fn set_cookies(
        &self,
        cookie_headers: &mut dyn Iterator<Item = &HeaderValue>,
        url: &reqwest::Url,
    ) {
        if self.origin.matches(url) {
            self.auth
                .observe_trusted_set_cookie_batch(cookie_headers, url);
        }
    }

    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue> {
        if self.origin.matches(url) {
            self.auth.cookie_header()
        } else {
            None
        }
    }
}

pub struct AtCoderClient {
    source: Source,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ContestOutline {
    pub(crate) contest_id: String,
    pub(crate) problems: Vec<ProblemOutline>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProblemOutline {
    pub(crate) index: String,
    pub(crate) title: String,
    pub(crate) task_id: String,
    pub(crate) url: String,
}

impl From<&crate::model::Problem> for ProblemOutline {
    fn from(problem: &crate::model::Problem) -> Self {
        Self {
            index: problem.index.clone(),
            title: problem.title.clone(),
            task_id: problem.task_id.clone(),
            url: problem.url.clone(),
        }
    }
}

impl AtCoderClient {
    pub fn new() -> Result<Self, AtCoderError> {
        let auth = auth::AuthSnapshot::load();
        if let auth::AuthSnapshot::Invalid(error) = auth.as_ref() {
            return Err(AtCoderError::Auth(*error));
        }
        Self::from_session_auth(auth::SessionAuth::from_snapshot(&auth))
    }

    #[cfg(test)]
    pub(crate) fn from_auth_snapshot(auth: &auth::AuthSnapshot) -> Result<Self, AtCoderError> {
        Self::from_session_auth(auth::SessionAuth::from_snapshot(auth))
    }

    pub(crate) fn from_session_auth(auth: Arc<auth::SessionAuth>) -> Result<Self, AtCoderError> {
        Ok(Self {
            source: Source::Http(HttpSource::new(auth)?),
        })
    }

    fn verify_authentication(&self) -> AuthenticationVerification {
        let Source::Http(http) = &self.source else {
            unreachable!("authentication verification always uses HTTP")
        };
        let settings = reqwest::Url::parse(&format!("{BASE_URL}/settings"))
            .expect("the static AtCoder settings URL must be valid");
        verify_authentication_http(http, settings)
    }

    pub fn fixture(root: impl Into<PathBuf>) -> Self {
        Self {
            source: Source::Fixture(root.into()),
        }
    }

    #[cfg(test)]
    pub(crate) fn credential_matches_for_test(&self, expected: &str) -> bool {
        match &self.source {
            Source::Http(http) => http.auth.credential_matches_for_test(expected),
            Source::Fixture(_) => false,
        }
    }

    // ============================================================
    // Contest
    // ============================================================

    pub(crate) fn fetch_contest(&self, contest_id: &str) -> Result<ContestOutline, AtCoderError> {
        validate_identifier("contest ID", contest_id)?;

        let html = match &self.source {
            Source::Http(http) => {
                let url = format!("{BASE_URL}/contests/{contest_id}/tasks");

                Self::get_text(http, &url)?
            }

            Source::Fixture(root) => {
                let path = root.join("contests").join(format!("{contest_id}.html"));

                read_fixture(path)?
            }
        };

        parse_contest(contest_id, &html)
    }

    // ============================================================
    // Samples
    // ============================================================

    pub(crate) fn fetch_samples(
        &self,
        problem: &ProblemOutline,
    ) -> Result<Vec<Sample>, AtCoderError> {
        let html = match &self.source {
            Source::Http(http) => {
                validate_problem_url(&problem.url)?;
                Self::get_text(http, &problem.url)?
            }

            Source::Fixture(root) => {
                validate_identifier("task ID", &problem.task_id)?;
                let path = root
                    .join("problems")
                    .join(format!("{}.html", problem.task_id));

                read_fixture(path)?
            }
        };

        parse_samples(&html)
    }

    // ============================================================
    // HTTP
    // ============================================================

    fn get_text(http: &HttpSource, url: &str) -> Result<String, AtCoderError> {
        match Self::get_text_until(http, url, &|| true)? {
            Some(text) => Ok(text),
            None => unreachable!("an always-continue GET cannot be cancelled"),
        }
    }

    fn get_text_until(
        http: &HttpSource,
        url: &str,
        should_continue: &dyn Fn() -> bool,
    ) -> Result<Option<String>, AtCoderError> {
        for retry_count in 0..=MAX_429_RETRIES {
            if !wait_for_request_slot_until(http, should_continue) {
                return Ok(None);
            }
            if !should_continue() {
                return Ok(None);
            }
            let response = http.send(http.client.get(url))?;
            if !should_continue() {
                return Ok(None);
            }

            // 429だけ特別扱い
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                if retry_count == MAX_429_RETRIES {
                    return Err(AtCoderError::RateLimited {
                        url: url.to_string(),
                    });
                }

                let wait = retry_wait(response.headers());

                if !interruptible_sleep(wait, should_continue) {
                    return Ok(None);
                }

                continue;
            }

            // 404 / 500などは普通のHTTPエラーとして返す
            let response = response.error_for_status()?;

            let html = response.text()?;

            return Ok(Some(html));
        }

        Err(AtCoderError::RateLimited {
            url: url.to_string(),
        })
    }
}

fn verify_authentication_http(
    http: &HttpSource,
    settings_url: reqwest::Url,
) -> AuthenticationVerification {
    let mut warnings = Vec::new();
    let response = match http
        .send_with_warning_sink(http.client.get(settings_url.clone()), |warning| {
            warnings.push(warning)
        }) {
        Ok(response) => response,
        Err(_) => {
            return AuthenticationVerification::Unavailable {
                warnings,
                lineage: http
                    .auth
                    .authentication_lineage()
                    .expect("authentication verification requires a configured session"),
            };
        }
    };

    let lineage = http
        .auth
        .authentication_lineage()
        .expect("authentication verification requires a configured session");

    if !response.status().is_success() {
        return AuthenticationVerification::Unavailable { warnings, lineage };
    }
    if response.url() == &settings_url {
        // Reaching the exact settings endpoint establishes authentication. Body failures and
        // navigation markup changes affect only the optional account label.
        let username = response
            .text()
            .ok()
            .and_then(|body| parse_authenticated_username(&body));
        return AuthenticationVerification::Authenticated {
            username,
            warnings,
            lineage,
        };
    }
    if is_canonical_login_redirect(response.url(), &settings_url) {
        return AuthenticationVerification::Rejected { warnings, lineage };
    }
    AuthenticationVerification::Unavailable { warnings, lineage }
}

fn is_canonical_login_redirect(final_url: &reqwest::Url, settings_url: &reqwest::Url) -> bool {
    final_url.scheme() == settings_url.scheme()
        && final_url.host_str() == settings_url.host_str()
        && final_url.port() == settings_url.port()
        && final_url.username().is_empty()
        && final_url.password().is_none()
        && final_url.path() == "/login"
        && final_url.fragment().is_none()
        && {
            let pairs = final_url.query_pairs().collect::<Vec<_>>();
            pairs.len() == 1 && pairs[0].0 == "continue" && pairs[0].1 == settings_url.as_str()
        }
}

fn parse_authenticated_username(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector =
        Selector::parse("nav.navbar #navbar-collapse ul.nav.navbar-nav.navbar-right a[href]")
            .expect("static authenticated-navigation selector must be valid");

    document.select(&selector).find_map(|link| {
        let href = link.value().attr("href")?;
        let username = href.strip_prefix("/users/")?;
        if username.is_empty()
            || username.contains('/')
            || username.contains('?')
            || username.contains('#')
            || !username
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return None;
        }
        Some(username.to_string())
    })
}

fn build_http_client(provider: Arc<SessionCookieProvider>) -> Result<Client, AtCoderError> {
    Ok(http_client_builder(provider).build()?)
}

fn build_submit_http_client(provider: Arc<SessionCookieProvider>) -> Result<Client, AtCoderError> {
    Ok(http_client_builder(provider)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()?)
}

fn http_client_builder(provider: Arc<SessionCookieProvider>) -> reqwest::blocking::ClientBuilder {
    let user_agent = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
    Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(10))
        .cookie_provider(provider)
}

fn read_fixture(path: PathBuf) -> Result<String, AtCoderError> {
    std::fs::read_to_string(&path).map_err(|source| AtCoderError::Fixture { path, source })
}

fn validate_identifier(kind: &'static str, value: &str) -> Result<(), AtCoderError> {
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));

    if valid {
        Ok(())
    } else {
        Err(AtCoderError::InvalidIdentifier {
            kind,
            value: value.to_string(),
        })
    }
}

fn validate_problem_url(url: &str) -> Result<(), AtCoderError> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| AtCoderError::InvalidProblemUrl(url.to_string()))?;
    let valid = parsed.scheme() == "https"
        && parsed.host_str() == Some("atcoder.jp")
        && parsed.port().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path().starts_with("/contests/")
        && parsed.path().contains("/tasks/");

    if valid {
        Ok(())
    } else {
        Err(AtCoderError::InvalidProblemUrl(url.to_string()))
    }
}

fn wait_for_request_slot_until(http: &HttpSource, should_continue: &dyn Fn() -> bool) -> bool {
    reserve_request_slot_until(&http.last_request, REQUEST_INTERVAL, should_continue).is_some()
}

fn reserve_request_slot_until(
    last_request: &Mutex<Option<Instant>>,
    request_interval: Duration,
    should_continue: &dyn Fn() -> bool,
) -> Option<Instant> {
    loop {
        if !should_continue() {
            return None;
        }

        let wait = {
            let mut last_request = match last_request.lock() {
                Ok(last_request) => last_request,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !should_continue() {
                return None;
            }

            let now = Instant::now();
            match remaining_request_interval_for(*last_request, now, request_interval) {
                Some(wait) => wait,
                None => {
                    // Reserve while holding the mutex. Concurrent waiters will observe this
                    // request start and compete for a later slot after sleeping without the lock.
                    *last_request = Some(now);
                    return Some(now);
                }
            }
        };

        if !interruptible_sleep(wait, should_continue) {
            return None;
        }
    }
}

fn interruptible_sleep(duration: Duration, should_continue: &dyn Fn() -> bool) -> bool {
    const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);
    let deadline = Instant::now() + duration;
    while should_continue() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::sleep(remaining.min(CANCEL_POLL_INTERVAL));
    }
    false
}

#[cfg(test)]
fn remaining_request_interval(previous: Option<Instant>, now: Instant) -> Option<Duration> {
    remaining_request_interval_for(previous, now, REQUEST_INTERVAL)
}

fn remaining_request_interval_for(
    previous: Option<Instant>,
    now: Instant,
    request_interval: Duration,
) -> Option<Duration> {
    previous
        .and_then(|previous| request_interval.checked_sub(now.saturating_duration_since(previous)))
        .filter(|wait| !wait.is_zero())
}

fn retry_wait(headers: &reqwest::header::HeaderMap) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds).min(MAX_RETRY_WAIT))
        .unwrap_or(DEFAULT_RETRY_WAIT)
}

// ============================================================
// Contest Parser
// ============================================================

fn parse_contest(contest_id: &str, html: &str) -> Result<ContestOutline, AtCoderError> {
    let document = Html::parse_document(html);

    let row_selector = Selector::parse("table tbody tr")
        .map_err(|_| AtCoderError::Parse("invalid row selector".to_string()))?;

    let link_selector = Selector::parse("td a[href*='/tasks/']")
        .map_err(|_| AtCoderError::Parse("invalid link selector".to_string()))?;

    let mut problems = Vec::new();
    let mut indexes = BTreeSet::new();
    let mut task_ids = BTreeSet::new();
    let expected_href_prefix = format!("/contests/{contest_id}/tasks/");

    for row in document.select(&row_selector) {
        let mut links = row.select(&link_selector);

        let Some(index_link) = links.next() else {
            continue;
        };

        let title_link = links
            .next()
            .ok_or_else(|| AtCoderError::Parse("problem title not found".to_string()))?;

        let index = index_link.text().collect::<String>().trim().to_string();

        let title = title_link.text().collect::<String>().trim().to_string();

        if index.is_empty() || title.is_empty() {
            return Err(AtCoderError::Parse(
                "problem index or title is empty".to_string(),
            ));
        }

        let href = index_link
            .value()
            .attr("href")
            .ok_or_else(|| AtCoderError::Parse("problem url not found".to_string()))?;

        let title_href = title_link
            .value()
            .attr("href")
            .ok_or_else(|| AtCoderError::Parse("problem title url not found".to_string()))?;

        if title_href != href {
            return Err(AtCoderError::Parse(format!(
                "problem links do not match for index {index}"
            )));
        }

        let task_id = href
            .strip_prefix(&expected_href_prefix)
            .ok_or_else(|| AtCoderError::Parse(format!("unexpected problem url: {href}")))?;
        validate_identifier("task ID", task_id)?;

        if !indexes.insert(index.clone()) {
            return Err(AtCoderError::Parse(format!(
                "duplicate problem index: {index}"
            )));
        }
        if !task_ids.insert(task_id.to_string()) {
            return Err(AtCoderError::Parse(format!("duplicate task ID: {task_id}")));
        }

        let url = format!("{BASE_URL}{href}");

        problems.push(ProblemOutline {
            index,
            title,
            task_id: task_id.to_string(),
            url,
        });
    }

    if problems.is_empty() {
        return Err(AtCoderError::Parse("no problems found".to_string()));
    }

    Ok(ContestOutline {
        contest_id: contest_id.to_string(),
        problems,
    })
}

// ============================================================
// Sample Parser
// ============================================================

fn parse_samples(html: &str) -> Result<Vec<Sample>, AtCoderError> {
    let document = Html::parse_document(html);

    let ja_selector = Selector::parse("#task-statement span.lang-ja")
        .map_err(|_| AtCoderError::Parse("invalid ja selector".to_string()))?;

    let en_selector = Selector::parse("#task-statement span.lang-en")
        .map_err(|_| AtCoderError::Parse("invalid en selector".to_string()))?;

    let section_selector = Selector::parse(".part section")
        .map_err(|_| AtCoderError::Parse("invalid section selector".to_string()))?;

    let h3_selector = Selector::parse("h3")
        .map_err(|_| AtCoderError::Parse("invalid h3 selector".to_string()))?;

    let pre_selector = Selector::parse("pre")
        .map_err(|_| AtCoderError::Parse("invalid pre selector".to_string()))?;

    // 日本語statementを優先。
    // 無ければ英語statementを使う。
    let (statement, input_prefix, output_prefix) =
        if let Some(ja) = document.select(&ja_selector).next() {
            (ja, "入力例 ", "出力例 ")
        } else if let Some(en) = document.select(&en_selector).next() {
            (en, "Sample Input ", "Sample Output ")
        } else {
            return Err(AtCoderError::Parse(
                "problem statement not found".to_string(),
            ));
        };

    let mut inputs = BTreeMap::new();
    let mut outputs = BTreeMap::new();

    for section in statement.select(&section_selector) {
        let Some(h3) = section.select(&h3_selector).next() else {
            continue;
        };

        let heading = h3.text().collect::<String>();
        let heading = heading.trim();

        // 「入力例 1」→ ("input", "1")
        // 「出力例 1」→ ("output", "1")
        // それ以外     → None

        let sample_kind = if let Some(number) = heading.strip_prefix(input_prefix) {
            Some((true, number))
        } else {
            heading
                .strip_prefix(output_prefix)
                .map(|number| (false, number))
        };

        let Some((is_input, number)) = sample_kind else {
            continue;
        };

        let number: usize = number
            .trim()
            .parse()
            .map_err(|_| AtCoderError::Parse(format!("invalid sample number: {heading}")))?;

        let pre = section
            .select(&pre_selector)
            .next()
            .ok_or_else(|| AtCoderError::Parse(format!("sample content not found: {heading}")))?;

        let content = pre.text().collect::<String>();

        let previous = if is_input {
            inputs.insert(number, content)
        } else {
            outputs.insert(number, content)
        };

        if previous.is_some() {
            return Err(AtCoderError::Parse(format!(
                "duplicate sample number: {heading}"
            )));
        }
    }

    if inputs.is_empty() && outputs.is_empty() {
        if statement_confidently_has_no_normal_samples(&statement, &section_selector, &h3_selector)
        {
            return Ok(Vec::new());
        }

        return Err(AtCoderError::Parse(
            "no normal samples found and the selected statement does not identify a known zero-sample problem"
                .to_string(),
        ));
    }

    // 入力例と出力例の個数が違うなら
    // parser側の異常として扱う。
    if inputs.len() != outputs.len() {
        return Err(AtCoderError::Parse(
            "sample input/output count mismatch".to_string(),
        ));
    }

    if inputs.keys().copied().ne(1..=inputs.len()) {
        return Err(AtCoderError::Parse(
            "sample numbers must be consecutive starting at 1".to_string(),
        ));
    }

    let mut samples = Vec::new();

    for (number, input) in inputs {
        let output = outputs
            .remove(&number)
            .ok_or_else(|| AtCoderError::Parse(format!("sample output {number} not found")))?;

        samples.push(Sample { input, output });
    }

    if !outputs.is_empty() {
        return Err(AtCoderError::Parse(
            "sample output without matching input".to_string(),
        ));
    }

    Ok(samples)
}

fn statement_confidently_has_no_normal_samples(
    statement: &scraper::ElementRef<'_>,
    section_selector: &Selector,
    h3_selector: &Selector,
) -> bool {
    for section in statement.select(section_selector) {
        let Some(h3) = section.select(h3_selector).next() else {
            continue;
        };
        let heading = h3.text().collect::<String>();
        let heading = heading.trim();
        if !matches!(heading, "問題文" | "Problem Statement") {
            continue;
        }

        let text = section
            .text()
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>()
            .join(" ");
        let Some(body) = text.strip_prefix(heading).map(str::trim_start) else {
            continue;
        };

        if heading == "Problem Statement" {
            let body = body.to_ascii_lowercase();
            let Some(rest) = body.strip_prefix("this is an interactive problem") else {
                continue;
            };
            return rest.is_empty()
                || rest.starts_with(|character: char| {
                    character.is_whitespace() || matches!(character, '(' | '.' | ',')
                });
        }

        let compact = body
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let Some(rest) = compact.strip_prefix("この問題はインタラクティブな問題")
        else {
            continue;
        };
        return rest.starts_with("です") || (rest.starts_with('（') && rest.contains("）です"));
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, mpsc};

    struct ObservedWireRequest {
        method: String,
        path: String,
        cookie_present: bool,
        cookie_matches: bool,
    }

    fn read_wire_request(
        stream: &mut TcpStream,
        expected_cookie: Option<&str>,
    ) -> ObservedWireRequest {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).expect("request read failed");
            assert!(read != 0, "request ended before its headers completed");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            assert!(
                bytes.len() <= 64 * 1024,
                "request headers exceeded the test limit"
            );
        };
        let head = std::str::from_utf8(&bytes[..header_end])
            .expect("request headers were not valid UTF-8");
        let mut lines = head.split("\r\n");
        let mut request_line = lines
            .next()
            .expect("request line missing")
            .split_ascii_whitespace();
        let method = request_line
            .next()
            .expect("request method missing")
            .to_string();
        let path = request_line
            .next()
            .expect("request path missing")
            .to_string();
        let mut cookie_present = false;
        let mut cookie_matches = false;
        let mut content_length = 0usize;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("cookie") {
                cookie_present = true;
                cookie_matches = expected_cookie.is_some_and(|expected| value.trim() == expected);
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = value
                    .trim()
                    .parse()
                    .expect("Content-Length was not numeric");
            }
        }
        while bytes.len() < header_end + content_length {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).expect("request body read failed");
            assert!(read != 0, "request body ended early");
            bytes.extend_from_slice(&chunk[..read]);
        }
        ObservedWireRequest {
            method,
            path,
            cookie_present,
            cookie_matches,
        }
    }

    fn write_wire_response(stream: &mut TcpStream, status: &str, headers: &[&str]) {
        let mut response =
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n");
        for header in headers {
            response.push_str(header);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        stream
            .write_all(response.as_bytes())
            .expect("response write failed");
    }

    fn write_wire_response_with_body(
        stream: &mut TcpStream,
        status: &str,
        headers: &[&str],
        body: &str,
    ) {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for header in headers {
            response.push_str(header);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        response.push_str(body);
        stream
            .write_all(response.as_bytes())
            .expect("response write failed");
    }

    fn local_origin(listener: &TcpListener) -> reqwest::Url {
        reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap()
    }

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    fn anonymous_session() -> Arc<auth::SessionAuth> {
        auth::SessionAuth::from_snapshot(&auth::AuthSnapshot::Missing)
    }

    fn managed_auth_location(root: &std::path::Path, value: &str) -> crate::paths::CookieLocation {
        let platform_base = root.join("platform-state");
        let state_dir = platform_base.join("atc").join("state");
        let location = crate::paths::CookieLocation {
            platform_base,
            file: state_dir.join("cookie"),
            state_dir,
        };
        std::fs::create_dir_all(&location.state_dir).unwrap();
        std::fs::write(&location.file, value).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&location.file, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        location
    }

    fn provider(session: Arc<auth::SessionAuth>) -> Arc<SessionCookieProvider> {
        Arc::new(SessionCookieProvider::for_atcoder(session))
    }

    #[test]
    fn request_time_cookie_header_is_sensitive_and_redacted() {
        let secret = "REVEL_SESSION=do-not-print";
        let auth = auth::SessionAuth::configured_for_test(secret);
        let cookie = auth.cookie_header().unwrap();

        assert!(cookie.to_str().is_ok_and(|value| value == secret));
        assert!(cookie.is_sensitive());
        assert!(!format!("{cookie:?}").contains(secret));
        assert!(!format!("{auth:?}").contains(secret));
    }

    #[test]
    fn anonymous_provider_has_no_cookie() {
        let provider = provider(anonymous_session());
        let url = reqwest::Url::parse(BASE_URL).unwrap();
        assert!(provider.cookies(&url).is_none());
    }

    fn observe_provider(provider: &SessionCookieProvider, values: &[&str], url: &str) {
        let headers = values
            .iter()
            .map(|value| HeaderValue::from_str(value).unwrap())
            .collect::<Vec<_>>();
        provider.set_cookies(
            &mut headers.iter(),
            &reqwest::Url::parse(url).expect("valid provider test URL"),
        );
    }

    #[test]
    fn configured_provider_follows_rotation_for_the_next_physical_request() {
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=request-a");
        let provider = provider(Arc::clone(&session));
        let url = reqwest::Url::parse("https://atcoder.jp/contests/abc500/tasks").unwrap();

        assert!(provider.cookies(&url).is_some_and(|cookie| {
            cookie.as_bytes() == b"REVEL_SESSION=request-a" && cookie.is_sensitive()
        }));
        observe_provider(
            &provider,
            &["REVEL_SESSION=request-b; Path=/; Secure; HttpOnly"],
            url.as_str(),
        );
        assert!(provider.cookies(&url).is_some_and(|cookie| {
            cookie.as_bytes() == b"REVEL_SESSION=request-b" && cookie.is_sensitive()
        }));
        assert!(session.credential_matches_for_test("REVEL_SESSION=request-b"));
    }

    #[test]
    fn transport_request_evolution_sends_a_then_b_on_the_wire() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let server = std::thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().unwrap();
            let first = read_wire_request(&mut first_stream, Some("REVEL_SESSION=wire-a"));
            write_wire_response(
                &mut first_stream,
                "200 OK",
                &["Set-Cookie: REVEL_SESSION=wire-b; Path=/"],
            );

            let (mut second_stream, _) = listener.accept().unwrap();
            let second = read_wire_request(&mut second_stream, Some("REVEL_SESSION=wire-b"));
            write_wire_response(&mut second_stream, "200 OK", &[]);
            (first, second)
        });
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=wire-a");
        let http = HttpSource::new_for_test_origin(Arc::clone(&session), &origin).unwrap();

        assert!(
            http.send(http.client.get(origin.join("one").unwrap()))
                .is_ok()
        );
        assert!(
            http.send(http.client.get(origin.join("two").unwrap()))
                .is_ok()
        );

        let (first, second) = server.join().unwrap();
        assert_eq!(
            (first.method.as_str(), first.path.as_str()),
            ("GET", "/one")
        );
        assert!(first.cookie_present && first.cookie_matches);
        assert_eq!(
            (second.method.as_str(), second.path.as_str()),
            ("GET", "/two")
        );
        assert!(second.cookie_present && second.cookie_matches);
        assert!(session.credential_matches_for_test("REVEL_SESSION=wire-b"));
    }

    #[test]
    fn transport_exchange_lock_holds_second_request_until_first_rotates() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let (first_seen_tx, first_seen_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().unwrap();
            let first = read_wire_request(&mut first_stream, Some("REVEL_SESSION=serial-wire-a"));
            first_seen_tx.send(()).unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut early_second = None;
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request =
                            read_wire_request(&mut stream, Some("REVEL_SESSION=serial-wire-a"));
                        write_wire_response(&mut stream, "500 Internal Server Error", &[]);
                        early_second = Some(request);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("second accept failed: {error}"),
                }
                match release_first_rx.try_recv() {
                    Ok(()) => break,
                    Err(mpsc::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(mpsc::TryRecvError::Disconnected) => panic!("release channel disconnected"),
                }
            }
            write_wire_response(
                &mut first_stream,
                "200 OK",
                &["Set-Cookie: REVEL_SESSION=serial-wire-b; Path=/"],
            );
            if early_second.is_some() {
                return (first, early_second, None);
            }

            listener.set_nonblocking(false).unwrap();
            let (mut second_stream, _) = listener.accept().unwrap();
            let second = read_wire_request(&mut second_stream, Some("REVEL_SESSION=serial-wire-b"));
            write_wire_response(&mut second_stream, "200 OK", &[]);
            (first, None, Some(second))
        });
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=serial-wire-a");
        let http = Arc::new(HttpSource::new_for_test_origin(session, &origin).unwrap());
        let first_http = Arc::clone(&http);
        let first_url = origin.join("first").unwrap();
        let first = std::thread::spawn(move || first_http.send(first_http.client.get(first_url)));
        first_seen_rx.recv().unwrap();

        let second_http = Arc::clone(&http);
        let second_url = origin.join("second").unwrap();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second = std::thread::spawn(move || {
            let result = second_http.send(second_http.client.get(second_url));
            second_done_tx.send(result.is_ok()).unwrap();
            result
        });
        assert!(
            second_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        release_first_tx.send(()).unwrap();
        assert!(first.join().unwrap().is_ok());
        assert!(second.join().unwrap().is_ok());

        let (first, early_second, second) = server.join().unwrap();
        assert!(first.cookie_present && first.cookie_matches);
        assert!(
            early_second.is_none(),
            "R2 reached the server before R1 completed"
        );
        let second = second.expect("R2 did not reach the server after R1 completed");
        assert!(second.cookie_present && second.cookie_matches);
    }

    #[test]
    fn same_origin_redirect_hop_uses_the_rotated_cookie_and_ignores_foreign_response() {
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=redirect-a");
        let provider = provider(Arc::clone(&session));
        let first_hop = reqwest::Url::parse("https://atcoder.jp/contests/abc500/redirect").unwrap();
        assert!(
            provider
                .cookies(&first_hop)
                .is_some_and(|cookie| { cookie.as_bytes() == b"REVEL_SESSION=redirect-a" })
        );

        observe_provider(
            &provider,
            &["REVEL_SESSION=redirect-b; Path=/"],
            first_hop.as_str(),
        );
        let second_hop = reqwest::Url::parse("https://atcoder.jp/contests/abc500/tasks").unwrap();
        assert!(
            provider
                .cookies(&second_hop)
                .is_some_and(|cookie| { cookie.as_bytes() == b"REVEL_SESSION=redirect-b" })
        );

        observe_provider(
            &provider,
            &["REVEL_SESSION=foreign-c; Path=/"],
            "https://example.com/redirect-target",
        );
        assert!(
            provider
                .cookies(&second_hop)
                .is_some_and(|cookie| { cookie.as_bytes() == b"REVEL_SESSION=redirect-b" })
        );
    }

    #[test]
    fn transport_redirect_uses_b_on_same_origin_next_hop() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let server = std::thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().unwrap();
            let first = read_wire_request(&mut first_stream, Some("REVEL_SESSION=redirect-wire-a"));
            write_wire_response(
                &mut first_stream,
                "302 Found",
                &[
                    "Location: /next",
                    "Set-Cookie: REVEL_SESSION=redirect-wire-b; Path=/",
                ],
            );

            let (mut second_stream, _) = listener.accept().unwrap();
            let second =
                read_wire_request(&mut second_stream, Some("REVEL_SESSION=redirect-wire-b"));
            write_wire_response(&mut second_stream, "200 OK", &[]);
            (first, second)
        });
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=redirect-wire-a");
        let http = HttpSource::new_for_test_origin(session, &origin).unwrap();

        let response = http
            .send(http.client.get(origin.join("start").unwrap()))
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.url().path(), "/next");

        let (first, second) = server.join().unwrap();
        assert_eq!(first.path, "/start");
        assert!(first.cookie_present && first.cookie_matches);
        assert_eq!(second.path, "/next");
        assert!(second.cookie_present && second.cookie_matches);
    }

    #[test]
    fn transport_foreign_redirect_sends_no_cookie_and_ignores_foreign_mutation() {
        let trusted_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let trusted_origin = local_origin(&trusted_listener);
        let foreign_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let foreign_origin = local_origin(&foreign_listener);
        let foreign_target = foreign_origin.join("landing").unwrap().to_string();
        let trusted_server = std::thread::spawn(move || {
            let (mut start_stream, _) = trusted_listener.accept().unwrap();
            let start = read_wire_request(&mut start_stream, Some("REVEL_SESSION=foreign-wire-a"));
            let location = format!("Location: {foreign_target}");
            write_wire_response(
                &mut start_stream,
                "302 Found",
                &[
                    location.as_str(),
                    "Set-Cookie: REVEL_SESSION=foreign-wire-b; Path=/",
                ],
            );

            let (mut after_stream, _) = trusted_listener.accept().unwrap();
            let after = read_wire_request(&mut after_stream, Some("REVEL_SESSION=foreign-wire-b"));
            write_wire_response(&mut after_stream, "200 OK", &[]);
            (start, after)
        });
        let foreign_server = std::thread::spawn(move || {
            let (mut stream, _) = foreign_listener.accept().unwrap();
            let request = read_wire_request(&mut stream, None);
            write_wire_response(
                &mut stream,
                "200 OK",
                &["Set-Cookie: REVEL_SESSION=foreign-wire-c; Path=/"],
            );
            request
        });
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=foreign-wire-a");
        let http = HttpSource::new_for_test_origin(Arc::clone(&session), &trusted_origin).unwrap();

        assert!(
            http.send(http.client.get(trusted_origin.join("start").unwrap()))
                .is_ok()
        );
        assert!(
            http.send(http.client.get(trusted_origin.join("after").unwrap()))
                .is_ok()
        );

        let (start, after) = trusted_server.join().unwrap();
        let foreign = foreign_server.join().unwrap();
        assert!(start.cookie_present && start.cookie_matches);
        assert!(!foreign.cookie_present);
        assert!(after.cookie_present && after.cookie_matches);
        assert!(session.credential_matches_for_test("REVEL_SESSION=foreign-wire-b"));
    }

    #[test]
    fn transport_baseline_post_and_discovery_use_a_b_then_c_with_one_post() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let server = std::thread::spawn(move || {
            let (mut baseline_stream, _) = listener.accept().unwrap();
            let baseline =
                read_wire_request(&mut baseline_stream, Some("REVEL_SESSION=submit-wire-a"));
            write_wire_response(
                &mut baseline_stream,
                "200 OK",
                &["Set-Cookie: REVEL_SESSION=submit-wire-b; Path=/"],
            );

            let (mut post_stream, _) = listener.accept().unwrap();
            let post = read_wire_request(&mut post_stream, Some("REVEL_SESSION=submit-wire-b"));
            write_wire_response(
                &mut post_stream,
                "302 Found",
                &[
                    "Location: /contests/abc500/submissions/me",
                    "Set-Cookie: REVEL_SESSION=submit-wire-c; Path=/",
                ],
            );

            let (mut discovery_stream, _) = listener.accept().unwrap();
            let discovery =
                read_wire_request(&mut discovery_stream, Some("REVEL_SESSION=submit-wire-c"));
            write_wire_response(&mut discovery_stream, "200 OK", &[]);
            (baseline, post, discovery)
        });
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=submit-wire-a");
        let http = HttpSource::new_for_test_origin(Arc::clone(&session), &origin).unwrap();
        let post_request = http
            .submit_client
            .get()
            .unwrap()
            .post(origin.join("contests/abc500/submit").unwrap())
            .form(&[("csrf_token", "redacted-test-token")]);
        let post_gate = AtomicUsize::new(0);

        assert!(
            http.send(
                http.client
                    .get(origin.join("contests/abc500/submissions/me").unwrap())
            )
            .is_ok()
        );
        let post_response = match http.send_authenticated_once(post_request, &|| {
            post_gate.fetch_add(1, Ordering::SeqCst);
            true
        }) {
            AuthenticatedSendResult::Sent(Ok(response)) => response,
            AuthenticatedSendResult::Sent(Err(_)) => panic!("physical POST transport failed"),
            AuthenticatedSendResult::AuthenticationRequired => {
                panic!("authentication disappeared before POST")
            }
            AuthenticatedSendResult::CancelledBeforePost => panic!("POST gate was cancelled"),
        };
        assert_eq!(post_response.status(), StatusCode::FOUND);
        assert!(
            http.send(
                http.client
                    .get(origin.join("contests/abc500/submissions/me").unwrap())
            )
            .is_ok()
        );

        let (baseline, post, discovery) = server.join().unwrap();
        assert_eq!(
            (baseline.method.as_str(), baseline.path.as_str()),
            ("GET", "/contests/abc500/submissions/me")
        );
        assert!(baseline.cookie_present && baseline.cookie_matches);
        assert_eq!(
            (post.method.as_str(), post.path.as_str()),
            ("POST", "/contests/abc500/submit")
        );
        assert!(post.cookie_present && post.cookie_matches);
        assert_eq!(
            (discovery.method.as_str(), discovery.path.as_str()),
            ("GET", "/contests/abc500/submissions/me")
        );
        assert!(discovery.cookie_present && discovery.cookie_matches);
        assert_eq!(post_gate.load(Ordering::SeqCst), 1);
        assert!(session.credential_matches_for_test("REVEL_SESSION=submit-wire-c"));
    }

    #[test]
    fn transport_baseline_deletion_rejects_post_without_consuming_its_gate() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let server = std::thread::spawn(move || {
            let (mut baseline_stream, _) = listener.accept().unwrap();
            let baseline =
                read_wire_request(&mut baseline_stream, Some("REVEL_SESSION=delete-wire-a"));
            write_wire_response(
                &mut baseline_stream,
                "200 OK",
                &["Set-Cookie: REVEL_SESSION=; Path=/; Max-Age=0"],
            );

            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(200);
            let mut unexpected_post = false;
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        unexpected_post = true;
                        let _ = read_wire_request(&mut stream, None);
                        write_wire_response(&mut stream, "500 Internal Server Error", &[]);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("unexpected POST accept failed: {error}"),
                }
            }
            (baseline, unexpected_post)
        });
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=delete-wire-a");
        let http = HttpSource::new_for_test_origin(Arc::clone(&session), &origin).unwrap();
        let post_request = http
            .submit_client
            .get()
            .unwrap()
            .post(origin.join("contests/abc500/submit").unwrap())
            .form(&[("csrf_token", "redacted-test-token")]);
        let post_gate = AtomicUsize::new(0);

        assert!(
            http.send(
                http.client
                    .get(origin.join("contests/abc500/submissions/me").unwrap())
            )
            .is_ok()
        );
        let result = http.send_authenticated_once(post_request, &|| {
            post_gate.fetch_add(1, Ordering::SeqCst);
            true
        });

        assert!(matches!(
            result,
            AuthenticatedSendResult::AuthenticationRequired
        ));
        assert_eq!(post_gate.load(Ordering::SeqCst), 0);
        assert_eq!(session.kind(), auth::SessionAuthKind::ServerDeleted);
        let (baseline, unexpected_post) = server.join().unwrap();
        assert!(baseline.cookie_present && baseline.cookie_matches);
        assert!(!unexpected_post, "a physical POST reached the server");
    }

    #[test]
    fn persistence_and_warning_sink_failures_keep_outcome_and_do_not_repeat_exchange() {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected warning sink failure",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected warning sink failure",
                ))
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let platform_base = temp.path().join("platform-state");
        let state_dir = platform_base.join("atc").join("state");
        let cookie_file = state_dir.join("cookie");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(&cookie_file, "REVEL_SESSION=persist-a").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&cookie_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let location = crate::paths::CookieLocation {
            platform_base,
            state_dir: state_dir.clone(),
            file: cookie_file.clone(),
        };
        let session = auth::SessionAuth::load_from_location_for_test(&location);
        std::fs::create_dir(state_dir.join(".cookie.lock")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_wire_request(&mut stream, Some("REVEL_SESSION=persist-a"));
            write_wire_response(
                &mut stream,
                "200 OK",
                &["Set-Cookie: REVEL_SESSION=persist-b; Path=/"],
            );
            request
        });
        let http = HttpSource::new_for_test_origin(Arc::clone(&session), &origin).unwrap();
        let exchanges = AtomicUsize::new(0);
        let warnings = AtomicUsize::new(0);
        let mut warning_writer = FailingWriter;

        let outcome = http.exchange_with_warning_sink(
            || {
                exchanges.fetch_add(1, Ordering::SeqCst);
                http.client.get(origin.join("warning").unwrap()).send()
            },
            |warning| {
                warnings.fetch_add(1, Ordering::SeqCst);
                write_auth_persistence_warning(&mut warning_writer, warning);
            },
        );

        assert_eq!(outcome.unwrap().status(), StatusCode::OK);
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(warnings.load(Ordering::SeqCst), 1);
        let request = server.join().unwrap();
        assert!(request.cookie_present && request.cookie_matches);
        assert!(session.credential_matches_for_test("REVEL_SESSION=persist-b"));
        assert!(
            std::fs::read_to_string(cookie_file)
                .is_ok_and(|value| value == "REVEL_SESSION=persist-a")
        );
    }

    #[test]
    fn post_builder_does_not_capture_cookie_before_baseline_rotation() {
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=baseline-a");
        let provider = provider(Arc::clone(&session));
        let client = build_submit_http_client(Arc::clone(&provider)).unwrap();
        let request = client
            .post("https://atcoder.jp/contests/abc500/submit")
            .form(&[("csrf_token", "redacted-test-token")])
            .build()
            .unwrap();

        assert!(request.headers().get(reqwest::header::COOKIE).is_none());
        observe_provider(
            &provider,
            &["REVEL_SESSION=baseline-b; Path=/"],
            "https://atcoder.jp/contests/abc500/submissions/me",
        );
        let post_url = request.url();
        assert!(
            provider
                .cookies(post_url)
                .is_some_and(|cookie| { cookie.as_bytes() == b"REVEL_SESSION=baseline-b" })
        );
    }

    #[test]
    fn post_rotation_is_visible_to_discovery_and_foreign_responses_are_ignored() {
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=post-b");
        let provider = provider(Arc::clone(&session));
        observe_provider(
            &provider,
            &["REVEL_SESSION=post-c; Path=/"],
            "https://atcoder.jp/contests/abc500/submit",
        );
        observe_provider(
            &provider,
            &["REVEL_SESSION=foreign-x; Path=/"],
            "https://example.com/redirect-target",
        );

        let discovery =
            reqwest::Url::parse("https://atcoder.jp/contests/abc500/submissions/me").unwrap();
        assert!(
            provider
                .cookies(&discovery)
                .is_some_and(|cookie| { cookie.as_bytes() == b"REVEL_SESSION=post-c" })
        );
    }

    #[test]
    fn same_session_exchange_lock_serializes_read_response_update_cycles() {
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=serial-a");
        let first_session = Arc::clone(&session);
        let (first_started_tx, first_started_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let first = std::thread::spawn(move || {
            first_session.with_exchange(|| {
                assert!(first_session.credential_matches_for_test("REVEL_SESSION=serial-a"));
                first_started_tx.send(()).unwrap();
                release_first_rx.recv().unwrap();
                let headers = [HeaderValue::from_static("REVEL_SESSION=serial-b; Path=/")];
                first_session.observe_set_cookie_batch(
                    headers.iter(),
                    &reqwest::Url::parse("https://atcoder.jp/").unwrap(),
                );
            });
        });
        first_started_rx.recv().unwrap();

        let second_session = Arc::clone(&session);
        let (second_observed_tx, second_observed_rx) = mpsc::channel();
        let second = std::thread::spawn(move || {
            second_session.with_exchange(|| {
                second_observed_tx
                    .send(second_session.credential_matches_for_test("REVEL_SESSION=serial-b"))
                    .unwrap();
            });
        });
        assert!(
            second_observed_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
        release_first_tx.send(()).unwrap();
        assert!(second_observed_rx.recv().unwrap());
        first.join().unwrap();
        second.join().unwrap();
    }

    #[test]
    fn snapshot_construction_configures_or_omits_cookie_without_disk_loading() {
        let marker = "REVEL_SESSION=snapshot-client-marker";
        let configured = auth::AuthSnapshot::configured_for_test(marker);
        let configured_client = AtCoderClient::from_auth_snapshot(&configured).unwrap();
        assert!(configured_client.credential_matches_for_test(marker));

        let missing_client =
            AtCoderClient::from_auth_snapshot(&auth::AuthSnapshot::Missing).unwrap();
        assert!(!missing_client.credential_matches_for_test(marker));
        let invalid_client = AtCoderClient::from_auth_snapshot(&auth::AuthSnapshot::Invalid(
            auth::AuthLoadError::from_io(&std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "test invalid cookie",
            )),
        ))
        .unwrap();
        assert!(!invalid_client.credential_matches_for_test(marker));
    }

    #[test]
    fn submit_client_builder_constructs_with_no_redirects_and_retries_disabled() {
        build_submit_http_client(provider(anonymous_session()))
            .expect("submit client configuration should construct without making a request");
    }

    #[test]
    fn http_source_construction_leaves_submit_client_uninitialized() {
        let http =
            HttpSource::new(anonymous_session()).expect("normal HTTP client should construct");
        let cached = http
            .submit_client
            .client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        assert!(cached.is_none());
    }

    #[test]
    fn lazy_submit_client_builds_once_and_reuses_the_cached_client() {
        let lazy = LazySubmitClient::new(provider(anonymous_session()));
        let builds = std::cell::Cell::new(0);

        lazy.get_or_try_init_with(|provider| {
            builds.set(builds.get() + 1);
            build_submit_http_client(provider)
        })
        .expect("first access should build the submit client");
        lazy.get_or_try_init_with(|_| {
            builds.set(builds.get() + 1);
            build_submit_http_client(provider(anonymous_session()))
        })
        .expect("later access should reuse the submit client");

        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn lazy_submit_client_build_failure_does_not_poison_later_access() {
        let lazy = LazySubmitClient::new(provider(anonymous_session()));
        let error = lazy
            .get_or_try_init_with(|_| Err(AtCoderError::Parse("injected failure".to_string())))
            .expect_err("injected submit client construction should fail");

        assert!(matches!(error, AtCoderError::Parse(_)));
        lazy.get_or_try_init_with(build_submit_http_client)
            .expect("a later submit client construction should still succeed");
    }

    #[test]
    fn parses_contest_from_tasks_fixture() {
        let client = AtCoderClient::fixture(fixture_root());

        let contest = client
            .fetch_contest("abc466")
            .expect("contest fixture should parse");

        assert_eq!(contest.contest_id, "abc466");
        assert_eq!(contest.problems.len(), 7);
        assert_eq!(contest.problems[0].index, "A");
        assert_eq!(contest.problems[0].title, "Compromise");
        assert_eq!(contest.problems[0].task_id, "abc466_a");
        assert_eq!(
            contest.problems[0].url,
            "https://atcoder.jp/contests/abc466/tasks/abc466_a"
        );
    }

    #[test]
    fn parses_samples_from_problem_fixture() {
        let client = AtCoderClient::fixture(fixture_root());
        let problem = ProblemOutline {
            index: "A".to_string(),
            title: "Compromise".to_string(),
            task_id: "abc466_a".to_string(),
            url: "https://atcoder.jp/contests/abc466/tasks/abc466_a".to_string(),
        };

        let samples = client
            .fetch_samples(&problem)
            .expect("problem fixture should parse");

        assert_eq!(samples.len(), 3);
        assert_eq!(
            samples[0],
            Sample {
                input: "4\n2 0 -1 2\n".to_string(),
                output: "No\n".to_string(),
            }
        );
    }

    #[test]
    fn recognized_interactive_statement_without_samples_returns_empty_samples() {
        let html = r#"
            <div id="task-statement">
                <span class="lang-ja">
                    <div class="part"><section><h3>問題文</h3><p>この問題はインタラクティブな問題です。</p></section></div>
                </span>
            </div>
        "#;

        let samples = parse_samples(html).expect("interactive statement should parse");

        assert!(samples.is_empty());
    }

    #[test]
    fn statement_without_samples_or_positive_zero_evidence_is_a_parse_error() {
        let html = r#"
            <div id="task-statement">
                <span class="lang-en">
                    <div class="part"><section><h3>Problem Statement</h3><p>Solve it.</p></section></div>
                </span>
            </div>
        "#;

        let error = parse_samples(html).expect_err("unrecognized absence must not become zero");

        assert!(error.to_string().contains("no normal samples found"));
    }

    #[test]
    fn negated_or_unrelated_interactive_wording_is_not_zero_sample_evidence() {
        for (language, heading, body) in [
            (
                "lang-en",
                "Problem Statement",
                "This is not an interactive problem.",
            ),
            (
                "lang-en",
                "Problem Statement",
                "Unlike an interactive problem, this task uses ordinary input.",
            ),
            (
                "lang-ja",
                "問題文",
                "この問題はインタラクティブな問題ではありません。",
            ),
            (
                "lang-ja",
                "問題文",
                "インタラクティブな問題とは異なり、通常の入力を用います。",
            ),
        ] {
            let html = format!(
                r#"<div id="task-statement"><span class="{language}">
                    <div class="part"><section><h3>{heading}</h3><p>{body}</p></section></div>
                </span></div>"#
            );

            let error = parse_samples(&html)
                .expect_err("negated or unrelated wording must not establish zero samples");
            assert!(
                error.to_string().contains("no normal samples found"),
                "{body}"
            );
        }
    }

    #[test]
    fn abc466_interactive_fixture_confidently_has_zero_samples() {
        let client = AtCoderClient::fixture(fixture_root());
        let contest = client.fetch_contest("abc466").unwrap();
        let problem = &contest.problems[2];

        let samples = client.fetch_samples(problem).unwrap();

        assert_eq!(problem.index, "C");
        assert!(samples.is_empty());
    }

    #[test]
    fn missing_fixture_reports_its_path() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let client = AtCoderClient::fixture(temp.path());

        let error = client
            .fetch_contest("abc999")
            .expect_err("missing fixture should fail");

        match error {
            AtCoderError::Fixture { path, source } => {
                assert_eq!(path, temp.path().join("contests").join("abc999.html"));
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn malformed_tasks_fixture_is_a_parse_error() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let contests = temp.path().join("contests");
        std::fs::create_dir(&contests).expect("contest fixture directory should be created");
        std::fs::write(contests.join("broken.html"), "<html><body></body></html>")
            .expect("fixture should be written");
        let client = AtCoderClient::fixture(temp.path());

        let error = client
            .fetch_contest("broken")
            .expect_err("malformed fixture should fail");

        assert!(matches!(error, AtCoderError::Parse(message) if message == "no problems found"));
    }

    #[test]
    fn fixture_mode_ignores_problem_url_and_never_uses_http() {
        let client = AtCoderClient::fixture(fixture_root());
        let problem = ProblemOutline {
            index: "A".to_string(),
            title: "Compromise".to_string(),
            task_id: "abc466_a".to_string(),
            url: "http://127.0.0.1:1/must-not-be-requested".to_string(),
        };

        let samples = client
            .fetch_samples(&problem)
            .expect("fixture lookup should not inspect or request the URL");

        assert_eq!(samples.len(), 3);
        assert!(matches!(client.source, Source::Fixture(_)));
    }

    #[test]
    fn incomplete_sample_pair_is_a_parse_error() {
        let html = r#"
            <div id="task-statement">
                <span class="lang-en">
                    <div class="part"><section><h3>Sample Input 1</h3><pre>1\n</pre></section></div>
                </span>
            </div>
        "#;

        let error = parse_samples(html).expect_err("incomplete sample should fail");

        assert!(matches!(error, AtCoderError::Parse(message) if message.contains("mismatch")));
    }

    #[test]
    fn request_interval_is_measured_between_request_starts() {
        let previous = Instant::now();

        assert_eq!(
            remaining_request_interval(Some(previous), previous + Duration::from_millis(125)),
            Some(Duration::from_millis(375))
        );
        assert_eq!(
            remaining_request_interval(Some(previous), previous + Duration::from_millis(500)),
            None
        );
        assert_eq!(remaining_request_interval(None, previous), None);
    }

    #[test]
    fn interruptible_wait_observes_cancellation_without_waiting_for_deadline() {
        let checks = std::cell::Cell::new(0usize);
        let started = Instant::now();
        let completed = interruptible_sleep(Duration::from_secs(60), &|| {
            checks.set(checks.get() + 1);
            checks.get() < 3
        });

        assert!(!completed);
        assert!(checks.get() >= 3);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn shared_rate_limiter_reserves_strictly_spaced_slots_without_holding_during_sleep() {
        let last_request = Arc::new(Mutex::new(None));
        let barrier = Arc::new(Barrier::new(5));
        let (slot_tx, slot_rx) = mpsc::channel();
        let interval = Duration::from_millis(8);
        let mut workers = Vec::new();
        for _ in 0..4 {
            let last_request = Arc::clone(&last_request);
            let barrier = Arc::clone(&barrier);
            let slot_tx = slot_tx.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                let slot = reserve_request_slot_until(&last_request, interval, &|| true)
                    .expect("uncancelled waiter should reserve a slot");
                slot_tx.send(slot).unwrap();
            }));
        }
        barrier.wait();
        drop(slot_tx);
        for worker in workers {
            worker.join().unwrap();
        }

        let mut slots = slot_rx.into_iter().collect::<Vec<_>>();
        slots.sort_unstable();
        assert_eq!(slots.len(), 4);
        for pair in slots.windows(2) {
            assert!(
                pair[1].duration_since(pair[0]) >= interval,
                "reserved request slots were too close: {:?}",
                pair[1].duration_since(pair[0])
            );
        }
    }

    #[test]
    fn rate_limit_waiter_releases_mutex_and_cancels_without_request() {
        let previous = Instant::now();
        let last_request = Arc::new(Mutex::new(Some(previous)));
        let should_continue = Arc::new(AtomicBool::new(true));
        let checks = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::new(AtomicUsize::new(0));
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let worker_last_request = Arc::clone(&last_request);
        let worker_continue = Arc::clone(&should_continue);
        let worker_checks = Arc::clone(&checks);
        let worker_requests = Arc::clone(&request_count);
        let worker = thread::spawn(move || {
            let reserved =
                reserve_request_slot_until(&worker_last_request, Duration::from_secs(5), &|| {
                    if worker_checks.fetch_add(1, Ordering::AcqRel) == 1 {
                        let _ = waiting_tx.send(());
                    }
                    worker_continue.load(Ordering::Acquire)
                });
            if reserved.is_some() {
                worker_requests.fetch_add(1, Ordering::AcqRel);
            }
            reserved
        });
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let lock_started = Instant::now();
        let observed = *last_request.lock().unwrap();
        assert!(lock_started.elapsed() < Duration::from_millis(100));
        assert_eq!(observed, Some(previous));

        let cancel_started = Instant::now();
        should_continue.store(false, Ordering::Release);
        assert_eq!(worker.join().unwrap(), None);
        assert!(cancel_started.elapsed() < Duration::from_millis(500));
        assert_eq!(request_count.load(Ordering::Acquire), 0);
        assert_eq!(*last_request.lock().unwrap(), Some(previous));
        assert!(
            checks.load(Ordering::Acquire) < 20,
            "rate-limit wait busy-looped"
        );
    }

    #[test]
    fn rate_limit_wait_does_not_busy_loop_before_reserving() {
        let interval = Duration::from_millis(35);
        let previous = Instant::now();
        let last_request = Mutex::new(Some(previous));
        let checks = AtomicUsize::new(0);

        let reserved = reserve_request_slot_until(&last_request, interval, &|| {
            checks.fetch_add(1, Ordering::Relaxed);
            true
        })
        .unwrap();

        assert!(reserved.duration_since(previous) >= interval);
        assert!(
            checks.load(Ordering::Relaxed) < 20,
            "rate-limit wait busy-looped"
        );
    }

    #[test]
    fn retry_after_delta_seconds_is_used_with_a_fallback() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(RETRY_AFTER, reqwest::header::HeaderValue::from_static("7"));
        assert_eq!(retry_wait(&headers), Duration::from_secs(7));

        headers.insert(
            RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("invalid"),
        );
        assert_eq!(retry_wait(&headers), DEFAULT_RETRY_WAIT);

        headers.insert(
            RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("18446744073709551615"),
        );
        assert_eq!(retry_wait(&headers), MAX_RETRY_WAIT);
    }

    #[test]
    fn settings_url_identifies_authenticated_session() {
        assert!(is_authenticated_settings_url(
            &reqwest::Url::parse("https://atcoder.jp/settings").unwrap()
        ));

        assert!(!is_authenticated_settings_url(
            &reqwest::Url::parse(
                "https://atcoder.jp/login?continue=https%3A%2F%2Fatcoder.jp%2Fsettings"
            )
            .unwrap()
        ));

        assert!(!is_authenticated_settings_url(
            &reqwest::Url::parse("http://atcoder.jp/settings").unwrap()
        ));

        assert!(!is_authenticated_settings_url(
            &reqwest::Url::parse("https://example.com/settings").unwrap()
        ));
    }

    #[test]
    fn authentication_requires_a_successful_final_settings_response() {
        let settings = reqwest::Url::parse("https://atcoder.jp/settings").unwrap();
        let login = reqwest::Url::parse(
            "https://atcoder.jp/login?continue=https%3A%2F%2Fatcoder.jp%2Fsettings",
        )
        .unwrap();

        assert_eq!(
            classify_authentication_response(StatusCode::OK, &settings).unwrap(),
            AuthenticationStatus::Authenticated
        );
        assert_eq!(
            classify_authentication_response(StatusCode::OK, &login).unwrap(),
            AuthenticationStatus::Unauthenticated
        );
        assert!(matches!(
            classify_authentication_response(StatusCode::FOUND, &settings),
            Err(AtCoderError::UnexpectedAuthenticationStatus(
                StatusCode::FOUND
            ))
        ));
    }

    #[test]
    fn authenticated_username_comes_only_from_the_authenticated_navigation_fixture() {
        let fixture =
            std::fs::read_to_string(fixture_root().join("authentication").join("settings.html"))
                .unwrap();
        assert_eq!(
            parse_authenticated_username(&fixture).as_deref(),
            Some("toppoun")
        );

        let unrelated = r#"
            <nav class="navbar"><div id="navbar-collapse">
              <ul class="nav navbar-nav navbar-right"><li><a href="/settings">Settings</a></li></ul>
            </div></nav>
            <main><a class="username" href="/users/contest_author">contest_author</a></main>
        "#;
        assert_eq!(parse_authenticated_username(unrelated), None);

        for invalid in [
            "/users/",
            "/users/name/extra",
            "/users/name?x=1",
            "/users/name#fragment",
            "/users/non_ascii_é",
        ] {
            let html = format!(
                r#"<nav class="navbar"><div id="navbar-collapse"><ul class="nav navbar-nav navbar-right"><li><a href="{invalid}">account</a></li></ul></div></nav>"#
            );
            assert_eq!(parse_authenticated_username(&html), None);
        }
    }

    #[test]
    fn verification_transport_reports_authenticated_username_and_rotation() {
        let temp = tempfile::tempdir().unwrap();
        let location = managed_auth_location(temp.path(), "REVEL_SESSION=verify-a");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let body =
            std::fs::read_to_string(fixture_root().join("authentication").join("settings.html"))
                .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_wire_request(&mut stream, Some("REVEL_SESSION=verify-a"));
            write_wire_response_with_body(
                &mut stream,
                "200 OK",
                &["Set-Cookie: REVEL_SESSION=verify-b; Path=/"],
                &body,
            );
            request
        });
        let snapshot = auth::AuthSnapshot::load_from(&location);
        let session = auth::SessionAuth::from_snapshot(&snapshot);
        let http = HttpSource::new_for_test_origin(Arc::clone(&session), &origin).unwrap();

        let result = verify_authentication_http(&http, origin.join("settings").unwrap());

        let rotated = auth::AuthSnapshot::load_from(&location);
        assert!(result.matches_snapshot(&rotated));
        let unrelated = auth::AuthSnapshot::configured_for_test("REVEL_SESSION=external-x");
        assert!(!result.matches_snapshot(&unrelated));
        assert!(matches!(
            result,
            AuthenticationVerification::Authenticated {
                username: Some(ref username),
                ref warnings,
                ..
            } if username == "toppoun" && warnings.is_empty()
        ));
        assert_eq!(server.join().unwrap().path, "/settings");
        assert!(session.credential_matches_for_test("REVEL_SESSION=verify-b"));
        assert!(
            std::fs::read_to_string(&location.file)
                .unwrap()
                .starts_with("REVEL_SESSION=verify-b")
        );
    }

    #[test]
    fn verification_keeps_authentication_when_username_body_is_unusable() {
        for body in ["<html><body>no navigation</body></html>", ""] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = local_origin(&listener);
            let owned_body = body.to_string();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_wire_request(&mut stream, Some("REVEL_SESSION=no-name"));
                write_wire_response_with_body(&mut stream, "200 OK", &[], &owned_body);
            });
            let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=no-name");
            let http = HttpSource::new_for_test_origin(session, &origin).unwrap();
            let result = verify_authentication_http(&http, origin.join("settings").unwrap());
            server.join().unwrap();
            assert!(matches!(
                result,
                AuthenticationVerification::Authenticated { username: None, .. }
            ));
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_wire_request(&mut stream, Some("REVEL_SESSION=broken-body"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort",
                )
                .unwrap();
        });
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=broken-body");
        let http = HttpSource::new_for_test_origin(session, &origin).unwrap();
        let result = verify_authentication_http(&http, origin.join("settings").unwrap());
        server.join().unwrap();
        assert!(matches!(
            result,
            AuthenticationVerification::Authenticated { username: None, .. }
        ));
    }

    #[test]
    fn verification_distinguishes_rejection_and_unavailable_responses() {
        fn verify_redirect(final_path: &str, canonical: bool) -> AuthenticationVerification {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = local_origin(&listener);
            let settings = origin.join("settings").unwrap();
            let location = if canonical {
                let mut login = origin.join("login").unwrap();
                login
                    .query_pairs_mut()
                    .append_pair("continue", settings.as_str());
                login.path().to_string() + "?" + login.query().unwrap()
            } else {
                final_path.to_string()
            };
            let redirect = format!("Location: {location}");
            let server = std::thread::spawn(move || {
                let (mut first, _) = listener.accept().unwrap();
                let _ = read_wire_request(&mut first, Some("REVEL_SESSION=classification"));
                write_wire_response(&mut first, "302 Found", &[&redirect]);
                let (mut second, _) = listener.accept().unwrap();
                let _ = read_wire_request(&mut second, Some("REVEL_SESSION=classification"));
                write_wire_response_with_body(&mut second, "200 OK", &[], "login or other");
            });
            let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=classification");
            let http = HttpSource::new_for_test_origin(session, &origin).unwrap();
            let result = verify_authentication_http(&http, settings);
            server.join().unwrap();
            result
        }

        assert!(matches!(
            verify_redirect("/login", true),
            AuthenticationVerification::Rejected { .. }
        ));
        assert!(matches!(
            verify_redirect("/unexpected", false),
            AuthenticationVerification::Unavailable { .. }
        ));

        for status in ["429 Too Many Requests", "500 Internal Server Error"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = local_origin(&listener);
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_wire_request(&mut stream, Some("REVEL_SESSION=status"));
                write_wire_response(&mut stream, status, &[]);
            });
            let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=status");
            let http = HttpSource::new_for_test_origin(session, &origin).unwrap();
            let result = verify_authentication_http(&http, origin.join("settings").unwrap());
            server.join().unwrap();
            assert!(matches!(
                result,
                AuthenticationVerification::Unavailable { .. }
            ));
        }
    }

    #[test]
    fn verification_timeout_is_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_wire_request(&mut stream, Some("REVEL_SESSION=timeout"));
            std::thread::sleep(Duration::from_millis(150));
        });
        let session = auth::SessionAuth::configured_for_test("REVEL_SESSION=timeout");
        let http = HttpSource::new_for_test_origin_with_timeout(
            session,
            &origin,
            Duration::from_millis(30),
        )
        .unwrap();

        let result = verify_authentication_http(&http, origin.join("settings").unwrap());
        server.join().unwrap();
        assert!(matches!(
            result,
            AuthenticationVerification::Unavailable { .. }
        ));
    }

    #[test]
    fn verification_server_deletion_removes_store_without_changing_http_classification() {
        let temp = tempfile::tempdir().unwrap();
        let location = managed_auth_location(temp.path(), "REVEL_SESSION=delete-a");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_wire_request(&mut stream, Some("REVEL_SESSION=delete-a"));
            write_wire_response_with_body(
                &mut stream,
                "200 OK",
                &["Set-Cookie: REVEL_SESSION=; Path=/; Max-Age=0"],
                "<html><body>settings</body></html>",
            );
        });
        let snapshot = auth::AuthSnapshot::load_from(&location);
        let session = auth::SessionAuth::from_snapshot(&snapshot);
        let http = HttpSource::new_for_test_origin(session, &origin).unwrap();

        let result = verify_authentication_http(&http, origin.join("settings").unwrap());
        server.join().unwrap();

        let missing = auth::AuthSnapshot::load_from(&location);
        assert!(result.matches_snapshot(&missing));
        assert!(matches!(
            result,
            AuthenticationVerification::Authenticated {
                username: None,
                ref warnings,
                ..
            } if warnings.is_empty()
        ));
        assert!(!location.file.exists());
        assert!(matches!(
            auth::AuthSnapshot::load_from(&location).as_ref(),
            auth::AuthSnapshot::Missing
        ));
    }

    #[test]
    fn verification_persistence_warning_does_not_change_authenticated_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let location = managed_auth_location(temp.path(), "REVEL_SESSION=warning-a");
        std::fs::create_dir(location.state_dir.join(".cookie.lock")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = local_origin(&listener);
        let body =
            std::fs::read_to_string(fixture_root().join("authentication").join("settings.html"))
                .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_wire_request(&mut stream, Some("REVEL_SESSION=warning-a"));
            write_wire_response_with_body(
                &mut stream,
                "200 OK",
                &["Set-Cookie: REVEL_SESSION=warning-b; Path=/"],
                &body,
            );
        });
        let snapshot = auth::AuthSnapshot::load_from(&location);
        let session = auth::SessionAuth::from_snapshot(&snapshot);
        let http = HttpSource::new_for_test_origin(session, &origin).unwrap();

        let result = verify_authentication_http(&http, origin.join("settings").unwrap());
        server.join().unwrap();

        let unchanged = auth::AuthSnapshot::load_from(&location);
        assert!(result.matches_snapshot(&unchanged));
        assert!(matches!(
            result,
            AuthenticationVerification::Authenticated {
                username: Some(ref username),
                ref warnings,
                ..
            } if username == "toppoun" && warnings == &[auth::AuthPersistenceWarning::Unsafe]
        ));
        assert!(
            std::fs::read_to_string(&location.file)
                .unwrap()
                .starts_with("REVEL_SESSION=warning-a")
        );
    }
}
