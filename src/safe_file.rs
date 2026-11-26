use aws_lc_rs::digest::{self, SHA256, SHA256_OUTPUT_LEN};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const SNAPSHOT_READ_CHUNK: usize = 64 * 1024;

#[cfg(target_os = "macos")]
pub(crate) mod macos_metadata;

#[cfg(windows)]
pub(crate) fn protected_windows_attributes(metadata: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
    };
    metadata.file_attributes()
        & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SYSTEM)
}

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

    pub(crate) fn is_stress_for(&self, contest_id: &str, problem_index: &str) -> bool {
        self.kind == "stress"
            && self.contest_id == contest_id
            && self
                .problem_index
                .as_ref()
                .is_some_and(|owner| owner.eq_ignore_ascii_case(problem_index))
    }
}

pub(crate) const RECOVERY_INTENT: &str = "recovery-intent.toml";
pub(crate) const RECOVERY_COMPLETE: &str = "recovery-complete.toml";

pub(crate) fn write_recovery_intent(root: &Path, identity: &RecoveryIdentity) -> io::Result<()> {
    write_recovery_record(&root.join(RECOVERY_INTENT), identity)
}

pub(crate) fn write_recovery_intent_at(root: &Dir, identity: &RecoveryIdentity) -> io::Result<()> {
    let file = root.open_with(
        RECOVERY_INTENT,
        OpenOptions::new().write(true).create_new(true),
    )?;
    write_recovery_record_to(file.into_std(), identity)
}

/// The staging name remains an interruption signal until the same-volume no-clobber rename.
/// A completed name is never installed unless the completion record has been flushed.
pub(crate) fn complete_recovery(
    staging: PreservedTempDir,
    completed_path: &Path,
    identity: &RecoveryIdentity,
) -> io::Result<PathBuf> {
    let root = staging.keep();
    let result = read_regular_file_snapshot(&root.join(RECOVERY_INTENT))
        .and_then(|record| parse_recovery_record(&record.bytes))
        .and_then(|intent| {
            if intent != *identity {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "recovery intent changed",
                ));
            }
            write_recovery_record(&root.join(RECOVERY_COMPLETE), identity)
        })
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

#[cfg(test)]
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

fn write_recovery_record(path: &Path, identity: &RecoveryIdentity) -> io::Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    write_recovery_record_to(file, identity)
}

