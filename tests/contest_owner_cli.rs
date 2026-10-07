use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn contest_repair_refuses_wrong_owner_even_when_url_is_repairable() {
    for with_tests in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir_all(destination.join(".atc")).unwrap();
        let metadata_path = destination.join(".atc/contest.toml");
        let original = concat!(
            "version = 1\ncontest_id = \"abc467\"\n",
            "[[problems]]\nindex = \"A\"\ntitle = \"A\"\n",
            "task_id = \"abc467_a\"\n",
            "url = \"https://atcoder.jp/contests/abc467/tasks/abc467_b\"\n",
            "sample_count = 0\n",
        );
        fs::write(&metadata_path, original).unwrap();
        if with_tests {
            fs::create_dir(destination.join("tests")).unwrap();
            fs::write(destination.join("tests/user-note.txt"), "keep me\n").unwrap();
        }

        let mut child = Command::new(env!("CARGO_BIN_EXE_atc"))
            .args(["contest", "abc466"])
            .current_dir(temp.path())
            .env(
                "ATC_FIXTURE_DIR",
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures"),
            )
            .env("APPDATA", temp.path().join("config"))
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"y\n").unwrap();
        let started = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(8) {
                child.kill().unwrap();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();

        assert!(!output.status.success());
        assert!(
            fs::read(&metadata_path).unwrap() == original.as_bytes(),
            "metadata bytes changed"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("contest ID mismatch"));
        if with_tests {
            assert_eq!(
                fs::read_to_string(destination.join("tests/user-note.txt")).unwrap(),
                "keep me\n"
            );
        } else {
            assert!(!destination.join("tests").exists());
        }
    }
}
