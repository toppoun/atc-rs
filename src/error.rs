use crate::atcoder::AtCoderError;
#[derive(Debug)]
pub enum AppError {
    AtCoder(AtCoderError),
    Io(std::io::Error),
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
