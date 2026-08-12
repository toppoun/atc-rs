use crate::language::Language;

const CPP_TEMPLATE: &str = include_str!("../assets/templates/default.cpp");

const PYTHON_TEMPLATE: &str = include_str!("../assets/templates/default.py");

pub fn builtin_template(language: Language) -> &'static str {
    match language {
        Language::Cpp => CPP_TEMPLATE,
        Language::Python => PYTHON_TEMPLATE,
    }
}
