use super::*;
use crate::workspace_registry::{EntryRef, VerifiedContestOpen, VerifiedWorkspace};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Instant;

struct Fixture {
    temp: tempfile::TempDir,
    store: RegistryStore,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("state-base");
        let location = RegistryLocation::from_platform_base(base).unwrap();
        let state = location.file().parent().unwrap().to_path_buf();
        let store = RegistryStore::new(location);
        Self { temp, store, state }
    }

    fn workspace(&self, name: &str) -> VerifiedWorkspace {
        VerifiedWorkspace::from_verified_root(self.temp.path().join(name), None).unwrap()
    }

    fn registry(&self) -> PathBuf {
        self.store.location.file()
    }

    fn read(&self) -> RegistrySnapshot {
        let RegistryLoad::Snapshot(snapshot) = self.store.load().unwrap() else {
            panic!("expected a Registry on disk")
        };
        snapshot
    }

    fn register(&self, workspace: &VerifiedWorkspace) -> EntryRef {
        let result = self
            .store
            .apply(RegistryMutation::Register(workspace))
            .unwrap();
        let RegistryApply::Applied {
            result: RegistryMutationResult::Registered(entry),
            ..
        } = result
        else {
            panic!("expected registration: {result:?}")
        };
        entry
    }

    fn seed(&self, bytes: &[u8]) {
        fs::create_dir_all(&self.state).unwrap();
        fs::write(self.registry(), bytes).unwrap();
    }

    fn identity(&self) -> DirectoryIdentity {
        DirectoryIdentity::of_handle(&File::open(self.registry()).unwrap()).unwrap()
    }

    fn check_no_staging(&self) {
        assert!(!fs::read_dir(&self.state).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".workspaces-staging-")
        }));
    }
}

#[test]
fn missing_read_is_side_effect_free_and_first_save_round_trips() {
    let fixture = Fixture::new();
    assert!(matches!(
        fixture.store.load().unwrap(),
        RegistryLoad::Missing
    ));
    assert!(!fixture.store.location.platform_base().exists());
    let workspace = fixture.workspace("競技");
    let entry = fixture.register(&workspace);
    let opened = fixture
        .store
        .apply(RegistryMutation::RecordWorkspaceOpen {
            entry: &entry,
            opened: &workspace,
        })
        .unwrap();
    let RegistryApply::Applied { snapshot, .. } = opened else {
        panic!("expected a recorded open")
    };
    assert_eq!(fixture.read(), snapshot);
    assert_eq!(
        snapshot.entries_by_recent_use()[0].root().as_os_str(),
        fixture.temp.path().join("競技").as_os_str()
    );
    fixture.check_no_staging();
}

#[test]
fn idempotent_register_does_not_replace_and_stale_references_do_not_revive() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("workspace");
    let old = fixture.register(&workspace);
    let id = fixture.identity();
    let before = fs::read(fixture.registry()).unwrap();
    assert!(
        matches!(fixture.store.apply(RegistryMutation::Register(&workspace)).unwrap(), RegistryApply::NoChange { result: RegistryMutationResult::AlreadyRegistered(ref entry), .. } if *entry == old)
    );
    assert_eq!(fixture.identity(), id);
    assert_eq!(fs::read(fixture.registry()).unwrap(), before);
    assert!(matches!(
        fixture.store.apply(RegistryMutation::Remove(&old)).unwrap(),
        RegistryApply::Applied { .. }
    ));
    assert!(fixture.read().entries_by_recent_use().is_empty());
    let new = fixture.register(&workspace);
    let current = fixture.identity();
    assert!(new.entry_id() > old.entry_id());
    let contest =
        VerifiedContestOpen::from_successful_open(&workspace, "abc123", "ABC/abc123").unwrap();
    for mutation in [
        RegistryMutation::RecordWorkspaceOpen {
            entry: &old,
            opened: &workspace,
        },
        RegistryMutation::RecordContestOpen {
            entry: &old,
            opened: &contest,
        },
        RegistryMutation::Remove(&old),
    ] {
        assert!(
            matches!(fixture.store.apply(mutation).unwrap(), RegistryApply::StaleEntry { entry_id } if entry_id == old.entry_id())
        );
    }
    assert_eq!(fixture.identity(), current);
    assert!(
        fixture.read().entries_by_recent_use()[0]
            .recent_contests_by_use()
            .is_empty()
    );
}

