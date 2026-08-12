use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Cpp,
    Python,
}

impl Language {
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
}
