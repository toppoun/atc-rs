use crate::config::Config;
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;

#[cfg(windows)]
mod windows;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EditorLaunchMode {
    External,
    Terminal,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditorConfig {
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    #[serde(default)]
    pub(crate) mode: Option<EditorLaunchMode>,
}

impl EditorConfig {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.command.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "editor.command must not be empty",
            ));
        }
        Ok(())
    }

    pub(crate) fn launch_mode(&self) -> EditorLaunchMode {
        self.mode
            .unwrap_or_else(|| infer_launch_mode(OsStr::new(&self.command)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorSource {
    Config,
    IntegratedVsCode,
    IntegratedCursor,
    VisualEnv,
    EditorEnv,
}

impl fmt::Display for EditorSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Config => "configured editor",
            Self::IntegratedVsCode => "auto-detected VS Code",
            Self::IntegratedCursor => "auto-detected Cursor",
            Self::VisualEnv => "VISUAL editor",
            Self::EditorEnv => "EDITOR editor",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedEditor {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) mode: EditorLaunchMode,
    pub(crate) source: EditorSource,
}

#[derive(Debug)]
pub(crate) enum EditorError {
    InvalidDeclaration {
        variable: &'static str,
        reason: &'static str,
    },
    Unresolved,
    Spawn {
        source: EditorSource,
        program: OsString,
        error: io::Error,
    },
    TerminalExit {
        source: EditorSource,
        program: OsString,
        status: ExitStatus,
    },
    Reaper {
        source: EditorSource,
        error: io::Error,
    },
}

impl fmt::Display for EditorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDeclaration { variable, reason } => write!(
                formatter,
                "{variable} is not a valid editor declaration: {reason}; use [editor].command and [editor].args for an unambiguous override"
            ),
            Self::Unresolved => formatter.write_str(
                "No editor configured.\n\nSet VISUAL or EDITOR, or configure [editor] in atc settings.",
            ),
            Self::Spawn {
                source,
                program,
                error,
            } => write!(
                formatter,
                "failed to launch {source} ({program:?}): {error}"
            ),
            Self::TerminalExit {
                source,
                program,
                status,
            } => write!(
                formatter,
                "{source} ({program:?}) exited unsuccessfully: {status}"
            ),
            Self::Reaper { source, error } => {
                write!(formatter, "failed to prepare {source} process cleanup: {error}")
            }
        }
    }
}

impl std::error::Error for EditorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { error, .. } | Self::Reaper { error, .. } => Some(error),
            Self::InvalidDeclaration { .. } | Self::Unresolved | Self::TerminalExit { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorBrand {
    VsCode,
    Cursor,
}

#[derive(Debug, Clone)]
struct WindowsProcess {
    executable_name: OsString,
    executable_path: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct ResolutionInputs {
    integrated: Option<ResolvedEditor>,
    visual: Option<OsString>,
    editor: Option<OsString>,
}

impl ResolutionInputs {
    fn current() -> Self {
        Self {
            integrated: detect_integrated_editor(),
            visual: std::env::var_os("VISUAL"),
            editor: std::env::var_os("EDITOR"),
        }
    }
}

pub(crate) fn resolve(config: &Config) -> Result<ResolvedEditor, EditorError> {
    resolve_with(config.editor.as_ref(), ResolutionInputs::current())
}

fn resolve_with(
    configured: Option<&EditorConfig>,
    inputs: ResolutionInputs,
) -> Result<ResolvedEditor, EditorError> {
    if let Some(configured) = configured {
        return Ok(ResolvedEditor {
            program: OsString::from(&configured.command),
            args: configured.args.iter().map(OsString::from).collect(),
            mode: configured.launch_mode(),
            source: EditorSource::Config,
        });
    }

    if let Some(integrated) = inputs.integrated {
        return Ok(integrated);
    }

    if let Some(visual) = nonblank_environment_value(inputs.visual) {
        return resolve_environment("VISUAL", visual, EditorSource::VisualEnv);
    }

    if let Some(editor) = nonblank_environment_value(inputs.editor) {
        return resolve_environment("EDITOR", editor, EditorSource::EditorEnv);
    }

    Err(EditorError::Unresolved)
}

fn nonblank_environment_value(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.to_string_lossy().trim().is_empty())
}

fn resolve_environment(
    variable: &'static str,
    declaration: OsString,
    source: EditorSource,
) -> Result<ResolvedEditor, EditorError> {
    let mut words = parse_editor_declaration(&declaration)
        .map_err(|reason| EditorError::InvalidDeclaration { variable, reason })?;
    if words.is_empty() || words[0].is_empty() {
        return Err(EditorError::InvalidDeclaration {
            variable,
            reason: "the program token is empty",
        });
    }

    let args = words.split_off(1);
    let program = words.pop().expect("non-empty declaration was checked");
    let mode = infer_launch_mode(&program);
    Ok(ResolvedEditor {
        program,
        args,
        mode,
        source,
    })
}

#[cfg(windows)]
fn parse_editor_declaration(value: &OsStr) -> Result<Vec<OsString>, &'static str> {
    windows::parse_command_line(value)
}

