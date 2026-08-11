use crate::model::{Contest, Problem, Sample};

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::RETRY_AFTER;

use scraper::{Html, Selector};

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

const BASE_URL: &str = "https://atcoder.jp";

// 正常時も短時間に連打しない
const REQUEST_INTERVAL: Duration = Duration::from_millis(500);

// 429を受けたとき、Retry-Afterが無い場合の待機時間
const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(2);

// 最初のリクエストとは別に何回retryするか
const MAX_429_RETRIES: usize = 3;

#[derive(Debug)]
pub enum AtCoderError {
    Http(reqwest::Error),
    Io(std::io::Error),
    Parse(String),

    // 429がretryしても解消しなかった
    RateLimited { url: String },
}

impl From<reqwest::Error> for AtCoderError {
    fn from(err: reqwest::Error) -> Self {
        AtCoderError::Http(err)
    }
}

impl From<std::io::Error> for AtCoderError {
    fn from(err: std::io::Error) -> Self {
        AtCoderError::Io(err)
    }
}

enum Source {
    Http,
    Fixture(PathBuf),
}

pub struct AtCoderClient {
    client: Client,
    source: Source,
}

impl AtCoderClient {
    pub fn new() -> Result<Self, AtCoderError> {
        let user_agent = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

        let client = Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(10))
            .cookie_store(true)
            .build()?;

        Ok(Self {
            client,
            source: Source::Http,
        })
    }

    pub fn fixture(root: impl Into<PathBuf>) -> Result<Self, AtCoderError> {
        let client = Client::builder().build()?;

        Ok(Self {
            client,
            source: Source::Fixture(root.into()),
        })
    }

    // ============================================================
    // Contest
    // ============================================================

    pub fn fetch_contest(&self, contest_id: &str) -> Result<Contest, AtCoderError> {
        let html = match &self.source {
            Source::Http => {
                let url = format!("{BASE_URL}/contests/{contest_id}/tasks");

                self.get_text(&url)?
            }

            Source::Fixture(root) => {
                let path = root.join("contests").join(format!("{contest_id}.html"));

                std::fs::read_to_string(path)?
            }
        };

        parse_contest(contest_id, &html)
    }

    // ============================================================
    // Samples
    // ============================================================

    pub fn fetch_samples(&self, problem: &Problem) -> Result<Vec<Sample>, AtCoderError> {
        let html = match &self.source {
            Source::Http => self.get_text(&problem.url)?,

            Source::Fixture(root) => {
                let path = root
                    .join("problems")
                    .join(format!("{}.html", problem.task_id));

                std::fs::read_to_string(path)?
            }
        };

        parse_samples(&html)
    }

    // ============================================================
    // HTTP
    // ============================================================

    fn get_text(&self, url: &str) -> Result<String, AtCoderError> {
        for retry_count in 0..=MAX_429_RETRIES {
            let response = self.client.get(url).send()?;

            println!("status: {}", response.status());

            // 429だけ特別扱い
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                println!("429 Too Many Requests");
                if retry_count == MAX_429_RETRIES {
                    return Err(AtCoderError::RateLimited {
                        url: url.to_string(),
                    });
                }

                let wait = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(DEFAULT_RETRY_WAIT);

                thread::sleep(wait);

                continue;
            }

            // 404 / 500などは普通のHTTPエラーとして返す
            let response = response.error_for_status()?;

            let html = response.text()?;

            // 次のrequestを即座に送らない
            thread::sleep(REQUEST_INTERVAL);

            return Ok(html);
        }

        unreachable!()
    }
}

// ============================================================
// Contest Parser
// ============================================================

fn parse_contest(contest_id: &str, html: &str) -> Result<Contest, AtCoderError> {
    let document = Html::parse_document(html);

    let row_selector = Selector::parse("table tbody tr")
        .map_err(|_| AtCoderError::Parse("invalid row selector".to_string()))?;

    let link_selector = Selector::parse("td a")
        .map_err(|_| AtCoderError::Parse("invalid link selector".to_string()))?;

    let mut problems = Vec::new();

    for row in document.select(&row_selector) {
        let mut links = row.select(&link_selector);

        let index_link = links
            .next()
            .ok_or_else(|| AtCoderError::Parse("problem index not found".to_string()))?;

        let title_link = links
            .next()
            .ok_or_else(|| AtCoderError::Parse("problem title not found".to_string()))?;

        let index = index_link.text().collect::<String>().trim().to_string();

        let title = title_link.text().collect::<String>().trim().to_string();

        let href = index_link
            .value()
            .attr("href")
            .ok_or_else(|| AtCoderError::Parse("problem url not found".to_string()))?;

        let task_id = href
            .rsplit('/')
            .next()
            .ok_or_else(|| AtCoderError::Parse("task id not found".to_string()))?
            .to_string();

        let url = format!("{BASE_URL}{href}");

        problems.push(Problem {
            index,
            title,
            task_id,
            url,
        });
    }

    if problems.is_empty() {
        return Err(AtCoderError::Parse("no problems found".to_string()));
    }

    Ok(Contest {
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

        let heading = h3.text().next().unwrap_or("").trim();

        // 「入力例 1」→ ("input", "1")
        // 「出力例 1」→ ("output", "1")
        // それ以外     → None

        let sample_kind = if let Some(number) = heading.strip_prefix(input_prefix) {
            Some(("input", number))
        } else {
            heading
                .strip_prefix(output_prefix)
                .map(|number| ("output", number))
        };

        let Some((kind, number)) = sample_kind else {
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

        match kind {
            "input" => {
                inputs.insert(number, content);
            }

            "output" => {
                outputs.insert(number, content);
            }

            _ => unreachable!(),
        }
    }

    // インタラクティブなど。
    // 通常sampleが無いのはエラーではない。
    if inputs.is_empty() && outputs.is_empty() {
        return Ok(Vec::new());
    }

    // 入力例と出力例の個数が違うなら
    // parser側の異常として扱う。
    if inputs.len() != outputs.len() {
        return Err(AtCoderError::Parse(
            "sample input/output count mismatch".to_string(),
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
