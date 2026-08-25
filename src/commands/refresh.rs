use super::{FetchedContestData, fetch_contest_data};
use crate::atcoder;
use crate::error::AppError;
use crate::ui::{Event, Reporter};
use crate::workspace;
use crate::workspace::validate_refresh_destination;
use std::io;
use std::path::Path;

pub(crate) fn refresh(
    contest: Option<String>,
    force: bool,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::resolve_contest_target(&cwd, contest.as_deref())?;

    let contest_id = resolve_refresh_contest_id(&destination, contest.as_deref(), force)?;

    let atcoder = if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)
    } else {
        atcoder::AtCoderClient::new()?
    };

    refresh_at(&destination, &contest_id, force, &atcoder, reporter)
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
    validate_refresh_destination(destination, contest_id, force)?;

    let FetchedContestData {
        contest,
        samples_by_problem,
    } = fetch_contest_data(contest_id, atcoder, reporter)?;
    workspace::validate_contest_identity(&contest, contest_id)?;
    workspace::validate_contest_paths(&contest)?;

    // Fetching may take long enough for the destination to change. Revalidate
    // immediately before choosing it as the staging parent.
    validate_refresh_destination(destination, contest_id, force)?;

    let staging = tempfile::Builder::new()
        .prefix(".atc-refresh-")
        .tempdir_in(destination)?;

    for (problem, samples) in contest.problems.iter().zip(samples_by_problem) {
        workspace::save_samples(staging.path(), problem, &samples)?;
    }
    workspace::save_metadata(staging.path(), &contest)?;

    workspace::replace_refresh_data(destination, staging, force)?;
    reporter.report(Event::WorkspaceRefreshed { destination });

    Ok(())
}
