use crate::config::{Config, RunnerConfig};
use crate::editor::EditorLaunchMode;
use crate::language::{Language, PythonRuntime};
use std::fmt;
use std::io;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsCategory {
    Defaults,
    Runner,
    Submit,
    Editor,
}

impl SettingsCategory {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Defaults => "Defaults",
            Self::Runner => "Runner",
            Self::Submit => "Submit",
            Self::Editor => "Editor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingInput {
    Enum,
    String,
    StringList,
    Number,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SettingKey {
    DefaultLanguage,
    RunnerPython,
    RunnerCppCompiler,
    RunnerCppFlags,
    RunnerTimeoutSeconds,
    RunnerCompileTimeoutSeconds,
    SubmitPythonRuntime,
    EditorCommand,
    EditorArgs,
    EditorMode,
}

impl SettingKey {
    pub(crate) const ALL: [Self; 10] = [
        Self::DefaultLanguage,
        Self::RunnerPython,
        Self::RunnerCppCompiler,
        Self::RunnerCppFlags,
        Self::RunnerTimeoutSeconds,
        Self::RunnerCompileTimeoutSeconds,
        Self::SubmitPythonRuntime,
        Self::EditorCommand,
        Self::EditorArgs,
        Self::EditorMode,
    ];

    pub(crate) const fn category(self) -> SettingsCategory {
        match self {
            Self::DefaultLanguage => SettingsCategory::Defaults,
            Self::RunnerPython
            | Self::RunnerCppCompiler
            | Self::RunnerCppFlags
            | Self::RunnerTimeoutSeconds
            | Self::RunnerCompileTimeoutSeconds => SettingsCategory::Runner,
            Self::SubmitPythonRuntime => SettingsCategory::Submit,
            Self::EditorCommand | Self::EditorArgs | Self::EditorMode => SettingsCategory::Editor,
        }
    }

