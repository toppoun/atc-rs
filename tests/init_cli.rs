use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const DEFAULT_WORKSPACE_CONFIG: &str = concat!(
    "# atc workspace configuration\n",
    "#\n",
    "# 各 contest ID を以下の pattern と照合し、保存先を振り分けます。\n",
    "#\n",
    "# 例:\n",
    "#   abc123 -> ABC/abc123\n",
    "#\n",
    "# 1つの pattern に一致した場合、その `path` 配下に contest を配置します。\n",
    "# どの pattern にも一致しない場合は、workspace 直下に配置します。\n",
    "# 複数の pattern に一致した場合はエラーになります。\n",
    "#\n",
    "# 不要な振り分けは、対応する [[paths]] を削除またはコメントアウトしてください。\n",
    "\n",
    "version = 1\n",
    "\n",
    "[[paths]]\n",
    "pattern = \"^abc[0-9]+$\"\n",
    "path = \"ABC\"\n",
    "\n",
    "[[paths]]\n",
    "pattern = \"^arc[0-9]+$\"\n",
    "path = \"ARC\"\n",
    "\n",
    "[[paths]]\n",
    "pattern = \"^agc[0-9]+$\"\n",
    "path = \"AGC\"\n",
);

fn run_init(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atc"))
        .arg("init")
        .current_dir(root)
        .output()
        .expect("atc init should run")
}

#[test]
fn init_creates_the_exact_marker_and_rerun_preserves_its_bytes() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join(".atc-workspace.toml");

    let created = run_init(root.path());

    assert_eq!(created.status.code(), Some(0));
    assert_eq!(created.stdout, b"");
    assert_eq!(
        String::from_utf8(created.stderr).unwrap(),
        format!("Initialized atc workspace: {}\n", marker.display())
    );
    assert_eq!(
        fs::read(&marker).unwrap(),
        DEFAULT_WORKSPACE_CONFIG.as_bytes()
    );

    let before_rerun = fs::read(&marker).unwrap();
    let rerun = run_init(root.path());

    assert_eq!(rerun.status.code(), Some(0));
    assert_eq!(rerun.stdout, b"");
    assert_eq!(
        String::from_utf8(rerun.stderr).unwrap(),
        format!("Workspace already initialized: {}\n", marker.display())
    );
    assert_eq!(fs::read(&marker).unwrap(), before_rerun);
}

#[test]
fn init_rejects_an_invalid_marker_without_changing_it() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join(".atc-workspace.toml");
    let invalid = b"invalid marker\n";
    fs::write(&marker, invalid).unwrap();

    let output = run_init(root.path());

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("error: filesystem operation failed"));
    assert!(stderr.contains("workspace config"));
    assert!(stderr.contains(marker.to_string_lossy().as_ref()));
    assert_eq!(fs::read(&marker).unwrap(), invalid);
}

#[test]
fn init_preserves_existing_files_in_a_non_empty_root() {
    let root = tempfile::tempdir().unwrap();
    let existing = root.path().join("keep.txt");
    let existing_bytes = b"keep this file\n";
    fs::write(&existing, existing_bytes).unwrap();

    let output = run_init(root.path());

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"");
    assert_eq!(fs::read(&existing).unwrap(), existing_bytes);
    assert_eq!(
        fs::read(root.path().join(".atc-workspace.toml")).unwrap(),
        DEFAULT_WORKSPACE_CONFIG.as_bytes()
    );
}
