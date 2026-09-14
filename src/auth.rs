use crate::paths::{self, CookieLocation};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, PathBuf};
use std::sync::Arc;

const SESSION_COOKIE_PREFIX: &str = "REVEL_SESSION=";
// RFC 6265 user agents are expected to support at least 4096 bytes per
// cookie. AtCoder's session cookie is much smaller; this cap also prevents an
// accidentally large file from being read into memory or used as a header.
const MAX_COOKIE_LINE_BYTES: usize = 4096;

pub(crate) struct Credential {
    value: String,
}

impl Credential {
    pub(crate) fn as_str(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credential(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthLoadErrorKind {
    InvalidFormat,
    UnsafeFilesystemState,
    Permission,
    Io,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthLoadError {
    kind: AuthLoadErrorKind,
}

impl AuthLoadError {
    pub(crate) fn from_io(error: &io::Error) -> Self {
        let kind = match error.kind() {
            io::ErrorKind::InvalidData => AuthLoadErrorKind::InvalidFormat,
            io::ErrorKind::InvalidInput => AuthLoadErrorKind::UnsafeFilesystemState,
            io::ErrorKind::PermissionDenied => AuthLoadErrorKind::Permission,
            _ => AuthLoadErrorKind::Io,
        };
        Self { kind }
    }

    #[cfg(test)]
    pub(crate) fn kind(self) -> AuthLoadErrorKind {
        self.kind
    }
}

impl fmt::Display for AuthLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            AuthLoadErrorKind::InvalidFormat => "authentication cookie format is invalid",
            AuthLoadErrorKind::UnsafeFilesystemState => {
                "authentication cookie filesystem state is unsafe"
            }
            AuthLoadErrorKind::Permission => "authentication cookie permissions are unsafe",
            AuthLoadErrorKind::Io => "authentication cookie could not be read safely",
        })
    }
}

impl std::error::Error for AuthLoadError {}

pub(crate) enum AuthSnapshot {
    Configured(Arc<Credential>),
    Missing,
    Invalid(AuthLoadError),
}

impl fmt::Debug for AuthSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configured(credential) => formatter
                .debug_tuple("Configured")
                .field(credential)
                .finish(),
            Self::Missing => formatter.write_str("Missing"),
            Self::Invalid(error) => formatter.debug_tuple("Invalid").field(error).finish(),
        }
    }
}

impl AuthSnapshot {
    pub(crate) fn load() -> Arc<Self> {
        let snapshot = match paths::cookie_location() {
            Ok(location) => load_auth_snapshot_from(&location),
            Err(error) => Self::Invalid(AuthLoadError::from_io(&io::Error::other(error))),
        };
        Arc::new(snapshot)
    }

    pub(crate) fn credential(&self) -> Option<&Credential> {
        match self {
            Self::Configured(credential) => Some(credential),
            Self::Missing | Self::Invalid(_) => None,
        }
    }

