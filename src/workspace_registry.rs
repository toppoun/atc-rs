//! Validated snapshots and pure updates for the Workspace and Contest recency index.

mod encoding;
pub(crate) mod store;

use std::cmp::Reverse;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace::{validate_path_component, validate_workspace_path_component};

pub(crate) use encoding::DirectoryIdentity;
use encoding::WorkspaceRoot;

const REGISTRY_VERSION: i64 = 1;
const MAX_RECENT_CONTESTS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistrySnapshot(RegistryDocument);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryDocument {
    version: i64,
    // IDs and use events share an allocator so removal cannot free an ID.
    next_seq: i64,
    #[serde(default)]
    workspaces: Vec<RegistryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryEntry {
    entry_id: i64,
    root: WorkspaceRoot,
    identity: Option<DirectoryIdentity>,
    // Zero represents a registration that has never been opened.
    last_used_seq: i64,
    #[serde(default)]
    recent_contests: Vec<RecentContest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecentContest {
    contest_id: String,
    relative_path: String,
    last_used_seq: i64,
}

/// The registration as seen by a caller. Kept across opens, removals and reloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntryRef {
    entry_id: i64,
    root: WorkspaceRoot,
    identity: Option<DirectoryIdentity>,
}

/// Evidence supplied by the caller after checking the Workspace on disk.
/// Registry data itself cannot provide this evidence.
#[derive(Debug)]
pub(crate) struct VerifiedWorkspace {
    root: WorkspaceRoot,
    identity: Option<DirectoryIdentity>,
    alias_of: Option<EntryRef>,
}

/// Evidence supplied by the caller after a Contest session successfully starts.
#[derive(Debug)]
pub(crate) struct VerifiedContestOpen<'a> {
    workspace: &'a VerifiedWorkspace,
    contest_id: &'a str,
    relative_path: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RegistryMutation<'a> {
    Register(&'a VerifiedWorkspace),
    RecordWorkspaceOpen {
        entry: &'a EntryRef,
        opened: &'a VerifiedWorkspace,
    },
    /// Only for a fresh direct launch. Session updates must use an EntryRef.
    RecordDirectWorkspaceOpen(&'a VerifiedWorkspace),
    RecordContestOpen {
        entry: &'a EntryRef,
        opened: &'a VerifiedContestOpen<'a>,
    },
    Remove(&'a EntryRef),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegistryMutationResult {
    Registered(EntryRef),
    AlreadyRegistered(EntryRef),
    Recorded(EntryRef),
    Removed,
}

#[derive(Debug)]
pub(crate) enum RegistryError {
    Decode(toml::de::Error),
    Encode(toml::ser::Error),
    UnsupportedVersion(i64),
    InvalidRoot(&'static str),
    InvalidEntryId(i64),
    DuplicateEntryId(i64),
    DuplicateRoot(i64),
    InvalidSequence(i64),
    InvalidContestId,
    InvalidContestPath,
    DuplicateContest(i64),
    TooManyRecentContests(i64),
    SequenceExhausted,
    StaleEntry(i64),
    WorkspaceMismatch(i64),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "invalid Workspace Registry TOML: {error}"),
            Self::Encode(error) => write!(f, "cannot encode Workspace Registry: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported Workspace Registry version: {version}")
            }
            Self::InvalidRoot(reason) => write!(f, "invalid Workspace Registry root: {reason}"),
            Self::InvalidEntryId(id) => write!(f, "invalid Workspace Registry entry ID: {id}"),
            Self::DuplicateEntryId(id) => write!(f, "duplicate Workspace Registry entry ID: {id}"),
            Self::DuplicateRoot(id) => {
                write!(f, "duplicate Workspace Registry root for entry {id}")
            }
            Self::InvalidSequence(seq) => {
                write!(f, "invalid or reused Workspace Registry sequence: {seq}")
            }
            Self::InvalidContestId => f.write_str("invalid Contest ID in Workspace Registry"),
            Self::InvalidContestPath => {
                f.write_str("invalid Contest relative path in Workspace Registry")
            }
            Self::DuplicateContest(id) => write!(
                f,
                "duplicate Contest history for Workspace Registry entry {id}"
            ),
            Self::TooManyRecentContests(id) => write!(
                f,
                "too many recent Contests for Workspace Registry entry {id}"
            ),
            Self::SequenceExhausted => f.write_str("Workspace Registry sequence exhausted"),
            Self::StaleEntry(id) => write!(
                f,
                "Workspace Registry entry {id} no longer matches the registration"
            ),
            Self::WorkspaceMismatch(id) => {
                write!(f, "verified Workspace does not match Registry entry {id}")
            }
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Encode(error) => Some(error),
            _ => None,
        }
    }
}

