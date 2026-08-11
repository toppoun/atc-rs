use clap::{Parser, Subcommand};

/// AtCoder用CLIツール
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    New {
        contest: String,
    },
    Refresh {
        #[arg(short, long)]
        contest: Option<String>,
    },
    Run {
        problem: String,
    },
    Login,
}
