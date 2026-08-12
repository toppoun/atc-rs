use crate::error::AppError;
use crate::workspace;

pub(crate) fn watch_tui() -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    workspace::validate_workspace_marker(&cwd)?;

    let contest = workspace::load_metadata(&cwd)?;
    workspace::validate_contest_paths(&contest)?;

    let sample_counts = contest
        .problems
        .iter()
        .map(|problem| workspace::load_samples(&cwd, &problem.index).map(|samples| samples.len()))
        .collect::<Result<Vec<_>, _>>()?;

    let mut terminal = ratatui::init();

    let result = crate::tui::run(&mut terminal, &contest, sample_counts);

    ratatui::restore();

    result?;

    Ok(())
}
