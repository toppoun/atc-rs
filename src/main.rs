use clap::Parser;
use std::process::ExitCode;

mod atcoder;
mod cli;
mod commands;
mod error;
mod language;
mod model;
mod paths;
mod template;
mod ui;
mod workspace;
use crate::error::AppError;
use ui::TerminalReporter;

fn main() -> ExitCode {
    println!("config dir : {}", paths::config_dir().unwrap().display());
    println!("config file: {}", paths::config_file().unwrap().display());

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
        cli::Command::Run { problem } => {
            println!("run problem: {problem}");
        }
        cli::Command::Login => {
            println!("login");
        }
    }
    Ok(())
}
