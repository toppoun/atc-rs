use crate::language::Language;
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
    Test {
        problem: String,

        #[arg(short = 'l', long = "language")]
        language: Option<Language>,
    },
    Login,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_refresh_with_and_without_contest_override() {
        let cli =
            Cli::try_parse_from(["atc", "refresh"]).expect("refresh without override should parse");
        assert!(matches!(cli.command, Command::Refresh { contest: None }));

        let cli = Cli::try_parse_from(["atc", "refresh", "-c", "abc466"])
            .expect("refresh with override should parse");
        assert!(matches!(
            cli.command,
            Command::Refresh { contest: Some(contest) } if contest == "abc466"
        ));
    }
}
