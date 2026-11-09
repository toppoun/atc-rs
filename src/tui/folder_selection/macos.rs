use std::io;
use std::path::{Path, PathBuf};

use objc2::MainThreadMarker;
use objc2::rc::autoreleasepool;
use objc2::runtime::NSObjectProtocol;
use objc2::sel;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSModalResponse, NSModalResponseCancel,
    NSModalResponseOK, NSOpenPanel,
};
use objc2_core_graphics::CGSessionCopyCurrentDictionary;
use objc2_foundation::NSURL;

use super::{FolderDialogResult, initial_directory_hint, show_if_gui_available};

pub(super) fn pick_folder(initial_dir: Option<&Path>) -> io::Result<FolderDialogResult> {
    // Check before initializing AppKit. The normal TUI runs synchronously on main and there
    // is no running AppKit event loop to service a dispatch from another thread.
    let main_thread = MainThreadMarker::new()
        .ok_or_else(|| io::Error::other("the macOS folder picker requires the main thread"))?;
    show_if_gui_available(CGSessionCopyCurrentDictionary().is_some(), || {
        autoreleasepool(|_| show_dialog(main_thread, initial_dir))
    })
}

fn show_dialog(
    main_thread: MainThreadMarker,
    initial_dir: Option<&Path>,
) -> io::Result<FolderDialogResult> {
    let app = NSApplication::sharedApplication(main_thread);
    let previous_policy = app.activationPolicy();
    let needs_accessory_policy = previous_policy == NSApplicationActivationPolicy::Prohibited;
    if needs_accessory_policy && !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory)
    {
        return Err(io::Error::other(
            "could not enable macOS folder picker windows",
        ));
    }

    let panel = NSOpenPanel::openPanel(main_thread);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setAllowsMultipleSelection(false);
    panel.setCanCreateDirectories(true);
    panel.setResolvesAliases(false);
    let initial_url = initial_directory_hint(initial_dir).and_then(NSURL::from_directory_path);
    panel.setDirectoryURL(initial_url.as_deref());

    request_activation(&app);
    let result = modal_result(panel.runModal(), || {
        let url = panel
            .URL()
            .ok_or_else(|| io::Error::other("folder panel returned no URL"))?;
        path_from_url(&url)
    });
    drop(panel);
    if needs_accessory_policy && !app.setActivationPolicy(previous_policy) {
        return Err(io::Error::other(
            "could not restore macOS application activation policy",
        ));
    }
    result
}

fn request_activation(app: &NSApplication) {
    // macOS 14 introduced cooperative activation. Check for the selector to support earlier OSes.
    if app.respondsToSelector(sel!(activate)) {
        app.activate();
    } else {
        // This request is initiated by the user's picker action.
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

fn modal_result(
    response: NSModalResponse,
    selected_path: impl FnOnce() -> io::Result<PathBuf>,
) -> io::Result<FolderDialogResult> {
    if response == NSModalResponseOK {
        selected_path().map(FolderDialogResult::Selected)
    } else if response == NSModalResponseCancel {
        Ok(FolderDialogResult::Cancelled)
    } else {
        Err(io::Error::other(format!(
            "unexpected folder panel response: {response}"
        )))
    }
}

fn path_from_url(url: &NSURL) -> io::Result<PathBuf> {
    if !url.isFileURL() {
        return Err(io::Error::other("folder panel returned a non-file URL"));
    }
    // objc2 uses getFileSystemRepresentation:maxLength: and copies its native bytes.
    let path = url.to_file_path().ok_or_else(|| {
        io::Error::other("folder panel URL has no usable file system representation")
    })?;
    if !path.is_absolute() {
        return Err(io::Error::other(
            "folder panel returned a non-absolute path",
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_thread_is_rejected_before_appkit_is_initialized() {
        let error = std::thread::spawn(|| pick_folder(None))
            .join()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("requires the main thread"));
    }

    #[test]
    fn modal_response_reads_the_selected_path_only_after_ok() {
        let path = PathBuf::from("/tmp/folder");
        assert!(matches!(
            modal_result(NSModalResponseOK, || Ok(path.clone())),
            Ok(FolderDialogResult::Selected(selected)) if selected == path
        ));
        assert!(matches!(
            modal_result(NSModalResponseCancel, || panic!(
                "cancel has no selected URL"
            )),
            Ok(FolderDialogResult::Cancelled)
        ));
        assert!(
            modal_result(objc2_app_kit::NSModalResponseAbort, || {
                panic!("aborted panel has no URL")
            })
            .is_err()
        );
    }

    #[test]
    fn file_url_round_trip_keeps_unicode_spaces_and_percent_signs() {
        autoreleasepool(|_| {
            let expected = Path::new("/tmp/日本語 with spaces 🦀 100%");
            let url = NSURL::from_directory_path(expected).unwrap();
            assert_eq!(path_from_url(&url).unwrap(), expected);
        });
    }
}
