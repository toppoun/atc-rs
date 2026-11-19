use super::{ContestUpdateChecks, fetch_contest_data};
use crate::atcoder;
use crate::auth::SessionAuth;
use crate::error::AppError;
use crate::safe_file::{self, FileFingerprint, FileSnapshot, TreeFingerprint};
use crate::ui::{Event, Reporter};
use crate::workspace;
use crate::workspace::diagnosis::{
    BlockReason, ContestDiagnosis, MigrationReason, RepairReason, diagnose_contest,
};
use crate::workspace::{
    PhysicalDirectoryIdentity, contest_directory_identity, validate_refresh_destination,
};
use std::borrow::Borrow;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) struct PreparedRefresh {
    destination: PathBuf,
    contest_id: String,
    force: bool,
    baseline: RefreshBaseline,
    staging: crate::safe_file::PreservedTempDir,
}

pub(super) struct RefreshBaseline {
    destination_identity: PhysicalDirectoryIdentity,
    metadata_snapshot: Option<FileFingerprint>,
    diagnosis: Option<RefreshDiagnosis>,
    tests: Option<RefreshTests>,
}

// A CLI update checks the original generation, including when `tests/` was absent.
struct RefreshTests(Option<TreeFingerprint>);

enum RefreshDiagnosis {
    Healthy,
    RepairRequired(RepairReason),
}

impl RefreshDiagnosis {
    fn for_update(diagnosis: ContestDiagnosis) -> Self {
        match diagnosis {
            ContestDiagnosis::Healthy { .. } => Self::Healthy,
            ContestDiagnosis::RepairRequired(reason) => Self::RepairRequired(reason),
            _ => unreachable!("only healthy or repairable contests can reach refresh planning"),
        }
    }

