use crate::config::Config;
use crate::safe_file;
use crate::settings::{SettingsDocument, SettingsDocumentError};
use crate::user_config_fs;
use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const STAGING_PREFIX: &str = ".atc-settings-";

#[cfg(target_os = "macos")]
mod macos;

#[derive(Debug)]
pub(crate) enum SettingsStoreLoadError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    InvalidUtf8 {
        path: PathBuf,
        source: std::str::Utf8Error,
    },
    InvalidDocument(SettingsDocumentError),
    UnsupportedFileType {
        path: PathBuf,
    },
}

impl fmt::Display for SettingsStoreLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} Global Config {}: {source}",
                path.display()
            ),
            Self::InvalidUtf8 { path, source } => write!(
                formatter,
                "Global Config is not valid UTF-8 ({}): {source}",
                path.display()
            ),
            Self::InvalidDocument(error) => error.fmt(formatter),
            Self::UnsupportedFileType { path } => write!(
                formatter,
                "Global Config must be a regular file: {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SettingsStoreLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidUtf8 { source, .. } => Some(source),
            Self::InvalidDocument(source) => Some(source),
            Self::UnsupportedFileType { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostCommitState {
    CandidatePresent,
    BaselinePresent,
    Unknown,
}

#[derive(Debug)]
pub(crate) enum SettingsSaveError {
    ReadOnly(String),
    Conflict(String),
    BeforeCommit(io::Error),
    PostCommit {
        source: io::Error,
        state: PostCommitState,
    },
}

impl fmt::Display for SettingsSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadOnly(reason) => write!(
                formatter,
                "Settings cannot safely update this Global Config: {reason}"
            ),
            Self::Conflict(reason) => write!(
                formatter,
                "Global Config changed outside Settings; the draft was not saved: {reason}"
            ),
            Self::BeforeCommit(error) => {
                write!(formatter, "Global Config was not replaced: {error}")
            }
            Self::PostCommit {
                source,
                state: PostCommitState::CandidatePresent,
            } => write!(
                formatter,
                "Global Config contains the Settings change, but finalization failed; durability is uncertain: {source}"
            ),
            Self::PostCommit {
                source,
                state: PostCommitState::BaselinePresent,
            } => write!(
                formatter,
                "Global Config still contains the previous settings, but finalization failed: {source}"
            ),
            Self::PostCommit {
                source,
                state: PostCommitState::Unknown,
            } => write!(
                formatter,
                "Global Config may have changed, but Settings could not confirm its state: {source}"
            ),
        }
    }
}

