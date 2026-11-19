use super::fetch_contest_data;
use crate::atcoder;
use crate::auth::SessionAuth;
use crate::error::AppError;
use crate::safe_file::{self, FileSnapshot};
use crate::ui::{Event, Reporter};
use crate::workspace;
use crate::workspace::diagnosis::{
    BlockReason, ContestDiagnosis, MigrationReason, diagnose_contest,
};
use crate::workspace::validate_refresh_destination;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) struct PreparedRefresh {
    destination: PathBuf,
    contest_id: String,
    force: bool,
    baseline: RefreshBaseline,
    staging: crate::safe_file::PreservedTempDir,
}

struct RefreshBaseline {
    destination_identity: RefreshDestinationIdentity,
    metadata_snapshot: Option<FileSnapshot>,
    diagnosis: Option<ContestDiagnosis>,
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
    refresh_from(
        &cwd,
        contest.as_deref(),
        force,
        create_atcoder_client,
        reporter,
    )
}

pub(super) fn refresh_from(
    cwd: &Path,
    specified_contest_id: Option<&str>,
    force: bool,
    create_client: impl FnOnce() -> Result<atcoder::AtCoderClient, AppError>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let destination = workspace::resolve_contest_target(cwd, specified_contest_id)?;
    // Capture the directory and metadata before diagnosis so the decision cannot
    // authorize an update to a different directory or manifest generation.
    let destination_identity = refresh_destination_identity(&destination);
    let metadata_snapshot = workspace::read_contest_metadata_snapshot(&destination);
    let contest_id = resolve_refresh_contest_id(
        &destination,
        specified_contest_id,
        force,
        &metadata_snapshot,
    )?;
    if !workspace::is_safe_atcoder_id(&contest_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid contest ID: {contest_id:?}"),
        )
        .into());
    }
    let diagnosis = diagnose_contest(&destination, &contest_id);
    let healthy = match &diagnosis {
        ContestDiagnosis::Healthy { .. } => true,
        ContestDiagnosis::RepairRequired(_) => false,
        ContestDiagnosis::MissingDirectory => {
            return Err(workspace::missing_contest_directory(&destination).into());
        }
        ContestDiagnosis::MigrationRequired(MigrationReason::UnsupportedVersion(version)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "refresh refused by local diagnosis for {contest_id}: unsupported contest \
                     metadata version {version}; inspect with `atc migrate --dry-run` in the contest directory"
                ),
            )
            .into());
        }
        ContestDiagnosis::Blocked(reason) => {
            return Err(blocked_refresh_error(&contest_id, reason));
        }
    };
    let baseline = RefreshBaseline {
        destination_identity: destination_identity?,
        metadata_snapshot: metadata_snapshot?,
        diagnosis: Some(diagnosis),
    };
    baseline.revalidate(&destination, &contest_id, force)?;
    if healthy && !force {
        reporter.report(Event::WorkspaceRefreshSkipped {
            contest_id: &contest_id,
        });
        return Ok(());
    }

    let atcoder = create_client()?;
    let prepared = prepare_refresh_from_baseline(
        &destination,
        &contest_id,
        force,
        baseline,
        &atcoder,
        reporter,
    )?;
    apply_refresh(prepared, reporter)
}

pub(super) fn create_atcoder_client() -> Result<atcoder::AtCoderClient, AppError> {
    if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        Ok(atcoder::AtCoderClient::fixture(path))
    } else {
        Ok(atcoder::AtCoderClient::new()?)
    }
}

pub(super) fn create_atcoder_client_from_auth(
    auth: &std::sync::Arc<SessionAuth>,
) -> Result<atcoder::AtCoderClient, AppError> {
    if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        Ok(atcoder::AtCoderClient::fixture(path))
    } else {
        Ok(atcoder::AtCoderClient::from_session_auth(
            std::sync::Arc::clone(auth),
        )?)
    }
}

fn resolve_refresh_contest_id(
    destination: &Path,
    specified_contest_id: Option<&str>,
    force: bool,
    metadata_snapshot: &io::Result<Option<FileSnapshot>>,
) -> Result<String, AppError> {
    if let Some(contest_id) = specified_contest_id {
        return Ok(contest_id.to_owned());
    }
    if force {
        return destination
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "current directory has no UTF-8 directory name: {}",
                        destination.display()
                    ),
                )
                .into()
            });
    }
    workspace::validate_workspace_marker(destination)?;
    let metadata = metadata_snapshot
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))?
        .as_ref()
        .ok_or_else(|| io::Error::new(
            io::ErrorKind::NotFound,
            "contest metadata not found; specify the contest ID with `atc refresh -c <contest-id>` from its parent or workspace root",
        ))?;
    std::str::from_utf8(&metadata.bytes)
        .ok()
        .and_then(workspace::metadata_contest_owner)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot confirm ownership of existing contest metadata",
            )
            .into()
        })
}

