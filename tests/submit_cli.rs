use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    _temp: tempfile::TempDir,
    contest: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let contest = temp.path().join("abc430");
        fs::create_dir_all(contest.join(".atc")).unwrap();
        fs::write(
            contest.join(".atc/contest.toml"),
            concat!(
                "version = 1\n",
                "contest_id = \"abc430\"\n",
                "[[problems]]\n",
                "index = \"A\"\n",
                "title = \"A\"\n",
                "task_id = \"abc430_a\"\n",
                "url = \"https://atcoder.jp/contests/abc430/tasks/abc430_a\"\n",
                "sample_count = 0\n",
            ),
        )
        .unwrap();

        let config = temp.path().join("config");
        fs::create_dir_all(config.join("atc")).unwrap();
        fs::write(
            config.join("atc/config.toml"),
            "defaults.language = \"python\"\n",
        )
        .unwrap();

        Self {
            _temp: temp,
            contest,
            config,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        // Every case in this file fails during parsing or local validation. The invalid proxy is
        // an additional guard that makes an accidental network path fail without reaching AtCoder.
        Command::new(env!("CARGO_BIN_EXE_atc"))
            .args(args)
            .current_dir(&self.contest)
            .env("APPDATA", &self.config)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("NO_COLOR", "1")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .output()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn ambiguous_sources_exit_nonzero_and_ignore_the_python_config_default() {
    let fixture = Fixture::new();
    fs::write(fixture.contest.join("A.cpp"), "cpp\n").unwrap();
    fs::write(fixture.contest.join("A.py"), "python\n").unwrap();

    let output = fixture.run(&["submit", "A"]);

    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(stderr.contains("Both C++ and Python sources exist for problem A"));
    assert!(stderr.contains("-l cpp"));
    assert!(stderr.contains("-l python"));
    assert!(!stderr.contains("HTTP request failed"));
}

#[test]
fn unknown_problem_exits_nonzero_before_any_network_path() {
    let fixture = Fixture::new();
    fs::write(fixture.contest.join("A.cpp"), "cpp\n").unwrap();

    let output = fixture.run(&["submit", "Z"]);

    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(stderr.contains("problem not found in this contest: Z"));
    assert!(!stderr.contains("HTTP request failed"));
}

#[test]
fn explicit_missing_language_exits_nonzero_without_fallback() {
    let fixture = Fixture::new();
    fs::write(fixture.contest.join("A.py"), "python\n").unwrap();

    let output = fixture.run(&["submit", "A", "-l", "cpp"]);

    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(stderr.contains("C++ source file not found for problem A"));
    assert!(!stderr.contains("HTTP request failed"));
}

#[test]
fn missing_problem_and_numeric_language_are_rejected_by_clap() {
    let fixture = Fixture::new();

    for args in [&["submit"][..], &["submit", "A", "-l", "6017"][..]] {
        let output = fixture.run(args);
        assert!(!output.status.success(), "args: {args:?}");
        assert!(stderr(&output).contains("error:"), "args: {args:?}");
    }
}
