use crate::error::AppError;
use crate::language::Language;
use crate::user_config_fs::{self, OptionalUtf8File};
use std::path::Path;

const CPP_TEMPLATE: &str = include_str!("../assets/templates/default.cpp");

const PYTHON_TEMPLATE: &str = include_str!("../assets/templates/default.py");

const STRESS_GENERATOR_TEMPLATE: &str = include_str!("../assets/templates/stress_gen.py");

const STRESS_BRUTE_TEMPLATE: &str = include_str!("../assets/templates/stress_brute.py");

pub fn builtin_template(language: Language) -> &'static str {
    match language {
        Language::Cpp => CPP_TEMPLATE,
        Language::Python => PYTHON_TEMPLATE,
    }
}

pub(crate) fn source_template_filename(language: Language) -> &'static str {
    match language {
        Language::Cpp => "cpp.cpp",
        Language::Python => "python.py",
    }
}

pub(crate) fn resolve_source_template(language: Language) -> Result<String, AppError> {
    let templates_dir = crate::paths::source_templates_dir()?;
    resolve_source_template_in(&templates_dir, language)
}

pub(crate) fn resolve_source_template_in(
    templates_dir: &Path,
    language: Language,
) -> Result<String, AppError> {
    if !user_config_fs::optional_directory_exists(templates_dir, "source template directory")? {
        return Ok(builtin_template(language).to_owned());
    }

    let template_path = templates_dir.join(source_template_filename(language));
    match user_config_fs::read_optional_utf8_file(&template_path, "source template")? {
        OptionalUtf8File::Missing => Ok(builtin_template(language).to_owned()),
        OptionalUtf8File::Present(contents) => Ok(contents),
    }
}

pub(crate) fn stress_generator_template() -> &'static str {
    STRESS_GENERATOR_TEMPLATE
}

