use super::resolve_language;
use crate::config::Config;
use crate::error::AppError;
use crate::language::Language;
use crate::template::builtin_template;
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

    create_at(&cwd, name, specified_language, &config, reporter)
}

pub(super) fn create_at(
    destination: &Path,
    name: &str,
    specified_language: Option<Language>,
    config: &Config,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let language = resolve_language(specified_language, config);
    let template = builtin_template(language);

    let path = workspace::create_source_file(destination, name, language, template)?;

    reporter.report(Event::SourceCreated { path: &path });

    Ok(())
}
