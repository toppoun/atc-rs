use super::{FetchedContestData, fetch_contest_data, resolve_language};
use crate::atcoder;
use crate::config::Config;
use crate::error::AppError;
use crate::language::Language;
use crate::template::builtin_template;
use crate::ui::{Event, Reporter};
use crate::workspace;
use std::path::Path;

pub(crate) fn new(
    contest_id: &str,
    cli_language: Option<Language>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::contest_path(&cwd, contest_id)?;

    if existing_contest_is_noop(&destination)? {
        return Ok(());
    }
    let config = Config::load()?;
    let language = resolve_language(cli_language, &config);

    let atcoder = if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)
    } else {
        atcoder::AtCoderClient::new()?
    };

    new_at(&destination, contest_id, language, &atcoder, reporter)
}

pub(super) fn new_at(
    destination: &Path,
    contest_id: &str,
    language: Language,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    if existing_contest_is_noop(destination)? {
        return Ok(());
    }

    let template = builtin_template(language);

    let FetchedContestData {
        contest,
        samples_by_problem,
    } = fetch_contest_data(contest_id, atcoder, reporter)?;

    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "contest destination has no parent directory: {}",
                destination.display()
            ),
        )
    })?;
    let staging = tempfile::Builder::new()
        .prefix(".atc-new-")
        .tempdir_in(parent)?;

    workspace::create_source_files(staging.path(), &contest.problems, language, template)?;
    for (problem, samples) in contest.problems.iter().zip(samples_by_problem) {
        if let Some(samples) = samples {
            workspace::save_samples(staging.path(), problem, &samples)?;
        }
    }
    workspace::save_metadata(staging.path(), &contest)?;

    // Another process may have created the contest while fixtures/HTTP were read.
    if existing_contest_is_noop(destination)? {
        return Ok(());
    }

    match std::fs::rename(staging.path(), destination) {
        Ok(()) => {
            drop(staging.keep());
            reporter.report(Event::WorkspaceCreated { destination });
            Ok(())
        }
        Err(_) if destination.is_dir() => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn existing_contest_is_noop(destination: &Path) -> std::io::Result<bool> {
    if !destination.try_exists()? {
        return Ok(false);
    }

    if destination.is_dir() {
        return Ok(true);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "contest destination exists but is not a directory: {}",
            destination.display()
        ),
    ))
}
