use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct InstallContext {
    action: &'static str,
    destination: PathBuf,
    source: io::Error,
}

#[derive(Debug)]
struct RenameContext {
    source_path: PathBuf,
    destination: PathBuf,
    source: io::Error,
}

impl std::fmt::Display for RenameContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "failed to move {} to {} without replacing the destination: {}",
            self.source_path.display(),
            self.destination.display(),
            self.source
        )
    }
}

impl std::error::Error for RenameContext {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl std::fmt::Display for InstallContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "failed to {} {}: {}",
            self.action,
            self.destination.display(),
            self.source
        )
    }
}

impl std::error::Error for InstallContext {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn install_error(destination: &Path, action: &'static str, source: io::Error) -> io::Error {
    let kind = source.kind();
    io::Error::new(
        kind,
        InstallContext {
            action,
            destination: destination.to_path_buf(),
            source,
        },
    )
}

pub(crate) fn install_noclobber(
    destination: &Path,
    contents: &[u8],
    temporary_prefix: &str,
) -> io::Result<()> {
    install_noclobber_with_hook(destination, contents, temporary_prefix, || {})
}

/// Atomically moves `source` to `destination` only when the destination name is
/// still unused. Unlike `std::fs::rename`, this never replaces an existing file,
/// directory, or symlink.
pub(crate) fn rename_noclobber(source: &Path, destination: &Path) -> io::Result<()> {
    rename_noclobber_platform(source, destination).map_err(|error| {
        let kind = if fs::symlink_metadata(destination).is_ok() {
            io::ErrorKind::AlreadyExists
        } else {
            error.kind()
        };
        io::Error::new(
            kind,
            RenameContext {
                source_path: source.to_path_buf(),
                destination: destination.to_path_buf(),
                source: error,
            },
        )
    })
}

#[cfg(windows)]
fn rename_noclobber_platform(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    fn resolved_parent_path(path: &Path) -> io::Result<PathBuf> {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("move path has no final component: {}", path.display()),
            )
        })?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Ok(fs::canonicalize(parent)?.join(name))
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("move path contains a NUL character: {}", path.display()),
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    let source = wide_path(&resolved_parent_path(source)?)?;
    let destination = wide_path(&resolved_parent_path(destination)?)?;

    // A zero flag set deliberately omits MOVEFILE_REPLACE_EXISTING.
    let moved = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_noclobber_platform(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source path contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination path contains a NUL byte",
        )
    })?;

    let moved = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if moved == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
fn rename_noclobber_platform(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source path contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination path contains a NUL byte",
        )
    })?;

    let moved =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if moved == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))
))]
fn rename_noclobber_platform(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-clobber rename is not implemented on this platform",
    ))
}

#[cfg(not(any(windows, unix)))]
fn rename_noclobber_platform(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-clobber rename is not implemented on this platform",
    ))
}

