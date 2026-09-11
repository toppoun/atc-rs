use std::process::Command;

const HELP_GOLDEN: &str = concat!(
    "\n",
    " █████╗ ████████╗ ██████╗\n",
    "██╔══██╗╚══██╔══╝██╔════╝\n",
    "███████║   ██║   ██║\n",
    "██╔══██║   ██║   ██║\n",
    "██║  ██║   ██║   ╚██████╗\n",
    "╚═╝  ╚═╝   ╚═╝    ╚═════╝\n",
    "Fast AtCoder workflow from your terminal.\n",
    "\n",
    "Usage:\n",
    "  atc [options] [command]\n",
    "\n",
    "Workspace\n",
    "  init      Initialize an atc workspace\n",
    "\n",
    "Configuration\n",
    "  config    Manage global configuration\n",
    "\n",
    "Contest\n",
    "  new       Create a contest workspace\n",
    "  contest   Open or create a contest\n",
    "  refresh   Refresh contest metadata and samples\n",
    "\n",
    "Run & Test\n",
    "  test      Run samples and the saved stress regression\n",
    "  watch     Watch sources and run tests\n",
    "  stress    Find counterexamples with stress testing\n",
    "  submit    Submit a solution to AtCoder\n",
    "\n",
    "Files\n",
    "  create    Create a source file\n",
    "  template  Manage source templates\n",
    "\n",
    "Account\n",
    "  login     Check AtCoder authentication\n",
    "\n",
    "Diagnostics\n",
    "  doctor    Diagnose the local atc environment\n",
    "\n",
    "Help\n",
    "  help      Print this message or the help of the given subcommand(s)\n",
    "\n",
    "Options\n",
    "  -h, --help       Show help\n",
    "  -V, --version    Show version\n",
);

#[test]
fn top_level_help_matches_the_independent_golden_through_the_real_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_atc"))
        .arg("--help")
        .env("NO_COLOR", "1")
        .output()
        .expect("atc --help should run");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stderr, b"");
    assert_eq!(output.stdout, HELP_GOLDEN.as_bytes());
}
