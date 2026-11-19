use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MANIFEST: &str = concat!(
    "version = 1\ncontest_id = \"abc484\"\n",
    "[[problems]]\nindex = \"A\"\ntitle = \"A\"\n",
    "task_id = \"archive_round_b\"\n",
    "url = \"https://atcoder.jp/contests/abc484/tasks/archive_round_b\"\nsample_count = 0\n",
    "[[problems]]\nindex = \"B\"\ntitle = \"B\"\n",
    "task_id = \"abc484_c\"\n",
    "url = \"https://atcoder.jp/contests/abc484/tasks/abc484_c\"\nsample_count = 0\n",
);
const MAPPING: &str = "version = 1\n[[paths]]\npattern = '^abc[0-9]+$'\npath = 'Contests/ABC'\n";
const CPP: &str = "#include <iostream>\r\nint main() { std::cout << \"あ\\n\"; }\r\n";
const PYTHON: &str = "print('こんにちは')\n";

fn make_contest(destination: PathBuf) -> PathBuf {
    fs::create_dir_all(destination.join(".atc")).unwrap();
    fs::write(destination.join(".atc/contest.toml"), MANIFEST).unwrap();
    destination
}

fn command(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atc"));
    command.arg("source").current_dir(cwd).env("NO_COLOR", "1");
    command
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    command(cwd).args(args).output().unwrap()
}

fn read_json(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(output.stdout.ends_with(b"\n"));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_error(output: Output, message: &str) {
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(message),
        "expected {message:?} in {stderr:?}"
    );
}

#[test]
fn index_and_exact_task_id_return_the_same_versioned_cpp_json() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace 日本語");
    let contest = make_contest(root.join("abc484"));
    let source = contest.join("A.cpp");
    fs::write(&source, CPP).unwrap();
    fs::write(contest.join("B.py"), PYTHON).unwrap();

    let lookup = |selector: &[&str]| {
        command(&contest)
            .args(["abc484", "--json"])
            .args(selector)
            .arg("--workspace")
            .arg(&root)
            .output()
            .unwrap()
    };
    let by_index = lookup(&["a"]);
    assert_eq!(
        read_json(&by_index),
        json!({
            "schema_version": 1,
            "contest_id": "abc484",
            "index": "A",
            "task_id": "archive_round_b",
            "url": "https://atcoder.jp/contests/abc484/tasks/archive_round_b",
            "language": "cpp",
            "source_path": source.to_str().unwrap(),
            "source": CPP,
        })
    );
    for selector in [&["a"][..], &["--task-id", "archive_round_b"]] {
        let output = lookup(selector);
        read_json(&output);
        assert_eq!(output.stdout, by_index.stdout);
    }

    assert_error(lookup(&["--task-id", "abc484_a"]), "not found in contest");
    assert_error(
        lookup(&["--task-id", "ARCHIVE_ROUND_B"]),
        "not found in contest",
    );
}

#[test]
fn selects_python_and_preserves_an_existing_empty_source() {
    let temp = tempfile::tempdir().unwrap();
    let contest = make_contest(temp.path().join("abc484"));
    let path = contest.join("B.py");
    fs::write(&path, PYTHON).unwrap();
    fs::write(contest.join("B.rs"), "unsupported language").unwrap();

    let output = run(temp.path(), &["abc484", "--task-id", "abc484_c", "--json"]);
    let response = read_json(&output);
    assert_eq!(response["index"], "B");
    assert_eq!(response["language"], "python");
    assert_eq!(response["source"], PYTHON);
    let returned_path = Path::new(response["source_path"].as_str().unwrap());
    assert!(returned_path.is_absolute());
    assert!(returned_path.ends_with(Path::new("abc484").join("B.py")));

    fs::write(&path, b"").unwrap();
    let output = run(temp.path(), &["abc484", "B", "--json"]);
    assert_eq!(read_json(&output)["source"], "");
    assert!(fs::read(&path).unwrap().is_empty());
}

#[test]
fn multiple_languages_require_a_choice_and_only_the_chosen_path_is_checked() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let contest = make_contest(root.join("abc484"));
    fs::write(contest.join("A.cpp"), CPP).unwrap();
    fs::write(contest.join("A.py"), PYTHON).unwrap();
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "select one with --language",
    );
    for (language, source) in [("cpp", CPP), ("python", PYTHON)] {
        let output = run(root, &["abc484", "A", "--json", "--language", language]);
        let response = read_json(&output);
        assert_eq!(response["language"], language);
        assert_eq!(response["source"], source);
    }

    fs::remove_file(contest.join("A.cpp")).unwrap();
    assert_error(
        run(root, &["abc484", "A", "--json", "--language", "cpp"]),
        "source file not found for A.cpp",
    );
    fs::create_dir(contest.join("A.cpp")).unwrap();
    let output = run(root, &["abc484", "A", "--json", "--language", "Python"]);
    assert_eq!(read_json(&output)["source"], PYTHON);
    assert_error(run(root, &["abc484", "A", "--json"]), "not a regular file");
}