impl Default for RegistrySnapshot {
    fn default() -> Self {
        Self(RegistryDocument {
            version: REGISTRY_VERSION,
            next_seq: 1,
            workspaces: Vec::new(),
        })
    }
}

impl RegistrySnapshot {
    pub(crate) fn from_toml(source: &str) -> Result<Self, RegistryError> {
        let table = toml::from_str(source).map_err(RegistryError::Decode)?;
        Self::from_toml_table(table)
    }

    fn from_toml_table(table: toml::Table) -> Result<Self, RegistryError> {
        if let Some(version) = table.get("version").and_then(toml::Value::as_integer)
            && version != REGISTRY_VERSION
        {
            return Err(RegistryError::UnsupportedVersion(version));
        }
        let document: RegistryDocument = table.try_into().map_err(RegistryError::Decode)?;
        document.validate()?;
        Ok(Self(document))
    }

    pub(crate) fn to_toml(&self) -> Result<String, RegistryError> {
        toml::to_string(&self.0).map_err(RegistryError::Encode)
    }

    /// Most recently used first. Unopened entries follow in registration order.
    pub(crate) fn entries_by_recent_use(&self) -> Vec<&RegistryEntry> {
        let mut entries: Vec<_> = self.0.workspaces.iter().collect();
        entries.sort_unstable_by_key(|entry| (Reverse(entry.last_used_seq), entry.entry_id));
        entries
    }

    /// Apply a delta to this snapshot, keeping the original intact on every outcome.
    pub(crate) fn apply(
        &self,
        mutation: RegistryMutation<'_>,
    ) -> Result<(Self, RegistryMutationResult), RegistryError> {
        use RegistryMutationResult as Outcome;

        let mut updated = self.clone();
        let document = &mut updated.0;
        let outcome = match mutation {
            RegistryMutation::Register(workspace) => {
                if let Some(index) = document.find_workspace(workspace)? {
                    Outcome::AlreadyRegistered(document.workspaces[index].entry_ref())
                } else {
                    let index = document.insert_workspace(workspace)?;
                    Outcome::Registered(document.workspaces[index].entry_ref())
                }
            }
            RegistryMutation::RecordDirectWorkspaceOpen(workspace) => {
                let index = match document.find_workspace(workspace)? {
                    Some(index) => index,
                    None => document.insert_workspace(workspace)?,
                };
                Outcome::Recorded(document.record_open(index, None)?)
            }
            RegistryMutation::RecordWorkspaceOpen { entry, opened } => {
                let index = document.find_opened_entry(entry, opened)?;
                Outcome::Recorded(document.record_open(index, None)?)
            }
            RegistryMutation::RecordContestOpen { entry, opened } => {
                let index = document.find_opened_entry(entry, opened.workspace)?;
                Outcome::Recorded(document.record_open(index, Some(opened))?)
            }
            RegistryMutation::Remove(entry) => {
                let index = document.find_entry(entry)?;
                document.workspaces.remove(index);
                Outcome::Removed
            }
        };
        Ok((updated, outcome))
    }
}

impl RegistryDocument {
    fn validate(&self) -> Result<(), RegistryError> {
        if self.version != REGISTRY_VERSION {
            return Err(RegistryError::UnsupportedVersion(self.version));
        }
        if self.next_seq <= 0 {
            return Err(RegistryError::InvalidSequence(self.next_seq));
        }

        let mut allocated = HashSet::new();
        let mut roots = HashSet::new();
        for entry in &self.workspaces {
            if !(1..self.next_seq).contains(&entry.entry_id) {
                return Err(RegistryError::InvalidEntryId(entry.entry_id));
            }
            if !allocated.insert(entry.entry_id) {
                return Err(RegistryError::DuplicateEntryId(entry.entry_id));
            }
            if !roots.insert(entry.root.as_path().as_os_str()) {
                return Err(RegistryError::DuplicateRoot(entry.entry_id));
            }
        }
        for entry in &self.workspaces {
            entry.validate_recency(self.next_seq, &mut allocated)?;
        }
        Ok(())
    }

