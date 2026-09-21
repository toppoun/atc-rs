use super::*;
use std::os::unix::fs::MetadataExt;
use std::process::Command;

const XATTR: &str = "com.atc-rs.settings-test";

fn command(program: &str, args: &[&str], path: &Path) -> Vec<u8> {
    let output = Command::new(program).args(args).arg(path).output().unwrap();
    assert!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn acl(path: &Path) -> Vec<String> {
    String::from_utf8(command("/bin/ls", &["-lde"], path))
        .unwrap()
        .lines()
        .skip(1)
        .map(str::to_owned)
        .collect()
}

fn xattr(path: &Path) -> Vec<u8> {
    command("/usr/bin/xattr", &["-p", "-x", XATTR], path)
}

fn metadata_change_after_copy_is_a_conflict(change_acl: bool) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = "[runner]\npython = \"python3\"\n";
    fs::write(&path, original).unwrap();
    command("/usr/bin/xattr", &["-w", XATTR, "before"], &path);
    let mut store = SettingsStore::load(&path).unwrap();
    set_python(&mut store, "draft-python");
    let draft = store.document().candidate();
    let Baseline::Existing(baseline) = store.baseline.clone() else {
        panic!("expected an existing baseline");
    };
    let before = fs::metadata(&path).unwrap();
    let initial_acl = acl(&path);
    let initial_xattr = xattr(&path);
    let mut external_acl = Vec::new();
    let mut external_xattr = Vec::new();
    let result = store.save_existing_with(
        draft.clone().into_bytes(),
        baseline,
        |staging| {
            // This hook runs after the production COPYFILE_METADATA call.
            let staged = staging_entries(temp.path());
            assert_eq!(staged.len(), 1);
            assert_eq!(acl(&staged[0]), initial_acl);
            assert_eq!(xattr(&staged[0]), initial_xattr);
            if change_acl {
                command("/bin/chmod", &["+a", "everyone allow readattr"], &path);
            } else {
                // Same-length values ensure size is not a proxy for xattr content.
                command("/usr/bin/xattr", &["-w", XATTR, "after!"], &path);
            }
            external_acl = acl(&path);
            external_xattr = xattr(&path);
            let after = fs::metadata(&path).unwrap();
            assert_eq!(after.ino(), before.ino());
            assert_eq!(after.mode(), before.mode());
            assert_eq!(after.mtime(), before.mtime());
            assert_eq!(after.mtime_nsec(), before.mtime_nsec());
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
            if change_acl {
                assert_ne!(external_acl, initial_acl);
            } else {
                assert_ne!(external_xattr, initial_xattr);
            }
            staging.sync_all()
        },
        safe_file::replace_file,
        sync_parent,
    );

    // Collect all preservation checks before asserting the expected error, so
    // a failing regression also establishes whether replacement lost metadata.
    let contents_preserved = fs::read_to_string(&path).unwrap() == original;
    let identity_preserved = fs::metadata(&path).unwrap().ino() == before.ino();
    let metadata_preserved = acl(&path) == external_acl && xattr(&path) == external_xattr;
    let staging_cleaned = staging_entries(temp.path()).is_empty();
    assert!(
        matches!(result, Err(SettingsSaveError::Conflict(_))),
        "expected Conflict, got {result:?}; contents preserved: {contents_preserved}, \
         identity preserved: {identity_preserved}, metadata preserved: {metadata_preserved}, \
         staging cleaned: {staging_cleaned}"
    );
    assert!(contents_preserved && identity_preserved && metadata_preserved && staging_cleaned);
    assert_eq!(store.document().candidate(), draft);
}

#[test]
fn acl_change_after_metadata_copy_is_a_conflict() {
    metadata_change_after_copy_is_a_conflict(true);
}

#[test]
fn xattr_change_after_metadata_copy_is_a_conflict() {
    metadata_change_after_copy_is_a_conflict(false);
}

fn observed_metadata(path: &Path) -> (u32, Vec<String>, Vec<u8>) {
    (
        fs::metadata(path).unwrap().mode(),
        acl(path),
        command("/usr/bin/xattr", &["-l", "-x"], path),
    )
}

fn seed_metadata(path: &Path) {
    command("/bin/chmod", &["640"], path);
    command("/bin/chmod", &["+a", "everyone allow readattr"], path);
    command("/usr/bin/xattr", &["-w", XATTR, "before"], path);
}

const MUTATIONS: &[(&str, &[&str])] = &[
    ("/bin/chmod", &["600"]),
    ("/bin/chmod", &["+a", "everyone deny execute"]),
    ("/bin/chmod", &["=a#", "0", "everyone allow read,readattr"]),
    ("/bin/chmod", &["-N"]),
    ("/usr/bin/xattr", &["-w", "com.atc-rs.another", "new"]),
    ("/usr/bin/xattr", &["-w", XATTR, "after!"]),
    ("/usr/bin/xattr", &["-d", XATTR]),
];

