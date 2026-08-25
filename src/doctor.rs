use crate::config::{ConfigValueSource, ResolvedConfig};
use crate::language::Language;
use crate::template::{self, SourceTemplateOrigin};
use crate::workspace::{self, ContestMetadataHealth};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod probe;

use probe::{CapturedStream, VersionProbeOutcome, probe_version};

const RUNNER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RUNNER_VERSION_DISPLAY_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagnosticLevel {
    Ok,
    Warn,
    Error,
    Info,
    Skip,
}

impl DiagnosticLevel {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Info => "INFO",
            Self::Skip => "SKIP",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Diagnostic {
    pub(crate) level: DiagnosticLevel,
    pub(crate) message: String,
    pub(crate) details: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticSection {
    pub(crate) title: &'static str,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DoctorReport {
    pub(crate) sections: Vec<DiagnosticSection>,
}

impl DoctorReport {
    fn new() -> Self {
        Self {
            sections: [
                "System",
                "Config",
                "Runners",
                "Templates",
                "Workspace",
                "Contest",
            ]
            .into_iter()
            .map(|title| DiagnosticSection {
                title,
                diagnostics: Vec::new(),
            })
            .collect(),
        }
    }

    fn push(&mut self, section: &'static str, diagnostic: Diagnostic) {
        self.sections
            .iter_mut()
            .find(|candidate| candidate.title == section)
            .expect("doctor section should be registered")
            .diagnostics
            .push(diagnostic);
    }

    pub(crate) fn error_count(&self) -> usize {
        self.count(DiagnosticLevel::Error)
    }

    pub(crate) fn warning_count(&self) -> usize {
        self.count(DiagnosticLevel::Warn)
    }

    pub(crate) fn is_success(&self) -> bool {
        self.error_count() == 0
    }

    fn count(&self, level: DiagnosticLevel) -> usize {
        self.sections
            .iter()
            .flat_map(|section| &section.diagnostics)
            .filter(|diagnostic| diagnostic.level == level)
            .count()
    }
}

fn diagnostic(level: DiagnosticLevel, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        level,
        message: message.into(),
        details: Vec::new(),
    }
}

#[derive(Debug)]
pub(crate) struct DoctorPaths {
    pub(crate) config_file: Result<PathBuf, String>,
    pub(crate) templates_dir: Result<PathBuf, String>,
}

#[derive(Debug)]
pub(crate) struct SystemInfo {
    pub(crate) version: String,
    pub(crate) os: String,
    pub(crate) architecture: String,
}

impl SystemInfo {
    pub(crate) fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RunnerProbeResult {
    Available { version_line: Option<String> },
    Unavailable { reason: String },
}

pub(crate) trait RunnerProbe {
    fn probe(&mut self, program: &str, cwd: &Path) -> RunnerProbeResult;
}

impl<F> RunnerProbe for F
where
    F: FnMut(&str, &Path) -> RunnerProbeResult,
{
    fn probe(&mut self, program: &str, cwd: &Path) -> RunnerProbeResult {
        self(program, cwd)
    }
}

pub(crate) struct ProcessRunnerProbe;

impl RunnerProbe for ProcessRunnerProbe {
    fn probe(&mut self, program: &str, cwd: &Path) -> RunnerProbeResult {
        match probe_version(Path::new(program), cwd, RUNNER_PROBE_TIMEOUT) {
            Ok(result) => match result.outcome {
                VersionProbeOutcome::Exited(status) if status.success() => {
                    let version_line = first_useful_line_in_capture(&result.stdout)
                        .or_else(|| first_useful_line_in_capture(&result.stderr));
                    RunnerProbeResult::Available { version_line }
                }
                VersionProbeOutcome::Exited(status) => {
                    let output = first_useful_line_in_capture(&result.stderr)
                        .or_else(|| first_useful_line_in_capture(&result.stdout));
                    let mut reason = format!("`{program} --version` exited with {status}");
                    if let Some(output) = output {
                        reason.push_str(": ");
                        reason.push_str(&output);
                    }
                    RunnerProbeResult::Unavailable { reason }
                }
                VersionProbeOutcome::TimedOut => RunnerProbeResult::Unavailable {
                    reason: format!(
                        "`{program} --version` timed out after {:.1}s",
                        RUNNER_PROBE_TIMEOUT.as_secs_f64()
                    ),
                },
            },
            Err(error) => RunnerProbeResult::Unavailable {
                reason: format!("cannot launch {program:?}: {error}"),
            },
        }
    }
}

#[cfg(test)]
fn first_useful_line(text: &str) -> Option<String> {
    select_useful_line(text, false)
}

fn first_useful_line_in_capture(capture: &CapturedStream) -> Option<String> {
    debug_assert!(capture.retained_bytes <= 4 * 1024);
    select_useful_line(&capture.text, capture.truncated)
}

fn select_useful_line(text: &str, capture_truncated: bool) -> Option<String> {
    for segment in text.split_inclusive('\n') {
        let line_complete = segment.ends_with('\n');
        let line = segment.trim();
        if line.is_empty() {
            continue;
        }

        let mut selected = sanitize_external_text(line);
        if capture_truncated && !line_complete {
            selected.push('…');
        }
        return Some(selected);
    }

    capture_truncated.then(|| "…".to_owned())
}

fn sanitize_external_text(text: &str) -> String {
    let mut sanitized = String::new();

    for character in text.chars() {
        if character.is_control() {
            for escaped in character.escape_default() {
                sanitized.push(escaped);
            }
        } else {
            sanitized.push(character);
        }
    }

    sanitized
}

fn bounded_runner_text(text: &str) -> String {
    let sanitized = sanitize_external_text(text);
    let mut characters = sanitized.chars();
    let mut bounded = characters
        .by_ref()
        .take(MAX_RUNNER_VERSION_DISPLAY_CHARS)
        .collect::<String>();

    if characters.next().is_some() {
        bounded.pop();
        bounded.push('…');
    }

    bounded
}

pub(crate) fn inspect(
    cwd: &Path,
    paths: DoctorPaths,
    system: SystemInfo,
    runner_probe: &mut dyn RunnerProbe,
) -> DoctorReport {
    let mut report = DoctorReport::new();

    diagnose_system(&mut report, system);

    let resolved_config = diagnose_config(&mut report, paths.config_file);
    match resolved_config.as_ref() {
        Some(config) => {
            diagnose_runners(&mut report, cwd, config, runner_probe);
            diagnose_templates(&mut report, paths.templates_dir, config);
        }
        None => {
            report.push(
                "Runners",
                diagnostic(
                    DiagnosticLevel::Skip,
                    "Skipped because the effective config is unavailable",
                ),
            );
            report.push(
                "Templates",
                diagnostic(
                    DiagnosticLevel::Skip,
                    "Skipped because the effective config is unavailable",
                ),
            );
        }
    }

    diagnose_workspace(&mut report, cwd);
    diagnose_contest(&mut report, cwd);
    report
}

fn diagnose_system(report: &mut DoctorReport, system: SystemInfo) {
    report.push(
        "System",
        diagnostic(DiagnosticLevel::Ok, format!("atc {}", system.version)),
    );

    let supported = matches!(
        (system.os.as_str(), system.architecture.as_str()),
        ("windows", "x86_64") | ("macos", "aarch64")
    );
    let platform = format!(
        "{} {}",
        display_os(&system.os),
        display_architecture(&system.architecture)
    );
    let message = if supported {
        platform
    } else {
        format!("{platform} (not a v0.1 release-supported platform)")
    };
    report.push(
        "System",
        diagnostic(
            if supported {
                DiagnosticLevel::Ok
            } else {
                DiagnosticLevel::Warn
            },
            message,
        ),
    );
}

fn display_os(os: &str) -> String {
    match os {
        "windows" => "Windows".to_owned(),
        "macos" => "macOS".to_owned(),
        "linux" => "Linux".to_owned(),
        other => other.to_owned(),
    }
}

fn display_architecture(architecture: &str) -> &str {
    architecture
}

fn diagnose_config(
    report: &mut DoctorReport,
    config_file: Result<PathBuf, String>,
) -> Option<ResolvedConfig> {
    let path = match config_file {
        Ok(path) => path,
        Err(error) => {
            report.push(
                "Config",
                Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: "Could not resolve the config path".to_owned(),
                    details: vec![error],
                },
            );
            return None;
        }
    };

