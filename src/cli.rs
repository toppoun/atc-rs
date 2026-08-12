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

        #[arg(short, long)]
        debug: bool,
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

    #[test]
    fn parses_test_problem_and_optional_language() {
        for problem in ["A", "a"] {
            let cli = Cli::try_parse_from(["atc-rs", "test", problem]).unwrap();
            assert!(matches!(
                cli.command,
                Command::Test { problem: parsed, language: None, debug } if parsed == problem
            ));
        }

        let cli = Cli::try_parse_from(["atc-rs", "test", "A", "-l", "python"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Test {
                problem,
                language: Some(Language::Python), debug } if problem == "A"
        ));
    }

    #[test]
    fn test_language_rejects_unsupported_alias() {
        assert!(Cli::try_parse_from(["atc-rs", "test", "A", "-l", "py"]).is_err());
    }
}
