use super::{FetchedContestData, fetch_contest_data, resolve_language};
use crate::atcoder;
use crate::config::Config;
use crate::error::AppError;
use crate::language::Language;
use crate::template::resolve_source_template;
use crate::ui::{Event, Reporter};
use crate::workspace;
use std::fs;
use std::io;
use std::path::Path;

pub(crate) fn new(
    contest_id: &str,
    cli_language: Option<Language>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::contest_path(&cwd, contest_id)?;

    let Some((language, template)) = prepare_new(
        &destination,
        cli_language,
        Config::load,
        resolve_source_template,
    )?
    else {
        return Ok(());
    };

    let atcoder = if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        atcoder::AtCoderClient::fixture(path)
    } else {
        atcoder::AtCoderClient::new()?
    };

    new_at(
        &destination,
        contest_id,
        language,
        &template,
        &atcoder,
        reporter,
    )
}

pub(super) fn prepare_new<L, R>(
    destination: &Path,
    cli_language: Option<Language>,
    load_config: L,
    resolve_template: R,
) -> Result<Option<(Language, String)>, AppError>
where
    L: FnOnce() -> Result<Config, AppError>,
    R: FnOnce(Language) -> Result<String, AppError>,
{
    if existing_contest_is_noop(destination)? {
        return Ok(None);
    }

    let config = load_config()?;
    let language = resolve_language(cli_language, &config);
    let template = resolve_template(language)?;

    Ok(Some((language, template)))
}

pub(super) fn new_at(
    destination: &Path,
    contest_id: &str,
    language: Language,
    template: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    new_at_with_parent_preparation(
        destination,
        contest_id,
        language,
        template,
        atcoder,
        reporter,
        workspace::ensure_contest_parent,
    )
}

pub(super) fn new_at_in_workspace(
    root: &Path,
    destination: &Path,
    contest_id: &str,
    language: Language,
    template: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    new_at_with_parent_preparation(
        destination,
        contest_id,
        language,
        template,
        atcoder,
        reporter,
        |destination| workspace::ensure_workspace_contest_parent(root, contest_id, destination),
    )
}

fn new_at_with_parent_preparation(
    destination: &Path,
    contest_id: &str,
    language: Language,
    template: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
    prepare_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), AppError> {
    if existing_contest_is_noop(destination)? {
        return Ok(());
    }

    let FetchedContestData {
        contest,
        samples_by_problem,
    } = fetch_contest_data(contest_id, atcoder, reporter)?;
    workspace::validate_contest_identity(&contest, contest_id)?;

    prepare_parent(destination)?;
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

    match fs::rename(staging.path(), destination) {
        Ok(()) => {
            drop(staging.keep());
            reporter.report(Event::WorkspaceCreated { destination });
            Ok(())
        }
        Err(rename_error) => match existing_contest_is_noop(destination) {
            Ok(true) => Ok(()),
            Ok(false) => Err(rename_error.into()),
            Err(safety_error) => Err(safety_error.into()),
        },
    }
}

fn existing_contest_is_noop(destination: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "contest destination must not be a symbolic link: {}",
                destination.display()
            ),
        )),
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "contest destination exists but is not a directory: {}",
                destination.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
