use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// A staging directory whose implicit drop preserves its contents. Callers remove only
/// verified owned entries explicitly; error paths keep possible recovery files intact.
pub(crate) struct PreservedTempDir {
    directory: Option<tempfile::TempDir>,
}

impl PreservedTempDir {
    pub(crate) fn new(directory: tempfile::TempDir) -> Self {
        Self {
            directory: Some(directory),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        self.directory
            .as_ref()
            .expect("staging directory is present")
            .path()
    }

    pub(crate) fn keep(mut self) -> PathBuf {
        self.directory
            .take()
            .expect("staging directory is present")
            .keep()
    }

    pub(crate) fn into_temp_dir(mut self) -> tempfile::TempDir {
        self.directory.take().expect("staging directory is present")
    }
}

impl Drop for PreservedTempDir {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let _ = directory.keep();
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryIdentity {
    version: u32,
    kind: String,
    contest_id: String,
    problem_index: Option<String>,
}

impl RecoveryIdentity {
    pub(crate) fn new(kind: &str, contest_id: &str, problem_index: Option<&str>) -> Self {
        Self {
            version: 1,
            kind: kind.to_owned(),
            contest_id: contest_id.to_owned(),
            problem_index: problem_index.map(str::to_owned),
        }
    }

    pub(crate) fn is_contest(&self) -> bool {
        self.kind == "contest" && self.problem_index.is_none()
    }
}

pub(crate) const RECOVERY_INTENT: &str = "recovery-intent.toml";
pub(crate) const RECOVERY_COMPLETE: &str = "recovery-complete.toml";

pub(crate) fn write_recovery_intent(root: &Path, identity: &RecoveryIdentity) -> io::Result<()> {
    write_recovery_record(&root.join(RECOVERY_INTENT), identity)
}

/// The staging name remains an interruption signal until the same-volume no-clobber rename.
/// A completed name is never installed unless the completion record has been flushed.
pub(crate) fn complete_recovery(
    staging: PreservedTempDir,
    completed_path: &Path,
    identity: &RecoveryIdentity,
) -> io::Result<PathBuf> {
    let root = staging.keep();
    let result = write_recovery_record(&root.join(RECOVERY_COMPLETE), identity)
        .and_then(|()| rename_noclobber(&root, completed_path));
    result.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to complete recovery area: {error}; data kept at {}",
                root.display()
            ),
        )
    })?;
    Ok(completed_path.to_path_buf())
}

pub(crate) fn read_completed_recovery(path: &Path) -> io::Result<RecoveryIdentity> {
    let intent = read_regular_file_snapshot(&path.join(RECOVERY_INTENT))?.bytes;
    let complete = read_regular_file_snapshot(&path.join(RECOVERY_COMPLETE))?.bytes;
    parse_completed_recovery(&intent, &complete)
}

pub(crate) fn parse_completed_recovery(
    intent_bytes: &[u8],
    complete_bytes: &[u8],
) -> io::Result<RecoveryIdentity> {
    let intent = parse_recovery_record(intent_bytes)?;
    let complete = parse_recovery_record(complete_bytes)?;
    if intent != complete {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovery records disagree",
        ));
    }
    Ok(complete)
}

fn parse_recovery_record(bytes: &[u8]) -> io::Result<RecoveryIdentity> {
    let content = std::str::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid recovery record encoding",
        )
    })?;
    let identity: RecoveryIdentity = toml::from_str(content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if identity.version != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported recovery record version",
        ));
    }
    Ok(identity)
}

pub(crate) fn recovery_intent_exists(path: &Path) -> io::Result<bool> {
    Ok(read_optional_regular_file_snapshot(&path.join(RECOVERY_INTENT))?.is_some())
}

fn write_recovery_record(path: &Path, identity: &RecoveryIdentity) -> io::Result<()> {
    let content = toml::to_string(identity).map_err(io::Error::other)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileSnapshot {
    pub(crate) bytes: Vec<u8>,
    identity: FileIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TreeSnapshot {
    directories: std::collections::BTreeMap<PathBuf, FileIdentity>,
    files: std::collections::BTreeMap<PathBuf, FileSnapshot>,
}

/// Records the exact files and directory identities in a generation. Callers validate their
/// ownership rules first; this snapshot only detects changes across the generation move.
pub(crate) fn read_tree_snapshot(root: &Path) -> io::Result<TreeSnapshot> {
    let mut snapshot = TreeSnapshot {
        directories: std::collections::BTreeMap::new(),
        files: std::collections::BTreeMap::new(),
    };
    read_tree_entry(root, Path::new(""), &mut snapshot)?;
    Ok(snapshot)
}

fn read_tree_entry(path: &Path, relative: &Path, snapshot: &mut TreeSnapshot) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("reparse point in generation: {}", path.display()),
            ));
        }
    }
    if metadata.file_type().is_dir() {
        snapshot
            .directories
            .insert(relative.to_path_buf(), directory_identity(path)?);
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            read_tree_entry(&entry.path(), &relative.join(entry.file_name()), snapshot)?;
        }
    } else if metadata.file_type().is_file() {
        snapshot
            .files
            .insert(relative.to_path_buf(), read_regular_file_snapshot(path)?);
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("non-regular entry in generation: {}", path.display()),
        ));
    }
    Ok(())
}

fn directory_identity(path: &Path) -> io::Result<FileIdentity> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("generation directory changed: {}", path.display()),
        ));
    }
    file_identity(&file, path)
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    volume: u32,
    index: u64,
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    canonical_path: PathBuf,
}

pub(crate) fn read_regular_file_snapshot(path: &Path) -> io::Result<FileSnapshot> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("file is a reparse point: {}", path.display()),
            ));
        }
    }
    let identity = file_identity(&file, path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(FileSnapshot { bytes, identity })
}

pub(crate) fn read_optional_regular_file_snapshot(path: &Path) -> io::Result<Option<FileSnapshot>> {
    match read_regular_file_snapshot(path) {
        Ok(snapshot) => Ok(Some(snapshot)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn file_identity(file: &fs::File, _path: &Path) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(file: &fs::File, _path: &Path) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe {
        GetFileInformationByHandle(file.as_raw_handle(), std::ptr::from_mut(&mut information))
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        volume: information.dwVolumeSerialNumber,
        index: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &fs::File, path: &Path) -> io::Result<FileIdentity> {
    Ok(FileIdentity {
        canonical_path: fs::canonicalize(path)?,
    })
}

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

/// Atomically replaces an existing regular file with a staged file in the same
/// directory. The caller is responsible for validating the destination and for
/// syncing both the staged file and its parent directory.
pub(crate) fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    replace_file_platform(source, destination)
}

#[cfg(windows)]
fn resolved_parent_path(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("file path has no final component: {}", path.display()),
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(fs::canonicalize(parent)?.join(name))
}

#[cfg(windows)]
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("file path contains a NUL character: {}", path.display()),
        ));
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(windows)]
fn replace_file_platform(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ptr;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let source = wide_path(&resolved_parent_path(source)?)?;
    let destination = wide_path(&resolved_parent_path(destination)?)?;
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            ptr::null(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn replace_file_platform(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(not(any(windows, unix)))]
fn replace_file_platform(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic file replacement is not implemented on this platform",
    ))
}

#[cfg(windows)]
fn rename_noclobber_platform(source: &Path, destination: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

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
