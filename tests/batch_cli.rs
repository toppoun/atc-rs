use std::fs;
use std::io::{self, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

#[cfg(any(unix, windows))]
mod support;

#[cfg(windows)]
#[path = "batch_cli/windows_interrupt.rs"]
mod windows_interrupt;

fn workspace(config: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join(".atc-workspace.toml"), config).unwrap();
    root
}

fn cli(root: &Path, args: &[&str], fixtures: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atc"))
        .current_dir(root)
        .env("ATC_FIXTURE_DIR", fixtures)
        .args(args)
        .output()
        .unwrap()
}

fn cli_without_http(root: &Path, args: &[&str]) -> Output {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, finished) = mpsc::channel();
    let proxy = std::thread::spawn(move || {
        let mut connections = 0;
        let mut stopping = false;
        loop {
            match listener.accept() {
                Ok(_) => connections += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && stopping => {
                    return connections;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    stopping = !matches!(
                        finished.recv_timeout(Duration::from_millis(5)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    );
                }
                Err(error) => panic!("loopback proxy failed: {error}"),
            }
        }
    });
    let state = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_atc"))
        .current_dir(root)
        .env_remove("ATC_FIXTURE_DIR")
        .env_remove("https_proxy")
        .env_remove("no_proxy")
        .env("HTTPS_PROXY", proxy_url)
        .env("NO_PROXY", "")
        .env("LOCALAPPDATA", state.path())
        .env("APPDATA", state.path())
        .env("XDG_STATE_HOME", state.path())
        .args(args)
        .output()
        .unwrap();
    stop.send(()).unwrap();
    assert_eq!(proxy.join().unwrap(), 0, "{}", stderr(&output));
    assert_eq!(fs::read_dir(state.path()).unwrap().count(), 0);
    output
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn summary_count(output: &str, label: &str, expected: usize) {
    let summary = output.split_once("Summary:\n").unwrap().1;
    let count = summary
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(label))
        .unwrap_or_else(|| panic!("{label} missing from {summary}"));
    assert_eq!(
        count.trim().parse::<usize>().unwrap(),
        expected,
        "{summary}"
    );
}

fn manifest(path: &Path, id: &str, count: usize) {
    fs::create_dir_all(path.join(".atc")).unwrap();
    fs::write(
        path.join(".atc/contest.toml"),
        format!(
            "version = 1\ncontest_id = '{id}'\n[[problems]]\nindex = 'A'\ntitle = 'A'\n\
             task_id = '{id}_a'\nurl = 'https://atcoder.jp/contests/{id}/tasks/{id}_a'\n\
             sample_count = {count}\n"
        ),
    )
    .unwrap();
}

fn legacy(path: &Path) {
    fs::create_dir_all(path.join("tests/legacy")).unwrap();
    fs::write(path.join("tests/legacy/custom.in"), b"old test").unwrap();
    fs::write(path.join("A.cpp"), b"source").unwrap();
}

fn fixtures(root: &Path, ids: &[&str]) {
    fs::create_dir_all(root.join("contests")).unwrap();
    fs::create_dir_all(root.join("problems")).unwrap();
    for id in ids {
        let href = format!("/contests/{id}/tasks/{id}_a");
        fs::write(root.join("contests").join(format!("{id}.html")), format!(
            "<table><tbody><tr><td><a href='{href}'>A</a></td><td><a href='{href}'>Task</a></td></tr></tbody></table>"
        )).unwrap();
        fs::write(
            root.join("problems").join(format!("{id}_a.html")),
            "<div id='task-statement'><span class='lang-en'><div class='part'>\
             <section><h3>Sample Input 1</h3><pre>42</pre></section>\
             <section><h3>Sample Output 1</h3><pre>43</pre></section></div></span></div>",
        )
        .unwrap();
    }
}

