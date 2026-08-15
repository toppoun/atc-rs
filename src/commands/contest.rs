use super::resolve_language;
use crate::atcoder;
use crate::config::Config;
use crate::error::AppError;
use crate::ui::Reporter;
use crate::workspace::{self, ContestMetadataHealth};
use std::io::{self, Write};
use std::path::Path;

pub(crate) fn contest(contest_id: &str, reporter: &mut dyn Reporter) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    let destination = workspace::resolve_contest_path(&cwd, contest_id)?;

    if !workspace::contest_directory_exists(&destination)? {
        create_contest(&destination, contest_id, reporter)?;
    } else {
        match workspace::inspect_contest_metadata(&destination)? {
            ContestMetadataHealth::Healthy(contest) => {
                if contest.contest_id != contest_id {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "contest ID mismatch: requested {contest_id:?}, \
                             but metadata contains {:?}",
                            contest.contest_id
                        ),
                    )
                    .into());
                }

                workspace::validate_contest_paths(&contest)?;
            }

            ContestMetadataHealth::Missing | ContestMetadataHealth::Invalid => {
                if !confirm_repair(&destination)? {
                    return Ok(());
                }

                repair_contest(&destination, contest_id, reporter)?;
            }

            ContestMetadataHealth::UnsupportedVersion(version) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "unsupported contest metadata version: {version}; \
                         refusing to repair automatically"
                    ),
                )
                .into());
            }
        }
    }

    super::watch_tui::watch_tui_at(&destination)
}

fn create_contest(
    destination: &Path,
    contest_id: &str,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let config = Config::load()?;
    let language = resolve_language(None, &config);
    let atcoder = create_atcoder_client()?;

    super::new::new_at(destination, contest_id, language, &atcoder, reporter)
}

fn repair_contest(
    destination: &Path,
    contest_id: &str,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let atcoder = create_atcoder_client()?;

    super::refresh::refresh_at(destination, contest_id, true, &atcoder, reporter)
}

fn create_atcoder_client() -> Result<atcoder::AtCoderClient, AppError> {
    if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        Ok(atcoder::AtCoderClient::fixture(path))
    } else {
        Ok(atcoder::AtCoderClient::new()?)
    }
}

fn confirm_repair(destination: &Path) -> io::Result<bool> {
    let metadata = destination.join(".atc").join("contest.toml");

    print!(
        "contest metadata is missing or invalid:\n{}\nRepair contest metadata and samples? [y/N] ",
        metadata.display()
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(matches!(input.trim(), "y" | "Y"))
}
