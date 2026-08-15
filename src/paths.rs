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

pub fn debug_include_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(cache_dir()?.join("include"))
}

pub fn state_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    let strategy = choose_base_strategy()?;

    Ok(match strategy.state_dir() {
        Some(path) => path.join("atc"),
        None => strategy.data_dir().join("atc").join("state"),
    })
}

pub fn cookie_file() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(state_dir()?.join("cookie"))
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

    #[test]
    fn debug_include_directory_is_under_atc_cache_directory() {
        let cache = cache_dir().unwrap();
        let include = debug_include_dir().unwrap();

        assert!(cache.is_absolute());
        assert_eq!(cache.file_name().unwrap(), "atc");
        assert_eq!(include, cache.join("include"));
    }
    #[test]
    fn cookie_file_is_under_state_directory() {
        let state = state_dir().unwrap();
        let cookie = cookie_file().unwrap();

        assert!(state.is_absolute());
        assert_eq!(cookie, state.join("cookie"));
    }
}
