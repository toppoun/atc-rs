//! Handle-relative, atomic no-clobber publish for a completed staging file.
//!
//! Contract A: a distinct existing object is never replaced. Windows may consolidate
//! source/destination aliases of the same object; that is also a successful publish.

use super::{CapDir, CapOpenOptions, METADATA_FILE, io_context, parse_input_file_name};
use cap_std::fs::OpenOptionsExt as _;
use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::mem::{align_of, offset_of, size_of};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::ptr::NonNull;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    GENERIC_WRITE, HANDLE, NTSTATUS, RtlNtStatusToDosError, STATUS_PENDING, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

pub(super) fn configure_source(options: &mut CapOpenOptions) {
    // No OVERLAPPED or DELETE_ON_CLOSE. CapDir creates a synchronous, create-new,
    // no-follow source; keep this handle from write/sync through publish.
    options
        .access_mode(GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
}

struct RenameBuffer {
    pointer: NonNull<u8>,
    layout: Layout,
    length: u32,
}

impl RenameBuffer {
    fn new(directory: HANDLE, destination: &OsStr) -> io::Result<Self> {
        // A lexical check only, never a destination existence check. Restrict the
        // native namespace to owned single-component names (no traversal/ADS/NUL).
        if destination != OsStr::new(METADATA_FILE) && parse_input_file_name(destination).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native publish destination must be canonical N.in or meta.toml",
            ));
        }
        let wide = destination.encode_wide().collect::<Vec<_>>();
        let invalid_size = || io::Error::from(io::ErrorKind::InvalidInput);
        let name_bytes = wide
            .len()
            .checked_mul(size_of::<u16>())
            .ok_or_else(invalid_size)?;
        let length = offset_of!(FILE_RENAME_INFORMATION, FileName)
            .checked_add(name_bytes)
            .and_then(|bytes| bytes.checked_add(size_of::<u16>()))
            .ok_or_else(invalid_size)?
            .max(
                size_of::<FILE_RENAME_INFORMATION>()
                    .checked_add(name_bytes)
                    .ok_or_else(invalid_size)?,
            );
        let byte_length = u32::try_from(length).map_err(|_| invalid_size())?;
        let name_length = u32::try_from(name_bytes).map_err(|_| invalid_size())?;
        let layout = Layout::from_size_align(length, align_of::<FILE_RENAME_INFORMATION>())
            .map_err(|_| invalid_size())?;
        // SAFETY: aligned for the native WDK structure, with checked capacity for
        // its complete variable UTF-16 tail plus a NUL outside FileNameLength.
        // Zeroing also initializes the BOOLEAN/Flags union and all padding.
        let pointer = NonNull::new(unsafe { alloc_zeroed(layout) })
            .unwrap_or_else(|| handle_alloc_error(layout));
        unsafe {
            let header = pointer.as_ptr().cast::<FILE_RENAME_INFORMATION>();
            (*header).Anonymous.ReplaceIfExists = false;
            (*header).RootDirectory = directory;
            (*header).FileNameLength = name_length;
            std::ptr::copy_nonoverlapping(
                wide.as_ptr(),
                pointer
                    .as_ptr()
                    .add(offset_of!(FILE_RENAME_INFORMATION, FileName))
                    .cast::<u16>(),
                wide.len(),
            );
        }
        Ok(Self {
            pointer,
            layout,
            length: byte_length,
        })
    }
}

impl Drop for RenameBuffer {
    fn drop(&mut self) {
        // SAFETY: the native operation is complete; use the original allocation layout.
        unsafe { dealloc(self.pointer.as_ptr(), self.layout) };
    }
}

fn status_error(status: NTSTATUS) -> io::Error {
    // GetLastError is not the result of an Nt call. Translate the returned NTSTATUS
    // for Rust's ErrorKind, while retaining both native and Win32 diagnostics.
    let error = io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32);
    io_context(
        error,
        format!(
            "NtSetInformationFile(FileRenameInformation), NTSTATUS 0x{:08X}",
            status as u32
        ),
    )
}

