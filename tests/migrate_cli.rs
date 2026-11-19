use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
mod support;

fn cli(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atc"));
    command.current_dir(cwd).env(
        "ATC_FIXTURE_DIR",
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures"),
    );
    command
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const CURRENT_METADATA: &str = concat!(
    "version = 1\ncontest_id = 'abc466'\n",
    "[[problems]]\nindex = 'A'\ntitle = 'A'\n",
    "task_id = 'abc466_a'\nurl = 'https://atcoder.jp/contests/abc466/tasks/abc466_a'\n",
    "sample_count = 0\n",
);

fn contest_with_custom_tests(destination: &Path) {
    let custom = destination.join("tests/A/custom");
    fs::create_dir_all(custom.join("empty")).unwrap();
    fs::create_dir_all(destination.join(".atc")).unwrap();
    fs::write(destination.join(".atc/contest.toml"), CURRENT_METADATA).unwrap();
    fs::write(destination.join(".atc/user-data"), b"user data").unwrap();
    fs::write(destination.join("A.cpp"), b"user source\r\n").unwrap();
    fs::write(custom.join("case.in"), b"123\n").unwrap();
    fs::write(custom.join("unknown.bin"), [0xff, 0, 1]).unwrap();
}

fn backup(output: &Output) -> PathBuf {
    let message = stderr(output);
    assert!(output.status.success(), "{message}");
    PathBuf::from(
        message
            .lines()
            .find_map(|line| line.strip_prefix("Backup: "))
            .expect("migration must report its backup path"),
    )
}

fn old_contest(destination: &Path, with_marker: bool) {
    fs::create_dir_all(destination.join("tests/a/extra")).unwrap();
    fs::write(
        destination.join("tests/a/extra/my-case.bin"),
        [0xff, 0, 0x0a],
    )
    .unwrap();
    fs::create_dir(destination.join("tests/empty")).unwrap();
    fs::write(destination.join("A.cpp"), b"// existing source\r\n").unwrap();
    fs::write(destination.join("extra.py"), [0xef, 0xbb, 0xbf, b'\n']).unwrap();
    if with_marker {
        fs::create_dir_all(destination.join(".atc/stress")).unwrap();
        fs::create_dir_all(destination.join(".atc/user-inputs")).unwrap();
        fs::write(destination.join(".atc/stress/failed.in"), b"saved stress").unwrap();
        fs::write(destination.join(".atc/user-inputs/data"), b"saved input").unwrap();
        fs::write(destination.join(".atc/other"), b"user data").unwrap();
    }
}

fn tree(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, directory: &Path, entries: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if entry.file_type().unwrap().is_dir() {
                entries.push((relative, None));
                visit(root, &path, entries);
            } else {
                entries.push((relative, Some(fs::read(path).unwrap())));
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort();
    entries
}

#[test]
fn migrate_refuses_a_workspace_root_before_planning_or_writing() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["abc466", "My Workspace"] {
        let root = temp.path().join(name);
        old_contest(&root, true);
        fs::write(root.join(".atc-workspace.toml"), "version = 1\n").unwrap();
        let before = tree(&root);

        for flag in ["--yes", "--dry-run"] {
            let output = cli(&root).args(["migrate", flag]).output().unwrap();
            let message = stderr(&output);
            assert_eq!(output.status.code(), Some(1), "{message}");
            assert!(message.contains("Workspace boundary at"), "{message}");
            assert!(message.contains("from that Workspace root"), "{message}");
            assert!(
                message.contains("`-c <contest-id>` or `--all`"),
                "{message}"
            );
            assert!(!message.contains("Migrate abc466"), "{message}");
            assert!(!message.contains("Fetching"), "{message}");
            assert_eq!(tree(&root), before);
        }
    }
}

#[test]
fn migrate_refuses_an_explicit_target_that_resolves_to_a_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join(".atc-workspace.toml"),
        "version = 1\n[[paths]]\npattern = '^abc'\npath = 'beginner'\n",
    )
    .unwrap();
    let destination = root.join("beginner/abc466");
    old_contest(&destination, true);
    let marker = destination.join(".atc-workspace.toml");
    fs::write(&marker, "version = 1\n").unwrap();
    let before = tree(root);

    let output = cli(root)
        .args(["migrate", "-c", "abc466", "--yes"])
        .output()
        .unwrap();
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(message.contains("Workspace boundary at"), "{message}");
    assert!(!message.contains("Fetching"), "{message}");
    assert_eq!(tree(root), before);

    fs::remove_file(marker).unwrap();
    let output = cli(root)
        .args(["migrate", "-c", "abc466", "--yes"])
        .output()
        .unwrap();
    let archive = backup(&output);
    assert!(archive.join("previous-tests/a/extra/my-case.bin").is_file());
    assert!(destination.join("tests/A/sample-1.in").is_file());
    assert_eq!(
        fs::read(destination.join("A.cpp")).unwrap(),
        b"// existing source\r\n"
    );
}

