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
    if optional_directory_exists(path, kind)? {
        return Ok(());
    }

    if let Err(create_error) = fs::create_dir_all(path) {
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
