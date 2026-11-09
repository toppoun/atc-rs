//! Lock-scoped persistence of Registry mutations. Callers do Workspace and
//! Contest verification before passing a mutation into this module.

mod files;

use std::fs::File;
use std::io::{self, Write};
use std::time::Duration;

use crate::paths::RegistryLocation;

use super::{
    DirectoryIdentity, RegistryError, RegistryMutation, RegistryMutationResult, RegistrySnapshot,
};
use files::{FileSnapshot, MAX_REGISTRY_BYTES, StateDirectory, preserve_metadata};

const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) enum RegistryLoad {
    Missing,
    Snapshot(RegistrySnapshot),
}

#[derive(Debug)]
pub(crate) enum RegistryApply {
    Applied {
        snapshot: RegistrySnapshot,
        result: RegistryMutationResult,
    },
    NoChange {
        snapshot: RegistrySnapshot,
        result: RegistryMutationResult,
    },
    StaleEntry {
        entry_id: i64,
    },
}

#[derive(Debug)]
pub(crate) enum RegistryReadError {
    Io {
        target: &'static str,
        source: io::Error,
    },
    UnsafeType(&'static str),
    ChangedDuringRead,
    RecoveryRequired,
    TooLarge,
    InvalidUtf8,
    InvalidToml,
    InvalidFields,
    InvalidSchema(RegistryError),
    UnsupportedVersion(i64),
}

#[derive(Debug)]
pub(crate) enum RegistryLockError {
    Open(RegistryReadError),
    Acquire(io::Error),
    TimedOut,
}

/// A read-back observation. The error still describes a failed filesystem step.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegistrySaveState {
    CandidatePresent,
    BaselinePresent,
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegistryPublishStep {
    Replace,
    Cleanup,
    SyncDirectory,
    Verify,
}

#[derive(Debug)]
pub(crate) enum RegistryWriteError {
    Read(RegistryReadError),
    Lock(RegistryLockError),
    Mutation(RegistryError),
    Conflict,
    BeforePublish(io::Error),
    AfterPublish {
        step: RegistryPublishStep,
        source: io::Error,
        observed: RegistrySaveState,
    },
}

impl std::fmt::Display for RegistryReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { target, source } => write!(f, "cannot access {target}: {source}"),
            Self::UnsafeType(target) => write!(f, "unsafe file type for {target}"),
            Self::ChangedDuringRead => f.write_str("Workspace Registry changed while reading"),
            Self::RecoveryRequired => {
                f.write_str("Workspace Registry is missing with a staged replacement still present")
            }
            Self::TooLarge => f.write_str("Workspace Registry exceeds the safe read limit"),
            Self::InvalidUtf8 => f.write_str("Workspace Registry is not UTF-8"),
            Self::InvalidToml => f.write_str("invalid Workspace Registry TOML"),
            Self::InvalidFields => f.write_str("invalid Workspace Registry fields"),
            Self::InvalidSchema(error) => write!(f, "invalid Workspace Registry data: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported Workspace Registry version {version}")
            }
        }
    }
}

impl std::error::Error for RegistryReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidSchema(error) => Some(error),
            _ => None,
        }
    }
}

impl std::fmt::Display for RegistryLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(error) => write!(f, "cannot open Workspace Registry lock: {error}"),
            Self::Acquire(error) => write!(f, "cannot acquire Workspace Registry lock: {error}"),
            Self::TimedOut => f.write_str("timed out waiting for Workspace Registry lock"),
        }
    }
}

impl std::error::Error for RegistryLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open(error) => Some(error),
            Self::Acquire(error) => Some(error),
            Self::TimedOut => None,
        }
    }
}

impl std::fmt::Display for RegistryWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(f, "cannot read Workspace Registry for update: {error}"),
            Self::Lock(error) => error.fmt(f),
            Self::Mutation(error) => error.fmt(f),
            Self::Conflict => f.write_str("Workspace Registry changed before publication"),
            Self::BeforePublish(error) => {
                write!(f, "cannot prepare Workspace Registry update: {error}")
            }
            Self::AfterPublish {
                step,
                source,
                observed,
            } => write!(
                f,
                "Workspace Registry publication error ({step:?}, {observed:?}): {source}"
            ),
        }
    }
}

