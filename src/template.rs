use crate::language::Language;

const CPP_TEMPLATE: &str = include_str!("../assets/templates/default.cpp");

const PYTHON_TEMPLATE: &str = include_str!("../assets/templates/default.py");

pub fn builtin_template(language: Language) -> &'static str {
    match language {
        Language::Cpp => CPP_TEMPLATE,
        Language::Python => PYTHON_TEMPLATE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_template_for_each_language() {
        let cpp = builtin_template(Language::Cpp);
        let python = builtin_template(Language::Python);

        assert!(cpp.contains("#include <bits/stdc++.h>"));
        assert!(python.contains("def main():"));
        assert!(cpp.ends_with('\n'));
        assert!(python.ends_with('\n'));
        assert_ne!(cpp, python);
    }
}
