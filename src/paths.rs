use etcetera::{BaseStrategy, choose_base_strategy};
use std::path::PathBuf;

pub fn config_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(choose_base_strategy()?.config_dir().join("atc"))
}

pub fn config_file() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn cache_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(choose_base_strategy()?.cache_dir().join("atc"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_is_under_atc_config_directory() {
        let directory = config_dir().unwrap();
        let file = config_file().unwrap();

        assert!(directory.is_absolute());
        assert_eq!(directory.file_name().unwrap(), "atc");
        assert_eq!(file, directory.join("config.toml"));
    }
}
