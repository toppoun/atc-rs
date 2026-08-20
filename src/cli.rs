use crate::language::Language;
use std::num::NonZeroU64;
use clap::{Command as ClapCommand, CommandFactory, FromArgMatches, Parser, Subcommand};

const LOGO: &str = r#"

 █████╗ ████████╗ ██████╗
██╔══██╗╚══██╔══╝██╔════╝
███████║   ██║   ██║     
██╔══██║   ██║   ██║     
██║  ██║   ██║   ╚██████╗
╚═╝  ╚═╝   ╚═╝    ╚═════╝"#;

const GRAY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

/// Fast AtCoder workflow from your terminal.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn parse() -> Self {
        let mut command = <Self as CommandFactory>::command();

        command = command.override_help(render_help());

        let mut matches = command.get_matches();

        Self::from_arg_matches_mut(&mut matches).unwrap_or_else(|err| err.exit())
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(flatten)]
    Contest(ContestCommand),

    #[command(flatten)]
    RunTest(RunTestCommand),

    #[command(flatten)]
    Files(FileCommand),

    #[command(flatten)]
    Account(AccountCommand),
}

// ============================================================
// Contest
// ============================================================

#[derive(Subcommand, Debug)]
pub enum ContestCommand {
    /// Create a contest workspace
    New {
        contest: String,

        #[arg(short = 'l', long)]
        language: Option<Language>,
    },

    /// Open or create a contest
    #[command(alias = "c")]
    Contest { contest_id: String },

    /// Refresh contest metadata and samples
    Refresh {
        #[arg(short, long)]
        contest: Option<String>,

        /// Rebuild the current directory without trusting workspace metadata
        #[arg(short, long)]
        force: bool,
    },
}

// ============================================================
// Run & Test
// ============================================================

#[derive(Subcommand, Debug)]
pub enum RunTestCommand {
    /// Run sample tests
    Test {
        problem: String,

        #[arg(short = 'c', long)]
        contest: Option<String>,

        #[arg(short = 'l', long = "language")]
        language: Option<Language>,

        #[arg(short, long)]
        debug: bool,
    },

    /// Watch sources and run tests
    Watch {
        #[arg(short = 'c', long)]
        contest: Option<String>,

        #[arg(long)]
        plain: bool,
    },

    /// Find counterexamples with stress testing
    Stress {
        problem: String,

        #[arg(short = 'c', long)]
        contest: Option<String>,

        #[arg(short = 'l', long = "language")]
        language: Option<Language>,

        #[arg(short, long)]
        debug: bool,

        #[arg(long, conflicts_with = "forever")]
        count: Option<NonZeroU64>,

        #[arg(long, conflicts_with = "count")]
        forever: bool,

        #[arg(long)]
        seed: Option<u64>,
    },
}

// ============================================================
// Files
// ============================================================

#[derive(Subcommand, Debug)]
pub enum FileCommand {
    /// Create a source file
    Create {
        name: String,

        #[arg(short = 'l', long)]
        language: Option<Language>,
    },
}

// ============================================================
// Account
// ============================================================

#[derive(Subcommand, Debug)]
pub enum AccountCommand {
    /// Check AtCoder authentication
    Login,
}

fn render_category<T: Subcommand>(title: &str) -> String {
    let command = T::augment_subcommands(ClapCommand::new("category"));

    let mut out = String::new();
    out.push_str(title);
    out.push('\n');

    for subcommand in command.get_subcommands() {
        let name = subcommand.get_name();
        let about = subcommand
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default();

        out.push_str(&format!("  {name:<10}{about}\n"));
    }

    out
}