impl std::error::Error for SettingsSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeforeCommit(error) | Self::PostCommit { source: error, .. } => Some(error),
            Self::ReadOnly(_) | Self::Conflict(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveOutcome {
    Unchanged,
    Saved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows { volume: u32, index: u64 },
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataContract {
    #[cfg(unix)]
    mode: u32,
    #[cfg(target_os = "macos")]
    extended: macos::ExtendedMetadata,
    #[cfg(windows)]
    protected_attributes: u32,
}

#[derive(Debug, Clone)]
struct FileSnapshot {
    bytes: Vec<u8>,
    identity: FileIdentity,
    metadata: MetadataContract,
    #[cfg(unix)]
    permissions: fs::Permissions,
}

impl FileSnapshot {
    fn same_baseline(&self, other: &Self) -> bool {
        self.bytes == other.bytes
            && self.identity == other.identity
            && self.metadata == other.metadata
    }
}

#[derive(Debug, Clone)]
enum Baseline {
    Missing,
    Existing(FileSnapshot),
}

#[derive(Debug, Clone)]
enum StoreAccess {
    Writable,
    ReadOnly(String),
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsStore {
    path: PathBuf,
    baseline: Baseline,
    access: StoreAccess,
    document: SettingsDocument,
}

impl SettingsStore {
    pub(crate) fn load(path: impl Into<PathBuf>) -> Result<Self, SettingsStoreLoadError> {
        let path = path.into();
        match inspect(&path)? {
            Inspected::Missing => Ok(Self {
                path,
                baseline: Baseline::Missing,
                access: StoreAccess::Writable,
                document: SettingsDocument::empty(),
            }),
            Inspected::Present { snapshot, linked } => {
                let contents = std::str::from_utf8(&snapshot.bytes).map_err(|source| {
                    SettingsStoreLoadError::InvalidUtf8 {
                        path: path.clone(),
                        source,
                    }
                })?;
                let document = SettingsDocument::parse(contents)
                    .map_err(SettingsStoreLoadError::InvalidDocument)?;
                let access = if linked {
                    StoreAccess::ReadOnly(
                        "the file is a symlink or Windows reparse point".to_string(),
                    )
                } else if let Some(reason) = document.preservation_issue() {
                    StoreAccess::ReadOnly(reason.to_string())
                } else {
                    StoreAccess::Writable
                };
                Ok(Self {
                    path,
                    baseline: Baseline::Existing(snapshot),
                    access,
                    document,
                })
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn document(&self) -> &SettingsDocument {
        &self.document
    }

    pub(crate) fn document_mut(&mut self) -> &mut SettingsDocument {
        &mut self.document
    }

    pub(crate) fn read_only_reason(&self) -> Option<&str> {
        match &self.access {
            StoreAccess::Writable => None,
            StoreAccess::ReadOnly(reason) => Some(reason),
        }
    }

    pub(crate) fn reload(&mut self) -> Result<(), SettingsStoreLoadError> {
        *self = Self::load(self.path.clone())?;
        Ok(())
    }

    pub(crate) fn save(&mut self) -> Result<SaveOutcome, SettingsSaveError> {
        let candidate = self.document.candidate().into_bytes();
        Config::parse(std::str::from_utf8(&candidate).expect("Settings candidate is UTF-8"))
            .map_err(SettingsSaveError::BeforeCommit)?;

        let baseline_bytes = match &self.baseline {
            Baseline::Missing => &[][..],
            Baseline::Existing(snapshot) => snapshot.bytes.as_slice(),
        };
        if !self.document.has_changes() || candidate == baseline_bytes {
            self.confirm_unchanged_baseline()?;
            return Ok(SaveOutcome::Unchanged);
        }
        if let StoreAccess::ReadOnly(reason) = &self.access {
            return Err(SettingsSaveError::ReadOnly(reason.clone()));
        }

        match self.baseline.clone() {
            Baseline::Missing => self.save_new(candidate),
            Baseline::Existing(baseline) => self.save_existing(candidate, baseline),
        }
    }

    fn confirm_unchanged_baseline(&self) -> Result<(), SettingsSaveError> {
        match &self.baseline {
            Baseline::Missing => match fs::symlink_metadata(&self.path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Ok(_) => Err(SettingsSaveError::Conflict(
                    "the missing file was created by another process".to_string(),
                )),
                Err(error) => Err(SettingsSaveError::BeforeCommit(error)),
            },
            Baseline::Existing(baseline) => match inspect_writable(&self.path) {
                Ok(current) if baseline.same_baseline(&current) => Ok(()),
                Ok(_)
                | Err(
                    SnapshotError::Missing
                    | SnapshotError::UnsafeType
                    | SnapshotError::ChangedDuringRead,
                ) => Err(SettingsSaveError::Conflict(
                    "the file changed outside Settings".to_string(),
                )),
                Err(SnapshotError::Io(error)) => Err(SettingsSaveError::BeforeCommit(error)),
            },
        }
    }

    fn save_new(&mut self, candidate: Vec<u8>) -> Result<SaveOutcome, SettingsSaveError> {
        let parent = parent_directory(&self.path).map_err(SettingsSaveError::BeforeCommit)?;
        user_config_fs::ensure_directory(parent, "Global Config directory").map_err(|error| {
            SettingsSaveError::BeforeCommit(io::Error::other(error.to_string()))
        })?;

        match safe_file::install_noclobber(&self.path, &candidate, STAGING_PREFIX) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(SettingsSaveError::Conflict(
                    "the missing file was created by another process".to_string(),
                ));
            }
            Err(error) => return Err(SettingsSaveError::BeforeCommit(error)),
        }

        if let Err(error) = sync_parent(parent) {
            return Err(self.post_commit_error(error, &candidate, None));
        }
        self.accept_saved_candidate(&candidate, None)
    }

    fn save_existing(
        &mut self,
        candidate: Vec<u8>,
        baseline: FileSnapshot,
    ) -> Result<SaveOutcome, SettingsSaveError> {
        self.save_existing_with(
            candidate,
            baseline,
            |file| file.sync_all(),
            safe_file::replace_file,
            sync_parent,
        )
    }

    fn save_existing_with(
        &mut self,
        candidate: Vec<u8>,
        baseline: FileSnapshot,
        sync_staging: impl FnOnce(&mut File) -> io::Result<()>,
        replace: impl FnOnce(&Path, &Path) -> io::Result<()>,
        sync_directory: impl FnOnce(&Path) -> io::Result<()>,
    ) -> Result<SaveOutcome, SettingsSaveError> {
        #[cfg(target_os = "macos")]
        self.confirm_unchanged_baseline()?;
        let parent = parent_directory(&self.path).map_err(SettingsSaveError::BeforeCommit)?;
        let mut staging = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempfile_in(parent)
            .map_err(SettingsSaveError::BeforeCommit)?;
        staging
            .write_all(&candidate)
            .map_err(SettingsSaveError::BeforeCommit)?;
        #[cfg(target_os = "macos")]
        macos::preserve_metadata(&self.path, staging.as_file())
            .map_err(SettingsSaveError::BeforeCommit)?;
        #[cfg(not(target_os = "macos"))]
        preserve_metadata(&self.path, staging.path(), &baseline)
            .map_err(SettingsSaveError::BeforeCommit)?;
        sync_staging(staging.as_file_mut()).map_err(SettingsSaveError::BeforeCommit)?;
        #[cfg(target_os = "macos")]
        {
            // Copying metadata and validating the source are separate operations.
            // Also reject a partial copy, or metadata copied from a transient
            // source state that has since reverted to the loaded baseline.
            let metadata = staging
                .as_file()
                .metadata()
                .map_err(SettingsSaveError::BeforeCommit)?;
            let staged_metadata = macos::metadata_contract(&metadata, staging.as_file())
                .map_err(SettingsSaveError::BeforeCommit)?;
            if staged_metadata != baseline.metadata {
                return Err(SettingsSaveError::BeforeCommit(io::Error::other(
                    "staged file metadata does not match the loaded Global Config",
                )));
            }
        }
        let staged_path = staging.into_temp_path();

        let current = inspect_writable(&self.path).map_err(|error| match error {
            SnapshotError::Missing
            | SnapshotError::UnsafeType
            | SnapshotError::ChangedDuringRead => SettingsSaveError::Conflict(
                "the file was deleted, replaced, changed type, or changed while being read"
                    .to_string(),
            ),
            SnapshotError::Io(error) => SettingsSaveError::BeforeCommit(error),
        })?;
        if !baseline.same_baseline(&current) {
            return Err(SettingsSaveError::Conflict(
                "its exact contents, file identity, or metadata no longer match the loaded baseline"
                    .to_string(),
            ));
        }

        if let Err(error) = replace(&staged_path, &self.path) {
            return Err(self.post_commit_error(error, &candidate, Some(&baseline)));
        }
        if let Err(error) = sync_directory(parent) {
            return Err(self.post_commit_error(error, &candidate, Some(&baseline)));
        }
        self.accept_saved_candidate(&candidate, Some(&baseline.metadata))
    }

    #[cfg(test)]
    pub(crate) fn save_existing_with_test_hooks(
        &mut self,
        replace: impl FnOnce(&Path, &Path) -> io::Result<()>,
        sync_directory: impl FnOnce(&Path) -> io::Result<()>,
    ) -> Result<SaveOutcome, SettingsSaveError> {
        let candidate = self.document.candidate().into_bytes();
        Config::parse(std::str::from_utf8(&candidate).expect("Settings candidate is UTF-8"))
            .map_err(SettingsSaveError::BeforeCommit)?;
        let Baseline::Existing(baseline) = self.baseline.clone() else {
            panic!("test hook requires an existing Config baseline");
        };
        self.save_existing_with(
            candidate,
            baseline,
            |file| file.sync_all(),
            replace,
            sync_directory,
        )
    }

    fn accept_saved_candidate(
        &mut self,
        candidate: &[u8],
        expected_metadata: Option<&MetadataContract>,
    ) -> Result<SaveOutcome, SettingsSaveError> {
        let snapshot = match inspect_writable(&self.path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(SettingsSaveError::PostCommit {
                    source: error.into_io("failed to inspect the saved Global Config"),
                    state: PostCommitState::Unknown,
                });
            }
        };
        if snapshot.bytes != candidate {
            return Err(SettingsSaveError::PostCommit {
                source: io::Error::other("saved bytes do not match the Settings candidate"),
                state: PostCommitState::Unknown,
            });
        }
        let contents = std::str::from_utf8(candidate).expect("Settings candidate is UTF-8");
        let document =
            SettingsDocument::parse(contents).map_err(|error| SettingsSaveError::PostCommit {
                source: io::Error::other(error.to_string()),
                state: PostCommitState::Unknown,
            })?;
        self.baseline = Baseline::Existing(snapshot);
        self.document = document;
        self.access = StoreAccess::Writable;
        if let Baseline::Existing(snapshot) = &self.baseline
            && expected_metadata.is_some_and(|expected| snapshot.metadata != *expected)
        {
            return Err(SettingsSaveError::PostCommit {
                source: io::Error::other("file metadata changed during replacement"),
                state: PostCommitState::CandidatePresent,
            });
        }
        Ok(SaveOutcome::Saved)
    }

    fn post_commit_error(
        &mut self,
        source: io::Error,
        candidate: &[u8],
        baseline: Option<&FileSnapshot>,
    ) -> SettingsSaveError {
        let state = match inspect_writable(&self.path) {
            Ok(snapshot) if snapshot.bytes == candidate => {
                if let Ok(contents) = std::str::from_utf8(&snapshot.bytes)
                    && let Ok(document) = SettingsDocument::parse(contents)
                {
                    self.baseline = Baseline::Existing(snapshot);
                    self.document = document;
                    PostCommitState::CandidatePresent
                } else {
                    PostCommitState::Unknown
                }
            }
            Ok(snapshot) if baseline.is_some_and(|baseline| baseline.same_baseline(&snapshot)) => {
                return SettingsSaveError::BeforeCommit(source);
            }
            Ok(snapshot) if baseline.is_some_and(|baseline| baseline.bytes == snapshot.bytes) => {
                std::str::from_utf8(&snapshot.bytes)
                    .ok()
                    .and_then(|contents| SettingsDocument::parse(contents).ok())
                    .map(|document| {
                        self.baseline = Baseline::Existing(snapshot);
                        self.document = document;
                        PostCommitState::BaselinePresent
                    })
                    .unwrap_or(PostCommitState::Unknown)
            }
            _ => PostCommitState::Unknown,
        };
        SettingsSaveError::PostCommit { source, state }
    }
}

enum Inspected {
    Missing,
    Present {
        snapshot: FileSnapshot,
        linked: bool,
    },
}

fn inspect(path: &Path) -> Result<Inspected, SettingsStoreLoadError> {
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Inspected::Missing),
        Err(source) => {
            return Err(SettingsStoreLoadError::Io {
                operation: "inspect",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let linked = is_link_or_reparse(&link_metadata);
    if linked {
        let resolved = fs::canonicalize(path).map_err(|source| SettingsStoreLoadError::Io {
            operation: "resolve",
            path: path.to_path_buf(),
            source,
        })?;
        let mut file = open_nofollow(&resolved).map_err(|source| SettingsStoreLoadError::Io {
            operation: "open",
            path: path.to_path_buf(),
            source,
        })?;
        let metadata = file
            .metadata()
            .map_err(|source| SettingsStoreLoadError::Io {
                operation: "inspect target",
                path: path.to_path_buf(),
                source,
            })?;
        if !metadata.is_file() {
            return Err(SettingsStoreLoadError::UnsupportedFileType {
                path: path.to_path_buf(),
            });
        }
        let bytes = read_stably(&mut file).map_err(|source| SettingsStoreLoadError::Io {
            operation: "read",
            path: path.to_path_buf(),
            source,
        })?;
        return Ok(Inspected::Present {
            snapshot: snapshot_from_parts(bytes, &metadata, &file).map_err(|source| {
                SettingsStoreLoadError::Io {
                    operation: "identify",
                    path: path.to_path_buf(),
                    source,
                }
            })?,
            linked: true,
        });
    }
    if !link_metadata.is_file() {
        return Err(SettingsStoreLoadError::UnsupportedFileType {
            path: path.to_path_buf(),
        });
    }
    let snapshot = inspect_writable(path).map_err(|error| match error {
        SnapshotError::Missing => SettingsStoreLoadError::Io {
            operation: "open",
            path: path.to_path_buf(),
            source: io::ErrorKind::NotFound.into(),
        },
        SnapshotError::UnsafeType => SettingsStoreLoadError::UnsupportedFileType {
            path: path.to_path_buf(),
        },
        SnapshotError::ChangedDuringRead => SettingsStoreLoadError::Io {
            operation: "read a stable snapshot of",
            path: path.to_path_buf(),
            source: io::Error::other("the file changed while it was being read"),
        },
        SnapshotError::Io(source) => SettingsStoreLoadError::Io {
            operation: "read",
            path: path.to_path_buf(),
            source,
        },
    })?;
    Ok(Inspected::Present {
        snapshot,
        linked: false,
    })
}

#[derive(Debug)]
enum SnapshotError {
    Missing,
    UnsafeType,
    ChangedDuringRead,
    Io(io::Error),
}

impl SnapshotError {
    fn into_io(self, context: &'static str) -> io::Error {
        match self {
            Self::Missing => io::Error::new(io::ErrorKind::NotFound, context),
            Self::UnsafeType => io::Error::new(io::ErrorKind::InvalidInput, context),
            Self::ChangedDuringRead => io::Error::other(context),
            Self::Io(error) => io::Error::new(error.kind(), format!("{context}: {error}")),
        }
    }
}

fn inspect_writable(path: &Path) -> Result<FileSnapshot, SnapshotError> {
    let mut file = open_nofollow(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            SnapshotError::Missing
        } else {
            SnapshotError::Io(error)
        }
    })?;
    let metadata = file.metadata().map_err(SnapshotError::Io)?;
    if !metadata.is_file() || is_link_or_reparse(&metadata) {
        return Err(SnapshotError::UnsafeType);
    }
    let bytes = read_stably(&mut file).map_err(|error| {
        if error.kind() == io::ErrorKind::Interrupted {
            SnapshotError::ChangedDuringRead
        } else {
            SnapshotError::Io(error)
        }
    })?;
    snapshot_from_parts(bytes, &metadata, &file).map_err(|error| {
        if cfg!(target_os = "macos") && error.kind() == io::ErrorKind::Interrupted {
            SnapshotError::ChangedDuringRead
        } else {
            SnapshotError::Io(error)
        }
    })
}

fn read_stably(file: &mut File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    file.seek(SeekFrom::Start(0))?;
    let mut confirmation = Vec::new();
    file.read_to_end(&mut confirmation)?;
    if bytes == confirmation {
        Ok(bytes)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "file changed while it was being read",
        ))
    }
}

#[cfg(unix)]
fn open_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(windows)]
fn open_nofollow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_nofollow(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-follow file inspection is not implemented on this platform",
    ))
}

