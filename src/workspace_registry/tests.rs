use super::*;

fn root(name: &str) -> PathBuf {
    #[cfg(windows)]
    let base = Path::new(r"C:\AtCoder");
    #[cfg(not(windows))]
    let base = Path::new("/AtCoder");
    base.join(name)
}

fn identity(number: u8) -> DirectoryIdentity {
    #[cfg(unix)]
    let identity = DirectoryIdentity::Unix {
        device: u64::MAX,
        inode: u64::from(number),
    };
    #[cfg(windows)]
    let identity = DirectoryIdentity::Windows {
        volume_serial_number: u64::MAX,
        file_id: [number; 16],
    };
    identity
}

fn verified(name: &str, identity: Option<DirectoryIdentity>) -> VerifiedWorkspace {
    VerifiedWorkspace::from_verified_root(root(name), identity).unwrap()
}

fn register(
    snapshot: &RegistrySnapshot,
    workspace: &VerifiedWorkspace,
) -> (RegistrySnapshot, EntryRef) {
    let (snapshot, result) = snapshot
        .apply(RegistryMutation::Register(workspace))
        .unwrap();
    let RegistryMutationResult::Registered(entry) = result else {
        panic!("expected a new registration: {result:?}")
    };
    (snapshot, entry)
}

fn record_contest(
    snapshot: &RegistrySnapshot,
    entry: &EntryRef,
    workspace: &VerifiedWorkspace,
    contest_id: &str,
    path: &str,
) -> RegistrySnapshot {
    let opened = VerifiedContestOpen::from_successful_open(workspace, contest_id, path).unwrap();
    let (snapshot, result) = snapshot
        .apply(RegistryMutation::RecordContestOpen {
            entry,
            opened: &opened,
        })
        .unwrap();
    assert_eq!(result, RegistryMutationResult::Recorded(entry.clone()));
    snapshot
}

fn sample_toml() -> String {
    let root = toml::Value::String(root("競技").into_os_string().into_string().unwrap());
    #[cfg(unix)]
    let identity = r#"kind = "unix", device = "ffffffffffffffff", inode = "0000000000000002""#;
    #[cfg(windows)]
    let identity = r#"kind = "windows", volume_serial_number = "ffffffffffffffff", file_id = "02020202020202020202020202020202""#;
    format!(
        r#"version = 1
next_seq = 9

[[workspaces]]
entry_id = 1
root = {{ encoding = "utf8", value = {root} }}
identity = {{ {identity} }}
last_used_seq = 8
recent_contests = [
  {{ contest_id = "abc123", relative_path = "ABC/abc123", last_used_seq = 8 }},
]
"#
    )
}

#[test]
fn empty_snapshot_round_trips_and_requires_a_version_and_counter() {
    let empty = RegistrySnapshot::default();
    assert!(empty.entries_by_recent_use().is_empty());
    assert_eq!(
        RegistrySnapshot::from_toml("version = 1\nnext_seq = 1").unwrap(),
        empty
    );
    assert_eq!(
        RegistrySnapshot::from_toml(&empty.to_toml().unwrap()).unwrap(),
        empty
    );
    assert!(matches!(
        RegistrySnapshot::from_toml(""),
        Err(RegistryError::Decode(_))
    ));
}

#[test]
fn documented_v1_toml_round_trips_with_utf8_root_identity_and_history() {
    let snapshot = RegistrySnapshot::from_toml(&sample_toml()).unwrap();
    let entries = snapshot.entries_by_recent_use();
    let entry = entries[0];
    assert_eq!(entry.entry_ref().entry_id(), 1);
    assert_eq!(entry.root().as_os_str(), root("競技").as_os_str());
    assert_eq!(entry.identity(), Some(identity(2)));
    assert_eq!(entry.last_used_seq(), Some(8));
    let history = entry.recent_contests_by_use();
    assert_eq!(history[0].contest_id(), "abc123");
    assert_eq!(history[0].relative_path(), "ABC/abc123");
    assert_eq!(history[0].last_used_seq(), 8);
    let encoded = snapshot.to_toml().unwrap();
    assert!(encoded.contains("utf8"));
    assert_eq!(RegistrySnapshot::from_toml(&encoded).unwrap(), snapshot);
}