#[test]
fn invalid_or_future_registry_is_never_replaced() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("workspace");
    for (bytes, expected) in [
        (b"version = [\nprivate text".as_slice(), "toml"),
        (b"version = 1\nnext_seq = 0\n", "schema"),
        (b"version = 2\nnew_field = true\n", "version"),
        (b"version = 1\nnext_seq = 'private text'\n", "fields"),
        (b"\xff", "utf8"),
    ] {
        fixture.seed(bytes);
        let id = fixture.identity();
        let error = fixture.store.load().unwrap_err();
        let actual = match error {
            RegistryReadError::InvalidToml => "toml",
            RegistryReadError::InvalidFields => "fields",
            RegistryReadError::InvalidSchema(_) => "schema",
            RegistryReadError::UnsupportedVersion(2) => "version",
            RegistryReadError::InvalidUtf8 => "utf8",
            error => panic!("unexpected read error: {error:?}"),
        };
        assert_eq!(actual, expected);
        assert!(!format!("{error:?}").contains("private text"));
        assert!(matches!(
            fixture.store.apply(RegistryMutation::Register(&workspace)),
            Err(RegistryWriteError::Read(_))
        ));
        assert_eq!(fixture.identity(), id);
        assert_eq!(fs::read(fixture.registry()).unwrap(), bytes);
        fixture.check_no_staging();
    }
}

#[test]
fn wrong_registry_and_lock_file_types_stop_operations() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.registry()).unwrap();
    let workspace = fixture.workspace("workspace");
    assert!(matches!(
        fixture.store.load(),
        Err(RegistryReadError::UnsafeType(_))
    ));
    assert!(matches!(
        fixture.store.apply(RegistryMutation::Register(&workspace)),
        Err(RegistryWriteError::Read(RegistryReadError::UnsafeType(_)))
    ));
    assert!(fixture.registry().is_dir());
    fs::remove_dir(fixture.registry()).unwrap();
    let lock = fixture.state.join(".workspaces.lock");
    fs::remove_file(&lock).unwrap();
    fs::create_dir(&lock).unwrap();
    assert!(matches!(
        fixture.store.apply(RegistryMutation::Register(&workspace)),
        Err(RegistryWriteError::Lock(RegistryLockError::Open(
            RegistryReadError::UnsafeType(_)
        )))
    ));
    assert!(lock.is_dir());
    assert!(!fixture.registry().exists());
}

#[test]
fn lock_with_a_second_name_is_rejected() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.state).unwrap();
    let external = fixture.temp.path().join("other-file");
    fs::write(&external, b"untouched").unwrap();
    fs::hard_link(&external, fixture.state.join(".workspaces.lock")).unwrap();
    assert!(matches!(
        fixture
            .store
            .apply(RegistryMutation::Register(&fixture.workspace("workspace"))),
        Err(RegistryWriteError::Lock(RegistryLockError::Open(
            RegistryReadError::UnsafeType(_)
        )))
    ));
    assert_eq!(fs::read(external).unwrap(), b"untouched");
    assert!(!fixture.registry().exists());
}

#[test]
fn registry_symlink_does_not_read_or_modify_target() {
    let fixture = Fixture::new();
    let target = fixture.temp.path().join("external.toml");
    let original = b"version = 1\nnext_seq = 1\n";
    fs::write(&target, original).unwrap();
    fs::create_dir_all(&fixture.state).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, fixture.registry()).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_file(&target, fixture.registry()) {
        if error.raw_os_error() == Some(1314) {
            eprintln!("Windows file symlinks require developer mode or the symlink privilege");
            return;
        }
        panic!("create test symlink: {error}");
    }
    assert!(matches!(
        fixture.store.load(),
        Err(RegistryReadError::UnsafeType(_))
    ));
    assert!(matches!(
        fixture
            .store
            .apply(RegistryMutation::Register(&fixture.workspace("workspace"))),
        Err(RegistryWriteError::Read(RegistryReadError::UnsafeType(_)))
    ));
    assert_eq!(fs::read(&target).unwrap(), original);
    assert!(
        fs::symlink_metadata(fixture.registry())
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn linked_owned_state_directory_is_rejected() {
    let fixture = Fixture::new();
    let base = fixture.store.location.platform_base();
    let external = fixture.temp.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::create_dir(base).unwrap();
    let link = base.join("atc");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&external, &link).unwrap();
    #[cfg(windows)]
    {
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&external)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(matches!(
        fixture.store.load(),
        Err(RegistryReadError::UnsafeType(_))
    ));
    assert!(matches!(
        fixture
            .store
            .apply(RegistryMutation::Register(&fixture.workspace("workspace"))),
        Err(RegistryWriteError::Read(RegistryReadError::UnsafeType(_)))
    ));
    assert!(fs::read_dir(external).unwrap().next().is_none());
}

