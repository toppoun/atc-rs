use crate::auth;
use crate::model::Sample;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{COOKIE, HeaderMap, HeaderValue, RETRY_AFTER};

use scraper::{Html, Selector};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

// Phase 1 deliberately exposes only a pure layer; production commands do not call it yet.
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
    Auth(std::io::Error),
    InvalidStoredCookie,
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

pub fn authentication_status() -> Result<AuthenticationStatus, AtCoderError> {
    let Some(cookie) = auth::load_cookie().map_err(AtCoderError::Auth)? else {
        return Ok(AuthenticationStatus::NotConfigured);
    };

    let client = build_http_client(Some(cookie))?;

    let response = client
        .get(format!("{BASE_URL}/settings"))
        .send()?
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

            Self::InvalidStoredCookie => {
                write!(formatter, "stored authentication cookie is invalid")
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
            Self::InvalidStoredCookie
            | Self::UnexpectedAuthenticationStatus(_)
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
    last_request: Mutex<Option<Instant>>,
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
        let cookie = auth::load_cookie().map_err(AtCoderError::Auth)?;
        let client = build_http_client(cookie)?;

        Ok(Self {
            source: Source::Http(HttpSource {
                client,
                last_request: Mutex::new(None),
            }),
        })
    }

    pub fn fixture(root: impl Into<PathBuf>) -> Self {
        Self {
            source: Source::Fixture(root.into()),
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
        for retry_count in 0..=MAX_429_RETRIES {
            wait_for_request_slot(http);
            let response = http.client.get(url).send()?;

            // 429だけ特別扱い
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                if retry_count == MAX_429_RETRIES {
                    return Err(AtCoderError::RateLimited {
                        url: url.to_string(),
                    });
                }

                let wait = retry_wait(response.headers());

                thread::sleep(wait);

                continue;
            }

            // 404 / 500などは普通のHTTPエラーとして返す
            let response = response.error_for_status()?;

            let html = response.text()?;

            return Ok(html);
        }

        Err(AtCoderError::RateLimited {
            url: url.to_string(),
        })
    }
}

fn build_http_client(cookie: Option<String>) -> Result<Client, AtCoderError> {
    let user_agent = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
    let authenticated = cookie.is_some();
    let headers = default_headers(cookie)?;

    let mut builder = Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(10))
        .default_headers(headers);

    // Preserve the pre-authentication anonymous cookie-jar behavior. A
    // manually stored Cookie header is deliberately authoritative instead of
    // competing with a second in-memory cookie source.
    if !authenticated {
        builder = builder.cookie_store(true);
    }

    Ok(builder.build()?)
}

fn default_headers(cookie: Option<String>) -> Result<HeaderMap, AtCoderError> {
    let mut headers = HeaderMap::new();

    if let Some(cookie) = cookie {
        let mut cookie =
            HeaderValue::from_str(&cookie).map_err(|_| AtCoderError::InvalidStoredCookie)?;
        cookie.set_sensitive(true);
        headers.insert(COOKIE, cookie);
    }

    Ok(headers)
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

fn wait_for_request_slot(http: &HttpSource) {
    let mut last_request = match http.last_request.lock() {
        Ok(last_request) => last_request,
        Err(poisoned) => poisoned.into_inner(),
    };

    let now = Instant::now();
    if let Some(wait) = remaining_request_interval(*last_request, now) {
        thread::sleep(wait);
    }

    *last_request = Some(Instant::now());
}

fn remaining_request_interval(previous: Option<Instant>, now: Instant) -> Option<Duration> {
    previous
        .and_then(|previous| REQUEST_INTERVAL.checked_sub(now.saturating_duration_since(previous)))
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

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    #[test]
    fn stored_cookie_header_is_sensitive_and_invalid_values_do_not_leak() {
        let secret = "REVEL_SESSION=do-not-print";
        let headers = default_headers(Some(secret.to_string())).unwrap();
        let cookie = headers.get(COOKIE).unwrap();

        assert_eq!(cookie.to_str().unwrap(), secret);
        assert!(cookie.is_sensitive());
        assert!(!format!("{headers:?}").contains(secret));

        let invalid = "REVEL_SESSION=secret\r\nX-Injected: yes";
        let error = default_headers(Some(invalid.to_string())).unwrap_err();
        assert!(matches!(error, AtCoderError::InvalidStoredCookie));
        assert!(!error.to_string().contains("secret"));
        assert!(!format!("{error:?}").contains("secret"));
    }

    #[test]
    fn anonymous_default_headers_have_no_cookie() {
        let headers = default_headers(None).unwrap();
        assert!(!headers.contains_key(COOKIE));
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
}
