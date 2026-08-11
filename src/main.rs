use clap::Parser;
use std::process::ExitCode;

mod atcoder;
mod cli;
mod commands;
mod error;
mod model;
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
        cli::Command::Run { problem } => {
            println!("run problem: {problem}");
        }
        cli::Command::Login => {
            println!("login");
        }
    }
    Ok(())
}