#[cfg(unix)]
fn parse_editor_declaration(value: &OsStr) -> Result<Vec<OsString>, &'static str> {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    shlex::bytes::split(value.as_bytes())
        .map(|words| words.into_iter().map(OsString::from_vec).collect())
        .ok_or("the quoting is malformed")
}

#[cfg(not(any(unix, windows)))]
fn parse_editor_declaration(_value: &OsStr) -> Result<Vec<OsString>, &'static str> {
    Err("command-line parsing is unsupported on this platform")
}

pub(crate) fn infer_launch_mode(program: &OsStr) -> EditorLaunchMode {
    match normalized_program_basename(program).as_deref() {
        Some("code" | "code-insiders" | "cursor" | "subl" | "zed" | "windsurf") => {
            EditorLaunchMode::External
        }
        _ => EditorLaunchMode::Terminal,
    }
}

fn normalized_program_basename(program: &OsStr) -> Option<String> {
    // Ask the native path implementation for the basename before converting it to UTF-8. This
    // preserves a recognizable ASCII executable name when only a parent directory is non-Unicode.
    let platform_basename = Path::new(program).file_name().unwrap_or(program);
    let text = platform_basename.to_str()?;
    // Keep accepting either separator in explicit cross-platform configuration and in tests.
    let basename = text.rsplit(['/', '\\']).next()?;
    let lowercase = basename.to_ascii_lowercase();
    Some(
        lowercase
            .strip_suffix(".exe")
            .or_else(|| lowercase.strip_suffix(".cmd"))
            .unwrap_or(&lowercase)
            .to_owned(),
    )
}

fn classify_integrated_executable(program: &OsStr) -> Option<EditorBrand> {
    match normalized_program_basename(program).as_deref() {
        Some("code") => Some(EditorBrand::VsCode),
        Some("cursor") => Some(EditorBrand::Cursor),
        _ => None,
    }
}

fn select_windows_ancestor(processes: &[WindowsProcess]) -> Option<ResolvedEditor> {
    for process in processes {
        let name_brand = classify_integrated_executable(&process.executable_name);
        let path_brand = process
            .executable_path
            .as_deref()
            .and_then(|path| classify_integrated_executable(path.as_os_str()));
        if name_brand.is_some() && path_brand.is_some() && name_brand != path_brand {
            continue;
        }
        let Some(brand) = path_brand.or(name_brand) else {
            continue;
        };
        let path = process.executable_path.as_ref().filter(|path| {
            path.is_absolute() && classify_integrated_executable(path.as_os_str()) == Some(brand)
        });
        return Some(windows_integrated_editor(brand, path.cloned()));
    }
    None
}