    match crate::config::Config::resolve_from(&path) {
        Ok(resolved) => {
            let message = if resolved.file_exists {
                path.display().to_string()
            } else {
                "Built-in defaults".to_owned()
            };
            let mut details = Vec::new();
            if !resolved.file_exists {
                details.push(format!(
                    "config override path  {} (not found)",
                    path.display()
                ));
            }
            details.extend(config_details(&resolved));
            report.push(
                "Config",
                Diagnostic {
                    level: DiagnosticLevel::Ok,
                    message,
                    details,
                },
            );
            Some(resolved)
        }
        Err(error) => {
            report.push(
                "Config",
                Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: path.display().to_string(),
                    details: vec![concise_error(&error)],
                },
            );
            None
        }
    }
}

fn concise_error(error: &impl std::fmt::Display) -> String {
    let text = error.to_string();
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(first) = lines.next() else {
        return "unknown error".to_owned();
    };
    let last = lines.next_back();
    match last {
        Some(last) if last != first => format!("{first}: {last}"),
        _ => first.to_owned(),
    }
}

fn config_details(resolved: &ResolvedConfig) -> Vec<String> {
    let config = &resolved.config;
    let sources = &resolved.sources;
    let flags = if config.runner.cpp_flags.is_empty() {
        "<none>".to_owned()
    } else {
        config.runner.cpp_flags.join(" ")
    };

    vec![
        setting(
            "default language",
            language_config_name(config.defaults.language),
            sources.default_language,
        ),
        setting(
            "C++ compiler",
            &config.runner.cpp_compiler,
            sources.cpp_compiler,
        ),
        setting("C++ flags", &flags, sources.cpp_flags),
        setting("Python executable", &config.runner.python, sources.python),
        setting(
            "runtime timeout",
            &format_seconds(config.runner.timeout_seconds),
            sources.timeout_seconds,
        ),
        setting(
            "compile timeout",
            &format_seconds(config.runner.compile_timeout_seconds),
            sources.compile_timeout_seconds,
        ),
    ]
}