#[test]
fn distinguishes_missing_contest_metadata_problem_and_source() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "contest directory not found",
    );
    let contest = make_contest(root.join("abc484"));
    assert_error(run(root, &["abc484", "Z", "--json"]), "problem not found");
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "source file not found",
    );
    assert_eq!(fs::read_dir(&contest).unwrap().count(), 1);

    fs::remove_file(contest.join(".atc/contest.toml")).unwrap();
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "contest metadata not found",
    );
    assert_eq!(fs::read_dir(contest.join(".atc")).unwrap().count(), 0);
}

#[test]
fn follows_workspace_mapping_and_does_not_search_parent_directories() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join(".atc-workspace.toml"), MAPPING).unwrap();
    let contest = make_contest(root.join("Contests/ABC/abc484"));
    fs::write(contest.join("A.cpp"), CPP).unwrap();
    let unmapped = make_contest(root.join("abc484"));
    fs::write(unmapped.join("A.cpp"), "wrong location").unwrap();

    let output = run(root, &["abc484", "A", "--json"]);
    assert_eq!(read_json(&output)["source"], CPP);
    let subdir = root.join("Contests");
    assert_error(
        run(&subdir, &["abc484", "A", "--json"]),
        "contest directory not found",
    );

    let output = run(&subdir, &["abc484", "A", "--json", "--workspace", ".."]);
    let response = read_json(&output);
    let path = Path::new(response["source_path"].as_str().unwrap());
    assert!(path.is_absolute());
    assert!(path.ends_with(Path::new("Contests/ABC/abc484/A.cpp")));
    assert_eq!(response["source"], CPP);

    fs::write(
        root.join(".atc-workspace.toml"),
        MAPPING.replace("Contests/ABC", "Missing"),
    )
    .unwrap();
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "contest directory not found",
    );
    assert!(!root.join("Missing").exists());
}

#[test]
fn validates_selectors_and_requires_json() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let help = run(root, &["--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for usage in [
        "atc source <CONTEST_ID> <INDEX> --json",
        "atc source <CONTEST_ID> --task-id <TASK_ID> --json",
    ] {
        assert!(help.contains(usage), "{help}");
    }
    assert_error(run(root, &["abc484", "--json"]), "required");
    assert_error(
        run(
            root,
            &["abc484", "A", "--task-id", "archive_round_b", "--json"],
        ),
        "cannot be used with",
    );
    assert_error(run(root, &["abc484", "A"]), "--json");
    for id in ["../abc484", "abc484:stream", "CON", "abc%34", ""] {
        assert_error(run(root, &[id, "A", "--json"]), "invalid contest ID");
    }
    for task_id in ["../archive_round_b", "archive%5fb", "abc484/a", ""] {
        assert_error(
            run(root, &["abc484", "--task-id", task_id, "--json"]),
            "invalid task ID",
        );
    }
    assert_error(
        run(root, &["abc484", "../A", "--json"]),
        "invalid problem index",
    );
    assert_error(
        run(root, &["abc484", "A", "--json", "--language", "rust"]),
        "unsupported language",
    );
}

#[test]
fn rejects_wrong_manifest_owner_and_an_interrupted_contest_update() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let contest = make_contest(root.join("abc484"));
    fs::write(contest.join("A.cpp"), CPP).unwrap();
    let metadata = contest.join(".atc/contest.toml");
    fs::write(&metadata, MANIFEST.replace("abc484", "abc485")).unwrap();
    assert_error(run(root, &["abc484", "A", "--json"]), "contest ID mismatch");

    fs::write(metadata, MANIFEST).unwrap();
    let staging = contest.join(".atc-refresh-interrupted");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("recovery-intent.toml"), "incomplete").unwrap();
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "requires recovery check",
    );
    assert!(staging.join("recovery-intent.toml").exists());
}

#[test]
fn enforces_the_contest_manifest_version_and_read_limits() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let contest = make_contest(root.join("abc484"));
    let metadata = contest.join(".atc/contest.toml");
    fs::write(&metadata, MANIFEST.replace("version = 1", "version = 2")).unwrap();
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "unsupported contest metadata version",
    );
    fs::write(&metadata, [0xff]).unwrap();
    assert_error(run(root, &["abc484", "A", "--json"]), "not valid UTF-8");
    fs::OpenOptions::new()
        .write(true)
        .open(metadata)
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "exceeds 16777216 bytes",
    );
}

