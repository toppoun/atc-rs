use crate::error::AppError;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const DEBUG_HPP: &str = include_str!("../assets/debug.hpp");

pub fn materialize_debug_header() -> Result<PathBuf, AppError> {
    let include_dir = crate::paths::debug_include_dir()?;
    materialize_debug_header_at(&include_dir)?;
    Ok(include_dir)
}

#[cfg(test)]
pub(crate) fn materialize_debug_header_in(cache_dir: &Path) -> io::Result<PathBuf> {
    let include_dir = cache_dir.join("include");
    materialize_debug_header_at(&include_dir)?;
    Ok(include_dir)
}

fn materialize_debug_header_at(include_dir: &Path) -> io::Result<()> {
    let cache_dir = include_dir.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "debug include directory has no parent: {}",
                include_dir.display()
            ),
        )
    })?;
    ensure_real_directory(cache_dir, "debug cache directory")?;

    ensure_real_directory(include_dir, "debug include directory")?;

    let atc_dir = include_dir.join("atc");
    ensure_real_directory(&atc_dir, "debug header directory")?;

    let header = atc_dir.join("debug.hpp");
    match fs::symlink_metadata(&header) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "debug header path is not a regular file: {}",
                    header.display()
                ),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Write beside the destination and atomically replace it, so an interrupted update cannot
    // leave a truncated header in the shared cache.
    let mut temporary = tempfile::NamedTempFile::new_in(&atc_dir)?;
    temporary.write_all(DEBUG_HPP.as_bytes())?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(&header).map_err(|error| error.error)?;

    Ok(())
}

fn ensure_real_directory(path: &Path, kind: &str) -> io::Result<()> {
    fs::create_dir_all(path)?;

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{kind} is not a real directory: {}", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_embedded_header_at_expected_cache_path() {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path().join("cache with spaces");

        let include_dir = materialize_debug_header_in(&cache_dir).unwrap();
        let header = include_dir.join("atc").join("debug.hpp");

        assert_eq!(include_dir, cache_dir.join("include"));
        assert_eq!(fs::read(&header).unwrap(), DEBUG_HPP.as_bytes());
    }

    #[test]
    fn atomically_replaces_stale_regular_header() {
        let temp = tempfile::tempdir().unwrap();
        let header = temp.path().join("include").join("atc").join("debug.hpp");
        fs::create_dir_all(header.parent().unwrap()).unwrap();
        fs::write(&header, "stale").unwrap();

        materialize_debug_header_in(temp.path()).unwrap();

        assert_eq!(fs::read(header).unwrap(), DEBUG_HPP.as_bytes());
    }

    #[test]
    fn rejects_non_directory_cache_components_and_header_directory() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("include"), "not a directory").unwrap();
        let error = materialize_debug_header_in(temp.path()).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::AlreadyExists
                | io::ErrorKind::NotADirectory
                | io::ErrorKind::InvalidInput
        ));

        let second = tempfile::tempdir().unwrap();
        let header = second.path().join("include").join("atc").join("debug.hpp");
        fs::create_dir_all(&header).unwrap();
        let error = materialize_debug_header_in(second.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