fn metadata_mutations_are_conflicts(after_copy: bool, no_op: bool) {
    for (program, args) in MUTATIONS {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let original = "[runner]\npython = \"python3\"\n";
        fs::write(&path, original).unwrap();
        seed_metadata(&path);
        let mut store = SettingsStore::load(&path).unwrap();
        if !no_op {
            set_python(&mut store, "draft-python");
        }
        let draft = store.document().candidate();
        let before = observed_metadata(&path);
        let inode = fs::metadata(&path).unwrap().ino();
        let mut external = None;
        let mut mutate = || {
            command(program, args, &path);
            external = Some(observed_metadata(&path));
            assert_ne!(external.as_ref().unwrap(), &before);
        };
        let result = if after_copy {
            let Baseline::Existing(baseline) = store.baseline.clone() else {
                panic!("expected an existing baseline");
            };
            store.save_existing_with(
                draft.clone().into_bytes(),
                baseline,
                |staging| {
                    mutate();
                    staging.sync_all()
                },
                |_, _| panic!("conflicting metadata must never reach replacement"),
                sync_parent,
            )
        } else {
            mutate();
            store.save()
        };
        assert!(
            matches!(result, Err(SettingsSaveError::Conflict(_))),
            "{program} {args:?}: {result:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(observed_metadata(&path), external.unwrap());
        assert_eq!(store.document().candidate(), draft);
        assert!(staging_entries(temp.path()).is_empty());
    }
}

#[test]
fn mode_acl_and_xattr_mutations_before_save_are_conflicts() {
    metadata_mutations_are_conflicts(false, false);
}

#[test]
fn mode_acl_and_xattr_mutations_after_copy_are_conflicts() {
    metadata_mutations_are_conflicts(true, false);
}

#[test]
fn no_op_save_detects_metadata_only_external_modifications() {
    metadata_mutations_are_conflicts(false, true);
}

#[test]
fn atomic_replacement_preserves_mode_acl_and_binary_empty_and_resource_fork_xattrs() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = "[runner]\npython = \"python3\"\n";
    fs::write(&path, original).unwrap();
    seed_metadata(&path);
    command("/bin/chmod", &["+a", "everyone deny execute"], &path);
    command("/usr/bin/xattr", &["-w", "-x", XATTR, "00fffe80"], &path);
    command("/usr/bin/xattr", &["-w", "com.atc-rs.empty", ""], &path);
    command(
        "/usr/bin/xattr",
        &["-w", "com.apple.ResourceFork", "resource"],
        &path,
    );
    let expected = observed_metadata(&path);
    let mut original_handle = File::open(&path).unwrap();
    let inode = original_handle.metadata().unwrap().ino();
    let mut store = SettingsStore::load(&path).unwrap();
    set_python(&mut store, "saved-python");
    let candidate = store.document().candidate();

    let result = store.save_existing_with_test_hooks(
        |staged, destination| {
            assert_eq!(fs::read_to_string(destination).unwrap(), original);
            assert_eq!(observed_metadata(staged), expected);
            safe_file::replace_file(staged, destination)?;
            // An already-open reader retains the complete original inode;
            // opening the path after rename sees the complete candidate.
            let mut old_contents = String::new();
            original_handle.read_to_string(&mut old_contents)?;
            assert_eq!(old_contents, original);
            assert_eq!(original_handle.metadata()?.ino(), inode);
            assert_ne!(fs::metadata(destination)?.ino(), inode);
            assert_eq!(fs::read_to_string(destination)?, candidate);
            Ok(())
        },
        sync_parent,
    );

    assert_eq!(result.unwrap(), SaveOutcome::Saved);
    assert_eq!(observed_metadata(&path), expected);
    assert!(staging_entries(temp.path()).is_empty());
    assert_eq!(store.save().unwrap(), SaveOutcome::Unchanged);
    set_python(&mut store, "saved-again");
    assert_eq!(store.save().unwrap(), SaveOutcome::Saved);
    assert_eq!(observed_metadata(&path), expected);
}

