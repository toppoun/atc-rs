use crate::error::AppError;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

pub(crate) enum OptionalUtf8File {
    Missing,
    Present(String),
}

pub(crate) fn optional_directory_exists(path: &Path, kind: &str) -> Result<bool, AppError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(contextual_io_error(
                error,
                format!("failed to inspect {kind}: {}", path.display()),
            )
            .into());
        }
    }

    let metadata = fs::metadata(path).map_err(|error| {
        contextual_io_error(
            error,
            format!("failed to follow {kind}: {}", path.display()),
        )
    })?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} must resolve to a directory: {}", path.display()),
        )
        .into());
    }

    Ok(true)
}

pub(crate) fn ensure_directory(path: &Path, kind: &str) -> Result<(), AppError> {
    ensure_directory_with(path, kind, &mut |path| fs::create_dir_all(path))
}

fn ensure_directory_with(
    path: &Path,
    kind: &str,
    create: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> Result<(), AppError> {
    if optional_directory_exists(path, kind)? {
        return Ok(());
    }

    if let Err(create_error) = create(path) {
        match optional_directory_exists(path, kind) {
            Ok(true) => return Ok(()),
            Err(validation_error) => return Err(validation_error),
            Ok(false) => {
                return Err(contextual_io_error(
                    create_error,
                    format!("failed to create {kind}: {}", path.display()),
                )
                .into());
            }
        }
    }

    if optional_directory_exists(path, kind)? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{kind} disappeared after creation: {}", path.display()),
        )
        .into())
    }
}

pub(crate) fn read_optional_utf8_file(
    path: &Path,
    kind: &str,
) -> Result<OptionalUtf8File, AppError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(OptionalUtf8File::Missing);
        }
        Err(error) => {
            return Err(contextual_io_error(
                error,
                format!("failed to inspect {kind}: {}", path.display()),
            )
            .into());
        }
    }

    let metadata = fs::metadata(path).map_err(|error| {
        contextual_io_error(
            error,
            format!("failed to follow {kind}: {}", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} must resolve to a regular file: {}", path.display()),
        )
        .into());
    }

    fs::read_to_string(path)
        .map(OptionalUtf8File::Present)
        .map_err(|error| {
            contextual_io_error(
                error,
                format!("failed to read {kind} as UTF-8: {}", path.display()),
            )
        })
        .map_err(AppError::from)
}

fn contextual_io_error(source: io::Error, context: String) -> io::Error {
    let kind = source.kind();
    io::Error::new(kind, ContextualIoError { context, source })
}

#[derive(Debug)]
struct ContextualIoError {
    context: String,
    source: io::Error,
}

impl fmt::Display for ContextualIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.context, self.source)
    }
}

impl std::error::Error for ContextualIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn directory_creation_race_accepts_a_directory_winner() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("atc");
        let mut create = |path: &Path| {
            fs::create_dir(path).unwrap();
            Err(io::ErrorKind::AlreadyExists.into())
        };

        ensure_directory_with(&directory, "test directory", &mut create).unwrap();

        assert!(directory.is_dir());
    }

    #[test]
    fn directory_creation_race_accepts_a_link_to_directory_winner() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("managed");
        let directory = temp.path().join("atc");
        fs::create_dir(&target).unwrap();
        let probe = temp.path().join("symlink-probe");
        if !create_directory_symlink(&target, &probe) {
            return;
        }
        fs::remove_dir(&probe).unwrap();
        let mut create = |path: &Path| {
            assert!(create_directory_symlink(&target, path));
            Err(io::ErrorKind::AlreadyExists.into())
        };

        ensure_directory_with(&directory, "test directory", &mut create).unwrap();

        assert!(
            fs::symlink_metadata(&directory)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(directory.is_dir());
    }

    #[test]
    fn directory_creation_race_rejects_and_preserves_a_wrong_type_winner() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("atc");
        let mut create = |path: &Path| {
            fs::write(path, b"race winner").unwrap();
            Err(io::ErrorKind::AlreadyExists.into())
        };

        let error = ensure_directory_with(&directory, "test directory", &mut create).unwrap_err();

        assert!(error.to_string().contains("must resolve to a directory"));
        assert_eq!(fs::read(directory).unwrap(), b"race winner");
    }
}