#[test]
fn native_non_unicode_root_round_trips_exactly() {
    #[cfg(unix)]
    let (path, tag) = {
        use std::os::unix::ffi::OsStringExt;
        (
            PathBuf::from(std::ffi::OsString::from_vec(
                b"/atcoder/\xff/\xc0\xaf".to_vec(),
            )),
            "unix_bytes",
        )
    };
    #[cfg(windows)]
    let (path, tag) = {
        use std::os::windows::ffi::OsStringExt;
        let mut wide: Vec<_> = r"\\?\C:\AtCoder\".encode_utf16().collect();
        wide.extend([0xd800, b'X' as u16, 0xdfff]);
        (
            PathBuf::from(std::ffi::OsString::from_wide(&wide)),
            "windows_utf16",
        )
    };
    assert!(path.to_str().is_none());
    let workspace = VerifiedWorkspace::from_verified_root(path.clone(), None).unwrap();
    let (snapshot, _) = register(&RegistrySnapshot::default(), &workspace);
    let encoded = snapshot.to_toml().unwrap();
    assert!(encoded.contains(tag));
    let decoded = RegistrySnapshot::from_toml(&encoded).unwrap();
    assert_eq!(
        decoded.entries_by_recent_use()[0].root().as_os_str(),
        path.as_os_str()
    );
    assert_eq!(decoded, snapshot);
}

#[test]
fn register_is_idempotent_and_only_successful_opens_change_recency() {
    let first = verified("first", None);
    let second = verified("second", None);
    let third = verified("third", None);
    let (snapshot, first_entry) = register(&RegistrySnapshot::default(), &first);
    let (snapshot, second_entry) = register(&snapshot, &second);
    assert!(
        snapshot
            .entries_by_recent_use()
            .iter()
            .all(|entry| entry.last_used_seq().is_none())
    );
    let (snapshot, result) = snapshot
        .apply(RegistryMutation::RecordDirectWorkspaceOpen(&second))
        .unwrap();
    assert_eq!(
        result,
        RegistryMutationResult::Recorded(second_entry.clone())
    );
    let (unchanged, result) = snapshot.apply(RegistryMutation::Register(&first)).unwrap();
    assert_eq!(
        result,
        RegistryMutationResult::AlreadyRegistered(first_entry.clone())
    );
    assert_eq!(unchanged, snapshot);
    assert_eq!(
        snapshot.entries_by_recent_use()[0].entry_ref(),
        second_entry
    );

    let (snapshot, result) = snapshot
        .apply(RegistryMutation::RecordDirectWorkspaceOpen(&third))
        .unwrap();
    let RegistryMutationResult::Recorded(third_entry) = result else {
        panic!("expected successful direct open")
    };
    assert_eq!(snapshot.entries_by_recent_use()[0].entry_ref(), third_entry);
    let (snapshot, result) = snapshot
        .apply(RegistryMutation::RecordWorkspaceOpen {
            entry: &first_entry,
            opened: &first,
        })
        .unwrap();
    assert_eq!(
        result,
        RegistryMutationResult::Recorded(first_entry.clone())
    );
    let order: Vec<_> = snapshot
        .entries_by_recent_use()
        .into_iter()
        .map(RegistryEntry::entry_ref)
        .collect();
    assert_eq!(order, [first_entry, third_entry, second_entry]);
    assert_eq!(
        RegistrySnapshot::from_toml(&snapshot.to_toml().unwrap()).unwrap(),
        snapshot
    );
}