#[test]
fn migrate_preserves_sources_and_restorable_tests_with_or_without_a_marker() {
    for with_marker in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        old_contest(&destination, with_marker);
        let output = cli(&destination)
            .args(["migrate", "--yes"])
            .output()
            .unwrap();
        let backup = backup(&output);
        // The child reports its OS-provided working directory, which can use a
        // different spelling from TempDir (for example /private/var on macOS).
        assert_eq!(
            backup.parent().unwrap().canonicalize().unwrap(),
            destination.canonicalize().unwrap()
        );
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".atc-recovery-migrate-")
        );
        assert!(backup.join("recovery-complete.toml").is_file());
        assert!(!backup.join("previous-contest.toml").exists());
        assert!(!destination.join("tests/a/extra").exists());
        assert!(destination.join("tests/A/sample-1.in").is_file());
        assert_eq!(
            fs::read(destination.join("A.cpp")).unwrap(),
            b"// existing source\r\n"
        );
        assert_eq!(
            fs::read(destination.join("extra.py")).unwrap(),
            [0xef, 0xbb, 0xbf, b'\n']
        );
        if with_marker {
            for (path, contents) in [
                (".atc/stress/failed.in", &b"saved stress"[..]),
                (".atc/user-inputs/data", &b"saved input"[..]),
                (".atc/other", &b"user data"[..]),
            ] {
                assert_eq!(fs::read(destination.join(path)).unwrap(), contents);
            }
        }

        let entry_count = fs::read_dir(&destination).unwrap().count();
        for (args, message) in [
            (&["migrate", "--yes"][..], "migration skipped"),
            (&["refresh"][..], "refresh skipped"),
        ] {
            let result = cli(&destination)
                .args(args)
                .env("ATC_FIXTURE_DIR", temp.path().join("no-fixtures"))
                .output()
                .unwrap();
            assert!(result.status.success(), "{}", stderr(&result));
            assert!(stderr(&result).contains(message));
        }
        assert_eq!(fs::read_dir(&destination).unwrap().count(), entry_count);

        let restored = temp.path().join("restored-tests");
        fs::rename(backup.join("previous-tests"), &restored).unwrap();
        assert_eq!(
            fs::read(restored.join("a/extra/my-case.bin")).unwrap(),
            [0xff, 0, 0x0a]
        );
        assert!(restored.join("empty").is_dir());
    }
}