    fn matches(&self, diagnosis: ContestDiagnosis) -> bool {
        match (self, diagnosis) {
            (Self::Healthy, ContestDiagnosis::Healthy { .. }) => true,
            (Self::RepairRequired(expected), ContestDiagnosis::RepairRequired(actual)) => {
                *expected == actual
            }
            _ => false,
        }
    }
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
    let route = specified_contest_id
        .map(|id| workspace::ExplicitContestRoute::resolve(cwd, id))
        .transpose()?;
    let destination = route
        .as_ref()
        .map_or(cwd, |route| route.destination.as_path());
    if route.is_none() {
        workspace::validate_workspace_update_target(destination)?;
    }
    // Capture the directory and metadata before diagnosis so the decision cannot
    // authorize an update to a different directory or manifest generation.
    let destination_identity = contest_directory_identity(destination);
    let metadata_snapshot = workspace::read_contest_metadata_snapshot(destination);
    let contest_id =
        resolve_refresh_contest_id(destination, specified_contest_id, force, &metadata_snapshot)?;
    if !workspace::is_safe_atcoder_id(&contest_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid contest ID: {contest_id:?}"),
        )
        .into());
    }
    let diagnosis = diagnose_contest(destination, &contest_id);
    let healthy = match &diagnosis {
        ContestDiagnosis::Healthy { .. } => true,
        ContestDiagnosis::RepairRequired(_) => false,
        ContestDiagnosis::MissingDirectory => {
            return Err(workspace::missing_contest_directory(destination).into());
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
        metadata_snapshot: metadata_snapshot?.as_ref().map(FileSnapshot::fingerprint),
        diagnosis: Some(RefreshDiagnosis::for_update(diagnosis)),
        tests: None,
    };
    if healthy && !force {
        baseline.revalidate(destination, &contest_id, force)?;
        reporter.report(Event::WorkspaceRefreshSkipped {
            contest_id: &contest_id,
        });
        return Ok(());
    }

    let baseline = baseline.capture_update(destination, &contest_id, force)?;
    let validate_route = |destination: &Path| match &route {
        Some(route) => route.revalidate(cwd, &contest_id),
        None => workspace::revalidate_implicit_contest_route(destination, &contest_id),
    };
    execute_cli_refresh(
        destination,
        &contest_id,
        force,
        baseline,
        create_client,
        reporter,
        ContestUpdateChecks {
            validate_route: &validate_route,
            ..ContestUpdateChecks::default()
        },
    )
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
    let name = workspace::implicit_contest_name(destination)?;
    if workspace::has_uppercase(&name) || force {
        let metadata = metadata_snapshot
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
        return Ok(workspace::infer_contest_id_from_metadata(
            destination,
            &name,
            metadata.as_ref(),
        )?);
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
    let destination_identity = contest_directory_identity(destination)?;
    validate_refresh_destination(destination, contest_id, force)?;
    let baseline = RefreshBaseline {
        destination_identity,
        metadata_snapshot: refresh_metadata_snapshot(destination)?
            .as_ref()
            .map(FileSnapshot::fingerprint),
        diagnosis: None,
        tests: None,
    };
    baseline.revalidate(destination, contest_id, force)?;
    prepare_refresh_from_baseline(
        destination,
        contest_id,
        force,
        baseline,
        atcoder,
        reporter,
        ContestUpdateChecks::default(),
    )
}

fn prepare_refresh_from_baseline(
    destination: &Path,
    contest_id: &str,
    force: bool,
    baseline: RefreshBaseline,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
    checks: ContestUpdateChecks<'_>,
) -> Result<PreparedRefresh, AppError> {
    let data = fetch_contest_data(contest_id, atcoder, reporter)?;
    workspace::validate_contest_identity(&data.contest, contest_id)?;
    workspace::validate_contest_paths(&data.contest)?;

    // Fetching may take long enough for the destination to change. Revalidate
    // immediately before choosing it as the staging parent.
    baseline.revalidate_update(destination, contest_id, force, checks)?;
    checks.cancellation.before_staging()?;

    let staging = crate::safe_file::PreservedTempDir::new(
        tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(destination)?,
    );

    // Creating the staging directory is path-based. Recheck immediately so a
    // replaced destination cannot cause staging to be built in another contest.
    (checks.validate_route)(destination)?;
    baseline.revalidate(destination, contest_id, force)?;

    data.stage(staging.path())?;

    (checks.validate_route)(destination)?;
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
    apply_refresh_checked(prepared, reporter, ContestUpdateChecks::default())
}

fn apply_refresh_checked(
    prepared: PreparedRefresh,
    reporter: &mut dyn Reporter,
    checks: ContestUpdateChecks<'_>,
) -> Result<(), AppError> {
    checks.cancellation.before_apply(prepared.staging.path())?;
    (checks.validate_route)(&prepared.destination)?;
    let PreparedRefresh {
        destination,
        contest_id,
        force,
        baseline,
        staging,
    } = prepared;

    let verify_diagnosis = baseline.diagnosis.is_some();
    baseline.revalidate_diagnosis(&destination, &contest_id)?;
    let metadata = baseline.revalidate(&destination, &contest_id, force)?;
    workspace::replace_refresh_data_with_snapshot(
        &destination,
        staging.into_temp_dir(),
        force,
        metadata,
        |tests| {
            (checks.validate_route)(&destination)?;
            validate_destination_identity(&destination, &baseline.destination_identity)?;
            if let Some(expected) = &baseline.tests
                && !safe_file::matches_tree_snapshot(expected.0.as_ref(), tests)
            {
                return Err(changed_tests());
            }
            Ok(())
        },
        checks.cancellation,
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

pub(super) fn plan_refresh_update(
    destination: &Path,
    contest_id: &str,
    force: bool,
    diagnosis: ContestDiagnosis,
) -> Result<RefreshBaseline, AppError> {
    RefreshBaseline {
        destination_identity: contest_directory_identity(destination)?,
        metadata_snapshot: workspace::read_contest_metadata_snapshot(destination)?
            .as_ref()
            .map(FileSnapshot::fingerprint),
        diagnosis: Some(RefreshDiagnosis::for_update(diagnosis)),
        tests: None,
    }
    .capture_update(destination, contest_id, force)
}

pub(super) fn execute_cli_refresh<C: Borrow<atcoder::AtCoderClient>>(
    destination: &Path,
    contest_id: &str,
    force: bool,
    baseline: RefreshBaseline,
    client: impl FnOnce() -> Result<C, AppError>,
    reporter: &mut dyn Reporter,
    checks: ContestUpdateChecks<'_>,
) -> Result<(), AppError> {
    baseline.revalidate_update(destination, contest_id, force, checks)?;
    checks.cancellation.before_fetch()?;
    let atcoder = client()?;
    let prepared = prepare_refresh_from_baseline(
        destination,
        contest_id,
        force,
        baseline,
        atcoder.borrow(),
        reporter,
        checks,
    )?;
    apply_refresh_checked(prepared, reporter, checks)
}

fn changed_tests() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "local tests changed while refresh was being prepared; inspect the contest before retrying",
    )
}

fn refresh_metadata_snapshot(destination: &Path) -> io::Result<Option<FileSnapshot>> {
    safe_file::read_optional_regular_file_snapshot(&destination.join(".atc").join("contest.toml"))
}

fn validate_destination_identity(
    destination: &Path,
    expected: &PhysicalDirectoryIdentity,
) -> io::Result<()> {
    if &contest_directory_identity(destination)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "contest destination changed while refresh was being prepared; retry refresh",
        ));
    }
    Ok(())
}

