use std::ffi::OsStr;
use std::fs::{self, File, TryLockError};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};

use super::{DirectoryIdentity, RegistryLocation, RegistryLockError, RegistryReadError};

const REGISTRY_FILE: &str = RegistryLocation::FILE_NAME;
const LOCK_FILE: &str = RegistryLocation::LOCK_NAME;
const NEW_STAGING_PREFIX: &str = ".workspaces-staging-new-";
const REPLACE_STAGING_PREFIX: &str = ".workspaces-staging-replace-";
pub(super) const MAX_REGISTRY_BYTES: u64 = 16 * 1024 * 1024;

pub(super) struct StateDirectory {
    dir: Dir,
    identity: DirectoryIdentity,
    #[cfg(windows)]
    path: std::path::PathBuf,
    #[cfg(windows)]
    _guards: Vec<File>,
}

impl StateDirectory {
    pub(super) fn open(
        location: &RegistryLocation,
        create: bool,
    ) -> Result<Option<Self>, RegistryReadError> {
        let base = location.platform_base();
        match fs::metadata(base) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(RegistryReadError::UnsafeType("state base"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::symlink_metadata(base) {
                    Ok(_) => return Err(RegistryReadError::UnsafeType("state base")),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(read_io("state base", error)),
                }
                if !create {
                    return Ok(None);
                }
                fs::create_dir_all(base).map_err(|error| read_io("state base", error))?;
            }
            Err(error) => return Err(read_io("state base", error)),
        }
        // The OS/user can redirect the platform base. All atc-owned descendants
        // are opened one component at a time without following links.
        let mut dir = Dir::open_ambient_dir(base, ambient_authority())
            .map_err(|error| read_io("state base", error))?;
        #[cfg(windows)]
        let (mut path, mut guards) = {
            let path = fs::canonicalize(base).map_err(|error| read_io("state base", error))?;
            let guard = guard_windows_directory(&dir, &path)?;
            (path, vec![guard])
        };
        for &name in RegistryLocation::STATE_COMPONENTS {
            match dir.symlink_metadata(name) {
                Ok(metadata) if !metadata.is_dir() || is_cap_link(&metadata) => {
                    return Err(RegistryReadError::UnsafeType("state directory"));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !create {
                        return Ok(None);
                    }
                    #[cfg(unix)]
                    let created = {
                        use cap_std::fs::{DirBuilder, DirBuilderExt as _};
                        let mut builder = DirBuilder::new();
                        builder.mode(0o700);
                        dir.create_dir_with(name, &builder)
                    };
                    #[cfg(windows)]
                    let created = dir.create_dir(name);
                    match created {
                        Ok(()) => sync_directory(&dir)
                            .map_err(|error| read_io("state directory", error))?,
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(read_io("state directory", error)),
                    }
                }
                Err(error) => return Err(read_io("state directory", error)),
            }
            let child = dir
                .open_dir_nofollow(name)
                .map_err(|error| read_io("state directory", error))?;
            let metadata = child
                .dir_metadata()
                .map_err(|error| read_io("state directory", error))?;
            if !metadata.is_dir() || is_cap_link(&metadata) {
                return Err(RegistryReadError::UnsafeType("state directory"));
            }
            #[cfg(unix)]
            {
                use cap_std::fs::MetadataExt as _;
                if metadata.mode() & 0o022 != 0 {
                    return Err(read_io(
                        "state directory",
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "state directory is writable by group or others",
                        ),
                    ));
                }
            }
            #[cfg(windows)]
            {
                path.push(name);
                guards.push(guard_windows_directory(&child, &path)?);
            }
            dir = child;
        }
        let identity = directory_id(&dir).map_err(|error| read_io("state directory", error))?;
        Ok(Some(Self {
            dir,
            identity,
            #[cfg(windows)]
            path,
            #[cfg(windows)]
            _guards: guards,
        }))
    }

    pub(super) fn is_current(
        &self,
        location: &RegistryLocation,
    ) -> Result<bool, RegistryReadError> {
        Ok(Self::open(location, false)?.is_some_and(|current| self.identity == current.identity))
    }

    pub(super) fn check_lock_type(&self) -> Result<(), RegistryReadError> {
        self.check_regular(LOCK_FILE).map(|_| ())
    }

    pub(super) fn lock(&self, timeout: Duration) -> Result<File, RegistryLockError> {
        self.check_lock_type().map_err(RegistryLockError::Open)?;
        let mut options = regular_options();
        options.write(true).create(true);
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
        let file = self
            .dir
            .open_with(LOCK_FILE, &options)
            .map_err(|error| RegistryLockError::Open(read_io(LOCK_FILE, error)))?
            .into_std();
        let _metadata = regular_metadata(&file, LOCK_FILE).map_err(RegistryLockError::Open)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if _metadata.nlink() != 1 || _metadata.mode() & 0o022 != 0 {
                return Err(RegistryLockError::Open(RegistryReadError::UnsafeType(
                    LOCK_FILE,
                )));
            }
        }
        #[cfg(windows)]
        if windows_link_count(&file).map_err(RegistryLockError::Acquire)? != 1 {
            return Err(RegistryLockError::Open(RegistryReadError::UnsafeType(
                LOCK_FILE,
            )));
        }
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::Error(error)) => return Err(RegistryLockError::Acquire(error)),
                Err(TryLockError::WouldBlock) => {
                    let remaining = timeout.saturating_sub(start.elapsed());
                    if remaining.is_zero() {
                        return Err(RegistryLockError::TimedOut);
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
            }
        }
    }

    pub(super) fn lock_is_current(&self, file: &File) -> Result<bool, RegistryReadError> {
        let identity =
            DirectoryIdentity::of_handle(file).map_err(|error| read_io(LOCK_FILE, error))?;
        self.name_matches(LOCK_FILE.as_ref(), identity)
    }

    pub(super) fn read_snapshot(&self) -> Result<Option<FileSnapshot>, RegistryReadError> {
        let Some(mut file) = self.open_regular(REGISTRY_FILE.as_ref())? else {
            return Ok(None);
        };
        let before = regular_metadata(&file, REGISTRY_FILE)?;
        let identity =
            DirectoryIdentity::of_handle(&file).map_err(|error| read_io(REGISTRY_FILE, error))?;
        let bytes = read_bytes(&mut file)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| read_io(REGISTRY_FILE, error))?;
        if bytes != read_bytes(&mut file)? {
            return Err(RegistryReadError::ChangedDuringRead);
        }
        #[cfg(target_os = "macos")]
        let captured = crate::safe_file::macos_metadata::metadata_contract(&before, &file);
        #[cfg(not(target_os = "macos"))]
        let captured = FileMetadata::read(&before, &file);
        let metadata = captured.map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted {
                RegistryReadError::ChangedDuringRead
            } else {
                read_io(REGISTRY_FILE, error)
            }
        })?;
        Ok(Some(FileSnapshot {
            bytes,
            identity,
            metadata,
            file,
        }))
    }

    pub(super) fn read_baseline(&self) -> Result<Option<FileSnapshot>, RegistryReadError> {
        if let Some(snapshot) = self.read_snapshot()? {
            return Ok(Some(snapshot));
        }
        let entries = self
            .dir
            .entries()
            .map_err(|error| read_io("state directory", error))?;
        for entry in entries {
            let entry = entry.map_err(|error| read_io("state directory", error))?;
            if entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(REPLACE_STAGING_PREFIX.as_bytes())
            {
                // An interrupted replace can leave the Registry name missing.
                // Keep the evidence and prevent an empty generation from reusing IDs.
                return Err(RegistryReadError::RecoveryRequired);
            }
        }
        Ok(None)
    }

    fn check_regular(&self, name: impl AsRef<OsStr>) -> Result<bool, RegistryReadError> {
        match self.dir.symlink_metadata(Path::new(name.as_ref())) {
            Ok(metadata) if metadata.is_file() && !is_cap_link(&metadata) => Ok(true),
            Ok(_) => Err(RegistryReadError::UnsafeType("Registry or lock file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(read_io("Registry or lock file", error)),
        }
    }

    fn open_regular(&self, name: &OsStr) -> Result<Option<File>, RegistryReadError> {
        if !self.check_regular(name)? {
            return Ok(None);
        }
        let file = match self.dir.open_with(Path::new(name), &regular_options()) {
            Ok(file) => file.into_std(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(read_io("Registry or lock file", error)),
        };
        regular_metadata(&file, "Registry or lock file")?;
        Ok(Some(file))
    }

    fn name_matches(
        &self,
        name: &OsStr,
        identity: DirectoryIdentity,
    ) -> Result<bool, RegistryReadError> {
        let Some(current) = self.open_regular(name)? else {
            return Ok(false);
        };
        DirectoryIdentity::of_handle(&current)
            .map(|current| current == identity)
            .map_err(|error| read_io("Registry file identity", error))
    }

    pub(super) fn stage(&self, replacing: bool) -> io::Result<(StagedFile<'_>, File)> {
        let mut options = regular_options();
        options.write(true).create_new(true);
        let staging = tempfile::Builder::new()
            .prefix(if replacing {
                REPLACE_STAGING_PREFIX
            } else {
                NEW_STAGING_PREFIX
            })
            .disable_cleanup(true)
            .make_in(".", |path| {
                self.dir
                    .open_with(file_name(path), &options)
                    .map(cap_std::fs::File::into_std)
            })?;
        let (file, path) = staging.into_parts();
        let identity = DirectoryIdentity::of_handle(&file)?;
        let staged = StagedFile {
            state: self,
            name: file_name(&path).to_owned(),
            identity,
            cleanup_on_drop: true,
        };
        Ok((staged, file))
    }

    pub(super) fn sync(&self) -> io::Result<()> {
        sync_directory(&self.dir)
    }
}

fn file_name(path: &Path) -> &OsStr {
    path.file_name()
        .expect("tempfile generates a final file name")
}

pub(super) struct StagedFile<'a> {
    state: &'a StateDirectory,
    name: std::ffi::OsString,
    pub(super) identity: DirectoryIdentity,
    cleanup_on_drop: bool,
}

impl StagedFile<'_> {
    pub(super) fn is_current(&self) -> Result<bool, RegistryReadError> {
        self.state.name_matches(&self.name, self.identity)
    }

    pub(super) fn preserve(&mut self) {
        self.cleanup_on_drop = false;
    }

    pub(super) fn publish(&self, replace: bool) -> io::Result<()> {
        #[cfg(unix)]
        {
            let dir = &self.state.dir;
            if replace {
                dir.rename(&self.name, dir, REGISTRY_FILE)
            } else {
                // Link publication fails atomically if the destination already exists.
                dir.hard_link(&self.name, dir, REGISTRY_FILE)?;
                Ok(())
            }
        }
        #[cfg(windows)]
        {
            use crate::safe_file;
            let source = self.state.path.join(&self.name);
            let destination = self.state.path.join(REGISTRY_FILE);
            if replace {
                safe_file::replace_file(&source, &destination)
            } else {
                safe_file::rename_noclobber_platform(&source, &destination)
            }
        }
    }

    pub(super) fn cleanup(&self) -> io::Result<()> {
        if self.is_current().map_err(io::Error::other)? {
            self.state.dir.remove_file(&self.name)?;
        }
        Ok(())
    }
}