fn write_recovery_record_to(mut file: fs::File, identity: &RecoveryIdentity) -> io::Result<()> {
    let content = toml::to_string(identity).map_err(io::Error::other)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileSnapshot {
    pub(crate) bytes: Vec<u8>,
    identity: FileIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TreeSnapshot<F = FileSnapshot> {
    directories: std::collections::BTreeMap<PathBuf, FileIdentity>,
    files: std::collections::BTreeMap<PathBuf, F>,
}

pub(crate) type TreeFingerprint = TreeSnapshot<FileFingerprint>;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileFingerprint {
    identity: FileIdentity,
    size: u64,
    sha256: [u8; SHA256_OUTPUT_LEN],
}

pub(crate) struct WatchedFileStamp {
    pub(crate) fingerprint: FileFingerprint,
    modified: std::time::SystemTime,
    #[cfg(target_os = "macos")]
    changed: (i64, i64),
}

impl WatchedFileStamp {
    #[cfg(target_os = "macos")]
    pub(crate) fn metadata_only_change_from(&self, previous: &Self) -> bool {
        self.changed != previous.changed && self.same_write_as(previous)
    }

    pub(crate) fn same_write_as(&self, previous: &Self) -> bool {
        self.fingerprint == previous.fingerprint && self.modified == previous.modified
    }
}

impl FileSnapshot {
    pub(crate) fn fingerprint(&self) -> FileFingerprint {
        FileFingerprint {
            identity: self.identity.clone(),
            size: self.bytes.len() as u64,
            sha256: sha256_bytes(digest::digest(&SHA256, &self.bytes)),
        }
    }
}

impl TreeFingerprint {
    /// Compare the plan with exact bytes already read for a replacement under lock.
    pub(crate) fn matches_snapshot(&self, snapshot: &TreeSnapshot) -> bool {
        self.directories == snapshot.directories
            && self.files.len() == snapshot.files.len()
            && self.files.iter().zip(&snapshot.files).all(
                |((expected_path, expected), (path, file))| {
                    expected_path == path && *expected == file.fingerprint()
                },
            )
    }
}

pub(crate) fn matches_tree_snapshot(
    expected: Option<&TreeFingerprint>,
    actual: Option<&TreeSnapshot>,
) -> bool {
    match (expected, actual) {
        (Some(expected), Some(actual)) => expected.matches_snapshot(actual),
        (None, None) => true,
        _ => false,
    }
}

/// Records the exact files and directory identities in a generation. Callers validate their
/// ownership rules first; this snapshot only detects changes across the generation move.
pub(crate) fn read_tree_snapshot(root: &Path) -> io::Result<TreeSnapshot> {
    let directory = Dir::from_std_file(open_tree_directory(root)?);
    read_tree_snapshot_in(&directory, root)
}

pub(crate) fn read_tree_snapshot_in(directory: &Dir, root: &Path) -> io::Result<TreeSnapshot> {
    read_tree_in(directory, root, read_regular_file_snapshot_at)
}

pub(crate) fn read_tree_fingerprint_in(
    directory: &Dir,
    root: &Path,
) -> io::Result<TreeFingerprint> {
    read_tree_in(directory, root, read_regular_file_fingerprint_at)
}

pub(crate) fn read_regular_file_fingerprint_at(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
) -> io::Result<FileFingerprint> {
    fingerprint_regular_file_at(directory, name, path, |file, limit| {
        hash_reader(file.take(limit))
    })
}

pub(crate) fn read_watched_file_stamp_at(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
) -> io::Result<WatchedFileStamp> {
    let (file, hash) = read_stable_regular_file_at(directory, name, path, |file, limit| {
        hash_reader(file.take(limit))
    })?;
    Ok(WatchedFileStamp {
        modified: file.modified,
        #[cfg(target_os = "macos")]
        changed: file.changed,
        fingerprint: file.into_fingerprint(hash),
    })
}

fn read_tree_in<F>(
    directory: &Dir,
    root: &Path,
    read_file: impl Fn(&Dir, &OsStr, &Path) -> io::Result<F>,
) -> io::Result<TreeSnapshot<F>> {
    let mut snapshot = TreeSnapshot {
        directories: std::collections::BTreeMap::new(),
        files: std::collections::BTreeMap::new(),
    };
    read_tree_entries(directory, root, Path::new(""), &mut snapshot, &read_file)?;
    Ok(snapshot)
}

fn read_tree_entries<F>(
    directory: &Dir,
    root: &Path,
    relative: &Path,
    snapshot: &mut TreeSnapshot<F>,
    read_file: &impl Fn(&Dir, &OsStr, &Path) -> io::Result<F>,
) -> io::Result<()> {
    let path = root.join(relative);
    require_tree_directory(&directory.dir_metadata()?, &path)?;
    let identity = file_identity(&directory.try_clone()?.into_std_file(), &path)?;
    snapshot
        .directories
        .insert(relative.to_path_buf(), identity);
    for entry in directory.entries()? {
        let name = entry?.file_name();
        let entry_relative = relative.join(&name);
        let entry_path = root.join(&entry_relative);
        let metadata = directory.symlink_metadata(&name)?;
        if metadata.is_dir() {
            require_tree_directory(&metadata, &entry_path)?;
            let child = directory.open_dir_nofollow(&name)?;
            read_tree_entries(&child, root, &entry_relative, snapshot, read_file)?;
        } else if metadata.is_file() {
            require_regular_file(&metadata, &entry_path)?;
            let file = read_file(directory, &name, &entry_path)?;
            snapshot.files.insert(entry_relative, file);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("non-regular entry in generation: {}", entry_path.display()),
            ));
        }
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct FileObservation {
    identity: FileIdentity,
    size: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl FileObservation {
    fn into_fingerprint(self, hash: digest::Digest) -> FileFingerprint {
        FileFingerprint {
            identity: self.identity,
            size: self.size,
            sha256: sha256_bytes(hash),
        }
    }
}

fn observe_regular_file(file: &fs::File, path: &Path) -> io::Result<FileObservation> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    let observation = FileObservation {
        identity: file_identity(file, path)?,
        size: metadata.len(),
        modified: metadata.modified()?,
        #[cfg(unix)]
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    };
    require_regular_file(&cap_std::fs::Metadata::from_just_metadata(metadata), path)?;
    Ok(observation)
}

/// Check sample encoding without retaining the sample text in a diagnosis.
pub(crate) fn validate_regular_utf8_at(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
) -> io::Result<()> {
    read_stable_regular_file_at(directory, name, path, |file, limit| {
        let mut reader = file.take(limit);
        validate_utf8(&mut reader).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to read {}: {error}", path.display()),
            )
        })?;
        Ok((limit - reader.limit(), ()))
    })?;
    Ok(())
}