fn tree(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        for entry in fs::read_dir(path).unwrap() {
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
fn current_metadata_resolves_an_uppercase_contest_for_single_and_batch_updates() {
    let root = workspace("version = 1\n[[paths]]\npattern = '^(?i:abc083)$'\npath = 'ABC'\n");
    let contest = root.path().join("ABC").join("ABC083");
    manifest(&contest, "abc083", 1);
    let fixtures_root = tempfile::tempdir().unwrap();
    fixtures(fixtures_root.path(), &["abc083"]);

    let output = cli_without_http(root.path(), &["refresh", "--all", "--dry-run"]);
    let message = stderr(&output);
    assert!(output.status.success(), "{message}");
    summary_count(&message, "Scanned:", 1);
    summary_count(&message, "Refresh planned:", 1);
    let relative = Path::new("ABC").join("ABC083");
    assert!(
        message.contains(&relative.display().to_string()),
        "{message}"
    );

    let output = cli(
        root.path(),
        &["refresh", "--all", "--yes"],
        fixtures_root.path(),
    );
    let message = stderr(&output);
    assert!(output.status.success(), "{message}");
    summary_count(&message, "Repaired:", 1);
    let output = cli(&contest, &["refresh", "--force"], fixtures_root.path());
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read(contest.join("tests/A/sample-1.in")).unwrap(),
        b"42"
    );
    let before = tree(root.path());
    for (cwd, args) in [
        (contest.as_path(), &["refresh"][..]),
        (contest.as_path(), &["migrate", "--dry-run"][..]),
        (root.path(), &["refresh", "-c", "abc083"][..]),
        (root.path(), &["migrate", "-c", "abc083", "--dry-run"][..]),
        (root.path(), &["refresh", "--all"][..]),
    ] {
        let output = cli_without_http(cwd, args);
        let message = stderr(&output);
        assert!(output.status.success(), "{args:?}: {message}");
        assert!(
            message.contains("abc083") && message.contains("healthy"),
            "{message}"
        );
    }
    for args in [
        &["refresh", "-c", "ABC083", "--force"][..],
        &["migrate", "-c", "ABC083", "--yes"][..],
    ] {
        let output = cli_without_http(root.path(), args);
        let message = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{message}");
        assert!(message.contains("contest ID mismatch"), "{message}");
    }
    assert_eq!(tree(root.path()), before);
}

#[test]
fn case_variant_legacy_contests_are_reported_without_authorizing_an_update() {
    let root = workspace("version = 1\n[[paths]]\npattern = '^abc083$'\npath = 'abc'\n");
    let contest = root.path().join("abc").join("ABC083");
    legacy(&contest);
    for metadata in [None, Some("version = 99\ncontest_id = 'abc083'\n")] {
        if let Some(metadata) = metadata {
            fs::create_dir(contest.join(".atc")).unwrap();
            fs::write(contest.join(".atc/contest.toml"), metadata).unwrap();
        }
        let before = tree(root.path());
        for args in [
            &["migrate", "--all", "--dry-run"][..],
            &["migrate", "--all", "--yes"][..],
            &["refresh", "--all", "--force", "--yes"][..],
        ] {
            let output = cli_without_http(root.path(), args);
            let message = stderr(&output);
            assert_eq!(output.status.code(), Some(1), "{message}");
            assert!(
                message.contains("Found 1 contests.") && message.contains("ABC083"),
                "{message}"
            );
            assert!(
                message.contains("cannot confirm contest ownership")
                    && message.contains("verify the official contest ID"),
                "{message}"
            );
            summary_count(&message, "Failed:", 1);
        }
        for (cwd, args) in [
            (contest.as_path(), &["refresh", "--force"][..]),
            (contest.as_path(), &["migrate", "--yes"][..]),
            (root.path(), &["migrate", "-c", "abc083", "--yes"][..]),
        ] {
            let output = cli_without_http(cwd, args);
            let message = stderr(&output);
            assert_eq!(output.status.code(), Some(1), "{message}");
            assert!(
                message.contains("cannot confirm contest ownership"),
                "{message}"
            );
        }
        assert_eq!(tree(root.path()), before);
    }
}

#[test]
fn uppercase_contest_rejects_conflicting_metadata_and_a_nested_workspace() {
    let root = workspace("version = 1\n");
    let contest = root.path().join("ABC083");
    manifest(&contest, "abc084", 1);
    for (name, reason) in [
        (None, "contest ID mismatch"),
        (Some(".atc-workspace.toml"), "Workspace boundary at"),
    ] {
        if let Some(name) = name {
            manifest(&contest, "abc083", 1);
            fs::write(contest.join(name), "version = 1\n").unwrap();
        }
        let before = tree(root.path());
        for (cwd, args) in [
            (contest.as_path(), &["refresh", "--force"][..]),
            (contest.as_path(), &["migrate", "--yes"][..]),
            (root.path(), &["refresh", "-c", "abc083", "--force"][..]),
            (root.path(), &["refresh", "--all", "--force", "--yes"][..]),
            (root.path(), &["migrate", "--all", "--dry-run"][..]),
        ] {
            let output = cli_without_http(cwd, args);
            let message = stderr(&output);
            assert_eq!(output.status.code(), Some(1), "{message}");
            assert!(message.contains(reason), "{message}");
        }
        assert_eq!(tree(root.path()), before);
    }
}

#[test]
fn batch_deduplicates_parent_aliases_and_refuses_distinct_case_variants() {
    let root = workspace("version = 1\n[[paths]]\npattern = '^abc083$'\npath = 'abc'\n");
    let series = root.path().join("abc");
    let contest = series.join("ABC083");
    manifest(&contest, "abc083", 0);
    let output = cli_without_http(root.path(), &["refresh", "--all"]);
    let message = stderr(&output);
    assert!(output.status.success(), "{message}");
    let relative = Path::new("abc").join("ABC083");
    assert!(
        message.contains(&relative.display().to_string()),
        "{message}"
    );
    summary_count(&message, "Scanned:", 1);
    summary_count(&message, "Healthy skipped:", 1);

    let duplicate = root.path().join("abc083");
    manifest(&duplicate, "abc083", 0);
    let before = tree(root.path());
    let output = cli_without_http(root.path(), &["refresh", "--all", "--force", "--yes"]);
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(
        message.contains("duplicate contest ID \"abc083\"") && message.contains("ABC083"),
        "{message}"
    );
    summary_count(&message, "Failed:", 1);
    assert_eq!(tree(root.path()), before);
}

#[test]
fn explicit_and_batch_updates_share_the_nested_workspace_boundary() {
    let fixtures_root = tempfile::tempdir().unwrap();
    fixtures(fixtures_root.path(), &["abc466"]);
    for operation in ["refresh", "migrate"] {
        for marker_at_target in [false, true] {
            let root = workspace(
                "version = 1\n[[paths]]\npattern = '^abc466$'\npath = 'work/Vault/series'\n",
            );
            let inner = root.path().join("work/vault");
            // Keep the path spelling distinct on conventional case-insensitive NTFS.
            let inner = if cfg!(windows) {
                inner
            } else {
                root.path().join("work/Vault")
            };
            let contest = inner.join("series/abc466");
            if operation == "refresh" {
                manifest(&contest, "abc466", 1);
            } else {
                legacy(&contest);
            }
            let boundary = if marker_at_target { &contest } else { &inner };
            let marker_name = if cfg!(windows) {
                ".ATC-WORKSPACE.TOML"
            } else {
                ".atc-workspace.toml"
            };
            fs::write(
                boundary.join(marker_name),
                "version = 1\n[[paths]]\npattern = '^abc466$'\npath = 'series'\n",
            )
            .unwrap();
            let before = tree(root.path());
            let mut single = vec![operation, "-c", "abc466"];
            let mut batch = vec![operation, "--all", "--yes"];
            if operation == "refresh" {
                single.push("--force");
                batch.push("--force");
            } else {
                single.push("--yes");
            }
            for args in [&single, &batch] {
                let result = cli(root.path(), args, fixtures_root.path());
                let message = stderr(&result);
                assert_eq!(result.status.code(), Some(1), "{message}");
                assert!(message.contains("Workspace boundary at"), "{message}");
                assert!(message.contains("from that Workspace root"), "{message}");
                let reported_boundary = root.path().join(if marker_at_target {
                    "work/Vault/series/abc466"
                } else {
                    "work/Vault"
                });
                let reported_boundary = reported_boundary.components().collect::<PathBuf>();
                assert!(
                    message.contains(&reported_boundary.display().to_string()),
                    "{message}"
                );
                assert!(!message.contains("Fetching"), "{message}");
                assert_eq!(tree(root.path()), before);
            }
            if !marker_at_target {
                let result = cli(&inner, &single, fixtures_root.path());
                assert!(result.status.success(), "{}", stderr(&result));
                assert!(contest.join("tests/A/sample-1.in").is_file());
            }
        }
    }
}

#[cfg(windows)]
#[test]
fn update_routes_reject_junctions_at_intermediate_and_target_directories() {
    let outside = tempfile::tempdir().unwrap();
    let contest = outside.path().join("abc466");
    manifest(&contest, "abc466", 1);
    fs::create_dir_all(contest.join("tests").join("A")).unwrap();
    fs::write(contest.join("tests").join("A").join("sample-1.in"), "keep").unwrap();
    let before = tree(outside.path());
    for link_at_target in [false, true] {
        let root =
            workspace("version = 1\n[[paths]]\npattern = '^abc466$'\npath = 'work/series'\n");
        let work = root.path().join("work");
        fs::create_dir(&work).unwrap();
        let series = work.join("series");
        let (link, target) = if link_at_target {
            fs::create_dir(&series).unwrap();
            (series.join("abc466"), contest.as_path())
        } else {
            (series, outside.path())
        };
        support::link_directory(&link, target);
        for args in [
            &["refresh", "-c", "abc466", "--force"][..],
            &["migrate", "-c", "abc466", "--yes"][..],
            &["refresh", "--all", "--yes", "--force"][..],
            &["migrate", "--all", "--yes"][..],
        ] {
            let result = cli(root.path(), args, root.path());
            let message = stderr(&result);
            assert_eq!(result.status.code(), Some(1), "{message}");
            assert!(message.contains("reparse point"), "{message}");
            assert!(!message.contains("Fetching"), "{message}");
            assert_eq!(tree(outside.path()), before);
        }
    }
}

#[test]
fn dry_run_plans_256_contests_with_large_test_files_without_fetching_or_writing() {
    #[derive(Debug, PartialEq, Eq)]
    enum Entry {
        Directory,
        File {
            digest: Vec<u8>,
            modified: SystemTime,
        },
    }

    fn entries(directory: &Path, state: &mut Vec<(PathBuf, Entry)>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                entries(&path, state);
                state.push((path, Entry::Directory));
            } else {
                assert!(
                    kind.is_file(),
                    "unexpected entry type at {}",
                    path.display()
                );
                let mut file = fs::File::open(&path).unwrap();
                let mut digest = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
                let mut buffer = [0; 64 * 1024];
                loop {
                    let count = file.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                }
                let modified = fs::metadata(&path).unwrap().modified().unwrap();
                state.push((
                    path,
                    Entry::File {
                        digest: digest.finish().as_ref().to_vec(),
                        modified,
                    },
                ));
            }
        }
    }

    for (operation, label) in [
        ("refresh", "Refresh planned:"),
        ("migrate", "Migration planned:"),
    ] {
        let root = workspace("version = 1\n");
        for n in 100..356 {
            let id = format!("abc{n}");
            let contest = root.path().join(&id);
            let test_file = if operation == "refresh" {
                manifest(&contest, &id, 2);
                let samples = contest.join("tests/A");
                fs::create_dir_all(&samples).unwrap();
                fs::write(samples.join("sample-1.out"), "42\n").unwrap();
                samples.join("sample-1.in")
            } else {
                let tests = contest.join("tests/custom");
                fs::create_dir_all(tests.join("empty")).unwrap();
                tests.join("legacy.in")
            };
            fs::File::create(test_file)
                .unwrap()
                .set_len(1024 * 1024)
                .unwrap();
        }
        let snapshot = || {
            let mut state = Vec::new();
            entries(root.path(), &mut state);
            state.sort_by(|left, right| left.0.cmp(&right.0));
            state
        };
        let before = snapshot();
        let output = cli_without_http(root.path(), &[operation, "--all", "--dry-run"]);
        let message = stderr(&output);
        assert!(output.status.success(), "{message}");
        summary_count(&message, label, 256);
        assert!(!message.contains("Fetching"), "{message}");
        let after = snapshot();
        assert_eq!(after.len(), before.len());
        for (actual, expected) in after.iter().zip(&before) {
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn batch_discovery_is_bounded_sorted_and_finds_mapped_and_legacy_contests() {
    let root =
        workspace("version = 1\n[[paths]]\npattern = '^special-2026$'\npath = 'custom/deep'\n");
    let paths = [
        "ABC(Atcoder Beginner Contest)/abc083",
        "ABC(Atcoder Beginner Contest)/abc084",
        "ABC(Atcoder Beginner Contest)/abc484",
        "AGC(Atcoder Grand Contest)/agc001",
        "ARC(Atcoder Regular Contest)/arc100",
        "custom/deep/special-2026",
    ];
    for path in paths {
        legacy(&root.path().join(path));
    }
    legacy(&root.path().join("notes/tests"));
    legacy(&root.path().join("notes/abc999"));
    legacy(&root.path().join("custom/deep/other"));
    fs::write(root.path().join("abc001"), b"ordinary file").unwrap();
    let unrelated = root.path().join("ABC/abc555");
    fs::create_dir_all(&unrelated).unwrap();
    fs::write(unrelated.join("notes.txt"), b"unrelated file").unwrap();
    let before = tree(root.path());

    let output = cli(
        root.path(),
        &["migrate", "--all", "--dry-run"],
        &root.path().join("absent"),
    );
    let message = stderr(&output);
    assert!(output.status.success(), "{message}");
    summary_count(&message, "Scanned:", 6);
    summary_count(&message, "Migration planned:", 6);
    let positions = [
        "abc083",
        "abc084",
        "abc484",
        "agc001",
        "arc100",
        "special-2026",
    ]
    .map(|id| message.find(id).unwrap());
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{message}"
    );
    for path in paths {
        let expected = root.path().join(path);
        // Windows emits backslashes and macOS may spell the temporary root with /private.
        let suffix = expected
            .strip_prefix(root.path())
            .unwrap()
            .components()
            .collect::<PathBuf>()
            .display()
            .to_string();
        assert!(message.contains(&suffix), "{message}");
    }
    assert!(message.contains("Blocked  MIGRATION PLANNED"));
    assert!(!message.contains("abc999"), "{message}");
    assert!(!message.contains("abc555"), "{message}");
    assert_eq!(tree(root.path()), before);

    let nested = cli(
        &root.path().join(paths[0]),
        &["migrate", "--all", "--dry-run"],
        &root.path().join("absent"),
    );
    assert!(!nested.status.success());
    assert!(stderr(&nested).contains("run --all from a Workspace root"));
}

#[test]
fn batch_rejects_duplicate_ambiguous_and_misrouted_contests() {
    let root = workspace(concat!(
        "version = 1\n[[paths]]\npattern = '^abc46'\npath = 'selected'\n",
        "[[paths]]\npattern = '^abc468$'\npath = 'alternate'\n",
    ));
    for path in [
        "selected/abc466",
        "ABC/abc466",
        "selected/abc468",
        "ABC/abc469",
        "selected/abc467",
    ] {
        legacy(&root.path().join(path));
    }
    let before = tree(root.path());
    let output = cli(
        root.path(),
        &["migrate", "--all", "--dry-run"],
        &root.path().join("missing"),
    );
    let message = stderr(&output);
    assert!(!output.status.success(), "{message}");
    assert!(
        message.contains("duplicate contest ID \"abc466\""),
        "{message}"
    );
    assert!(
        message.contains("multiple workspace path rules match contest ID \"abc468\""),
        "{message}"
    );
    assert!(
        message.contains("contest is outside its Workspace route"),
        "{message}"
    );
    summary_count(&message, "Scanned:", 4);
    summary_count(&message, "Failed:", 3);
    summary_count(&message, "Migration planned:", 1);
    assert_eq!(tree(root.path()), before);
}

#[cfg(any(unix, windows))]
#[test]
fn batch_does_not_follow_links_at_the_contest_or_any_mapping_component() {
    use support::link_directory;

    let root =
        workspace("version = 1\n[[paths]]\npattern = '^abc466$'\npath = 'redirect/nested'\n");
    let outside = tempfile::tempdir().unwrap();
    legacy(&outside.path().join("nested/abc466"));
    legacy(&outside.path().join("abc467"));
    link_directory(&root.path().join("redirect"), outside.path());
    link_directory(&root.path().join("ABC467"), &outside.path().join("abc467"));
    fs::create_dir(root.path().join("local")).unwrap();
    link_directory(&root.path().join("local/unrelated"), outside.path());
    let before = tree(outside.path());

    let output = cli(
        root.path(),
        &["migrate", "--all", "--yes"],
        &root.path().join("missing"),
    );
    let message = stderr(&output);
    assert!(!output.status.success(), "{message}");
    assert!(message.contains("redirect"), "{message}");
    assert!(!message.contains("abc466"), "{message}");
    assert!(
        message.contains("ABC467") && message.contains("symbolic link or reparse point"),
        "{message}"
    );
    summary_count(&message, "Failed:", 1);
    summary_count(&message, "Discovery errors:", 1);
    assert!(!message.contains("Fetching"), "{message}");
    assert_eq!(tree(outside.path()), before);
}

#[test]
fn batch_refresh_is_read_only_in_dry_run_and_repairs_only_the_selected_contest() {
    let root = workspace("version = 1\n");
    manifest(&root.path().join("abc464"), "abc464", 0);
    manifest(&root.path().join("abc466"), "abc466", 1);
    fs::write(
        root.path().join("abc464/.atc/contest.toml"),
        "version = 42\ncontest_id = 'abc464'\n",
    )
    .unwrap();
    manifest(&root.path().join("abc467"), "abc467", 0);
    let before = tree(root.path());
    let absent_fixtures = root.path().join("absent");
    let dry = cli(
        root.path(),
        &["refresh", "--all", "--dry-run", "--yes"],
        &absent_fixtures,
    );
    let message = stderr(&dry);
    assert!(dry.status.success(), "{message}");
    assert!(
        message.contains("RepairRequired  REFRESH PLANNED"),
        "{message}"
    );
    summary_count(&message, "Healthy skipped:", 1);
    summary_count(&message, "Refresh planned:", 1);
    summary_count(&message, "Migration required:", 1);
    assert_eq!(tree(root.path()), before);

    let fixture_root = tempfile::tempdir().unwrap();
    fixtures(fixture_root.path(), &["abc466", "abc467"]);
    let no_approval = cli(root.path(), &["refresh", "--all"], fixture_root.path());
    assert!(!no_approval.status.success());
    assert!(stderr(&no_approval).contains("require --yes"));
    assert_eq!(tree(root.path()), before);

    let run = cli(
        root.path(),
        &["refresh", "--all", "--yes"],
        fixture_root.path(),
    );
    let message = stderr(&run);
    assert!(run.status.success(), "{message}");
    summary_count(&message, "Healthy skipped:", 1);
    summary_count(&message, "Repaired:", 1);
    summary_count(&message, "Migration required:", 1);
    summary_count(&message, "Failed:", 0);
    assert!(root.path().join("abc466/tests/A/sample-1.in").is_file());
    assert!(!root.path().join("abc467/.atc-update.lock").exists());

    let after = tree(root.path());
    let rerun = cli(root.path(), &["refresh", "--all"], &absent_fixtures);
    let message = stderr(&rerun);
    assert!(rerun.status.success(), "{message}");
    summary_count(&message, "Healthy skipped:", 2);
    summary_count(&message, "Migration required:", 1);
    assert_eq!(tree(root.path()), after);

    let forced = cli(
        root.path(),
        &["refresh", "--all", "--force", "--yes"],
        fixture_root.path(),
    );
    let message = stderr(&forced);
    assert!(forced.status.success(), "{message}");
    summary_count(&message, "Refreshed:", 2);
    summary_count(&message, "Healthy skipped:", 0);
    assert!(root.path().join("abc467/tests/A/sample-1.in").is_file());
}

#[test]
fn batch_migration_continues_after_fetch_failure_and_preserves_legacy_tests() {
    let root = workspace("version = 1\n");
    for id in ["abc460", "abc466", "abc467"] {
        legacy(&root.path().join(id));
    }
    manifest(&root.path().join("abc461"), "abc999", 0);
    manifest(&root.path().join("abc462"), "abc462", 1);
    manifest(&root.path().join("abc463"), "abc463", 0);
    let recovery = root.path().join("abc464/.atc-migrate-interrupted");
    fs::create_dir_all(recovery.join("previous-tests")).unwrap();
    fs::write(recovery.join("previous-tests/old"), b"keep recovery").unwrap();
    let before = tree(root.path());
    let fixture_root = tempfile::tempdir().unwrap();
    fixtures(fixture_root.path(), &["abc466", "abc467"]);

    let unapproved = cli(root.path(), &["migrate", "--all"], fixture_root.path());
    assert!(!unapproved.status.success());
    assert!(stderr(&unapproved).contains("require --yes"));
    assert_eq!(tree(root.path()), before);

    let run = cli(
        root.path(),
        &["migrate", "--all", "--yes"],
        fixture_root.path(),
    );
    let message = stderr(&run);
    assert!(!run.status.success(), "{message}");
    summary_count(&message, "Scanned:", 7);
    summary_count(&message, "Healthy skipped:", 1);
    summary_count(&message, "Failed:", 3);
    summary_count(&message, "Refresh required:", 1);
    summary_count(&message, "Migrated:", 2);
    let failures = message.split_once("Failures:\n").unwrap().1;
    assert!(failures.contains("failed to read fixture"), "{message}");
    assert!(failures.contains("contest ID mismatch"), "{message}");
    assert!(failures.contains("requires recovery check"), "{message}");
    for id in ["abc460", "abc461", "abc462", "abc463", "abc464"] {
        assert!(!root.path().join(id).join(".atc-update.lock").exists());
    }
    assert_eq!(
        fs::read(recovery.join("previous-tests/old")).unwrap(),
        b"keep recovery"
    );
    for id in ["abc466", "abc467"] {
        let destination = root.path().join(id);
        let backup = fs::read_dir(&destination)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".atc-recovery-migrate-")
            })
            .unwrap()
            .path();
        assert_eq!(
            fs::read(backup.join("previous-tests/legacy/custom.in")).unwrap(),
            b"old test"
        );
        assert_eq!(fs::read(destination.join("A.cpp")).unwrap(), b"source");
        assert!(destination.join("tests/A/sample-1.in").is_file());
        assert!(!destination.join("tests/legacy").exists());
    }

    let after = tree(root.path());
    let rerun = cli(
        root.path(),
        &["migrate", "--all", "--yes"],
        &root.path().join("absent"),
    );
    let message = stderr(&rerun);
    assert!(!rerun.status.success(), "{message}");
    summary_count(&message, "Healthy skipped:", 3);
    assert!(!message.contains("abc466  MIGRATING"), "{message}");
    assert!(!message.contains("abc467  MIGRATING"), "{message}");
    assert_eq!(tree(root.path()), after);
}

