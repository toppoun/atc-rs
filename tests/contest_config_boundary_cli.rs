use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, config_base: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atc"))
        .args(args)
        .current_dir(root)
        .env("APPDATA", config_base)
        .env("XDG_CONFIG_HOME", config_base)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn assert_invalid_config_failure(output: Output) {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("config") && (stderr.contains("parse") || stderr.contains("TOML")),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn direct_contest_aliases_and_tui_watch_reject_invalid_config_before_creation() {
    for args in [
        &["c", "abc599"][..],
        &["contest", "abc599"][..],
        &["watch", "-c", "abc599"][..],
    ] {
        let temp = tempfile::tempdir().unwrap();
        let config_base = temp.path().join("config-base");
        fs::create_dir_all(config_base.join("atc")).unwrap();
        fs::write(config_base.join("atc/config.toml"), "invalid = [\n").unwrap();

        assert_invalid_config_failure(run(temp.path(), &config_base, args));
        assert!(
            !temp.path().join("abc599").exists(),
            "invalid Config must fail before contest creation for {args:?}"
        );
    }
}
