#![cfg(windows)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn toml_string(value: &str) -> String {
    format!("{value:?}")
}

fn run_doctor(cwd: &Path, config_base: &Path, config: &str) -> Output {
    let config_file = config_base.join("atc").join("config.toml");
    fs::create_dir_all(config_file.parent().unwrap()).unwrap();
    fs::write(config_file, config).unwrap();

    Command::new(env!("CARGO_BIN_EXE_atc"))
        .arg("doctor")
        .current_dir(cwd)
        .env("APPDATA", config_base)
        .env("XDG_CONFIG_HOME", config_base)
        .env_remove("NO_COLOR")
        .output()
        .unwrap()
}

fn runner_config(default_language: &str) -> String {
    let available = toml_string(env!("CARGO_BIN_EXE_atc"));
    let missing = toml_string("Z:\\atc-doctor-test\\definitely-missing.exe");
    format!(
        "defaults.language = {default_language:?}\n\
         runner.cpp_compiler = {missing}\n\
         runner.python = {available}\n"
    )
}

#[test]
fn warn_only_doctor_exits_zero_and_redirected_output_has_no_ansi() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    fs::create_dir(&cwd).unwrap();

    let output = run_doctor(&cwd, &temp.path().join("config"), &runner_config("python"));
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("[WARN] C++"));
    assert!(stdout.contains("[OK] Python"));
    assert!(stdout.contains("Result: OK ("));
    assert!(!stdout.contains('\x1b'));
    assert!(output.stderr.is_empty());
}

#[test]
fn error_doctor_exits_one_without_a_generic_command_error() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    fs::create_dir(&cwd).unwrap();

    let output = run_doctor(&cwd, &temp.path().join("config"), &runner_config("cpp"));
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout.contains("[ERROR] C++"));
    assert!(stdout.contains("Result: FAILED ("));
    assert!(output.stderr.is_empty());
}

#[test]
fn invalid_config_skips_runner_and_template_process_paths_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    fs::create_dir(&cwd).unwrap();

    let output = run_doctor(&cwd, &temp.path().join("config"), "[runner\n");
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout.contains("Config\n  [ERROR]"));
    assert!(stdout.contains("Runners\n  [SKIP]"));
    assert!(stdout.contains("Templates\n  [SKIP]"));
    assert!(stdout.contains("Result: FAILED"));
    assert!(output.stderr.is_empty());
}

#[test]
fn executable_supplied_controls_are_neutralized_in_redirected_output() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    fs::create_dir(&cwd).unwrap();

    let runner = temp.path().join("ansi-version.cmd");
    fs::write(
        &runner,
        b"@echo off\r\necho \x1b[31mrunner-version\x1b[0m\x07\r\n",
    )
    .unwrap();
    let runner = toml_string(runner.to_str().unwrap());
    let config = format!(
        "defaults.language = \"cpp\"\n\
         runner.cpp_compiler = {runner}\n\
         runner.python = {runner}\n"
    );

    let output = run_doctor(&cwd, &temp.path().join("config"), &config);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success(), "stdout: {stdout}");
    assert!(stdout.contains("runner-version"));
    assert!(!stdout.contains('\x1b'));
    assert!(!stdout.contains('\x07'));
    assert!(output.stderr.is_empty());
}
