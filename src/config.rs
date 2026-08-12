use crate::AppError;
use crate::language::Language;
use serde::Deserialize;
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

        if !path.try_exists()? {
            return Ok(Self::default());
        }

        let text = std::fs::read_to_string(&path)?;

        let config = toml::from_str::<Config>(&text).map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to parse config {}: {err}", path.display()),
            )
        })?;

        Ok(config)
    }
}