fn validate_utf8(mut reader: impl Read) -> io::Result<()> {
    let mut buffer = [0; SNAPSHOT_READ_CHUNK];
    let mut pending = 0;
    loop {
        let count = match reader.read(&mut buffer[pending..]) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        match std::str::from_utf8(&buffer[..pending + count]) {
            Ok(_) if count == 0 => return Ok(()),
            Ok(_) => pending = 0,
            Err(error) if count != 0 && error.error_len().is_none() => {
                let end = pending + count;
                pending = end - error.valid_up_to();
                buffer.copy_within(error.valid_up_to()..end, 0);
            }
            Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
        }
    }
}

fn fingerprint_regular_file_at(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
    read: impl FnOnce(&mut fs::File, u64) -> io::Result<(u64, digest::Digest)>,
) -> io::Result<FileFingerprint> {
    let (file, hash) = read_stable_regular_file_at(directory, name, path, read)?;
    Ok(file.into_fingerprint(hash))
}

fn read_stable_regular_file_at<T>(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
    read: impl FnOnce(&mut fs::File, u64) -> io::Result<(u64, T)>,
) -> io::Result<(FileObservation, T)> {
    let mut file = open_file_nofollow_at(directory, name)?;
    let before = observe_regular_file(&file, path)?;
    // Read at most one byte beyond the original length so concurrent growth cannot keep
    // the scanner reading indefinitely. The reopened file must still name the same object.
    let (size, value) = read(&mut file, before.size.saturating_add(1))?;
    if size != before.size
        || observe_regular_file(&file, path)? != before
        || observe_regular_file(&open_file_nofollow_at(directory, name)?, path)? != before
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!(
                "file changed while reading its snapshot: {}",
                path.display()
            ),
        ));
    }
    Ok((before, value))
}

fn hash_reader(mut reader: impl Read) -> io::Result<(u64, digest::Digest)> {
    let mut hash = digest::Context::new(&SHA256);
    let mut buffer = [0; SNAPSHOT_READ_CHUNK];
    let mut size = 0;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok((size, hash.finish())),
            Ok(read) => {
                size += read as u64;
                hash.update(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn sha256_bytes(hash: digest::Digest) -> [u8; SHA256_OUTPUT_LEN] {
    hash.as_ref().try_into().expect("SHA-256 digest length")
}

fn require_tree_directory(metadata: &cap_std::fs::Metadata, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("reparse point in generation: {}", path.display()),
            ));
        }
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("generation directory changed: {}", path.display()),
        ));
    }
    Ok(())
}

fn open_tree_directory(path: &Path) -> io::Result<fs::File> {
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
        options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
    }
    options.open(path)
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
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    snapshot_regular_file(options.open(path)?, path)
}

pub(crate) fn read_regular_file_snapshot_at(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
) -> io::Result<FileSnapshot> {
    snapshot_regular_file(open_file_nofollow_at(directory, name)?, path)
}

fn open_file_nofollow_at(directory: &Dir, name: &OsStr) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    Ok(directory.open_with(name, &options)?.into_std())
}

fn snapshot_regular_file(mut file: fs::File, path: &Path) -> io::Result<FileSnapshot> {
    let metadata = file.metadata()?;
    require_regular_file(
        &cap_std::fs::Metadata::from_just_metadata(metadata.clone()),
        path,
    )?;
    let identity = file_identity(&file, path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(FileSnapshot { bytes, identity })
}

pub(crate) fn regular_file_exists_at(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
) -> io::Result<bool> {
    match directory.symlink_metadata(name) {
        Ok(metadata) => {
            require_regular_file(&metadata, path)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn read_bounded_regular_text_at(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
    max_bytes: u64,
) -> io::Result<String> {
    let file = open_file_nofollow_at(directory, name).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read {}: {error}", path.display()),
        )
    })?;
    let (bytes, _) = read_bounded_regular_bytes(file, path, max_bytes)?;
    String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file is not valid UTF-8: {}", path.display()),
        )
    })
}

pub(crate) fn read_bounded_regular_file_snapshot_at(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
    max_bytes: u64,
) -> io::Result<FileSnapshot> {
    read_bounded_regular_file_snapshot_with_metadata_at(directory, name, path, max_bytes)
        .map(|(snapshot, _)| snapshot)
}

pub(crate) fn read_bounded_regular_file_snapshot_with_metadata_at(
    directory: &Dir,
    name: &OsStr,
    path: &Path,
    max_bytes: u64,
) -> io::Result<(FileSnapshot, fs::Metadata)> {
    let file = open_file_nofollow_at(directory, name)?;
    let identity = file_identity(&file, path)?;
    let (bytes, metadata) = read_bounded_regular_bytes(file, path, max_bytes)?;
    Ok((FileSnapshot { bytes, identity }, metadata))
}

