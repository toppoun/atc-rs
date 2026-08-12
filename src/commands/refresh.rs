use super::{FetchedContestData, fetch_contest_data};
use crate::atcoder;
use crate::error::AppError;
use crate::ui::{Event, Reporter};
use crate::workspace;
use crate::workspace::validate_refresh_destination;
use std::path::Path;

pub(crate) fn refresh(
    contest: Option<String>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let contest_id = resolve_refresh_contest_id(&cwd, contest.as_deref())?;

    let atcoder = if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)
    } else {
        atcoder::AtCoderClient::new()?
    };

    refresh_at(&cwd, &contest_id, &atcoder, reporter)
}

pub(super) fn resolve_refresh_contest_id(
    destination: &Path,
    specified_contest_id: Option<&str>,
) -> Result<String, AppError> {
    match specified_contest_id {
        Some(contest_id) => {
            validate_refresh_destination(destination, contest_id)?;
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
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    workspace::validate_workspace_marker(destination)?;

    let FetchedContestData {
        contest,
        samples_by_problem,
    } = fetch_contest_data(contest_id, atcoder, reporter)?;
    workspace::validate_contest_paths(&contest)?;

    let staging = tempfile::Builder::new()
        .prefix(".atc-refresh-")
        .tempdir_in(destination)?;

    for (problem, samples) in contest.problems.iter().zip(samples_by_problem) {
        if let Some(samples) = samples {
            workspace::save_samples(staging.path(), problem, &samples)?;
        }
    }
    workspace::save_metadata(staging.path(), &contest)?;

    workspace::replace_refresh_data(destination, staging)?;
    reporter.report(Event::WorkspaceRefreshed { destination });

    Ok(())
}