fn snapshot_from_parts(
    bytes: Vec<u8>,
    metadata: &Metadata,
    file: &File,
) -> io::Result<FileSnapshot> {
    Ok(FileSnapshot {
        bytes,
        identity: file_identity(metadata, file)?,
        #[cfg(target_os = "macos")]
        metadata: macos::metadata_contract(metadata, file)?,
        #[cfg(not(target_os = "macos"))]
        metadata: metadata_contract(metadata),
        #[cfg(unix)]
        permissions: metadata.permissions(),
    })
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata, _file: &File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Ok(FileIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(_metadata: &Metadata, file: &File) -> io::Result<FileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    let information = unsafe { information.assume_init() };
    let volume = information.dwVolumeSerialNumber;
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok(FileIdentity::Windows { volume, index })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_metadata: &Metadata, _file: &File) -> io::Result<FileIdentity> {
    Ok(FileIdentity::Unsupported)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn metadata_contract(metadata: &Metadata) -> MetadataContract {
    use std::os::unix::fs::MetadataExt;
    MetadataContract {
        mode: metadata.mode(),
    }
}

#[cfg(windows)]
fn metadata_contract(metadata: &Metadata) -> MetadataContract {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
    };
    MetadataContract {
        protected_attributes: metadata.file_attributes()
            & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SYSTEM),
    }
}

