use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct InstallContext {
    action: &'static str,
    destination: PathBuf,
    source: io::Error,
}

impl std::fmt::Display for InstallContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "failed to {} {}: {}",
            self.action,
            self.destination.display(),
            self.source
        )
    }
}

impl std::error::Error for InstallContext {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn install_error(destination: &Path, action: &'static str, source: io::Error) -> io::Error {
    let kind = source.kind();
    io::Error::new(
        kind,
        InstallContext {
            action,
            destination: destination.to_path_buf(),
            source,
        },
    )
}

pub(crate) fn install_noclobber(
    destination: &Path,
    contents: &[u8],
    temporary_prefix: &str,
) -> io::Result<()> {
    install_noclobber_with_hook(destination, contents, temporary_prefix, || {})
}

fn install_noclobber_with_hook(
    destination: &Path,
    contents: &[u8],
    temporary_prefix: &str,
    before_persist: impl FnOnce(),
) -> io::Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "file destination has no parent directory: {}",
                destination.display()
            ),
        )
    })?;

    let mut staging = tempfile::Builder::new()
        .prefix(temporary_prefix)
        .tempfile_in(parent)
        .map_err(|error| install_error(destination, "stage", error))?;

    staging
        .write_all(contents)
        .map_err(|error| install_error(destination, "write", error))?;
    staging
        .as_file_mut()
        .sync_all()
        .map_err(|error| install_error(destination, "flush", error))?;

    before_persist();

    match staging.persist_noclobber(destination) {
        Ok(_) => Ok(()),
        Err(error) => {
            let tempfile::PersistError { error, file } = error;
            drop(file);
            Err(install_error(destination, "create", error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;

    const PREFIX: &str = ".atc-safe-create-";

    #[test]
    fn installs_complete_contents_and_leaves_no_staging_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("target.txt");
        let contents = b"complete contents\n";

        install_noclobber(&destination, contents, PREFIX).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), contents);
        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [OsStr::new("target.txt")]);
    }

    #[test]
    fn deterministic_competitor_is_not_clobbered_and_owned_staging_is_cleaned() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("target.txt");
        let user_artifact = temp.path().join(".atc-safe-create-user-data");
        fs::write(&user_artifact, b"keep this").unwrap();

        let error = install_noclobber_with_hook(&destination, b"new contents\n", PREFIX, || {
            fs::write(&destination, b"competitor contents\n").unwrap()
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&destination).unwrap(), b"competitor contents\n");
        assert_eq!(fs::read(&user_artifact).unwrap(), b"keep this");

        let mut entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            [
                OsStr::new(".atc-safe-create-user-data"),
                OsStr::new("target.txt"),
            ]
        );
    }

    #[test]
    fn failure_retains_the_operation_destination_and_source() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("missing-parent").join("target.txt");

        let error = install_noclobber(&destination, b"contents\n", PREFIX).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("failed to stage"));
        assert!(
            error
                .to_string()
                .contains(&destination.display().to_string())
        );
        let context = error
            .get_ref()
            .expect("safe-file error should retain context");
        assert!(
            context.source().is_some(),
            "the underlying filesystem error should remain in the source chain"
        );
        assert!(!destination.exists());
    }
}
