use super::{FetchedContestData, fetch_contest_data};
use crate::atcoder;
use crate::error::AppError;
use crate::ui::{Event, Reporter};
use crate::workspace;
use crate::workspace::validate_refresh_destination;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) struct PreparedRefresh {
    destination: PathBuf,
    destination_identity: RefreshDestinationIdentity,
    contest_id: String,
    force: bool,
    metadata_snapshot: Option<Vec<u8>>,
    staging: tempfile::TempDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RefreshDestinationIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows {
        volume_serial_number: u32,
        file_index: u64,
    },
    #[cfg(not(any(unix, windows)))]
    Other { canonical_path: PathBuf },
}

impl PreparedRefresh {
    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) fn contest_id(&self) -> &str {
        &self.contest_id
    }

    #[cfg(test)]
    pub(super) fn staging_path(&self) -> &Path {
        self.staging.path()
    }
}

pub(crate) fn refresh(
    contest: Option<String>,
    force: bool,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::resolve_contest_target(&cwd, contest.as_deref())?;

    let contest_id = resolve_refresh_contest_id(&destination, contest.as_deref(), force)?;

    let atcoder = create_atcoder_client()?;

    refresh_at(&destination, &contest_id, force, &atcoder, reporter)
}

pub(super) fn create_atcoder_client() -> Result<atcoder::AtCoderClient, AppError> {
    if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        Ok(atcoder::AtCoderClient::fixture(path))
    } else {
        Ok(atcoder::AtCoderClient::new()?)
    }
}

pub(super) fn resolve_refresh_contest_id(
    destination: &Path,
    specified_contest_id: Option<&str>,
    force: bool,
) -> Result<String, AppError> {
    if force {
        let contest_id = match specified_contest_id {
            Some(contest_id) => contest_id.to_string(),
            None => destination
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "current directory has no UTF-8 directory name: {}",
                            destination.display()
                        ),
                    )
                })?
                .to_string(),
        };

        validate_refresh_destination(destination, &contest_id, true)?;
        return Ok(contest_id);
    }

    match specified_contest_id {
        Some(contest_id) => {
            validate_refresh_destination(destination, contest_id, false)?;

            match workspace::inspect_contest_metadata(destination)? {
                workspace::ContestMetadataHealth::Healthy(contest) => {
                    workspace::validate_contest_identity(&contest, contest_id)?;
                }
                workspace::ContestMetadataHealth::UnsupportedVersion(version) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsupported contest metadata version: {version}"),
                    )
                    .into());
                }
                workspace::ContestMetadataHealth::Missing
                | workspace::ContestMetadataHealth::Invalid => {
                    // An explicit contest ID is the existing recovery mechanism for
                    // missing or malformed metadata. Healthy metadata, however, must
                    // agree with the requested target before it may be replaced.
                }
            }

            Ok(contest_id.to_string())
        }
        None => {
            workspace::validate_workspace_marker(destination)?;
            Ok(workspace::load_metadata(destination)?.contest_id)
        }
    }
}

pub(super) fn refresh_at(
    destination: &Path,
    contest_id: &str,
    force: bool,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let prepared = prepare_refresh(destination, contest_id, force, atcoder, reporter)?;
    apply_refresh(prepared, reporter)
}

