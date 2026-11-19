#[cfg(any(unix, windows))]
use std::path::Path;

#[cfg(unix)]
pub fn link_directory(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
pub fn link_directory(link: &Path, target: &Path) {
    use std::fs;
    use std::os::windows::{fs::MetadataExt as _, process::CommandExt as _};
    use std::path::PathBuf;
    use std::process::Command;

    let native = |path: &Path| {
        std::path::absolute(path)
            .unwrap()
            .components()
            .collect::<PathBuf>()
    };
    let link = native(link);
    let target = native(target);
    let output = Command::new("cmd.exe")
        .args(["/D", "/V:OFF", "/C"])
        .raw_arg(r#"mklink /J "%ATC_TEST_JUNCTION%" "%ATC_TEST_JUNCTION_TARGET%""#)
        .env("ATC_TEST_JUNCTION", &link)
        .env("ATC_TEST_JUNCTION_TARGET", &target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to create junction {}: {}{}",
        link.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_ne!(
        fs::symlink_metadata(&link).unwrap().file_attributes() & 0x0400,
        0
    );
    assert_eq!(
        fs::canonicalize(&link).unwrap(),
        fs::canonicalize(&target).unwrap()
    );
}
