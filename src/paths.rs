use etcetera::{BaseStrategy, choose_base_strategy};
use std::path::PathBuf;

pub fn config_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(choose_base_strategy()?.config_dir().join("atc"))
}

pub fn config_file() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(config_dir()?.join("config.toml"))
}
