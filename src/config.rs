use crate::error::AppError;
use crate::language::Language;
use serde::Deserialize;
use std::io;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(
        default = "default_language",
        deserialize_with = "deserialize_language"
    )]
    pub language: Language,
}

fn default_language() -> Language {
    Language::Cpp
}

fn deserialize_language<'de, D>(deserializer: D) -> Result<Language, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;

    Language::from_str(&value).map_err(serde::de::Error::custom)
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            language: default_language(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, AppError> {
        let path = crate::paths::config_file()?;
        Self::load_from(&path)
    }

    fn load_from(path: &Path) -> Result<Self, AppError> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(config_io_error(path, "inspect", err).into()),
        }

        let text =
            std::fs::read_to_string(path).map_err(|err| config_io_error(path, "read", err))?;

        let config = toml::from_str::<Config>(&text).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to parse config {}: {err}", path.display()),
            )
        })?;

        Ok(config)
    }
}

fn config_io_error(path: &Path, action: &str, source: io::Error) -> io::Error {
    io::Error::new(
        source.kind(),
        format!("failed to {action} config {}: {source}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_config_uses_default() {
        let temp = tempdir().unwrap();
        let config = Config::load_from(&temp.path().join("config.toml")).unwrap();

        assert_eq!(config.defaults.language, Language::Cpp);
    }

    #[test]
    fn valid_config_loads_language_case_insensitively() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[defaults]\nlanguage = \"PyThOn\"\n").unwrap();

        let config = Config::load_from(&path).unwrap();

        assert_eq!(config.defaults.language, Language::Python);
    }

    #[test]
    fn omitted_language_uses_default() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[defaults]\n").unwrap();

        let config = Config::load_from(&path).unwrap();

        assert_eq!(config.defaults.language, Language::Cpp);
    }

    #[test]
    fn malformed_config_is_an_error() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[defaults\n").unwrap();

        let err = Config::load_from(&path).unwrap_err();

        assert!(matches!(err, AppError::Io(ref err) if err.kind() == io::ErrorKind::InvalidData));
    }

    #[test]
    fn unsupported_language_is_an_error() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[defaults]\nlanguage = \"py\"\n").unwrap();

        let err = Config::load_from(&path).unwrap_err();

        assert!(matches!(err, AppError::Io(ref err) if err.kind() == io::ErrorKind::InvalidData));
    }

    #[test]
    fn unknown_fields_are_errors() {
        let temp = tempdir().unwrap();
        let top_level = temp.path().join("top-level.toml");
        std::fs::write(&top_level, "unexpected = true\n").unwrap();
        let nested = temp.path().join("nested.toml");
        std::fs::write(
            &nested,
            "[defaults]\nlanguage = \"cpp\"\nunexpected = true\n",
        )
        .unwrap();

        for path in [top_level, nested] {
            let err = Config::load_from(&path).unwrap_err();
            assert!(
                matches!(err, AppError::Io(ref err) if err.kind() == io::ErrorKind::InvalidData)
            );
        }
    }

    #[test]
    fn existing_unreadable_config_path_does_not_use_default() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();

        assert!(Config::load_from(&path).is_err());
    }
}