    fn find_entry(&self, reference: &EntryRef) -> Result<usize, RegistryError> {
        self.workspaces
            .iter()
            .position(|entry| reference.matches_entry(entry))
            .ok_or(RegistryError::StaleEntry(reference.entry_id))
    }

    fn find_workspace(
        &self,
        workspace: &VerifiedWorkspace,
    ) -> Result<Option<usize>, RegistryError> {
        let exact = self
            .workspaces
            .iter()
            .position(|entry| entry.root == workspace.root);
        if let Some(reference) = &workspace.alias_of {
            let index = self.find_entry(reference)?;
            if exact.is_some_and(|exact| exact != index) {
                return Err(RegistryError::WorkspaceMismatch(reference.entry_id));
            }
            return Ok(Some(index));
        }
        if let Some(index) = exact {
            let entry = &self.workspaces[index];
            if !matches_observation(entry.identity, workspace.identity) {
                return Err(RegistryError::WorkspaceMismatch(entry.entry_id));
            }
        }
        Ok(exact)
    }

    fn find_opened_entry(
        &self,
        entry: &EntryRef,
        opened: &VerifiedWorkspace,
    ) -> Result<usize, RegistryError> {
        let index = self.find_entry(entry)?;
        if self.find_workspace(opened)? != Some(index) {
            return Err(RegistryError::WorkspaceMismatch(entry.entry_id));
        }
        Ok(index)
    }

    fn allocate_sequence(&mut self) -> Result<i64, RegistryError> {
        let sequence = self.next_seq;
        self.next_seq = sequence
            .checked_add(1)
            .ok_or(RegistryError::SequenceExhausted)?;
        Ok(sequence)
    }

    fn insert_workspace(&mut self, workspace: &VerifiedWorkspace) -> Result<usize, RegistryError> {
        let entry_id = self.allocate_sequence()?;
        let index = self.workspaces.len();
        self.workspaces.push(RegistryEntry {
            entry_id,
            root: workspace.root.clone(),
            identity: workspace.identity,
            last_used_seq: 0,
            recent_contests: Vec::new(),
        });
        Ok(index)
    }

    fn record_open(
        &mut self,
        index: usize,
        contest: Option<&VerifiedContestOpen<'_>>,
    ) -> Result<EntryRef, RegistryError> {
        let sequence = self.allocate_sequence()?;
        let entry = &mut self.workspaces[index];
        entry.last_used_seq = sequence;
        if let Some(contest) = contest {
            entry
                .recent_contests
                .retain(|recent| recent.contest_id != contest.contest_id);
            entry.recent_contests.push(RecentContest {
                contest_id: contest.contest_id.to_owned(),
                relative_path: contest.relative_path.to_owned(),
                last_used_seq: sequence,
            });
            entry
                .recent_contests
                .sort_unstable_by_key(|recent| Reverse(recent.last_used_seq));
            entry.recent_contests.truncate(MAX_RECENT_CONTESTS);
        }
        Ok(entry.entry_ref())
    }
}

