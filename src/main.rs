use clap::{Parser, Subcommand};

mod atcoder;
mod commands;
mod error;
mod model;
mod workspace;
use crate::error::AppError;

/// AtCoder用CLIツール
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    New { contest: String },
    Run { problem: String },
    Login,
}

fn main() -> Result<(), AppError> {
    let cli = Cli::parse();

    match cli.command {
        Commands::New { contest } => {
            commands::new(contest)?;
        }
        Commands::Run { problem } => {
            println!("run problem: {problem}");
        }
        Commands::Login => {
            println!("login");
        }
    }
    Ok(())
}
