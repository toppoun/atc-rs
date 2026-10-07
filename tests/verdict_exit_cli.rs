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
fn missing_all_official_samples_fails_before_running_solution() {
    let fixture = Fixture::new("open('ran.txt', 'w').write('ran')\nprint(input())\n");
    let metadata_path = fixture.contest.join(".atc/contest.toml");
    let metadata = fs::read_to_string(&metadata_path)
        .unwrap()
        .replace("sample_count = 1", "sample_count = 3");
    fs::write(&metadata_path, &metadata).unwrap();
    fs::remove_file(fixture.contest.join("tests/A/sample-1.in")).unwrap();
    fs::remove_file(fixture.contest.join("tests/A/sample-1.out")).unwrap();

    let output = fixture.test();
    assert!(
        !output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!fixture.contest.join("ran.txt").exists());
    assert_eq!(fs::read_to_string(metadata_path).unwrap(), metadata);
}

#[test]
fn missing_one_official_sample_fails_before_running_solution() {
    let fixture = Fixture::new("open('ran.txt', 'w').write('ran')\nprint(input())\n");
    let metadata_path = fixture.contest.join(".atc/contest.toml");
    let metadata = fs::read_to_string(&metadata_path)
        .unwrap()
        .replace("sample_count = 1", "sample_count = 3");
    fs::write(&metadata_path, &metadata).unwrap();
    fs::write(fixture.contest.join("tests/A/sample-2.in"), "43\n").unwrap();
    fs::write(fixture.contest.join("tests/A/sample-2.out"), "43\n").unwrap();

    let output = fixture.test();
    assert!(
        !output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!fixture.contest.join("ran.txt").exists());
    assert_eq!(fs::read_to_string(metadata_path).unwrap(), metadata);
}

#[test]
fn unusable_saved_stress_case_fails_cli_test_before_running_solution() {
    let fixture = Fixture::new("open('ran.txt', 'w').write('ran')\nprint(input())\n");
    let target = fixture.contest.join(".atc/stress/A");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("failed.in"), "42\n").unwrap();
    fs::write(target.join("actual.out"), "wrong\n").unwrap();
    fs::write(target.join("expected.out"), "42\n").unwrap();
    fs::write(target.join("meta.toml"), "broken = [").unwrap();

    let output = fixture.test();
    assert!(
        !output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!fixture.contest.join("ran.txt").exists());
    assert_eq!(
        fs::read_to_string(target.join("meta.toml")).unwrap(),
        "broken = ["
    );
}

#[test]
fn valid_saved_stress_case_runs_after_official_sample() {
    let fixture = Fixture::new("print(input())\n");
    let target = fixture.contest.join(".atc/stress/A");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("failed.in"), "43\n").unwrap();
    fs::write(target.join("actual.out"), "wrong\n").unwrap();
    fs::write(target.join("expected.out"), "43\n").unwrap();
    fs::write(target.join("meta.toml"),
        "version = 1\ncontest = \"abc123\"\nproblem = \"A\"\nkind = \"wrong-answer\"\ncase = 1\nbase_seed = 10\nseed = 10\n").unwrap();

    let output = fixture.test();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Result: 2/2 AC"));
    assert_eq!(
        fs::read_to_string(target.join("failed.in")).unwrap(),
        "43\n"
    );
}

#[test]
fn legacy_v1_re_without_expected_is_reported_and_not_counted_as_ac() {
    let fixture = Fixture::new("print(input())\n");
    let target = fixture.contest.join(".atc/stress/A");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("failed.in"), "43\n").unwrap();
    fs::write(target.join("actual.out"), "runtime failure\n").unwrap();
    fs::write(target.join("meta.toml"),
        "version = 1\ncontest = \"abc123\"\nproblem = \"A\"\nkind = \"runtime-error\"\ncase = 1\nbase_seed = 10\nseed = 10\n").unwrap();
    let old_metadata = fs::read(target.join("meta.toml")).unwrap();

    let output = fixture.test();
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Result: 1/1 AC (saved Stress case unverified)")
    );
    let notice = String::from_utf8_lossy(&output.stderr);
    assert!(notice.contains("Saved Stress case skipped"), "{notice}");
    assert!(notice.contains("expected.out"), "{notice}");
    assert!(notice.contains("historical or damaged"), "{notice}");
    assert_eq!(fs::read(target.join("meta.toml")).unwrap(), old_metadata);
    assert!(!target.join("expected.out").exists());
}