impl Drop for StagedFile<'_> {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = self.cleanup();
        }
    }
}

pub(super) struct FileSnapshot {
    pub(super) bytes: Vec<u8>,
    pub(super) identity: DirectoryIdentity,
    pub(super) metadata: FileMetadata,
    // Keep the baseline file open so its identity cannot be reused during comparison.
    pub(super) file: File,
}

impl FileSnapshot {
    pub(super) fn matches(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.bytes == other.bytes
            && self.metadata == other.metadata
    }
}

#[cfg(target_os = "macos")]
pub(super) use crate::safe_file::macos_metadata::MetadataContract as FileMetadata;

#[cfg(not(target_os = "macos"))]
#[derive(PartialEq, Eq)]
pub(super) struct FileMetadata {
    #[cfg(windows)]
    attributes: u32,
    #[cfg(windows)]
    dacl: Vec<u32>,
    #[cfg(unix)]
    mode: u32,
}

#[cfg(not(target_os = "macos"))]
impl FileMetadata {
    fn read(before: &fs::Metadata, file: &File) -> io::Result<Self> {
        #[cfg(windows)]
        {
            let attributes = crate::safe_file::protected_windows_attributes(before);
            let dacl = read_dacl(file)?;
            if dacl != read_dacl(file)?
                || attributes != crate::safe_file::protected_windows_attributes(&file.metadata()?)
            {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Registry attributes changed during read",
                ));
            }
            Ok(Self { attributes, dacl })
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let mode = before.mode();
            if mode != file.metadata()?.mode() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Registry permissions changed during read",
                ));
            }
            Ok(Self { mode })
        }
    }
}

