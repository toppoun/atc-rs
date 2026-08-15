use crate::paths;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};

pub fn save_cookie(cookie: &str) -> io::Result<()> {
    let cookie = cookie.trim();

    if cookie.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cookie must not be empty",
        ));
    }

    let path = paths::cookie_file().map_err(io::Error::other)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cookie path has no parent directory",
        )
    })?;

    fs::create_dir_all(parent)?;

    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cookie path is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;

    file.write_all(cookie.as_bytes())?;

    Ok(())
}

pub fn load_cookie() -> io::Result<Option<String>> {
    let path = paths::cookie_file().map_err(io::Error::other)?;

    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cookie path is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    }

    let cookie = fs::read_to_string(path)?;

    if cookie.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stored cookie is empty",
        ));
    }

    Ok(Some(cookie.trim().to_string()))
}