impl std::error::Error for RegistryWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Lock(error) => Some(error),
            Self::Mutation(error) => Some(error),
            Self::BeforePublish(error) | Self::AfterPublish { source: error, .. } => Some(error),
            Self::Conflict => None,
        }
    }
}

pub(crate) struct RegistryStore {
    location: RegistryLocation,
}

impl RegistryStore {
    pub(crate) fn new(location: RegistryLocation) -> Self {
        Self { location }
    }

    /// Read an atomic snapshot without creating the state directory or lock.
    pub(crate) fn load(&self) -> Result<RegistryLoad, RegistryReadError> {
        let Some(state) = StateDirectory::open(&self.location, false)? else {
            return Ok(RegistryLoad::Missing);
        };
        state.check_lock_type()?;
        match state.read_baseline()? {
            Some(snapshot) => decode(&snapshot.bytes).map(RegistryLoad::Snapshot),
            None => Ok(RegistryLoad::Missing),
        }
    }

    pub(crate) fn apply(
        &self,
        mutation: RegistryMutation<'_>,
    ) -> Result<RegistryApply, RegistryWriteError> {
        self.apply_with(mutation, LOCK_TIMEOUT, |_| Ok(()))
    }

    fn apply_with(
        &self,
        mutation: RegistryMutation<'_>,
        timeout: Duration,
        mut hook: impl FnMut(WriteStep) -> io::Result<()>,
    ) -> Result<RegistryApply, RegistryWriteError> {
        use RegistryWriteError as Error;

        let state = StateDirectory::open(&self.location, true)
            .map_err(Error::Read)?
            .ok_or(Error::Conflict)?;
        let lock = state.lock(timeout).map_err(Error::Lock)?;
        self.verify_lock(&state, &lock)?;

        let mut retry_create_collision = true;
        loop {
            let baseline = state.read_baseline().map_err(Error::Read)?;
            let snapshot = baseline
                .as_ref()
                .map(|file| decode(&file.bytes))
                .transpose()
                .map_err(Error::Read)?
                .unwrap_or_default();
            let (updated, result) = match snapshot.apply(mutation) {
                Err(RegistryError::StaleEntry(entry_id)) => {
                    return Ok(RegistryApply::StaleEntry { entry_id });
                }
                Err(error) => return Err(Error::Mutation(error)),
                Ok(result) => result,
            };
            if updated == snapshot {
                self.verify_lock(&state, &lock)?;
                if !same_baseline(
                    baseline.as_ref(),
                    state.read_snapshot().map_err(Error::Read)?.as_ref(),
                ) {
                    return Err(Error::Conflict);
                }
                return Ok(RegistryApply::NoChange { snapshot, result });
            }

            let candidate = updated.to_toml().map_err(Error::Mutation)?.into_bytes();
            validate_candidate(&candidate, &updated).map_err(Error::BeforePublish)?;
            let (mut staged, mut staged_file) = state
                .stage(baseline.is_some())
                .map_err(Error::BeforePublish)?;
            staged_file
                .write_all(&candidate)
                .map_err(Error::BeforePublish)?;
            if let Some(baseline) = &baseline {
                preserve_metadata(baseline, &staged_file).map_err(Error::BeforePublish)?;
            }
            hook(WriteStep::SyncStaged)
                .and_then(|()| staged_file.sync_all())
                .map_err(Error::BeforePublish)?;
            #[cfg(windows)]
            let _stage_guard =
                files::stage_identity_guard(staged_file).map_err(Error::BeforePublish)?;
            #[cfg(unix)]
            let _stage_guard = staged_file;

            hook(WriteStep::BeforeRecheck).map_err(Error::BeforePublish)?;
            self.verify_lock(&state, &lock)?;
            let current = state.read_snapshot().map_err(Error::Read)?;
            if !same_baseline(baseline.as_ref(), current.as_ref()) {
                if baseline.is_none() && std::mem::take(&mut retry_create_collision) {
                    continue;
                }
                return Err(Error::Conflict);
            }
            if !staged.is_current().map_err(Error::Read)? {
                return Err(Error::Conflict);
            }
            hook(WriteStep::BeforePublish).map_err(Error::BeforePublish)?;

            let published = staged.publish(baseline.is_some());
            if baseline.is_none()
                && published
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists)
            {
                if std::mem::take(&mut retry_create_collision) {
                    continue;
                }
                return Err(Error::Conflict);
            }
            let commit = published
                .map_err(|source| (RegistryPublishStep::Replace, source))
                .and_then(|()| {
                    staged
                        .cleanup()
                        .map_err(|source| (RegistryPublishStep::Cleanup, source))
                })
                .and_then(|()| {
                    hook(WriteStep::AfterPublish)
                        .map_err(|source| (RegistryPublishStep::Replace, source))
                })
                .and_then(|()| {
                    hook(WriteStep::SyncDirectory)
                        .and_then(|()| state.sync())
                        .map_err(|source| (RegistryPublishStep::SyncDirectory, source))
                });
            let observed = self
                .verify_lock(&state, &lock)
                .and_then(|()| state.read_snapshot().map_err(Error::Read));
            let candidate_present = matches!(&observed, Ok(Some(file)) if file.identity == staged.identity && file.bytes == candidate);
            if let Err((step, source)) = commit {
                let observed = if candidate_present {
                    RegistrySaveState::CandidatePresent
                } else if matches!(&observed, Ok(file) if same_baseline(baseline.as_ref(), file.as_ref()))
                {
                    RegistrySaveState::BaselinePresent
                } else {
                    staged.preserve();
                    RegistrySaveState::Unknown
                };
                return Err(Error::AfterPublish {
                    step,
                    source,
                    observed,
                });
            }
            let saved = match observed {
                Ok(Some(saved)) if candidate_present => saved,
                observed => {
                    staged.preserve();
                    return Err(Error::AfterPublish {
                        step: RegistryPublishStep::Verify,
                        source: observed.err().map_or_else(
                            || {
                                io::Error::other(
                                    "could not confirm the published Registry candidate",
                                )
                            },
                            io::Error::other,
                        ),
                        observed: RegistrySaveState::Unknown,
                    });
                }
            };
            if baseline
                .as_ref()
                .is_some_and(|baseline| baseline.metadata != saved.metadata)
            {
                return Err(Error::AfterPublish {
                    step: RegistryPublishStep::Verify,
                    source: io::Error::other("Registry metadata differs after replacement"),
                    observed: RegistrySaveState::CandidatePresent,
                });
            }
            return Ok(RegistryApply::Applied {
                snapshot: updated,
                result,
            });
        }
    }

    fn verify_lock(&self, state: &StateDirectory, lock: &File) -> Result<(), RegistryWriteError> {
        if state
            .is_current(&self.location)
            .map_err(RegistryWriteError::Read)?
            && state
                .lock_is_current(lock)
                .map_err(RegistryWriteError::Read)?
        {
            Ok(())
        } else {
            Err(RegistryWriteError::Conflict)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteStep {
    SyncStaged,
    BeforeRecheck,
    BeforePublish,
    AfterPublish,
    SyncDirectory,
}

fn same_baseline(before: Option<&FileSnapshot>, now: Option<&FileSnapshot>) -> bool {
    match (before, now) {
        (Some(before), Some(now)) => before.matches(now),
        (None, None) => true,
        _ => false,
    }
}

fn decode(bytes: &[u8]) -> Result<RegistrySnapshot, RegistryReadError> {
    let source = std::str::from_utf8(bytes).map_err(|_| RegistryReadError::InvalidUtf8)?;
    // TOML diagnostics may embed arbitrary on-disk contents.
    let table = toml::from_str(source).map_err(|_| RegistryReadError::InvalidToml)?;
    RegistrySnapshot::from_toml_table(table).map_err(|error| match error {
        RegistryError::Decode(_) => RegistryReadError::InvalidFields,
        RegistryError::UnsupportedVersion(version) => {
            RegistryReadError::UnsupportedVersion(version)
        }
        error => RegistryReadError::InvalidSchema(error),
    })
}

fn validate_candidate(bytes: &[u8], expected: &RegistrySnapshot) -> io::Result<()> {
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(io::Error::other(RegistryReadError::TooLarge));
    }
    if decode(bytes).map_err(io::Error::other)? != *expected {
        return Err(io::Error::other("Registry candidate does not round-trip"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
