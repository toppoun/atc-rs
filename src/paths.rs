use etcetera::{BaseStrategy, choose_base_strategy};
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CookieLocation {
    pub(crate) platform_base: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryLocation {
    platform_base: PathBuf,
}

impl RegistryLocation {
    pub(crate) const FILE_NAME: &str = "workspaces.toml";
    pub(crate) const LOCK_NAME: &str = ".workspaces.lock";
    #[cfg(windows)]
    pub(crate) const STATE_COMPONENTS: &[&str] = &["atc", "state"];
    #[cfg(not(windows))]
    pub(crate) const STATE_COMPONENTS: &[&str] = &["atc"];

    pub(crate) fn from_platform_base(platform_base: PathBuf) -> io::Result<Self> {
        if !platform_base.is_absolute()
            || platform_base.as_os_str().as_encoded_bytes().contains(&0)
            || platform_base
                .components()
                .any(|part| part == Component::ParentDir)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Workspace Registry state base",
            ));
        }
        #[cfg(windows)]
        {
            use std::path::Prefix;
            if !matches!(platform_base.components().next(), Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_) | Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _)))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid Windows Registry state namespace",
                ));
            }
        }
        Ok(Self { platform_base })
    }

    pub(crate) fn platform_base(&self) -> &Path {
        &self.platform_base
    }

    /// Display and diagnostics path. Use the store for I/O.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn file(&self) -> PathBuf {
        let mut path = self.platform_base.clone();
        path.extend(Self::STATE_COMPONENTS);
        path.join(Self::FILE_NAME)
    }
}

#[cfg_attr(not(test), allow(dead_code))] // The frontend connects in a later phase.
pub(crate) fn registry_location() -> io::Result<RegistryLocation> {
    #[cfg(windows)]
    let base = choose_base_strategy()
        .map_err(io::Error::other)?
        .cache_dir();
    #[cfg(not(windows))]
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => etcetera::home_dir()
            .map_err(io::Error::other)?
            .join(".local")
            .join("state"),
    };
    RegistryLocation::from_platform_base(base)
}

pub fn config_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(choose_base_strategy()?.config_dir().join("atc"))
}

pub fn config_file() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(config_dir()?.join("config.toml"))
}

pub(crate) fn source_templates_dir() -> Result<PathBuf, AppError> {
    Ok(config_dir()?.join("templates"))
}

pub fn cache_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(choose_base_strategy()?.cache_dir().join("atc"))
}

pub fn debug_include_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(cache_dir()?.join("include"))
}

pub(crate) fn submission_tracking_dir() -> Result<PathBuf, etcetera::HomeDirError> {
    Ok(cache_dir()?.join("submission-tracking"))
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
    fn registry_base_rejects_ambiguous_and_invalid_locations() {
        #[cfg(windows)]
        let base = Path::new(r"C:\state");
        #[cfg(not(windows))]
        let base = Path::new("/state");
        for path in [
            PathBuf::new(),
            "relative".into(),
            base.join("../other"),
            base.join("bad\0path"),
        ] {
            assert_eq!(
                RegistryLocation::from_platform_base(path)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        #[cfg(windows)]
        assert_eq!(
            RegistryLocation::from_platform_base(r"\\.\pipe\state".into())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            RegistryLocation::from_platform_base(base.to_owned())
                .unwrap()
                .platform_base(),
            base
        );
    }

    #[test]
    fn config_file_is_under_atc_config_directory() {
        let directory = config_dir().unwrap();
        let file = config_file().unwrap();
        let templates = source_templates_dir().unwrap();

        assert!(directory.is_absolute());
        assert_eq!(directory.file_name().unwrap(), "atc");
        assert_eq!(file, directory.join("config.toml"));
        assert_eq!(templates, directory.join("templates"));
    }

    #[test]
    fn derived_directories_are_under_atc_cache_directory() {
        let cache = cache_dir().unwrap();
        let include = debug_include_dir().unwrap();
        let submission_tracking = submission_tracking_dir().unwrap();

        assert!(cache.is_absolute());
        assert_eq!(cache.file_name().unwrap(), "atc");
        assert_eq!(include, cache.join("include"));
        assert_eq!(submission_tracking, cache.join("submission-tracking"));
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
