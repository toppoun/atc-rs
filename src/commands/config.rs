use crate::config::{Config, INITIAL_CONFIG};
use crate::error::AppError;
use crate::safe_file;
use crate::ui::{Event, Reporter};
use crate::user_config_fs::{self, OptionalUtf8File};
use std::io;
use std::path::Path;

const CONFIG_INIT_TEMPORARY_PREFIX: &str = ".atc-config-init-";
const CONFIG_DIRECTORY_KIND: &str = "global config directory";
const CONFIG_FILE_KIND: &str = "global config file";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigFileState {
    Missing,
    Exists,
}

pub(crate) fn config_init(reporter: &mut dyn Reporter) -> Result<(), AppError> {
    let config_file = crate::paths::config_file()?;
    initialize_config_at(&config_file, reporter)
}

pub(crate) fn initialize_config_at(
    config_file: &Path,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    initialize_config_at_with(config_file, reporter, &mut |path, contents| {
        safe_file::install_noclobber(path, contents, CONFIG_INIT_TEMPORARY_PREFIX)
    })
}

fn initialize_config_at_with(
    config_file: &Path,
    reporter: &mut dyn Reporter,
    installer: &mut dyn FnMut(&Path, &[u8]) -> io::Result<()>,
) -> Result<(), AppError> {
    let parent = config_file.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "global config file has no parent directory: {}",
                config_file.display()
            ),
        )
    })?;
    user_config_fs::ensure_directory(parent, CONFIG_DIRECTORY_KIND)?;

    if inspect_config_file(config_file)? == ConfigFileState::Exists {
        reporter.report(Event::ConfigFileExists { path: config_file });
        return Ok(());
    }

    match installer(config_file, INITIAL_CONFIG.as_bytes()) {
        Ok(()) => {
            reporter.report(Event::ConfigFileCreated { path: config_file });
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match inspect_config_file(config_file)? {
                ConfigFileState::Exists => {
                    reporter.report(Event::ConfigFileExists { path: config_file });
                    Ok(())
                }
                ConfigFileState::Missing => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn inspect_config_file(path: &Path) -> Result<ConfigFileState, AppError> {
    match user_config_fs::read_optional_utf8_file(path, CONFIG_FILE_KIND)? {
        OptionalUtf8File::Missing => Ok(ConfigFileState::Missing),
        OptionalUtf8File::Present(contents) => {
            Config::parse(&contents).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "failed to parse and validate global config file {}: {error}",
                        path.display()
                    ),
                )
            })?;
            Ok(ConfigFileState::Exists)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;

    #[derive(Debug, Default)]
    struct RecordingReporter {
        events: Vec<String>,
    }

    impl Reporter for RecordingReporter {
        fn report(&mut self, event: Event<'_>) {
            match event {
                Event::ConfigFileCreated { path } => {
                    self.events.push(format!("created:{}", path.display()));
                }
                Event::ConfigFileExists { path } => {
                    self.events.push(format!("exists:{}", path.display()));
                }
                _ => panic!("unexpected event"),
            }
        }
    }

    fn initialize(config_file: &Path) -> Result<RecordingReporter, AppError> {
        let mut reporter = RecordingReporter::default();
        initialize_config_at(config_file, &mut reporter)?;
        Ok(reporter)
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

    fn assert_no_staging_artifacts(directory: &Path) {
        assert!(
            fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .all(|name| !name
                    .to_string_lossy()
                    .starts_with(CONFIG_INIT_TEMPORARY_PREFIX))
        );
    }

    #[test]
    fn creates_exact_comments_only_config_and_no_templates() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("atc");
        let config_file = config_dir.join("config.toml");

        let reporter = initialize(&config_file).unwrap();

        assert_eq!(fs::read(&config_file).unwrap(), INITIAL_CONFIG.as_bytes());
        assert_eq!(
            Config::parse(&fs::read_to_string(&config_file).unwrap()).unwrap(),
            Config::default()
        );
        assert!(!config_dir.join("templates").exists());
        assert_eq!(
            fs::read_dir(&config_dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            [OsStr::new("config.toml")]
        );
        assert_eq!(
            reporter.events,
            [format!("created:{}", config_file.display())]
        );
    }

    #[test]
    fn rerun_is_idempotent_and_preserves_exact_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("atc").join("config.toml");

        let first = initialize(&config_file).unwrap();
        let before = fs::read(&config_file).unwrap();
        let second = initialize(&config_file).unwrap();

        assert_eq!(before, INITIAL_CONFIG.as_bytes());
        assert_eq!(fs::read(&config_file).unwrap(), before);
        assert_eq!(first.events, [format!("created:{}", config_file.display())]);
        assert_eq!(second.events, [format!("exists:{}", config_file.display())]);
        assert_no_staging_artifacts(config_file.parent().unwrap());
    }

    #[test]
    fn existing_valid_custom_config_is_preserved_byte_for_byte() {
        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("config.toml");
        let custom = b"runner.timeout_seconds = 3.0\n# keep this comment  \n";
        fs::write(&config_file, custom).unwrap();

        let reporter = initialize(&config_file).unwrap();

        assert_eq!(fs::read(&config_file).unwrap(), custom);
        assert_eq!(
            reporter.events,
            [format!("exists:{}", config_file.display())]
        );
        assert!(!temp.path().join("templates").exists());
    }

    #[test]
    fn invalid_existing_configs_are_errors_and_preserved_without_events_or_staging() {
        for (name, original) in [
            ("malformed", b"[runner\n".as_slice()),
            ("unknown", b"unknown = true\n".as_slice()),
            (
                "semantic-timeout",
                b"runner.timeout_seconds = 0\n".as_slice(),
            ),
            ("semantic-program", b"runner.python = \"   \"\n".as_slice()),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let config_file = temp.path().join("config.toml");
            fs::write(&config_file, original).unwrap();
            let mut reporter = RecordingReporter::default();

            let error = initialize_config_at(&config_file, &mut reporter).unwrap_err();

            assert!(
                matches!(error, AppError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData),
                "unexpected error for {name}: {error}"
            );
            assert_eq!(fs::read(&config_file).unwrap(), original, "case {name}");
            assert!(reporter.events.is_empty(), "case {name}");
            assert_no_staging_artifacts(temp.path());
        }
    }

    #[test]
    fn invalid_utf8_directory_and_wrong_parent_type_are_rejected_and_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let invalid_utf8 = temp.path().join("invalid.toml");
        fs::write(&invalid_utf8, [0xff, 0xfe, 0x80]).unwrap();
        let mut reporter = RecordingReporter::default();
        let error = initialize_config_at(&invalid_utf8, &mut reporter).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
        assert_eq!(fs::read(&invalid_utf8).unwrap(), [0xff, 0xfe, 0x80]);
        assert!(reporter.events.is_empty());

        let directory = temp.path().join("directory.toml");
        fs::create_dir(&directory).unwrap();
        let error = initialize(&directory).unwrap_err();
        assert!(error.to_string().contains("regular file"));
        assert!(directory.is_dir());

        let wrong_parent = temp.path().join("not-a-directory");
        fs::write(&wrong_parent, b"keep parent bytes").unwrap();
        let error = initialize(&wrong_parent.join("config.toml")).unwrap_err();
        assert!(error.to_string().contains("global config directory"));
        assert_eq!(fs::read(&wrong_parent).unwrap(), b"keep parent bytes");
    }

    #[test]
    fn valid_config_file_symlink_and_target_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let external = temp.path().join("managed.toml");
        let config_file = temp.path().join("config.toml");
        let custom = b"defaults.language = \"python\"\n";
        fs::write(&external, custom).unwrap();
        if !create_file_symlink(&external, &config_file) {
            return;
        }

        let reporter = initialize(&config_file).unwrap();

        assert!(
            fs::symlink_metadata(&config_file)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&external).unwrap(), custom);
        assert_eq!(
            reporter.events,
            [format!("exists:{}", config_file.display())]
        );
    }

    #[test]
    fn invalid_config_file_symlink_and_target_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let external = temp.path().join("managed.toml");
        let config_file = temp.path().join("config.toml");
        let malformed = b"[runner\n";
        fs::write(&external, malformed).unwrap();
        if !create_file_symlink(&external, &config_file) {
            return;
        }
        let mut reporter = RecordingReporter::default();

        let error = initialize_config_at(&config_file, &mut reporter).unwrap_err();

        assert!(matches!(
            error,
            AppError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert!(
            fs::symlink_metadata(&config_file)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&external).unwrap(), malformed);
        assert!(reporter.events.is_empty());
    }

    #[test]
    fn dangling_and_wrong_target_config_symlinks_are_rejected_and_preserved() {
        for dangling in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let config_file = temp.path().join("config.toml");
            let target = temp.path().join("target");
            let created = if dangling {
                create_file_symlink(&target, &config_file)
            } else {
                fs::create_dir(&target).unwrap();
                create_directory_symlink(&target, &config_file)
            };
            if !created {
                return;
            }

            let error = initialize(&config_file).unwrap_err();

            if dangling {
                assert!(error.to_string().contains("follow global config file"));
            } else {
                assert!(error.to_string().contains("regular file"));
            }
            assert!(
                fs::symlink_metadata(&config_file)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn valid_symlinked_config_directory_is_followed_and_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("managed-config");
        let linked = temp.path().join("atc");
        fs::create_dir(&target).unwrap();
        if !create_directory_symlink(&target, &linked) {
            return;
        }

        initialize(&linked.join("config.toml")).unwrap();

        assert!(
            fs::symlink_metadata(&linked)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(target.join("config.toml")).unwrap(),
            INITIAL_CONFIG.as_bytes()
        );
    }

    #[test]
    fn dangling_config_directory_symlink_is_rejected_and_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let linked = temp.path().join("atc");
        if !create_directory_symlink(&temp.path().join("missing"), &linked) {
            return;
        }

        let error = initialize(&linked.join("config.toml")).unwrap_err();

        assert!(error.to_string().contains("global config directory"));
        assert!(
            fs::symlink_metadata(&linked)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn race_winner_valid_config_is_preserved_and_reported_as_existing() {
        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("config.toml");
        let winner = b"runner.timeout_seconds = 3.0\n";
        let mut reporter = RecordingReporter::default();
        let mut installer = |path: &Path, _: &[u8]| {
            fs::write(path, winner).unwrap();
            Err(io::ErrorKind::AlreadyExists.into())
        };

        initialize_config_at_with(&config_file, &mut reporter, &mut installer).unwrap();

        assert_eq!(fs::read(&config_file).unwrap(), winner);
        assert_eq!(
            reporter.events,
            [format!("exists:{}", config_file.display())]
        );
    }

    #[test]
    fn race_winner_valid_config_symlink_is_preserved_and_reported_as_existing() {
        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("config.toml");
        let managed = temp.path().join("managed.toml");
        let winner = b"defaults.language = \"python\"\n";
        fs::write(&managed, winner).unwrap();
        let probe = temp.path().join("symlink-probe");
        if !create_file_symlink(&managed, &probe) {
            return;
        }
        fs::remove_file(&probe).unwrap();
        let mut reporter = RecordingReporter::default();
        let mut installer = |path: &Path, _: &[u8]| {
            assert!(create_file_symlink(&managed, path));
            Err(io::ErrorKind::AlreadyExists.into())
        };

        initialize_config_at_with(&config_file, &mut reporter, &mut installer).unwrap();

        assert!(
            fs::symlink_metadata(&config_file)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&managed).unwrap(), winner);
        assert_eq!(
            reporter.events,
            [format!("exists:{}", config_file.display())]
        );
    }

    #[test]
    fn invalid_race_winners_are_preserved_and_not_reported_as_success() {
        for (name, winner) in [
            ("malformed", Some(b"[runner\n".as_slice())),
            ("unknown", Some(b"unknown = true\n".as_slice())),
            (
                "semantic",
                Some(b"runner.timeout_seconds = -1\n".as_slice()),
            ),
            ("directory", None),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let config_file = temp.path().join("config.toml");
            let mut reporter = RecordingReporter::default();
            let mut installer = |path: &Path, _: &[u8]| {
                match winner {
                    Some(contents) => fs::write(path, contents).unwrap(),
                    None => fs::create_dir(path).unwrap(),
                }
                Err(io::ErrorKind::AlreadyExists.into())
            };

            let error =
                initialize_config_at_with(&config_file, &mut reporter, &mut installer).unwrap_err();

            if let Some(contents) = winner {
                assert_eq!(fs::read(&config_file).unwrap(), contents, "case {name}");
            } else {
                assert!(config_file.is_dir());
            }
            assert!(!error.to_string().is_empty());
            assert!(reporter.events.is_empty(), "case {name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn special_object_race_winner_is_preserved_and_not_reported_as_success() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("config.toml");
        let mut reporter = RecordingReporter::default();
        let mut installer = |path: &Path, _: &[u8]| {
            let listener = UnixListener::bind(path).unwrap();
            drop(listener);
            Err(io::ErrorKind::AlreadyExists.into())
        };

        let error =
            initialize_config_at_with(&config_file, &mut reporter, &mut installer).unwrap_err();

        assert!(error.to_string().contains("regular file"));
        assert!(fs::symlink_metadata(config_file).is_ok());
        assert!(reporter.events.is_empty());
    }

    #[test]
    fn config_init_and_templates_remain_independent() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("atc");
        let config_file = config_dir.join("config.toml");
        let templates = config_dir.join("templates");
        fs::create_dir_all(&templates).unwrap();
        let cpp = templates.join("cpp.cpp");
        let python = templates.join("python.py");
        let cpp_bytes = b"custom cpp\n";
        let python_bytes = b"custom python\n";
        fs::write(&cpp, cpp_bytes).unwrap();
        fs::write(&python, python_bytes).unwrap();

        initialize(&config_file).unwrap();

        assert_eq!(fs::read(&cpp).unwrap(), cpp_bytes);
        assert_eq!(fs::read(&python).unwrap(), python_bytes);

        let second = temp.path().join("second");
        fs::create_dir(&second).unwrap();
        fs::write(
            second.join("config.toml"),
            b"runner.timeout_seconds = 3.0\n",
        )
        .unwrap();
        initialize(&second.join("config.toml")).unwrap();
        assert!(!second.join("templates").exists());

        let malformed = b"[runner\n";
        fs::write(&config_file, malformed).unwrap();
        let before_cpp = fs::read(&cpp).unwrap();
        let before_python = fs::read(&python).unwrap();
        assert!(initialize(&config_file).is_err());
        assert_eq!(fs::read(&config_file).unwrap(), malformed);
        assert_eq!(fs::read(&cpp).unwrap(), before_cpp);
        assert_eq!(fs::read(&python).unwrap(), before_python);
    }

    #[cfg(windows)]
    #[test]
    fn valid_config_directory_junction_is_followed_and_preserved() {
        use std::os::windows::fs::MetadataExt;
        use std::process::Command;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("managed-config");
        let junction = temp.path().join("atc-junction");
        fs::create_dir(&target).unwrap();
        let output = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to create test junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let before_attributes = fs::symlink_metadata(&junction).unwrap().file_attributes();
        assert_ne!(before_attributes & FILE_ATTRIBUTE_REPARSE_POINT, 0);

        initialize(&junction.join("config.toml")).unwrap();

        assert_eq!(
            fs::read(target.join("config.toml")).unwrap(),
            INITIAL_CONFIG.as_bytes()
        );
        let after_attributes = fs::symlink_metadata(&junction).unwrap().file_attributes();
        assert_ne!(after_attributes & FILE_ATTRIBUTE_REPARSE_POINT, 0);

        fs::remove_dir(&junction).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn special_objects_and_symlinks_to_special_objects_are_rejected() {
        use std::os::unix::net::UnixListener;

        for use_symlink in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let config_file = temp.path().join("config.toml");
            let socket = if use_symlink {
                temp.path().join("socket")
            } else {
                config_file.clone()
            };
            let _listener = UnixListener::bind(&socket).unwrap();
            if use_symlink {
                std::os::unix::fs::symlink(&socket, &config_file).unwrap();
            }

            let error = initialize(&config_file).unwrap_err();

            assert!(error.to_string().contains("regular file"));
            assert!(fs::symlink_metadata(config_file).is_ok());
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_regular_config_is_rejected_where_permissions_are_enforced() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("config.toml");
        let contents = b"runner.timeout_seconds = 3.0\n";
        fs::write(&config_file, contents).unwrap();
        fs::set_permissions(&config_file, fs::Permissions::from_mode(0o0)).unwrap();
        let permissions_are_enforced = fs::read(&config_file).is_err();

        let result = initialize(&config_file);
        fs::set_permissions(&config_file, fs::Permissions::from_mode(0o600)).unwrap();

        if !permissions_are_enforced {
            return;
        }
        let error = result.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to read global config file")
        );
        assert_eq!(fs::read(config_file).unwrap(), contents);
    }
}