fn install_noclobber_with_hook(
    destination: &Path,
    contents: &[u8],
    temporary_prefix: &str,
    before_persist: impl FnOnce(),
) -> io::Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "file destination has no parent directory: {}",
                destination.display()
            ),
        )
    })?;

    let mut staging = tempfile::Builder::new()
        .prefix(temporary_prefix)
        .tempfile_in(parent)
        .map_err(|error| install_error(destination, "stage", error))?;

    staging
        .write_all(contents)
        .map_err(|error| install_error(destination, "write", error))?;
    staging
        .as_file_mut()
        .sync_all()
        .map_err(|error| install_error(destination, "flush", error))?;

    before_persist();

    match staging.persist_noclobber(destination) {
        Ok(_) => Ok(()),
        Err(error) => {
            let tempfile::PersistError { error, file } = error;
            drop(file);
            Err(install_error(destination, "create", error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;

    const PREFIX: &str = ".atc-safe-create-";

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(error) if matches!(error.raw_os_error(), Some(5) | Some(50) | Some(1314)) => false,
            Err(error) => panic!("failed to create test symlink: {error}"),
        }
    }

    #[test]
    fn installs_complete_contents_and_leaves_no_staging_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("target.txt");
        let contents = b"complete contents\n";

        install_noclobber(&destination, contents, PREFIX).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), contents);
        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [OsStr::new("target.txt")]);
    }

    #[test]
    fn deterministic_competitor_is_not_clobbered_and_owned_staging_is_cleaned() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("target.txt");
        let user_artifact = temp.path().join(".atc-safe-create-user-data");
        fs::write(&user_artifact, b"keep this").unwrap();

        let error = install_noclobber_with_hook(&destination, b"new contents\n", PREFIX, || {
            fs::write(&destination, b"competitor contents\n").unwrap()
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&destination).unwrap(), b"competitor contents\n");
        assert_eq!(fs::read(&user_artifact).unwrap(), b"keep this");

        let mut entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            [
                OsStr::new(".atc-safe-create-user-data"),
                OsStr::new("target.txt"),
            ]
        );
    }

    #[test]
    fn failure_retains_the_operation_destination_and_source() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("missing-parent").join("target.txt");

        let error = install_noclobber(&destination, b"contents\n", PREFIX).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("failed to stage"));
        assert!(
            error
                .to_string()
                .contains(&destination.display().to_string())
        );
        let context = error
            .get_ref()
            .expect("safe-file error should retain context");
        assert!(
            context.source().is_some(),
            "the underlying filesystem error should remain in the source chain"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn rename_noclobber_moves_a_directory_to_an_unused_name() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("contents.txt"), "complete\n").unwrap();

        rename_noclobber(&source, &destination).unwrap();

        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(destination.join("contents.txt")).unwrap(),
            "complete\n"
        );
    }

    #[test]
    fn rename_noclobber_moves_a_file_to_an_unused_name() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("destination.txt");
        fs::write(&source, "complete\n").unwrap();

        rename_noclobber(&source, &destination).unwrap();

        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "complete\n");
    }

    #[test]
    fn rename_noclobber_preserves_every_existing_file_or_directory_destination() {
        for source_is_directory in [false, true] {
            for destination_is_directory in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let source = temp.path().join("source");
                let destination = temp.path().join("destination");
                if source_is_directory {
                    fs::create_dir(&source).unwrap();
                    fs::write(source.join("ours.txt"), "ours\n").unwrap();
                } else {
                    fs::write(&source, "ours\n").unwrap();
                }
                if destination_is_directory {
                    fs::create_dir(&destination).unwrap();
                    fs::write(destination.join("competitor.txt"), "competitor\n").unwrap();
                } else {
                    fs::write(&destination, "competitor\n").unwrap();
                }

                let error = rename_noclobber(&source, &destination).unwrap_err();

                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                if source_is_directory {
                    assert_eq!(
                        fs::read_to_string(source.join("ours.txt")).unwrap(),
                        "ours\n"
                    );
                } else {
                    assert_eq!(fs::read_to_string(&source).unwrap(), "ours\n");
                }
                if destination_is_directory {
                    assert_eq!(
                        fs::read_to_string(destination.join("competitor.txt")).unwrap(),
                        "competitor\n"
                    );
                } else {
                    assert_eq!(fs::read_to_string(&destination).unwrap(), "competitor\n");
                }
            }
        }
    }

    #[test]
    fn rename_noclobber_handles_spaces_and_unicode() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("源 source");
        let destination = temp.path().join("宛先 destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("内容.txt"), "complete\n").unwrap();

        rename_noclobber(&source, &destination).unwrap();

        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(destination.join("内容.txt")).unwrap(),
            "complete\n"
        );
    }

    #[test]
    fn rename_noclobber_preserves_an_existing_symlink_destination() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        fs::write(external.path(), "competitor\n").unwrap();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("destination.txt");
        fs::write(&source, "ours\n").unwrap();
        if !create_file_symlink(external.path(), &destination) {
            return;
        }

        let error = rename_noclobber(&source, &destination).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&source).unwrap(), "ours\n");
        assert_eq!(fs::read_to_string(external.path()).unwrap(), "competitor\n");
        assert!(
            fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(windows)]
    #[test]
    fn rename_noclobber_preserves_an_existing_junction_destination() {
        use std::os::windows::fs::MetadataExt as _;
        use std::process::Command;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let external = temp.path().join("external");
        let destination = temp.path().join("destination-junction");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("ours.txt"), "ours\n").unwrap();
        fs::create_dir(&external).unwrap();
        fs::write(external.join("competitor.txt"), "competitor\n").unwrap();
        let output = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&destination)
            .arg(&external)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to create test junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let error = rename_noclobber(&source, &destination).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(source.join("ours.txt")).unwrap(),
            "ours\n"
        );
        assert_eq!(
            fs::read_to_string(external.join("competitor.txt")).unwrap(),
            "competitor\n"
        );
        assert_ne!(
            fs::symlink_metadata(&destination)
                .unwrap()
                .file_attributes()
                & FILE_ATTRIBUTE_REPARSE_POINT,
            0
        );
        fs::remove_dir(&destination).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn rename_noclobber_rejects_interior_nul_without_truncating_the_path() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::write(&source, "ours\n").unwrap();

        let invalid_source = temp.path().join(OsString::from_wide(&[
            b's' as u16,
            b'o' as u16,
            b'u' as u16,
            b'r' as u16,
            b'c' as u16,
            b'e' as u16,
            0,
            b'x' as u16,
        ]));
        let error = rename_noclobber(&invalid_source, &destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_to_string(&source).unwrap(), "ours\n");
        assert!(!destination.exists());

        let invalid_destination = temp.path().join(OsString::from_wide(&[
            b'd' as u16,
            b'e' as u16,
            b's' as u16,
            b't' as u16,
            0,
            b'x' as u16,
        ]));
        let error = rename_noclobber(&source, &invalid_destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_to_string(&source).unwrap(), "ours\n");
        assert!(!temp.path().join("dest").exists());
    }
}
