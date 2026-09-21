use super::{MetadataContract, open_nofollow};
use std::collections::BTreeMap;
use std::ffi::{CStr, c_void};
use std::fmt;
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::macos::fs::MetadataExt as MacMetadataExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

// Resource forks and other xattrs can be arbitrarily large. Refuse structured
// writes when their complete metadata cannot be inspected within this budget.
const METADATA_BUDGET: usize = 16 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ExtendedMetadata {
    acl: Vec<u8>,
    xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl fmt::Debug for ExtendedMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Attribute names and values are opaque user data, never diagnostics.
        formatter.write_str("ExtendedMetadata { .. }")
    }
}

pub(super) fn metadata_contract(before: &Metadata, file: &File) -> io::Result<MetadataContract> {
    let extended = read_extended(file)?;
    let confirmation = read_extended(file)?;
    let after = file.metadata()?;
    if extended != confirmation || !same_stat(before, &after) {
        return Err(changed_during_read());
    }
    Ok(MetadataContract {
        mode: after.mode(),
        extended,
    })
}

fn same_stat(before: &Metadata, after: &Metadata) -> bool {
    // ctime brackets the entire snapshot, including the caller's content reads.
    // atime is excluded because reading the file may itself update it. This is
    // a stability check, not an atomic CAS against uncooperative external writers.
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.mode() == after.mode()
        && before.uid() == after.uid()
        && before.gid() == after.gid()
        && before.st_flags() == after.st_flags()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn changed_during_read() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "file metadata changed while it was being read",
    )
}

fn read_extended(file: &File) -> io::Result<ExtendedMetadata> {
    let acl = read_acl(file)?;
    let mut budget = METADATA_BUDGET;
    let names = read_sized(&mut budget, |buffer, size| unsafe {
        libc::flistxattr(
            file.as_raw_fd(),
            buffer.cast(),
            size,
            libc::XATTR_SHOWCOMPRESSION,
        )
    })?;
    let mut xattrs = BTreeMap::new();
    for name in names.split_inclusive(|byte| *byte == 0) {
        let name = CStr::from_bytes_with_nul(name)
            .ok()
            .filter(|name| !name.to_bytes().is_empty())
            .ok_or_else(|| io::Error::other("invalid extended attribute name list"))?;
        let value = read_sized(&mut budget, |buffer, size| unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                buffer,
                size,
                0,
                libc::XATTR_SHOWCOMPRESSION,
            )
        })?;
        if xattrs.insert(name.to_bytes().to_vec(), value).is_some() {
            return Err(changed_during_read());
        }
    }
    Ok(ExtendedMetadata { acl, xattrs })
}

fn read_sized(
    budget: &mut usize,
    mut read: impl FnMut(*mut c_void, usize) -> libc::ssize_t,
) -> io::Result<Vec<u8>> {
    let size = read(std::ptr::null_mut(), 0);
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    let size = size as usize;
    *budget = budget.checked_sub(size).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "file metadata exceeds the safe inspection limit",
        )
    })?;
    // Even an empty value/list needs a nonzero buffer for the second call;
    // passing size=0 again would only measure a value that may have grown.
    let mut bytes = vec![0; size.max(1)];
    let actual = read(bytes.as_mut_ptr().cast(), bytes.len());
    if actual < 0 {
        return Err(io::Error::last_os_error());
    }
    if actual as usize != size {
        return Err(changed_during_read());
    }
    bytes.truncate(size);
    Ok(bytes)
}

// Darwin's ACL functions are not exposed by libc. These declarations match
// <sys/acl.h>; acl_t is an opaque pointer and ACL_TYPE_EXTENDED is 0x100.
unsafe extern "C" {
    fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut c_void;
    fn acl_size(acl: *mut c_void) -> libc::ssize_t;
    fn acl_copy_ext(buffer: *mut c_void, acl: *mut c_void, size: libc::ssize_t) -> libc::ssize_t;
    fn acl_free(object: *mut c_void) -> libc::c_int;
}

struct Acl(*mut c_void);

impl Drop for Acl {
    fn drop(&mut self) {
        // SAFETY: this owns a successful acl_get_fd_np allocation.
        unsafe { acl_free(self.0) };
    }
}

fn read_acl(file: &File) -> io::Result<Vec<u8>> {
    // SAFETY: the descriptor stays open throughout the call.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // Darwin's filesec_get_property(FILESEC_ACL) reports ENOENT when
        // this open file has no ACL. Keep absence distinct from an empty ACL
        // representation. Permission/unsupported/I/O errors must still fail.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(Vec::new())
        } else {
            Err(error)
        };
    }
    let acl = Acl(acl);
    // acl_copy_ext serializes flags, ordered entries and UUID qualifiers without
    // name-service lookups or process-local pointers, zeroing representation padding.
    let size = unsafe { acl_size(acl.0) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    if size as usize > METADATA_BUDGET {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ACL exceeds the safe inspection limit",
        ));
    }
    // Darwin writes a kauth_filesec (u32 fields and byte-array UUIDs), so
    // provide u32 alignment instead of relying on the allocator for Vec<u8>.
    let mut words = vec![0_u32; (size as usize).div_ceil(size_of::<u32>())];
    let copied = unsafe { acl_copy_ext(words.as_mut_ptr().cast(), acl.0, size) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    if copied != size {
        return Err(changed_during_read());
    }
    Ok(words
        .into_iter()
        .flat_map(u32::to_ne_bytes)
        .take(size as usize)
        .collect())
}

pub(super) fn preserve_metadata(source: &Path, destination: &File) -> io::Result<()> {
    let source = open_nofollow(source)?;
    if !source.metadata()?.is_file() {
        return Err(io::Error::other("metadata source must be a regular file"));
    }
    // Use the no-follow source descriptor and the already-owned staging file.
    // No pathname is reopened by copyfile, and source data is never copied.
    let result = unsafe {
        libc::fcopyfile(
            source.as_raw_fd(),
            destination.as_raw_fd(),
            std::ptr::null_mut(),
            libc::COPYFILE_METADATA,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xattr_size_changes_and_removal_between_measurement_and_read_are_rejected() {
        for (initial, changed) in [
            (b"".as_slice(), Some(b"x".as_slice())),
            (b"before".as_slice(), Some(b"longer value".as_slice())),
            (b"before".as_slice(), Some(b"short".as_slice())),
            (b"before".as_slice(), None),
        ] {
            let file = tempfile::tempfile().unwrap();
            let fd = file.as_raw_fd();
            let name = c"com.atc-rs.snapshot-race";
            assert_eq!(
                unsafe {
                    libc::fsetxattr(
                        fd,
                        name.as_ptr(),
                        initial.as_ptr().cast(),
                        initial.len(),
                        0,
                        0,
                    )
                },
                0
            );
            let mut budget = METADATA_BUDGET;
            let result = read_sized(&mut budget, |buffer, size| {
                let result = unsafe { libc::fgetxattr(fd, name.as_ptr(), buffer, size, 0, 0) };
                if buffer.is_null() {
                    let changed = unsafe {
                        match changed {
                            Some(value) => libc::fsetxattr(
                                fd,
                                name.as_ptr(),
                                value.as_ptr().cast(),
                                value.len(),
                                0,
                                0,
                            ),
                            None => libc::fremovexattr(fd, name.as_ptr(), 0),
                        }
                    };
                    assert_eq!(changed, 0);
                }
                result
            });
            assert!(result.is_err());
        }
    }
}
