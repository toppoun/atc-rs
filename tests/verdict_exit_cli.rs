use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn executable(names: &[&str]) -> String {
    names
        .iter()
        .find(|name| {
            Command::new(name)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("CLI integration tests require Python and a C++ compiler")
        .to_string()
}

fn quoted(text: &str) -> String {
    toml::Value::String(text.into()).to_string()
}

struct Fixture {
    _temp: tempfile::TempDir,
    contest: PathBuf,
    config: PathBuf,
    python: String,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let contest = temp.path().join("abc123");
        fs::create_dir_all(contest.join(".atc")).unwrap();
        fs::create_dir_all(contest.join("tests/A")).unwrap();
        fs::write(
            contest.join(".atc/contest.toml"),
            concat!(
                "version = 1\ncontest_id = \"abc123\"\n",
                "[[problems]]\nindex = \"A\"\ntitle = \"A\"\ntask_id = \"abc123_a\"\n",
                "url = \"https://atcoder.jp/contests/abc123/tasks/abc123_a\"\nsample_count = 1\n",
            ),
        )
        .unwrap();
        fs::write(contest.join("tests/A/sample-1.in"), "42\n").unwrap();
        fs::write(contest.join("tests/A/sample-1.out"), "42\n").unwrap();
        fs::write(contest.join("A.py"), source).unwrap();
        let config = temp.path().join("config");
        fs::create_dir_all(config.join("atc")).unwrap();
        let fixture = Self {
            _temp: temp,
            contest,
            config,
            python: executable(&["python3", "python"]),
        };
        fixture.configure("");
        fixture
    }

    fn configure(&self, extra: &str) {
        fs::write(
            self.config.join("atc/config.toml"),
            format!("runner.python = {}\n{extra}", quoted(&self.python)),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_atc"))
            .args(args)
            .current_dir(&self.contest)
            .env("APPDATA", &self.config)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn test(&self) -> Output {
        self.run(&["test", "A", "--language", "python"])
    }

    fn stress(&self) -> Output {
        fs::write(self.contest.join("A_gen.py"), "print(42)\n").unwrap();
        fs::write(self.contest.join("A_brute.py"), "print(42)\n").unwrap();
        self.run(&[
            "stress",
            "A",
            "--language",
            "python",
            "--count",
            "2",
            "--seed",
            "1",
        ])
    }
}

fn assert_verdict(output: Output, success: bool, expected: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.success(),
        success,
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(expected),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Operational errors are printed by main, while verdicts already have human-readable
    // reporting. Do not add a second generic error for a failed verdict.
    assert!(
        !stderr.lines().any(|line| line.starts_with("error: ")),
        "{stderr}"
    );
}

#[test]
fn all_ac_exits_zero() {
    assert_verdict(
        Fixture::new("print(input())\n").test(),
        true,
        "Result: 1/1 AC",
    );
}

#[test]
fn wrong_answer_exits_nonzero() {
    assert_verdict(Fixture::new("print('wrong')\n").test(), false, "WA");
}

#[test]
fn runtime_error_exits_nonzero() {
    assert_verdict(
        Fixture::new("raise RuntimeError('expected test failure')\n").test(),
        false,
        "RE",
    );
}

#[test]
fn runtime_timeout_exits_nonzero() {
    let fixture = Fixture::new("import time; time.sleep(5)\n");
    fixture.configure("runner.timeout_seconds = 0.5\n");
    assert_verdict(fixture.test(), false, "TLE");
}

#[test]
fn compile_error_exits_nonzero() {
    let fixture = Fixture::new("unused");
    let compiler = executable(&["g++", "clang++"]);
    fixture.configure(&format!("runner.cpp_compiler = {}\n", quoted(&compiler)));
    fs::write(fixture.contest.join("A.cpp"), "not valid C++\n").unwrap();
    assert_verdict(
        fixture.run(&["test", "A", "--language", "cpp"]),
        false,
        "Compile Error",
    );
}

#[test]
fn compile_timeout_exits_nonzero() {
    let fixture = Fixture::new("unused");
    // An unresponsive compiler stand-in, through the real compile command and timeout path.
    fixture.configure(&format!(
        "runner.cpp_compiler = {}\nrunner.cpp_flags = [\"-c\", \"import time; time.sleep(5)\"]\nrunner.compile_timeout_seconds = 0.5\n",
        quoted(&fixture.python),
    ));
    fs::write(fixture.contest.join("A.cpp"), "unused").unwrap();
    assert_verdict(
        fixture.run(&["test", "A", "--language", "cpp"]),
        false,
        "Compile Timed Out",
    );
}

#[test]
fn stress_counterexample_exits_nonzero_after_reporting_and_saving() {
    let fixture = Fixture::new("print('wrong')\n");
    assert_verdict(fixture.stress(), false, "WA at case 1");
    assert!(fixture.contest.join(".atc/stress/A/failed.in").is_file());
}

#[test]
fn successful_finite_stress_exits_zero() {
    assert_verdict(
        Fixture::new("print(input())\n").stress(),
        true,
        "2 cases passed",
    );
}

#[test]
fn a_later_ac_does_not_erase_an_earlier_failed_verdict() {
    let fixture = Fixture::new("print(input())\n");
    fs::write(fixture.contest.join("tests/A/sample-1.out"), "wrong\n").unwrap();
    fs::write(fixture.contest.join("tests/A/sample-2.in"), "42\n").unwrap();
    fs::write(fixture.contest.join("tests/A/sample-2.out"), "42\n").unwrap();
    let metadata_path = fixture.contest.join(".atc/contest.toml");
    let metadata = fs::read_to_string(&metadata_path).unwrap();
    fs::write(
        metadata_path,
        metadata.replace("sample_count = 1", "sample_count = 2"),
    )
    .unwrap();
    assert_verdict(fixture.test(), false, "Result: 1/2 AC");
}

#[test]
fn operational_failure_keeps_the_existing_single_error_report() {
    let fixture = Fixture::new("print(input())\n");
    fs::remove_file(fixture.contest.join("A.py")).unwrap();
    let output = fixture.test();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("source file not found"), "{stderr}");
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("error: "))
            .count(),
        1
    );
}
