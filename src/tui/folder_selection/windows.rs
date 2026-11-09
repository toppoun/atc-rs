use std::ffi::OsString;
use std::io;
use std::marker::PhantomData;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::thread;

use windows::Win32::Foundation::{ERROR_CANCELLED, HANDLE, HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
};
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_DONTADDTORECENT, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR,
    FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindowVisible, WSF_VISIBLE};
use windows::core::{HRESULT, PCWSTR, PWSTR};

use super::{FolderDialogResult, initial_directory_hint, show_if_gui_available};

pub(super) fn pick_folder(initial_dir: Option<&Path>) -> io::Result<FolderDialogResult> {
    show_if_gui_available(interactive_window_station()?, || {
        with_sta(|| show_dialog(initial_dir))
    })
}

fn interactive_window_station() -> io::Result<bool> {
    let mut flags = USEROBJECTFLAGS::default();
    // SAFETY: the station handle is borrowed from the process. The output buffer is a
    // writable USEROBJECTFLAGS of the size required by UOI_FLAGS.
    unsafe {
        let station = GetProcessWindowStation()
            .map_err(|error| windows_error("get process window station", error))?;
        GetUserObjectInformationW(
            HANDLE(station.0),
            UOI_FLAGS,
            Some(std::ptr::from_mut(&mut flags).cast()),
            size_of::<USEROBJECTFLAGS>() as u32,
            None,
        )
        .map_err(|error| windows_error("query window station flags", error))?;
    }
    Ok(flags.dwFlags & WSF_VISIBLE as u32 != 0)
}

struct ComApartment(PhantomData<*mut ()>);

impl ComApartment {
    fn initialize(model: COINIT) -> windows::core::Result<Self> {
        // SAFETY: initializes COM on the current thread. Both S_OK and S_FALSE take a reference.
        unsafe { CoInitializeEx(None, model | COINIT_DISABLE_OLE1DDE) }.ok()?;
        Ok(Self(PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: the guard stays on the thread that initialized COM.
        unsafe { CoUninitialize() };
    }
}

fn with_sta<T: Send>(operation: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
    match ComApartment::initialize(COINIT_APARTMENTTHREADED) {
        Ok(_sta) => operation(),
        Err(error) if error.code() == RPC_E_CHANGED_MODE => thread::scope(|scope| {
            let worker = thread::Builder::new()
                .name("folder-picker-sta".into())
                .spawn_scoped(scope, || {
                    let _sta = ComApartment::initialize(COINIT_APARTMENTTHREADED)
                        .map_err(|error| windows_error("initialize folder picker STA", error))?;
                    operation()
                })
                .map_err(|error| io::Error::other(format!("start folder picker STA: {error}")))?;
            worker
                .join()
                .map_err(|_| io::Error::other("folder picker STA thread panicked"))?
        }),
        Err(error) => Err(windows_error("initialize COM for folder picker", error)),
    }
}

fn show_dialog(initial_dir: Option<&Path>) -> io::Result<FolderDialogResult> {
    // SAFETY: with_sta keeps COM initialized on this thread until the dialog, shell items,
    // and task-allocated path have been dropped. Show runs its own modal message loop.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| windows_error("create Common Item Dialog", error))?;
        let options = dialog
            .GetOptions()
            .map_err(|error| windows_error("get folder dialog options", error))?;
        dialog
            .SetOptions(
                (options & !FOS_ALLOWMULTISELECT)
                    | FOS_PICKFOLDERS
                    | FOS_FORCEFILESYSTEM
                    | FOS_NOCHANGEDIR
                    | FOS_DONTADDTORECENT,
            )
            .map_err(|error| windows_error("set folder dialog options", error))?;
        if let Some(path) = initial_directory_hint(initial_dir) {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            if let Ok(folder) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None)
            {
                let _ = dialog.SetDefaultFolder(&folder);
            }
        }

        if !show_completed(dialog.Show(console_owner()))? {
            return Ok(FolderDialogResult::Cancelled);
        }
        let item = dialog
            .GetResult()
            .map_err(|error| windows_error("get selected folder", error))?;
        let path = TaskPath(
            item.GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|error| windows_error("get selected folder file system path", error))?,
        );
        if path.0.is_null() {
            return Err(io::Error::other("folder dialog returned a null path"));
        }
        Ok(FolderDialogResult::Selected(path_from_wide(
            path.0.as_wide(),
        )?))
    }
}

