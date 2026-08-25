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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigValueSource {
    BuiltIn,
    UserOverride,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConfigSources {
    pub(crate) default_language: ConfigValueSource,
    pub(crate) python: ConfigValueSource,
    pub(crate) cpp_compiler: ConfigValueSource,
    pub(crate) cpp_flags: ConfigValueSource,
    pub(crate) timeout_seconds: ConfigValueSource,
    pub(crate) compile_timeout_seconds: ConfigValueSource,
}

impl Default for ConfigSources {
    fn default() -> Self {
        Self {
            default_language: ConfigValueSource::BuiltIn,
            python: ConfigValueSource::BuiltIn,
            cpp_compiler: ConfigValueSource::BuiltIn,
            cpp_flags: ConfigValueSource::BuiltIn,
            timeout_seconds: ConfigValueSource::BuiltIn,
            compile_timeout_seconds: ConfigValueSource::BuiltIn,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct ResolvedConfig {
    pub(crate) config: Config,
    pub(crate) file_exists: bool,
    pub(crate) sources: ConfigSources,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigOverrides {
    defaults: DefaultsOverrides,
    runner: RunnerOverrides,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DefaultsOverrides {
    #[serde(deserialize_with = "deserialize_optional_language")]
    language: Option<Language>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RunnerOverrides {
    python: Option<String>,
    cpp_compiler: Option<String>,
    cpp_flags: Option<Vec<String>>,
    timeout_seconds: Option<f64>,
    compile_timeout_seconds: Option<f64>,
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

fn deserialize_optional_language<'de, D>(deserializer: D) -> Result<Option<Language>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;

    value
        .map(|value| Language::from_str(&value).map_err(serde::de::Error::custom))
        .transpose()
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
        let Some(text) = read_optional_config(path)? else {
            return Ok(Self::default());
        };

        Self::parse(&text).map_err(|err| config_io_error(path, "parse and validate", err).into())
    }

    pub(crate) fn resolve_from(path: &Path) -> Result<ResolvedConfig, AppError> {
        let Some(text) = read_optional_config(path)? else {
            return Ok(ResolvedConfig {
                config: Self::default(),
                file_exists: false,
                sources: ConfigSources::default(),
            });
        };

        let (config, sources) = Self::parse_with_sources(&text)
            .map_err(|err| config_io_error(path, "parse and validate", err))?;

        Ok(ResolvedConfig {
            config,
            file_exists: true,
            sources,
        })
    }

    pub(crate) fn parse(contents: &str) -> io::Result<Self> {
        Self::parse_with_sources(contents).map(|(config, _)| config)
    }

    fn parse_with_sources(contents: &str) -> io::Result<(Self, ConfigSources)> {
        let overrides = toml::from_str::<ConfigOverrides>(contents).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to parse config: {err}"),
            )
        })?;

        let (config, sources) = overrides.resolve();

        config.validate()?;

        Ok((config, sources))
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

fn read_optional_config(path: &Path) -> Result<Option<String>, AppError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(config_io_error(path, "inspect", err).into()),
    }

    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|err| config_io_error(path, "read", err).into())
}

impl ConfigOverrides {
    fn resolve(self) -> (Config, ConfigSources) {
        let sources = ConfigSources {
            default_language: value_source(&self.defaults.language),
            python: value_source(&self.runner.python),
            cpp_compiler: value_source(&self.runner.cpp_compiler),
            cpp_flags: value_source(&self.runner.cpp_flags),
            timeout_seconds: value_source(&self.runner.timeout_seconds),
            compile_timeout_seconds: value_source(&self.runner.compile_timeout_seconds),
        };

        let runner_defaults = RunnerConfig::default();
        let config = Config {
            defaults: Defaults {
                language: self.defaults.language.unwrap_or_else(default_language),
            },
            runner: RunnerConfig {
                python: self.runner.python.unwrap_or(runner_defaults.python),
                cpp_compiler: self
                    .runner
                    .cpp_compiler
                    .unwrap_or(runner_defaults.cpp_compiler),
                cpp_flags: self.runner.cpp_flags.unwrap_or(runner_defaults.cpp_flags),
                timeout_seconds: self
                    .runner
                    .timeout_seconds
                    .unwrap_or(runner_defaults.timeout_seconds),
                compile_timeout_seconds: self
                    .runner
                    .compile_timeout_seconds
                    .unwrap_or(runner_defaults.compile_timeout_seconds),
            },
        };

        (config, sources)
    }
}

fn value_source<T>(value: &Option<T>) -> ConfigValueSource {
    if value.is_some() {
        ConfigValueSource::UserOverride
    } else {
        ConfigValueSource::BuiltIn
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
    fn resolved_config_tracks_explicit_fields_even_when_the_value_matches_the_builtin() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "defaults.language = \"cpp\"\n\
             runner.cpp_compiler = \"g++\"\n\
             runner.timeout_seconds = 2.0\n",
        )
        .unwrap();

        let resolved = Config::resolve_from(&path).unwrap();

        assert!(resolved.file_exists);
        assert_eq!(resolved.config, Config::default());
        assert_eq!(
            resolved.sources.default_language,
            ConfigValueSource::UserOverride
        );
        assert_eq!(
            resolved.sources.cpp_compiler,
            ConfigValueSource::UserOverride
        );
        assert_eq!(
            resolved.sources.timeout_seconds,
            ConfigValueSource::UserOverride
        );
        assert_eq!(resolved.sources.python, ConfigValueSource::BuiltIn);
        assert_eq!(resolved.sources.cpp_flags, ConfigValueSource::BuiltIn);
        assert_eq!(
            resolved.sources.compile_timeout_seconds,
            ConfigValueSource::BuiltIn
        );
    }

    #[test]
    fn normal_tables_and_dotted_keys_have_equivalent_values_and_provenance() {
        let temp = tempdir().unwrap();
        let table_path = temp.path().join("tables.toml");
        let dotted_path = temp.path().join("dotted.toml");
        std::fs::write(
            &table_path,
            "[defaults]\n\
             language = \"python\"\n\
             [runner]\n\
             python = \"python-custom\"\n\
             cpp_compiler = \"clang++\"\n\
             cpp_flags = [\"-O0\"]\n\
             timeout_seconds = 4.5\n\
             compile_timeout_seconds = 12.0\n",
        )
        .unwrap();
        std::fs::write(
            &dotted_path,
            "defaults.language = \"python\"\n\
             runner.python = \"python-custom\"\n\
             runner.cpp_compiler = \"clang++\"\n\
             runner.cpp_flags = [\"-O0\"]\n\
             runner.timeout_seconds = 4.5\n\
             runner.compile_timeout_seconds = 12.0\n",
        )
        .unwrap();

        let tables = Config::resolve_from(&table_path).unwrap();
        let dotted = Config::resolve_from(&dotted_path).unwrap();

        assert_eq!(tables.config, dotted.config);
        assert_eq!(tables.sources, dotted.sources);
        assert_eq!(
            tables.sources,
            ConfigSources {
                default_language: ConfigValueSource::UserOverride,
                python: ConfigValueSource::UserOverride,
                cpp_compiler: ConfigValueSource::UserOverride,
                cpp_flags: ConfigValueSource::UserOverride,
                timeout_seconds: ConfigValueSource::UserOverride,
                compile_timeout_seconds: ConfigValueSource::UserOverride,
            }
        );
    }

    #[test]
    fn partial_and_absent_overrides_keep_exact_defaults_and_sources() {
        let temp = tempdir().unwrap();
        let partial_path = temp.path().join("partial.toml");
        let absent_path = temp.path().join("absent.toml");
        std::fs::write(
            &partial_path,
            "[runner]\npython = \"python-custom\"\ncpp_flags = []\n",
        )
        .unwrap();
        std::fs::write(&absent_path, INITIAL_CONFIG).unwrap();

        let partial = Config::resolve_from(&partial_path).unwrap();
        assert_eq!(partial.config.defaults, Defaults::default());
        assert_eq!(partial.config.runner.python, "python-custom");
        assert!(partial.config.runner.cpp_flags.is_empty());
        assert_eq!(partial.config.runner.cpp_compiler, "g++");
        assert_eq!(partial.config.runner.timeout_seconds, 2.0);
        assert_eq!(
            partial.sources,
            ConfigSources {
                python: ConfigValueSource::UserOverride,
                cpp_flags: ConfigValueSource::UserOverride,
                ..ConfigSources::default()
            }
        );

        let absent = Config::resolve_from(&absent_path).unwrap();
        assert!(absent.file_exists);
        assert_eq!(absent.config, Config::default());
        assert_eq!(absent.sources, ConfigSources::default());

        let missing = Config::resolve_from(&temp.path().join("missing.toml")).unwrap();
        assert!(!missing.file_exists);
        assert_eq!(missing.config, Config::default());
        assert_eq!(missing.sources, ConfigSources::default());
    }

    #[test]
    fn resolved_config_rejects_invalid_values_and_unknown_fields_before_provenance() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.toml");

        for contents in [
            "runner.timeout_seconds = 0\n",
            "unknown = true\n",
            "[runner]\nunknown = true\n",
            "[defaults]\nlanguage = \"unsupported\"\n",
        ] {
            std::fs::write(&path, contents).unwrap();
            let error = Config::resolve_from(&path).unwrap_err();
            assert!(
                matches!(error, AppError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData),
                "accepted invalid config: {contents:?}"
            );
        }
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