impl RefreshBaseline {
    /// Capture the local test tree and validate the lock entry without opening or creating it.
    /// Both single and batch CLI refresh use this only for targets that will be updated.
    fn capture_update(
        mut self,
        destination: &Path,
        contest_id: &str,
        force: bool,
    ) -> Result<Self, AppError> {
        self.tests = Some(RefreshTests(workspace::fingerprint_contest_tests(
            destination,
        )?));
        self.revalidate_diagnosis(destination, contest_id)?;
        self.revalidate(destination, contest_id, force)?;
        Ok(self)
    }

    /// Recheck the route and the original local generation around the download. The final
    /// tests comparison uses the replacement routine's snapshot taken under the update lock.
    fn revalidate_update(
        &self,
        destination: &Path,
        contest_id: &str,
        force: bool,
        checks: ContestUpdateChecks<'_>,
    ) -> Result<(), AppError> {
        (checks.validate_route)(destination)?;
        self.revalidate_diagnosis(destination, contest_id)?;
        self.revalidate(destination, contest_id, force)?;
        if let Some(expected) = &self.tests
            && workspace::fingerprint_contest_tests(destination)? != expected.0
        {
            return Err(changed_tests().into());
        }
        Ok(())
    }

    fn revalidate(
        &self,
        destination: &Path,
        contest_id: &str,
        force: bool,
    ) -> Result<Option<FileSnapshot>, AppError> {
        validate_destination_identity(destination, &self.destination_identity)?;
        validate_refresh_destination(destination, contest_id, force)?;
        let current_metadata = if self.diagnosis.is_some() {
            workspace::read_contest_metadata_snapshot(destination)?
        } else {
            refresh_metadata_snapshot(destination)?
        };
        if current_metadata.as_ref().map(FileSnapshot::fingerprint) != self.metadata_snapshot {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "contest metadata changed while refresh was being prepared; retry refresh",
            )
            .into());
        }
        workspace::validate_existing_contest_identity(destination, contest_id)?;
        validate_destination_identity(destination, &self.destination_identity)?;
        Ok(current_metadata)
    }

    fn revalidate_diagnosis(&self, destination: &Path, contest_id: &str) -> Result<(), AppError> {
        if let Some(expected) = &self.diagnosis
            && !expected.matches(diagnose_contest(destination, contest_id))
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