#[cfg(not(any(unix, windows)))]
fn metadata_contract(_metadata: &Metadata) -> MetadataContract {
    MetadataContract {}
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn preserve_metadata(
    _source: &Path,
    destination: &Path,
    baseline: &FileSnapshot,
) -> io::Result<()> {
    fs::set_permissions(destination, baseline.permissions.clone())
}

#[cfg(windows)]
fn preserve_metadata(
    _source: &Path,
    _destination: &Path,
    _baseline: &FileSnapshot,
) -> io::Result<()> {
    // ReplaceFileW merges the original file's ACLs, attributes, named streams,
    // compression, and encryption state into the replacement file.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn preserve_metadata(
    _source: &Path,
    _destination: &Path,
    _baseline: &FileSnapshot,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "metadata-preserving replacement is not implemented on this platform",
    ))
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    // Rust does not expose a portable directory-sync operation on Windows.
    // The staged file is synced before ReplaceFileW, which performs the atomic
    // name replacement and metadata merge.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "directory sync is not implemented on this platform",
    ))
}

fn parent_directory(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Global Config path has no parent: {}", path.display()),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{DocumentChange, SettingKey, SettingValue};

    #[cfg(target_os = "macos")]
    #[path = "macos_tests.rs"]
    mod macos;

    fn set_python(store: &mut SettingsStore, value: &str) {
        assert_eq!(
            store
                .document_mut()
                .set(
                    SettingKey::RunnerPython,
                    SettingValue::String(value.to_string()),
                )
                .unwrap(),
            DocumentChange::Changed
        );
    }

    fn staging_entries(parent: &Path) -> Vec<PathBuf> {
        fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(STAGING_PREFIX))
            })
            .collect()
    }

    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(target, link);

        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("failed to create file symlink: {error}"),
        }
    }

    #[test]
    fn missing_reset_does_not_create_a_file_or_parent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config").join("config.toml");
        let mut store = SettingsStore::load(&path).unwrap();

        assert_eq!(
            store
                .document_mut()
                .reset(SettingKey::RunnerPython)
                .unwrap(),
            DocumentChange::Unchanged
        );
        assert_eq!(store.save().unwrap(), SaveOutcome::Unchanged);
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn missing_file_is_created_only_after_an_explicit_change() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config").join("config.toml");
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "python-custom");

        assert_eq!(store.save().unwrap(), SaveOutcome::Saved);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("python = \"python-custom\"")
        );
        assert_eq!(store.save().unwrap(), SaveOutcome::Unchanged);
        assert!(staging_entries(path.parent().unwrap()).is_empty());
    }

    #[test]
    fn missing_file_no_clobber_race_preserves_the_winner_and_draft() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "draft-python");
        let draft = store.document().candidate();
        fs::write(&path, "# external\n").unwrap();

        let error = store.save().unwrap_err();

        assert!(matches!(error, SettingsSaveError::Conflict(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "# external\n");
        assert_eq!(store.document().candidate(), draft);
        assert!(staging_entries(temp.path()).is_empty());
    }

    #[test]
    fn existing_file_update_preserves_unrelated_text_and_refreshes_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let source = "# keep\n[runner]\npython = \"python3\"\n\n[unrelated]\nvalue = \"日本語\"\n";
        // Unknown sections are intentionally invalid in the production parser,
        // so use comments and another supported setting as unrelated content.
        let source = source.replace(
            "[unrelated]\nvalue = \"日本語\"",
            "[submit]\n# 日本語\npython_runtime = \"pypy\"",
        );
        fs::write(&path, &source).unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "python-custom");

        assert_eq!(store.save().unwrap(), SaveOutcome::Saved);
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with("# keep\n[runner]\n"));
        assert!(saved.contains("[submit]\n# 日本語\npython_runtime = \"pypy\""));
        assert_eq!(store.save().unwrap(), SaveOutcome::Unchanged);
    }

    #[test]
    fn exact_external_modification_is_a_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runner]\npython = \"python3\"\n").unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "draft-python");
        fs::write(&path, "[runner]\npython = \"external\"\n").unwrap();

        let error = store.save().unwrap_err();

        assert!(matches!(error, SettingsSaveError::Conflict(_)));
        assert!(fs::read_to_string(&path).unwrap().contains("external"));
        assert!(store.document().candidate().contains("draft-python"));
        assert!(staging_entries(temp.path()).is_empty());
    }

    #[test]
    fn no_op_edit_does_not_report_stale_external_contents_as_current() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let original = "[runner]\npython = \"python3\"\n";
        fs::write(&path, original).unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        assert_eq!(
            store
                .document_mut()
                .set(
                    SettingKey::RunnerPython,
                    SettingValue::String("python3".to_string()),
                )
                .unwrap(),
            DocumentChange::Unchanged
        );
        fs::write(&path, "[runner]\npython = \"external\"\n").unwrap();

        assert!(matches!(store.save(), Err(SettingsSaveError::Conflict(_))));
        assert!(fs::read_to_string(&path).unwrap().contains("external"));
    }

    #[test]
    fn deletion_and_recreation_with_same_bytes_is_a_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let original = "[runner]\npython = \"python3\"\n";
        fs::write(&path, original).unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "draft-python");
        fs::remove_file(&path).unwrap();
        fs::write(&path, original).unwrap();

        let error = store.save().unwrap_err();

        assert!(matches!(error, SettingsSaveError::Conflict(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn symlink_is_viewable_but_structured_write_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.toml");
        let link = temp.path().join("config.toml");
        fs::write(&target, "[runner]\npython = \"python3\"\n").unwrap();
        if !create_file_symlink(&target, &link) {
            return;
        }
        let mut store = SettingsStore::load(&link).unwrap();
        assert!(store.read_only_reason().is_some());
        set_python(&mut store, "draft-python");

        let error = store.save().unwrap_err();

        assert!(matches!(error, SettingsSaveError::ReadOnly(_)));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "[runner]\npython = \"python3\"\n"
        );
        assert!(store.document().candidate().contains("draft-python"));
    }

    #[test]
    fn directory_is_not_treated_as_a_config_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::create_dir(&path).unwrap();

        assert!(matches!(
            SettingsStore::load(&path).unwrap_err(),
            SettingsStoreLoadError::UnsupportedFileType { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_rejected_as_a_nonregular_config_file() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let native = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(native.as_ptr(), 0o600) }, 0);

        assert!(matches!(
            SettingsStore::load(&path).unwrap_err(),
            SettingsStoreLoadError::UnsupportedFileType { .. }
        ));
    }

    #[test]
    fn non_roundtrippable_toml_is_viewable_but_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let source = "[runner]\npython = \"python3\"";
        fs::write(&path, source).unwrap();
        let mut store = SettingsStore::load(&path).unwrap();

        assert!(store.read_only_reason().is_some());
        assert_eq!(store.document().candidate(), source);
        let error = store
            .document_mut()
            .set(
                SettingKey::RunnerPython,
                SettingValue::String("python4".to_string()),
            )
            .unwrap_err();
        assert!(matches!(error, SettingsDocumentError::UnsupportedFormat(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), source);
    }

    #[test]
    fn invalid_config_is_not_presented_as_valid_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[unknown]\nvalue = true\n").unwrap();

        assert!(matches!(
            SettingsStore::load(&path).unwrap_err(),
            SettingsStoreLoadError::InvalidDocument(_)
        ));
    }

    #[test]
    fn explicit_reload_discards_the_draft_and_reads_external_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runner]\npython = \"python3\"\n").unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "draft-python");
        fs::write(&path, "[runner]\npython = \"external\"\n").unwrap();

        store.reload().unwrap();

        assert_eq!(
            store.document().effective_value(SettingKey::RunnerPython),
            SettingValue::String("external".to_string())
        );
    }

    #[test]
    fn crlf_is_preserved_by_existing_file_update() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runner]\r\npython = \"python3\"\r\n").unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "python-custom");

        store.save().unwrap();

        let saved = fs::read(&path).unwrap();
        assert!(saved.windows(2).any(|bytes| bytes == b"\r\n"));
        assert!(
            !String::from_utf8(saved)
                .unwrap()
                .replace("\r\n", "")
                .contains('\n')
        );
    }

    #[test]
    fn staging_sync_failure_is_pre_commit_and_cleans_the_staging_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let original = "[runner]\npython = \"python3\"\n";
        fs::write(&path, original).unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "draft-python");
        let Baseline::Existing(baseline) = store.baseline.clone() else {
            panic!("expected an existing baseline");
        };
        let candidate = store.document().candidate().into_bytes();

        let error = store
            .save_existing_with(
                candidate,
                baseline,
                |_| Err(io::Error::other("injected staging sync failure")),
                safe_file::replace_file,
                sync_parent,
            )
            .unwrap_err();

        assert!(matches!(error, SettingsSaveError::BeforeCommit(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(staging_entries(temp.path()).is_empty());
    }

    #[test]
    fn replacement_failure_with_intact_identity_is_pre_commit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let original = "[runner]\npython = \"python3\"\n";
        fs::write(&path, original).unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "draft-python");
        let Baseline::Existing(baseline) = store.baseline.clone() else {
            panic!("expected an existing baseline");
        };
        let candidate = store.document().candidate().into_bytes();

        let error = store
            .save_existing_with(
                candidate,
                baseline,
                |file| file.sync_all(),
                |_, _| Err(io::Error::other("injected replace failure")),
                sync_parent,
            )
            .unwrap_err();

        assert!(matches!(error, SettingsSaveError::BeforeCommit(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(staging_entries(temp.path()).is_empty());
    }

    #[test]
    fn directory_sync_failure_reports_candidate_present_and_rereads_disk() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runner]\npython = \"python3\"\n").unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "saved-python");
        let Baseline::Existing(baseline) = store.baseline.clone() else {
            panic!("expected an existing baseline");
        };
        let candidate = store.document().candidate().into_bytes();

        let error = store
            .save_existing_with(
                candidate,
                baseline,
                |file| file.sync_all(),
                safe_file::replace_file,
                |_| Err(io::Error::other("injected directory sync failure")),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            SettingsSaveError::PostCommit {
                state: PostCommitState::CandidatePresent,
                ..
            }
        ));
        assert!(fs::read_to_string(&path).unwrap().contains("saved-python"));
        assert_eq!(store.save().unwrap(), SaveOutcome::Unchanged);
        assert!(staging_entries(temp.path()).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_protected_attributes_survive_replacement() {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runner]\npython = \"python3\"\n").unwrap();
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        wide.push(0);
        assert_ne!(
            unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) },
            0
        );
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "python-custom");

        store.save().unwrap();

        assert_ne!(
            fs::metadata(&path).unwrap().file_attributes() & FILE_ATTRIBUTE_HIDDEN,
            0
        );
    }

    #[cfg(windows)]
    fn windows_dacl(path: &Path) -> Vec<u8> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, GetKernelObjectSecurity};

        let file = File::open(path).unwrap();
        let mut size = 0;
        unsafe {
            GetKernelObjectSecurity(
                file.as_raw_handle(),
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut size,
            );
        }
        assert!(size > 0, "could not measure the file DACL");
        let mut descriptor = vec![0_u8; size as usize];
        let success = unsafe {
            GetKernelObjectSecurity(
                file.as_raw_handle(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        };
        assert_ne!(success, 0, "{}", io::Error::last_os_error());
        descriptor.truncate(size as usize);
        descriptor
    }

    #[cfg(windows)]
    #[test]
    fn windows_custom_dacl_survives_replacement() {
        use std::process::Command;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runner]\npython = \"python3\"\n").unwrap();
        // Copy inherited ACEs to explicit ACEs. A new staging file retains the
        // directory's inherited ACL, so this distinguishes metadata merging.
        let output = Command::new("icacls")
            .arg(&path)
            .arg("/inheritance:d")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "icacls failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let staging = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
        assert_ne!(windows_dacl(&path), windows_dacl(staging.path()));
        let expected = windows_dacl(&path);
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "python-custom");

        store.save().unwrap();

        assert_eq!(windows_dacl(&path), expected);
    }

    #[cfg(unix)]
    #[test]
    fn unix_permissions_survive_replacement() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runner]\npython = \"python3\"\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "python-custom");

        store.save().unwrap();

        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
    }
}
