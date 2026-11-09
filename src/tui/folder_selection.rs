//! Shared folder selection flow for future Global Home and Setup actions.
#![allow(dead_code)] // Screen actions are wired in later phases.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::{fmt, io};

use super::terminal::{TerminaSession, run_suspended_with};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(debug_assertions)]
pub(crate) mod probe;
#[cfg(windows)]
mod windows;

pub(crate) trait FolderPicker {
    /// Runs synchronously until the dialog closes. The initial directory is an optional hint.
    /// Return the native path as selected, with no Workspace or Registry operations.
    fn pick_folder(&mut self, initial_dir: Option<&Path>) -> FolderDialogResult;
}

pub(crate) struct NativeFolderPicker;

impl FolderPicker for NativeFolderPicker {
    fn pick_folder(&mut self, initial_dir: Option<&Path>) -> FolderDialogResult {
        #[cfg(windows)]
        let result = windows::pick_folder(initial_dir);
        #[cfg(target_os = "macos")]
        let result = macos::pick_folder(initial_dir);
        #[cfg(not(any(windows, target_os = "macos")))]
        let result: io::Result<FolderDialogResult> = {
            let _ = initial_dir;
            Ok(FolderDialogResult::Unavailable)
        };

        result.unwrap_or_else(|error| FolderDialogResult::Failed(FolderDialogFailure::new(error)))
    }
}

fn initial_directory_hint(path: Option<&Path>) -> Option<&Path> {
    path.filter(|path| path.is_absolute() && path.is_dir())
}

fn show_if_gui_available(
    available: bool,
    show: impl FnOnce() -> io::Result<FolderDialogResult>,
) -> io::Result<FolderDialogResult> {
    if available {
        show()
    } else {
        Ok(FolderDialogResult::Unavailable)
    }
}

#[derive(Debug)]
pub(crate) enum FolderDialogResult {
    Selected(PathBuf),
    Cancelled,
    Unavailable,
    Failed(FolderDialogFailure),
}

#[derive(Debug)]
pub(crate) struct FolderDialogFailure {
    cause: io::Error,
}

impl FolderDialogFailure {
    pub(crate) fn new(cause: io::Error) -> Self {
        Self { cause }
    }
}

impl fmt::Display for FolderDialogFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Could not open or complete the folder selection dialog.")
    }
}

impl std::error::Error for FolderDialogFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

pub(crate) enum FolderSelectionRequest<'a> {
    SystemDialog {
        picker: &'a mut dyn FolderPicker,
        initial_dir: Option<&'a Path>,
    },
    Explorer,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExplorerReason {
    Requested,
    DialogUnavailable,
}

#[derive(Debug)]
pub(crate) enum FolderSelection {
    /// An unverified candidate. Workspace operations must validate it independently.
    Selected(PathBuf),
    Cancelled,
    OpenExplorer(ExplorerReason),
    /// Show the display message and let the user choose whether to open Explorer.
    Failed(FolderDialogFailure),
}

/// For a system dialog request, end the current input batch before calling this function and
/// redraw the resulting screen before reading more input. A terminal error must propagate to
/// the TUI's existing cleanup path.
pub(crate) fn select_folder(
    terminal: &mut TerminaSession,
    request: FolderSelectionRequest<'_>,
) -> io::Result<FolderSelection> {
    select_folder_with(
        terminal,
        request,
        TerminaSession::suspend_for_folder_dialog,
        TerminaSession::resume_after_folder_dialog,
    )
}

