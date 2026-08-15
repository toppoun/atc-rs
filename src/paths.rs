use etcetera::{BaseStrategy, choose_base_strategy};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CookieLocation {
    pub(crate) platform_base: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) file: PathBuf,
}

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

#[cfg(test)]
pub fn state_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(cookie_location()?.state_dir)
}

pub fn cookie_file() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(cookie_location()?.file)
}

pub(crate) fn cookie_location() -> Result<CookieLocation, etcetera::HomeDirError> {
    let strategy = choose_base_strategy()?;
    let (platform_base, state_dir) = match strategy.state_dir() {
        Some(base) => {
            let state_dir = base.join("atc");
            (base, state_dir)
        }
        None => {
            let base = strategy.data_dir();
            let state_dir = base.join("atc").join("state");
            (base, state_dir)
        }
    };
    let file = state_dir.join("cookie");

    Ok(CookieLocation {
        platform_base,
        state_dir,
        file,
    })
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
        let location = cookie_location().unwrap();
        let state = state_dir().unwrap();
        let cookie = cookie_file().unwrap();

        assert!(state.is_absolute());
        assert_eq!(cookie, state.join("cookie"));
        assert_eq!(location.state_dir, state);
        assert_eq!(location.file, cookie);
        assert!(state.starts_with(&location.platform_base));

        #[cfg(windows)]
        assert_eq!(
            state.strip_prefix(&location.platform_base).unwrap(),
            std::path::Path::new("atc").join("state")
        );
    }
}