#[test]
fn legacy_v1_tle_without_expected_is_reported_and_not_counted_as_ac() {
    let fixture = Fixture::new("print(input())\n");
    let target = fixture.contest.join(".atc/stress/A");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("failed.in"), "43\n").unwrap();
    fs::write(target.join("actual.out"), "").unwrap();
    fs::write(target.join("meta.toml"),
        "version = 1\ncontest = \"abc123\"\nproblem = \"A\"\nkind = \"timed-out\"\ncase = 1\nbase_seed = 10\nseed = 10\n").unwrap();

    let output = fixture.test();
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Result: 1/1 AC (saved Stress case unverified)")
    );
    let notice = String::from_utf8_lossy(&output.stderr);
    assert!(notice.contains("Saved Stress case skipped"), "{notice}");
    assert!(notice.contains("timed-out"), "{notice}");
    assert!(!target.join("expected.out").exists());
}

#[test]
fn historical_and_damaged_v1_runtime_failures_are_indistinguishable_on_disk() {
    let mut observed = Vec::new();
    for _origin in ["historical", "later expected.out loss"] {
        let fixture = Fixture::new("print(input())\n");
        let target = fixture.contest.join(".atc/stress/A");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("failed.in"), "43\n").unwrap();
        fs::write(target.join("actual.out"), "runtime failure\n").unwrap();
        fs::write(target.join("meta.toml"),
            "version = 1\ncontest = \"abc123\"\nproblem = \"A\"\nkind = \"runtime-error\"\ncase = 1\nbase_seed = 10\nseed = 10\n").unwrap();
        let before = fs::read(target.join("meta.toml")).unwrap();
        let output = fixture.test();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("saved Stress case unverified"));
        assert_eq!(fs::read(target.join("meta.toml")).unwrap(), before);
        observed.push((
            output.status.code(),
            fs::read(target.join("failed.in")).unwrap(),
            fs::read(target.join("actual.out")).unwrap(),
            before,
            output.stderr,
        ));
    }
    assert_eq!(observed[0], observed[1]);
}

#[test]
fn ambiguous_v1_stress_case_does_not_hide_an_official_sample_wa() {
    let fixture = Fixture::new("print('wrong')\n");
    let target = fixture.contest.join(".atc/stress/A");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("failed.in"), "43\n").unwrap();
    fs::write(target.join("actual.out"), "runtime failure\n").unwrap();
    fs::write(target.join("meta.toml"),
        "version = 1\ncontest = \"abc123\"\nproblem = \"A\"\nkind = \"runtime-error\"\ncase = 1\nbase_seed = 10\nseed = 10\n").unwrap();

    let output = fixture.test();
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("WA"), "{stdout}");
    assert!(
        stdout.contains("Result: 0/1 AC (saved Stress case unverified)"),
        "{stdout}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Saved Stress case skipped"));
}

#[test]
fn v2_runtime_failure_without_expected_is_an_error_before_running_solution() {
    let fixture = Fixture::new("open('ran.txt', 'w').write('ran')\nprint(input())\n");
    let target = fixture.contest.join(".atc/stress/A");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("failed.in"), "43\n").unwrap();
    fs::write(target.join("actual.out"), "runtime failure\n").unwrap();
    let metadata = "version = 2\ncontest = \"abc123\"\nproblem = \"A\"\nkind = \"runtime-error\"\ncase = 1\nbase_seed = 10\nseed = 10\n";
    fs::write(target.join("meta.toml"), metadata).unwrap();

    let output = fixture.test();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("expected.out"), "{error}");
    assert!(!error.contains("Saved Stress case skipped"), "{error}");
    assert!(!fixture.contest.join("ran.txt").exists());
    assert_eq!(
        fs::read_to_string(target.join("meta.toml")).unwrap(),
        metadata
    );
}

