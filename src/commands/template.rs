use crate::error::AppError;
use crate::language::Language;
use crate::safe_file;
use crate::template::{builtin_template, source_template_path};
use crate::ui::{Event, Reporter};
use crate::user_config_fs::{self, OptionalUtf8File};
use std::io;
use std::path::{Path, PathBuf};

const TEMPLATE_INIT_TEMPORARY_PREFIX: &str = ".atc-template-init-";
const SOURCE_TEMPLATE_DIRECTORY_KIND: &str = "source template directory";
const SOURCE_TEMPLATE_KIND: &str = "source template";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TemplateFileState {
    Missing,
    Exists,
}

struct TemplateTarget {
    path: PathBuf,
    contents: &'static [u8],
}

pub(crate) fn template_init(
    language: Option<Language>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let templates_dir = crate::paths::source_templates_dir()?;

    match language {
        Some(language) => initialize_source_templates_at(
            &templates_dir,
            std::slice::from_ref(&language),
            reporter,
        ),
        None => initialize_source_templates_at(&templates_dir, &Language::ALL, reporter),
    }
}

pub(crate) fn initialize_source_templates_at(
    templates_dir: &Path,
    selected_languages: &[Language],
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    initialize_source_templates_at_with(
        templates_dir,
        selected_languages,
        reporter,
        &mut |path, contents| {
            safe_file::install_noclobber(path, contents, TEMPLATE_INIT_TEMPORARY_PREFIX)
        },
    )
}

