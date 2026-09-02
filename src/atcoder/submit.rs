use crate::language::Language;

use scraper::{ElementRef, Html, Selector};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

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
        for option in language_select.select(&option_selector) {
            let id = option
                .value()
                .attr("value")
                .ok_or(SubmitPageError::MalformedPage(
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

            let label = normalized_text(&option);
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

        if languages.is_empty() {
            return Err(SubmitPageError::MalformedPage(
                "task has no language options",
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn current_live_style_dom_parses() {
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

        let page = current_page();
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
}