pub(super) fn publish(
    directory: &CapDir,
    source: &fs::File,
    destination: &OsStr,
) -> io::Result<()> {
    let buffer = RenameBuffer::new(directory.as_raw_handle(), destination)?;
    let mut completion = IO_STATUS_BLOCK::default();
    // SAFETY: native WDK ABI, live borrowed handles, owned aligned request and
    // writable completion buffer. RootDirectory is the actual CapDir handle;
    // FileName stays relative. No Win32 wrapper or ambient-path fallback.
    let mut status = unsafe {
        NtSetInformationFile(
            source.as_raw_handle(),
            &mut completion,
            buffer.pointer.as_ptr().cast(),
            buffer.length,
            FileRenameInformation,
        )
    };
    if status == STATUS_PENDING {
        // Set-information is documented synchronous, and our source is synchronous.
        // Defensively complete unexpected pending I/O without retrying the rename
        // or releasing buffers that the kernel might still access.
        let waited = unsafe { WaitForSingleObject(source.as_raw_handle(), INFINITE) };
        if waited != WAIT_OBJECT_0 {
            // A live SYNCHRONIZE-capable handle cannot fail this wait. Returning or
            // unwinding on a broken completion contract could free in-flight memory.
            std::process::abort();
        }
        status = unsafe { completion.Anonymous.Status };
        if status == STATUS_PENDING {
            std::process::abort();
        }
    }
    // The returned status is authoritative; on failure IOSB can remain untouched.
    if status < 0 {
        Err(status_error(status))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{
        STATUS_ACCESS_DENIED, STATUS_INVALID_PARAMETER, STATUS_OBJECT_NAME_COLLISION,
        STATUS_OBJECT_NAME_NOT_FOUND,
    };

    #[test]
    fn native_status_mapping_preserves_kind_and_diagnostics() {
        for (status, kind, code) in [
            (
                STATUS_OBJECT_NAME_COLLISION,
                io::ErrorKind::AlreadyExists,
                183,
            ),
            (STATUS_ACCESS_DENIED, io::ErrorKind::PermissionDenied, 5),
            (STATUS_OBJECT_NAME_NOT_FOUND, io::ErrorKind::NotFound, 2),
            (STATUS_INVALID_PARAMETER, io::ErrorKind::InvalidInput, 87),
        ] {
            let error = status_error(status);
            assert_eq!(error.kind(), kind);
            assert!(
                error
                    .to_string()
                    .contains(&format!("0x{:08X}", status as u32))
            );
            assert!(error.to_string().contains(&format!("os error {code}")));
        }
    }

    #[test]
    fn native_buffer_layout_and_destination_boundary() {
        let pointer = size_of::<HANDLE>();
        assert_eq!(align_of::<FILE_RENAME_INFORMATION>(), pointer);
        assert_eq!(offset_of!(FILE_RENAME_INFORMATION, RootDirectory), pointer);
        assert_eq!(
            offset_of!(FILE_RENAME_INFORMATION, FileNameLength),
            2 * pointer
        );
        assert_eq!(
            offset_of!(FILE_RENAME_INFORMATION, FileName),
            2 * pointer + 4
        );
        assert_eq!(
            size_of::<FILE_RENAME_INFORMATION>(),
            if pointer == 8 { 24 } else { 16 }
        );
        assert_eq!(align_of::<IO_STATUS_BLOCK>(), pointer);
        assert_eq!(size_of::<IO_STATUS_BLOCK>(), 2 * pointer);
        assert_eq!(offset_of!(IO_STATUS_BLOCK, Information), pointer);
        assert_eq!(FileRenameInformation, 10);
        for invalid in [
            "",
            ".",
            "..",
            "../x",
            "..\\x",
            "C:\\1.in",
            "\\1.in",
            "x:y",
            "a/1.in",
            "a\\1.in",
            "1.in\0",
            "0.in",
            "01.in",
            "+1.in",
            "META.TOML",
            "meta.toml.",
            "meta.toml ",
            "meta.toml:ads",
            "../meta.toml",
            "..\\meta.toml",
            "meta.toml\0",
        ] {
            assert!(
                matches!(RenameBuffer::new(std::ptr::null_mut(), OsStr::new(invalid)), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
            );
        }
        for name in ["1.in", "18446744073709551615.in", METADATA_FILE] {
            let buffer = RenameBuffer::new(std::ptr::null_mut(), OsStr::new(name)).unwrap();
            let bytes = name.encode_utf16().collect::<Vec<_>>();
            assert_eq!(
                buffer.pointer.as_ptr() as usize % align_of::<FILE_RENAME_INFORMATION>(),
                0
            );
            assert!(
                buffer.length as usize
                    >= offset_of!(FILE_RENAME_INFORMATION, FileName)
                        + (bytes.len() + 1) * size_of::<u16>()
            );
            // SAFETY: inspect initialized fields and the checked tail while owned buffer lives.
            unsafe {
                let header = &*buffer.pointer.as_ptr().cast::<FILE_RENAME_INFORMATION>();
                assert!(!header.Anonymous.ReplaceIfExists);
                assert!(header.RootDirectory.is_null());
                assert_eq!(header.FileNameLength as usize, bytes.len() * 2);
                assert_eq!(
                    std::slice::from_raw_parts(
                        buffer
                            .pointer
                            .as_ptr()
                            .add(offset_of!(FILE_RENAME_INFORMATION, FileName))
                            .cast::<u16>(),
                        bytes.len()
                    ),
                    bytes
                );
            }
        }
    }
}