#[test]
fn all_conflicts_with_contest_and_refresh_dry_run_and_yes_require_all() {
    let root = workspace("version = 1\n");
    for command in ["refresh", "migrate"] {
        let output = cli(
            root.path(),
            &[command, "-c", "abc466", "--all"],
            root.path(),
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("cannot be used with"));
    }
    for flag in ["--dry-run", "--yes"] {
        let output = cli(root.path(), &["refresh", flag], root.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--all"));
    }
}

#[cfg(unix)]
#[test]
fn batch_interrupt_during_fetch_stops_before_the_next_contest_without_writing() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let root = workspace("version = 1\n");
    manifest(&root.path().join("abc466"), "abc466", 1);
    manifest(&root.path().join("abc467"), "abc467", 1);
    let before = tree(root.path());
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_atc"))
        .current_dir(root.path())
        .args(["refresh", "--all", "--yes"])
        .env_remove("ATC_FIXTURE_DIR")
        .env_remove("https_proxy")
        .env_remove("no_proxy")
        .env(
            "HTTPS_PROXY",
            format!("http://{}", proxy.local_addr().unwrap()),
        )
        .env("NO_PROXY", "")
        .env("XDG_STATE_HOME", root.path().join("state"))
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match proxy.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("batch did not connect to the loopback proxy: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = String::new();
    BufReader::new(&stream).read_line(&mut request).unwrap();
    assert_eq!(request, "CONNECT atcoder.jp:443 HTTP/1.1\r\n");
    // SAFETY: the PID belongs to the child that is waiting for this proxy's response.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    stream
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    drop(stream);

    let output = child.wait_with_output().unwrap();
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(130), "{message}");
    assert!(!message.contains("abc467  REFRESHING"), "{message}");
    summary_count(&message, "Not run:", 2);
    assert_eq!(tree(root.path()), before);
}
