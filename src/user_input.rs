use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
#[cfg(windows)]
use cap_std::fs::MetadataExt as _;
use cap_std::fs::{Dir as CapDir, OpenOptions as CapOpenOptions};
use serde::{Deserialize, Serialize};

use crate::workspace;

#[cfg(windows)]
mod windows_publish;

const FORMAT_VERSION: u32 = 1;
const USER_INPUTS_DIRECTORY: &str = "user-inputs";
const METADATA_FILE: &str = "meta.toml";
const STAGING_PREFIX: &str = ".user-input-staging-";

static NEXT_STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn io_context(error: io::Error, context: impl Into<String>) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", context.into()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserInput {
    pub(crate) id: u64,
    pub(crate) content: String,
}

#[derive(Debug)]
pub(crate) enum UserInputCreateError {
    // This attempt did not install its input. Directories, staging, and the
    // metadata high-water reservation may already have changed; retry reads disk truth.
    BeforeInstall(io::Error),
    // The input was installed after its ID was reserved in metadata.
    AfterInstall { id: u64, error: io::Error },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserInputSaveOutcome {
    Saved,
    Unchanged,
    Conflict,
    Missing,
}

impl UserInputCreateError {
    fn into_io_error(self) -> io::Error {
        match self {
            Self::BeforeInstall(error) | Self::AfterInstall { error, .. } => error,
        }
    }
}

impl std::fmt::Display for UserInputCreateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeInstall(error) => error.fmt(formatter),
            Self::AfterInstall { id, error } => {
                write!(
                    formatter,
                    "user input {id} was installed, but finalization failed: {error}"
                )
            }
        }
    }
}

impl std::error::Error for UserInputCreateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeforeInstall(error) | Self::AfterInstall { error, .. } => Some(error),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UserInputMetadata {
    version: u32,
    next_id: u64,
}

pub(crate) fn load_user_inputs(
    destination: &Path,
    problem_index: &str,
) -> io::Result<Vec<UserInput>> {
    load_user_inputs_with_hooks(destination, problem_index, || Ok(()), || Ok(()), || Ok(()))
}

fn load_user_inputs_with_hooks(
    destination: &Path,
    problem_index: &str,
    after_destination_validation: impl FnOnce() -> io::Result<()>,
    after_missing_lock: impl FnOnce() -> io::Result<()>,
    after_optimistic_read: impl FnOnce() -> io::Result<()>,
) -> io::Result<Vec<UserInput>> {
    validate_request(problem_index)?;

    let marker = open_workspace_marker_after(destination, after_destination_validation)?;
    let Some(root) =
        open_optional_real_directory(&marker, USER_INPUTS_DIRECTORY, "user input root directory")?
    else {
        return Ok(Vec::new());
    };

    let lock = open_problem_lock(&root, problem_index, false)?;
    if let Some(lock) = lock.as_ref() {
        lock.lock_shared()?;
        return load_problem_user_inputs(&root, problem_index);
    }

    after_missing_lock()?;
    let optimistic_result = load_problem_user_inputs(&root, problem_index);
    after_optimistic_read()?;

    let Some(lock) = open_problem_lock(&root, problem_index, false)? else {
        return optimistic_result;
    };
    lock.lock_shared()?;
    load_problem_user_inputs(&root, problem_index)
}

fn load_problem_user_inputs(root: &CapDir, problem_index: &str) -> io::Result<Vec<UserInput>> {
    let Some(problem) =
        open_optional_real_directory(root, problem_index, "user input problem directory")?
    else {
        return Ok(Vec::new());
    };

    let snapshot = scan_problem_directory(&problem, true)?;
    read_metadata(&problem)?;
    Ok(snapshot.inputs)
}

pub(crate) fn create_user_input(
    destination: &Path,
    problem_index: &str,
    content: &str,
) -> io::Result<u64> {
    create_user_input_with_outcome(destination, problem_index, content)
        .map_err(UserInputCreateError::into_io_error)
}

pub(crate) fn create_user_input_with_outcome(
    destination: &Path,
    problem_index: &str,
    content: &str,
) -> Result<u64, UserInputCreateError> {
    create_user_input_with_outcome_and_hooks(
        destination,
        problem_index,
        content,
        || Ok(()),
        || Ok(()),
        || Ok(()),
    )
}

fn create_user_input_with_hooks(
    destination: &Path,
    problem_index: &str,
    content: &str,
    after_destination_validation: impl FnOnce() -> io::Result<()>,
    before_input_install: impl FnOnce() -> io::Result<()>,
    after_input_install: impl FnOnce() -> io::Result<()>,
) -> io::Result<u64> {
    create_user_input_with_outcome_and_hooks(
        destination,
        problem_index,
        content,
        after_destination_validation,
        before_input_install,
        after_input_install,
    )
    .map_err(UserInputCreateError::into_io_error)
}

fn create_user_input_with_outcome_and_hooks(
    destination: &Path,
    problem_index: &str,
    content: &str,
    after_destination_validation: impl FnOnce() -> io::Result<()>,
    before_input_install: impl FnOnce() -> io::Result<()>,
    after_input_install: impl FnOnce() -> io::Result<()>,
) -> Result<u64, UserInputCreateError> {
    create_user_input_with_reservation(
        destination,
        problem_index,
        content,
        after_destination_validation,
        before_input_install,
        after_input_install,
        persist_metadata,
    )
}

#[cfg(test)]
pub(crate) fn create_user_input_with_after_install_hook(
    destination: &Path,
    problem_index: &str,
    content: &str,
    after_input_install: impl FnOnce() -> io::Result<()>,
) -> Result<u64, UserInputCreateError> {
    create_user_input_with_outcome_and_hooks(
        destination,
        problem_index,
        content,
        || Ok(()),
        || Ok(()),
        after_input_install,
    )
}

fn create_user_input_with_reservation(
    destination: &Path,
    problem_index: &str,
    content: &str,
    after_destination_validation: impl FnOnce() -> io::Result<()>,
    before_input_install: impl FnOnce() -> io::Result<()>,
    after_input_install: impl FnOnce() -> io::Result<()>,
    reserve: impl FnOnce(&CapDir, UserInputMetadata, bool) -> io::Result<()>,
) -> Result<u64, UserInputCreateError> {
    validate_request(problem_index).map_err(UserInputCreateError::BeforeInstall)?;

    let marker = open_workspace_marker_after(destination, after_destination_validation)
        .map_err(UserInputCreateError::BeforeInstall)?;
    let root =
        ensure_real_child_directory(&marker, USER_INPUTS_DIRECTORY, "user input root directory")
            .map_err(UserInputCreateError::BeforeInstall)?;
    let lock = open_problem_lock(&root, problem_index, true)
        .map_err(UserInputCreateError::BeforeInstall)?
        .expect("create was requested for the user input lock");
    lock.lock()
        .map_err(|error| io_context(error, "failed to lock user input storage for create"))
        .map_err(UserInputCreateError::BeforeInstall)?;

    let problem = ensure_real_child_directory(&root, problem_index, "user input problem directory")
        .map_err(UserInputCreateError::BeforeInstall)?;
    let snapshot =
        scan_problem_directory(&problem, false).map_err(UserInputCreateError::BeforeInstall)?;
    let stored_metadata = read_metadata(&problem).map_err(UserInputCreateError::BeforeInstall)?;
    let next_id = effective_next_id(stored_metadata, snapshot.max_id)
        .map_err(UserInputCreateError::BeforeInstall)?;
    let following_id = next_id
        .checked_add(1)
        .ok_or_else(id_space_exhausted)
        .map_err(UserInputCreateError::BeforeInstall)?;
    let input_name = input_file_name(next_id).map_err(UserInputCreateError::BeforeInstall)?;

    let staged = StagedFile::for_install(&problem, "input", content.as_bytes())
        .map_err(UserInputCreateError::BeforeInstall)?;

    // Burn this ID before publishing the input. A later failed publish leaves a
    // deliberate gap, and deleting an installed input can never erase this reservation.
    // Even a reservation error may have advanced metadata before its sync failed.
    reserve(
        &problem,
        UserInputMetadata {
            version: FORMAT_VERSION,
            next_id: following_id,
        },
        stored_metadata.is_some(),
    )
    .map_err(UserInputCreateError::BeforeInstall)?;
    before_input_install().map_err(UserInputCreateError::BeforeInstall)?;
    staged
        .install_noclobber_with_outcome(&input_name)
        .map_err(|error| match error {
            StagedInstallError::BeforeInstall(error) => UserInputCreateError::BeforeInstall(error),
            StagedInstallError::AfterInstall(error) => {
                UserInputCreateError::AfterInstall { id: next_id, error }
            }
        })?;

    let after_install = |error| UserInputCreateError::AfterInstall { id: next_id, error };
    after_input_install().map_err(after_install)?;
    Ok(next_id)
}

/// Compare and (only on an exact match) replace under the same exclusive lock.
/// Unchanged still verifies disk content, but never stages or writes input/metadata.
/// This coordinates lock-respecting writers, not arbitrary lock-ignoring mutations.
pub(crate) fn save_user_input_if_unchanged(
    destination: &Path,
    problem_index: &str,
    id: u64,
    expected_content: &str,
    content: &str,
) -> io::Result<UserInputSaveOutcome> {
    save_user_input_if_unchanged_with_hooks(
        destination,
        problem_index,
        id,
        expected_content,
        content,
        || Ok(()),
        || Ok(()),
    )
}

