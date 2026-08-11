use crate::atcoder::AtCoderError;
use std::fmt;

#[derive(Debug)]
pub enum AppError {
    AtCoder(AtCoderError),
    Io(std::io::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AtCoder(error) => write!(formatter, "AtCoder operation failed: {error}"),
            Self::Io(error) => write!(formatter, "filesystem operation failed: {error}"),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AtCoder(error) => Some(error),
            Self::Io(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        AppError::Io(err)
    }
}

impl From<AtCoderError> for AppError {
    fn from(err: AtCoderError) -> Self {
        AppError::AtCoder(err)
    }
}