pub(super) fn preserve_metadata(baseline: &FileSnapshot, staged: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use crate::safe_file::macos_metadata;
        macos_metadata::preserve_metadata(&baseline.file, staged)?;
        if macos_metadata::metadata_contract(&staged.metadata()?, staged)? != baseline.metadata {
            return Err(io::Error::other(
                "staged Registry metadata does not match baseline",
            ));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    staged.set_permissions(baseline.file.metadata()?.permissions())?;
    #[cfg(windows)]
    {
        // ReplaceFileW merges the existing DACL and file attributes at publication.
        let _ = (&baseline.file, staged);
    }
    Ok(())
}

fn read_bytes(file: &mut File) -> Result<Vec<u8>, RegistryReadError> {
    let mut bytes = Vec::new();
    file.take(MAX_REGISTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| read_io(REGISTRY_FILE, error))?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(RegistryReadError::TooLarge);
    }
    Ok(bytes)
}

fn regular_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
}

fn regular_metadata(file: &File, target: &'static str) -> Result<fs::Metadata, RegistryReadError> {
    let metadata = file.metadata().map_err(|error| read_io(target, error))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(RegistryReadError::UnsafeType(target));
        }
    }
    if !metadata.is_file() {
        return Err(RegistryReadError::UnsafeType(target));
    }
    Ok(metadata)
}