fn read_bounded_regular_bytes(
    file: fs::File,
    path: &Path,
    max_bytes: u64,
) -> io::Result<(Vec<u8>, fs::Metadata)> {
    let metadata = file
        .metadata()
        .map_err(|error| bounded_read_error(path, error))?;
    require_regular_file(
        &cap_std::fs::Metadata::from_just_metadata(metadata.clone()),
        path,
    )?;
    let bytes = read_bounded_bytes(file, metadata.len(), path, max_bytes)?;
    Ok((bytes, metadata))
}

fn bounded_read_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("failed to read {}: {error}", path.display()),
    )
}

fn read_bounded_bytes(
    reader: impl Read,
    observed_size: u64,
    path: &Path,
    max_bytes: u64,
) -> io::Result<Vec<u8>> {
    let too_large = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds {max_bytes} bytes: {}", path.display()),
        )
    };
    if observed_size > max_bytes {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| bounded_read_error(path, error))?;
    if bytes.len() as u64 > max_bytes {
        return Err(too_large());
    }
    Ok(bytes)
}

fn require_regular_file(metadata: &cap_std::fs::Metadata, path: &Path) -> io::Result<()> {
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("file is a reparse point: {}", path.display()),
            ));
        }
    }
    Ok(())
}

pub(crate) fn read_optional_regular_file_snapshot(path: &Path) -> io::Result<Option<FileSnapshot>> {
    match read_regular_file_snapshot(path) {
        Ok(snapshot) => Ok(Some(snapshot)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn require_absent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("unexpected entry at {}", path.display()),
        )),
    }
}