fn setting(name: &str, value: &str, source: ConfigValueSource) -> String {
    let source = match source {
        ConfigValueSource::BuiltIn => "built-in",
        ConfigValueSource::UserOverride => "user override",
    };
    format!("{name:<18} {value} ({source})")
}

fn format_seconds(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}s")
    } else {
        format!("{value}s")
    }
}

fn language_config_name(language: Language) -> &'static str {
    match language {
        Language::Cpp => "cpp",
        Language::Python => "python",
    }
}

fn language_display_name(language: Language) -> &'static str {
    match language {
        Language::Cpp => "C++",
        Language::Python => "Python",
    }
}

fn runner_program(config: &ResolvedConfig, language: Language) -> &str {
    match language {
        Language::Cpp => &config.config.runner.cpp_compiler,
        Language::Python => &config.config.runner.python,
    }
}

fn diagnose_runners(
    report: &mut DoctorReport,
    cwd: &Path,
    config: &ResolvedConfig,
    runner_probe: &mut dyn RunnerProbe,
) {
    for language in Language::ALL {
        let program = runner_program(config, language);
        let name = language_display_name(language);
        match runner_probe.probe(program, cwd) {
            RunnerProbeResult::Available { version_line } => {
                let result = version_line
                    .unwrap_or_else(|| format!("{program} (`--version` completed successfully)"));
                report.push(
                    "Runners",
                    diagnostic(
                        DiagnosticLevel::Ok,
                        bounded_runner_text(&format!("{name:<7} {result}")),
                    ),
                );
            }
            RunnerProbeResult::Unavailable { reason } => {
                let level = if language == config.config.defaults.language {
                    DiagnosticLevel::Error
                } else {
                    DiagnosticLevel::Warn
                };
                report.push(
                    "Runners",
                    Diagnostic {
                        level,
                        message: bounded_runner_text(&format!(
                            "{name:<7} executable unavailable: {program}"
                        )),
                        details: vec![bounded_runner_text(&reason)],
                    },
                );
            }
        }
    }
}