pub(crate) fn prepare_refresh(
    destination: &Path,
    contest_id: &str,
    force: bool,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<PreparedRefresh, AppError> {
    let destination_identity = refresh_destination_identity(destination)?;
    validate_refresh_destination(destination, contest_id, force)?;
    let metadata_snapshot = refresh_metadata_snapshot(destination)?;
    revalidate_prepared_destination(
        destination,
        &destination_identity,
        contest_id,
        force,
        metadata_snapshot.as_deref(),
    )?;

    let FetchedContestData {
        contest,
        samples_by_problem,
    } = fetch_contest_data(contest_id, atcoder, reporter)?;
    workspace::validate_contest_identity(&contest, contest_id)?;
    workspace::validate_contest_paths(&contest)?;

    // Fetching may take long enough for the destination to change. Revalidate
    // immediately before choosing it as the staging parent.
    revalidate_prepared_destination(
        destination,
        &destination_identity,
        contest_id,
        force,
        metadata_snapshot.as_deref(),
    )?;

    let staging = tempfile::Builder::new()
        .prefix(".atc-refresh-")
        .tempdir_in(destination)?;

    // Creating the staging directory is path-based. Recheck immediately so a
    // replaced destination cannot cause staging to be built in another contest.
    revalidate_prepared_destination(
        destination,
        &destination_identity,
        contest_id,
        force,
        metadata_snapshot.as_deref(),
    )?;

    for (problem, samples) in contest.problems.iter().zip(samples_by_problem) {
        workspace::save_samples(staging.path(), problem, &samples)?;
    }
    workspace::save_metadata(staging.path(), &contest)?;

    revalidate_prepared_destination(
        destination,
        &destination_identity,
        contest_id,
        force,
        metadata_snapshot.as_deref(),
    )?;

    Ok(PreparedRefresh {
        destination: destination.to_path_buf(),
        destination_identity,
        contest_id: contest_id.to_owned(),
        force,
        metadata_snapshot,
        staging,
    })
}

pub(crate) fn apply_refresh(
    prepared: PreparedRefresh,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let PreparedRefresh {
        destination,
        destination_identity,
        contest_id,
        force,
        metadata_snapshot,
        staging,
    } = prepared;

    revalidate_prepared_destination(
        &destination,
        &destination_identity,
        &contest_id,
        force,
        metadata_snapshot.as_deref(),
    )?;
    workspace::replace_refresh_data(&destination, staging, force)?;
    reporter.report(Event::WorkspaceRefreshed {
        destination: &destination,
    });

    Ok(())
}

fn refresh_metadata_snapshot(destination: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(destination.join(".atc").join("contest.toml")) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn refresh_destination_identity(destination: &Path) -> io::Result<RefreshDestinationIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = fs::symlink_metadata(destination)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "contest destination is not a real directory: {}",
                destination.display()
            ),
        ));
    }

    Ok(RefreshDestinationIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn refresh_destination_identity(destination: &Path) -> io::Result<RefreshDestinationIdentity> {
    use std::os::windows::io::AsRawHandle as _;
    use std::os::windows::prelude::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        GetFileInformationByHandle,
    };

    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(destination)?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` keeps the handle valid for the call and `information`
    // points to writable storage of the exact structure required by Win32.
    if unsafe {
        GetFileInformationByHandle(file.as_raw_handle(), std::ptr::from_mut(&mut information))
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "contest destination is not a real directory: {}",
                destination.display()
            ),
        ));
    }
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);

    Ok(RefreshDestinationIdentity::Windows {
        volume_serial_number: information.dwVolumeSerialNumber,
        file_index,
    })
}

#[cfg(not(any(unix, windows)))]
fn refresh_destination_identity(destination: &Path) -> io::Result<RefreshDestinationIdentity> {
    Ok(RefreshDestinationIdentity::Other {
        canonical_path: fs::canonicalize(destination)?,
    })
}

fn changed_refresh_destination_error() -> AppError {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "contest destination changed while refresh was being prepared; retry refresh",
    )
    .into()
}

fn revalidate_prepared_destination(
    destination: &Path,
    expected_identity: &RefreshDestinationIdentity,
    contest_id: &str,
    force: bool,
    expected_metadata: Option<&[u8]>,
) -> Result<(), AppError> {
    if refresh_destination_identity(destination)? != *expected_identity {
        return Err(changed_refresh_destination_error());
    }
    validate_refresh_destination(destination, contest_id, force)?;
    let current_metadata = refresh_metadata_snapshot(destination)?;
    if current_metadata.as_deref() != expected_metadata {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "contest metadata changed while refresh was being prepared; retry refresh",
        )
        .into());
    }
    if refresh_destination_identity(destination)? != *expected_identity {
        return Err(changed_refresh_destination_error());
    }
    Ok(())
}