fn select_folder_with<S>(
    terminal: &mut S,
    request: FolderSelectionRequest<'_>,
    suspend: impl FnOnce(&mut S) -> io::Result<()>,
    resume: impl FnOnce(&mut S) -> io::Result<()>,
) -> io::Result<FolderSelection> {
    let FolderSelectionRequest::SystemDialog {
        picker,
        initial_dir,
    } = request
    else {
        return Ok(FolderSelection::OpenExplorer(ExplorerReason::Requested));
    };

    // Dialog failures are recoverable outcomes only after the terminal has resumed successfully.
    let result = run_suspended_with(
        terminal,
        suspend,
        |_| Ok::<_, Infallible>(picker.pick_folder(initial_dir)),
        resume,
    )
    .map_err(|error| io::Error::other(error.to_string()))?;

    Ok(match result {
        FolderDialogResult::Selected(path) => FolderSelection::Selected(path),
        FolderDialogResult::Cancelled => FolderSelection::Cancelled,
        FolderDialogResult::Unavailable => {
            FolderSelection::OpenExplorer(ExplorerReason::DialogUnavailable)
        }
        FolderDialogResult::Failed(error) => FolderSelection::Failed(error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::error::Error as _;
    use std::fs;

    #[test]
    fn initial_directory_hint_keeps_valid_paths_and_ignores_unusable_paths() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("日本語 and spaces 🦀");
        let file = root.path().join("file.txt");
        fs::create_dir(&folder).unwrap();
        fs::write(&file, b"fixture").unwrap();

        assert_eq!(
            initial_directory_hint(Some(&folder)),
            Some(folder.as_path())
        );
        for path in [&file, &root.path().join("missing"), Path::new("relative")] {
            assert_eq!(initial_directory_hint(Some(path)), None);
        }
        assert_eq!(initial_directory_hint(None), None);
    }

    #[test]
    fn missing_gui_skips_the_native_dialog() {
        assert!(matches!(
            show_if_gui_available(false, || panic!("no GUI")),
            Ok(FolderDialogResult::Unavailable)
        ));
    }

    struct MockPicker<'a> {
        result: Option<FolderDialogResult>,
        initial_dir: Option<PathBuf>,
        steps: &'a RefCell<Vec<&'static str>>,
    }

    impl FolderPicker for MockPicker<'_> {
        fn pick_folder(&mut self, initial_dir: Option<&Path>) -> FolderDialogResult {
            self.steps.borrow_mut().push("pick");
            self.initial_dir = initial_dir.map(Path::to_path_buf);
            self.result.take().expect("picker must only run once")
        }
    }

    #[derive(Default)]
    struct Harness {
        steps: RefCell<Vec<&'static str>>,
        suspend_error: bool,
        resume_error: bool,
    }

    impl Harness {
        fn attempt(
            &self,
            initial_dir: Option<&Path>,
            result: FolderDialogResult,
        ) -> io::Result<FolderSelection> {
            let mut picker = MockPicker {
                result: Some(result),
                initial_dir: None,
                steps: &self.steps,
            };
            let selection = select_folder_with(
                &mut (),
                FolderSelectionRequest::SystemDialog {
                    picker: &mut picker,
                    initial_dir,
                },
                |()| {
                    self.steps.borrow_mut().push("suspend");
                    if self.suspend_error {
                        return Err(io::Error::other("suspend failure"));
                    }
                    Ok(())
                },
                |()| {
                    self.steps.borrow_mut().push("resume");
                    if self.resume_error {
                        return Err(io::Error::other("resume failure"));
                    }
                    Ok(())
                },
            );
            if !self.suspend_error {
                assert_eq!(
                    picker.initial_dir.as_deref().map(Path::as_os_str),
                    initial_dir.map(Path::as_os_str),
                );
            }
            selection
        }
    }

    #[test]
    fn selection_preserves_the_path_and_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("選択したフォルダ");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("existing.txt"), b"keep this file").unwrap();
        let selected_path = folder.join("..").join("選択したフォルダ");
        let harness = Harness::default();

        let selection = harness
            .attempt(
                Some(temp.path()),
                FolderDialogResult::Selected(selected_path.clone()),
            )
            .unwrap();

        let FolderSelection::Selected(selected) = selection else {
            panic!("expected the selected path, got {selection:?}");
        };
        assert_eq!(selected.as_os_str(), selected_path.as_os_str());
        assert_eq!(*harness.steps.borrow(), ["suspend", "pick", "resume"]);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 1);
        assert_eq!(
            fs::read(folder.join("existing.txt")).unwrap(),
            b"keep this file"
        );
    }

    #[test]
    fn cancellation_returns_to_the_caller_after_resume() {
        let harness = Harness::default();
        let selection = harness
            .attempt(None, FolderDialogResult::Cancelled)
            .unwrap();
        assert!(matches!(selection, FolderSelection::Cancelled));
        assert_eq!(*harness.steps.borrow(), ["suspend", "pick", "resume"]);
    }

    #[test]
    fn an_unavailable_dialog_routes_to_explorer_after_resume() {
        let harness = Harness::default();
        let selection = harness
            .attempt(None, FolderDialogResult::Unavailable)
            .unwrap();
        assert!(matches!(
            selection,
            FolderSelection::OpenExplorer(ExplorerReason::DialogUnavailable)
        ));
        assert_eq!(*harness.steps.borrow(), ["suspend", "pick", "resume"]);
    }

    #[test]
    fn explicit_explorer_needs_neither_a_picker_nor_terminal_suspension() {
        let selection = select_folder_with(
            &mut (),
            FolderSelectionRequest::Explorer,
            |()| panic!("Explorer must not suspend the terminal"),
            |()| panic!("Explorer must not restart the terminal"),
        )
        .unwrap();
        assert!(matches!(
            selection,
            FolderSelection::OpenExplorer(ExplorerReason::Requested)
        ));
    }

    #[test]
    fn launch_failure_offers_an_explicit_explorer_choice_with_safe_display_text() {
        let harness = Harness::default();
        let cause = "internal dialog launch error";
        let selection = harness
            .attempt(
                None,
                FolderDialogResult::Failed(FolderDialogFailure::new(io::Error::other(cause))),
            )
            .unwrap();
        let FolderSelection::Failed(error) = selection else {
            panic!("expected a recoverable dialog failure, got {selection:?}");
        };
        assert_eq!(
            error.to_string(),
            "Could not open or complete the folder selection dialog."
        );
        assert_eq!(error.source().unwrap().to_string(), cause);
        assert_eq!(*harness.steps.borrow(), ["suspend", "pick", "resume"]);
    }

    #[test]
    fn suspension_failure_prevents_dialog_and_attempts_recovery() {
        for resume_error in [false, true] {
            let harness = Harness {
                suspend_error: true,
                resume_error,
                ..Harness::default()
            };
            let error = harness
                .attempt(None, FolderDialogResult::Unavailable)
                .unwrap_err();
            let message = error.to_string();
            assert!(message.contains("suspend failure"));
            assert_eq!(message.contains("resume failure"), resume_error);
            assert_eq!(*harness.steps.borrow(), ["suspend", "resume"]);
        }
    }

    #[test]
    fn resumption_failure_prevents_selection_and_explorer_fallback() {
        for result in [
            FolderDialogResult::Selected(PathBuf::from("candidate")),
            FolderDialogResult::Cancelled,
            FolderDialogResult::Unavailable,
            FolderDialogResult::Failed(FolderDialogFailure::new(io::Error::other(
                "dialog failure",
            ))),
        ] {
            let harness = Harness {
                resume_error: true,
                ..Harness::default()
            };
            let error = harness.attempt(None, result).unwrap_err();
            assert_eq!(
                error.to_string(),
                "failed to restore the TUI terminal: resume failure"
            );
            assert_eq!(*harness.steps.borrow(), ["suspend", "pick", "resume"]);
        }
    }
}