#[test]
fn interrupted_stress_replacement_fails_cli_test_and_keeps_previous_case() {
    let fixture = Fixture::new("print(input())\n");
    let stress_root = fixture.contest.join(".atc/stress");
    let target = stress_root.join("A");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("failed.in"), "43\n").unwrap();
    fs::write(target.join("actual.out"), "wrong\n").unwrap();
    fs::write(target.join("expected.out"), "43\n").unwrap();
    fs::write(target.join("meta.toml"),
        "version = 1\ncontest = \"abc123\"\nproblem = \"A\"\nkind = \"wrong-answer\"\ncase = 1\nbase_seed = 10\nseed = 10\n").unwrap();
    let staging = stress_root.join(".stress-staging-interrupted");
    fs::create_dir(&staging).unwrap();
    fs::rename(&target, staging.join("previous")).unwrap();

    let output = fixture.test();
    assert!(
        !output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("recovery"));
    assert_eq!(
        fs::read_to_string(staging.join("previous/failed.in")).unwrap(),
        "43\n"
    );
    assert!(!target.exists());
}

#[test]
fn incomplete_legacy_staging_without_metadata_fails_cli_test() {
    let fixture = Fixture::new("open('ran.txt', 'w').write('ran')\nprint(input())\n");
    let staging = fixture
        .contest
        .join(".atc/stress/.stress-staging-interrupted/new");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("failed.in"), "old input\n").unwrap();

    let output = fixture.test();
    assert!(
        !output.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("recovery"));
    assert!(!fixture.contest.join("ran.txt").exists());
    assert_eq!(
        fs::read_to_string(staging.join("failed.in")).unwrap(),
        "old input\n"
    );
}

#[test]
fn another_problems_unfinished_current_staging_does_not_fail_cli_test() {
    let fixture = Fixture::new("print(input())\n");
    let staging = fixture
        .contest
        .join(".atc/stress/.stress-staging-1-B-empty");
    fs::create_dir_all(&staging).unwrap();

    let output = fixture.test();
    assert_verdict(output, true, "Result: 1/1 AC");
    let metadata_path = fixture.contest.join(".atc/contest.toml");
    let mut metadata = fs::read_to_string(&metadata_path).unwrap();
    metadata.push_str(concat!(
        "[[problems]]\nindex = \"B\"\ntitle = \"B\"\ntask_id = \"abc123_b\"\n",
        "url = \"https://atcoder.jp/contests/abc123/tasks/abc123_b\"\nsample_count = 1\n"
    ));
    fs::write(&metadata_path, metadata).unwrap();
    fs::create_dir_all(fixture.contest.join("tests/B")).unwrap();
    fs::write(fixture.contest.join("tests/B/sample-1.in"), "42\n").unwrap();
    fs::write(fixture.contest.join("tests/B/sample-1.out"), "42\n").unwrap();
    fs::write(fixture.contest.join("B.py"), "print(input())\n").unwrap();
    let blocked = fixture.run(&["test", "B", "--language", "python"]);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("recovery"));
    assert!(staging.is_dir());

    fs::create_dir(staging.join("new")).unwrap();
    fs::write(staging.join("new/meta.toml"), "version =").unwrap();
    assert_verdict(fixture.test(), true, "Result: 1/1 AC");
    let blocked = fixture.run(&["test", "B", "--language", "python"]);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("recovery"));
    assert_eq!(
        fs::read_to_string(staging.join("new/meta.toml")).unwrap(),
        "version ="
    );
}

#[test]
fn missing_metadata_still_fails_before_running_solution() {
    let fixture = Fixture::new("open('ran.txt', 'w').write('ran')\nprint(input())\n");
    fs::remove_file(fixture.contest.join(".atc/contest.toml")).unwrap();
    let output = fixture.test();
    assert!(!output.status.success());
    assert!(!fixture.contest.join("ran.txt").exists());
}

#[test]
fn missing_official_samples_and_broken_stress_never_succeed() {
    let fixture = Fixture::new("open('ran.txt', 'w').write('ran')\nprint(input())\n");
    let metadata_path = fixture.contest.join(".atc/contest.toml");
    fs::write(
        &metadata_path,
        fs::read_to_string(&metadata_path)
            .unwrap()
            .replace("sample_count = 1", "sample_count = 3"),
    )
    .unwrap();
    let target = fixture.contest.join(".atc/stress/A");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("meta.toml"), "broken = [").unwrap();

    let output = fixture.test();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("local samples"));
    assert!(!fixture.contest.join("ran.txt").exists());
    assert_eq!(
        fs::read_to_string(target.join("meta.toml")).unwrap(),
        "broken = ["
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