fn save_user_input_if_unchanged_with_hooks(
    destination: &Path,
    problem_index: &str,
    id: u64,
    expected_content: &str,
    content: &str,
    before_lock: impl FnOnce() -> io::Result<()>,
    before_replace: impl FnOnce() -> io::Result<()>,
) -> io::Result<UserInputSaveOutcome> {
    validate_request(problem_index)?;
    let input_name = input_file_name(id)?;
    let (root, problem) = match open_existing_problem(destination, problem_index, || Ok(())) {
        Ok(opened) => opened,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(UserInputSaveOutcome::Missing);
        }
        Err(error) => return Err(error),
    };
    drop(problem);
    let lock = open_problem_lock(&root, problem_index, true)?
        .expect("create was requested for the user input lock");
    before_lock()?;
    lock.lock()
        .map_err(|error| io_context(error, "failed to lock user input storage for checked save"))?;

    let problem = match reopen_existing_problem(&root, problem_index) {
        Ok(problem) => problem,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(UserInputSaveOutcome::Missing);
        }
        Err(error) => return Err(error),
    };
    let Some(mut target) = open_regular_file(&problem, &input_name, "user input")? else {
        return Ok(UserInputSaveOutcome::Missing);
    };
    let mut current = String::new();
    target.read_to_string(&mut current).map_err(|error| {
        io_context(
            error,
            format!("failed to read user input {id} as UTF-8 for checked save"),
        )
    })?;
    drop(target);
    if current != expected_content {
        return Ok(UserInputSaveOutcome::Conflict);
    }
    if content == expected_content {
        return Ok(UserInputSaveOutcome::Unchanged);
    }

    // `lock` remains live through comparison, metadata reconciliation and replacement.
    replace_user_input(&problem, &input_name, content, before_replace, || Ok(()))?;
    Ok(UserInputSaveOutcome::Saved)
}

pub(crate) fn save_user_input(
    destination: &Path,
    problem_index: &str,
    id: u64,
    content: &str,
) -> io::Result<()> {
    save_user_input_with_hooks(
        destination,
        problem_index,
        id,
        content,
        || Ok(()),
        || Ok(()),
        || Ok(()),
    )
}

fn save_user_input_with_hook(
    destination: &Path,
    problem_index: &str,
    id: u64,
    content: &str,
    before_replace: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    save_user_input_with_hooks(
        destination,
        problem_index,
        id,
        content,
        || Ok(()),
        before_replace,
        || Ok(()),
    )
}

fn save_user_input_with_hooks(
    destination: &Path,
    problem_index: &str,
    id: u64,
    content: &str,
    after_destination_validation: impl FnOnce() -> io::Result<()>,
    before_replace: impl FnOnce() -> io::Result<()>,
    after_replace: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    validate_request(problem_index)?;
    let input_name = input_file_name(id)?;
    let (root, problem) =
        open_existing_problem(destination, problem_index, after_destination_validation)?;
    drop(problem);
    let lock = open_problem_lock(&root, problem_index, true)?
        .expect("create was requested for the user input lock");
    lock.lock()
        .map_err(|error| io_context(error, "failed to lock user input storage for save"))?;

    let problem = reopen_existing_problem(&root, problem_index)?;
    let target = open_regular_file(&problem, &input_name, "user input")?
        .ok_or_else(|| missing_input_error(problem_index, id))?;
    drop(target);

    replace_user_input(
        &problem,
        &input_name,
        content,
        before_replace,
        after_replace,
    )
}

// Both save APIs call this only while retaining their exclusive problem lock.
fn replace_user_input(
    problem: &CapDir,
    input_name: &OsStr,
    content: &str,
    before_replace: impl FnOnce() -> io::Result<()>,
    after_replace: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    reconcile_metadata(problem)?;
    let staged = StagedFile::new(problem, "input", content.as_bytes())?;
    before_replace()?;
    staged.replace_with_hook(input_name, after_replace)
}

pub(crate) fn delete_user_input(
    destination: &Path,
    problem_index: &str,
    id: u64,
) -> io::Result<()> {
    delete_user_input_with_hook(destination, problem_index, id, || Ok(()))
}

fn delete_user_input_with_hook(
    destination: &Path,
    problem_index: &str,
    id: u64,
    after_destination_validation: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    validate_request(problem_index)?;
    let input_name = input_file_name(id)?;
    let (root, problem) =
        open_existing_problem(destination, problem_index, after_destination_validation)?;
    drop(problem);
    let lock = open_problem_lock(&root, problem_index, true)?
        .expect("create was requested for the user input lock");
    lock.lock()?;

    let problem = reopen_existing_problem(&root, problem_index)?;
    let target = open_regular_file(&problem, &input_name, "user input")?
        .ok_or_else(|| missing_input_error(problem_index, id))?;
    drop(target);

    // Persist the high-water mark before removing the maximum ID. A crash after this point can
    // leave the input present, but cannot make its ID reusable.
    reconcile_metadata(&problem)?;
    problem.remove_file(&input_name)?;
    sync_directory(&problem)
}

fn validate_request(problem_index: &str) -> io::Result<()> {
    workspace::validate_problem_index(problem_index)
}

fn open_workspace_marker_after(
    destination: &Path,
    after_validation: impl FnOnce() -> io::Result<()>,
) -> io::Result<CapDir> {
    let workspace =
        workspace::open_validated_contest_destination_after(destination, after_validation)?;
    open_optional_real_directory(&workspace, ".atc", "workspace marker")?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "workspace marker disappeared: {}",
                destination.join(".atc").display()
            ),
        )
    })
}

fn open_existing_problem(
    destination: &Path,
    problem_index: &str,
    after_destination_validation: impl FnOnce() -> io::Result<()>,
) -> io::Result<(CapDir, CapDir)> {
    let marker = open_workspace_marker_after(destination, after_destination_validation)?;
    let root =
        open_optional_real_directory(&marker, USER_INPUTS_DIRECTORY, "user input root directory")?
            .ok_or_else(|| missing_input_problem_error(problem_index))?;
    let problem =
        open_optional_real_directory(&root, problem_index, "user input problem directory")?
            .ok_or_else(|| missing_input_problem_error(problem_index))?;
    Ok((root, problem))
}

fn reopen_existing_problem(root: &CapDir, problem_index: &str) -> io::Result<CapDir> {
    open_optional_real_directory(root, problem_index, "user input problem directory")?
        .ok_or_else(|| missing_input_problem_error(problem_index))
}

fn open_optional_real_directory(
    parent: &CapDir,
    name: &str,
    kind: &str,
) -> io::Result<Option<CapDir>> {
    let metadata = match parent.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_real_directory_metadata(&metadata, name, kind)?;

    let directory = parent.open_dir_nofollow(name)?;
    let metadata = directory.dir_metadata()?;
    validate_real_directory_metadata(&metadata, name, kind)?;
    Ok(Some(directory))
}

fn ensure_real_child_directory(parent: &CapDir, name: &str, kind: &str) -> io::Result<CapDir> {
    if let Some(directory) = open_optional_real_directory(parent, name, kind)? {
        return Ok(directory);
    }

    match parent.create_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    open_optional_real_directory(parent, name, kind)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{kind} disappeared after creation: {name}"),
        )
    })
}

fn validate_real_directory_metadata(
    metadata: &cap_std::fs::Metadata,
    name: &str,
    kind: &str,
) -> io::Result<()> {
    if metadata_is_reparse(metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is a reparse point: {name}"),
        ));
    }
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a real directory: {name}"),
        ));
    }
    Ok(())
}

fn open_problem_lock(
    root: &CapDir,
    problem_index: &str,
    create: bool,
) -> io::Result<Option<fs::File>> {
    let name = format!(".{problem_index}.lock");
    match root.symlink_metadata(&name) {
        Ok(metadata) => {
            validate_regular_file_metadata(&metadata, OsStr::new(&name), "user input lock")?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    if create {
        options.write(true).create(true).truncate(false);
    }

    let file = match root.open_with(&name, &options) {
        Ok(file) => file,
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_regular_file_metadata(&file.metadata()?, OsStr::new(&name), "user input lock")?;
    Ok(Some(file.into_std()))
}

struct ProblemSnapshot {
    inputs: Vec<UserInput>,
    max_id: Option<u64>,
}

fn scan_problem_directory(problem: &CapDir, read_contents: bool) -> io::Result<ProblemSnapshot> {
    let mut inputs = Vec::new();
    let mut max_id = None;

    for entry in problem.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = parse_input_file_name(&name) else {
            continue;
        };
        max_id = Some(max_id.map_or(id, |current: u64| current.max(id)));

        let Some(mut file) = open_regular_file(problem, &name, "user input")? else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "user input disappeared while loading: {}",
                    name.to_string_lossy()
                ),
            ));
        };
        if read_contents {
            let mut content = String::new();
            file.read_to_string(&mut content).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "failed to read user input {} as UTF-8: {error}",
                        name.to_string_lossy()
                    ),
                )
            })?;
            inputs.push(UserInput { id, content });
        }
    }

    inputs.sort_by_key(|input| input.id);
    Ok(ProblemSnapshot { inputs, max_id })
}

