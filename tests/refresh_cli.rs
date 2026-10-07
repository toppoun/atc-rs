use std::fs;
use std::process::Command;

#[test]
fn force_refresh_refuses_a_different_contests_metadata_without_tests() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    fs::create_dir_all(destination.join(".atc")).unwrap();
    let metadata_path = destination.join(".atc/contest.toml");
    let original = concat!(
        "version = 1\ncontest_id = \"abc467\"\n",
        "[[problems]]\nindex = \"A\"\ntitle = \"A\"\n",
        "task_id = \"abc467_a\"\n",
        "url = \"https://atcoder.jp/contests/abc467/tasks/abc467_a\"\n",
        "sample_count = 0\n",
    );
    fs::write(&metadata_path, original).unwrap();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");

    let output = Command::new(env!("CARGO_BIN_EXE_atc"))
        .args(["refresh", "--force"])
        .current_dir(&destination)
        .env("ATC_FIXTURE_DIR", fixtures)
        .env("APPDATA", temp.path().join("config"))
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "force refresh replaced a different contest: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("contest ID mismatch"));
    assert_eq!(fs::read(&metadata_path).unwrap(), original.as_bytes());
    assert!(!destination.join("tests").exists());
}