fn diagnose_templates(
    report: &mut DoctorReport,
    templates_dir: Result<PathBuf, String>,
    config: &ResolvedConfig,
) {
    let templates_dir = match templates_dir {
        Ok(path) => path,
        Err(error) => {
            report.push(
                "Templates",
                Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: "Could not resolve the source-template path".to_owned(),
                    details: vec![error],
                },
            );
            return;
        }
    };

    for language in Language::ALL {
        let name = language_display_name(language);
        match template::resolve_source_template_with_origin_in(&templates_dir, language) {
            Ok(resolved) => {
                let source = match resolved.origin {
                    SourceTemplateOrigin::BuiltIn => "built-in".to_owned(),
                    SourceTemplateOrigin::UserOverride(path) => path.display().to_string(),
                };
                report.push(
                    "Templates",
                    diagnostic(DiagnosticLevel::Ok, format!("{name:<7} {source}")),
                );
            }
            Err(error) => {
                let level = if language == config.config.defaults.language {
                    DiagnosticLevel::Error
                } else {
                    DiagnosticLevel::Warn
                };
                report.push(
                    "Templates",
                    Diagnostic {
                        level,
                        message: format!("{name:<7} source template is invalid or unreadable"),
                        details: vec![concise_error(&error)],
                    },
                );
            }
        }
    }
}