fn render_help() -> String {
    format!(
        "{LOGO}
{GRAY}Fast AtCoder workflow from your terminal.{RESET}

Usage:
  atc <command> [options]

{}

{}

{}

{}

Options
  -h, --help       Show help
  -V, --version    Show version
",
        render_category::<ContestCommand>("Contest").trim_end(),
        render_category::<RunTestCommand>("Run & Test").trim_end(),
        render_category::<FileCommand>("Files").trim_end(),
        render_category::<AccountCommand>("Account").trim_end(),
    )
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
            Command::Contest(ContestCommand::New {
                contest,
                language: None,
            }) if contest == "abc466"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "new", "abc466", "-l", "python"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Contest(ContestCommand::New {
                contest,
                language: Some(Language::Python),
            }) if contest == "abc466"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "new", "abc466", "--language", "cpp"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Contest(ContestCommand::New {
                contest,
                language: Some(Language::Cpp),
            }) if contest == "abc466"
        ));
    }

    #[test]
    fn parses_refresh_with_and_without_contest_override() {
        let cli =
            Cli::try_parse_from(["atc", "refresh"]).expect("refresh without override should parse");

        assert!(matches!(
            cli.command,
            Command::Contest(ContestCommand::Refresh {
                contest: None,
                force: false,
            })
        ));

        let cli = Cli::try_parse_from(["atc", "refresh", "-c", "abc466"])
            .expect("refresh with override should parse");

        assert!(matches!(
            cli.command,
            Command::Contest(ContestCommand::Refresh {
                contest: Some(contest),
                force: false,
            }) if contest == "abc466"
        ));

        let cli = Cli::try_parse_from(["atc", "refresh", "-c", "abc350", "-f"])
            .expect("forced refresh should parse");

        assert!(matches!(
            cli.command,
            Command::Contest(ContestCommand::Refresh {
                contest: Some(contest),
                force: true,
            }) if contest == "abc350"
        ));

        let cli = Cli::try_parse_from(["atc", "refresh", "-f"])
            .expect("forced refresh without an override should parse");

        assert!(matches!(
            cli.command,
            Command::Contest(ContestCommand::Refresh {
                contest: None,
                force: true,
            })
        ));

        let cli = Cli::try_parse_from(["atc", "refresh", "--contest", "abc350", "--force"])
            .expect("long forced refresh options should parse");

        assert!(matches!(
            cli.command,
            Command::Contest(ContestCommand::Refresh {
                contest: Some(contest),
                force: true,
            }) if contest == "abc350"
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
                Command::RunTest(RunTestCommand::Test {
                    problem: parsed,
                    contest: None,
                    language: None,
                    debug: false,
                }) if parsed == problem
            ));
        }

        let cli = Cli::try_parse_from(["atc-rs", "test", "A", "-l", "python"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Test {
                problem,
                contest: None,
                language: Some(Language::Python),
                debug: false,
            }) if problem == "A"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "test", "A", "--debug"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Test {
                problem,
                contest: None,
                language: None,
                debug: true,
            }) if problem == "A"
        ));
    }

    #[test]
    fn test_language_rejects_unsupported_alias() {
        assert!(Cli::try_parse_from(["atc-rs", "test", "A", "-l", "py"]).is_err());
    }

    #[test]
    fn parses_stress_options() {
        let cli = Cli::try_parse_from(["atc-rs", "stress", "A"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Stress {
                problem,
                contest: None,
                language: None,
                debug: false,
                count: None,
                forever: false,
                seed: None,
            }) if problem == "A"
        ));

        let cli = Cli::try_parse_from([
            "atc-rs",
            "stress",
            "B",
            "-c",
            "abc466",
            "-l",
            "cpp",
            "--debug",
            "--count",
            "1000",
            "--seed",
            "42",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Stress {
                problem,
                contest: Some(contest),
                language: Some(Language::Cpp),
                debug: true,
                count: Some(count),
                forever: false,
                seed: Some(42),
            }) if problem == "B" && contest == "abc466" && count.get() == 1000
        ));
    }

    #[test]
    fn stress_count_and_forever_are_exclusive_and_count_must_be_nonzero() {
        assert!(
            Cli::try_parse_from(["atc-rs", "stress", "A", "--count", "10", "--forever"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["atc-rs", "stress", "A", "--count", "0"]).is_err());

        let cli = Cli::try_parse_from(["atc-rs", "stress", "A", "--forever"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Stress {
                forever: true,
                count: None,
                ..
            })
        ));
    }

    #[test]
    fn parses_create_name_and_optional_language() {
        let cli = Cli::try_parse_from(["atc-rs", "create", "A"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Files(FileCommand::Create {
                name,
                language: None,
            }) if name == "A"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "create", "main", "-l", "python"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Files(FileCommand::Create {
                name,
                language: Some(Language::Python),
            }) if name == "main"
        ));

        assert!(Cli::try_parse_from(["atc-rs", "create", "A", "-l", "py"]).is_err());
    }

    #[test]
    fn parses_watch_plain_option() {
        let cli = Cli::try_parse_from(["atc-rs", "watch"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Watch {
                plain: false,
                contest: None,
            })
        ));

        let cli = Cli::try_parse_from(["atc-rs", "watch", "--plain"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Watch {
                plain: true,
                contest: None,
            })
        ));

        let cli = Cli::try_parse_from(["atc-rs", "watch", "-c", "abc466"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Watch {
                plain: false,
                contest: Some(contest),
            }) if contest == "abc466"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "watch", "--plain", "-c", "abc466"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Watch {
                plain: true,
                contest: Some(contest),
            }) if contest == "abc466"
        ));
    }

    #[test]
    fn parses_test_contest() {
        let cli = Cli::try_parse_from(["atc-rs", "test", "A", "-c", "abc466"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::RunTest(RunTestCommand::Test {
                problem,
                contest: Some(contest),
                language: None,
                debug: false,
            }) if problem == "A" && contest == "abc466"
        ));
    }

    #[test]
    fn parses_contest() {
        let cli = Cli::try_parse_from(["atc-rs", "contest", "abc466"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Contest(ContestCommand::Contest { contest_id })
                if contest_id == "abc466"
        ));

        let cli = Cli::try_parse_from(["atc-rs", "c", "abc471"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Contest(ContestCommand::Contest { contest_id })
                if contest_id == "abc471"
        ));
    }

    #[test]
    fn parses_login_without_arguments() {
        let cli = Cli::try_parse_from(["atc-rs", "login"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Account(AccountCommand::Login)
        ));

        assert!(Cli::try_parse_from(["atc-rs", "login", "extra"]).is_err());
    }
}