#[test]
fn a_matching_stored_identity_is_not_enough_to_merge_an_alias() {
    let original = verified("École", Some(identity(1)));
    let case_alias = verified("école", Some(identity(1)));
    let unicode_alias = verified("E\u{301}cole", Some(identity(1)));
    let (snapshot, original_entry) = register(&RegistrySnapshot::default(), &original);
    let (separate, _) = register(&snapshot, &case_alias);
    let (separate, _) = register(&separate, &unicode_alias);
    assert_eq!(separate.entries_by_recent_use().len(), 3);

    let alias = case_alias
        .with_verified_alias_of(&original_entry, &original)
        .unwrap();
    let (unchanged, result) = snapshot.apply(RegistryMutation::Register(&alias)).unwrap();
    assert_eq!(
        result,
        RegistryMutationResult::AlreadyRegistered(original_entry.clone())
    );
    assert_eq!(unchanged, snapshot);
    let (opened, result) = snapshot
        .apply(RegistryMutation::RecordDirectWorkspaceOpen(&alias))
        .unwrap();
    assert_eq!(
        result,
        RegistryMutationResult::Recorded(original_entry.clone())
    );
    assert_eq!(
        opened.entries_by_recent_use()[0].root().as_os_str(),
        original.root.as_path().as_os_str()
    );
    assert!(matches!(
        separate.apply(RegistryMutation::Register(&alias)),
        Err(RegistryError::WorkspaceMismatch(1))
    ));

    let (removed, _) = snapshot
        .apply(RegistryMutation::Remove(&original_entry))
        .unwrap();
    assert!(matches!(
        removed.apply(RegistryMutation::RecordDirectWorkspaceOpen(&alias)),
        Err(RegistryError::StaleEntry(1))
    ));
}

#[test]
fn path_comparison_preserves_spelling_even_when_path_equality_skips_components() {
    let original = verified("contests", None);
    let spelling = root(".").join("contests");
    assert_eq!(original.root.as_path(), spelling);
    let alias = VerifiedWorkspace::from_verified_root(spelling.clone(), None).unwrap();
    let (snapshot, _) = register(&RegistrySnapshot::default(), &original);
    let (snapshot, _) = register(&snapshot, &alias);
    let decoded = RegistrySnapshot::from_toml(&snapshot.to_toml().unwrap()).unwrap();
    assert_eq!(decoded.entries_by_recent_use().len(), 2);
    assert_eq!(
        decoded.entries_by_recent_use()[1].root().as_os_str(),
        spelling.as_os_str()
    );
}

#[test]
fn refuses_replacement_at_same_root_and_requires_fresh_evidence_for_aliases() {
    let original = verified("workspace", Some(identity(1)));
    let replacement = verified("workspace", Some(identity(2)));
    let unavailable = verified("workspace", None);
    let alias = verified("alias", Some(identity(1)));
    let (snapshot, entry) = register(&RegistrySnapshot::default(), &original);
    let original_toml = snapshot.to_toml().unwrap();
    for observed in [&replacement, &unavailable, &alias] {
        assert!(matches!(
            snapshot.apply(RegistryMutation::RecordWorkspaceOpen {
                entry: &entry,
                opened: observed
            }),
            Err(RegistryError::WorkspaceMismatch(1))
        ));
    }
    assert!(matches!(
        snapshot.apply(RegistryMutation::Register(&replacement)),
        Err(RegistryError::WorkspaceMismatch(1))
    ));
    assert!(matches!(
        snapshot.apply(RegistryMutation::RecordDirectWorkspaceOpen(&replacement)),
        Err(RegistryError::WorkspaceMismatch(1))
    ));
    assert!(matches!(
        alias.with_verified_alias_of(&entry, &replacement),
        Err(RegistryError::WorkspaceMismatch(1))
    ));
    assert!(matches!(
        verified("alias", Some(identity(2))).with_verified_alias_of(&entry, &replacement),
        Err(RegistryError::WorkspaceMismatch(1))
    ));
    assert_eq!(snapshot.to_toml().unwrap(), original_toml);
}