fn windows_integrated_editor(brand: EditorBrand, executable: Option<PathBuf>) -> ResolvedEditor {
    let program = executable.map_or_else(
        || {
            OsString::from(match brand {
                EditorBrand::VsCode => "code",
                EditorBrand::Cursor => "cursor",
            })
        },
        PathBuf::into_os_string,
    );
    ResolvedEditor {
        program,
        args: Vec::new(),
        mode: EditorLaunchMode::External,
        source: source_for_brand(brand),
    }
}

fn windows_environment_brand(
    term_program: Option<&OsStr>,
    paths: &[OsString],
) -> Option<EditorBrand> {
    if term_program.and_then(OsStr::to_str) != Some("vscode") {
        return None;
    }
    unique_brand(
        paths
            .iter()
            .filter_map(|path| classify_integrated_executable(path)),
    )
}

fn classify_macos_application_path(path: &OsStr) -> Option<EditorBrand> {
    Path::new(path).components().find_map(|component| {
        let component = component.as_os_str();
        if component.eq_ignore_ascii_case("Visual Studio Code.app") {
            Some(EditorBrand::VsCode)
        } else if component.eq_ignore_ascii_case("Cursor.app") {
            Some(EditorBrand::Cursor)
        } else {
            None
        }
    })
}

fn macos_environment_brand(
    term_program: Option<&OsStr>,
    paths: &[OsString],
) -> Option<EditorBrand> {
    if term_program.and_then(OsStr::to_str) != Some("vscode") {
        return None;
    }
    unique_brand(
        paths
            .iter()
            .filter_map(|path| classify_macos_application_path(path)),
    )
}

fn unique_brand(brands: impl IntoIterator<Item = EditorBrand>) -> Option<EditorBrand> {
    let mut brands = brands.into_iter();
    let first = brands.next()?;
    brands.all(|brand| brand == first).then_some(first)
}

fn macos_integrated_editor(brand: EditorBrand) -> ResolvedEditor {
    ResolvedEditor {
        program: OsString::from("open"),
        args: vec![
            OsString::from("-a"),
            OsString::from(match brand {
                EditorBrand::VsCode => "Visual Studio Code",
                EditorBrand::Cursor => "Cursor",
            }),
        ],
        mode: EditorLaunchMode::External,
        source: source_for_brand(brand),
    }
}

fn source_for_brand(brand: EditorBrand) -> EditorSource {
    match brand {
        EditorBrand::VsCode => EditorSource::IntegratedVsCode,
        EditorBrand::Cursor => EditorSource::IntegratedCursor,
    }
}

#[cfg(windows)]
fn detect_integrated_editor() -> Option<ResolvedEditor> {
    windows::detect_integrated_editor()
}

