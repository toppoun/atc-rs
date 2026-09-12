use std::path::{Path, PathBuf};

use super::terminal::{KeyCode, KeyEvent, KeyEventKind};
use crate::language::Language;
use crate::template::{self, SourceTemplateOrigin};
use crate::user_config_fs::{self, EditableFileState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TemplateStatus {
    Ready,
    Missing,
    Invalid,
}

impl TemplateStatus {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Missing => "Missing",
            Self::Invalid => "Invalid",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TemplateAction {
    Open,
    InitializeAndOpen,
    OpenToRepair,
}

impl TemplateAction {
    pub(super) const fn footer_label(self) -> &'static str {
        match self {
            Self::Open => "[Enter] Open",
            Self::InitializeAndOpen => "[Enter] Initialize & Open",
            Self::OpenToRepair => "[Enter] Open to Repair",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TemplateInspection {
    pub(super) status: TemplateStatus,
    pub(super) action: Option<TemplateAction>,
    pub(super) detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TemplateRow {
    pub(super) language: Language,
    pub(super) filename: &'static str,
    pub(super) status: TemplateStatus,
    pub(super) is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TemplateRequest {
    pub(super) language: Language,
    pub(super) path: PathBuf,
    pub(super) action: TemplateAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TemplateModalTransition {
    NotHandled,
    Handled,
    Close,
    Activate(TemplateRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OpenTemplateModal {
    templates_dir: Result<PathBuf, String>,
    selected_index: usize,
    default_language: Language,
    pub(super) error: Option<String>,
}

impl OpenTemplateModal {
    pub(super) fn new(
        templates_dir: Result<PathBuf, String>,
        initial_language: Language,
        default_language: Language,
    ) -> Self {
        let selected_index = Language::ALL
            .iter()
            .position(|language| *language == initial_language)
            .unwrap_or_default();
        Self {
            templates_dir,
            selected_index,
            default_language,
            error: None,
        }
    }

    pub(super) fn selected_language(&self) -> Language {
        Language::ALL[self.selected_index]
    }

    #[cfg(test)]
    pub(super) fn default_language(&self) -> Language {
        self.default_language
    }

    pub(super) fn path_for(&self, language: Language) -> Result<PathBuf, String> {
        self.templates_dir
            .as_deref()
            .map(|directory| template::source_template_path(directory, language))
            .map_err(Clone::clone)
    }

    pub(super) fn selected_path(&self) -> Result<PathBuf, String> {
        self.path_for(self.selected_language())
    }

    pub(super) fn rows(&self) -> Vec<TemplateRow> {
        Language::ALL
            .into_iter()
            .map(|language| TemplateRow {
                language,
                filename: template::source_template_filename(language),
                status: self.inspect(language).status,
                is_default: language == self.default_language,
            })
            .collect()
    }

    pub(super) fn selected_inspection(&self) -> TemplateInspection {
        self.inspect(self.selected_language())
    }

    pub(super) fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> TemplateModalTransition {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return TemplateModalTransition::NotHandled;
        }
        if key.kind == KeyEventKind::Press && key.code == KeyCode::Escape {
            return TemplateModalTransition::Close;
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_previous();
                TemplateModalTransition::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next();
                TemplateModalTransition::Handled
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => self.activation(),
            KeyCode::Char('i') if key.kind == KeyEventKind::Press => {
                if self.selected_inspection().action == Some(TemplateAction::InitializeAndOpen) {
                    self.activation()
                } else {
                    TemplateModalTransition::Handled
                }
            }
            _ => TemplateModalTransition::NotHandled,
        }
    }

    fn activation(&self) -> TemplateModalTransition {
        let language = self.selected_language();
        let inspection = self.inspect(language);
        let (Ok(path), Some(action)) = (self.path_for(language), inspection.action) else {
            return TemplateModalTransition::Handled;
        };
        TemplateModalTransition::Activate(TemplateRequest {
            language,
            path,
            action,
        })
    }

    fn select_previous(&mut self) {
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(Language::ALL.len().saturating_sub(1));
        self.error = None;
    }

    fn select_next(&mut self) {
        self.selected_index = self.selected_index.saturating_add(1) % Language::ALL.len();
        self.error = None;
    }

    fn inspect(&self, language: Language) -> TemplateInspection {
        let templates_dir = match self.templates_dir.as_deref() {
            Ok(directory) => directory,
            Err(error) => {
                return TemplateInspection {
                    status: TemplateStatus::Invalid,
                    action: None,
                    detail: Some(error.clone()),
                };
            }
        };

        match template::resolve_source_template_with_origin_in(templates_dir, language) {
            Ok(resolved) => match resolved.origin {
                SourceTemplateOrigin::UserOverride(_) => TemplateInspection {
                    status: TemplateStatus::Ready,
                    action: Some(TemplateAction::Open),
                    detail: None,
                },
                SourceTemplateOrigin::BuiltIn => TemplateInspection {
                    status: TemplateStatus::Missing,
                    action: Some(TemplateAction::InitializeAndOpen),
                    detail: None,
                },
            },
            Err(error) => {
                let detail = error.to_string();
                let action = self
                    .path_for(language)
                    .ok()
                    .and_then(|path| repair_action(&path));
                TemplateInspection {
                    status: TemplateStatus::Invalid,
                    action,
                    detail: Some(detail),
                }
            }
        }
    }
}

fn repair_action(path: &Path) -> Option<TemplateAction> {
    match user_config_fs::inspect_editable_file(path, "source template") {
        Ok(EditableFileState::Existing) => Some(TemplateAction::OpenToRepair),
        Ok(EditableFileState::Missing) | Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io};

    use super::*;
    use crate::tui::terminal::Modifiers;

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            kind,
            modifiers: Modifiers::default(),
        }
    }

    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(target, link);
        symlink_created_or_unsupported(result)
    }

    fn create_directory_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(target, link);
        symlink_created_or_unsupported(result)
    }

    fn symlink_created_or_unsupported(result: io::Result<()>) -> bool {
        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("failed to create test symlink: {error}"),
        }
    }

    #[test]
    fn selection_uses_language_all_indices_and_wraps_for_press_and_repeat() {
        let temp = tempfile::tempdir().unwrap();
        let mut modal =
            OpenTemplateModal::new(Ok(temp.path().to_path_buf()), Language::Cpp, Language::Cpp);
        modal.set_error("old error".to_string());

        for (code, kind, expected) in [
            (KeyCode::Up, KeyEventKind::Press, Language::Python),
            (KeyCode::Down, KeyEventKind::Repeat, Language::Cpp),
            (KeyCode::Char('j'), KeyEventKind::Press, Language::Python),
            (KeyCode::Char('k'), KeyEventKind::Repeat, Language::Cpp),
        ] {
            assert_eq!(
                modal.handle_key(key(code, kind)),
                TemplateModalTransition::Handled
            );
            assert_eq!(modal.selected_language(), expected);
            assert!(modal.error.is_none());
        }
    }

    #[test]
    fn rows_use_actual_filenames_statuses_and_config_default() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("templates")).unwrap();
        fs::write(temp.path().join("templates/cpp.cpp"), "// ready\n").unwrap();
        let modal = OpenTemplateModal::new(
            Ok(temp.path().join("templates")),
            Language::Cpp,
            Language::Python,
        );

        assert_eq!(
            modal.rows(),
            vec![
                TemplateRow {
                    language: Language::Cpp,
                    filename: "cpp.cpp",
                    status: TemplateStatus::Ready,
                    is_default: false,
                },
                TemplateRow {
                    language: Language::Python,
                    filename: "python.py",
                    status: TemplateStatus::Missing,
                    is_default: true,
                },
            ]
        );
    }

    #[test]
    fn invalid_utf8_is_repairable_but_unsafe_paths_are_not() {
        let temp = tempfile::tempdir().unwrap();
        let templates = temp.path().join("templates");
        fs::create_dir(&templates).unwrap();
        let cpp = template::source_template_path(&templates, Language::Cpp);
        fs::write(&cpp, [0xff, 0xfe]).unwrap();
        let mut modal = OpenTemplateModal::new(Ok(templates.clone()), Language::Cpp, Language::Cpp);
        assert_eq!(
            modal.selected_inspection().action,
            Some(TemplateAction::OpenToRepair)
        );

        fs::remove_file(&cpp).unwrap();
        fs::create_dir(&cpp).unwrap();
        assert_eq!(modal.selected_inspection().status, TemplateStatus::Invalid);
        assert_eq!(modal.selected_inspection().action, None);
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter, KeyEventKind::Press)),
            TemplateModalTransition::Handled
        );
    }

    #[test]
    fn selected_path_updates_and_missing_enter_and_i_are_equivalent() {
        let temp = tempfile::tempdir().unwrap();
        let templates = temp.path().join("templates");
        let mut modal =
            OpenTemplateModal::new(Ok(templates.clone()), Language::Cpp, Language::Python);
        assert_eq!(modal.selected_path().unwrap(), templates.join("cpp.cpp"));
        let TemplateModalTransition::Activate(enter) =
            modal.handle_key(key(KeyCode::Enter, KeyEventKind::Press))
        else {
            panic!("missing template must activate with Enter")
        };
        assert_eq!(enter.action, TemplateAction::InitializeAndOpen);

        modal.handle_key(key(KeyCode::Down, KeyEventKind::Press));
        assert_eq!(modal.selected_path().unwrap(), templates.join("python.py"));
        let TemplateModalTransition::Activate(alias) =
            modal.handle_key(key(KeyCode::Char('i'), KeyEventKind::Press))
        else {
            panic!("i must remain a missing-template compatibility alias")
        };
        assert_eq!(alias.action, TemplateAction::InitializeAndOpen);
    }

    #[test]
    fn symlink_status_and_action_follow_the_template_and_editable_file_contracts() {
        let temp = tempfile::tempdir().unwrap();
        let templates = temp.path().join("templates");
        fs::create_dir(&templates).unwrap();
        let selected = templates.join("cpp.cpp");
        let regular = temp.path().join("regular.cpp");
        fs::write(&regular, "// linked\n").unwrap();
        if !create_file_symlink(&regular, &selected) {
            return;
        }
        let modal = OpenTemplateModal::new(Ok(templates.clone()), Language::Cpp, Language::Cpp);
        assert_eq!(modal.selected_inspection().status, TemplateStatus::Ready);
        assert_eq!(
            modal.selected_inspection().action,
            Some(TemplateAction::Open)
        );

        fs::remove_file(&selected).unwrap();
        let directory = temp.path().join("directory");
        fs::create_dir(&directory).unwrap();
        if !create_directory_symlink(&directory, &selected) {
            return;
        }
        assert_eq!(modal.selected_inspection().status, TemplateStatus::Invalid);
        assert_eq!(modal.selected_inspection().action, None);

        #[cfg(unix)]
        fs::remove_file(&selected).unwrap();
        #[cfg(windows)]
        fs::remove_dir(&selected).unwrap();
        if !create_file_symlink(&temp.path().join("missing.cpp"), &selected) {
            return;
        }
        assert_eq!(modal.selected_inspection().status, TemplateStatus::Invalid);
        assert_eq!(modal.selected_inspection().action, None);
    }
}