#[test]
fn contest_open_updates_both_sequences_and_replaces_the_old_route() {
    let workspace = verified("workspace", None);
    let (snapshot, entry) = register(&RegistrySnapshot::default(), &workspace);
    let snapshot = record_contest(&snapshot, &entry, &workspace, "abc123", "ABC/abc123");
    let snapshot = record_contest(&snapshot, &entry, &workspace, "arc123", "ARC/arc123");
    let snapshot = record_contest(&snapshot, &entry, &workspace, "abc123", "new/abc123");
    let entries = snapshot.entries_by_recent_use();
    let history = entries[0].recent_contests_by_use();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].contest_id(), "abc123");
    assert_eq!(history[0].relative_path(), "new/abc123");
    assert_eq!(history[1].contest_id(), "arc123");
    assert_eq!(Some(history[0].last_used_seq()), entries[0].last_used_seq());
    assert!(history[0].last_used_seq() > history[1].last_used_seq());
}

#[test]
fn contest_history_is_bounded_per_workspace_by_recency() {
    let workspace = verified("workspace", None);
    let other = verified("other", None);
    let (snapshot, other_entry) = register(&RegistrySnapshot::default(), &other);
    let snapshot = record_contest(&snapshot, &other_entry, &other, "abc002", "ABC/abc002");
    let (mut snapshot, entry) = register(&snapshot, &workspace);
    for number in 1..=20 {
        let contest_id = format!("abc{number:03}");
        snapshot = record_contest(&snapshot, &entry, &workspace, &contest_id, &contest_id);
    }
    let snapshot = record_contest(&snapshot, &entry, &workspace, "abc001", "old/abc001");
    let snapshot = record_contest(&snapshot, &entry, &workspace, "abc021", "ABC/abc021");
    let entries = snapshot.entries_by_recent_use();
    let history = entries[0].recent_contests_by_use();
    assert_eq!(history.len(), MAX_RECENT_CONTESTS);
    assert_eq!(history[0].contest_id(), "abc021");
    assert_eq!(history[1].contest_id(), "abc001");
    assert!(!history.iter().any(|recent| recent.contest_id() == "abc002"));
    assert_eq!(
        entries[1].recent_contests_by_use()[0].contest_id(),
        "abc002"
    );
}

#[test]
fn remove_drops_history_and_delayed_updates_cannot_affect_a_new_registration() {
    let workspace = verified("workspace", Some(identity(1)));
    let (snapshot, old_entry) = register(&RegistrySnapshot::default(), &workspace);
    let snapshot = record_contest(&snapshot, &old_entry, &workspace, "abc123", "abc123");
    let (removed, result) = snapshot
        .apply(RegistryMutation::Remove(&old_entry))
        .unwrap();
    assert_eq!(result, RegistryMutationResult::Removed);
    assert!(removed.entries_by_recent_use().is_empty());
    let removed = RegistrySnapshot::from_toml(&removed.to_toml().unwrap()).unwrap();
    let (readded, new_entry) = register(&removed, &workspace);
    assert!(new_entry.entry_id() > old_entry.entry_id());
    assert!(
        readded.entries_by_recent_use()[0]
            .recent_contests_by_use()
            .is_empty()
    );

    let opened = VerifiedContestOpen::from_successful_open(&workspace, "abc123", "abc123").unwrap();
    for snapshot in [&removed, &readded] {
        let before = snapshot.to_toml().unwrap();
        for mutation in [
            RegistryMutation::RecordWorkspaceOpen {
                entry: &old_entry,
                opened: &workspace,
            },
            RegistryMutation::RecordContestOpen {
                entry: &old_entry,
                opened: &opened,
            },
            RegistryMutation::Remove(&old_entry),
        ] {
            assert!(matches!(
                snapshot.apply(mutation),
                Err(RegistryError::StaleEntry(1))
            ));
        }
        assert_eq!(snapshot.to_toml().unwrap(), before);
    }
}