#[test]
fn lock_contention_is_bounded_for_independent_handles_in_one_process() {
    let fixture = Fixture::new();
    let dir = StateDirectory::open(&fixture.store.location, true)
        .unwrap()
        .unwrap();
    let lock = dir.lock(Duration::ZERO).unwrap();
    let workspace = fixture.workspace("workspace");
    let result = fixture.store.apply_with(
        RegistryMutation::Register(&workspace),
        Duration::from_millis(30),
        |_| Ok(()),
    );
    assert!(
        matches!(
            result,
            Err(RegistryWriteError::Lock(RegistryLockError::TimedOut))
        ),
        "{result:?}"
    );
    assert!(!fixture.registry().exists());
    drop(lock);
    fixture.register(&workspace);
}

#[test]
fn same_bytes_new_file_and_changed_bytes_are_both_conflicts() {
    for replace_file in [false, true] {
        let fixture = Fixture::new();
        fixture.register(&fixture.workspace("existing"));
        let original = fs::read(fixture.registry()).unwrap();
        let id = fixture.identity();
        let replacement = fixture.state.join("external-edit.toml");
        let result = fixture.store.apply_with(
            RegistryMutation::Register(&fixture.workspace("new")),
            LOCK_TIMEOUT,
            |step| {
                if step == WriteStep::BeforeRecheck {
                    if replace_file {
                        fs::write(&replacement, &original)?;
                        fs::rename(&replacement, fixture.registry())?;
                    } else {
                        fs::write(fixture.registry(), b"version = 1\nnext_seq = 41\n")?;
                    }
                }
                Ok(())
            },
        );
        assert!(
            matches!(result, Err(RegistryWriteError::Conflict)),
            "{result:?}"
        );
        if replace_file {
            assert_ne!(fixture.identity(), id);
            assert_eq!(fs::read(fixture.registry()).unwrap(), original);
        } else {
            assert_eq!(fixture.identity(), id);
            assert_eq!(
                fs::read(fixture.registry()).unwrap(),
                b"version = 1\nnext_seq = 41\n"
            );
        }
        fixture.check_no_staging();
    }
}

#[test]
fn initial_noclobber_collision_rereads_and_applies_to_winner() {
    let fixture = Fixture::new();
    let existing = fixture.workspace("external-winner");
    let (winner, _) = RegistrySnapshot::default()
        .apply(RegistryMutation::Register(&existing))
        .unwrap();
    let winner = winner.to_toml().unwrap();
    let mut injected = false;
    let result = fixture
        .store
        .apply_with(
            RegistryMutation::Register(&fixture.workspace("new")),
            LOCK_TIMEOUT,
            |step| {
                if step == WriteStep::BeforePublish && !injected {
                    fs::write(fixture.registry(), &winner)?;
                    injected = true;
                }
                Ok(())
            },
        )
        .unwrap();
    assert!(matches!(result, RegistryApply::Applied { .. }));
    assert!(injected);
    let snapshot = fixture.read();
    let entries = snapshot.entries_by_recent_use();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].root().file_name().unwrap(), "external-winner");
    fixture.check_no_staging();
}

