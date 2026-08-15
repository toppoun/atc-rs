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

        #[arg(short = 'l', long)]
        language: Option<Language>,
    },
    Refresh {
        #[arg(short, long)]
        contest: Option<String>,

        /// Rebuild the current directory without trusting workspace metadata
        #[arg(short, long)]
        force: bool,
    },
    Test {
        problem: String,

        #[arg(short = 'c', long)]
        contest: Option<String>,

        #[arg(short = 'l', long = "language")]
        language: Option<Language>,

        #[arg(short, long)]
        debug: bool,
    },
    Create {
        name: String,

        #[arg(short = 'l', long)]
        language: Option<Language>,
    },
    Watch {
        #[arg(short = 'c', long)]
        contest: Option<String>,

        #[arg(long)]
        plain: bool,
    },
    Contest {
        contest_id: String,
    },
    Login,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn parses_new_contest_and_optional_language() {
        let cli = Cli::try_parse_from(["atc-rs", "new", "abc466"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::New {
                contest,
                language: None,
            } if contest == "abc466"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "new", "abc466", "-l", "python"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::New {
                contest,
                language: Some(Language::Python),
            } if contest == "abc466"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "new", "abc466", "--language", "cpp"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::New {
                contest,
                language: Some(Language::Cpp),
            } if contest == "abc466"
        ));
    }

    #[test]
    fn parses_refresh_with_and_without_contest_override() {
        let cli =
            Cli::try_parse_from(["atc", "refresh"]).expect("refresh without override should parse");
        assert!(matches!(
            cli.command,
            Command::Refresh {
                contest: None,
                force: false,
            }
        ));

        let cli = Cli::try_parse_from(["atc", "refresh", "-c", "abc466"])
            .expect("refresh with override should parse");
        assert!(matches!(
            cli.command,
            Command::Refresh {
                contest: Some(contest),
                force: false,
            } if contest == "abc466"
        ));

        let cli = Cli::try_parse_from(["atc", "refresh", "-c", "abc350", "-f"])
            .expect("forced refresh should parse");
        assert!(matches!(
            cli.command,
            Command::Refresh {
                contest: Some(contest),
                force: true,
            } if contest == "abc350"
        ));

        let cli = Cli::try_parse_from(["atc", "refresh", "-f"])
            .expect("forced refresh without an override should parse");
        assert!(matches!(
            cli.command,
            Command::Refresh {
                contest: None,
                force: true,
            }
        ));

        let cli = Cli::try_parse_from(["atc", "refresh", "--contest", "abc350", "--force"])
            .expect("long forced refresh options should parse");
        assert!(matches!(
            cli.command,
            Command::Refresh {
                contest: Some(contest),
                force: true,
            } if contest == "abc350"
        ));

        let mut command = Cli::command();
        let refresh = command
            .find_subcommand_mut("refresh")
            .expect("refresh subcommand should exist");
        let help = refresh.render_long_help().to_string();
        assert!(help.contains("-f, --force"));
        assert!(help.contains("Rebuild the current directory without trusting workspace metadata"));
    }

    #[test]
    fn parses_test_problem_and_optional_language() {
        for problem in ["A", "a"] {
            let cli = Cli::try_parse_from(["atc-rs", "test", problem]).unwrap();
            assert!(matches!(
                cli.command,
                Command::Test {
                    problem: parsed,
                    contest: None,
                    language: None,
                    debug: false,
                } if parsed == problem
            ));
        }

        let cli = Cli::try_parse_from(["atc-rs", "test", "A", "-l", "python"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Test {
                problem,
                contest: None,
                language: Some(Language::Python),
                debug: false,
            } if problem == "A"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "test", "A", "--debug"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Test {
                problem,
                contest: None,
                language: None,
                debug: true,
            } if problem == "A"
        ));
    }

    #[test]
    fn test_language_rejects_unsupported_alias() {
        assert!(Cli::try_parse_from(["atc-rs", "test", "A", "-l", "py"]).is_err());
    }

    #[test]
    fn parses_create_name_and_optional_language() {
        let cli = Cli::try_parse_from(["atc-rs", "create", "A"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Create {
                name,
                language: None,
            } if name == "A"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "create", "main", "-l", "python"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Create {
                name,
                language: Some(Language::Python),
            } if name == "main"
        ));

        assert!(Cli::try_parse_from(["atc-rs", "create", "A", "-l", "py"]).is_err());
    }

    #[test]
    fn parses_watch_plain_option() {
        let cli = Cli::try_parse_from(["atc-rs", "watch"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Watch {
                plain: false,
                contest: None,
            }
        ));

        let cli = Cli::try_parse_from(["atc-rs", "watch", "--plain"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Watch {
                plain: true,
                contest: None,
            }
        ));

        let cli = Cli::try_parse_from(["atc-rs", "watch", "-c", "abc466"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Watch {
                plain: false,
                contest: Some(contest),
            } if contest == "abc466"
        ));
    }
    #[test]
    fn parses_test_contest() {
        let cli = Cli::try_parse_from(["atc-rs", "test", "A", "-c", "abc466"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Test {
                problem,
                contest: Some(contest),
                language: None,
                debug: false,
            } if problem == "A" && contest == "abc466"
        ));
    }
    #[test]
    fn parses_contest() {
        let cli = Cli::try_parse_from(["atc-rs", "contest", "abc466"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Contest { contest_id }
                if contest_id == "abc466"
        ));
    }
}