#[test]
fn references_match_the_full_registration_and_contest_evidence_matches_workspace() {
    let workspace = verified("workspace", Some(identity(1)));
    let different_root = verified("different", Some(identity(1)));
    let different_identity = verified("workspace", Some(identity(2)));
    let empty = RegistrySnapshot::default();
    let (snapshot, entry) = register(&empty, &workspace);
    for different in [&different_root, &different_identity] {
        let (_, wrong_entry) = register(&empty, different);
        assert_eq!(wrong_entry.entry_id(), entry.entry_id());
        assert!(matches!(
            snapshot.apply(RegistryMutation::Remove(&wrong_entry)),
            Err(RegistryError::StaleEntry(1))
        ));
        let opened =
            VerifiedContestOpen::from_successful_open(different, "abc123", "abc123").unwrap();
        assert!(matches!(
            snapshot.apply(RegistryMutation::RecordContestOpen {
                entry: &entry,
                opened: &opened
            }),
            Err(RegistryError::WorkspaceMismatch(1))
        ));
    }
}

#[test]
fn sorted_views_leave_the_document_order_intact() {
    let mut document = RegistrySnapshot::from_toml(&sample_toml()).unwrap().0;
    let first = &mut document.workspaces[0];
    first.recent_contests.insert(
        0,
        RecentContest {
            contest_id: "arc100".into(),
            relative_path: "ARC/arc100".into(),
            last_used_seq: 3,
        },
    );
    let other = verified("other", None);
    document.workspaces.insert(
        0,
        RegistryEntry {
            entry_id: 2,
            root: other.root,
            identity: None,
            last_used_seq: 0,
            recent_contests: Vec::new(),
        },
    );
    let source = toml::to_string(&document).unwrap();
    let snapshot = RegistrySnapshot::from_toml(&source).unwrap();
    let entries = snapshot.entries_by_recent_use();
    assert_eq!(entries[0].entry_ref().entry_id(), 1);
    assert_eq!(
        entries[0].recent_contests_by_use()[0].contest_id(),
        "abc123"
    );
    assert_eq!(snapshot.to_toml().unwrap(), source);
}

#[test]
fn rejects_invalid_versions_ids_and_sequence_relationships() {
    let valid = sample_toml();
    assert!(matches!(
        RegistrySnapshot::from_toml(&valid.replace("version = 1", "version = 2")),
        Err(RegistryError::UnsupportedVersion(2))
    ));
    for id in [0, -1, 9] {
        let source = valid.replace("entry_id = 1", &format!("entry_id = {id}"));
        assert!(
            matches!(RegistrySnapshot::from_toml(&source), Err(RegistryError::InvalidEntryId(actual)) if actual == id)
        );
    }
    let (_, workspace) = valid.split_once("[[workspaces]]").unwrap();
    let duplicated = format!("{valid}\n[[workspaces]]{workspace}");
    assert!(matches!(
        RegistrySnapshot::from_toml(&duplicated),
        Err(RegistryError::DuplicateEntryId(1))
    ));
    let duplicate_root = format!(
        "{valid}\n[[workspaces]]{}",
        workspace.replace("entry_id = 1", "entry_id = 2")
    );
    assert!(matches!(
        RegistrySnapshot::from_toml(&duplicate_root),
        Err(RegistryError::DuplicateRoot(2))
    ));

    for (from, to) in [
        ("next_seq = 9", "next_seq = 0"),
        ("next_seq = 9", "next_seq = -1"),
        ("next_seq = 9", "next_seq = 8"),
        ("last_used_seq = 8\nrecent", "last_used_seq = -1\nrecent"),
        ("last_used_seq = 8\nrecent", "last_used_seq = 0\nrecent"),
        ("last_used_seq = 8\nrecent", "last_used_seq = 7\nrecent"),
        ("last_used_seq = 8 },", "last_used_seq = 1 },"),
    ] {
        let source = valid.replace(from, to);
        assert!(
            matches!(
                RegistrySnapshot::from_toml(&source),
                Err(RegistryError::InvalidSequence(_))
            ),
            "accepted {source}"
        );
    }
    let other_root = toml::Value::String(root("different").to_str().unwrap().to_owned());
    let reuse_sequence = format!(
        "{valid}\n[[workspaces]]\nentry_id = 2\nroot = {{ encoding = 'utf8', value = {other_root} }}\nlast_used_seq = 8\n"
    );
    assert!(matches!(
        RegistrySnapshot::from_toml(&reuse_sequence),
        Err(RegistryError::InvalidSequence(8))
    ));
}