fn open_regular_file(directory: &CapDir, name: &OsStr, kind: &str) -> io::Result<Option<fs::File>> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_regular_file_metadata(&metadata, name, kind)?;

    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_regular_file_metadata(&file.metadata()?, name, kind)?;
    Ok(Some(file.into_std()))
}

fn validate_regular_file_metadata(
    metadata: &cap_std::fs::Metadata,
    name: &OsStr,
    kind: &str,
) -> io::Result<()> {
    if metadata_is_reparse(metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is a reparse point: {}", name.to_string_lossy()),
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a regular file: {}", name.to_string_lossy()),
        ));
    }
    Ok(())
}

fn read_metadata(problem: &CapDir) -> io::Result<Option<UserInputMetadata>> {
    let Some(mut file) =
        open_regular_file(problem, OsStr::new(METADATA_FILE), "user input metadata")?
    else {
        return Ok(None);
    };

    let mut content = String::new();
    file.read_to_string(&mut content).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read user input metadata as UTF-8: {error}"),
        )
    })?;
    let metadata: UserInputMetadata = toml::from_str(&content).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid user input metadata: {error}"),
        )
    })?;
    if metadata.version != FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported user input metadata version: {}",
                metadata.version
            ),
        ));
    }
    if metadata.next_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "user input metadata next_id must be at least 1",
        ));
    }
    Ok(Some(metadata))
}

fn effective_next_id(
    metadata: Option<UserInputMetadata>,
    max_existing_id: Option<u64>,
) -> io::Result<u64> {
    let after_existing = match max_existing_id {
        Some(id) => id.checked_add(1).ok_or_else(id_space_exhausted)?,
        None => 1,
    };
    Ok(metadata
        .map(|metadata| metadata.next_id.max(after_existing))
        .unwrap_or(after_existing))
}

fn reconcile_metadata(problem: &CapDir) -> io::Result<()> {
    let snapshot = scan_problem_directory(problem, false)?;
    let stored = read_metadata(problem)?;
    let next_id = effective_next_id(stored, snapshot.max_id)?;
    if stored.is_some_and(|metadata| metadata.next_id == next_id) {
        return Ok(());
    }

    persist_metadata(
        problem,
        UserInputMetadata {
            version: FORMAT_VERSION,
            next_id,
        },
        stored.is_some(),
    )
}

fn persist_metadata(
    problem: &CapDir,
    metadata: UserInputMetadata,
    replace_existing: bool,
) -> io::Result<()> {
    let content = toml::to_string_pretty(&metadata).map_err(io::Error::other)?;
    let staged = if replace_existing {
        StagedFile::new(problem, "metadata", content.as_bytes())?
    } else {
        StagedFile::for_install(problem, "metadata", content.as_bytes())?
    };
    if replace_existing {
        let existing =
            open_regular_file(problem, OsStr::new(METADATA_FILE), "user input metadata")?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "user input metadata disappeared before replacement",
                    )
                })?;
        drop(existing);
        staged.replace(OsStr::new(METADATA_FILE))
    } else {
        staged.install_noclobber(OsStr::new(METADATA_FILE))
    }
}

struct StagedFile {
    directory: CapDir,
    name: OsString,
    // Only no-clobber publish retains the original DELETE-capable handle.
    // Replacement staging keeps its existing write/sync/close sequence.
    #[cfg(windows)]
    publish_source: Option<fs::File>,
}

#[derive(Debug)]
enum StagedInstallError {
    BeforeInstall(io::Error),
    AfterInstall(io::Error),
}

impl StagedFile {
    fn new(directory: &CapDir, purpose: &str, content: &[u8]) -> io::Result<Self> {
        Self::new_with_publish_handle(directory, purpose, content, false)
    }

    fn for_install(directory: &CapDir, purpose: &str, content: &[u8]) -> io::Result<Self> {
        Self::new_with_publish_handle(directory, purpose, content, cfg!(windows))
    }

    fn new_with_publish_handle(
        directory: &CapDir,
        purpose: &str,
        content: &[u8],
        _retain_publish_handle: bool,
    ) -> io::Result<Self> {
        let directory = directory.try_clone().map_err(|error| {
            io_context(
                error,
                format!("failed to clone directory handle for staged {purpose} file"),
            )
        })?;
        for _ in 0..128 {
            let sequence = NEXT_STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = OsString::from(format!(
                "{STAGING_PREFIX}{purpose}-{}-{sequence}",
                std::process::id()
            ));
            let mut options = CapOpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No)
                .nonblock(true);
            #[cfg(windows)]
            if _retain_publish_handle {
                windows_publish::configure_source(&mut options);
            }
            let file = match directory.open_with(&name, &options) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(io_context(
                        error,
                        format!(
                            "failed to create staged {purpose} file {}",
                            name.to_string_lossy()
                        ),
                    ));
                }
            };
            let staged = Self {
                directory,
                name,
                #[cfg(windows)]
                publish_source: None,
            };
            #[cfg(windows)]
            if _retain_publish_handle {
                let validated = file.metadata().and_then(|metadata| {
                    validate_regular_file_metadata(&metadata, &staged.name, "staged publish source")
                });
                if let Err(error) = validated {
                    drop(file);
                    return Err(io_context(
                        error,
                        "failed to validate staged publish source",
                    ));
                }
            }
            let mut file = file.into_std();
            if let Err(error) = file.write_all(content) {
                drop(file);
                let error = io_context(
                    error,
                    format!(
                        "failed to write staged {purpose} file {}",
                        staged.name.to_string_lossy()
                    ),
                );
                drop(staged);
                return Err(error);
            }
            if let Err(error) = file.sync_all() {
                drop(file);
                let error = io_context(
                    error,
                    format!(
                        "failed to sync staged {purpose} file {}",
                        staged.name.to_string_lossy()
                    ),
                );
                drop(staged);
                return Err(error);
            }
            #[cfg(windows)]
            if _retain_publish_handle {
                let mut staged = staged;
                staged.publish_source = Some(file);
                return Ok(staged);
            }
            drop(file);
            return Ok(staged);
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique user input staging file",
        ))
    }

    fn install_noclobber(self, destination: &OsStr) -> io::Result<()> {
        self.install_noclobber_with_outcome(destination)
            .map_err(|error| match error {
                StagedInstallError::BeforeInstall(error)
                | StagedInstallError::AfterInstall(error) => error,
            })
    }

    fn install_noclobber_with_outcome(self, destination: &OsStr) -> Result<(), StagedInstallError> {
        self.install_noclobber_with_sync(destination, sync_directory)
    }

    fn install_noclobber_with_sync(
        mut self,
        destination: &OsStr,
        sync: impl FnOnce(&CapDir) -> io::Result<()>,
    ) -> Result<(), StagedInstallError> {
        #[cfg(windows)]
        {
            let source = self.publish_source.as_ref().ok_or_else(|| {
                StagedInstallError::BeforeInstall(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "no-clobber publish requires its original staging handle",
                ))
            })?;
            windows_publish::publish(&self.directory, source, destination)
                .map_err(|error| {
                    io_context(
                        error,
                        format!(
                            "failed to publish staged file {} as {} without replacement",
                            self.name.to_string_lossy(),
                            destination.to_string_lossy()
                        ),
                    )
                })
                .map_err(StagedInstallError::BeforeInstall)?;
            // Native rename success is the commit point. It consumes the staging name;
            // never unlink an alias or undo the published destination after this point.
            self.name.clear();
            drop(self.publish_source.take());
        }
        #[cfg(not(windows))]
        {
            self.directory
                .hard_link(&self.name, &self.directory, destination)
                .map_err(|error| {
                    io_context(
                        error,
                        format!(
                            "failed to hard-link staged file {} as {}",
                            self.name.to_string_lossy(),
                            destination.to_string_lossy()
                        ),
                    )
                })
                .map_err(StagedInstallError::BeforeInstall)?;
            self.directory
                .remove_file(&self.name)
                .map_err(|error| {
                    io_context(
                        error,
                        format!(
                            "failed to remove staging file {} after installing {}",
                            self.name.to_string_lossy(),
                            destination.to_string_lossy()
                        ),
                    )
                })
                .map_err(StagedInstallError::AfterInstall)?;
            self.name.clear();
        }
        sync(&self.directory)
            .map_err(|error| {
                io_context(
                    error,
                    format!(
                        "failed to sync user input directory after installing {}",
                        destination.to_string_lossy()
                    ),
                )
            })
            .map_err(StagedInstallError::AfterInstall)
    }

    fn replace(self, destination: &OsStr) -> io::Result<()> {
        self.replace_with_hook(destination, || Ok(()))
    }

    fn replace_with_hook(
        mut self,
        destination: &OsStr,
        after_replace: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let result = self
            .directory
            .rename(&self.name, &self.directory, destination);
        result.map_err(|error| {
            io_context(
                error,
                format!(
                    "failed to replace {} with staged file {}",
                    destination.to_string_lossy(),
                    self.name.to_string_lossy()
                ),
            )
        })?;
        self.name.clear();
        after_replace()?;
        sync_directory(&self.directory).map_err(|error| {
            io_context(
                error,
                format!(
                    "failed to sync user input directory after replacing {}",
                    destination.to_string_lossy()
                ),
            )
        })
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        #[cfg(windows)]
        drop(self.publish_source.take());
        if !self.name.is_empty() {
            let _ = self.directory.remove_file(&self.name);
        }
    }
}