pub(crate) fn same_open_file(left: &fs::File, right: &fs::File) -> io::Result<bool> {
    #[cfg(any(unix, windows))]
    {
        Ok(file_identity(left, Path::new(""))? == file_identity(right, Path::new(""))?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file identity is unavailable",
        ))
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

/// Move between verified parent directories. On Windows, callers keep the parent
/// handles open without delete sharing for the duration of the path-based move.
pub(crate) fn rename_noclobber_at(
    source_parent: &Dir,
    source: &Path,
    destination_parent: &Dir,
    destination: &Path,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;

        let name = |path: &Path| {
            path.file_name().map(PathBuf::from).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "rename path has no filename")
            })
        };
        rename_noclobber_relative(
            source_parent.as_raw_fd(),
            &name(source)?,
            destination_parent.as_raw_fd(),
            &name(destination)?,
        )
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                RenameContext {
                    source_path: source.to_path_buf(),
                    destination: destination.to_path_buf(),
                    source: error,
                },
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (source_parent, destination_parent);
        rename_noclobber(source, destination)
    }
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
// Registry uses the raw result to retry only a confirmed name collision.
pub(crate) fn rename_noclobber_platform(source: &Path, destination: &Path) -> io::Result<()> {
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

#[cfg(unix)]
fn rename_noclobber_platform(source: &Path, destination: &Path) -> io::Result<()> {
    rename_noclobber_relative(libc::AT_FDCWD, source, libc::AT_FDCWD, destination)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_noclobber_relative(
    source_parent: std::os::fd::RawFd,
    source: &Path,
    destination_parent: std::os::fd::RawFd,
    destination: &Path,
) -> io::Result<()> {
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
            source_parent,
            source.as_ptr(),
            destination_parent,
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
fn rename_noclobber_relative(
    source_parent: std::os::fd::RawFd,
    source: &Path,
    destination_parent: std::os::fd::RawFd,
    destination: &Path,
) -> io::Result<()> {
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
        libc::renameatx_np(
            source_parent,
            source.as_ptr(),
            destination_parent,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
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
fn rename_noclobber_relative(
    _source_parent: std::os::fd::RawFd,
    _source: &Path,
    _destination_parent: std::os::fd::RawFd,
    _destination: &Path,
) -> io::Result<()> {
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

    #[test]
    fn bounded_read_stops_when_a_regular_file_grows_during_read() {
        use std::io::Seek as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("contest.toml");
        fs::write(&path, [b'a'; 8]).unwrap();
        let mut file = fs::File::open(&path).unwrap();
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        let observed_size = file.metadata().unwrap().len();
        let mut first_read = true;
        let reader = io::Read::by_ref(&mut file);
        struct GrowOnRead<F>(F);
        impl<F: FnMut(&mut [u8]) -> io::Result<usize>> Read for GrowOnRead<F> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                (self.0)(bytes)
            }
        }
        let reader = GrowOnRead(|buffer: &mut [u8]| {
            let read = reader.read(buffer)?;
            if first_read {
                assert!(read > 0);
                writer.write_all(&[b'b'; 32])?;
                first_read = false;
            }
            Ok(read)
        });
        let error = read_bounded_bytes(reader, observed_size, &path, 16).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(file.stream_position().unwrap(), 17);
        assert_eq!(file.metadata().unwrap().len(), 40);
    }

    #[test]
    fn tree_fingerprints_match_exact_snapshots_and_detect_content_structure_and_identity() {
        for change in [
            "same-size",
            "new-file",
            "missing-file",
            "file",
            "directory",
            "new-empty",
            "missing-empty",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("tests");
            fs::create_dir_all(root.join("custom/empty")).unwrap();
            let path = root.join("custom/input");
            fs::write(&path, b"original").unwrap();
            let current = || {
                let directory = Dir::from_std_file(open_tree_directory(&root).unwrap());
                read_tree_fingerprint_in(&directory, &root).unwrap()
            };
            let expected = current();
            assert!(expected.matches_snapshot(&read_tree_snapshot(&root).unwrap()));
            match change {
                "same-size" => fs::write(&path, b"modified").unwrap(),
                "new-file" => fs::write(root.join("custom/new"), b"").unwrap(),
                "missing-file" => fs::remove_file(&path).unwrap(),
                "file" => {
                    fs::rename(&path, temp.path().join("original-file")).unwrap();
                    fs::write(&path, b"original").unwrap();
                }
                "directory" => {
                    fs::rename(root.join("custom/empty"), temp.path().join("original-dir"))
                        .unwrap();
                    fs::create_dir(root.join("custom/empty")).unwrap();
                }
                "new-empty" => fs::create_dir(root.join("custom/new")).unwrap(),
                "missing-empty" => fs::remove_dir(root.join("custom/empty")).unwrap(),
                _ => unreachable!(),
            }
            let edited = current();
            let exact = read_tree_snapshot(&root).unwrap();
            assert_ne!(edited, expected, "{change}");
            assert!(edited.matches_snapshot(&exact), "{change}");
            assert!(!expected.matches_snapshot(&exact), "{change}");
        }
    }

    #[test]
    fn fingerprint_rejects_a_same_size_edit_during_read_and_propagates_io_errors() {
        use std::time::{Duration, SystemTime};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sample");
        let name = OsStr::new("sample");
        fs::write(&path, b"before").unwrap();
        let directory = Dir::from_std_file(open_tree_directory(temp.path()).unwrap());
        let stable_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stable_time)
            .unwrap();
        let error = fingerprint_regular_file_at(&directory, name, &path, |file, limit| {
            let mut first = [0];
            file.read_exact(&mut first)?;
            let mut writer = fs::File::options().write(true).open(&path)?;
            writer.write_all(b"edited")?;
            writer.set_modified(stable_time + Duration::from_secs(5))?;
            drop(writer);
            hash_reader(io::Cursor::new(first).chain(file.take(limit - 1)))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);

        let error = fingerprint_regular_file_at(&directory, name, &path, |_, _| {
            hash_reader(io::repeat(0).take(1).chain(TestReadError))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    struct TestReadError;

    impl Read for TestReadError {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
    }

    #[test]
    fn streaming_utf8_validates_characters_crossing_the_buffer_boundary() {
        let mut bytes = vec![b'a'; SNAPSHOT_READ_CHUNK - 1];
        bytes.extend_from_slice("🦀".as_bytes());
        assert!(validate_utf8(bytes.as_slice()).is_ok());
        bytes.pop();
        assert_eq!(
            validate_utf8(bytes.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        bytes.push(0xff);
        assert_eq!(
            validate_utf8(bytes.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            validate_utf8(TestReadError).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
    use std::ffi::OsStr;
    use std::fs;

    const PREFIX: &str = ".atc-safe-create-";

    #[cfg(unix)]
    #[test]
    fn recovery_record_fifo_is_rejected_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        use std::sync::mpsc;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(RECOVERY_INTENT);
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (done, result) = mpsc::channel();
        let record = path.clone();
        let reader = std::thread::spawn(move || {
            done.send(read_regular_file_snapshot(&record)).unwrap();
        });

        let read = result.recv_timeout(Duration::from_secs(2));
        if read.is_err() {
            // Unblock a regressed reader so the test fails without hanging the suite.
            if let Ok(writer) = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
            {
                reader.join().unwrap();
                drop(writer);
            }
            panic!("opening the recovery record blocked without a FIFO writer");
        }
        reader.join().unwrap();
        assert_eq!(
            read.unwrap().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

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