    pub(crate) fn submission_unavailable_message(&self) -> Option<&'static str> {
        match self {
            Self::Configured(_) => None,
            Self::Missing => {
                Some("Authentication is not configured.\nReturn Home to configure authentication.")
            }
            Self::Invalid(_) => {
                Some("Authentication configuration is invalid.\nReturn Home to fix authentication.")
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn configured_for_test(value: &str) -> Arc<Self> {
        Arc::new(Self::Configured(Arc::new(Credential {
            value: value.to_string(),
        })))
    }
}

fn load_auth_snapshot_from(location: &CookieLocation) -> AuthSnapshot {
    match load_credential_from(location) {
        Ok(Some(credential)) => AuthSnapshot::Configured(Arc::new(credential)),
        Ok(None) => AuthSnapshot::Missing,
        Err(error) => AuthSnapshot::Invalid(AuthLoadError::from_io(&error)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CookieFileState {
    Missing,
    Existing,
}

/// Inspect cookie state without reading credential contents.
///
/// The file is opened with the same path-hierarchy, no-follow, regular-file, and permission checks
/// used by authentication. This is intended for status and setup guidance only: the result does
/// not make a later pathname reopen safe or guarantee that it would refer to the inspected object.
pub(crate) fn inspect_cookie_file(location: &CookieLocation) -> io::Result<CookieFileState> {
    match open_validated_cookie_file(location)? {
        Some(_) => Ok(CookieFileState::Existing),
        None => Ok(CookieFileState::Missing),
    }
}

fn load_credential_from(location: &CookieLocation) -> io::Result<Option<Credential>> {
    let Some(file) = open_validated_cookie_file(location)? else {
        return Ok(None);
    };

    let mut cookie = String::new();
    file.take((MAX_COOKIE_LINE_BYTES + 3) as u64)
        .read_to_string(&mut cookie)?;

    Ok(Some(parse_cookie_file(&cookie)?))
}

fn parse_cookie_file(contents: &str) -> io::Result<Credential> {
    if contents.len() > MAX_COOKIE_LINE_BYTES + 2 {
        return Err(invalid_cookie_file_error());
    }

    let cookie = contents
        .strip_suffix("\r\n")
        .or_else(|| contents.strip_suffix('\n'))
        .unwrap_or(contents);

    if cookie.len() > MAX_COOKIE_LINE_BYTES || cookie.contains('\r') || cookie.contains('\n') {
        return Err(invalid_cookie_file_error());
    }

    let Some(value) = cookie.strip_prefix(SESSION_COOKIE_PREFIX) else {
        return Err(invalid_cookie_file_error());
    };

    if value.is_empty() {
        return Err(invalid_cookie_file_error());
    }

    if !value.bytes().all(is_cookie_octet) {
        return Err(invalid_cookie_file_error());
    }

    Ok(Credential {
        value: cookie.to_string(),
    })
}

fn is_cookie_octet(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'..=b'+' | b'-'..=b':' | b'<'..=b'[' | b']'..=b'~'
    )
}

fn invalid_cookie_file_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "authentication cookie must have the form REVEL_SESSION=<value>",
    )
}

fn validate_cookie_location(location: &CookieLocation) -> io::Result<()> {
    if !location.platform_base.is_absolute()
        || !location.state_dir.is_absolute()
        || !location.file.is_absolute()
        || location.file.parent() != Some(location.state_dir.as_path())
        || location.file.file_name() != Some(std::ffi::OsStr::new("cookie"))
    {
        return Err(unsafe_path_error("cookie location is invalid"));
    }

    Ok(())
}

fn open_cookie_file(location: &CookieLocation) -> io::Result<Option<fs::File>> {
    open_cookie_file_with(location, || {})
}

fn open_validated_cookie_file(location: &CookieLocation) -> io::Result<Option<fs::File>> {
    validate_cookie_location(location)?;
    let Some(file) = open_cookie_file(location)? else {
        return Ok(None);
    };

    if !file.metadata()?.file_type().is_file() {
        return Err(unsafe_path_error("cookie path is not a regular file"));
    }
    validate_cookie_file_permissions(&file)?;
    Ok(Some(file))
}

fn open_cookie_file_with(
    location: &CookieLocation,
    before_cookie_open: impl FnOnce(),
) -> io::Result<Option<fs::File>> {
    // Validate the complete lexical relationship before filesystem existence
    // can turn an invalid location into a misleading NotConfigured result.
    let state_directories = application_state_directories(location)?;

    // The platform base may itself be redirected by the operating system, so
    // it is opened with ambient authority. Every atc-rs-owned descendant is
    // then opened relative to a pinned directory handle without following the
    // final component. This removes the metadata/open race for both ancestor
    // directories and the cookie file.
    let mut directory = match Dir::open_ambient_dir(&location.platform_base, ambient_authority()) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    for path in state_directories {
        let component = path
            .file_name()
            .ok_or_else(|| unsafe_path_error("cookie state path is invalid"))?;
        directory = match directory.open_dir_nofollow(component) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
    }

    before_cookie_open();

    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);

    match directory.open_with("cookie", &options) {
        Ok(file) => Ok(Some(file.into_std())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn validate_cookie_file_permissions(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if file.metadata()?.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "authentication cookie must not be accessible by group or other users (use chmod 600)",
        ));
    }

    Ok(())
}

#[cfg(not(unix))]
fn validate_cookie_file_permissions(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

fn application_state_directories(location: &CookieLocation) -> io::Result<Vec<PathBuf>> {
    let relative = location
        .state_dir
        .strip_prefix(&location.platform_base)
        .map_err(|_| unsafe_path_error("cookie state path is outside its platform base"))?;
    let mut current = location.platform_base.clone();
    let mut directories = Vec::new();

    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(unsafe_path_error("cookie state path is invalid"));
        };
        current.push(component);
        directories.push(current.clone());
    }

    if directories.last() != Some(&location.state_dir) {
        return Err(unsafe_path_error("cookie state path is invalid"));
    }

    Ok(directories)
}