fn parse_input_file_name(name: &OsStr) -> Option<u64> {
    let name = name.to_str()?;
    let digits = name.strip_suffix(".in")?;
    let id = digits.parse::<u64>().ok()?;
    (id != 0 && digits == id.to_string()).then_some(id)
}

fn input_file_name(id: u64) -> io::Result<OsString> {
    if id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "user input ID must be at least 1",
        ));
    }
    Ok(OsString::from(format!("{id}.in")))
}

fn missing_input_problem_error(problem_index: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("no persisted user inputs for problem {problem_index}"),
    )
}

fn missing_input_error(problem_index: &str, id: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("user input {id} does not exist for problem {problem_index}"),
    )
}

fn id_space_exhausted() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "user input ID space is exhausted",
    )
}

#[cfg(unix)]
fn sync_directory(directory: &CapDir) -> io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &CapDir) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &cap_std::fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(_metadata: &cap_std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    struct TestWorkspace {
        _temp: tempfile::TempDir,
        destination: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let destination = temp.path().join("contest");
            fs::create_dir(&destination).unwrap();
            fs::create_dir(destination.join(".atc")).unwrap();
            Self {
                _temp: temp,
                destination,
            }
        }

        fn problem_directory(&self, problem: &str) -> PathBuf {
            self.destination
                .join(".atc")
                .join(USER_INPUTS_DIRECTORY)
                .join(problem)
        }
    }

    fn ids(inputs: &[UserInput]) -> Vec<u64> {
        inputs.iter().map(|input| input.id).collect()
    }

    #[test]
    fn missing_directory_loads_empty_and_first_creates_are_monotonic() {
        let workspace = TestWorkspace::new();
        assert!(
            load_user_inputs(&workspace.destination, "A")
                .unwrap()
                .is_empty()
        );

        assert_eq!(
            create_user_input(&workspace.destination, "A", "one").unwrap(),
            1
        );
        assert_eq!(
            create_user_input(&workspace.destination, "A", "two").unwrap(),
            2
        );
        assert_eq!(
            create_user_input(&workspace.destination, "A", "three").unwrap(),
            3
        );

        let loaded = load_user_inputs(&workspace.destination, "A").unwrap();
        assert_eq!(ids(&loaded), [1, 2, 3]);
        assert_eq!(
            loaded
                .iter()
                .map(|input| input.content.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        assert_eq!(
            create_user_input(&workspace.destination, "A", "after reload").unwrap(),
            4
        );
    }

    #[test]
    fn deleting_middle_or_last_id_never_renumbers_or_reuses_ids() {
        for deleted in [2, 3] {
            let workspace = TestWorkspace::new();
            for content in ["one", "two", "three"] {
                create_user_input(&workspace.destination, "A", content).unwrap();
            }

            delete_user_input(&workspace.destination, "A", deleted).unwrap();
            let expected = if deleted == 2 { vec![1, 3] } else { vec![1, 2] };
            assert_eq!(
                ids(&load_user_inputs(&workspace.destination, "A").unwrap()),
                expected
            );
            assert_eq!(
                create_user_input(&workspace.destination, "A", "new").unwrap(),
                4
            );
        }
    }

    #[test]
    fn stale_or_missing_metadata_is_reconciled_without_reusing_existing_ids() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(&problem).unwrap();
        fs::write(problem.join("1.in"), "one").unwrap();
        fs::write(problem.join("4.in"), "four").unwrap();
        fs::write(problem.join(METADATA_FILE), "version = 1\nnext_id = 2\n").unwrap();

        assert_eq!(
            create_user_input(&workspace.destination, "A", "five").unwrap(),
            5
        );
        assert!(problem.join("1.in").is_file());
        assert!(problem.join("4.in").is_file());
        assert!(problem.join("5.in").is_file());
    }

    #[test]
    fn deleting_highest_id_without_metadata_persists_the_high_water_mark_first() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(&problem).unwrap();
        for id in 1..=3 {
            fs::write(problem.join(format!("{id}.in")), id.to_string()).unwrap();
        }

        delete_user_input(&workspace.destination, "A", 3).unwrap();
        assert_eq!(
            create_user_input(&workspace.destination, "A", "four").unwrap(),
            4
        );
    }

    #[test]
    fn exact_text_round_trips_through_create_and_save() {
        let workspace = TestWorkspace::new();
        let values = [
            "",
            " ",
            "\n",
            "abc",
            "abc\n",
            "abc\n\n",
            "a\n\nb\n",
            "a\r\nb\r\n",
        ];

        for value in values {
            let id = create_user_input(&workspace.destination, "A", value).unwrap();
            let loaded = load_user_inputs(&workspace.destination, "A").unwrap();
            assert_eq!(
                loaded.iter().find(|input| input.id == id).unwrap().content,
                value
            );

            let replacement = format!("{value}\0 replacement\r\n\n");
            save_user_input(&workspace.destination, "A", id, &replacement).unwrap();
            let bytes =
                fs::read(workspace.problem_directory("A").join(format!("{id}.in"))).unwrap();
            assert_eq!(bytes, replacement.as_bytes());
        }
    }

    #[test]
    fn save_missing_does_not_create_the_requested_id() {
        let workspace = TestWorkspace::new();
        create_user_input(&workspace.destination, "A", "one").unwrap();

        let error = save_user_input(&workspace.destination, "A", 2, "must not exist").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!workspace.problem_directory("A").join("2.in").exists());
    }

    #[test]
    fn checked_save_replaces_exact_content_while_holding_the_problem_lock() {
        for (expected, new_content) in [("A\r\n\0", "C\n\n\t "), ("A", ""), ("", "C\r\n")] {
            let workspace = TestWorkspace::new();
            let id = create_user_input(&workspace.destination, "A", expected).unwrap();
            let lock_path = workspace.destination.join(".atc/user-inputs/.A.lock");
            let result = save_user_input_if_unchanged_with_hooks(
                &workspace.destination,
                "A",
                id,
                expected,
                new_content,
                || Ok(()),
                || {
                    let contender = fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&lock_path)?;
                    assert!(matches!(
                        contender.try_lock(),
                        Err(fs::TryLockError::WouldBlock)
                    ));
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(result, UserInputSaveOutcome::Saved);
            assert_eq!(
                fs::read(workspace.problem_directory("A").join(format!("{id}.in"))).unwrap(),
                new_content.as_bytes()
            );
            assert_eq!(
                ids(&load_user_inputs(&workspace.destination, "A").unwrap()),
                [id]
            );
        }
    }

    #[test]
    fn checked_unchanged_save_verifies_disk_without_replacement_or_metadata_write() {
        let workspace = TestWorkspace::new();
        let content = "A\r\n\n\0\t ";
        let id = create_user_input(&workspace.destination, "A", content).unwrap();
        let directory = workspace.problem_directory("A");
        // A stale but valid high-water mark would be rewritten by the write path.
        let metadata = b"version = 1\nnext_id = 1\n";
        fs::write(directory.join(METADATA_FILE), metadata).unwrap();
        assert_eq!(
            save_user_input_if_unchanged_with_hooks(
                &workspace.destination,
                "A",
                id,
                content,
                content,
                || Ok(()),
                || panic!("Unchanged must not enter replacement"),
            )
            .unwrap(),
            UserInputSaveOutcome::Unchanged
        );
        assert_eq!(fs::read(directory.join(METADATA_FILE)).unwrap(), metadata);
        assert_eq!(
            fs::read(directory.join(format!("{id}.in"))).unwrap(),
            content.as_bytes()
        );
        assert_eq!(fs::read_dir(directory).unwrap().count(), 2);
    }

    #[test]
    fn checked_conflict_and_missing_never_enter_the_write_path() {
        let workspace = TestWorkspace::new();
        let id = create_user_input(&workspace.destination, "A", "A\r\n").unwrap();
        save_user_input(&workspace.destination, "A", id, "B\n").unwrap();
        let directory = workspace.problem_directory("A");
        let metadata = fs::read(directory.join(METADATA_FILE)).unwrap();
        // Even new == disk is Conflict when the edit baseline differs.
        for new_content in ["C", "A\r\n", "B\n"] {
            assert_eq!(
                save_user_input_if_unchanged_with_hooks(
                    &workspace.destination,
                    "A",
                    id,
                    "A\r\n",
                    new_content,
                    || Ok(()),
                    || panic!("Conflict must not enter replacement"),
                )
                .unwrap(),
                UserInputSaveOutcome::Conflict
            );
            assert_eq!(
                fs::read(directory.join(format!("{id}.in"))).unwrap(),
                b"B\n"
            );
        }
        fs::remove_file(directory.join(format!("{id}.in"))).unwrap();
        assert_eq!(
            save_user_input_if_unchanged_with_hooks(
                &workspace.destination,
                "A",
                id,
                "B\n",
                "C",
                || Ok(()),
                || panic!("Missing must not enter replacement"),
            )
            .unwrap(),
            UserInputSaveOutcome::Missing
        );
        assert!(!directory.join(format!("{id}.in")).exists());
        assert_eq!(fs::read(directory.join(METADATA_FILE)).unwrap(), metadata);
        assert_eq!(
            save_user_input_if_unchanged(&workspace.destination, "B", id, "", "C",).unwrap(),
            UserInputSaveOutcome::Missing
        );
        assert_eq!(
            save_user_input_if_unchanged(&workspace.destination.join("absent"), "A", id, "", "C",)
                .unwrap(),
            UserInputSaveOutcome::Missing
        );
    }

    #[test]
    fn checked_save_reads_after_a_cooperative_writer_wins_the_lock() {
        let workspace = TestWorkspace::new();
        let id = create_user_input(&workspace.destination, "A", "A").unwrap();
        let destination = workspace.destination.clone();
        let (start_tx, start_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            done_tx
                .send(save_user_input(&destination, "A", id, "B"))
                .unwrap();
        });
        let outcome = save_user_input_if_unchanged_with_hooks(
            &workspace.destination,
            "A",
            id,
            "A",
            "C",
            || {
                start_tx.send(()).unwrap();
                done_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?
            },
            || panic!("the winning writer's content must not be replaced"),
        );
        writer.join().unwrap();
        assert_eq!(outcome.unwrap(), UserInputSaveOutcome::Conflict);
        assert_eq!(
            load_user_inputs(&workspace.destination, "A").unwrap()[0].content,
            "B"
        );
    }

    #[test]
    fn concurrent_checked_saves_of_one_baseline_have_only_one_winner() {
        let workspace = Arc::new(TestWorkspace::new());
        let id = create_user_input(&workspace.destination, "A", "A").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let workers = ["B", "C"].map(|content| {
            let workspace = Arc::clone(&workspace);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let outcome = save_user_input_if_unchanged_with_hooks(
                    &workspace.destination,
                    "A",
                    id,
                    "A",
                    content,
                    || {
                        barrier.wait();
                        Ok(())
                    },
                    || Ok(()),
                )
                .unwrap();
                (content, outcome)
            })
        });
        barrier.wait();
        let results = workers.map(|worker| worker.join().unwrap());
        assert_eq!(
            results
                .iter()
                .filter(|(_, outcome)| *outcome == UserInputSaveOutcome::Saved)
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|(_, outcome)| *outcome == UserInputSaveOutcome::Conflict)
                .count(),
            1
        );
        let winner = results
            .iter()
            .find(|(_, outcome)| *outcome == UserInputSaveOutcome::Saved)
            .unwrap()
            .0;
        assert_eq!(
            load_user_inputs(&workspace.destination, "A").unwrap()[0].content,
            winner
        );
    }

    #[test]
    fn checked_save_rejects_invalid_requests_and_unreadable_utf8_without_replacing() {
        let workspace = TestWorkspace::new();
        let id = create_user_input(&workspace.destination, "A", "A").unwrap();
        for problem in ["../escape", "A\\B", "CON"] {
            assert!(
                save_user_input_if_unchanged(&workspace.destination, problem, id, "A", "C")
                    .is_err()
            );
        }
        assert_eq!(
            save_user_input_if_unchanged(&workspace.destination, "A", 0, "A", "C")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let path = workspace.problem_directory("A").join(format!("{id}.in"));
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(
            save_user_input_if_unchanged(&workspace.destination, "A", id, "A", "C")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(path).unwrap(), [0xff, 0xfe]);
    }

    #[test]
    fn delete_removes_only_the_requested_id_and_missing_is_an_error() {
        let workspace = TestWorkspace::new();
        for content in ["one", "two", "three"] {
            create_user_input(&workspace.destination, "A", content).unwrap();
        }

        delete_user_input(&workspace.destination, "A", 2).unwrap();
        assert_eq!(
            ids(&load_user_inputs(&workspace.destination, "A").unwrap()),
            [1, 3]
        );
        let error = delete_user_input(&workspace.destination, "A", 2).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn invalid_ids_problem_indices_and_workspace_markers_are_rejected() {
        let workspace = TestWorkspace::new();
        for problem in ["../escape", "..", "/absolute", "A\\B", "CON"] {
            assert!(create_user_input(&workspace.destination, problem, "x").is_err());
        }
        assert_eq!(
            save_user_input(&workspace.destination, "A", 0, "x")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );

        let missing = tempfile::tempdir().unwrap();
        assert!(load_user_inputs(missing.path(), "A").is_err());
        let invalid = tempfile::tempdir().unwrap();
        fs::write(invalid.path().join(".atc"), "not a directory").unwrap();
        assert!(create_user_input(invalid.path(), "A", "x").is_err());
    }

    #[test]
    fn malformed_unsupported_unknown_and_zero_metadata_are_errors() {
        for metadata in [
            "not toml =",
            "version = 2\nnext_id = 1\n",
            "version = 1\nnext_id = 1\nunknown = true\n",
            "version = 1\nnext_id = 0\n",
        ] {
            let workspace = TestWorkspace::new();
            let problem = workspace.problem_directory("A");
            fs::create_dir_all(&problem).unwrap();
            fs::write(problem.join(METADATA_FILE), metadata).unwrap();
            assert!(load_user_inputs(&workspace.destination, "A").is_err());
            assert!(create_user_input(&workspace.destination, "A", "x").is_err());
        }
    }

    #[test]
    fn id_overflow_is_an_explicit_error_without_creating_an_input() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(&problem).unwrap();
        fs::write(
            problem.join(METADATA_FILE),
            format!("version = 1\nnext_id = {}\n", u64::MAX),
        )
        .unwrap();

        let error = create_user_input(&workspace.destination, "A", "x").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_dir(problem)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| parse_input_file_name(&entry.file_name()).is_some())
                .count(),
            0
        );
    }

    #[test]
    fn invalid_utf8_owned_input_is_an_error() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(&problem).unwrap();
        fs::write(problem.join("1.in"), [0xff, 0xfe]).unwrap();

        let error = load_user_inputs(&workspace.destination, "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unknown_files_and_noncanonical_input_names_are_preserved() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(&problem).unwrap();
        for (name, content) in [
            ("notes.txt", "notes"),
            ("backup.in.old", "backup"),
            ("0.in", "zero"),
            ("01.in", "leading zero"),
        ] {
            fs::write(problem.join(name), content).unwrap();
        }

        assert!(
            load_user_inputs(&workspace.destination, "A")
                .unwrap()
                .is_empty()
        );
        let id = create_user_input(&workspace.destination, "A", "owned").unwrap();
        assert_eq!(id, 1);
        delete_user_input(&workspace.destination, "A", id).unwrap();
        for (name, content) in [
            ("notes.txt", "notes"),
            ("backup.in.old", "backup"),
            ("0.in", "zero"),
            ("01.in", "leading zero"),
        ] {
            assert_eq!(fs::read_to_string(problem.join(name)).unwrap(), content);
        }
    }

    #[test]
    fn owned_input_directory_is_an_error() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(problem.join("1.in")).unwrap();

        assert!(load_user_inputs(&workspace.destination, "A").is_err());
        assert!(save_user_input(&workspace.destination, "A", 1, "x").is_err());
        assert!(delete_user_input(&workspace.destination, "A", 1).is_err());
        assert!(problem.join("1.in").is_dir());
    }

    #[test]
    fn interrupted_save_preserves_the_previous_input() {
        let workspace = TestWorkspace::new();
        let id = create_user_input(&workspace.destination, "A", "previous").unwrap();

        let error =
            save_user_input_with_hook(&workspace.destination, "A", id, "replacement", || {
                Err(io::Error::other("simulated interruption"))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            load_user_inputs(&workspace.destination, "A").unwrap()[0].content,
            "previous"
        );
    }

    #[test]
    fn save_error_after_replacement_leaves_recoverable_new_content() {
        let workspace = TestWorkspace::new();
        let id = create_user_input(&workspace.destination, "A", "previous").unwrap();

        let error = save_user_input_with_hooks(
            &workspace.destination,
            "A",
            id,
            "replacement\r\n\r\n",
            || Ok(()),
            || Ok(()),
            || Err(io::Error::other("simulated post-replacement sync failure")),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            load_user_inputs(&workspace.destination, "A").unwrap()[0].content,
            "replacement\r\n\r\n"
        );
    }

    #[test]
    fn permission_denied_after_replacement_does_not_retry_rename_or_hook() {
        let workspace = TestWorkspace::new();
        let id = create_user_input(&workspace.destination, "A", "previous").unwrap();
        let hook_calls = Cell::new(0);
        let error = save_user_input_with_hooks(
            &workspace.destination,
            "A",
            id,
            "replacement\r\n",
            || Ok(()),
            || Ok(()),
            || {
                hook_calls.set(hook_calls.get() + 1);
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "post-replacement failure",
                ))
            },
        )
        .unwrap_err();

        assert_eq!(hook_calls.get(), 1);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "post-replacement failure");
        assert_eq!(
            load_user_inputs(&workspace.destination, "A").unwrap()[0].content,
            "replacement\r\n"
        );
    }

    #[test]
    fn create_outcome_reports_failure_before_install_without_an_id() {
        let workspace = TestWorkspace::new();
        let error = create_user_input_with_outcome_and_hooks(
            &workspace.destination,
            "A",
            "not installed",
            || Ok(()),
            || Err(io::Error::other("simulated pre-install failure")),
            || Ok(()),
        )
        .unwrap_err();
        assert!(matches!(error, UserInputCreateError::BeforeInstall(_)));
        assert!(
            load_user_inputs(&workspace.destination, "A")
                .unwrap()
                .is_empty()
        );
        // BeforeInstall promises no installed input, not an untouched filesystem.
        let problem = CapDir::open_ambient_dir(
            workspace.problem_directory("A"),
            cap_std::ambient_authority(),
        )
        .unwrap();
        assert_eq!(read_metadata(&problem).unwrap().unwrap().next_id, 2);
        assert_eq!(
            create_user_input(&workspace.destination, "A", "retry").unwrap(),
            2
        );
    }

    #[test]
    fn after_install_failure_has_reserved_metadata_and_never_reuses_a_deleted_id() {
        let workspace = TestWorkspace::new();
        let error = create_user_input_with_outcome_and_hooks(
            &workspace.destination,
            "A",
            "complete first",
            || Ok(()),
            || Ok(()),
            || Err(io::Error::other("simulated interruption")),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            UserInputCreateError::AfterInstall { id: 1, .. }
        ));
        assert_eq!(
            fs::read(workspace.problem_directory("A").join("1.in")).unwrap(),
            b"complete first"
        );

        let problem = CapDir::open_ambient_dir(
            workspace.problem_directory("A"),
            cap_std::ambient_authority(),
        )
        .unwrap();
        assert_eq!(read_metadata(&problem).unwrap().unwrap().next_id, 2);
        fs::remove_file(workspace.problem_directory("A").join("1.in")).unwrap();

        assert_eq!(
            create_user_input(&workspace.destination, "A", "second").unwrap(),
            2
        );
    }

    #[test]
    fn metadata_reservation_precedes_input_publish_for_initial_and_existing_metadata() {
        let workspace = TestWorkspace::new();
        for id in 1..=2 {
            let path = workspace.problem_directory("A");
            assert_eq!(
                create_user_input_with_outcome_and_hooks(
                    &workspace.destination,
                    "A",
                    "complete",
                    || Ok(()),
                    || {
                        let problem =
                            CapDir::open_ambient_dir(&path, cap_std::ambient_authority())?;
                        assert_eq!(read_metadata(&problem)?.unwrap().next_id, id + 1);
                        assert!(!path.join(format!("{id}.in")).exists());
                        Ok(())
                    },
                    || {
                        let problem =
                            CapDir::open_ambient_dir(&path, cap_std::ambient_authority())?;
                        assert_eq!(read_metadata(&problem)?.unwrap().next_id, id + 1);
                        assert_eq!(fs::read(path.join(format!("{id}.in")))?, b"complete");
                        Ok(())
                    },
                )
                .unwrap(),
                id
            );
        }
    }

    #[test]
    fn reservation_errors_never_publish_and_retry_uses_actual_metadata() {
        for existing_metadata in [false, true] {
            for reservation_written in [false, true] {
                let workspace = TestWorkspace::new();
                if existing_metadata {
                    create_user_input(&workspace.destination, "A", "retained").unwrap();
                }
                let reserved_id = if existing_metadata { 2 } else { 1 };
                let outcome = create_user_input_with_reservation(
                    &workspace.destination,
                    "A",
                    "not published",
                    || Ok(()),
                    || panic!("reservation failure must stop before input publish"),
                    || panic!("input must not be installed"),
                    |problem, metadata, replace| {
                        if reservation_written {
                            persist_metadata(problem, metadata, replace)?;
                        }
                        Err(io::Error::other(
                            "injected metadata persistence/sync failure",
                        ))
                    },
                );
                assert!(matches!(
                    outcome,
                    Err(UserInputCreateError::BeforeInstall(_))
                ));
                let path = workspace.problem_directory("A");
                assert!(!path.join(format!("{reserved_id}.in")).exists());
                assert!(!fs::read_dir(&path).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(STAGING_PREFIX)
                }));
                let expected_id = reserved_id + u64::from(reservation_written);
                assert_eq!(
                    create_user_input(&workspace.destination, "A", "retry").unwrap(),
                    expected_id
                );
            }
        }
    }

    #[test]
    fn concurrent_creates_allocate_distinct_ids_without_clobbering() {
        let workspace = Arc::new(TestWorkspace::new());
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for content in ["left", "right"] {
            let workspace = Arc::clone(&workspace);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                create_user_input(&workspace.destination, "A", content).unwrap()
            }));
        }
        barrier.wait();
        let mut created = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        created.sort_unstable();
        assert_eq!(created, [1, 2]);

        let loaded = load_user_inputs(&workspace.destination, "A").unwrap();
        assert_eq!(ids(&loaded), [1, 2]);
        let mut contents = loaded
            .into_iter()
            .map(|input| input.content)
            .collect::<Vec<_>>();
        contents.sort();
        assert_eq!(contents, ["left", "right"]);
    }

    #[test]
    fn create_race_never_clobbers_a_competing_input_file() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");

        let error = create_user_input_with_outcome_and_hooks(
            &workspace.destination,
            "A",
            "ours",
            || Ok(()),
            || {
                fs::write(problem.join("1.in"), "competitor")?;
                Ok(())
            },
            || Ok(()),
        )
        .unwrap_err();
        let UserInputCreateError::BeforeInstall(error) = error else {
            panic!("publish collision must be classified before install");
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        #[cfg(not(windows))]
        assert!(
            error
                .to_string()
                .contains("failed to hard-link staged file")
        );
        #[cfg(windows)]
        assert!(error.to_string().contains("NTSTATUS 0xC0000035"));
        assert!(error.to_string().contains("1.in"));
        assert_eq!(
            fs::read_to_string(problem.join("1.in")).unwrap(),
            "competitor"
        );
        assert_eq!(
            create_user_input(&workspace.destination, "A", "next").unwrap(),
            2
        );
    }

    fn staged_fixture() -> (tempfile::TempDir, CapDir) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("owned");
        fs::create_dir(&path).unwrap();
        let directory = CapDir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap();
        (temp, directory)
    }

    #[test]
    fn staged_publish_exact_content_consumes_source_and_stays_directory_relative() {
        let (temp, directory) = staged_fixture();
        for (name, bytes) in [
            ("1.in", b"\r\n\t space\0\n".as_slice()),
            ("2.in", b""),
            (METADATA_FILE, b"version = 1\nnext_id = 3\n"),
        ] {
            fs::write(temp.path().join(name), "ambient sentinel").unwrap();
            let staged = StagedFile::for_install(&directory, "test", bytes).unwrap();
            let staging_name = staged.name.clone();
            staged
                .install_noclobber_with_outcome(OsStr::new(name))
                .unwrap();
            assert!(!temp.path().join("owned").join(staging_name).exists());
            assert_eq!(directory.read(name).unwrap(), bytes);
            assert_eq!(
                fs::read(temp.path().join(name)).unwrap(),
                b"ambient sentinel"
            );
        }
    }

    #[test]
    fn staged_publish_collision_preserves_competitor_and_cleans_staging() {
        let (_temp, directory) = staged_fixture();
        for name in ["1.in", METADATA_FILE] {
            let staged = StagedFile::for_install(&directory, "test", b"ours").unwrap();
            let staging_name = staged.name.clone();
            // The competitor appears after staging is ready; no existence precheck
            // can arbitrate this race. The publish primitive must do so atomically.
            let mut options = CapOpenOptions::new();
            options.write(true).create_new(true);
            directory
                .open_with(name, &options)
                .unwrap()
                .write_all(b"competitor")
                .unwrap();
            let sync_calls = Cell::new(0);
            let error = staged
                .install_noclobber_with_sync(OsStr::new(name), |_| {
                    sync_calls.set(sync_calls.get() + 1);
                    Ok(())
                })
                .unwrap_err();
            assert!(
                matches!(error, StagedInstallError::BeforeInstall(ref error) if error.kind() == io::ErrorKind::AlreadyExists)
            );
            assert_eq!(sync_calls.get(), 0);
            assert_eq!(directory.read(name).unwrap(), b"competitor");
            assert!(!directory.exists(staging_name));
        }
    }

    #[test]
    fn staged_publish_sync_failure_is_after_install_and_keeps_destination() {
        let (_temp, directory) = staged_fixture();
        for name in ["1.in", METADATA_FILE] {
            let staged = StagedFile::for_install(&directory, "test", b"complete").unwrap();
            let staging_name = staged.name.clone();
            let sync_calls = Cell::new(0);
            let error = staged
                .install_noclobber_with_sync(OsStr::new(name), |_| {
                    sync_calls.set(sync_calls.get() + 1);
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected directory sync failure",
                    ))
                })
                .unwrap_err();
            assert!(
                matches!(error, StagedInstallError::AfterInstall(ref error) if error.kind() == io::ErrorKind::PermissionDenied)
            );
            assert_eq!(sync_calls.get(), 1);
            assert_eq!(directory.read(name).unwrap(), b"complete");
            assert!(!directory.exists(staging_name));
        }
    }

    #[test]
    fn dropping_unpublished_staging_closes_handle_and_removes_name() {
        let (_temp, directory) = staged_fixture();
        let staged = StagedFile::for_install(&directory, "test", b"not published").unwrap();
        let name = staged.name.clone();
        drop(staged);
        assert!(!directory.exists(name));
        assert!(directory.entries().unwrap().next().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn native_publish_failure_preserves_source_until_staging_cleanup() {
        let (_temp, directory) = staged_fixture();
        let staged = StagedFile::for_install(&directory, "test", b"ours").unwrap();
        directory.write("1.in", b"competitor").unwrap();
        let error = windows_publish::publish(
            &directory,
            staged.publish_source.as_ref().unwrap(),
            OsStr::new("1.in"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(directory.read("1.in").unwrap(), b"competitor");
        assert_eq!(directory.read(&staged.name).unwrap(), b"ours");
        let name = staged.name.clone();
        drop(staged);
        assert!(!directory.exists(name));
    }

    #[cfg(windows)]
    #[test]
    fn native_publish_same_file_alias_is_a_successful_install() {
        let (_temp, directory) = staged_fixture();
        let staged = StagedFile::for_install(&directory, "test", b"same object").unwrap();
        let name = staged.name.clone();
        directory.hard_link(&name, &directory, "1.in").unwrap();
        staged
            .install_noclobber_with_outcome(OsStr::new("1.in"))
            .unwrap();
        assert!(!directory.exists(name));
        assert_eq!(directory.read("1.in").unwrap(), b"same object");
        assert_eq!(directory.entries().unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn native_publish_uses_retained_source_handle_not_a_reopened_staging_path() {
        let (_temp, directory) = staged_fixture();
        let staged = StagedFile::for_install(&directory, "test", b"completed source").unwrap();
        let name = staged.name.clone();
        directory.rename(&name, &directory, "moved-source").unwrap();
        directory.write(&name, b"different staging object").unwrap();

        staged
            .install_noclobber_with_outcome(OsStr::new("1.in"))
            .unwrap();

        assert_eq!(directory.read("1.in").unwrap(), b"completed source");
        assert_eq!(directory.read(name).unwrap(), b"different staging object");
        assert!(!directory.exists("moved-source"));
    }

    #[cfg(windows)]
    #[test]
    fn native_publish_rejects_invalid_destination_before_install_and_cleans_source() {
        let (_temp, directory) = staged_fixture();
        let staged = StagedFile::for_install(&directory, "test", b"ours").unwrap();
        let name = staged.name.clone();
        let error = staged
            .install_noclobber_with_outcome(OsStr::new("../1.in"))
            .unwrap_err();
        assert!(
            matches!(error, StagedInstallError::BeforeInstall(ref error) if error.kind() == io::ErrorKind::InvalidInput)
        );
        assert!(!directory.exists(name));
    }

    #[cfg(windows)]
    #[test]
    fn native_publish_does_not_replace_directory_or_follow_symlink() {
        let (temp, directory) = staged_fixture();
        directory.create_dir("1.in").unwrap();
        let external = temp.path().join("external");
        fs::write(&external, "external").unwrap();
        let has_symlink = create_file_symlink(&external, &temp.path().join("owned/2.in"));
        for name in ["1.in", "2.in"] {
            if name == "2.in" && !has_symlink {
                continue;
            }
            let staged = StagedFile::for_install(&directory, "test", b"ours").unwrap();
            let staging_name = staged.name.clone();
            let error = staged
                .install_noclobber_with_outcome(OsStr::new(name))
                .unwrap_err();
            assert!(
                matches!(error, StagedInstallError::BeforeInstall(ref error) if error.kind() == io::ErrorKind::AlreadyExists)
            );
            assert!(!directory.exists(staging_name));
        }
        assert!(directory.symlink_metadata("1.in").unwrap().is_dir());
        if has_symlink {
            assert!(
                directory
                    .symlink_metadata("2.in")
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        assert_eq!(fs::read(external).unwrap(), b"external");
    }

    fn seed_external_workspace(workspace: &TestWorkspace) -> (PathBuf, PathBuf) {
        let moved_destination = workspace._temp.path().join("validated-contest");
        let external_destination = workspace._temp.path().join("external-contest");
        let external_problem = external_destination
            .join(".atc")
            .join(USER_INPUTS_DIRECTORY)
            .join("A");
        fs::create_dir_all(&external_problem).unwrap();
        fs::write(external_problem.join("1.in"), "external input").unwrap();
        fs::write(
            external_problem.join(METADATA_FILE),
            "version = 1\nnext_id = 2\n",
        )
        .unwrap();
        (moved_destination, external_destination)
    }

    #[cfg(unix)]
    fn replace_destination_with_external_link(
        destination: &Path,
        moved_destination: &Path,
        external_destination: &Path,
    ) -> io::Result<bool> {
        fs::rename(destination, moved_destination)?;
        std::os::unix::fs::symlink(external_destination, destination)?;
        Ok(true)
    }

    #[cfg(windows)]
    fn replace_destination_with_external_link(
        destination: &Path,
        moved_destination: &Path,
        external_destination: &Path,
    ) -> io::Result<bool> {
        use std::process::Command;

        fs::rename(destination, moved_destination)?;
        let output = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(destination)
            .arg(external_destination)
            .output()?;
        if output.status.success() {
            return Ok(true);
        }

        // Junction creation can be disabled by the host policy. Restore the test workspace and
        // skip this platform-specific case without leaving a replaced destination behind.
        if fs::symlink_metadata(destination).is_ok() {
            fs::remove_dir(destination)?;
        }
        fs::rename(moved_destination, destination)?;
        Ok(false)
    }

    #[test]
    fn destination_link_replacement_race_never_touches_external_workspace() {
        for operation in ["load", "create", "save", "delete"] {
            let workspace = TestWorkspace::new();
            let (moved_destination, external_destination) = seed_external_workspace(&workspace);
            let external_problem = external_destination
                .join(".atc")
                .join(USER_INPUTS_DIRECTORY)
                .join("A");
            let swapped = Cell::new(false);
            let replacement_setup_completed = Cell::new(false);

            let result = match operation {
                "load" => load_user_inputs_with_hooks(
                    &workspace.destination,
                    "A",
                    || {
                        swapped.set(replace_destination_with_external_link(
                            &workspace.destination,
                            &moved_destination,
                            &external_destination,
                        )?);
                        replacement_setup_completed.set(true);
                        Ok(())
                    },
                    || Ok(()),
                    || Ok(()),
                )
                .map(|_| ()),
                "create" => create_user_input_with_hooks(
                    &workspace.destination,
                    "A",
                    "must stay local",
                    || {
                        swapped.set(replace_destination_with_external_link(
                            &workspace.destination,
                            &moved_destination,
                            &external_destination,
                        )?);
                        replacement_setup_completed.set(true);
                        Ok(())
                    },
                    || Ok(()),
                    || Ok(()),
                )
                .map(|_| ()),
                "save" => save_user_input_with_hooks(
                    &workspace.destination,
                    "A",
                    1,
                    "must not replace external",
                    || {
                        swapped.set(replace_destination_with_external_link(
                            &workspace.destination,
                            &moved_destination,
                            &external_destination,
                        )?);
                        replacement_setup_completed.set(true);
                        Ok(())
                    },
                    || Ok(()),
                    || Ok(()),
                ),
                "delete" => delete_user_input_with_hook(&workspace.destination, "A", 1, || {
                    swapped.set(replace_destination_with_external_link(
                        &workspace.destination,
                        &moved_destination,
                        &external_destination,
                    )?);
                    replacement_setup_completed.set(true);
                    Ok(())
                }),
                _ => unreachable!(),
            };

            if !swapped.get() {
                assert!(
                    replacement_setup_completed.get(),
                    "{operation} replacement setup failed: {result:?}"
                );
                continue;
            }
            assert!(
                result.is_err(),
                "{operation} accepted a replaced destination"
            );
            assert_eq!(
                fs::read_to_string(external_problem.join("1.in")).unwrap(),
                "external input"
            );
            assert_eq!(
                fs::read_to_string(external_problem.join(METADATA_FILE)).unwrap(),
                "version = 1\nnext_id = 2\n"
            );
            assert!(!external_problem.join("2.in").exists());
            assert!(!external_problem.parent().unwrap().join(".A.lock").exists());
        }
    }

    #[test]
    fn physical_destination_replacement_after_validation_is_rejected() {
        let workspace = TestWorkspace::new();
        let (moved_destination, external_destination) = seed_external_workspace(&workspace);

        let error = load_user_inputs_with_hooks(
            &workspace.destination,
            "A",
            || {
                fs::rename(&workspace.destination, &moved_destination)?;
                fs::rename(&external_destination, &workspace.destination)
            },
            || Ok(()),
            || Ok(()),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let replacement_problem = workspace.problem_directory("A");
        assert_eq!(
            fs::read_to_string(replacement_problem.join("1.in")).unwrap(),
            "external input"
        );
        assert!(
            !replacement_problem
                .parent()
                .unwrap()
                .join(".A.lock")
                .exists()
        );
    }

    #[test]
    fn lock_appearing_during_optimistic_load_forces_a_locked_reread() {
        let workspace = Arc::new(TestWorkspace::new());
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(&problem).unwrap();
        fs::write(problem.join("1.in"), "old").unwrap();
        fs::write(problem.join(METADATA_FILE), "version = 1\nnext_id = 2\n").unwrap();
        let lock_path = problem.parent().unwrap().join(".A.lock");
        assert!(!lock_path.exists());

        let missing_lock = Arc::new(Barrier::new(2));
        let resume_optimistic_load = Arc::new(Barrier::new(2));
        let optimistic_read_complete = Arc::new(Barrier::new(2));
        let resume_lock_recheck = Arc::new(Barrier::new(2));
        let writer_ready = Arc::new(Barrier::new(2));
        let finish_writer = Arc::new(Barrier::new(2));
        let (result_sender, result_receiver) = mpsc::channel();

        let load_workspace = Arc::clone(&workspace);
        let load_missing_lock = Arc::clone(&missing_lock);
        let load_resume_optimistic = Arc::clone(&resume_optimistic_load);
        let load_optimistic_complete = Arc::clone(&optimistic_read_complete);
        let load_resume_recheck = Arc::clone(&resume_lock_recheck);
        let load_handle = std::thread::spawn(move || {
            let result = load_user_inputs_with_hooks(
                &load_workspace.destination,
                "A",
                || Ok(()),
                || {
                    load_missing_lock.wait();
                    load_resume_optimistic.wait();
                    Ok(())
                },
                || {
                    load_optimistic_complete.wait();
                    load_resume_recheck.wait();
                    Ok(())
                },
            );
            result_sender.send(result).unwrap();
        });

        missing_lock.wait();

        let writer_workspace = Arc::clone(&workspace);
        let writer_at_replace = Arc::clone(&writer_ready);
        let writer_finish = Arc::clone(&finish_writer);
        let writer_handle = std::thread::spawn(move || {
            save_user_input_with_hook(&writer_workspace.destination, "A", 1, "new", || {
                writer_at_replace.wait();
                writer_finish.wait();
                Ok(())
            })
        });

        writer_ready.wait();
        assert!(lock_path.is_file());
        resume_optimistic_load.wait();
        optimistic_read_complete.wait();
        resume_lock_recheck.wait();

        // The writer still holds the exclusive lock. Returning here would mean the old optimistic
        // snapshot was accepted even though the lock appeared during the read.
        assert!(
            result_receiver
                .recv_timeout(Duration::from_millis(500))
                .is_err()
        );

        finish_writer.wait();
        writer_handle.join().unwrap().unwrap();
        let loaded = result_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        load_handle.join().unwrap();
        assert_eq!(
            loaded,
            [UserInput {
                id: 1,
                content: "new".into()
            }]
        );
    }

    #[test]
    fn optimistic_load_without_a_writer_does_not_create_a_lock_file() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(&problem).unwrap();
        fs::write(problem.join("1.in"), "input").unwrap();
        fs::write(problem.join(METADATA_FILE), "version = 1\nnext_id = 2\n").unwrap();
        let lock_path = problem.parent().unwrap().join(".A.lock");

        assert!(!lock_path.exists());
        assert_eq!(
            load_user_inputs(&workspace.destination, "A").unwrap(),
            [UserInput {
                id: 1,
                content: "input".into()
            }]
        );
        assert!(!lock_path.exists());
    }

    #[test]
    fn staging_leftover_is_ignored_and_preserved_by_all_operations() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(&problem).unwrap();
        let leftover = problem.join(".user-input-staging-crash-leftover");
        fs::write(&leftover, "leftover").unwrap();

        assert!(
            load_user_inputs(&workspace.destination, "A")
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read_to_string(&leftover).unwrap(), "leftover");
        let id = create_user_input(&workspace.destination, "A", "created").unwrap();
        assert_eq!(fs::read_to_string(&leftover).unwrap(), "leftover");
        save_user_input(&workspace.destination, "A", id, "saved").unwrap();
        assert_eq!(fs::read_to_string(&leftover).unwrap(), "leftover");
        delete_user_input(&workspace.destination, "A", id).unwrap();

        assert_eq!(fs::read_to_string(leftover).unwrap(), "leftover");
    }

    #[cfg(unix)]
    fn create_directory_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn create_directory_symlink(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => true,
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || matches!(error.raw_os_error(), Some(5) | Some(50) | Some(1314)) =>
            {
                false
            }
            Err(error) => panic!("failed to create test directory symlink: {error}"),
        }
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || matches!(error.raw_os_error(), Some(5) | Some(50) | Some(1314)) =>
            {
                false
            }
            Err(error) => panic!("failed to create test file symlink: {error}"),
        }
    }

    #[test]
    fn symlinked_root_or_problem_directory_is_not_followed() {
        for problem_link in [false, true] {
            let workspace = TestWorkspace::new();
            let external = workspace._temp.path().join("external");
            fs::create_dir(&external).unwrap();
            let root = workspace
                .destination
                .join(".atc")
                .join(USER_INPUTS_DIRECTORY);
            if problem_link {
                fs::create_dir(&root).unwrap();
                if !create_directory_symlink(&external, &root.join("A")) {
                    continue;
                }
            } else if !create_directory_symlink(&external, &root) {
                continue;
            }

            assert!(create_user_input(&workspace.destination, "A", "outside").is_err());
            assert!(load_user_inputs(&workspace.destination, "A").is_err());
            assert!(
                save_user_input_if_unchanged(&workspace.destination, "A", 1, "", "outside")
                    .is_err()
            );
            assert!(fs::read_dir(&external).unwrap().next().is_none());
        }
    }

    #[test]
    fn symlinked_owned_input_is_never_read_written_or_deleted() {
        let workspace = TestWorkspace::new();
        let problem = workspace.problem_directory("A");
        fs::create_dir_all(&problem).unwrap();
        let external = workspace._temp.path().join("external.txt");
        fs::write(&external, "external").unwrap();
        if !create_file_symlink(&external, &problem.join("1.in")) {
            return;
        }

        assert!(load_user_inputs(&workspace.destination, "A").is_err());
        assert!(save_user_input(&workspace.destination, "A", 1, "changed").is_err());
        assert!(
            save_user_input_if_unchanged(&workspace.destination, "A", 1, "external", "changed")
                .is_err()
        );
        assert!(delete_user_input(&workspace.destination, "A", 1).is_err());
        assert_eq!(fs::read_to_string(external).unwrap(), "external");
        assert!(
            fs::symlink_metadata(problem.join("1.in"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn symlinked_metadata_and_lock_are_rejected_without_touching_targets() {
        for name in [METADATA_FILE, ".A.lock"] {
            let workspace = TestWorkspace::new();
            let problem = workspace.problem_directory("A");
            fs::create_dir_all(&problem).unwrap();
            let external = workspace._temp.path().join(format!("external-{name}"));
            fs::write(&external, "external").unwrap();
            let link = if name == METADATA_FILE {
                problem.join(name)
            } else {
                problem.parent().unwrap().join(name)
            };
            if !create_file_symlink(&external, &link) {
                continue;
            }

            assert!(load_user_inputs(&workspace.destination, "A").is_err());
            assert!(create_user_input(&workspace.destination, "A", "x").is_err());
            assert_eq!(fs::read_to_string(external).unwrap(), "external");
        }
    }

    #[test]
    fn non_regular_metadata_and_lock_are_rejected() {
        for name in [METADATA_FILE, ".A.lock"] {
            let workspace = TestWorkspace::new();
            let problem = workspace.problem_directory("A");
            fs::create_dir_all(&problem).unwrap();
            let path = if name == METADATA_FILE {
                problem.join(name)
            } else {
                problem.parent().unwrap().join(name)
            };
            fs::create_dir(&path).unwrap();

            assert!(load_user_inputs(&workspace.destination, "A").is_err());
            assert!(create_user_input(&workspace.destination, "A", "x").is_err());
            assert!(path.is_dir());
        }
    }

    #[cfg(windows)]
    #[test]
    fn junctioned_root_or_problem_directory_is_not_followed() {
        use std::process::Command;

        for problem_junction in [false, true] {
            let workspace = TestWorkspace::new();
            let external = workspace._temp.path().join("junction-external");
            fs::create_dir(&external).unwrap();
            let root = workspace
                .destination
                .join(".atc")
                .join(USER_INPUTS_DIRECTORY);
            let junction = if problem_junction {
                fs::create_dir(&root).unwrap();
                root.join("A")
            } else {
                root
            };
            let output = Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(&junction)
                .arg(&external)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "failed to create test junction: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            assert!(create_user_input(&workspace.destination, "A", "outside").is_err());
            assert!(load_user_inputs(&workspace.destination, "A").is_err());
            assert!(fs::read_dir(&external).unwrap().next().is_none());
        }
    }
}