    pub(crate) const fn path(self) -> &'static str {
        match self {
            Self::DefaultLanguage => "defaults.language",
            Self::RunnerPython => "runner.python",
            Self::RunnerCppCompiler => "runner.cpp_compiler",
            Self::RunnerCppFlags => "runner.cpp_flags",
            Self::RunnerTimeoutSeconds => "runner.timeout_seconds",
            Self::RunnerCompileTimeoutSeconds => "runner.compile_timeout_seconds",
            Self::SubmitPythonRuntime => "submit.python_runtime",
            Self::EditorCommand => "editor.command",
            Self::EditorArgs => "editor.args",
            Self::EditorMode => "editor.mode",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::DefaultLanguage => "Default language",
            Self::RunnerPython => "Python command",
            Self::RunnerCppCompiler => "C++ compiler",
            Self::RunnerCppFlags => "C++ flags",
            Self::RunnerTimeoutSeconds => "Run timeout",
            Self::RunnerCompileTimeoutSeconds => "Compile timeout",
            Self::SubmitPythonRuntime => "Python runtime",
            Self::EditorCommand => "Editor command",
            Self::EditorArgs => "Editor arguments",
            Self::EditorMode => "Editor mode",
        }
    }

    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::DefaultLanguage => "Language used when a command does not specify one.",
            Self::RunnerPython => "Command used to run Python solutions and Stress Helpers.",
            Self::RunnerCppCompiler => "Command used to compile C++ solutions.",
            Self::RunnerCppFlags => "Arguments passed to the C++ compiler, in order.",
            Self::RunnerTimeoutSeconds => {
                "Time limit for solutions and Stress Helpers, in seconds."
            }
            Self::RunnerCompileTimeoutSeconds => "Time limit for C++ compilation, in seconds.",
            Self::SubmitPythonRuntime => "Python runtime selected for submissions.",
            Self::EditorCommand => {
                "Editor command. When unset, atc uses automatic editor resolution."
            }
            Self::EditorArgs => "Arguments inserted between the editor command and target path.",
            Self::EditorMode => "Whether the editor owns the terminal or launches externally.",
        }
    }

    pub(crate) const fn input(self) -> SettingInput {
        match self {
            Self::DefaultLanguage | Self::SubmitPythonRuntime | Self::EditorMode => {
                SettingInput::Enum
            }
            Self::RunnerPython | Self::RunnerCppCompiler | Self::EditorCommand => {
                SettingInput::String
            }
            Self::RunnerCppFlags | Self::EditorArgs => SettingInput::StringList,
            Self::RunnerTimeoutSeconds | Self::RunnerCompileTimeoutSeconds => SettingInput::Number,
        }
    }

    const fn section_and_name(self) -> (&'static str, &'static str) {
        match self {
            Self::DefaultLanguage => ("defaults", "language"),
            Self::RunnerPython => ("runner", "python"),
            Self::RunnerCppCompiler => ("runner", "cpp_compiler"),
            Self::RunnerCppFlags => ("runner", "cpp_flags"),
            Self::RunnerTimeoutSeconds => ("runner", "timeout_seconds"),
            Self::RunnerCompileTimeoutSeconds => ("runner", "compile_timeout_seconds"),
            Self::SubmitPythonRuntime => ("submit", "python_runtime"),
            Self::EditorCommand => ("editor", "command"),
            Self::EditorArgs => ("editor", "args"),
            Self::EditorMode => ("editor", "mode"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorModeValue {
    Auto,
    Terminal,
    External,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SettingValue {
    Language(Language),
    String(String),
    StringList(Vec<String>),
    Number(f64),
    PythonRuntime(PythonRuntime),
    EditorMode(EditorModeValue),
}

impl SettingValue {
    pub(crate) fn display(&self) -> String {
        match self {
            Self::Language(Language::Cpp) => "C++".to_string(),
            Self::Language(Language::Python) => "Python".to_string(),
            Self::String(value) => value.clone(),
            Self::StringList(values) if values.is_empty() => "[]".to_string(),
            Self::StringList(values) => format!("[{}]", values.join(", ")),
            Self::Number(value) => value.to_string(),
            Self::PythonRuntime(runtime) => runtime.display_name().to_string(),
            Self::EditorMode(EditorModeValue::Auto) => "Auto".to_string(),
            Self::EditorMode(EditorModeValue::Terminal) => "Terminal".to_string(),
            Self::EditorMode(EditorModeValue::External) => "External".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DocumentChange {
    Unchanged,
    Changed,
}

#[derive(Debug)]
pub(crate) enum SettingsDocumentError {
    InvalidConfig(io::Error),
    UnsupportedFormat(String),
    WrongValueType {
        key: SettingKey,
        expected: SettingInput,
    },
    EditorCommandRequired(SettingKey),
}

impl fmt::Display for SettingsDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(formatter, "invalid Global Config: {error}"),
            Self::UnsupportedFormat(reason) => write!(
                formatter,
                "this TOML format cannot be safely updated in Settings: {reason}; open the file in an editor instead"
            ),
            Self::WrongValueType { key, expected } => {
                write!(formatter, "{} requires {expected:?} input", key.path())
            }
            Self::EditorCommandRequired(key) => write!(
                formatter,
                "{} cannot be edited until editor.command is configured",
                key.path()
            ),
        }
    }
}

impl std::error::Error for SettingsDocumentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::UnsupportedFormat(_)
            | Self::WrongValueType { .. }
            | Self::EditorCommandRequired(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsDocument {
    original: String,
    document: DocumentMut,
    effective: Config,
    line_ending: LineEnding,
    preservation_issue: Option<String>,
    changed: bool,
}

#[derive(Debug, Clone, Copy)]
enum LineEnding {
    Lf,
    Crlf,
}

impl SettingsDocument {
    pub(crate) fn parse(contents: &str) -> Result<Self, SettingsDocumentError> {
        let effective = Config::parse(contents).map_err(SettingsDocumentError::InvalidConfig)?;
        let (line_ending, normalized) = normalize_line_endings(contents)?;
        let document = normalized.parse::<DocumentMut>().map_err(|error| {
            SettingsDocumentError::UnsupportedFormat(format!("TOML parser rejected it: {error}"))
        })?;
        let preservation_issue = (document.to_string() != normalized)
            .then(|| "toml_edit cannot reproduce the original bytes exactly".to_string());

        Ok(Self {
            original: contents.to_string(),
            document,
            effective,
            line_ending,
            preservation_issue,
            changed: false,
        })
    }

    pub(crate) fn empty() -> Self {
        Self::parse("").expect("an empty config is valid and exactly representable")
    }

    pub(crate) fn candidate(&self) -> String {
        if self.changed {
            apply_line_ending(self.document.to_string(), self.line_ending)
        } else {
            self.original.clone()
        }
    }

    pub(crate) fn has_changes(&self) -> bool {
        self.changed
    }

    pub(crate) fn preservation_issue(&self) -> Option<&str> {
        self.preservation_issue.as_deref()
    }

    pub(crate) fn is_modified(&self, key: SettingKey) -> bool {
        let (section, name) = key.section_and_name();
        section_get(self.document.as_table(), section, name).is_some()
    }

    pub(crate) fn editor_configured(&self) -> bool {
        self.effective.editor.is_some()
    }

    pub(crate) fn editable(&self, key: SettingKey) -> bool {
        !matches!(key, SettingKey::EditorArgs | SettingKey::EditorMode) || self.editor_configured()
    }

    pub(crate) fn effective_value(&self, key: SettingKey) -> SettingValue {
        effective_value_from_config(&self.effective, key)
    }

    pub(crate) fn configured_value(&self, key: SettingKey) -> Option<SettingValue> {
        self.is_modified(key).then(|| self.effective_value(key))
    }

    pub(crate) fn built_in_value(key: SettingKey) -> SettingValue {
        let config = Config::default();
        let runner = RunnerConfig::default();
        match key {
            SettingKey::DefaultLanguage => SettingValue::Language(config.defaults.language),
            SettingKey::RunnerPython => SettingValue::String(runner.python),
            SettingKey::RunnerCppCompiler => SettingValue::String(runner.cpp_compiler),
            SettingKey::RunnerCppFlags => SettingValue::StringList(runner.cpp_flags),
            SettingKey::RunnerTimeoutSeconds => SettingValue::Number(runner.timeout_seconds),
            SettingKey::RunnerCompileTimeoutSeconds => {
                SettingValue::Number(runner.compile_timeout_seconds)
            }
            SettingKey::SubmitPythonRuntime => {
                SettingValue::PythonRuntime(config.submit.python_runtime)
            }
            SettingKey::EditorCommand => SettingValue::String("Auto-detected".to_string()),
            SettingKey::EditorArgs => SettingValue::StringList(Vec::new()),
            SettingKey::EditorMode => SettingValue::EditorMode(EditorModeValue::Auto),
        }
    }

    pub(crate) fn set(
        &mut self,
        key: SettingKey,
        new_value: SettingValue,
    ) -> Result<DocumentChange, SettingsDocumentError> {
        if !self.editable(key) {
            return Err(SettingsDocumentError::EditorCommandRequired(key));
        }
        validate_value_type(key, &new_value)?;
        if key == SettingKey::EditorMode
            && new_value == SettingValue::EditorMode(EditorModeValue::Auto)
        {
            return self.reset(key);
        }
        if self.is_modified(key) && self.effective_value(key) == new_value {
            return Ok(DocumentChange::Unchanged);
        }
        self.require_structured_write()?;

        let mut candidate = self.document.clone();
        let (section, name) = key.section_and_name();
        section_insert(
            candidate.as_table_mut(),
            section,
            name,
            toml_value(new_value),
        );
        self.commit_candidate(candidate)
    }

    pub(crate) fn reset(
        &mut self,
        key: SettingKey,
    ) -> Result<DocumentChange, SettingsDocumentError> {
        if !self.is_modified(key) {
            return Ok(DocumentChange::Unchanged);
        }
        self.require_structured_write()?;

        let mut candidate = self.document.clone();
        if key == SettingKey::EditorCommand {
            candidate.as_table_mut().remove("editor");
        } else {
            let (section, name) = key.section_and_name();
            section_remove(candidate.as_table_mut(), section, name);
        }
        self.commit_candidate(candidate)
    }

    pub(crate) fn validate_set(
        &self,
        key: SettingKey,
        new_value: SettingValue,
    ) -> Result<(), SettingsDocumentError> {
        let mut candidate = self.clone();
        candidate.set(key, new_value).map(|_| ())
    }

    fn commit_candidate(
        &mut self,
        candidate: DocumentMut,
    ) -> Result<DocumentChange, SettingsDocumentError> {
        let rendered = candidate.to_string();
        if rendered == self.document.to_string() {
            return Ok(DocumentChange::Unchanged);
        }
        let rendered_with_original_line_endings = apply_line_ending(rendered, self.line_ending);
        let effective = Config::parse(&rendered_with_original_line_endings)
            .map_err(SettingsDocumentError::InvalidConfig)?;
        let original = Self::parse(&self.original)
            .expect("a document can only be edited after its original parsed successfully");
        if SettingKey::ALL.into_iter().all(|key| {
            original.is_modified(key)
                == section_get(
                    candidate.as_table(),
                    key.section_and_name().0,
                    key.section_and_name().1,
                )
                .is_some()
                && original.effective_value(key) == effective_value_from_config(&effective, key)
        }) {
            self.document = original.document;
            self.effective = original.effective;
            self.changed = false;
            return Ok(DocumentChange::Changed);
        }
        self.document = candidate;
        self.effective = effective;
        self.changed = true;
        Ok(DocumentChange::Changed)
    }

    fn require_structured_write(&self) -> Result<(), SettingsDocumentError> {
        match &self.preservation_issue {
            Some(reason) => Err(SettingsDocumentError::UnsupportedFormat(reason.clone())),
            None => Ok(()),
        }
    }
}

fn effective_value_from_config(config: &Config, key: SettingKey) -> SettingValue {
    let runner = &config.runner;
    match key {
        SettingKey::DefaultLanguage => SettingValue::Language(config.defaults.language),
        SettingKey::RunnerPython => SettingValue::String(runner.python.clone()),
        SettingKey::RunnerCppCompiler => SettingValue::String(runner.cpp_compiler.clone()),
        SettingKey::RunnerCppFlags => SettingValue::StringList(runner.cpp_flags.clone()),
        SettingKey::RunnerTimeoutSeconds => SettingValue::Number(runner.timeout_seconds),
        SettingKey::RunnerCompileTimeoutSeconds => {
            SettingValue::Number(runner.compile_timeout_seconds)
        }
        SettingKey::SubmitPythonRuntime => {
            SettingValue::PythonRuntime(config.submit.python_runtime)
        }
        SettingKey::EditorCommand => config.editor.as_ref().map_or_else(
            || SettingValue::String("Auto-detected".to_string()),
            |editor| SettingValue::String(editor.command.clone()),
        ),
        SettingKey::EditorArgs => SettingValue::StringList(
            config
                .editor
                .as_ref()
                .map_or_else(Vec::new, |editor| editor.args.clone()),
        ),
        SettingKey::EditorMode => SettingValue::EditorMode(config.editor.as_ref().map_or(
            EditorModeValue::Auto,
            |editor| match editor.mode {
                None => EditorModeValue::Auto,
                Some(EditorLaunchMode::Terminal) => EditorModeValue::Terminal,
                Some(EditorLaunchMode::External) => EditorModeValue::External,
            },
        )),
    }
}

fn normalize_line_endings(contents: &str) -> Result<(LineEnding, String), SettingsDocumentError> {
    if contents.contains("\r\n") {
        let without_crlf = contents.replace("\r\n", "");
        if without_crlf.contains(['\r', '\n']) {
            return Err(SettingsDocumentError::UnsupportedFormat(
                "mixed or unsupported line endings".to_string(),
            ));
        }
        Ok((LineEnding::Crlf, contents.replace("\r\n", "\n")))
    } else if contents.contains('\r') {
        Err(SettingsDocumentError::UnsupportedFormat(
            "lone carriage-return line endings".to_string(),
        ))
    } else {
        Ok((LineEnding::Lf, contents.to_string()))
    }
}

fn apply_line_ending(mut contents: String, line_ending: LineEnding) -> String {
    if matches!(line_ending, LineEnding::Crlf) {
        contents = contents.replace('\n', "\r\n");
    }
    contents
}

fn validate_value_type(key: SettingKey, value: &SettingValue) -> Result<(), SettingsDocumentError> {
    let valid = matches!(
        (key, value),
        (SettingKey::DefaultLanguage, SettingValue::Language(_))
            | (
                SettingKey::SubmitPythonRuntime,
                SettingValue::PythonRuntime(_)
            )
            | (SettingKey::EditorMode, SettingValue::EditorMode(_))
            | (
                SettingKey::RunnerPython
                    | SettingKey::RunnerCppCompiler
                    | SettingKey::EditorCommand,
                SettingValue::String(_)
            )
            | (
                SettingKey::RunnerCppFlags | SettingKey::EditorArgs,
                SettingValue::StringList(_)
            )
            | (
                SettingKey::RunnerTimeoutSeconds | SettingKey::RunnerCompileTimeoutSeconds,
                SettingValue::Number(_)
            )
    );
    if valid {
        Ok(())
    } else {
        Err(SettingsDocumentError::WrongValueType {
            key,
            expected: key.input(),
        })
    }
}

fn toml_value(value: SettingValue) -> Value {
    match value {
        SettingValue::Language(Language::Cpp) => Value::from("cpp"),
        SettingValue::Language(Language::Python) => Value::from("python"),
        SettingValue::String(value) => Value::from(value),
        SettingValue::StringList(values) => {
            let mut array = Array::new();
            for value in values {
                array.push(value);
            }
            Value::Array(array)
        }
        SettingValue::Number(value) => Value::from(value),
        SettingValue::PythonRuntime(PythonRuntime::CPython) => Value::from("cpython"),
        SettingValue::PythonRuntime(PythonRuntime::PyPy) => Value::from("pypy"),
        SettingValue::EditorMode(EditorModeValue::Terminal) => Value::from("terminal"),
        SettingValue::EditorMode(EditorModeValue::External) => Value::from("external"),
        SettingValue::EditorMode(EditorModeValue::Auto) => {
            unreachable!("Auto is handled as reset before TOML conversion")
        }
    }
}

fn section_get<'a>(root: &'a Table, section: &str, name: &str) -> Option<&'a Value> {
    let section = root.get(section)?;
    match section {
        Item::Table(table) => table.get(name)?.as_value(),
        Item::Value(Value::InlineTable(table)) => table.get(name),
        _ => None,
    }
}

fn section_insert(root: &mut Table, section: &str, name: &str, new_value: Value) {
    fn preserve_value_decor(mut new_value: Value, old_value: Option<&Value>) -> Value {
        if let Some(old_value) = old_value {
            *new_value.decor_mut() = old_value.decor().clone();
        }
        new_value
    }

    match root.get_mut(section) {
        Some(Item::Table(table)) => {
            if let Some(item) = table.get_mut(name) {
                let new_value = preserve_value_decor(new_value, item.as_value());
                *item = Item::Value(new_value);
            } else {
                table.insert(name, Item::Value(new_value));
            }
        }
        Some(Item::Value(Value::InlineTable(table))) => {
            if let Some(old_value) = table.get_mut(name) {
                let new_value = preserve_value_decor(new_value, Some(old_value));
                *old_value = new_value;
            } else {
                table.insert(name, new_value);
            }
        }
        Some(_) => unreachable!("production Config parsing rejects non-table sections"),
        None => {
            let mut table = Table::new();
            table.insert(name, Item::Value(new_value));
            root.insert(section, Item::Table(table));
        }
    }
}

fn section_remove(root: &mut Table, section: &str, name: &str) {
    match root.get_mut(section) {
        Some(Item::Table(table)) => {
            table.remove(name);
        }
        Some(Item::Value(Value::InlineTable(table))) => {
            table.remove(name);
        }
        Some(_) | None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_explicit() -> &'static str {
        "# heading\n\
[defaults]\n\
language = \"cpp\" # language\n\
\n\
[runner]\n\
python = \"python\"\n\
cpp_compiler = \"g++\"\n\
cpp_flags = []\n\
timeout_seconds = 2.0\n\
compile_timeout_seconds = 10.0\n\
\n\
[submit]\n\
python_runtime = \"cpython\"\n\
\n\
[editor]\n\
command = \"nvim\"\n\
args = []\n\
mode = \"terminal\"\n"
    }

    #[test]
    fn definitions_cover_all_ten_settings_in_category_order() {
        assert_eq!(SettingKey::ALL.len(), 10);
        assert_eq!(SettingKey::ALL[0].category(), SettingsCategory::Defaults);
        assert_eq!(SettingKey::ALL[1].category(), SettingsCategory::Runner);
        assert_eq!(SettingKey::ALL[6].category(), SettingsCategory::Submit);
        assert_eq!(SettingKey::ALL[7].category(), SettingsCategory::Editor);
        assert_eq!(SettingKey::EditorMode.input(), SettingInput::Enum);
    }

    #[test]
    fn provenance_uses_presence_including_explicit_defaults_and_empty_arrays() {
        let mut document = SettingsDocument::parse(all_explicit()).unwrap();
        for key in SettingKey::ALL {
            assert!(document.is_modified(key), "{}", key.path());
        }
        assert_eq!(
            document.configured_value(SettingKey::DefaultLanguage),
            Some(SettingValue::Language(Language::Cpp))
        );
        assert_eq!(
            document.configured_value(SettingKey::RunnerCppFlags),
            Some(SettingValue::StringList(Vec::new()))
        );

        assert_eq!(
            document.reset(SettingKey::RunnerCppFlags).unwrap(),
            DocumentChange::Changed
        );
        assert!(!document.is_modified(SettingKey::RunnerCppFlags));
    }

    #[test]
    fn setting_all_typed_values_reuses_production_validation() {
        let mut document = SettingsDocument::empty();
        let changes = [
            (
                SettingKey::DefaultLanguage,
                SettingValue::Language(Language::Python),
            ),
            (
                SettingKey::RunnerPython,
                SettingValue::String("python3".into()),
            ),
            (
                SettingKey::RunnerCppCompiler,
                SettingValue::String("clang++".into()),
            ),
            (
                SettingKey::RunnerCppFlags,
                SettingValue::StringList(vec!["-std=c++23".into(), "-O0".into()]),
            ),
            (SettingKey::RunnerTimeoutSeconds, SettingValue::Number(3.5)),
            (
                SettingKey::RunnerCompileTimeoutSeconds,
                SettingValue::Number(12.0),
            ),
            (
                SettingKey::SubmitPythonRuntime,
                SettingValue::PythonRuntime(PythonRuntime::PyPy),
            ),
            (
                SettingKey::EditorCommand,
                SettingValue::String("code".into()),
            ),
            (
                SettingKey::EditorArgs,
                SettingValue::StringList(vec!["--reuse-window".into()]),
            ),
            (
                SettingKey::EditorMode,
                SettingValue::EditorMode(EditorModeValue::External),
            ),
        ];
        for (key, value) in changes {
            assert_eq!(document.set(key, value).unwrap(), DocumentChange::Changed);
        }
        let parsed = Config::parse(&document.candidate()).unwrap();
        assert_eq!(parsed.defaults.language, Language::Python);
        assert_eq!(parsed.runner.python, "python3");
        assert_eq!(parsed.runner.cpp_compiler, "clang++");
        assert_eq!(parsed.runner.cpp_flags, ["-std=c++23", "-O0"]);
        assert_eq!(parsed.runner.timeout_seconds, 3.5);
        assert_eq!(parsed.runner.compile_timeout_seconds, 12.0);
        assert_eq!(parsed.submit.python_runtime, PythonRuntime::PyPy);
        assert_eq!(parsed.editor.as_ref().unwrap().command, "code");
    }

    #[test]
    fn invalid_values_are_rejected_without_changing_the_candidate() {
        let mut document = SettingsDocument::empty();
        let before = document.candidate();
        for (key, value) in [
            (SettingKey::RunnerPython, SettingValue::String("  ".into())),
            (SettingKey::RunnerTimeoutSeconds, SettingValue::Number(0.0)),
            (
                SettingKey::RunnerTimeoutSeconds,
                SettingValue::Number(f64::NAN),
            ),
            (
                SettingKey::RunnerTimeoutSeconds,
                SettingValue::Number(f64::INFINITY),
            ),
        ] {
            assert!(document.set(key, value).is_err());
            assert_eq!(document.candidate(), before);
        }
    }

    #[test]
    fn same_explicit_typed_value_and_default_reset_do_not_rewrite() {
        let source = "[defaults]\nlanguage = \"CPP\" # keep case and comment\n";
        let mut document = SettingsDocument::parse(source).unwrap();
        assert_eq!(
            document
                .set(
                    SettingKey::DefaultLanguage,
                    SettingValue::Language(Language::Cpp)
                )
                .unwrap(),
            DocumentChange::Unchanged
        );
        assert_eq!(document.candidate(), source);

        let mut empty = SettingsDocument::empty();
        assert_eq!(
            empty.reset(SettingKey::DefaultLanguage).unwrap(),
            DocumentChange::Unchanged
        );
        assert_eq!(empty.candidate(), "");
    }

    #[test]
    fn edits_that_return_to_the_loaded_settings_restore_exact_original_text() {
        let source = "# untouched\n[runner]\n# independent comment\n";
        let mut document = SettingsDocument::parse(source).unwrap();

        document
            .set(
                SettingKey::RunnerPython,
                SettingValue::String("python-custom".to_string()),
            )
            .unwrap();
        document.reset(SettingKey::RunnerPython).unwrap();

        assert!(!document.has_changes());
        assert_eq!(document.candidate(), source);
    }

    #[test]
    fn unrelated_comments_blank_lines_order_and_unicode_are_preserved() {
        let source = "# 日本語 heading\n\
[runner] # runner table\n\
# compiler note\n\
cpp_compiler = 'g++' # keep me\n\
\n\
# timeout note\n\
timeout_seconds = 2.0\n\
\n\
[defaults]\n\
language = \"cpp\"\n";
        let mut document = SettingsDocument::parse(source).unwrap();
        document
            .set(SettingKey::RunnerTimeoutSeconds, SettingValue::Number(4.0))
            .unwrap();
        let candidate = document.candidate();
        assert!(candidate.contains("# 日本語 heading"));
        assert!(candidate.contains("# compiler note\ncpp_compiler = 'g++' # keep me"));
        assert!(candidate.contains("\n\n# timeout note\n"));
        assert!(candidate.find("[runner]").unwrap() < candidate.find("[defaults]").unwrap());
        assert_eq!(
            Config::parse(&candidate).unwrap().runner.timeout_seconds,
            4.0
        );
    }

    #[test]
    fn crlf_is_preserved_when_toml_edit_can_round_trip_it() {
        let source = "[runner]\r\npython = \"python\"\r\n# keep\r\n";
        let mut document = SettingsDocument::parse(source).unwrap();
        document
            .set(
                SettingKey::RunnerPython,
                SettingValue::String("python3".into()),
            )
            .unwrap();
        let candidate = document.candidate();
        assert!(candidate.contains("\r\n"));
        assert!(!candidate.replace("\r\n", "").contains('\n'));
        assert!(candidate.ends_with("\r\n# keep\r\n"));
    }

    #[test]
    fn dotted_keys_and_inline_tables_remain_supported() {
        let dotted = "defaults.language = \"cpp\"\nrunner.timeout_seconds = 2.0\n";
        let mut dotted_document = SettingsDocument::parse(dotted).unwrap();
        dotted_document
            .set(SettingKey::RunnerTimeoutSeconds, SettingValue::Number(5.0))
            .unwrap();
        let dotted_candidate = dotted_document.candidate();
        assert!(dotted_candidate.contains("defaults.language = \"cpp\""));
        assert_eq!(
            Config::parse(&dotted_candidate)
                .unwrap()
                .runner
                .timeout_seconds,
            5.0
        );

        let inline = "runner = { python = \"python\", timeout_seconds = 2.0 }\n";
        let mut inline_document = SettingsDocument::parse(inline).unwrap();
        inline_document
            .set(SettingKey::RunnerTimeoutSeconds, SettingValue::Number(6.0))
            .unwrap();
        let inline_candidate = inline_document.candidate();
        assert!(inline_candidate.starts_with("runner = {"));
        assert_eq!(
            Config::parse(&inline_candidate)
                .unwrap()
                .runner
                .timeout_seconds,
            6.0
        );
    }

    #[test]
    fn unsupported_trailing_newline_form_is_viewable_but_structured_write_is_refused() {
        let source = "[runner]\npython = \"python3\"";
        let mut document = SettingsDocument::parse(source).unwrap();

        assert!(document.preservation_issue().is_some());
        assert!(document.preservation_issue().is_some());
        assert_eq!(document.candidate(), source);
        let error = document
            .set(
                SettingKey::RunnerPython,
                SettingValue::String("python4".to_string()),
            )
            .unwrap_err();
        assert!(matches!(error, SettingsDocumentError::UnsupportedFormat(_)));
        assert_eq!(document.candidate(), source);
    }

    #[test]
    fn reset_removes_only_the_target_and_its_decor() {
        let source = "[runner]\n# belongs to python\npython = \"python3\" # inline\n# belongs to compiler\ncpp_compiler = \"clang++\"\n";
        let mut document = SettingsDocument::parse(source).unwrap();
        document.reset(SettingKey::RunnerPython).unwrap();
        let candidate = document.candidate();
        assert!(!candidate.contains("belongs to python"));
        assert!(!candidate.contains("python3"));
        assert!(candidate.contains("# belongs to compiler"));
        assert!(candidate.contains("cpp_compiler = \"clang++\""));
        assert!(candidate.contains("[runner]"));
    }

    #[test]
    fn reset_keeps_an_explicit_table_after_its_last_key_is_removed() {
        let mut document = SettingsDocument::parse("[runner]\npython = \"python3\"\n").unwrap();

        document.reset(SettingKey::RunnerPython).unwrap();

        assert_eq!(document.candidate(), "[runner]\n");
        assert_eq!(
            Config::parse(&document.candidate()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn resetting_editor_command_removes_the_complete_section() {
        let source = "# before\n[editor]\ncommand = \"code\"\nargs = [\"-r\"]\nmode = \"external\"\n\n[runner]\npython = \"python3\"\n";
        let mut document = SettingsDocument::parse(source).unwrap();
        document.reset(SettingKey::EditorCommand).unwrap();
        let candidate = document.candidate();
        assert!(!candidate.contains("[editor]"));
        assert!(!candidate.contains("command"));
        assert!(!candidate.contains("args"));
        assert!(!candidate.contains("mode"));
        assert!(candidate.contains("[runner]"));
        assert_eq!(Config::parse(&candidate).unwrap().editor, None);

        let mut inline = SettingsDocument::parse(
            "editor = { command = \"code\", args = [\"-r\"], mode = \"external\" }\n",
        )
        .unwrap();
        inline.reset(SettingKey::EditorCommand).unwrap();
        assert_eq!(inline.candidate(), "");
    }

    #[test]
    fn editor_dependencies_and_auto_mode_follow_the_config_contract() {
        let mut document = SettingsDocument::empty();
        assert!(!document.editable(SettingKey::EditorArgs));
        assert!(!document.editable(SettingKey::EditorMode));
        assert!(matches!(
            document.set(
                SettingKey::EditorArgs,
                SettingValue::StringList(vec!["-r".into()])
            ),
            Err(SettingsDocumentError::EditorCommandRequired(_))
        ));

        document
            .set(
                SettingKey::EditorCommand,
                SettingValue::String("code".into()),
            )
            .unwrap();
        document
            .set(
                SettingKey::EditorMode,
                SettingValue::EditorMode(EditorModeValue::External),
            )
            .unwrap();
        assert!(document.is_modified(SettingKey::EditorMode));
        document
            .set(
                SettingKey::EditorMode,
                SettingValue::EditorMode(EditorModeValue::Auto),
            )
            .unwrap();
        assert!(!document.is_modified(SettingKey::EditorMode));
        assert!(document.editor_configured());
    }

    #[test]
    fn malformed_unknown_and_invalid_config_are_rejected_by_production_parser() {
        for source in [
            "[runner\n",
            "unknown = true\n",
            "[unknown]\nvalue = true\n",
            "[runner]\ntimeout_seconds = 0\n",
            "[editor]\nargs = []\n",
            "[editor]\ncommand = \"\"\n",
            "[editor]\ncommand = \"nvim\"\nmode = \"embedded\"\n",
        ] {
            assert!(matches!(
                SettingsDocument::parse(source),
                Err(SettingsDocumentError::InvalidConfig(_))
            ));
        }
    }

    #[test]
    fn typed_set_rejects_values_belonging_to_a_different_enum_setting() {
        let mut document = SettingsDocument::empty();
        let error = document
            .set(
                SettingKey::DefaultLanguage,
                SettingValue::PythonRuntime(PythonRuntime::PyPy),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            SettingsDocumentError::WrongValueType {
                key: SettingKey::DefaultLanguage,
                expected: SettingInput::Enum,
            }
        ));
        assert_eq!(document.candidate(), "");
    }
}
