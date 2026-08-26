mod attempt_executor;
mod config;
mod contest;
mod create;
mod doctor;
mod init;
mod login;
mod new;
mod refresh;
mod run_scheduler;
pub(crate) mod stress;
mod template;
mod test;
mod watch;
mod watch_source;
mod watch_tui;
mod watch_worker;

pub(crate) use config::config_init;
pub(crate) use contest::contest;
pub(crate) use create::{create, create_source};
pub(crate) use doctor::doctor;
pub(crate) use init::init;
pub(crate) use login::login;
pub(crate) use new::new;
pub(crate) use refresh::refresh;
pub(crate) use stress::{stress, stress_init};
pub(crate) use template::template_init;
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
use new::new_at_with_install_hook;
#[cfg(test)]
use new::{new_at, new_at_in_workspace, prepare_new};
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
use crate::template::{builtin_template, resolve_source_template_in};
#[cfg(test)]
use std::io;
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::time::Duration;

struct FetchedContestData {
    contest: Contest,
    samples_by_problem: Vec<Vec<Sample>>,
}

fn fetch_contest_data(
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<FetchedContestData, AppError> {
    reporter.report(Event::ContestFetching { contest_id });

    let outline = atcoder.fetch_contest(contest_id)?;

    reporter.report(Event::ContestFetched {
        contest_id: &outline.contest_id,
        problems: outline.problems.len(),
    });

    for problem in &outline.problems {
        crate::workspace::validate_problem_index(&problem.index)?;
    }

    let total = outline.problems.len();
    let mut samples_by_problem = Vec::with_capacity(total);
    let mut problems = Vec::with_capacity(total);

    for (i, problem) in outline.problems.into_iter().enumerate() {
        reporter.report(Event::ProblemFetching {
            index: &problem.index,
            current: i + 1,
            total,
        });
        match atcoder.fetch_samples(&problem) {
            Ok(samples) => {
                reporter.report(Event::ProblemFetched {
                    index: &problem.index,
                    samples: samples.len(),
                });
                problems.push(crate::model::Problem {
                    index: problem.index,
                    title: problem.title,
                    task_id: problem.task_id,
                    url: problem.url,
                    sample_count: samples.len(),
                });
                samples_by_problem.push(samples);
            }

            Err(err) => {
                let message = err.to_string();

                reporter.report(Event::ProblemFetchFailed {
                    index: &problem.index,
                    error: &message,
                });
                return Err(err.into());
            }
        }
    }
    let contest = Contest {
        contest_id: outline.contest_id,
        problems,
    };
    Ok(FetchedContestData {
        contest,
        samples_by_problem,
    })
}

fn fetch_samples_for_manifest(
    contest: &Contest,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<Vec<Vec<Sample>>, AppError> {
    let total = contest.problems.len();
    let mut samples_by_problem = Vec::with_capacity(total);

    for (i, problem) in contest.problems.iter().enumerate() {
        if problem.sample_count == 0 {
            samples_by_problem.push(Vec::new());
            continue;
        }

        reporter.report(Event::ProblemFetching {
            index: &problem.index,
            current: i + 1,
            total,
        });
        let outline = atcoder::ProblemOutline::from(problem);
        let samples = match atcoder.fetch_samples(&outline) {
            Ok(samples) => samples,
            Err(error) => {
                let message = error.to_string();
                reporter.report(Event::ProblemFetchFailed {
                    index: &problem.index,
                    error: &message,
                });
                return Err(error.into());
            }
        };
        reporter.report(Event::ProblemFetched {
            index: &problem.index,
            samples: samples.len(),
        });

        if samples.len() != problem.sample_count {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "remote sample count for problem {} is {}, but the local manifest requires {}; run `atc refresh` to synchronize remote changes",
                    problem.index,
                    samples.len(),
                    problem.sample_count
                ),
            )
            .into());
        }

        samples_by_problem.push(samples);
    }

    Ok(samples_by_problem)
}

fn resolve_language(cli_language: Option<Language>, config: &Config) -> Language {
    cli_language.unwrap_or(config.defaults.language)
}

#[cfg(test)]
mod tests;