#[test]
fn migrate_uses_workspace_mapping_and_preserves_old_metadata_bytes() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".atc-workspace.toml"),
        "version = 1\n[[paths]]\npattern = '^abc'\npath = 'beginner'\n",
    )
    .unwrap();
    let decoy = temp.path().join("abc466");
    old_contest(&decoy, false);
    let destination = temp.path().join("beginner").join("abc466");
    for original in [
        "version = 99\ncontest_id = 'abc466'\n",
        "version = 1\ncontest_id = 'abc466'\nproblems = 'invalid schema'\n",
        concat!(
            "version = 1\ncontest_id = 'abc466'\n",
            "[[problems]]\nindex = 'A'\ntitle = 'A'\ntask_id = 'abc466_a'\n",
            "url = 'https://atcoder.jp/contests/abc466/tasks/abc466_a'\nsample_count = 0\n",
        ),
    ] {
        old_contest(&destination, true);
        fs::write(destination.join(".atc/contest.toml"), original).unwrap();
        let output = cli(temp.path())
            .args(["migrate", "-c", "abc466", "--yes"])
            .output()
            .unwrap();
        let message = stderr(&output);
        let reported = message
            .lines()
            .find_map(|line| line.strip_prefix("Migrate abc466 at "));
        let reported = Path::new(reported.unwrap_or_else(|| panic!("{message}")));
        assert!(
            reported.ends_with(Path::new("beginner").join("abc466")),
            "{message}"
        );
        assert_eq!(
            fs::canonicalize(reported).unwrap(),
            fs::canonicalize(&destination).unwrap(),
            "{message}"
        );
        let backup = backup(&output);
        assert_eq!(
            fs::read(backup.join("previous-contest.toml")).unwrap(),
            original.as_bytes()
        );
        assert!(backup.join("previous-tests/a/extra/my-case.bin").is_file());
        assert!(!decoy.join(".atc").exists());
        fs::remove_dir_all(&destination).unwrap();
    }
}

#[test]
fn single_and_batch_migrate_archive_custom_subdirectories_that_refresh_refuses() {
    for batch in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join(".atc-workspace.toml"), "version = 1\n").unwrap();
        let contest = root.join("abc466");
        contest_with_custom_tests(&contest);
        let before = tree(root);
        let tests_before = tree(&contest.join("tests"));

        for (cwd, args) in [
            (contest.as_path(), &["refresh"][..]),
            (contest.as_path(), &["refresh", "--force"][..]),
            (root, &["refresh", "--all", "--dry-run"][..]),
        ] {
            let output = cli(cwd).args(args).output().unwrap();
            let message = stderr(&output);
            assert_eq!(output.status.code(), Some(1), "{message}");
            assert!(
                message.contains("unowned entry in tests") && message.contains("custom"),
                "{message}"
            );
            assert!(!message.contains("Fetching"), "{message}");
            assert_eq!(tree(root), before);
        }
        for (cwd, args) in [
            (contest.as_path(), &["migrate", "--dry-run"][..]),
            (root, &["migrate", "--all", "--dry-run"][..]),
        ] {
            let output = cli(cwd).args(args).output().unwrap();
            let message = stderr(&output);
            assert!(output.status.success(), "{message}");
            assert!(
                message.contains("tests contain entries outside atc management"),
                "{message}"
            );
            assert!(!message.contains("Fetching"), "{message}");
            assert_eq!(tree(root), before);
        }

        let output = if batch {
            cli(root)
                .args(["migrate", "--all", "--yes"])
                .output()
                .unwrap()
        } else {
            cli(&contest).args(["migrate", "--yes"]).output().unwrap()
        };
        assert!(output.status.success(), "{}", stderr(&output));
        let archive = fs::read_dir(&contest)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".atc-recovery-migrate-")
            })
            .expect("migration must keep a recovery archive");
        assert!(archive.join("recovery-complete.toml").is_file());
        assert_eq!(tree(&archive.join("previous-tests")), tests_before);
        assert_eq!(
            fs::read_to_string(archive.join("previous-contest.toml")).unwrap(),
            CURRENT_METADATA
        );
        assert!(contest.join("tests/A/sample-1.in").is_file());
        assert!(!contest.join("tests/A/custom").exists());
        assert_eq!(fs::read(contest.join("A.cpp")).unwrap(), b"user source\r\n");
        assert_eq!(
            fs::read(contest.join(".atc/user-data")).unwrap(),
            b"user data"
        );
        assert_eq!(
            fs::read(root.join(".atc-workspace.toml")).unwrap(),
            b"version = 1\n"
        );
    }
}

