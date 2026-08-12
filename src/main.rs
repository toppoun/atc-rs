use clap::Parser;
use std::process::ExitCode;

mod atcoder;
mod cli;
mod commands;
mod comparator;
mod config;
mod error;
mod language;
mod model;
mod paths;
mod runner;
mod template;
mod ui;
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

    let mut reporter = TerminalReporter;

    match cli.command {
        cli::Command::New { contest } => {
            commands::new(&contest, &mut reporter)?;
        }
        cli::Command::Refresh { contest } => commands::refresh(contest, &mut reporter)?,
        cli::Command::Test { problem, language } => {
            commands::test(&problem, language, &mut reporter)?;
        }
        cli::Command::Login => {
            println!("login");
        }
    }
    Ok(())
}
