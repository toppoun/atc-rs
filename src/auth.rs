use crate::paths::{self, CookieLocation};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

pub fn save_cookie(cookie: &str) -> io::Result<PathBuf> {
    if cookie.trim().is_empty() {
        return Err(empty_cookie_error());
    }

    let location = paths::cookie_location().map_err(io::Error::other)?;
    save_cookie_to(&location, cookie)
}

pub fn load_cookie() -> io::Result<Option<String>> {
    let location = paths::cookie_location().map_err(io::Error::other)?;
    load_cookie_from(&location)
}

fn save_cookie_to(location: &CookieLocation, cookie: &str) -> io::Result<PathBuf> {
    let cookie = cookie.trim();
    if cookie.is_empty() {
        return Err(empty_cookie_error());
    }

    validate_cookie_location(location)?;
    ensure_application_state_directory(location)?;
    validate_cookie_file_type(&location.file)?;

    // Write a new private file and atomically replace the directory entry. This
    // avoids truncating a path that may have become a symlink after inspection.
    let mut staged = tempfile::Builder::new()
        .prefix(".cookie-")
        .tempfile_in(&location.state_dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    staged.write_all(cookie.as_bytes())?;
    staged.flush()?;
    staged
        .persist(&location.file)
        .map_err(|error| error.error)?;

    Ok(location.file.clone())
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
    if cookie.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stored cookie is empty",
        ));
    }

    Ok(Some(cookie.trim().to_string()))
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

fn ensure_application_state_directory(location: &CookieLocation) -> io::Result<()> {
    // The platform base is selected by etcetera and may legitimately be a
    // redirected platform directory. Only atc-rs-owned descendants are required
    // to be real directories rather than symlinks.
    fs::create_dir_all(&location.platform_base)?;

    for directory in application_state_directories(location)? {
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        validate_real_directory(&directory)?;
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

fn validate_real_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(unsafe_path_error(
            "cookie state path is not a real directory",
        )),
        Err(error) => Err(error),
    }
}

fn validate_cookie_file_type(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(unsafe_path_error("cookie path is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn unsafe_path_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn empty_cookie_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "cookie must not be empty")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn save_and_load_round_trip_replaces_the_existing_cookie() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());

        assert_eq!(
            save_cookie_to(&location, "  REVEL_SESSION=first  ").unwrap(),
            location.file
        );
        assert_eq!(
            load_cookie_from(&location).unwrap().as_deref(),
            Some("REVEL_SESSION=first")
        );

        save_cookie_to(&location, "REVEL_SESSION=second").unwrap();
        assert_eq!(
            load_cookie_from(&location).unwrap().as_deref(),
            Some("REVEL_SESSION=second")
        );
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
    }

    #[test]
    fn empty_cookie_is_rejected_before_creating_state() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());

        assert_eq!(
            save_cookie_to(&location, " \r\n ").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!location.platform_base.exists());
    }

    #[test]
    fn relative_cookie_location_is_rejected() {
        let location = CookieLocation {
            platform_base: PathBuf::from("relative-platform-state"),
            state_dir: PathBuf::from("relative-platform-state/atc/state"),
            file: PathBuf::from("relative-platform-state/atc/state/cookie"),
        };

        assert!(load_cookie_from(&location).is_err());
        assert!(save_cookie_to(&location, "REVEL_SESSION=value").is_err());
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
        assert!(save_cookie_to(&symlink_location, "REVEL_SESSION=new").is_err());
        assert_eq!(
            fs::read_to_string(external.path()).unwrap(),
            "external secret"
        );

        let directory_root = tempfile::tempdir().unwrap();
        let directory_location = location(directory_root.path());
        fs::create_dir_all(&directory_location.file).unwrap();
        assert!(load_cookie_from(&directory_location).is_err());
        assert!(save_cookie_to(&directory_location, "REVEL_SESSION=new").is_err());
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
        assert!(save_cookie_to(&location, "REVEL_SESSION=new").is_err());
        assert!(!external.path().join("cookie").exists());
    }

    #[cfg(unix)]
    #[test]
    fn saved_cookie_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        save_cookie_to(&location, "REVEL_SESSION=private").unwrap();

        assert_eq!(
            fs::metadata(&location.file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