#[cfg(unix)]
#[test]
fn both_migrate_modes_reject_symlinks_and_special_files_inside_custom_subdirectories() {
    use std::ffi::CString;
    use std::os::unix::{ffi::OsStrExt as _, fs::symlink};

    for link in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join(".atc-workspace.toml"), "version = 1\n").unwrap();
        let contest = root.join("abc466");
        contest_with_custom_tests(&contest);
        let outside = root.join("outside");
        fs::write(&outside, b"outside data").unwrap();
        let entry = contest.join("tests/A/custom/unsafe-entry");
        if link {
            symlink(&outside, &entry).unwrap();
        } else {
            let path = CString::new(entry.as_os_str().as_bytes()).unwrap();
            // SAFETY: path is a valid C string pointing to an unused path in this test's TempDir.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }

        for (cwd, args) in [
            (contest.as_path(), &["migrate", "--yes"][..]),
            (root, &["migrate", "--all", "--yes"][..]),
        ] {
            let output = cli(cwd).args(args).output().unwrap();
            let message = stderr(&output);
            assert_eq!(output.status.code(), Some(1), "{message}");
            assert!(
                message.contains("cannot safely back up") && message.contains("unsafe-entry"),
                "{message}"
            );
            assert!(!message.contains("Fetching"), "{message}");
            assert_eq!(fs::read(outside.as_path()).unwrap(), b"outside data");
            assert!(fs::symlink_metadata(&entry).is_ok());
            assert!(!contest.join(".atc-update.lock").exists());
        }
        fs::remove_file(entry).unwrap();
        assert_eq!(fs::read_dir(&contest).unwrap().count(), 3);
        assert_eq!(
            fs::read_to_string(contest.join(".atc/contest.toml")).unwrap(),
            CURRENT_METADATA
        );
        assert_eq!(
            fs::read(contest.join("tests/A/custom/case.in")).unwrap(),
            b"123\n"
        );
    }
}

#[test]
fn migrate_refuses_mismatched_or_undeterminable_owner_and_interrupted_recovery() {
    for (metadata, interrupted, expected_error) in [
        (
            "version = 99\ncontest_id = 'abc467'\n",
            false,
            "contest ID mismatch",
        ),
        (
            "version = 1\ncontest_id = 'abc467'\n[[problems]]\nindex = '../escape'\ntitle = 'A'\ntask_id = 'abc467_a'\nurl = 'https://atcoder.jp/contests/abc467/tasks/abc467_a'\nsample_count = 0\n",
            false,
            "contest ID mismatch",
        ),
        (
            "version = 1\ncontest_id = 'abc466'\ninvalid = [\n",
            false,
            "cannot confirm ownership",
        ),
        (
            "version = 1\ncontest_id = 'abc466'\n",
            true,
            "requires recovery check",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        old_contest(&destination, true);
        fs::write(destination.join(".atc/contest.toml"), metadata).unwrap();
        if interrupted {
            let recovery = destination.join(".atc-migrate-interrupted");
            fs::create_dir(&recovery).unwrap();
            fs::write(recovery.join("recovery-intent.toml"), "interrupted").unwrap();
        }
        let count = fs::read_dir(&destination).unwrap().count();
        let output = cli(&destination)
            .args(["migrate", "--yes"])
            .output()
            .unwrap();
        let error = stderr(&output);
        assert!(!output.status.success(), "{error}");
        assert!(error.contains(expected_error), "{error}");
        assert!(!error.contains("Fetching"), "{error}");
        assert_eq!(
            fs::read(destination.join(".atc/contest.toml")).unwrap(),
            metadata.as_bytes()
        );
        assert!(destination.join("tests/a/extra/my-case.bin").is_file());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), count);
    }
}