fn console_owner() -> Option<HWND> {
    // SAFETY: these APIs query window state and do not acquire ownership of the handles.
    unsafe {
        let console = GetConsoleWindow();
        // ConPTY's console HWND is not the visible terminal. Do not parent to that HWND
        // or to an unrelated foreground window, and let Windows manage activation.
        (!console.is_invalid()
            && console == GetForegroundWindow()
            && IsWindowVisible(console).as_bool())
        .then_some(console)
    }
}

fn show_completed(result: windows::core::Result<()>) -> io::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => Ok(false),
        Err(error) => Err(windows_error("show folder dialog", error)),
    }
}

struct TaskPath(PWSTR);

impl Drop for TaskPath {
    fn drop(&mut self) {
        // SAFETY: GetDisplayName allocates this string with the COM task allocator.
        // CoTaskMemFree also accepts a null pointer.
        unsafe { CoTaskMemFree(Some(self.0.0.cast())) };
    }
}

fn path_from_wide(wide: &[u16]) -> io::Result<PathBuf> {
    let path = PathBuf::from(OsString::from_wide(wide));
    if !path.is_absolute() {
        return Err(io::Error::other(
            "folder dialog returned a non-absolute path",
        ));
    }
    Ok(path)
}

fn windows_error(action: &str, error: windows::core::Error) -> io::Error {
    io::Error::other(format!("{action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::E_ACCESSDENIED;
    use windows::Win32::System::Com::{
        APTTYPE, APTTYPE_MAINSTA, APTTYPE_MTA, APTTYPE_STA, APTTYPEQUALIFIER, COINIT_MULTITHREADED,
        CoGetApartmentType,
    };

    fn apartment_type() -> windows::core::Result<APTTYPE> {
        let mut apartment = APTTYPE::default();
        let mut qualifier = APTTYPEQUALIFIER::default();
        // SAFETY: both pointers refer to writable output values.
        unsafe { CoGetApartmentType(&mut apartment, &mut qualifier)? };
        Ok(apartment)
    }

    #[test]
    fn sta_reuses_the_calling_thread_and_balances_nested_initialization() {
        thread::spawn(|| {
            let caller = thread::current().id();
            with_sta(|| {
                let apartment = apartment_type().unwrap();
                assert!(apartment == APTTYPE_STA || apartment == APTTYPE_MAINSTA);
                with_sta(|| {
                    assert_eq!(thread::current().id(), caller);
                    Ok(())
                })?;
                assert_eq!(apartment_type().unwrap(), apartment);
                Ok(())
            })
            .unwrap();
            // A concurrent COM user may give this thread an implicit MTA. Reinitializing
            // explicitly as MTA still proves no STA initialization was left outstanding.
            let _mta = ComApartment::initialize(COINIT_MULTITHREADED).unwrap();
            assert_eq!(apartment_type().unwrap(), APTTYPE_MTA);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn mta_caller_runs_on_another_sta_and_receives_its_error() {
        thread::spawn(|| {
            let mta = ComApartment::initialize(COINIT_MULTITHREADED).unwrap();
            let caller = thread::current().id();
            let error = with_sta(|| {
                assert_ne!(thread::current().id(), caller);
                let apartment = apartment_type().unwrap();
                assert!(apartment == APTTYPE_STA || apartment == APTTYPE_MAINSTA);
                Err::<(), _>(io::Error::other("worker error"))
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "worker error");
            assert_eq!(apartment_type().unwrap(), APTTYPE_MTA);
            drop(mta);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn only_explicit_show_cancellation_is_cancelled() {
        assert!(show_completed(Ok(())).unwrap());
        assert!(!show_completed(Err(HRESULT::from_win32(ERROR_CANCELLED.0).into())).unwrap());
        assert!(show_completed(Err(E_ACCESSDENIED.into())).is_err());
    }

    #[test]
    fn file_system_path_keeps_native_utf16_and_namespace_prefixes() {
        for path in [r"\\?\C:\日本語 with spaces 🦀", r"\\server\share\directory"] {
            let mut wide: Vec<u16> = path.encode_utf16().collect();
            wide.push(0xd800); // Windows OsString can preserve an unpaired surrogate.
            let selected = path_from_wide(&wide).unwrap();
            assert_eq!(selected.as_os_str().encode_wide().collect::<Vec<_>>(), wide);
        }
        assert!(path_from_wide(&[]).is_err());
    }
}