#[cfg(target_os = "macos")]
fn detect_integrated_editor() -> Option<ResolvedEditor> {
    let term_program = std::env::var_os("TERM_PROGRAM");
    let evidence = [
        std::env::var_os("VSCODE_GIT_ASKPASS_MAIN"),
        std::env::var_os("VSCODE_GIT_ASKPASS_NODE"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    macos_environment_brand(term_program.as_deref(), &evidence).map(macos_integrated_editor)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn detect_integrated_editor() -> Option<ResolvedEditor> {
    None
}

pub(crate) fn launch(editor: &ResolvedEditor, target: &Path) -> Result<(), EditorError> {
    let mut command = editor_command(editor, target);
    match editor.mode {
        EditorLaunchMode::Terminal => launch_terminal(editor, &mut command),
        EditorLaunchMode::External => launch_external(editor, command),
    }
}

fn launch_terminal(editor: &ResolvedEditor, command: &mut Command) -> Result<(), EditorError> {
    let status = command.status().map_err(|error| EditorError::Spawn {
        source: editor.source,
        program: editor.program.clone(),
        error,
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(EditorError::TerminalExit {
            source: editor.source,
            program: editor.program.clone(),
            status,
        })
    }
}

fn editor_command(editor: &ResolvedEditor, target: &Path) -> Command {
    let mut command = Command::new(&editor.program);
    command.args(&editor.args).arg(target);
    match editor.mode {
        EditorLaunchMode::Terminal => {
            command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
        EditorLaunchMode::External => {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        }
    }
    command
}

fn launch_external(editor: &ResolvedEditor, mut command: Command) -> Result<(), EditorError> {
    launch_external_with_reap_observer(editor, &mut command, None)
}

fn launch_external_with_reap_observer(
    editor: &ResolvedEditor,
    command: &mut Command,
    reap_observer: Option<mpsc::Sender<io::Result<ExitStatus>>>,
) -> Result<(), EditorError> {
    let (sender, receiver) = mpsc::sync_channel::<std::process::Child>(1);
    thread::Builder::new()
        .name("atc-editor-reaper".to_owned())
        .spawn(move || {
            if let Ok(mut child) = receiver.recv() {
                let result = child.wait();
                if let Some(observer) = reap_observer {
                    let _ = observer.send(result);
                }
            }
        })
        .map_err(|error| EditorError::Reaper {
            source: editor.source,
            error,
        })?;

    let child = command.spawn().map_err(|error| EditorError::Spawn {
        source: editor.source,
        program: editor.program.clone(),
        error,
    })?;
    if let Err(mpsc::SendError(mut child)) = sender.send(child) {
        let kill_error = child.kill().err();
        let wait_error = child.wait().err();
        let detail = wait_error.or(kill_error).map_or_else(
            || "reaper stopped before accepting the child".to_owned(),
            |error| format!("reaper stopped before accepting the child; cleanup failed: {error}"),
        );
        return Err(EditorError::Reaper {
            source: editor.source,
            error: io::Error::new(io::ErrorKind::BrokenPipe, detail),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    fn configured(command: &str, args: &[&str], mode: Option<EditorLaunchMode>) -> EditorConfig {
        EditorConfig {
            command: command.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            mode,
        }
    }

    fn inputs(
        integrated: Option<ResolvedEditor>,
        visual: Option<&str>,
        editor: Option<&str>,
    ) -> ResolutionInputs {
        ResolutionInputs {
            integrated,
            visual: visual.map(OsString::from),
            editor: editor.map(OsString::from),
        }
    }

    #[test]
    fn known_gui_mode_inference_uses_an_exact_normalized_basename() {
        for program in [
            "code",
            "/usr/local/bin/code",
            "code.exe",
            "code.cmd",
            "code-insiders",
            "cursor",
            r"C:\Tools\Cursor.exe",
            "subl",
            "zed",
            "windsurf",
        ] {
            assert_eq!(
                infer_launch_mode(OsStr::new(program)),
                EditorLaunchMode::External,
                "program {program:?}"
            );
        }
        for program in ["nvim", "vim", "hx", "nano", "my-editor", "my-code-wrapper"] {
            assert_eq!(
                infer_launch_mode(OsStr::new(program)),
                EditorLaunchMode::Terminal,
                "program {program:?}"
            );
        }
    }

    #[test]
    fn explicit_config_wins_and_preserves_arguments_and_mode() {
        let config = configured(
            "my-custom-editor",
            &["--first", "second value"],
            Some(EditorLaunchMode::External),
        );
        let detected = windows_integrated_editor(EditorBrand::VsCode, None);

        let resolved = resolve_with(
            Some(&config),
            inputs(Some(detected), Some("nvim -f"), Some("vim")),
        )
        .unwrap();

        assert_eq!(resolved.program, "my-custom-editor");
        assert_eq!(resolved.args, ["--first", "second value"]);
        assert_eq!(resolved.mode, EditorLaunchMode::External);
        assert_eq!(resolved.source, EditorSource::Config);
    }

    #[test]
    fn integrated_editor_wins_over_visual_and_editor() {
        let detected = windows_integrated_editor(EditorBrand::Cursor, None);

        let resolved =
            resolve_with(None, inputs(Some(detected), Some("nvim"), Some("vim"))).unwrap();

        assert_eq!(resolved.program, "cursor");
        assert_eq!(resolved.source, EditorSource::IntegratedCursor);
    }

    #[test]
    fn visual_wins_over_editor_and_editor_is_the_final_fallback() {
        let visual = resolve_with(None, inputs(None, Some("nvim -f"), Some("vim"))).unwrap();
        assert_eq!(visual.program, "nvim");
        assert_eq!(visual.args, ["-f"]);
        assert_eq!(visual.source, EditorSource::VisualEnv);

        let editor = resolve_with(None, inputs(None, None, Some("code --reuse-window"))).unwrap();
        assert_eq!(editor.program, "code");
        assert_eq!(editor.args, ["--reuse-window"]);
        assert_eq!(editor.mode, EditorLaunchMode::External);
        assert_eq!(editor.source, EditorSource::EditorEnv);
    }

    #[test]
    fn blank_environment_values_are_unset_but_invalid_present_values_are_errors() {
        let resolved = resolve_with(None, inputs(None, Some("   "), Some("vim"))).unwrap();
        assert_eq!(resolved.program, "vim");

        let error =
            resolve_with(None, inputs(None, Some("\"unterminated"), Some("vim"))).unwrap_err();
        assert!(matches!(
            error,
            EditorError::InvalidDeclaration {
                variable: "VISUAL",
                ..
            }
        ));

        let error = resolve_with(None, inputs(None, None, Some("\"unterminated"))).unwrap_err();
        assert!(matches!(
            error,
            EditorError::InvalidDeclaration {
                variable: "EDITOR",
                ..
            }
        ));

        let error = resolve_with(None, inputs(None, None, Some("\"\""))).unwrap_err();
        assert!(matches!(
            error,
            EditorError::InvalidDeclaration {
                variable: "EDITOR",
                ..
            }
        ));
    }

    #[test]
    fn plain_terminal_without_declarations_remains_unresolved() {
        let error = resolve_with(None, inputs(None, None, None)).unwrap_err();
        assert!(matches!(error, EditorError::Unresolved));
        assert!(error.to_string().contains("Set VISUAL or EDITOR"));
    }

    #[test]
    fn installed_editor_commands_are_not_resolution_evidence() {
        // There is intentionally no PATH input in the resolver. Installed commands cannot turn a
        // plain terminal into an integrated-editor terminal.
        let error = resolve_with(None, inputs(None, None, None)).unwrap_err();
        assert!(matches!(error, EditorError::Unresolved));
    }

    #[test]
    fn active_environment_parser_keeps_program_and_arguments_separate() {
        let simple = resolve_environment("EDITOR", "nvim".into(), EditorSource::EditorEnv).unwrap();
        assert_eq!(simple.program, "nvim");
        assert!(simple.args.is_empty());

        let with_args = resolve_environment(
            "EDITOR",
            "code --reuse-window".into(),
            EditorSource::EditorEnv,
        )
        .unwrap();
        assert_eq!(with_args.program, "code");
        assert_eq!(with_args.args, ["--reuse-window"]);
        assert_eq!(with_args.mode, EditorLaunchMode::External);
    }

    #[cfg(windows)]
    #[test]
    fn windows_environment_parser_supports_quoted_program_paths() {
        let resolved = resolve_environment(
            "EDITOR",
            r#""C:\Program Files\Some Editor\editor.exe" --wait"#.into(),
            EditorSource::EditorEnv,
        )
        .unwrap();

        assert_eq!(resolved.program, r"C:\Program Files\Some Editor\editor.exe");
        assert_eq!(resolved.args, ["--wait"]);
        assert_eq!(resolved.mode, EditorLaunchMode::Terminal);
    }

    #[cfg(windows)]
    #[test]
    fn windows_basename_normalization_does_not_require_unicode_parent_directories() {
        use std::os::windows::ffi::OsStringExt as _;

        let mut path = vec![u16::from(b'C'), u16::from(b':'), u16::from(b'\\'), 0xd800];
        path.push(u16::from(b'\\'));
        path.extend("Cursor.exe".encode_utf16());
        let path = OsString::from_wide(&path);

        assert_eq!(infer_launch_mode(&path), EditorLaunchMode::External);
        assert_eq!(
            classify_integrated_executable(&path),
            Some(EditorBrand::Cursor)
        );
    }

    #[cfg(unix)]
    #[test]
    fn posix_environment_parser_supports_quoted_program_paths() {
        let resolved = resolve_environment(
            "EDITOR",
            r#"'/Applications/Some Editor/editor' --wait"#.into(),
            EditorSource::EditorEnv,
        )
        .unwrap();

        assert_eq!(resolved.program, "/Applications/Some Editor/editor");
        assert_eq!(resolved.args, ["--wait"]);
    }

    #[cfg(unix)]
    #[test]
    fn posix_environment_parser_preserves_non_utf8_program_paths() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let declaration = OsString::from_vec(b"'/tmp/\xff editor' --wait".to_vec());
        let resolved = resolve_environment("EDITOR", declaration, EditorSource::EditorEnv).unwrap();

        assert_eq!(resolved.program.as_os_str().as_bytes(), b"/tmp/\xff editor");
        assert_eq!(resolved.args, ["--wait"]);
    }

    #[cfg(unix)]
    #[test]
    fn macos_bundle_classification_does_not_require_unicode_parent_directories() {
        use std::os::unix::ffi::OsStringExt as _;

        let path = OsStr::from_bytes(
            b"/Volumes/invalid-\xff/Cursor.app/Contents/Frameworks/Cursor Helper (Plugin)",
        );
        assert_eq!(
            classify_macos_application_path(path),
            Some(EditorBrand::Cursor)
        );
    }

    #[test]
    fn windows_executable_classifier_and_ancestor_order_are_conservative() {
        let vscode =
            PathBuf::from(r"C:\Users\name\AppData\Local\Programs\Microsoft VS Code\Code.exe");
        let cursor = PathBuf::from(r"C:\Users\name\AppData\Local\Programs\cursor\Cursor.exe");
        assert_eq!(
            classify_integrated_executable(vscode.as_os_str()),
            Some(EditorBrand::VsCode)
        );
        assert_eq!(
            classify_integrated_executable(cursor.as_os_str()),
            Some(EditorBrand::Cursor)
        );

        let processes = [
            WindowsProcess {
                executable_name: "pwsh.exe".into(),
                executable_path: Some(PathBuf::from(r"C:\Windows\pwsh.exe")),
            },
            WindowsProcess {
                executable_name: "Cursor.exe".into(),
                executable_path: Some(cursor.clone()),
            },
            WindowsProcess {
                executable_name: "Code.exe".into(),
                executable_path: Some(vscode),
            },
        ];
        let selected = select_windows_ancestor(&processes).unwrap();
        assert_eq!(selected.program, cursor.into_os_string());
        assert_eq!(selected.source, EditorSource::IntegratedCursor);
        assert_eq!(selected.mode, EditorLaunchMode::External);
    }

    #[test]
    fn windows_integrated_representation_preserves_absolute_launcher_and_has_brand_fallback() {
        let code =
            PathBuf::from(r"C:\Users\name\AppData\Local\Programs\Microsoft VS Code\Code.exe");
        let exact = windows_integrated_editor(EditorBrand::VsCode, Some(code.clone()));
        assert_eq!(exact.program, code.into_os_string());
        assert_eq!(exact.source, EditorSource::IntegratedVsCode);
        assert_eq!(exact.mode, EditorLaunchMode::External);

        let fallback = windows_integrated_editor(EditorBrand::Cursor, None);
        assert_eq!(fallback.program, "cursor");
        assert!(fallback.args.is_empty());
        assert_eq!(fallback.source, EditorSource::IntegratedCursor);
    }

    #[test]
    fn windows_environment_requires_vscode_family_terminal_and_unambiguous_brand_evidence() {
        let vscode = OsString::from(r"C:\Programs\Microsoft VS Code\Code.exe");
        let cursor = OsString::from(r"C:\Programs\Cursor\Cursor.exe");
        assert_eq!(
            windows_environment_brand(Some(OsStr::new("vscode")), std::slice::from_ref(&vscode)),
            Some(EditorBrand::VsCode)
        );
        assert_eq!(
            windows_environment_brand(Some(OsStr::new("vscode")), std::slice::from_ref(&cursor)),
            Some(EditorBrand::Cursor)
        );
        assert_eq!(
            windows_environment_brand(Some(OsStr::new("vscode")), &[vscode, cursor]),
            None
        );
        assert_eq!(
            windows_environment_brand(Some(OsStr::new("Windows_Terminal")), &[]),
            None
        );
        assert_eq!(
            windows_environment_brand(Some(OsStr::new("vscode")), &[]),
            None
        );
    }

    #[test]
    fn macos_application_bundle_classifier_ignores_installation_root() {
        for path in [
            "/Applications/Visual Studio Code.app/Contents/MacOS/Electron",
            "/Applications/Visual Studio Code.app/Contents/Resources/app/extensions/git/dist/askpass-main.js",
        ] {
            assert_eq!(
                classify_macos_application_path(OsStr::new(path)),
                Some(EditorBrand::VsCode)
            );
        }
        for path in [
            "/Applications/Cursor.app/Contents/MacOS/Cursor",
            "/Volumes/Cursor Installer/Cursor.app/Contents/Frameworks/Cursor Helper (Plugin)",
        ] {
            assert_eq!(
                classify_macos_application_path(OsStr::new(path)),
                Some(EditorBrand::Cursor)
            );
        }
    }

    #[test]
    fn macos_detection_requires_term_program_and_concrete_brand_evidence() {
        let cursor = OsString::from(
            "/Volumes/Cursor Installer/Cursor.app/Contents/Frameworks/Cursor Helper (Plugin)",
        );
        assert_eq!(
            macos_environment_brand(Some(OsStr::new("vscode")), &[cursor]),
            Some(EditorBrand::Cursor)
        );
        assert_eq!(
            macos_environment_brand(Some(OsStr::new("vscode")), &[]),
            None
        );
        assert_eq!(
            macos_environment_brand(Some(OsStr::new("Apple_Terminal")), &[]),
            None
        );
    }

    #[test]
    fn macos_integrated_launch_representation_uses_open_a() {
        let vscode = macos_integrated_editor(EditorBrand::VsCode);
        assert_eq!(vscode.program, "open");
        assert_eq!(vscode.args, ["-a", "Visual Studio Code"]);
        assert_eq!(vscode.mode, EditorLaunchMode::External);

        let cursor = macos_integrated_editor(EditorBrand::Cursor);
        assert_eq!(cursor.program, "open");
        assert_eq!(cursor.args, ["-a", "Cursor"]);
        assert_eq!(cursor.mode, EditorLaunchMode::External);
    }

    #[test]
    fn command_arguments_are_configured_args_then_target_without_interpolation() {
        let editor = ResolvedEditor {
            program: "editor".into(),
            args: vec!["--first".into(), "two words".into()],
            mode: EditorLaunchMode::Terminal,
            source: EditorSource::Config,
        };
        let target = Path::new(r"directory with spaces\$literal source.cpp");
        let command = editor_command(&editor, target);

        assert_eq!(command.get_program(), OsStr::new("editor"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("--first"),
                OsStr::new("two words"),
                target.as_os_str()
            ]
        );
    }

    fn test_executable_editor(mode: EditorLaunchMode, helper: &str) -> (ResolvedEditor, PathBuf) {
        (
            ResolvedEditor {
                program: std::env::current_exe().unwrap().into_os_string(),
                args: vec![
                    "--exact".into(),
                    format!("editor::tests::{helper}").into(),
                    "--ignored".into(),
                ],
                mode,
                source: EditorSource::Config,
            },
            PathBuf::from("--nocapture"),
        )
    }

    #[test]
    fn terminal_launch_waits_for_completion_and_returns_success() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("started");
        let release = temp.path().join("release");
        let completed = temp.path().join("completed");
        let (editor, target) =
            test_executable_editor(EditorLaunchMode::Terminal, "blocking_helper");
        let mut command = editor_command(&editor, &target);
        command
            .env("ATC_EDITOR_TEST_MARKER", &marker)
            .env("ATC_EDITOR_TEST_RELEASE", &release)
            .env("ATC_EDITOR_TEST_COMPLETED", &completed);
        let coordinator = thread::spawn({
            let marker = marker.clone();
            let release = release.clone();
            move || {
                wait_for_test_file(&marker);
                assert!(!release.exists());
                fs::write(release, b"release").unwrap();
            }
        });

        launch_terminal(&editor, &mut command).unwrap();
        coordinator.join().unwrap();
        assert!(completed.exists());
    }

    #[test]
    fn terminal_launch_reports_nonzero_exit() {
        let (editor, target) =
            test_executable_editor(EditorLaunchMode::Terminal, "terminal_failure_helper");

        let error = launch(&editor, &target).unwrap_err();

        assert!(matches!(error, EditorError::TerminalExit { .. }));
    }

    #[test]
    fn launch_reports_immediate_spawn_failures_in_both_modes() {
        for mode in [EditorLaunchMode::Terminal, EditorLaunchMode::External] {
            let editor = ResolvedEditor {
                program: "atc-editor-that-does-not-exist-7f34bf29".into(),
                args: Vec::new(),
                mode,
                source: EditorSource::Config,
            };
            let error = launch(&editor, Path::new("target")).unwrap_err();
            assert!(matches!(error, EditorError::Spawn { .. }));
        }
    }

    #[test]
    fn external_launch_returns_before_exit_and_reaps_the_launcher() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("started");
        let release = temp.path().join("release");
        let completed = temp.path().join("completed");
        let (editor, target) =
            test_executable_editor(EditorLaunchMode::External, "blocking_helper");
        let mut command = editor_command(&editor, &target);
        command
            .env("ATC_EDITOR_TEST_MARKER", &marker)
            .env("ATC_EDITOR_TEST_RELEASE", &release)
            .env("ATC_EDITOR_TEST_COMPLETED", &completed);
        let (reaped_tx, reaped_rx) = mpsc::channel();

        launch_external_with_reap_observer(&editor, &mut command, Some(reaped_tx)).unwrap();

        wait_for_test_file(&marker);
        assert!(!completed.exists(), "external launch waited for child exit");
        fs::write(&release, b"release").unwrap();
        let status = reaped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("external child was not reaped")
            .expect("external child wait failed");
        assert!(status.success());
        assert!(completed.exists());
    }

    #[test]
    #[ignore = "launched as a child process by editor tests"]
    fn terminal_failure_helper() {
        std::process::exit(7);
    }

    #[test]
    #[ignore = "launched as a child process by editor tests"]
    fn blocking_helper() {
        let marker = PathBuf::from(std::env::var_os("ATC_EDITOR_TEST_MARKER").unwrap());
        let release = PathBuf::from(std::env::var_os("ATC_EDITOR_TEST_RELEASE").unwrap());
        let completed = PathBuf::from(std::env::var_os("ATC_EDITOR_TEST_COMPLETED").unwrap());
        fs::write(marker, b"started").unwrap();
        wait_for_test_file(&release);
        fs::write(completed, b"completed").unwrap();
    }

    fn wait_for_test_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(path.exists(), "timed out waiting for {}", path.display());
    }
}