#[cfg(unix)]
#[test]
fn migrate_refuses_symlinks_nested_under_unknown_tests_even_for_an_old_version() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    old_contest(&destination, true);
    fs::write(
        destination.join(".atc/contest.toml"),
        "version = 99\ncontest_id = 'abc466'\n",
    )
    .unwrap();
    let external = temp.path().join("outside");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("notes"), "untouched").unwrap();
    let link = destination.join("tests/a/extra/outside");
    symlink(&external, &link).unwrap();
    let result = cli(&destination)
        .args(["migrate", "--yes"])
        .output()
        .unwrap();
    assert!(!result.status.success(), "{}", stderr(&result));
    assert!(
        stderr(&result).contains("cannot safely back up"),
        "{}",
        stderr(&result)
    );
    assert!(!stderr(&result).contains("Fetching"));
    assert_eq!(fs::read_link(link).unwrap(), external);
    assert_eq!(fs::read(external.join("notes")).unwrap(), b"untouched");
}

#[cfg(windows)]
#[test]
fn migrate_refuses_junctions_nested_under_custom_tests() {
    for current in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        let link = if current {
            contest_with_custom_tests(&destination);
            destination.join("tests/A/custom/テスト (junction)")
        } else {
            old_contest(&destination, false);
            destination.join("tests/a/extra/テスト (junction)")
        };
        let target = temp.path().join("外部 (target)");
        fs::create_dir(&target).unwrap();
        support::link_directory(&link, &target);
        let result = cli(&destination)
            .args(["migrate", "--yes"])
            .output()
            .unwrap();
        fs::remove_dir(link).unwrap();
        assert!(!result.status.success(), "{}", stderr(&result));
        assert!(
            stderr(&result).contains("cannot safely back up"),
            "{}",
            stderr(&result)
        );
        assert!(!stderr(&result).contains("Fetching"));
    }
}

#[test]
fn migrate_dry_run_and_noninteractive_input_leave_the_tree_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    old_contest(&destination, false);
    let approval = temp.path().join("approval.txt");
    fs::write(&approval, b"yes\n").unwrap();
    let run = |args: &[&str]| {
        cli(&destination)
            .args(args)
            .env("ATC_FIXTURE_DIR", temp.path().join("no-fixtures"))
            .stdin(Stdio::from(fs::File::open(&approval).unwrap()))
            .output()
            .unwrap()
    };
    let dry = run(&["migrate", "--dry-run", "--yes"]);
    assert!(dry.status.success(), "{}", stderr(&dry));
    assert!(stderr(&dry).contains("tests/ -> previous-tests/"));
    assert!(!stderr(&dry).contains("Fetching"));
    let refused = run(&["migrate"]);
    assert!(!refused.status.success(), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("explicit approval with --yes"));
    assert!(!stderr(&refused).contains("Fetching"));
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 3);
    assert!(destination.join("tests/a/extra/my-case.bin").is_file());

    let out_of_scope = cli(&destination)
        .args(["migrate", "--all"])
        .output()
        .unwrap();
    assert_eq!(out_of_scope.status.code(), Some(1));
    assert!(stderr(&out_of_scope).contains("run --all from a Workspace root"));
}

#[test]
fn migrate_http_failure_preserves_all_existing_data() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    old_contest(&destination, true);
    let original = b"version = 99\ncontest_id = 'abc466'\n";
    fs::write(destination.join(".atc/contest.toml"), original).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let proxy = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("loopback proxy received no request: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        stream
            .write_all(
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        line
    });
    let output = cli(&destination)
        .args(["migrate", "--yes"])
        .env_remove("ATC_FIXTURE_DIR")
        .env_remove("https_proxy")
        .env_remove("no_proxy")
        .env("HTTPS_PROXY", proxy_url)
        .env("NO_PROXY", "")
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("APPDATA", temp.path().join("state"))
        .output()
        .unwrap();
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("AtCoder operation failed"),
        "{}",
        stderr(&output)
    );
    assert_eq!(proxy.join().unwrap(), "CONNECT atcoder.jp:443 HTTP/1.1\r\n");
    assert_eq!(
        fs::read(destination.join(".atc/contest.toml")).unwrap(),
        original
    );
    assert_eq!(
        fs::read(destination.join("tests/a/extra/my-case.bin")).unwrap(),
        [0xff, 0, 0x0a]
    );
    assert_eq!(
        fs::read(destination.join("A.cpp")).unwrap(),
        b"// existing source\r\n"
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 4);
}
