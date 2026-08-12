use clap::Parser;
use std::process::ExitCode;

mod atcoder;
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
        cli::Command::New { contest, language } => {
            commands::new(&contest, language, &mut reporter)?;
        }
        cli::Command::Refresh { contest, force } => {
            commands::refresh(contest, force, &mut reporter)?
        }
        cli::Command::Test {
            problem,
            language,
            debug,
        } => {
            commands::test(&problem, language, debug, &mut reporter)?;
        }
        cli::Command::Create { name, language } => {
            commands::create(&name, language, &mut reporter)?
        }
        cli::Command::Watch { tui: false } => {
            commands::watch(&mut reporter)?;
        }

        cli::Command::Watch { tui: true } => {
            commands::watch_tui()?;
        }
        cli::Command::Login => {
            println!("login");
        }
    }
    Ok(())
}
