use super::resolve_language;
use crate::config::Config;
use crate::error::AppError;
use crate::language::Language;
use crate::template::resolve_source_template;
use crate::ui::{Event, Reporter};
use crate::workspace;
use std::path::Path;

pub(crate) fn create(
    name: &str,
    specified_language: Option<Language>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let config = Config::load()?;
    let cwd = std::env::current_dir()?;

    create_at(
        &cwd,
        name,
        specified_language,
        &config,
        resolve_source_template,
        reporter,
    )
}

pub(super) fn create_at<R>(
    destination: &Path,
    name: &str,
    specified_language: Option<Language>,
    config: &Config,
    resolve_template: R,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError>
where
    R: FnOnce(Language) -> Result<String, AppError>,
{
    let language = resolve_language(specified_language, config);
    let template = resolve_template(language)?;

    create_source_at(destination, name, language, &template, reporter)
}

fn create_source_at(
    destination: &Path,
    name: &str,
    language: Language,
    template: &str,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let path = workspace::create_source_file(destination, name, language, template)?;

    reporter.report(Event::SourceCreated { path: &path });

    Ok(())
}
