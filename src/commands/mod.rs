mod create;
mod new;
mod refresh;
mod test;
mod watch;
mod watch_source;
mod watch_tui;

pub(crate) use create::create;
pub(crate) use new::new;
pub(crate) use refresh::refresh;
pub(crate) use test::test;
pub(crate) use watch::watch;
pub(crate) use watch_tui::watch_tui;

use crate::atcoder;
use crate::config::Config;
use crate::error::AppError;
use crate::language::Language;
use crate::model::{Contest, Sample};
use crate::ui::{Event, Reporter};

#[cfg(test)]
use create::create_at;
#[cfg(test)]
use new::new_at;
#[cfg(test)]
use refresh::{refresh_at, resolve_refresh_contest_id};
#[cfg(test)]
use test::{
    find_problem, report_case_result, report_compile_result, run_test_cases, test_problem,
    test_problem_with_debug_header, validate_debug_language,
};

#[cfg(test)]
use crate::config::RunnerConfig;
#[cfg(test)]
use crate::runner::{self, ExecutionOutcome};
#[cfg(test)]
use crate::template::builtin_template;
#[cfg(test)]
use std::io;
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::time::Duration;

struct FetchedContestData {
    contest: Contest,
    samples_by_problem: Vec<Option<Vec<Sample>>>,
}

fn fetch_contest_data(
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<FetchedContestData, AppError> {
    reporter.report(Event::ContestFetching { contest_id });

    let contest = atcoder.fetch_contest(contest_id)?;

    reporter.report(Event::ContestFetched {
        contest_id: &contest.contest_id,
        problems: contest.problems.len(),
    });

    let total = contest.problems.len();
    let mut samples_by_problem = Vec::with_capacity(total);

    for (i, problem) in contest.problems.iter().enumerate() {
        reporter.report(Event::ProblemFetching {
            index: &problem.index,
            current: i + 1,
            total,
        });
        match atcoder.fetch_samples(problem) {
            Ok(samples) => {
                reporter.report(Event::ProblemFetched {
                    index: &problem.index,
                    samples: samples.len(),
                });
                samples_by_problem.push(Some(samples));
            }

            Err(err) => {
                let message = err.to_string();

                reporter.report(Event::ProblemFetchFailed {
                    index: &problem.index,
                    error: &message,
                });
                samples_by_problem.push(None);
            }
        }
    }
    Ok(FetchedContestData {
        contest,
        samples_by_problem,
    })
}

fn resolve_language(cli_language: Option<Language>, config: &Config) -> Language {
    cli_language.unwrap_or(config.defaults.language)
}

#[cfg(test)]
mod tests;
