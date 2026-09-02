use etcetera::HomeDirError;

use crate::atcoder::AtCoderError;
use crate::atcoder::submit::SubmitError;
use crate::editor::EditorError;
use crate::stress::StressError;
use std::fmt;

#[derive(Debug)]
pub enum AppError {
    AtCoder(AtCoderError),
    Editor(EditorError),
    Io(std::io::Error),
    HomeDir(HomeDirError),
    Stress(StressError),
    Submit(SubmitError),
    UnknownSubmissionOutcome,
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AtCoder(error) => write!(formatter, "AtCoder operation failed: {error}"),
            Self::Editor(error) => write!(formatter, "editor operation failed: {error}"),
            Self::Io(error) => write!(formatter, "filesystem operation failed: {error}"),
            Self::HomeDir(error) => write!(formatter, "failed to resolve home directory: {error}"),
            Self::Stress(error) => write!(formatter, "stress failed: {error}"),
            Self::Submit(error) => match error {
                SubmitError::AuthenticationRequired => {
                    formatter.write_str("AtCoder authentication is required.")
                }
                SubmitError::SubmissionRejected => formatter.write_str(
                    "Submission was rejected by AtCoder.\nThe submit page may require browser verification.",
                ),
                SubmitError::RateLimited => {
                    formatter.write_str("Submission was rate limited by AtCoder.")
                }
                _ => write!(formatter, "AtCoder submit failed: {error}"),
            },
            Self::UnknownSubmissionOutcome => formatter.write_str(
                "Submission outcome is unknown.\nCheck My Submissions before retrying.",
            ),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AtCoder(error) => Some(error),
            Self::Editor(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::HomeDir(error) => Some(error),
            Self::Stress(error) => Some(error),
            Self::Submit(error) => Some(error),
            Self::UnknownSubmissionOutcome => None,
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

impl From<EditorError> for AppError {
    fn from(err: EditorError) -> Self {
        AppError::Editor(err)
    }
}

impl From<HomeDirError> for AppError {
    fn from(err: HomeDirError) -> Self {
        AppError::HomeDir(err)
    }
}

impl From<StressError> for AppError {
    fn from(err: StressError) -> Self {
        AppError::Stress(err)
    }
}

impl From<SubmitError> for AppError {
    fn from(err: SubmitError) -> Self {
        AppError::Submit(err)
    }
}
