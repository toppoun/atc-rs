use std::str::FromStr;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Cpp,
    Python,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PythonRuntime {
    #[default]
    CPython,
    PyPy,
}

impl PythonRuntime {
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::CPython => "CPython",
            Self::PyPy => "PyPy",
        }
    }
}

impl FromStr for PythonRuntime {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.to_ascii_lowercase();

        match normalized.as_str() {
            "cpython" => Ok(Self::CPython),
            "pypy" => Ok(Self::PyPy),
            _ => Err(format!("unsupported Python runtime: {s}")),
        }
    }
}

impl<'de> Deserialize<'de> for PythonRuntime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmissionTarget {
    Cpp,
    Python(PythonRuntime),
}

impl Language {
    pub const ALL: [Self; 2] = [Self::Cpp, Self::Python];

    pub fn extension(self) -> &'static str {
        match self {
            Self::Cpp => "cpp",
            Self::Python => "py",
        }
    }
}

impl FromStr for Language {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.to_ascii_lowercase();

        match s.as_str() {
            "cpp" => Ok(Language::Cpp),
            "python" => Ok(Language::Python),
            _ => Err(format!("unsupported language: {s}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpp() {
        assert_eq!("cpp".parse::<Language>().unwrap(), Language::Cpp);
    }

    #[test]
    fn parsing_is_case_insensitive() {
        assert_eq!("CPP".parse::<Language>().unwrap(), Language::Cpp);
        assert_eq!("CpP".parse::<Language>().unwrap(), Language::Cpp);
        assert_eq!("Python".parse::<Language>().unwrap(), Language::Python);
        assert_eq!("PytHon".parse::<Language>().unwrap(), Language::Python);
    }

    #[test]
    fn rejects_unknown_language() {
        assert!("rust".parse::<Language>().is_err());
    }

    #[test]
    fn rejects_unsupported_aliases() {
        for alias in ["c++", "py", "python3", "pypy", " cpp "] {
            assert!(
                alias.parse::<Language>().is_err(),
                "accepted alias: {alias}"
            );
        }
    }

    #[test]
    fn returns_extension() {
        assert_eq!(Language::Cpp.extension(), "cpp");
        assert_eq!(Language::Python.extension(), "py");
    }

    #[test]
    fn all_contains_exactly_the_supported_normal_source_languages() {
        assert_eq!(Language::ALL, [Language::Cpp, Language::Python]);
    }

    #[test]
    fn parses_python_runtimes_without_exposing_atcoder_ids() {
        assert_eq!("cpython".parse(), Ok(PythonRuntime::CPython));
        assert_eq!("pypy".parse(), Ok(PythonRuntime::PyPy));
        assert_eq!("CPython".parse(), Ok(PythonRuntime::CPython));
        assert!("rust".parse::<PythonRuntime>().is_err());
    }
}
