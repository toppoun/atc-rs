use crate::doctor::{self, DoctorPaths, ProcessRunnerProbe, SystemInfo};
use crate::error::AppError;
use crate::ui::{Event, Reporter};

pub(crate) fn doctor(reporter: &mut dyn Reporter) -> Result<bool, AppError> {
    let cwd = std::env::current_dir()?;
    let paths = DoctorPaths {
        config_file: crate::paths::config_file().map_err(|error| error.to_string()),
        templates_dir: crate::paths::source_templates_dir().map_err(|error| error.to_string()),
    };
    let mut runner_probe = ProcessRunnerProbe;
    let report = doctor::inspect(&cwd, paths, SystemInfo::current(), &mut runner_probe);
    let success = report.is_success();

    reporter.report(Event::DoctorReport { report: &report });

    Ok(success)
}