impl RegistryEntry {
    pub(crate) fn entry_ref(&self) -> EntryRef {
        EntryRef {
            entry_id: self.entry_id,
            root: self.root.clone(),
            identity: self.identity,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        self.root.as_path()
    }

    pub(crate) fn identity(&self) -> Option<DirectoryIdentity> {
        self.identity
    }

    pub(crate) fn last_used_seq(&self) -> Option<i64> {
        (self.last_used_seq != 0).then_some(self.last_used_seq)
    }

    pub(crate) fn recent_contests_by_use(&self) -> Vec<&RecentContest> {
        let mut contests: Vec<_> = self.recent_contests.iter().collect();
        contests.sort_unstable_by_key(|contest| Reverse(contest.last_used_seq));
        contests
    }

    fn validate_recency(
        &self,
        next_seq: i64,
        allocated: &mut HashSet<i64>,
    ) -> Result<(), RegistryError> {
        if self.last_used_seq != 0
            && (self.last_used_seq <= self.entry_id || self.last_used_seq >= next_seq)
        {
            return Err(RegistryError::InvalidSequence(self.last_used_seq));
        }
        if self.recent_contests.len() > MAX_RECENT_CONTESTS {
            return Err(RegistryError::TooManyRecentContests(self.entry_id));
        }

        let mut contest_ids = HashSet::new();
        let mut latest_use_was_contest = false;
        for contest in &self.recent_contests {
            validate_contest_fields(&contest.contest_id, &contest.relative_path)?;
            if !contest_ids.insert(&contest.contest_id) {
                return Err(RegistryError::DuplicateContest(self.entry_id));
            }
            let sequence = contest.last_used_seq;
            if sequence <= self.entry_id
                || sequence > self.last_used_seq
                || !allocated.insert(sequence)
            {
                return Err(RegistryError::InvalidSequence(sequence));
            }
            latest_use_was_contest |= sequence == self.last_used_seq;
        }
        if self.last_used_seq != 0
            && !latest_use_was_contest
            && !allocated.insert(self.last_used_seq)
        {
            return Err(RegistryError::InvalidSequence(self.last_used_seq));
        }
        Ok(())
    }
}

impl RecentContest {
    pub(crate) fn contest_id(&self) -> &str {
        &self.contest_id
    }

    /// A routing comparison hint. Re-resolve and verify ownership before reopening.
    pub(crate) fn relative_path(&self) -> &str {
        &self.relative_path
    }

    pub(crate) fn last_used_seq(&self) -> i64 {
        self.last_used_seq
    }
}

impl EntryRef {
    pub(crate) fn entry_id(&self) -> i64 {
        self.entry_id
    }

    fn matches_entry(&self, entry: &RegistryEntry) -> bool {
        self.entry_id == entry.entry_id
            && self.root == entry.root
            && self.identity == entry.identity
    }
}

impl VerifiedWorkspace {
    /// The caller has checked the directory and Workspace marker at this exact root.
    /// Supply only identity observed in that check. This constructor performs no I/O.
    pub(crate) fn from_verified_root(
        root: PathBuf,
        identity: Option<DirectoryIdentity>,
    ) -> Result<Self, RegistryError> {
        Ok(Self {
            root: WorkspaceRoot::new(root).map_err(RegistryError::InvalidRoot)?,
            identity,
            alias_of: None,
        })
    }

    /// The caller must safely open and check both spellings of the directory for
    /// this operation. A stored identity alone may be stale or reused by the OS.
    pub(crate) fn with_verified_alias_of(
        mut self,
        registered: &EntryRef,
        observed_registered: &Self,
    ) -> Result<Self, RegistryError> {
        if self.root == registered.root
            || observed_registered.root != registered.root
            || self.identity.is_none()
            || self.identity != observed_registered.identity
            || !matches_observation(registered.identity, observed_registered.identity)
        {
            return Err(RegistryError::WorkspaceMismatch(registered.entry_id));
        }
        self.alias_of = Some(registered.clone());
        Ok(self)
    }
}

impl<'a> VerifiedContestOpen<'a> {
    /// The caller has already resolved the current route, checked ownership and
    /// started the session in this Workspace. Only portable path syntax is checked here.
    pub(crate) fn from_successful_open(
        workspace: &'a VerifiedWorkspace,
        contest_id: &'a str,
        relative_path: &'a str,
    ) -> Result<Self, RegistryError> {
        validate_contest_fields(contest_id, relative_path)?;
        Ok(Self {
            workspace,
            contest_id,
            relative_path,
        })
    }
}

fn matches_observation(
    stored: Option<DirectoryIdentity>,
    observed: Option<DirectoryIdentity>,
) -> bool {
    stored.is_none() || stored == observed
}

fn validate_contest_fields(contest_id: &str, relative_path: &str) -> Result<(), RegistryError> {
    validate_path_component(contest_id, "contest ID")
        .map_err(|_| RegistryError::InvalidContestId)?;
    if !relative_path
        .split('/')
        .all(|component| validate_workspace_path_component(component).is_ok())
    {
        return Err(RegistryError::InvalidContestPath);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