#[test]
fn enforces_the_source_byte_limit_and_utf8() {
    const LIMIT: usize = 1024 * 1024;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let contest = make_contest(root.join("abc484"));
    let source = contest.join("A.cpp");
    fs::write(&source, [0xff, 0xfe]).unwrap();
    assert_error(run(root, &["abc484", "A", "--json"]), "not valid UTF-8");

    let text = "x".repeat(LIMIT);
    fs::write(&source, &text).unwrap();
    let output = run(root, &["abc484", "A", "--json"]);
    assert_eq!(read_json(&output)["source"].as_str(), Some(text.as_str()));
    fs::OpenOptions::new()
        .write(true)
        .open(&source)
        .unwrap()
        .set_len((LIMIT + 1) as u64)
        .unwrap();
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "exceeds 1048576 bytes",
    );
}

#[test]
fn succeeds_offline_with_no_writes_or_user_config_dependency() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let contest = make_contest(root.join("abc484"));
    let source = contest.join("A.cpp");
    fs::write(&source, CPP).unwrap();
    let user_dir = root.join("unusable-user-directory");
    fs::write(&user_dir, "regular file, cannot contain config or state").unwrap();
    let metadata = contest.join(".atc/contest.toml");
    let paths = [&source, &metadata, &user_dir];
    let snapshot = || {
        paths.map(|path| {
            (
                fs::read(path).unwrap(),
                fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
    };
    let before = snapshot();

    let output = command(root)
        .args(["abc484", "A", "--json"])
        .envs([
            ("HOME", &user_dir),
            ("USERPROFILE", &user_dir),
            ("APPDATA", &user_dir),
            ("LOCALAPPDATA", &user_dir),
            ("XDG_CONFIG_HOME", &user_dir),
            ("XDG_STATE_HOME", &user_dir),
            ("XDG_CACHE_HOME", &user_dir),
            ("XDG_DATA_HOME", &user_dir),
        ])
        .env("HTTPS_PROXY", "http://127.0.0.1:0")
        .env("https_proxy", "http://127.0.0.1:0")
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .output()
        .unwrap();
    assert_eq!(read_json(&output)["source"], CPP);
    assert_eq!(before, snapshot());
    assert_eq!(fs::read_dir(root).unwrap().count(), 2);
    assert_eq!(fs::read_dir(&contest).unwrap().count(), 2);
    assert_eq!(fs::read_dir(contest.join(".atc")).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn rejects_source_and_metadata_symlinks_including_dangling_links() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let contest = make_contest(root.join("abc484"));
    let outside = root.join("outside.cpp");
    let source = contest.join("A.cpp");
    symlink(&outside, &source).unwrap();
    assert_error(run(root, &["abc484", "A", "--json"]), "not a regular file");
    fs::write(&outside, CPP).unwrap();
    assert_error(run(root, &["abc484", "A", "--json"]), "not a regular file");
    fs::remove_file(&source).unwrap();
    fs::write(source, CPP).unwrap();

    let manifest = contest.join(".atc/contest.toml");
    let outside = root.join("outside.toml");
    fs::rename(&manifest, &outside).unwrap();
    symlink(outside, manifest).unwrap();
    assert_error(run(root, &["abc484", "A", "--json"]), "not a regular file");
}

#[cfg(unix)]
#[test]
fn rejects_a_fifo_without_blocking() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let contest = make_contest(root.join("abc484"));
    let status = Command::new("mkfifo")
        .arg(contest.join("A.cpp"))
        .status()
        .unwrap();
    assert!(status.success());
    assert_error(run(root, &["abc484", "A", "--json"]), "not a regular file");
}

#[cfg(unix)]
fn link_directory(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn link_directory(link: &Path, target: &Path) {
    let output = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[cfg(any(unix, windows))]
#[test]
fn rejects_directory_links_in_routing_and_in_the_contest() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let outside = root.join("real-contests");
    let contest = make_contest(outside.join("ABC/abc484"));
    fs::write(contest.join("A.cpp"), CPP).unwrap();

    fs::write(root.join(".atc-workspace.toml"), MAPPING).unwrap();
    link_directory(&root.join("Contests"), &outside);
    assert_error(
        run(root, &["abc484", "A", "--json"]),
        "workspace mapping directory",
    );

    fs::remove_file(root.join(".atc-workspace.toml")).unwrap();
    link_directory(&root.join("abc484"), &contest);
    assert_error(run(root, &["abc484", "A", "--json"]), "contest directory");

    let marker = contest.join(".atc");
    let outside_marker = root.join("real-marker");
    fs::rename(&marker, &outside_marker).unwrap();
    link_directory(&marker, &outside_marker);
    assert_error(
        run(&outside.join("ABC"), &["abc484", "A", "--json"]),
        "workspace marker",
    );

    let output = command(root)
        .args(["abc484", "A", "--json", "--workspace"])
        .arg(root.join("Contests").join("."))
        .output()
        .unwrap();
    assert_error(output, "Workspace directory");
}
