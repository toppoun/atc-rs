use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const INITIAL_CONFIG: &[u8] = b"# atc configuration\n\
#\n\
# Add only the settings you want to override.\n\
# See the configuration documentation for available settings.\n\
#\n\
# [editor]\n\
# command = \"nvim\"\n\
# args = []\n\
# mode = \"terminal\"\n";

fn isolated_config_file(base: &Path) -> PathBuf {
    base.join("atc").join("config.toml")
}

fn run_config_init(base: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atc"))
        .args(["config", "init"])
        .env("APPDATA", base)
        .env("XDG_CONFIG_HOME", base)
        .output()
        .unwrap()
}

#[test]
fn config_init_dispatches_to_the_isolated_global_path_and_reports_created_then_exists() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("config-base");
    let config_file = isolated_config_file(&base);

    let created = run_config_init(&base);

    assert!(
        created.status.success(),
        "config init failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert_eq!(
        String::from_utf8(created.stdout).unwrap(),
        format!("Created {}\n", config_file.display())
    );
    assert_eq!(fs::read(&config_file).unwrap(), INITIAL_CONFIG);
    assert!(!base.join("atc").join("templates").exists());

    let custom = b"runner.timeout_seconds = 3.0\n# preserved\n";
    fs::write(&config_file, custom).unwrap();
    let exists = run_config_init(&base);

    assert!(
        exists.status.success(),
        "config init rerun failed: {}",
        String::from_utf8_lossy(&exists.stderr)
    );
    assert_eq!(
        String::from_utf8(exists.stdout).unwrap(),
        format!("Exists {}\n", config_file.display())
    );
    assert_eq!(fs::read(&config_file).unwrap(), custom);
    assert!(!base.join("atc").join("templates").exists());
}