fn unsafe_path_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::Path;

    fn location(root: &Path) -> CookieLocation {
        let platform_base = root.join("platform-state");
        let state_dir = platform_base.join("atc").join("state");
        let file = state_dir.join("cookie");
        CookieLocation {
            platform_base,
            state_dir,
            file,
        }
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

    fn create_directory_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(target, link);

        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("failed to create directory symlink: {error}"),
        }
    }

    fn write_cookie_file(path: &Path, contents: impl AsRef<[u8]>) {
        fs::write(path, contents).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn missing_cookie_is_the_only_anonymous_state() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        assert!(load_credential_from(&location).unwrap().is_none());
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Missing
        ));
        assert_eq!(
            inspect_cookie_file(&location).unwrap(),
            CookieFileState::Missing
        );

        fs::create_dir_all(&location.state_dir).unwrap();
        write_cookie_file(&location.file, " \r\n ");
        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Invalid(error)
                if error.kind() == AuthLoadErrorKind::InvalidFormat
        ));

        write_cookie_file(&location.file, [0xff]);
        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Invalid(error)
                if error.kind() == AuthLoadErrorKind::InvalidFormat
        ));

        write_cookie_file(&location.file, "value-only");
        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        write_cookie_file(&location.file, "REVEL_SESSION=secret\n");
        assert_eq!(
            load_credential_from(&location)
                .unwrap()
                .as_ref()
                .map(Credential::as_str),
            Some("REVEL_SESSION=secret")
        );
        assert_eq!(
            inspect_cookie_file(&location).unwrap(),
            CookieFileState::Existing
        );
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Configured(_)
        ));
    }

    #[test]
    fn credential_and_auth_snapshot_debug_are_redacted() {
        let marker = "REVEL_SESSION=distinctive-auth-secret-7f2c";
        let snapshot = AuthSnapshot::Configured(Arc::new(Credential {
            value: marker.to_string(),
        }));
        let credential_debug = format!("{:?}", snapshot.credential().unwrap());
        let snapshot_debug = format!("{snapshot:?}");

        assert_eq!(credential_debug, "Credential(<redacted>)");
        assert!(!credential_debug.contains(marker));
        assert!(!snapshot_debug.contains(marker));
        assert!(snapshot_debug.contains("Configured"));

        let invalid = AuthSnapshot::Invalid(AuthLoadError {
            kind: AuthLoadErrorKind::InvalidFormat,
        });
        assert!(!format!("{invalid:?}").contains(marker));
        assert!(
            !invalid
                .submission_unavailable_message()
                .unwrap()
                .contains(marker)
        );
    }

    #[test]
    fn relative_cookie_location_is_rejected() {
        let location = CookieLocation {
            platform_base: PathBuf::from("relative-platform-state"),
            state_dir: PathBuf::from("relative-platform-state/atc/state"),
            file: PathBuf::from("relative-platform-state/atc/state/cookie"),
        };

        assert!(load_credential_from(&location).is_err());
    }

    #[test]
    fn cookie_location_must_stay_beneath_its_platform_base() {
        let temp = tempfile::tempdir().unwrap();
        let platform_base = temp.path().join("platform-state");

        for state_dir in [
            temp.path().join("outside-state"),
            platform_base.clone(),
            platform_base.join("atc").join("..").join("state"),
        ] {
            let location = CookieLocation {
                platform_base: platform_base.clone(),
                file: state_dir.join("cookie"),
                state_dir,
            };

            assert!(load_credential_from(&location).is_err());
            assert!(inspect_cookie_file(&location).is_err());
        }
    }

    #[test]
    fn cookie_symlink_and_directory_are_rejected_without_touching_the_target() {
        let symlink_root = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        fs::write(external.path(), "external secret").unwrap();
        let symlink_location = location(symlink_root.path());
        fs::create_dir_all(&symlink_location.state_dir).unwrap();
        if !create_file_symlink(external.path(), &symlink_location.file) {
            return;
        }

        assert!(load_credential_from(&symlink_location).is_err());
        assert!(matches!(
            load_auth_snapshot_from(&symlink_location),
            AuthSnapshot::Invalid(_)
        ));
        assert!(inspect_cookie_file(&symlink_location).is_err());
        assert_eq!(
            fs::read_to_string(external.path()).unwrap(),
            "external secret"
        );

        let directory_root = tempfile::tempdir().unwrap();
        let directory_location = location(directory_root.path());
        fs::create_dir_all(&directory_location.file).unwrap();
        assert!(load_credential_from(&directory_location).is_err());
        assert!(inspect_cookie_file(&directory_location).is_err());
    }

    #[test]
    fn symlinked_application_state_directory_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(location.state_dir.parent().unwrap()).unwrap();
        if !create_directory_symlink(external.path(), &location.state_dir) {
            return;
        }

        assert!(load_credential_from(&location).is_err());
        assert!(inspect_cookie_file(&location).is_err());
        assert!(!external.path().join("cookie").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_cookie_is_rejected_as_a_special_file() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        let _listener = UnixListener::bind(&location.file).unwrap();

        assert!(load_credential_from(&location).is_err());
        assert!(inspect_cookie_file(&location).is_err());
    }

    #[test]
    fn cookie_value_can_contain_percent() {
        assert_eq!(
            parse_cookie_file("REVEL_SESSION=secret%value")
                .unwrap()
                .as_str(),
            "REVEL_SESSION=secret%value"
        );
    }

    #[test]
    fn cookie_file_requires_exact_revel_session_format() {
        assert_eq!(
            parse_cookie_file("REVEL_SESSION=secret").unwrap().as_str(),
            "REVEL_SESSION=secret"
        );

        assert_eq!(
            parse_cookie_file("REVEL_SESSION=secret\n")
                .unwrap()
                .as_str(),
            "REVEL_SESSION=secret"
        );

        assert_eq!(
            parse_cookie_file("REVEL_SESSION=secret\r\n")
                .unwrap()
                .as_str(),
            "REVEL_SESSION=secret"
        );

        for invalid in [
            "",
            "secret",
            "REVEL_SESSION=",
            "OTHER_COOKIE=secret",
            "REVEL_SESSION=secret\r",
            "REVEL_SESSION=secret\n\n",
            "REVEL_SESSION=secret\r\n\r\n",
            "REVEL_SESSION=secret\nOTHER_COOKIE=value",
            "REVEL_SESSION=secret; OTHER_COOKIE=value",
            "REVEL_SESSION=secret value",
            "REVEL_SESSION=secret\tvalue",
            "REVEL_SESSION=secret\0value",
            "REVEL_SESSION=secret\u{7f}value",
            "REVEL_SESSION=secret,OTHER_COOKIE=value",
            "REVEL_SESSION=\"secret\"",
            "REVEL_SESSION=secret\\value",
            "REVEL_SESSION=sécret",
        ] {
            assert_eq!(
                parse_cookie_file(invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn cookie_file_size_is_bounded() {
        let max_value = "x".repeat(MAX_COOKIE_LINE_BYTES - SESSION_COOKIE_PREFIX.len());
        let max_cookie = format!("{SESSION_COOKIE_PREFIX}{max_value}");

        assert_eq!(parse_cookie_file(&max_cookie).unwrap().as_str(), max_cookie);
        assert!(parse_cookie_file(&format!("{max_cookie}\r\n")).is_ok());
        assert_eq!(
            parse_cookie_file(&format!("{max_cookie}x"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        write_cookie_file(&location.file, format!("{max_cookie}x"));
        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn cookie_symlink_swap_immediately_before_open_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        write_cookie_file(&location.file, "REVEL_SESSION=original");
        write_cookie_file(external.path(), "REVEL_SESSION=external");
        let swapped = Cell::new(false);

        let result = open_cookie_file_with(&location, || {
            fs::remove_file(&location.file).unwrap();
            swapped.set(create_file_symlink(external.path(), &location.file));
        });

        if !swapped.get() {
            return;
        }
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn opened_state_directory_is_not_redirected_by_a_later_symlink_swap() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        write_cookie_file(&location.file, "REVEL_SESSION=original");
        write_cookie_file(&external.path().join("cookie"), "REVEL_SESSION=external");
        let moved_state = location.state_dir.with_file_name("original-state");

        let mut file = open_cookie_file_with(&location, || {
            fs::rename(&location.state_dir, &moved_state).unwrap();
            assert!(create_directory_symlink(
                external.path(),
                &location.state_dir
            ));
        })
        .unwrap()
        .unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();

        assert_eq!(contents, "REVEL_SESSION=original");
    }

    #[cfg(unix)]
    #[test]
    fn group_or_other_readable_cookie_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        fs::write(&location.file, "REVEL_SESSION=secret").unwrap();
        fs::set_permissions(&location.file, fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Invalid(error) if error.kind() == AuthLoadErrorKind::Permission
        ));
        assert_eq!(
            inspect_cookie_file(&location).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