#[test]
fn staged_metadata_mismatch_is_rejected_before_replacement() {
    for (program, args) in MUTATIONS {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let original = "[runner]\npython = \"python3\"\n";
        fs::write(&path, original).unwrap();
        seed_metadata(&path);
        let before = observed_metadata(&path);
        let inode = fs::metadata(&path).unwrap().ino();
        let mut store = SettingsStore::load(&path).unwrap();
        set_python(&mut store, "draft-python");
        let draft = store.document().candidate();
        let Baseline::Existing(baseline) = store.baseline.clone() else {
            panic!("expected an existing baseline");
        };
        let result = store.save_existing_with(
            draft.clone().into_bytes(),
            baseline,
            |staging| {
                let staged = staging_entries(temp.path());
                assert_eq!(staged.len(), 1);
                command(program, args, &staged[0]);
                staging.sync_all()
            },
            |_, _| panic!("unverified staging metadata must never reach replacement"),
            sync_parent,
        );
        assert!(matches!(result, Err(SettingsSaveError::BeforeCommit(_))));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(observed_metadata(&path), before);
        assert_eq!(store.document().candidate(), draft);
        assert!(staging_entries(temp.path()).is_empty());
    }
}

fn unreadable_metadata_after_copy_refuses_write(denied: &str) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = "[runner]\npython = \"python3\"\n";
    fs::write(&path, original).unwrap();
    seed_metadata(&path);
    let original_xattr = xattr(&path);
    let original_acl = acl(&path);
    let mut store = SettingsStore::load(&path).unwrap();
    set_python(&mut store, "draft-python");
    let draft = store.document().candidate();
    let inode = fs::metadata(&path).unwrap().ino();
    let Baseline::Existing(baseline) = store.baseline.clone() else {
        panic!("expected an existing baseline");
    };
    let mut external_acl = Vec::new();
    let result = store.save_existing_with(
        draft.clone().into_bytes(),
        baseline,
        |staging| {
            command("/bin/chmod", &["+a#", "0", denied], &path);
            if denied != "everyone deny readsecurity" {
                external_acl = acl(&path);
            }
            staging.sync_all()
        },
        |_, _| panic!("unreadable metadata must never reach replacement"),
        sync_parent,
    );
    assert!(
        matches!(&result, Err(SettingsSaveError::BeforeCommit(error))
            if error.kind() == io::ErrorKind::PermissionDenied),
        "{result:?}"
    );
    assert!(matches!(
        SettingsStore::load(&path),
        Err(SettingsStoreLoadError::Io { .. })
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    if denied == "everyone deny readsecurity" {
        let output = Command::new("/bin/ls")
            .arg("-lde")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "the external ACL must still deny inspection"
        );
    } else {
        assert_eq!(acl(&path), external_acl);
    }
    assert_eq!(store.document().candidate(), draft);
    assert!(staging_entries(temp.path()).is_empty());
    command("/bin/chmod", &["-N"], &path);
    command("/bin/chmod", &["+a", "everyone allow readattr"], &path);
    // Darwin may deny stat while readsecurity is denied. Check identity after
    // restoring inspection rights on the same fixture.
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(acl(&path), original_acl);
    assert_eq!(xattr(&path), original_xattr);
}

#[test]
fn unreadable_xattrs_after_copy_refuse_write_and_clean_staging() {
    unreadable_metadata_after_copy_refuses_write("everyone deny readextattr");
}

#[test]
fn unreadable_acl_after_copy_refuses_write_and_cleans_staging() {
    unreadable_metadata_after_copy_refuses_write("everyone deny readsecurity");
}

#[test]
fn metadata_change_within_snapshot_is_rejected() {
    for (program, args) in MUTATIONS {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[runner]\npython = \"python3\"\n").unwrap();
        seed_metadata(&path);
        let file = File::open(&path).unwrap();
        let before = file.metadata().unwrap();
        command(program, args, &path);
        let error = crate::settings_store::macos::metadata_contract(&before, &file).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
}

#[test]
fn oversized_resource_fork_after_copy_refuses_write_and_cleans_staging() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    let original = "[runner]\npython = \"python3\"\n";
    fs::write(&path, original).unwrap();
    let mut store = SettingsStore::load(&path).unwrap();
    set_python(&mut store, "draft-python");
    let draft = store.document().candidate();
    let inode = fs::metadata(&path).unwrap().ino();
    let Baseline::Existing(baseline) = store.baseline.clone() else {
        panic!("expected an existing baseline");
    };
    let fork_path = path.join("..namedfork/rsrc");
    let length = 17 * 1024 * 1024;
    let result = store.save_existing_with(
        draft.clone().into_bytes(),
        baseline,
        |staging| {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&fork_path)?
                .set_len(length)?;
            staging.sync_all()
        },
        |_, _| panic!("oversized metadata must never reach replacement"),
        sync_parent,
    );
    assert!(
        matches!(&result, Err(SettingsSaveError::BeforeCommit(error))
        if error.kind() == io::ErrorKind::Unsupported),
        "{result:?}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(fs::metadata(&fork_path).unwrap().len(), length);
    assert_eq!(store.document().candidate(), draft);
    assert!(staging_entries(temp.path()).is_empty());
}