#[cfg(test)]
// Exercise the shared prepare/apply path used by the TUI.
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
    let baseline = RefreshBaseline {
        destination_identity,
        metadata_snapshot: refresh_metadata_snapshot(destination)?,
        diagnosis: None,
    };
    baseline.revalidate(destination, contest_id, force)?;
    prepare_refresh_from_baseline(destination, contest_id, force, baseline, atcoder, reporter)
}

fn prepare_refresh_from_baseline(
    destination: &Path,
    contest_id: &str,
    force: bool,
    baseline: RefreshBaseline,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<PreparedRefresh, AppError> {
    let data = fetch_contest_data(contest_id, atcoder, reporter)?;
    workspace::validate_contest_identity(&data.contest, contest_id)?;
    workspace::validate_contest_paths(&data.contest)?;

    // Fetching may take long enough for the destination to change. Revalidate
    // immediately before choosing it as the staging parent.
    baseline.revalidate_diagnosis(destination, contest_id)?;
    baseline.revalidate(destination, contest_id, force)?;

    let staging = crate::safe_file::PreservedTempDir::new(
        tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(destination)?,
    );

    // Creating the staging directory is path-based. Recheck immediately so a
    // replaced destination cannot cause staging to be built in another contest.
    baseline.revalidate(destination, contest_id, force)?;

    data.stage(staging.path())?;

    baseline.revalidate(destination, contest_id, force)?;

    Ok(PreparedRefresh {
        destination: destination.to_path_buf(),
        contest_id: contest_id.to_owned(),
        force,
        baseline,
        staging,
    })
}

pub(crate) fn apply_refresh(
    prepared: PreparedRefresh,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let PreparedRefresh {
        destination,
        contest_id,
        force,
        baseline,
        staging,
    } = prepared;

    let verify_diagnosis = baseline.diagnosis.is_some();
    baseline.revalidate_diagnosis(&destination, &contest_id)?;
    baseline.revalidate(&destination, &contest_id, force)?;
    workspace::replace_refresh_data_with_snapshot(
        &destination,
        staging.into_temp_dir(),
        force,
        baseline.metadata_snapshot,
    )?;
    if verify_diagnosis
        && !matches!(
            diagnose_contest(&destination, &contest_id),
            ContestDiagnosis::Healthy { .. }
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("refresh installed new data for {contest_id}, but local verification failed"),
        )
        .into());
    }
    reporter.report(Event::WorkspaceRefreshed {
        destination: &destination,
    });

    Ok(())
}

fn refresh_metadata_snapshot(destination: &Path) -> io::Result<Option<FileSnapshot>> {
    safe_file::read_optional_regular_file_snapshot(&destination.join(".atc").join("contest.toml"))
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

impl RefreshBaseline {
    fn revalidate(
        &self,
        destination: &Path,
        contest_id: &str,
        force: bool,
    ) -> Result<(), AppError> {
        if refresh_destination_identity(destination)? != self.destination_identity {
            return Err(changed_refresh_destination_error());
        }
        validate_refresh_destination(destination, contest_id, force)?;
        let current_metadata = if self.diagnosis.is_some() {
            workspace::read_contest_metadata_snapshot(destination)?
        } else {
            refresh_metadata_snapshot(destination)?
        };
        if current_metadata != self.metadata_snapshot {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "contest metadata changed while refresh was being prepared; retry refresh",
            )
            .into());
        }
        workspace::validate_existing_contest_identity(destination, contest_id)?;
        if refresh_destination_identity(destination)? != self.destination_identity {
            return Err(changed_refresh_destination_error());
        }
        Ok(())
    }

    fn revalidate_diagnosis(&self, destination: &Path, contest_id: &str) -> Result<(), AppError> {
        if let Some(expected) = &self.diagnosis
            && diagnose_contest(destination, contest_id) != *expected
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "local contest diagnosis changed while refresh was being prepared; retry refresh",
            )
            .into());
        }
        Ok(())
    }
}

fn blocked_refresh_error(contest_id: &str, reason: &BlockReason) -> AppError {
    let (kind, message) = reason.error_details();
    io::Error::new(
        kind,
        format!("refresh refused by local diagnosis for {contest_id}: {message}"),
    )
    .into()
}