fn is_cap_link(metadata: &cap_std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn directory_id(dir: &Dir) -> io::Result<DirectoryIdentity> {
    DirectoryIdentity::from_open_directory(&dir.try_clone()?.into_std_file())
}

#[cfg(unix)]
fn sync_directory(dir: &Dir) -> io::Result<()> {
    dir.open(".")?.into_std().sync_all()
}

#[cfg(windows)]
fn sync_directory(_dir: &Dir) -> io::Result<()> {
    // The staged file is synced before publication. Windows has no equivalent
    // directory sync in the existing atomic-replacement contract.
    Ok(())
}

#[cfg(windows)]
pub(super) fn stage_identity_guard(file: File) -> io::Result<File> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReOpenFile,
    };

    // ReplaceFileW opens the replacement without sharing data access. An
    // attributes-only handle keeps its identity live without blocking that open.
    // SAFETY: the source handle is live; a successful ReOpenFile returns an owned handle.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ReOpenFile returned a valid, independently owned handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(windows)]
fn guard_windows_directory(dir: &Dir, path: &Path) -> Result<File, RegistryReadError> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    // ReplaceFileW takes names, so also hold the resolved directory chain open
    // without delete sharing through the commit and its verification.
    let guard = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| read_io("state directory", error))?;
    let actual = DirectoryIdentity::from_open_directory(&guard)
        .map_err(|error| read_io("state directory", error))?;
    if actual != directory_id(dir).map_err(|error| read_io("state directory", error))? {
        return Err(RegistryReadError::ChangedDuringRead);
    }
    Ok(guard)
}

#[cfg(windows)]
fn windows_link_count(file: &File) -> io::Result<u32> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file is open and info is a valid output buffer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks)
}

#[cfg(windows)]
fn read_dacl(file: &File) -> io::Result<Vec<u32>> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, GetKernelObjectSecurity};
    let mut size = 0;
    // SAFETY: a zero-size request reports the required buffer length in size.
    let result = unsafe {
        GetKernelObjectSecurity(
            file.as_raw_handle(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut size,
        )
    };
    let error = io::Error::last_os_error();
    if result == 0 && error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(error);
    }
    if result != 0 || size == 0 || size > 128 * 1024 {
        return Err(io::Error::other("invalid Registry DACL length"));
    }
    let mut descriptor = vec![0_u32; (size as usize).div_ceil(size_of::<u32>())];
    let mut written = 0;
    // SAFETY: descriptor is aligned and has at least size writable bytes.
    if unsafe {
        GetKernelObjectSecurity(
            file.as_raw_handle(),
            DACL_SECURITY_INFORMATION,
            descriptor.as_mut_ptr().cast(),
            size,
            &mut written,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if written != size {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Registry DACL changed during read",
        ));
    }
    Ok(descriptor)
}

pub(super) fn read_io(target: &'static str, source: io::Error) -> RegistryReadError {
    RegistryReadError::Io { target, source }
}