fn diagnose_workspace(report: &mut DoctorReport, cwd: &Path) {
    match workspace::inspect_workspace_config(cwd) {
        Ok(Some(workspace)) => report.push(
            "Workspace",
            Diagnostic {
                level: DiagnosticLevel::Ok,
                message: cwd.display().to_string(),
                details: vec![
                    format!("config  {}", workspace.path.display()),
                    format!(
                        "{} path mapping{}",
                        workspace.mapping_count,
                        if workspace.mapping_count == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                ],
            },
        ),
        Ok(None) => report.push(
            "Workspace",
            diagnostic(
                DiagnosticLevel::Info,
                "Workspace config not found in the current directory",
            ),
        ),
        Err(error) => report.push(
            "Workspace",
            Diagnostic {
                level: DiagnosticLevel::Error,
                message: format!("Invalid workspace config in {}", cwd.display()),
                details: vec![concise_error(&error)],
            },
        ),
    }
}

fn diagnose_contest(report: &mut DoctorReport, cwd: &Path) {
    match workspace::inspect_contest_metadata(cwd) {
        Ok(ContestMetadataHealth::Healthy(contest)) => {
            let message = if contest.contest_id.is_empty() {
                "Contest metadata is valid".to_owned()
            } else {
                format!("Contest metadata: {}", contest.contest_id)
            };
            report.push("Contest", diagnostic(DiagnosticLevel::Ok, message));
        }
        Ok(ContestMetadataHealth::Missing) => report.push(
            "Contest",
            diagnostic(
                DiagnosticLevel::Info,
                "Contest metadata not found in the current directory",
            ),
        ),
        Ok(ContestMetadataHealth::Invalid) => report.push(
            "Contest",
            diagnostic(
                DiagnosticLevel::Error,
                format!("Contest metadata is invalid in {}", cwd.display()),
            ),
        ),
        Ok(ContestMetadataHealth::UnsupportedVersion(version)) => report.push(
            "Contest",
            diagnostic(
                DiagnosticLevel::Error,
                format!("Unsupported contest metadata version: {version}"),
            ),
        ),
        Err(error) => report.push(
            "Contest",
            Diagnostic {
                level: DiagnosticLevel::Error,
                message: format!("Could not inspect contest metadata in {}", cwd.display()),
                details: vec![concise_error(&error)],
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Contest, Problem};
    use std::collections::BTreeMap;
    use std::fs;

    fn supported_system() -> SystemInfo {
        SystemInfo {
            version: "0.1.0-test".to_owned(),
            os: "windows".to_owned(),
            architecture: "x86_64".to_owned(),
        }
    }

    fn paths(root: &Path) -> DoctorPaths {
        DoctorPaths {
            config_file: Ok(root.join("config").join("config.toml")),
            templates_dir: Ok(root.join("config").join("templates")),
        }
    }

    fn available_probe(program: &str, _: &Path) -> RunnerProbeResult {
        RunnerProbeResult::Available {
            version_line: Some(format!("{program} test-version")),
        }
    }

    fn inspect_root(root: &Path) -> DoctorReport {
        inspect(root, paths(root), supported_system(), &mut available_probe)
    }

    fn section<'a>(report: &'a DoctorReport, title: &str) -> &'a DiagnosticSection {
        report
            .sections
            .iter()
            .find(|section| section.title == title)
            .unwrap()
    }

    fn levels(report: &DoctorReport, title: &str) -> Vec<DiagnosticLevel> {
        section(report, title)
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.level)
            .collect()
    }

    fn all_text(report: &DoctorReport) -> String {
        report
            .sections
            .iter()
            .flat_map(|section| &section.diagnostics)
            .flat_map(|diagnostic| {
                std::iter::once(diagnostic.message.as_str())
                    .chain(diagnostic.details.iter().map(String::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn write_valid_metadata(root: &Path) {
        workspace::save_metadata(
            root,
            &Contest {
                contest_id: "abc466".to_owned(),
                problems: vec![Problem {
                    index: "A".to_owned(),
                    title: "A".to_owned(),
                    task_id: "abc466_a".to_owned(),
                    url: "https://atcoder.jp/contests/abc466/tasks/abc466_a".to_owned(),
                }],
            },
        )
        .unwrap();
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, current: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let mut entries = fs::read_dir(current)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                if entry.file_type().unwrap().is_dir() {
                    snapshot.insert(relative.clone(), Vec::new());
                    visit(root, &path, snapshot);
                } else {
                    snapshot.insert(relative, fs::read(path).unwrap());
                }
            }
        }

        let mut result = BTreeMap::new();
        visit(root, root, &mut result);
        result
    }

    #[test]
    fn runner_version_text_neutralizes_ansi_and_preserves_readable_content() {
        let version = first_useful_line("\x1b[31mPython 3.14.0\x1b[0m\n").unwrap();

        assert!(version.contains("Python 3.14.0"));
        assert!(!version.contains('\x1b'));
        assert!(version.chars().all(|character| !character.is_control()));
    }

    #[test]
    fn runner_version_text_neutralizes_other_terminal_controls() {
        let version = first_useful_line("tool\t3.0\u{7}\u{8}\rnext\u{9b}31m").unwrap();

        assert!(version.contains("tool"));
        assert!(version.contains("next"));
        assert!(version.chars().all(|character| !character.is_control()));
    }

    #[test]
    fn final_runner_diagnostic_bounds_a_multi_megabyte_no_newline_line() {
        let output = "v".repeat(2 * 1024 * 1024);
        let version = first_useful_line(&output).unwrap();
        let diagnostic = bounded_runner_text(&format!("C++     {version}"));

        assert_eq!(diagnostic.chars().count(), MAX_RUNNER_VERSION_DISPLAY_CHARS);
        assert!(diagnostic.ends_with('…'));
    }

    #[test]
    fn plain_doctor_rendering_remains_free_of_runner_supplied_controls() {
        let version = first_useful_line("\x1b[32mtool 1.0\x1b[0m\u{7}").unwrap();
        let report = DoctorReport {
            sections: vec![DiagnosticSection {
                title: "Runners",
                diagnostics: vec![diagnostic(DiagnosticLevel::Ok, version)],
            }],
        };

        let rendered = crate::ui::render_doctor_report(&report, false);
        assert!(!rendered.contains('\x1b'));
        assert!(
            rendered
                .chars()
                .all(|character| character == '\n' || !character.is_control())
        );
    }

    #[test]
    fn assembled_runner_success_diagnostics_are_sanitized_unicode_safe_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let supplied_version = format!("\x1b[31m{}", "界".repeat(400));
        let mut probe = |_: &str, _: &Path| RunnerProbeResult::Available {
            version_line: Some(supplied_version.clone()),
        };

        let report = inspect(
            temp.path(),
            paths(temp.path()),
            supported_system(),
            &mut probe,
        );

        for diagnostic in &section(&report, "Runners").diagnostics {
            assert!(
                diagnostic.message.chars().count() <= MAX_RUNNER_VERSION_DISPLAY_CHARS,
                "runner diagnostic exceeded bound: {:?}",
                diagnostic.message
            );
            assert!(diagnostic.message.ends_with('…'));
            assert!(diagnostic.message.contains("\\u{1b}"));
            assert!(!diagnostic.message.chars().any(char::is_control));
        }
    }

    #[test]
    fn assembled_runner_failure_messages_and_details_are_independently_safe_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config").join("config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let long_program = "runner".repeat(80);
        fs::write(
            &config,
            format!("runner.cpp_compiler = {long_program:?}\nrunner.python = {long_program:?}\n"),
        )
        .unwrap();
        let long_reason = format!("\u{9b}{}", "失".repeat(400));
        let mut probe = |_: &str, _: &Path| RunnerProbeResult::Unavailable {
            reason: long_reason.clone(),
        };

        let report = inspect(
            temp.path(),
            paths(temp.path()),
            supported_system(),
            &mut probe,
        );

        for diagnostic in &section(&report, "Runners").diagnostics {
            assert_eq!(
                diagnostic.message.chars().count(),
                MAX_RUNNER_VERSION_DISPLAY_CHARS
            );
            assert!(diagnostic.message.ends_with('…'));
            assert!(!diagnostic.message.chars().any(char::is_control));

            let detail = diagnostic.details.first().unwrap();
            assert_eq!(detail.chars().count(), MAX_RUNNER_VERSION_DISPLAY_CHARS);
            assert!(detail.ends_with('…'));
            assert!(detail.contains("\\u{9b}"));
            assert!(!detail.chars().any(char::is_control));
        }
    }

    #[test]
    fn missing_config_reports_builtin_defaults_and_succeeds() {
        let temp = tempfile::tempdir().unwrap();

        let report = inspect_root(temp.path());
        let text = all_text(&report);

        assert!(report.is_success());
        assert!(text.contains("Built-in defaults"));
        assert!(text.contains("config override path"));
        assert!(text.contains("default language   cpp (built-in)"));
        assert!(text.contains("compile timeout    10.0s (built-in)"));
    }

    #[test]
    fn valid_config_reports_effective_values_and_exact_sources() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config").join("config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            "defaults.language = \"python\"\n\
             runner.cpp_flags = []\n\
             runner.python = \"python-custom\"\n\
             runner.timeout_seconds = 3.5\n",
        )
        .unwrap();

        let report = inspect_root(temp.path());
        let text = all_text(&report);

        assert!(report.is_success());
        assert!(text.contains(&config.display().to_string()));
        assert!(text.contains("default language   python (user override)"));
        assert!(text.contains("C++ compiler       g++ (built-in)"));
        assert!(text.contains("C++ flags          <none> (user override)"));
        assert!(text.contains("Python executable  python-custom (user override)"));
        assert!(text.contains("runtime timeout    3.5s (user override)"));
        assert!(text.contains("compile timeout    10.0s (built-in)"));
    }

    #[test]
    fn invalid_config_is_an_error_and_skips_runners_and_templates() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config").join("config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "[runner\n").unwrap();
        let mut probe_calls = 0;
        let mut probe = |_: &str, _: &Path| {
            probe_calls += 1;
            available_probe("unused", temp.path())
        };

        let report = inspect(
            temp.path(),
            paths(temp.path()),
            supported_system(),
            &mut probe,
        );

        assert!(!report.is_success());
        assert_eq!(levels(&report, "Config"), [DiagnosticLevel::Error]);
        assert_eq!(levels(&report, "Runners"), [DiagnosticLevel::Skip]);
        assert_eq!(levels(&report, "Templates"), [DiagnosticLevel::Skip]);
        assert_eq!(probe_calls, 0);
    }

    #[test]
    fn missing_runner_severity_follows_the_default_language() {
        for (default, expected) in [
            ("cpp", [DiagnosticLevel::Error, DiagnosticLevel::Ok]),
            ("python", [DiagnosticLevel::Warn, DiagnosticLevel::Ok]),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let config = temp.path().join("config").join("config.toml");
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::write(
                config,
                format!(
                    "[defaults]\nlanguage = {default:?}\n[runner]\ncpp_compiler = \"missing\"\n"
                ),
            )
            .unwrap();
            let mut probe = |program: &str, _: &Path| {
                if program == "missing" {
                    RunnerProbeResult::Unavailable {
                        reason: "not found".to_owned(),
                    }
                } else {
                    available_probe(program, temp.path())
                }
            };

            let report = inspect(
                temp.path(),
                paths(temp.path()),
                supported_system(),
                &mut probe,
            );

            assert_eq!(levels(&report, "Runners"), expected);
            assert_eq!(report.is_success(), default == "python");
        }
    }

    #[test]
    fn templates_report_builtin_and_user_override_paths() {
        let temp = tempfile::tempdir().unwrap();
        let templates = temp.path().join("config").join("templates");
        fs::create_dir_all(&templates).unwrap();
        let python = templates.join("python.py");
        fs::write(&python, "print('custom')\n").unwrap();

        let report = inspect_root(temp.path());
        let text = all_text(&report);

        assert_eq!(levels(&report, "Templates"), [DiagnosticLevel::Ok; 2]);
        assert!(text.contains("C++     built-in"));
        assert!(text.contains(&format!("Python  {}", python.display())));
    }

    #[test]
    fn invalid_template_severity_follows_the_default_language() {
        for (default, expected) in [
            ("cpp", [DiagnosticLevel::Error, DiagnosticLevel::Ok]),
            ("python", [DiagnosticLevel::Warn, DiagnosticLevel::Ok]),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let config = temp.path().join("config").join("config.toml");
            let templates = temp.path().join("config").join("templates");
            fs::create_dir_all(&templates).unwrap();
            fs::write(config, format!("defaults.language = {default:?}\n")).unwrap();
            fs::create_dir(templates.join("cpp.cpp")).unwrap();

            let report = inspect_root(temp.path());

            assert_eq!(levels(&report, "Templates"), expected);
            assert_eq!(report.is_success(), default == "python");
        }
    }

    #[test]
    fn workspace_absent_valid_and_invalid_are_classified_without_parent_search() {
        let parent = tempfile::tempdir().unwrap();
        let child = parent.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::write(
            parent.path().join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
        write_valid_metadata(parent.path());

        let absent = inspect_root(&child);
        assert_eq!(levels(&absent, "Workspace"), [DiagnosticLevel::Info]);
        assert_eq!(levels(&absent, "Contest"), [DiagnosticLevel::Info]);

        fs::write(
            child.join(".atc-workspace.toml"),
            "version = 1\n\
             [[paths]]\npattern = \"^abc\"\npath = \"ABC\"\n\
             [[paths]]\npattern = \"^arc\"\npath = \"ARC\"\n",
        )
        .unwrap();
        let valid = inspect_root(&child);
        assert_eq!(levels(&valid, "Workspace"), [DiagnosticLevel::Ok]);
        assert!(all_text(&valid).contains("2 path mappings"));

        fs::write(
            child.join(".atc-workspace.toml"),
            "version = 99\npaths = []\n",
        )
        .unwrap();
        let invalid = inspect_root(&child);
        assert_eq!(levels(&invalid, "Workspace"), [DiagnosticLevel::Error]);
    }

    #[test]
    fn contest_metadata_absent_valid_and_invalid_are_classified() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            levels(&inspect_root(temp.path()), "Contest"),
            [DiagnosticLevel::Info]
        );

        write_valid_metadata(temp.path());
        let valid = inspect_root(temp.path());
        assert_eq!(levels(&valid, "Contest"), [DiagnosticLevel::Ok]);
        assert!(all_text(&valid).contains("Contest metadata: abc466"));

        fs::write(temp.path().join(".atc").join("contest.toml"), "invalid").unwrap();
        assert_eq!(
            levels(&inspect_root(temp.path()), "Contest"),
            [DiagnosticLevel::Error]
        );
    }

    #[test]
    fn inspection_is_read_only_has_no_network_path_and_only_invokes_two_local_runner_probes() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config").join("config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "runner.python = \"py-local\"\n").unwrap();
        fs::write(
            temp.path().join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
        write_valid_metadata(temp.path());
        let before = snapshot(temp.path());
        let mut calls = Vec::new();
        let mut probe = |program: &str, cwd: &Path| {
            calls.push((program.to_owned(), cwd.to_path_buf()));
            available_probe(program, cwd)
        };

        let report = inspect(
            temp.path(),
            paths(temp.path()),
            supported_system(),
            &mut probe,
        );

        assert!(report.is_success());
        assert_eq!(
            calls,
            [
                ("g++".to_owned(), temp.path().to_path_buf()),
                ("py-local".to_owned(), temp.path().to_path_buf()),
            ]
        );
        assert_eq!(snapshot(temp.path()), before);
    }

    #[test]
    fn warnings_do_not_fail_but_errors_do() {
        let temp = tempfile::tempdir().unwrap();
        let mut missing_cpp = |program: &str, cwd: &Path| {
            if program == "g++" {
                RunnerProbeResult::Unavailable {
                    reason: "not found".to_owned(),
                }
            } else {
                available_probe(program, cwd)
            }
        };
        let warning_report = inspect(
            temp.path(),
            paths(temp.path()),
            SystemInfo {
                version: "test".to_owned(),
                os: "linux".to_owned(),
                architecture: "x86_64".to_owned(),
            },
            &mut missing_cpp,
        );
        assert!(!warning_report.is_success());

        let config = temp.path().join("config").join("config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(config, "defaults.language = \"python\"\n").unwrap();
        let warning_report = inspect(
            temp.path(),
            paths(temp.path()),
            SystemInfo {
                version: "test".to_owned(),
                os: "linux".to_owned(),
                architecture: "x86_64".to_owned(),
            },
            &mut missing_cpp,
        );
        assert!(warning_report.is_success());
        assert!(warning_report.warning_count() >= 2);

        fs::write(temp.path().join(".atc-workspace.toml"), "invalid").unwrap();
        let error_report = inspect_root(temp.path());
        assert!(!error_report.is_success());
        assert!(error_report.error_count() >= 1);
    }
}