fn initialize_source_templates_at_with(
    templates_dir: &Path,
    selected_languages: &[Language],
    reporter: &mut dyn Reporter,
    installer: &mut dyn FnMut(&Path, &[u8]) -> io::Result<()>,
) -> Result<(), AppError> {
    let targets = selected_languages
        .iter()
        .map(|&language| TemplateTarget {
            path: source_template_path(templates_dir, language),
            contents: builtin_template(language).as_bytes(),
        })
        .collect::<Vec<_>>();

    let directory_exists =
        user_config_fs::optional_directory_exists(templates_dir, SOURCE_TEMPLATE_DIRECTORY_KIND)?;
    let mut states = if directory_exists {
        inspect_all_targets(&targets)?
    } else {
        vec![TemplateFileState::Missing; targets.len()]
    };

    user_config_fs::ensure_directory(templates_dir, SOURCE_TEMPLATE_DIRECTORY_KIND)?;

    if !directory_exists {
        states = inspect_all_targets(&targets)?;
    }

    for (target, state) in targets.iter().zip(states) {
        match state {
            TemplateFileState::Exists => {
                reporter.report(Event::TemplateFileExists { path: &target.path });
            }
            TemplateFileState::Missing => match installer(&target.path, target.contents) {
                Ok(()) => {
                    reporter.report(Event::TemplateFileCreated { path: &target.path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match inspect_template_file(&target.path)? {
                        TemplateFileState::Exists => {
                            reporter.report(Event::TemplateFileExists { path: &target.path });
                        }
                        TemplateFileState::Missing => return Err(error.into()),
                    }
                }
                Err(error) => return Err(error.into()),
            },
        }
    }

    Ok(())
}

fn inspect_all_targets(targets: &[TemplateTarget]) -> Result<Vec<TemplateFileState>, AppError> {
    targets
        .iter()
        .map(|target| inspect_template_file(&target.path))
        .collect()
}

fn inspect_template_file(path: &Path) -> Result<TemplateFileState, AppError> {
    match user_config_fs::read_optional_utf8_file(path, SOURCE_TEMPLATE_KIND)? {
        OptionalUtf8File::Missing => Ok(TemplateFileState::Missing),
        OptionalUtf8File::Present(_) => Ok(TemplateFileState::Exists),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::{resolve_source_template_in, source_template_filename};
    use std::fs;

    #[derive(Debug, Default)]
    struct RecordingReporter {
        events: Vec<String>,
    }

    impl Reporter for RecordingReporter {
        fn report(&mut self, event: Event<'_>) {
            match event {
                Event::TemplateFileCreated { path } => {
                    self.events.push(format!("created:{}", path.display()));
                }
                Event::TemplateFileExists { path } => {
                    self.events.push(format!("exists:{}", path.display()));
                }
                _ => panic!("unexpected event"),
            }
        }
    }

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

    fn initialize(
        templates_dir: &Path,
        languages: &[Language],
    ) -> Result<RecordingReporter, AppError> {
        let mut reporter = RecordingReporter::default();
        initialize_source_templates_at(templates_dir, languages, &mut reporter)?;
        Ok(reporter)
    }

    #[test]
    fn initializes_both_missing_templates_with_exact_builtin_bytes_only() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("config");
        let templates_dir = config_dir.join("templates");

        let reporter = initialize(&templates_dir, &Language::ALL).unwrap();
        let cpp = templates_dir.join("cpp.cpp");
        let python = templates_dir.join("python.py");

        assert_eq!(
            fs::read(&cpp).unwrap(),
            builtin_template(Language::Cpp).as_bytes()
        );
        assert_eq!(
            fs::read(&python).unwrap(),
            builtin_template(Language::Python).as_bytes()
        );
        assert!(!config_dir.join("config.toml").exists());
        assert!(!templates_dir.join("stress_gen.py").exists());
        assert!(!templates_dir.join("stress_brute.py").exists());

        let mut entries = fs::read_dir(&templates_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries, ["cpp.cpp", "python.py"]);
        assert_eq!(
            reporter.events,
            [
                format!("created:{}", cpp.display()),
                format!("created:{}", python.display()),
            ]
        );
    }

    #[test]
    fn initializes_only_the_selected_language() {
        for language in Language::ALL {
            let temp = tempfile::tempdir().unwrap();
            let templates_dir = temp.path().join("config").join("templates");
            let reporter = initialize(&templates_dir, &[language]).unwrap();
            let selected = source_template_path(&templates_dir, language);
            let other = Language::ALL
                .into_iter()
                .find(|candidate| *candidate != language)
                .unwrap();

            assert_eq!(
                fs::read(&selected).unwrap(),
                builtin_template(language).as_bytes()
            );
            assert!(!source_template_path(&templates_dir, other).exists());
            assert_eq!(reporter.events, [format!("created:{}", selected.display())]);
        }
    }

    #[test]
    fn rerun_and_existing_utf8_files_are_preserved_byte_for_byte() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let cpp = templates_dir.join("cpp.cpp");
        let python = templates_dir.join("python.py");
        let custom = b"// heavily customized\nint main() { return 7; }\n";
        fs::write(&cpp, custom).unwrap();
        fs::write(&python, b"").unwrap();

        let reporter = initialize(&templates_dir, &Language::ALL).unwrap();

        assert_eq!(fs::read(&cpp).unwrap(), custom);
        assert_eq!(fs::read(&python).unwrap(), b"");
        assert_eq!(
            reporter.events,
            [
                format!("exists:{}", cpp.display()),
                format!("exists:{}", python.display()),
            ]
        );

        let rerun = initialize(&templates_dir, &Language::ALL).unwrap();
        assert_eq!(fs::read(&cpp).unwrap(), custom);
        assert_eq!(fs::read(&python).unwrap(), b"");
        assert_eq!(rerun.events, reporter.events);
    }

    #[test]
    fn partial_state_preserves_custom_cpp_and_creates_python() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let cpp = templates_dir.join("cpp.cpp");
        let python = templates_dir.join("python.py");
        let custom = b"// pinned user template\n";
        fs::write(&cpp, custom).unwrap();

        let reporter = initialize(&templates_dir, &Language::ALL).unwrap();

        assert_eq!(fs::read(&cpp).unwrap(), custom);
        assert_eq!(
            fs::read(&python).unwrap(),
            builtin_template(Language::Python).as_bytes()
        );
        assert_eq!(
            reporter.events,
            [
                format!("exists:{}", cpp.display()),
                format!("created:{}", python.display()),
            ]
        );
    }

    #[test]
    fn initialized_template_is_immediately_used_by_the_phase_one_resolver() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");

        initialize(&templates_dir, &[Language::Cpp]).unwrap();

        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Cpp).unwrap(),
            fs::read_to_string(templates_dir.join("cpp.cpp")).unwrap()
        );
        assert_eq!(
            resolve_source_template_in(&templates_dir, Language::Python).unwrap(),
            builtin_template(Language::Python)
        );
        assert!(!templates_dir.join("python.py").exists());
    }

    #[test]
    fn malformed_config_is_neither_loaded_nor_modified() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("config");
        let templates_dir = config_dir.join("templates");
        fs::create_dir(&config_dir).unwrap();
        let config_file = config_dir.join("config.toml");
        let malformed = b"this is not valid = [toml\n";
        fs::write(&config_file, malformed).unwrap();

        initialize(&templates_dir, &[Language::Cpp]).unwrap();

        assert_eq!(fs::read(config_file).unwrap(), malformed);
        assert!(templates_dir.join("cpp.cpp").is_file());
    }

    #[test]
    fn preflights_every_selected_target_before_creating_files() {
        for invalid_language in Language::ALL {
            let temp = tempfile::tempdir().unwrap();
            let templates_dir = temp.path().join("templates");
            fs::create_dir(&templates_dir).unwrap();
            let invalid = templates_dir.join(source_template_filename(invalid_language));
            fs::create_dir(&invalid).unwrap();
            let other_language = Language::ALL
                .into_iter()
                .find(|candidate| *candidate != invalid_language)
                .unwrap();
            let other = templates_dir.join(source_template_filename(other_language));
            let mut reporter = RecordingReporter::default();

            let error =
                initialize_source_templates_at(&templates_dir, &Language::ALL, &mut reporter)
                    .unwrap_err();

            assert!(error.to_string().contains("regular file"));
            assert!(invalid.is_dir());
            assert!(!other.exists());
            assert!(reporter.events.is_empty());
        }
    }

    #[test]
    fn invalid_utf8_is_rejected_and_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let cpp = templates_dir.join("cpp.cpp");
        let invalid = [0xff, 0xfe, 0x80];
        fs::write(&cpp, invalid).unwrap();

        let error = initialize(&templates_dir, &[Language::Cpp]).unwrap_err();

        assert!(error.to_string().contains("UTF-8"));
        assert_eq!(fs::read(cpp).unwrap(), invalid);
    }

    #[test]
    fn valid_file_symlink_is_preserved_with_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let target = temp.path().join("custom.cpp");
        let link = templates_dir.join("cpp.cpp");
        let custom = b"// linked customization\n";
        fs::write(&target, custom).unwrap();
        if !create_file_symlink(&target, &link) {
            return;
        }

        let reporter = initialize(&templates_dir, &[Language::Cpp]).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), custom);
        assert_eq!(reporter.events, [format!("exists:{}", link.display())]);
    }

    #[test]
    fn dangling_file_symlink_and_symlink_to_directory_are_rejected() {
        for dangling in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let templates_dir = temp.path().join("templates");
            fs::create_dir(&templates_dir).unwrap();
            let link = templates_dir.join("cpp.cpp");
            let target = temp.path().join("target");
            let created = if dangling {
                create_file_symlink(&target, &link)
            } else {
                fs::create_dir(&target).unwrap();
                create_directory_symlink(&target, &link)
            };
            if !created {
                return;
            }

            let error = initialize(&templates_dir, &[Language::Cpp]).unwrap_err();

            if dangling {
                assert!(error.to_string().contains("follow source template"));
            } else {
                assert!(error.to_string().contains("regular file"));
            }
            assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        }
    }

    #[test]
    fn creates_missing_directories_and_reuses_existing_templates_directory() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("missing-config");
        let templates_dir = config_dir.join("templates");

        initialize(&templates_dir, &[Language::Cpp]).unwrap();
        assert!(config_dir.is_dir());
        assert!(templates_dir.is_dir());

        initialize(&templates_dir, &[Language::Python]).unwrap();
        assert!(templates_dir.join("cpp.cpp").is_file());
        assert!(templates_dir.join("python.py").is_file());
    }

    #[test]
    fn valid_templates_directory_symlink_is_followed() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("managed-templates");
        let link = temp.path().join("templates");
        fs::create_dir(&target).unwrap();
        if !create_directory_symlink(&target, &link) {
            return;
        }

        initialize(&link, &[Language::Python]).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(target.join("python.py")).unwrap(),
            builtin_template(Language::Python).as_bytes()
        );
    }

    #[test]
    fn dangling_templates_symlink_and_nondirectory_templates_path_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let dangling = temp.path().join("dangling-templates");
        if !create_directory_symlink(&temp.path().join("missing"), &dangling) {
            return;
        }
        let error = initialize(&dangling, &[Language::Cpp]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("follow source template directory")
        );
        assert!(
            fs::symlink_metadata(&dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let file = temp.path().join("templates-file");
        fs::write(&file, b"keep").unwrap();
        let error = initialize(&file, &[Language::Cpp]).unwrap_err();
        assert!(error.to_string().contains("must resolve to a directory"));
        assert_eq!(fs::read(file).unwrap(), b"keep");
    }

    #[test]
    fn valid_symlinked_config_directory_is_followed() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("managed-config");
        let link = temp.path().join("config-link");
        fs::create_dir(&target).unwrap();
        if !create_directory_symlink(&target, &link) {
            return;
        }

        initialize(&link.join("templates"), &[Language::Cpp]).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(target.join("templates").join("cpp.cpp").is_file());
    }

    #[test]
    fn broken_config_directory_symlink_is_an_error_and_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("config-link");
        if !create_directory_symlink(&temp.path().join("missing-config"), &link) {
            return;
        }

        let error = initialize(&link.join("templates"), &[Language::Cpp]).unwrap_err();

        assert!(error.to_string().contains("source template directory"));
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    #[test]
    fn config_path_with_the_wrong_type_is_an_error_and_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("config");
        fs::write(&config_file, b"user data").unwrap();

        let error = initialize(&config_file.join("templates"), &[Language::Cpp]).unwrap_err();

        assert!(error.to_string().contains("source template directory"));
        assert_eq!(fs::read(config_file).unwrap(), b"user data");
    }

    #[test]
    fn race_created_valid_utf8_file_is_reinspected_and_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let cpp = templates_dir.join("cpp.cpp");
        let competitor = b"// race winner\n";
        let mut reporter = RecordingReporter::default();
        let mut installer = |path: &Path, _: &[u8]| {
            fs::write(path, competitor).unwrap();
            Err(io::ErrorKind::AlreadyExists.into())
        };

        initialize_source_templates_at_with(
            &templates_dir,
            &[Language::Cpp],
            &mut reporter,
            &mut installer,
        )
        .unwrap();

        assert_eq!(fs::read(&cpp).unwrap(), competitor);
        assert_eq!(reporter.events, [format!("exists:{}", cpp.display())]);
    }

    #[test]
    fn race_created_invalid_target_is_reinspected_and_preserved() {
        for invalid_kind in ["directory", "invalid-utf8"] {
            let temp = tempfile::tempdir().unwrap();
            let templates_dir = temp.path().join("templates");
            fs::create_dir(&templates_dir).unwrap();
            let cpp = templates_dir.join("cpp.cpp");
            let mut reporter = RecordingReporter::default();
            let mut installer = |path: &Path, _: &[u8]| {
                if invalid_kind == "directory" {
                    fs::create_dir(path).unwrap();
                } else {
                    fs::write(path, [0xff, 0xfe]).unwrap();
                }
                Err(io::ErrorKind::AlreadyExists.into())
            };

            let error = initialize_source_templates_at_with(
                &templates_dir,
                &[Language::Cpp],
                &mut reporter,
                &mut installer,
            )
            .unwrap_err();

            if invalid_kind == "directory" {
                assert!(cpp.is_dir());
                assert!(error.to_string().contains("regular file"));
            } else {
                assert_eq!(fs::read(&cpp).unwrap(), [0xff, 0xfe]);
                assert!(error.to_string().contains("UTF-8"));
            }
            assert!(reporter.events.is_empty());
        }
    }

    #[test]
    fn completed_first_install_remains_after_second_race_failure_and_rerun_converges() {
        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let cpp = templates_dir.join("cpp.cpp");
        let python = templates_dir.join("python.py");
        let mut calls = 0;
        let mut reporter = RecordingReporter::default();
        let mut installer = |path: &Path, contents: &[u8]| {
            calls += 1;
            if calls == 2 {
                fs::create_dir(path).unwrap();
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            safe_file::install_noclobber(path, contents, TEMPLATE_INIT_TEMPORARY_PREFIX)
        };

        let error = initialize_source_templates_at_with(
            &templates_dir,
            &Language::ALL,
            &mut reporter,
            &mut installer,
        )
        .unwrap_err();

        assert!(error.to_string().contains("regular file"));
        assert_eq!(
            fs::read(&cpp).unwrap(),
            builtin_template(Language::Cpp).as_bytes()
        );
        assert!(python.is_dir());
        assert_eq!(reporter.events, [format!("created:{}", cpp.display())]);

        fs::remove_dir(&python).unwrap();
        let rerun = initialize(&templates_dir, &Language::ALL).unwrap();
        assert_eq!(
            fs::read(&python).unwrap(),
            builtin_template(Language::Python).as_bytes()
        );
        assert_eq!(
            rerun.events,
            [
                format!("exists:{}", cpp.display()),
                format!("created:{}", python.display()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_objects_and_symlinks_to_special_objects_are_rejected() {
        use std::os::unix::net::UnixListener;

        for use_symlink in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let templates_dir = temp.path().join("templates");
            fs::create_dir(&templates_dir).unwrap();
            let cpp = templates_dir.join("cpp.cpp");
            let socket = if use_symlink {
                temp.path().join("socket")
            } else {
                cpp.clone()
            };
            let _listener = UnixListener::bind(&socket).unwrap();
            if use_symlink {
                std::os::unix::fs::symlink(&socket, &cpp).unwrap();
            }

            let error = initialize(&templates_dir, &[Language::Cpp]).unwrap_err();

            assert!(error.to_string().contains("regular file"));
            assert!(fs::symlink_metadata(cpp).is_ok());
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_regular_file_is_rejected_where_permissions_are_enforced() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let templates_dir = temp.path().join("templates");
        fs::create_dir(&templates_dir).unwrap();
        let cpp = templates_dir.join("cpp.cpp");
        fs::write(&cpp, b"preserve me").unwrap();
        fs::set_permissions(&cpp, fs::Permissions::from_mode(0)).unwrap();
        let permissions_are_enforced = fs::read(&cpp).is_err();

        let result = initialize(&templates_dir, &[Language::Cpp]);
        fs::set_permissions(&cpp, fs::Permissions::from_mode(0o600)).unwrap();

        if !permissions_are_enforced {
            return;
        }
        let error = result.unwrap_err();
        assert!(error.to_string().contains("failed to read source template"));
        assert_eq!(fs::read(cpp).unwrap(), b"preserve me");
    }
}
