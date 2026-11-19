use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn refresh(cwd: &Path, args: &[&str], fixtures: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atc"))
        .arg("refresh")
        .args(args)
        .current_dir(cwd)
        .env("ATC_FIXTURE_DIR", fixtures)
        .output()
        .unwrap()
}

#[test]
fn refresh_resolves_workspace_routing_and_skips_the_repaired_contest() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("beginner/abc466");
    fs::create_dir_all(destination.join(".atc")).unwrap();
    fs::write(
        temp.path().join(".atc-workspace.toml"),
        "version = 1\n[[paths]]\npattern = '^abc'\npath = 'beginner'\n",
    )
    .unwrap();
    let decoy = temp.path().join("abc466");
    fs::create_dir_all(decoy.join(".atc")).unwrap();

    let output = refresh(temp.path(), &["-c", "abc466"], &fixtures());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("Refreshed"), "{stderr}");
    assert!(destination.join(".atc/contest.toml").is_file());
    assert!(destination.join("tests/A/sample-1.in").is_file());
    assert!(!decoy.join(".atc/contest.toml").exists());

    let missing_fixtures = temp.path().join("missing-fixtures");
    for (cwd, args) in [
        (destination.as_path(), &[][..]),
        (temp.path(), &["-c", "abc466"][..]),
    ] {
        let output = refresh(cwd, args, &missing_fixtures);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert_eq!(stderr.trim(), "abc466: local data healthy; refresh skipped");
    }
}

#[test]
fn refresh_healthy_sends_no_requests_and_force_transport_failure_keeps_data() {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("abc466");
    fs::create_dir_all(destination.join(".atc")).unwrap();
    let metadata = destination.join(".atc/contest.toml");
    let original = concat!(
        "version = 1\ncontest_id = 'abc466'\n",
        "[[problems]]\nindex = 'A'\ntitle = 'A'\n",
        "task_id = 'abc466_a'\n",
        "url = 'https://atcoder.jp/contests/abc466/tasks/abc466_a'\n",
        "sample_count = 0\n",
    );
    fs::write(&metadata, original).unwrap();
    fs::write(destination.join("A.cpp"), "user source").unwrap();

    // The loopback proxy counts production transport attempts without contacting AtCoder.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let (finished, stop) = mpsc::channel();
    let proxy = std::thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request_line = String::new();
                    BufReader::new(&stream)
                        .read_line(&mut request_line)
                        .unwrap();
                    requests.push(request_line);
                    stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if !matches!(
                        stop.recv_timeout(Duration::from_millis(10)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        return requests;
                    }
                }
                Err(error) => panic!("loopback proxy failed: {error}"),
            }
        }
    });
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_atc"))
            .arg("refresh")
            .args(args)
            .current_dir(&destination)
            .env_remove("ATC_FIXTURE_DIR")
            .env_remove("https_proxy")
            .env_remove("no_proxy")
            .env("HTTPS_PROXY", &proxy_url)
            .env("NO_PROXY", "")
            .env("XDG_STATE_HOME", temp.path().join("state"))
            .env("APPDATA", temp.path().join("state"))
            .output()
            .unwrap()
    };

    let healthy = run(&[]);
    let forced = run(&["--force"]);
    finished.send(()).unwrap();
    let requests = proxy.join().unwrap();

    assert!(
        healthy.status.success(),
        "{}",
        String::from_utf8_lossy(&healthy.stderr)
    );
    assert!(String::from_utf8_lossy(&healthy.stderr).contains("refresh skipped"));
    assert!(!forced.status.success());
    let stderr = String::from_utf8_lossy(&forced.stderr);
    assert!(stderr.contains("AtCoder operation failed"), "{stderr}");
    assert_eq!(requests, ["CONNECT atcoder.jp:443 HTTP/1.1\r\n"]);
    assert_eq!(fs::read_to_string(metadata).unwrap(), original);
    assert_eq!(fs::read(destination.join("A.cpp")).unwrap(), b"user source");
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 2);
}

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
    let output = refresh(&destination, &["--force"], &fixtures());

    assert!(
        !output.status.success(),
        "force refresh replaced a different contest: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("contest ID mismatch"));
    assert_eq!(fs::read(&metadata_path).unwrap(), original.as_bytes());
    assert!(!destination.join("tests").exists());
}