pub(crate) fn stress_brute_template() -> &'static str {
    STRESS_BRUTE_TEMPLATE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io};

    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(target, link);

        symlink_created_or_unsupported(result, "file")
    }

    fn create_directory_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(target, link);

        symlink_created_or_unsupported(result, "directory")
    }

    fn symlink_created_or_unsupported(result: io::Result<()>, kind: &str) -> bool {
        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("failed to create {kind} symlink: {error}"),
        }
    }

    const EXPECTED_STRESS_GENERATOR: &str = concat!(
        "import random\n",
        "import string\n",
        "import sys\n",
        "\n",
        "\n",
        "def ni(lo: int, hi: int) -> int:\n",
        "    return random.randint(lo, hi)\n",
        "\n",
        "def nl(amount: int, lo: int, hi: int) -> str:\n",
        "    return \" \".join(str(ni(lo, hi)) for _ in range(amount))\n",
        "\n",
        "def si(length: int = 1, chars: str = string.ascii_lowercase) -> str:\n",
        "    return \"\".join(random.choice(chars) for _ in range(length))\n",
        "\n",
        "\n",
        "def main() -> None:\n",
        "    seed = int(sys.argv[1])\n",
        "    random.seed(seed)\n",
        "\n",
        "    # TODO: generate one test case\n",
        "    n = ni(2, 8)\n",
        "    a = nl(n, 2, 8)\n",
        "\n",
        "    print(n)\n",
        "    print(a)\n",
        "\n",
        "\n",
        "if __name__ == \"__main__\":\n",
        "    main()\n",
    );

    const EXPECTED_STRESS_BRUTE: &str = concat!(
        "import sys\n",
        "\n",
        "input = sys.stdin.readline\n",
        "\n",
        "\n",
        "def ni() -> int:\n",
        "    return int(input())\n",
        "\n",
        "def nm():\n",
        "    return map(int, input().split())\n",
        "\n",
        "def nl() -> list[int]:\n",
        "    return list(nm())\n",
        "\n",
        "def si() -> str:\n",
        "    return input().strip()\n",
        "\n",
        "\n",
        "def brute() -> None:\n",
        "    # TODO: implement a simple correct solution\n",
        "    n = ni()\n",
        "    a = nl()\n",
        "\n",
        "    # ...\n",
        "\n",
        "    print()\n",
        "\n",
        "\n",
        "if __name__ == \"__main__\":\n",
        "    brute()\n",
    );

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

    #[test]
    fn source_template_filenames_are_conventional_and_language_specific() {
        assert_eq!(source_template_filename(Language::Cpp), "cpp.cpp");
        assert_eq!(source_template_filename(Language::Python), "python.py");
    }

    #[test]
    fn missing_templates_directory_uses_exact_builtins_without_creating_state() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("config");
        let templates_dir = config_dir.join("templates");

        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Cpp).unwrap(),
            builtin_template(Language::Cpp)
        );
        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Python).unwrap(),
            builtin_template(Language::Python)
        );

        assert!(!config_dir.exists());
        assert!(!config_dir.join("config.toml").exists());
        assert!(!templates_dir.exists());
        assert!(!templates_dir.join("cpp.cpp").exists());
        assert!(!templates_dir.join("python.py").exists());
    }

    #[test]
    fn missing_selected_file_uses_exact_builtin() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();

        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Cpp).unwrap(),
            builtin_template(Language::Cpp)
        );
    }

    #[test]
    fn regular_utf8_files_are_returned_exactly_including_empty_files() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        fs::write(templates_dir.join("cpp.cpp"), "// custom C++\n").unwrap();
        fs::write(templates_dir.join("python.py"), "print('custom')\n").unwrap();

        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Cpp).unwrap(),
            "// custom C++\n"
        );
        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Python).unwrap(),
            "print('custom')\n"
        );

        fs::write(templates_dir.join("cpp.cpp"), "").unwrap();
        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Cpp).unwrap(),
            ""
        );
    }

    #[test]
    fn language_overrides_are_independent() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        fs::write(templates_dir.join("cpp.cpp"), "cpp custom").unwrap();

        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Cpp).unwrap(),
            "cpp custom"
        );
        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Python).unwrap(),
            builtin_template(Language::Python)
        );

        fs::create_dir(templates_dir.join("python.py")).unwrap();
        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Cpp).unwrap(),
            "cpp custom"
        );

        fs::remove_dir(templates_dir.join("python.py")).unwrap();
        fs::remove_file(templates_dir.join("cpp.cpp")).unwrap();
        fs::create_dir(templates_dir.join("cpp.cpp")).unwrap();
        fs::write(templates_dir.join("python.py"), "python custom").unwrap();
        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Python).unwrap(),
            "python custom"
        );
    }

    #[test]
    fn selected_directory_and_invalid_utf8_are_errors() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let selected = templates_dir.join("cpp.cpp");
        fs::create_dir(&selected).unwrap();

        let error = resolve_source_template_in(&templates_dir, Language::Cpp).unwrap_err();
        assert!(error.to_string().contains("regular file"));
        assert!(error.to_string().contains(&selected.display().to_string()));

        fs::remove_dir(&selected).unwrap();
        fs::write(&selected, [0xff, 0xfe]).unwrap();
        let error = resolve_source_template_in(&templates_dir, Language::Cpp).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
        assert!(error.to_string().contains(&selected.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn selected_special_object_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let selected = templates_dir.join("cpp.cpp");
        let _socket = std::os::unix::net::UnixListener::bind(&selected).unwrap();

        let error = resolve_source_template_in(&templates_dir, Language::Cpp).unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[test]
    fn valid_selected_file_symlink_is_followed() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let target = temp.path().join("custom.cpp");
        fs::write(&target, "linked custom").unwrap();
        if !create_file_symlink(&target, &templates_dir.join("cpp.cpp")) {
            return;
        }

        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Cpp).unwrap(),
            "linked custom"
        );
    }

    #[test]
    fn dangling_selected_file_symlink_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        if !create_file_symlink(
            &temp.path().join("missing.cpp"),
            &templates_dir.join("cpp.cpp"),
        ) {
            return;
        }

        let error = resolve_source_template_in(&templates_dir, Language::Cpp).unwrap_err();
        assert!(error.to_string().contains("follow source template"));
    }

    #[test]
    fn selected_symlink_to_directory_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        let target = temp.path().join("directory-target");
        fs::create_dir(&templates_dir).unwrap();
        fs::create_dir(&target).unwrap();
        if !create_directory_symlink(&target, &templates_dir.join("cpp.cpp")) {
            return;
        }

        let error = resolve_source_template_in(&templates_dir, Language::Cpp).unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[test]
    fn valid_templates_directory_symlink_is_followed() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("template-target");
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("python.py"), "linked python").unwrap();
        if !create_directory_symlink(&target, &templates_dir) {
            return;
        }

        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Python).unwrap(),
            "linked python"
        );
    }

    #[test]
    fn dangling_templates_directory_symlink_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        if !create_directory_symlink(&temp.path().join("missing"), &templates_dir) {
            return;
        }

        let error = resolve_source_template_in(&templates_dir, Language::Cpp).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("follow source template directory")
        );
    }

    #[test]
    fn templates_path_must_resolve_to_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::write(&templates_dir, "not a directory").unwrap();

        let error = resolve_source_template_in(&templates_dir, Language::Cpp).unwrap_err();
        assert!(error.to_string().contains("must resolve to a directory"));
        assert!(
            error
                .to_string()
                .contains(&templates_dir.display().to_string())
        );
    }

    #[test]
    fn symlinked_parent_config_directory_is_supported() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config-target");
        let linked_config = temp.path().join("config-link");
        fs::create_dir(&target).unwrap();
        fs::create_dir(target.join("templates")).unwrap();
        fs::write(target.join("templates").join("cpp.cpp"), "parent linked").unwrap();
        if !create_directory_symlink(&target, &linked_config) {
            return;
        }

        assert_eq!(
            resolve_source_template_in(&linked_config.join("templates"), Language::Cpp).unwrap(),
            "parent linked"
        );
    }

    #[test]
    fn embeds_the_exact_protocol_stress_templates() {
        let generator = stress_generator_template();
        let brute = stress_brute_template();

        assert_eq!(generator.as_bytes(), EXPECTED_STRESS_GENERATOR.as_bytes());
        assert_eq!(brute.as_bytes(), EXPECTED_STRESS_BRUTE.as_bytes());

        for template in [generator, brute] {
            assert!(template.ends_with('\n'));
            assert!(!template.ends_with("\n\n"));
            assert!(!template.as_bytes().contains(&b'\r'));
        }

        assert!(generator.contains("sys.argv[1]"));
    }
}
