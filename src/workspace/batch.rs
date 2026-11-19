use super::{
    CapDir, PhysicalDirectoryIdentity, WORKSPACE_CONFIG_FILE, WorkspaceConfig,
    WorkspaceRelativePath, is_safe_atcoder_id, load_existing_workspace_config,
    managed_metadata_is_reparse, matching_workspace_path, open_managed_directory,
    open_workspace_root_nofollow, opened_directory_identity, validate_path_component,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

const SERIES_DIRECTORIES: [(&str, &str); 6] = [
    ("ABC", "abc"),
    ("ABC(Atcoder Beginner Contest)", "abc"),
    ("ARC", "arc"),
    ("ARC(Atcoder Regular Contest)", "arc"),
    ("AGC", "agc"),
    ("AGC(Atcoder Grand Contest)", "agc"),
];

pub(crate) struct BatchWorkspace {
    root: PathBuf,
    identity: PhysicalDirectoryIdentity,
    config: WorkspaceConfig,
    parents: Vec<WorkspaceRelativePath>,
}

pub(crate) struct Discovery {
    pub(crate) contests: Vec<(String, PathBuf, io::Result<ContestLocation>)>,
    pub(crate) errors: Vec<(PathBuf, io::Error)>,
}

pub(crate) struct ContestLocation {
    path: PathBuf,
    parent: WorkspaceRelativePath,
    parent_identity: PhysicalDirectoryIdentity,
    identity: PhysicalDirectoryIdentity,
}

impl BatchWorkspace {
    pub(crate) fn scan(root: &Path) -> io::Result<(Self, Discovery)> {
        let directory = open_workspace_root_nofollow(root)?;
        let config = load_existing_workspace_config(root, &directory)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "run --all from a Workspace root containing {WORKSPACE_CONFIG_FILE}: {}",
                    root.display()
                ),
            )
        })?;
        let mut parents = BTreeSet::from([Vec::new()]);
        parents.extend(config.paths.iter().map(|rule| rule.path.components.clone()));
        parents.extend(SERIES_DIRECTORIES.map(|(name, _)| vec![name.to_owned()]));
        let workspace = Self {
            root: root.to_path_buf(),
            identity: opened_directory_identity(&directory, root)?,
            config,
            parents: parents
                .into_iter()
                .map(|components| WorkspaceRelativePath { components })
                .collect(),
        };

        let mut candidates = BTreeMap::<String, Vec<Candidate>>::new();
        let mut errors = Vec::new();
        for parent in &workspace.parents {
            if let Err(error) = workspace.scan_parent(&directory, parent, &mut candidates) {
                errors.push((parent.append_to(root), error));
            }
        }

        let contests = candidates
            .into_iter()
            .map(|(id, candidates)| {
                let path = candidates[0].path.clone();
                match workspace.select(&id, candidates) {
                    Ok(location) => (id, location.path.clone(), Ok(location)),
                    Err(error) => (id, path, Err(error)),
                }
            })
            .collect();
        Ok((workspace, Discovery { contests, errors }))
    }

    fn scan_parent(
        &self,
        root: &CapDir,
        parent: &WorkspaceRelativePath,
        candidates: &mut BTreeMap<String, Vec<Candidate>>,
    ) -> io::Result<()> {
        let Some(directory) = self.open_parent(root, parent)? else {
            return Ok(());
        };
        let path = parent.append_to(&self.root);
        let identity = opened_directory_identity(&directory, &path)?;
        let names = directory
            .entries()?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<BTreeSet<_>>>()?;
        for name in names {
            let Some(id) = name.to_str().filter(|id| is_safe_atcoder_id(id)) else {
                continue;
            };
            let path = path.join(id);
            let result = match self.inspect_entry(&directory, parent, id, &path) {
                Ok(None) => continue,
                Ok(Some(identity)) => Ok(identity),
                Err(error) => Err(error),
            };
            candidates
                .entry(id.to_owned())
                .or_default()
                .push(Candidate {
                    path,
                    parent: parent.clone(),
                    parent_identity: identity.clone(),
                    identity: result,
                });
        }
        Ok(())
    }

    fn inspect_entry(
        &self,
        directory: &CapDir,
        parent: &WorkspaceRelativePath,
        id: &str,
        path: &Path,
    ) -> io::Result<Option<PhysicalDirectoryIdentity>> {
        let metadata = match directory.symlink_metadata(id) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let standard_id = series_id(id).is_some();
        let configured_id = self
            .config
            .paths
            .iter()
            .any(|rule| rule.path == *parent && rule.pattern.is_match(id));
        if metadata.file_type().is_symlink() || managed_metadata_is_reparse(&metadata) {
            return if standard_id || configured_id {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "contest directory is a symbolic link or reparse point: {}",
                        path.display()
                    ),
                ))
            } else {
                Ok(None)
            };
        }
        if !metadata.is_dir() {
            return Ok(None);
        }

        let child = open_managed_directory(directory, id, path, "contest directory")?
            .ok_or_else(|| super::missing_contest_directory(path))?;
        if !super::entry_exists(&child, ".atc")?
            && !((standard_id || configured_id) && has_legacy_data(&child)?)
        {
            return Ok(None);
        }
        validate_path_component(id, "contest ID")?;
        reject_nested_workspace(&child, path)?;
        Ok(Some(opened_directory_identity(&child, path)?))
    }

    fn select(&self, id: &str, mut candidates: Vec<Candidate>) -> io::Result<ContestLocation> {
        let mapping = matching_workspace_path(&self.config, id)?;
        // A configured spelling wins when both spellings open the same directory.
        if let Some(index) = candidates
            .iter()
            .position(|candidate| mapping == Some(&candidate.parent))
        {
            candidates.swap(0, index);
        }
        let first = &candidates[0];
        if candidates[1..].iter().any(|candidate| {
            !(first.identity.as_ref().ok().is_some()
                && first.identity.as_ref().ok() == candidate.identity.as_ref().ok()
                && first.parent_identity == candidate.parent_identity)
        }) {
            let paths = candidates
                .iter()
                .map(|candidate| candidate.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate contest ID {id:?}: {paths}"),
            ));
        }
        let Candidate {
            path,
            parent,
            parent_identity,
            identity,
        } = candidates.swap_remove(0);
        let identity = identity?;
        let allowed = match mapping {
            Some(mapping) => *mapping == parent,
            None => {
                parent.components.is_empty()
                    || SERIES_DIRECTORIES.iter().any(|(name, series)| {
                        parent.components == [*name] && series_id(id) == Some(*series)
                    })
            }
        };
        if !allowed {
            let expected = mapping.map_or_else(
                || self.root.clone(),
                |mapping| mapping.append_to(&self.root),
            );
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "contest is outside its Workspace route: {}; expected a contest under {}",
                    path.display(),
                    expected.display()
                ),
            ));
        }
        Ok(ContestLocation {
            path,
            parent,
            parent_identity,
            identity,
        })
    }

    fn open_parent(
        &self,
        root: &CapDir,
        parent: &WorkspaceRelativePath,
    ) -> io::Result<Option<CapDir>> {
        let mut directory = root.try_clone()?;
        let mut path = self.root.clone();
        for component in parent.components() {
            path.push(component);
            let Some(child) =
                open_managed_directory(&directory, component, &path, "workspace scan directory")?
            else {
                return Ok(None);
            };
            reject_nested_workspace(&child, &path)?;
            directory = child;
        }
        Ok(Some(directory))
    }

    pub(crate) fn revalidate(&self, id: &str, location: &ContestLocation) -> io::Result<()> {
        let root = open_workspace_root_nofollow(&self.root)?;
        let changed = || {
            io::Error::new(
                io::ErrorKind::Interrupted,
                format!(
                    "Workspace route or contest directory changed after planning: {}",
                    location.path.display()
                ),
            )
        };
        if opened_directory_identity(&root, &self.root)? != self.identity {
            return Err(changed());
        }
        let config = load_existing_workspace_config(&self.root, &root)?.ok_or_else(changed)?;
        if !config
            .paths
            .iter()
            .map(|rule| (rule.pattern.as_str(), &rule.path))
            .eq(self
                .config
                .paths
                .iter()
                .map(|rule| (rule.pattern.as_str(), &rule.path)))
        {
            return Err(changed());
        }
        let parent_path = location.parent.append_to(&self.root);
        let parent = self
            .open_parent(&root, &location.parent)?
            .ok_or_else(changed)?;
        if opened_directory_identity(&parent, &parent_path)? != location.parent_identity {
            return Err(changed());
        }
        let directory = open_managed_directory(&parent, id, &location.path, "contest directory")?
            .ok_or_else(changed)?;
        reject_nested_workspace(&directory, &location.path)?;
        if opened_directory_identity(&directory, &location.path)? != location.identity {
            return Err(changed());
        }
        // A new same-ID entry must not turn a previously unambiguous plan into a write.
        // Inaccessible scan locations are kept out of the operation.
        for other in &self.parents {
            if other == &location.parent {
                continue;
            }
            let Ok(Some(directory)) = self.open_parent(&root, other) else {
                continue;
            };
            let parent_path = other.append_to(&self.root);
            let path = parent_path.join(id);
            if opened_directory_identity(&directory, &parent_path)? != location.parent_identity
                && !matches!(self.inspect_entry(&directory, other, id, &path), Ok(None))
            {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!(
                        "duplicate contest ID {id:?} after planning: {} and {}",
                        location.path.display(),
                        path.display()
                    ),
                ));
            }
        }
        Ok(())
    }
}

struct Candidate {
    path: PathBuf,
    parent: WorkspaceRelativePath,
    parent_identity: PhysicalDirectoryIdentity,
    identity: io::Result<PhysicalDirectoryIdentity>,
}

fn series_id(id: &str) -> Option<&'static str> {
    ["abc", "arc", "agc", "ahc"].into_iter().find(|prefix| {
        id.strip_prefix(prefix).is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

fn has_legacy_data(directory: &CapDir) -> io::Result<bool> {
    if super::entry_exists(directory, "tests")?
        || super::entry_exists(directory, ".atc-update.lock")?
    {
        return Ok(true);
    }
    for entry in directory.entries()? {
        if entry?.file_name().to_str().is_some_and(|name| {
            super::is_contest_staging_name(name) || super::is_contest_recovery_name(name)
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn reject_nested_workspace(directory: &CapDir, path: &Path) -> io::Result<()> {
    if super::workspace_config_entry_exists(directory)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "nested Workspace is outside this operation: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}