#[test]
fn rejects_invalid_root_paths_encodings_and_identity_fields() {
    let valid = sample_toml();
    let root_line = valid
        .lines()
        .find(|line| line.starts_with("root = "))
        .unwrap();
    let foreign = if cfg!(windows) {
        "unix_bytes"
    } else {
        "windows_utf16"
    };
    let malformed_native = if cfg!(windows) {
        "windows_utf16"
    } else {
        "unix_bytes"
    };
    for encoded in [
        "encoding = 'utf8', value = 'relative/path'".to_owned(),
        r#"encoding = "utf8", value = "C:/bad\u0000path""#.to_owned(),
        "encoding = 'unknown', value = 'data'".to_owned(),
        format!("encoding = '{foreign}', value = 'abcd'"),
        format!("encoding = '{malformed_native}', value = 'abc'"),
        format!("encoding = '{malformed_native}', value = '00gg'"),
    ] {
        let source = valid.replace(root_line, &format!("root = {{ {encoded} }}"));
        assert!(
            matches!(
                RegistrySnapshot::from_toml(&source),
                Err(RegistryError::Decode(_))
            ),
            "accepted {source}"
        );
    }
    assert!(matches!(
        VerifiedWorkspace::from_verified_root("relative".into(), None),
        Err(RegistryError::InvalidRoot(_))
    ));
    assert!(matches!(
        VerifiedWorkspace::from_verified_root(root("bad\0path"), None),
        Err(RegistryError::InvalidRoot(_))
    ));
    for malformed in ["fffffffffffffff", "FFFFFFFFFFFFFFFF", "fffffffffffffffg"] {
        assert!(matches!(
            RegistrySnapshot::from_toml(&valid.replace("ffffffffffffffff", malformed)),
            Err(RegistryError::Decode(_))
        ));
    }
    let foreign_identity = if cfg!(windows) { "unix" } else { "windows" };
    let identity_line = valid
        .lines()
        .find(|line| line.starts_with("identity = "))
        .unwrap();
    assert!(matches!(
        RegistrySnapshot::from_toml(&valid.replace(
            identity_line,
            &format!("identity = {{ kind = '{foreign_identity}' }}")
        )),
        Err(RegistryError::Decode(_))
    ));
}

