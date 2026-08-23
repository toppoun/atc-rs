use crate::error::AppError;
use crate::language::Language;
use serde::Deserialize;
use std::io;
use std::path::Path;
use std::str::FromStr;

pub(crate) const INITIAL_CONFIG: &str = "# atc configuration\n\
#\n\
# Add only the settings you want to override.\n\
# See the configuration documentation for available settings.\n";

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,

    #[serde(default)]
    pub runner: RunnerConfig,
}

#[derive(Debug, Deserialize, PartialEq)]
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

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RunnerConfig {
    pub python: String,
    pub cpp_compiler: String,
    pub cpp_flags: Vec<String>,
    pub timeout_seconds: f64,
    pub compile_timeout_seconds: f64,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            python: "python".to_string(),
            cpp_compiler: "g++".to_string(),
            cpp_flags: vec![
                "-std=c++23".to_string(),
                "-O2".to_string(),
                "-Wall".to_string(),
                "-Wextra".to_string(),
            ],
            timeout_seconds: 2.0,
            compile_timeout_seconds: 10.0,
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

        Self::parse(&text).map_err(|err| config_io_error(path, "parse and validate", err).into())
    }

    pub(crate) fn parse(contents: &str) -> io::Result<Self> {
        let config = toml::from_str::<Config>(contents).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to parse config: {err}"),
            )
        })?;

        config.validate()?;

        Ok(config)
    }

    fn validate(&self) -> io::Result<()> {
        for (name, value) in [
            ("runner.timeout_seconds", self.runner.timeout_seconds),
            (
                "runner.compile_timeout_seconds",
                self.runner.compile_timeout_seconds,
            ),
        ] {
            let duration = std::time::Duration::try_from_secs_f64(value);
            if !value.is_finite()
                || value <= 0.0
                || !matches!(duration, Ok(duration) if !duration.is_zero())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{name} must be a positive finite duration"),
                ));
            }
        }

        if self.runner.python.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runner.python must not be empty",
            ));
        }

        if self.runner.cpp_compiler.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runner.cpp_compiler must not be empty",
            ));
        }

        Ok(())
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
    fn initial_comments_only_config_has_exact_bytes_and_effective_defaults() {
        assert_eq!(
            INITIAL_CONFIG.as_bytes(),
            b"# atc configuration\n\
#\n\
# Add only the settings you want to override.\n\
# See the configuration documentation for available settings.\n"
        );
        assert!(!INITIAL_CONFIG.as_bytes().contains(&b'\r'));
        assert_eq!(Config::parse(INITIAL_CONFIG).unwrap(), Config::default());
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
        let runner = temp.path().join("runner.toml");
        std::fs::write(&runner, "[runner]\nunexpected = true\n").unwrap();

        for path in [top_level, nested, runner] {
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

    #[test]
    fn partial_runner_config_uses_built_in_defaults() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[runner]\ntimeout_seconds = 3.5\n").unwrap();

        let config = Config::load_from(&path).unwrap();

        assert_eq!(config.runner.python, "python");
        assert_eq!(config.runner.cpp_compiler, "g++");
        assert_eq!(
            config.runner.cpp_flags,
            ["-std=c++23", "-O2", "-Wall", "-Wextra"]
        );
        assert_eq!(config.runner.timeout_seconds, 3.5);
        assert_eq!(config.runner.compile_timeout_seconds, 10.0);
    }

    #[test]
    fn dotted_and_table_overrides_parse_equivalently() {
        let dotted =
            Config::parse("defaults.language = \"python\"\nrunner.timeout_seconds = 3.0\n")
                .unwrap();
        let tables =
            Config::parse("[defaults]\nlanguage = \"python\"\n[runner]\ntimeout_seconds = 3.0\n")
                .unwrap();

        assert_eq!(dotted, tables);
        assert_eq!(dotted.defaults.language, Language::Python);
        assert_eq!(dotted.runner.timeout_seconds, 3.0);
        assert_eq!(dotted.runner.python, RunnerConfig::default().python);
    }

    #[test]
    fn runner_timeouts_must_be_positive_finite_and_representable() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");

        for field in ["timeout_seconds", "compile_timeout_seconds"] {
            for value in ["0", "-1", "nan", "inf", "-inf", "1e-300", "1e300"] {
                std::fs::write(&path, format!("[runner]\n{field} = {value}\n")).unwrap();

                let error = Config::load_from(&path).unwrap_err();

                assert!(
                    matches!(error, AppError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData),
                    "accepted {field} = {value}"
                );
            }
        }
    }

    #[test]
    fn runner_program_names_must_not_be_empty() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");

        for (field, value) in [("python", ""), ("cpp_compiler", "   ")] {
            std::fs::write(&path, format!("[runner]\n{field} = {value:?}\n")).unwrap();

            let error = Config::load_from(&path).unwrap_err();

            assert!(matches!(
                error,
                AppError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData
            ));
        }
    }
}
