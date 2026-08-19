use clap::Parser;
use std::process::ExitCode;

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
mod error;
mod language;
mod model;
mod paths;
mod runner;
mod template;
mod tui;
mod ui;
mod watcher;
mod workspace;
use crate::error::AppError;
use ui::TerminalReporter;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), AppError> {
    let cli = cli::Cli::parse();

    let mut reporter = TerminalReporter::default();

    match cli.command {
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
            commands::test(&problem, contest.as_deref(), language, debug, &mut reporter)?;
        }

        cli::Command::RunTest(cli::RunTestCommand::Watch { plain, contest }) => {
            if plain {
                commands::watch(contest.as_deref(), &mut reporter)?;
            } else {
                commands::watch_tui(contest.as_deref())?;
            }
        }

        cli::Command::Files(cli::FileCommand::Create { name, language }) => {
            commands::create(&name, language, &mut reporter)?;
        }

        cli::Command::Account(cli::AccountCommand::Login) => {
            commands::login()?;
        }
    }

    Ok(())
}