#[test]
fn rejects_unsafe_contest_paths_on_both_load_and_mutation_boundaries() {
    let workspace = verified("workspace", None);
    let valid = sample_toml();
    for path in [
        "",
        ".",
        "..",
        "../abc123",
        "ABC/../abc123",
        "ABC/./abc123",
        "/abc123",
        "C:/abc123",
        "C:abc123",
        r"ABC\abc123",
        "ABC//abc123",
        "ABC/abc123/",
        "ABC/NUL",
        "ABC/abc123.",
        "ABC/abc\0",
    ] {
        let value = toml::Value::String(path.to_owned());
        let source = valid.replace(r#""ABC/abc123""#, &value.to_string());
        assert!(
            matches!(
                RegistrySnapshot::from_toml(&source),
                Err(RegistryError::InvalidContestPath)
            ),
            "accepted {path:?}"
        );
        assert!(matches!(
            VerifiedContestOpen::from_successful_open(&workspace, "abc123", path),
            Err(RegistryError::InvalidContestPath)
        ));
    }
    assert!(matches!(
        RegistrySnapshot::from_toml(&valid.replace("abc123", "..")),
        Err(RegistryError::InvalidContestId)
    ));
    assert!(matches!(
        VerifiedContestOpen::from_successful_open(&workspace, "..", "ABC/abc123"),
        Err(RegistryError::InvalidContestId)
    ));
}

#[test]
fn rejects_duplicate_contests_and_oversized_history_without_repair() {
    let valid = sample_toml();
    let recent = r#"  { contest_id = "abc123", relative_path = "ABC/abc123", last_used_seq = 8 },"#;
    let duplicate = format!(
        "{recent}\n  {{ contest_id = 'abc123', relative_path = 'new/abc123', last_used_seq = 7 }},"
    );
    assert!(matches!(
        RegistrySnapshot::from_toml(&valid.replace(recent, &duplicate)),
        Err(RegistryError::DuplicateContest(1))
    ));
    let reused_seq = duplicate
        .replace("contest_id = 'abc123'", "contest_id = 'arc123'")
        .replace("last_used_seq = 7", "last_used_seq = 8");
    assert!(matches!(
        RegistrySnapshot::from_toml(&valid.replace(recent, &reused_seq)),
        Err(RegistryError::InvalidSequence(8))
    ));
    let oversized = format!("{recent}\n").repeat(MAX_RECENT_CONTESTS + 1);
    assert!(matches!(
        RegistrySnapshot::from_toml(&valid.replace(recent, &oversized)),
        Err(RegistryError::TooManyRecentContests(1))
    ));
}

#[test]
fn counter_exhaustion_fails_without_consuming_ids_or_recording_partial_updates() {
    let workspace = verified("workspace", None);
    let almost_full = format!("version = 1\nnext_seq = {}", i64::MAX - 1);
    let snapshot = RegistrySnapshot::from_toml(&almost_full).unwrap();
    assert!(matches!(
        snapshot.apply(RegistryMutation::RecordDirectWorkspaceOpen(&workspace)),
        Err(RegistryError::SequenceExhausted)
    ));
    assert_eq!(snapshot, RegistrySnapshot::from_toml(&almost_full).unwrap());
    let (full, entry) = register(&snapshot, &workspace);
    assert_eq!(entry.entry_id(), i64::MAX - 1);
    let full_toml = full.to_toml().unwrap();
    assert_eq!(RegistrySnapshot::from_toml(&full_toml).unwrap(), full);
    assert!(matches!(
        full.apply(RegistryMutation::Register(&verified("new", None))),
        Err(RegistryError::SequenceExhausted)
    ));
    assert!(matches!(
        full.apply(RegistryMutation::RecordWorkspaceOpen {
            entry: &entry,
            opened: &workspace
        }),
        Err(RegistryError::SequenceExhausted)
    ));
    let opened = VerifiedContestOpen::from_successful_open(&workspace, "abc123", "abc123").unwrap();
    assert!(matches!(
        full.apply(RegistryMutation::RecordContestOpen {
            entry: &entry,
            opened: &opened
        }),
        Err(RegistryError::SequenceExhausted)
    ));
    assert_eq!(
        full.apply(RegistryMutation::Register(&workspace)).unwrap(),
        (
            full.clone(),
            RegistryMutationResult::AlreadyRegistered(entry.clone())
        )
    );
    let (removed, _) = full.apply(RegistryMutation::Remove(&entry)).unwrap();
    assert!(matches!(
        removed.apply(RegistryMutation::Register(&workspace)),
        Err(RegistryError::SequenceExhausted)
    ));
    assert_eq!(full.to_toml().unwrap(), full_toml);
    assert!(matches!(
        RegistrySnapshot::from_toml("version = 1\nnext_seq = 9223372036854775808"),
        Err(RegistryError::Decode(_))
    ));
}
