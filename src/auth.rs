use crate::paths::{self, CookieLocation};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, PathBuf};

const SESSION_COOKIE_PREFIX: &str = "REVEL_SESSION=";

pub fn load_cookie() -> io::Result<Option<String>> {
    let location = paths::cookie_location().map_err(io::Error::other)?;
    load_cookie_from(&location)
}

fn load_cookie_from(location: &CookieLocation) -> io::Result<Option<String>> {
    validate_cookie_location(location)?;
    if !validate_application_state_directory(location)? {
        return Ok(None);
    }

    match fs::symlink_metadata(&location.file) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(unsafe_path_error("cookie path is not a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }

    // Checking the opened handle as well prevents a directory or other special
    // file from being accepted if the path changed after symlink_metadata.
    let mut file = match fs::File::open(&location.file) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.file_type().is_file() {
        return Err(unsafe_path_error("cookie path is not a regular file"));
    }

    let mut cookie = String::new();
    file.read_to_string(&mut cookie)?;

    Ok(Some(parse_cookie_file(&cookie)?))
}

fn parse_cookie_file(contents: &str) -> io::Result<String> {
    let cookie = contents
        .strip_suffix("\r\n")
        .or_else(|| contents.strip_suffix('\n'))
        .unwrap_or(contents);

    if cookie.contains('\r') || cookie.contains('\n') {
        return Err(invalid_cookie_file_error());
    }

    let Some(value) = cookie.strip_prefix(SESSION_COOKIE_PREFIX) else {
        return Err(invalid_cookie_file_error());
    };

    if value.is_empty() {
        return Err(invalid_cookie_file_error());
    }

    if value.contains(';') {
        return Err(invalid_cookie_file_error());
    }

    Ok(cookie.to_string())
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

fn validate_application_state_directory(location: &CookieLocation) -> io::Result<bool> {
    for directory in application_state_directories(location)? {
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(unsafe_path_error(
                    "cookie state path is not a real directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    }

    Ok(true)
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

    #[test]
    fn missing_cookie_is_the_only_anonymous_state() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        assert_eq!(load_cookie_from(&location).unwrap(), None);

        fs::create_dir_all(&location.state_dir).unwrap();
        fs::write(&location.file, " \r\n ").unwrap();
        assert_eq!(
            load_cookie_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        fs::write(&location.file, [0xff]).unwrap();
        assert_eq!(
            load_cookie_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        fs::write(&location.file, "value-only").unwrap();
        assert_eq!(
            load_cookie_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        fs::write(&location.file, "REVEL_SESSION=secret\n").unwrap();
        assert_eq!(
            load_cookie_from(&location).unwrap().as_deref(),
            Some("REVEL_SESSION=secret")
        );
    }

    #[test]
    fn relative_cookie_location_is_rejected() {
        let location = CookieLocation {
            platform_base: PathBuf::from("relative-platform-state"),
            state_dir: PathBuf::from("relative-platform-state/atc/state"),
            file: PathBuf::from("relative-platform-state/atc/state/cookie"),
        };

        assert!(load_cookie_from(&location).is_err());
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

        assert!(load_cookie_from(&symlink_location).is_err());
        assert_eq!(
            fs::read_to_string(external.path()).unwrap(),
            "external secret"
        );

        let directory_root = tempfile::tempdir().unwrap();
        let directory_location = location(directory_root.path());
        fs::create_dir_all(&directory_location.file).unwrap();
        assert!(load_cookie_from(&directory_location).is_err());
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

        assert!(load_cookie_from(&location).is_err());
        assert!(!external.path().join("cookie").exists());
    }

    #[test]
    fn cookie_file_requires_exact_revel_session_format() {
        assert_eq!(
            parse_cookie_file("REVEL_SESSION=secret").unwrap(),
            "REVEL_SESSION=secret"
        );

        assert_eq!(
            parse_cookie_file("REVEL_SESSION=secret\n").unwrap(),
            "REVEL_SESSION=secret"
        );

        assert_eq!(
            parse_cookie_file("REVEL_SESSION=secret\r\n").unwrap(),
            "REVEL_SESSION=secret"
        );

        for invalid in [
            "",
            "secret",
            "REVEL_SESSION=",
            "OTHER_COOKIE=secret",
            "REVEL_SESSION=secret\nOTHER_COOKIE=value",
            "REVEL_SESSION=secret; OTHER_COOKIE=value",
        ] {
            assert_eq!(
                parse_cookie_file(invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
