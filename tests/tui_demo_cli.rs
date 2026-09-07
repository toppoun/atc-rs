use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atc"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

#[test]
fn tui_demo_with_extra_arguments_returns_to_normal_clap_parsing() {
    for args in [
        &["--tui-demo", "--unexpected"][..],
        &["--tui-demo", "--help"][..],
        &["--tui-demo", "foo"][..],
    ] {
        let output = run(args);
        assert!(!output.status.success(), "args={args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("error:"), "args={args:?}\n{stderr}");
        assert!(stderr.contains("--tui-demo"), "args={args:?}\n{stderr}");
        assert!(stderr.contains("Usage:"), "args={args:?}\n{stderr}");
    }
}