#[test]
fn initial_collision_with_invalid_winner_preserves_it() {
    let fixture = Fixture::new();
    let invalid = b"bad winner";
    let result = fixture.store.apply_with(
        RegistryMutation::Register(&fixture.workspace("new")),
        LOCK_TIMEOUT,
        |step| {
            if step == WriteStep::BeforePublish {
                fs::write(fixture.registry(), invalid)?;
            }
            Ok(())
        },
    );
    assert!(
        matches!(
            result,
            Err(RegistryWriteError::Read(RegistryReadError::InvalidToml))
        ),
        "{result:?}"
    );
    assert_eq!(fs::read(fixture.registry()).unwrap(), invalid);
    fixture.check_no_staging();
}

#[test]
fn staging_sync_failure_preserves_the_baseline_and_cleans_its_own_stage() {
    let fixture = Fixture::new();
    fixture.register(&fixture.workspace("existing"));
    let id = fixture.identity();
    let before = fs::read(fixture.registry()).unwrap();
    let result = fixture.store.apply_with(
        RegistryMutation::Register(&fixture.workspace("new")),
        LOCK_TIMEOUT,
        |step| {
            if step == WriteStep::SyncStaged {
                Err(io::Error::other("injected staging sync error"))
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(result, Err(RegistryWriteError::BeforePublish(_))));
    assert_eq!(fixture.identity(), id);
    assert_eq!(fs::read(fixture.registry()).unwrap(), before);
    fixture.check_no_staging();
}

#[test]
fn directory_sync_failure_reports_the_published_candidate() {
    for existing in [false, true] {
        let fixture = Fixture::new();
        if existing {
            fixture.register(&fixture.workspace("existing"));
        }
        let result = fixture.store.apply_with(
            RegistryMutation::Register(&fixture.workspace("new")),
            LOCK_TIMEOUT,
            |step| {
                if step == WriteStep::SyncDirectory {
                    Err(io::Error::other("injected parent sync error"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(
            matches!(
                result,
                Err(RegistryWriteError::AfterPublish {
                    step: RegistryPublishStep::SyncDirectory,
                    observed: RegistrySaveState::CandidatePresent,
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(
            fixture.read().entries_by_recent_use().len(),
            1 + usize::from(existing)
        );
        fixture.check_no_staging();
    }
}

#[test]
fn unrecognized_state_after_publish_is_unknown_and_never_retried() {
    let fixture = Fixture::new();
    let external = b"version = 1\nnext_seq = 50\n";
    let mut attempts = 0;
    let result = fixture.store.apply_with(
        RegistryMutation::RecordDirectWorkspaceOpen(&fixture.workspace("new")),
        LOCK_TIMEOUT,
        |step| {
            if step == WriteStep::AfterPublish {
                attempts += 1;
                fs::write(fixture.registry(), external)?;
                return Err(io::Error::other("injected ambiguous publication"));
            }
            Ok(())
        },
    );
    assert!(
        matches!(
            result,
            Err(RegistryWriteError::AfterPublish {
                step: RegistryPublishStep::Replace,
                observed: RegistrySaveState::Unknown,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(attempts, 1);
    assert_eq!(fs::read(fixture.registry()).unwrap(), external);
    fixture.check_no_staging();
}

#[cfg(windows)]
#[test]
fn missing_target_after_replace_attempt_keeps_candidate_for_recovery() {
    let fixture = Fixture::new();
    fixture.register(&fixture.workspace("existing"));
    let baseline = fs::read(fixture.registry()).unwrap();
    let displaced = fixture.state.join("displaced.toml");
    let workspace = fixture.workspace("new");
    let result = fixture.store.apply_with(
        RegistryMutation::Register(&workspace),
        LOCK_TIMEOUT,
        |step| {
            if step == WriteStep::BeforePublish {
                fs::rename(fixture.registry(), &displaced)?;
            }
            Ok(())
        },
    );
    assert!(
        matches!(
            result,
            Err(RegistryWriteError::AfterPublish {
                step: RegistryPublishStep::Replace,
                observed: RegistrySaveState::Unknown,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(fs::read(displaced).unwrap(), baseline);
    assert!(!fixture.registry().exists());
    assert!(matches!(
        fixture.store.load(),
        Err(RegistryReadError::RecoveryRequired)
    ));
    assert!(matches!(
        fixture.store.apply(RegistryMutation::Register(&workspace)),
        Err(RegistryWriteError::Read(
            RegistryReadError::RecoveryRequired
        ))
    ));
    let candidates: Vec<_> = fs::read_dir(&fixture.state)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".workspaces-staging-replace-")
        })
        .collect();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        decode(&fs::read(candidates[0].path()).unwrap())
            .unwrap()
            .entries_by_recent_use()
            .len(),
        2
    );
}

#[test]
fn interrupted_first_creation_neither_publishes_nor_deletes_the_staging_file() {
    let fixture = Fixture::new();
    let state = StateDirectory::open(&fixture.store.location, true)
        .unwrap()
        .unwrap();
    let (mut staged, mut file) = state.stage(false).unwrap();
    file.write_all(b"partial candidate").unwrap();
    staged.preserve();
    drop(staged);
    drop(file);
    assert!(matches!(
        fixture.store.load().unwrap(),
        RegistryLoad::Missing
    ));
    fixture.register(&fixture.workspace("new"));
    let leftovers: Vec<_> = fs::read_dir(&fixture.state)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".workspaces-staging-new-")
        })
        .collect();
    assert_eq!(leftovers.len(), 1);
    assert_eq!(fs::read(leftovers[0].path()).unwrap(), b"partial candidate");
}

#[cfg(windows)]
#[test]
fn sharing_violation_preserves_baseline_and_reports_readback() {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let fixture = Fixture::new();
    fixture.register(&fixture.workspace("existing"));
    let blocker = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(fixture.registry())
        .unwrap();
    let before = fs::read(fixture.registry()).unwrap();
    let result = fixture
        .store
        .apply(RegistryMutation::Register(&fixture.workspace("new")));
    assert!(
        matches!(
            result,
            Err(RegistryWriteError::AfterPublish {
                step: RegistryPublishStep::Replace,
                observed: RegistrySaveState::BaselinePresent,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(fs::read(fixture.registry()).unwrap(), before);
    drop(blocker);
    fixture.check_no_staging();
}

#[cfg(windows)]
#[test]
fn windows_replacement_preserves_hidden_and_explicit_dacl() {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
    let fixture = Fixture::new();
    fixture.register(&fixture.workspace("existing"));
    let path = fixture.registry();
    assert!(
        Command::new("attrib")
            .arg("+H")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("icacls")
            .arg(&path)
            .arg("/inheritance:d")
            .output()
            .unwrap()
            .status
            .success()
    );
    fixture.register(&fixture.workspace("new"));
    assert_ne!(
        fs::metadata(path).unwrap().file_attributes() & FILE_ATTRIBUTE_HIDDEN,
        0
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_replacement_preserves_mode_acl_and_xattr() {
    let fixture = Fixture::new();
    fixture.register(&fixture.workspace("existing"));
    let path = fixture.registry();
    let command = |program: &str, args: &[&str]| {
        assert!(
            Command::new(program)
                .args(args)
                .arg(&path)
                .output()
                .unwrap()
                .status
                .success()
        );
    };
    command("/bin/chmod", &["640"]);
    command("/bin/chmod", &["+a", "everyone allow readattr"]);
    command(
        "/usr/bin/xattr",
        &["-w", "com.atc-rs.registry-test", "before"],
    );
    let state = StateDirectory::open(&fixture.store.location, false)
        .unwrap()
        .unwrap();
    let before = state.read_snapshot().unwrap().unwrap().metadata;
    fixture.register(&fixture.workspace("new"));
    let after = state.read_snapshot().unwrap().unwrap().metadata;
    assert_eq!(after, before);
}

#[test]
fn native_workspace_paths_round_trip_without_touching_workspace_files() {
    let fixture = Fixture::new();
    let root = fixture.temp.path().join("workspace");
    let contest = root.join("ABC").join("abc123");
    fs::create_dir_all(&contest).unwrap();
    fs::write(root.join(".atc-workspace.toml"), b"version = 1\n").unwrap();
    fs::write(contest.join("A.py"), b"print(1)\n").unwrap();
    let workspace = fixture.workspace("workspace");
    let entry = fixture.register(&workspace);
    let opened =
        VerifiedContestOpen::from_successful_open(&workspace, "abc123", "ABC/abc123").unwrap();
    fixture
        .store
        .apply(RegistryMutation::RecordContestOpen {
            entry: &entry,
            opened: &opened,
        })
        .unwrap();
    fixture
        .store
        .apply(RegistryMutation::Remove(&entry))
        .unwrap();
    assert_eq!(
        fs::read(root.join(".atc-workspace.toml")).unwrap(),
        b"version = 1\n"
    );
    assert_eq!(fs::read(contest.join("A.py")).unwrap(), b"print(1)\n");
    assert_eq!(fs::read_dir(root).unwrap().count(), 2);
    assert_eq!(fs::read_dir(contest).unwrap().count(), 1);

    #[cfg(unix)]
    let native = {
        use std::os::unix::ffi::OsStringExt as _;
        std::ffi::OsString::from_vec(vec![b'a', 0xff])
    };
    #[cfg(windows)]
    let native = {
        use std::os::windows::ffi::OsStringExt as _;
        std::ffi::OsString::from_wide(&[0x0061, 0xd800])
    };
    let path = fixture.temp.path().join(native);
    let workspace = VerifiedWorkspace::from_verified_root(path.clone(), None).unwrap();
    fixture.register(&workspace);
    assert_eq!(
        fixture.read().entries_by_recent_use()[0].root().as_os_str(),
        path.as_os_str()
    );
}

const CHILD_BASE: &str = "ATC_TEST_REGISTRY_CHILD_BASE";
const CHILD_CONTEST: &str = "ATC_TEST_REGISTRY_CHILD_CONTEST";

#[test]
fn registry_child_process() {
    let Some(base) = std::env::var_os(CHILD_BASE).map(PathBuf::from) else {
        return;
    };
    let contest_id = std::env::var(CHILD_CONTEST).unwrap();
    let workspace = VerifiedWorkspace::from_verified_root(base.join("shared"), None).unwrap();
    let location = crate::paths::registry_location().unwrap();
    assert_eq!(location.platform_base(), base.as_path());
    let store = RegistryStore::new(location);
    fs::write(base.join(format!("ready-{contest_id}")), b"").unwrap();
    let entry = match store.apply(RegistryMutation::Register(&workspace)).unwrap() {
        RegistryApply::Applied {
            result: RegistryMutationResult::Registered(entry),
            ..
        }
        | RegistryApply::NoChange {
            result: RegistryMutationResult::AlreadyRegistered(entry),
            ..
        } => entry,
        result => panic!("expected registration: {result:?}"),
    };
    let opened =
        VerifiedContestOpen::from_successful_open(&workspace, &contest_id, &contest_id).unwrap();
    assert!(matches!(
        store
            .apply(RegistryMutation::RecordContestOpen {
                entry: &entry,
                opened: &opened
            })
            .unwrap(),
        RegistryApply::Applied { .. }
    ));
}

#[test]
fn processes_serialize_register_and_record_against_latest_snapshot() {
    let fixture = Fixture::new();
    let dir = StateDirectory::open(&fixture.store.location, true)
        .unwrap()
        .unwrap();
    let base = fixture.store.location.platform_base();
    let lock = dir.lock(Duration::ZERO).unwrap();
    let mut children = Vec::new();
    for id in ["abc123", "arc123"] {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_registry::store::tests::registry_child_process",
            ])
            .env(CHILD_BASE, base)
            .env(
                if cfg!(windows) {
                    "LOCALAPPDATA"
                } else {
                    "XDG_STATE_HOME"
                },
                base,
            )
            .env(CHILD_CONTEST, id)
            .spawn()
            .unwrap();
        children.push(ChildProcess(child));
    }
    wait_until(|| {
        ["abc123", "arc123"]
            .iter()
            .all(|id| base.join(format!("ready-{id}")).exists())
    });
    drop(lock);
    for child in &mut children {
        wait_until(|| child.0.try_wait().unwrap().is_some());
        assert!(child.0.wait().unwrap().success());
    }
    let snapshot = fixture.read();
    let entries = snapshot.entries_by_recent_use();
    assert_eq!(entries.len(), 1);
    let mut ids: Vec<_> = entries[0]
        .recent_contests_by_use()
        .into_iter()
        .map(|recent| recent.contest_id())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["abc123", "arc123"]);
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "test process timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct ChildProcess(Child);

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
