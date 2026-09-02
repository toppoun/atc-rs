use std::process::ExitCode;

mod app_context;
mod atcoder;
// S2でphysical attemptのcancel/outcome境界だけを実装する。
// production workerへの接続は後続段階で行う。
#[cfg_attr(not(test), allow(dead_code))]
mod attempt;
mod auth;
mod cli;
mod commands;
mod comparator;
mod config;
mod debug;
mod doctor;
// This slice establishes the boundary for later frontend actions; no production caller exists yet.
#[allow(dead_code)]
mod editor;
mod error;
mod language;
mod model;
mod paths;
mod runner;
mod safe_file;
mod stress;
mod template;
mod tui;
mod ui;
mod user_config_fs;
// Phase 1 establishes persistence for a later TUI feature; no production caller exists yet.
#[cfg_attr(not(test), allow(dead_code))]
mod user_input;
mod watcher;
mod workspace;
use crate::error::AppError;
use ui::{Event, Reporter, TerminalReporter};

// Only the one-shot CLI test/stress commands translate verdicts to process status. The
// normal reporter still receives each event exactly once; TUI/plain watch keep their
// existing outcome and operational-error semantics.
fn run_verdict_command(
    reporter: &mut dyn Reporter,
    command: impl FnOnce(&mut dyn Reporter) -> Result<(), AppError>,
) -> Result<ExitCode, AppError> {
    struct VerdictReporter<'a> {
        inner: &'a mut dyn Reporter,
        failed: bool,
    }
    impl Reporter for VerdictReporter<'_> {
        fn report(&mut self, event: Event<'_>) {
            self.failed |= matches!(
                event,
                Event::CompileFailed { .. }
                    | Event::CompileTimedOut { .. }
                    | Event::TestCaseWrongAnswer { .. }
                    | Event::TestCaseRuntimeError { .. }
                    | Event::TestCaseTimedOut { .. }
                    | Event::StressFailed { .. }
                    | Event::StressCancelled { .. }
            );
            self.inner.report(event);
        }
    }
    let mut verdict = VerdictReporter {
        inner: reporter,
        failed: false,
    };
    command(&mut verdict)?;
    Ok(if verdict.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, AppError> {
    let cli = cli::Cli::parse();

    let mut reporter = TerminalReporter::default();

    match cli.command {
        cli::Command::Workspace(cli::WorkspaceCommand::Init) => {
            commands::init(&mut reporter)?;
        }

        cli::Command::Configuration(cli::ConfigurationCommand::Config {
            command: cli::ConfigCommand::Init,
        }) => {
            commands::config_init(&mut reporter)?;
        }

        cli::Command::Contest(cli::ContestCommand::New { contest, language }) => {
            commands::new(&contest, language, &mut reporter)?;
        }

        cli::Command::Contest(cli::ContestCommand::Refresh { contest, force }) => {
            commands::refresh(contest, force, &mut reporter)?;
        }

        cli::Command::Contest(cli::ContestCommand::Contest { contest_id }) => {
            commands::contest(&contest_id, &mut reporter)?;
        }

        cli::Command::RunTest(cli::RunTestCommand::Test {
            problem,
            contest,
            language,
            debug,
        }) => {
            return run_verdict_command(&mut reporter, |reporter| {
                commands::test(&problem, contest.as_deref(), language, debug, reporter)
            });
        }

        cli::Command::RunTest(cli::RunTestCommand::Watch { plain, contest }) => {
            if plain {
                commands::watch(contest.as_deref(), &mut reporter)?;
            } else {
                commands::watch_tui(contest.as_deref())?;
            }
        }

        cli::Command::RunTest(cli::RunTestCommand::Stress(args)) => match args.command {
            Some(cli::StressSubcommand::Init(init)) => {
                commands::stress_init(&init.problem, init.contest.as_deref(), &mut reporter)?;
            }
            None => {
                let problem = args
                    .run
                    .problem
                    .expect("clap requires a problem when no stress subcommand is selected");
                return run_verdict_command(&mut reporter, |reporter| {
                    commands::stress(
                        &problem,
                        args.run.contest.as_deref(),
                        args.run.language,
                        args.run.debug,
                        args.run.count,
                        args.run.forever,
                        args.run.seed,
                        reporter,
                    )
                });
            }
        },

        cli::Command::RunTest(cli::RunTestCommand::Submit {
            problem,
            contest,
            language,
        }) => {
            commands::submit(&problem, contest.as_deref(), language)?;
        }

        cli::Command::Files(cli::FileCommand::Create { name, language }) => {
            commands::create(&name, language, &mut reporter)?;
        }

        cli::Command::Files(cli::FileCommand::Template {
            command: cli::TemplateCommand::Init { language },
        }) => {
            commands::template_init(language, &mut reporter)?;
        }

        cli::Command::Account(cli::AccountCommand::Login) => {
            commands::login()?;
        }

        cli::Command::Diagnostics(cli::DiagnosticsCommand::Doctor) => {
            if !commands::doctor(&mut reporter)? {
                return Ok(ExitCode::FAILURE);
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}
