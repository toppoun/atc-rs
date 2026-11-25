use super::update_cancellation::UpdateCancellation;
use super::{
    CapDir, PhysicalDirectoryIdentity, WORKSPACE_CONFIG_FILE, WorkspaceConfig,
    WorkspaceRelativePath, contest_spellings, infer_contest_id, is_safe_atcoder_id,
    load_existing_workspace_config, managed_metadata_is_reparse, matching_workspace_path,
    open_managed_directory, open_workspace_root_nofollow, open_workspace_update_parent,
    opened_directory_identity, reject_nested_workspace, standard_contest_series,
    validate_contest_id_match,
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
    pub(crate) found: usize,
    pub(crate) complete: bool,
}

pub(crate) struct ContestLocation {
    path: PathBuf,
    parent: WorkspaceRelativePath,
    parent_identity: PhysicalDirectoryIdentity,
    identity: PhysicalDirectoryIdentity,
}

pub(crate) struct WatchParent {
    pub(crate) path: PathBuf,
    pub(crate) identity: io::Result<Option<PhysicalDirectoryIdentity>>,
}

impl BatchWorkspace {
    pub(crate) fn watch_route(
        &self,
        id: &str,
        location: &ContestLocation,
    ) -> io::Result<Option<super::ExplicitContestRoute>> {
        let mapping = matching_workspace_path(&self.config, id)?;
        if mapping.map_or(!location.parent.components.is_empty(), |path| {
            *path != location.parent
        }) {
            // Batch additionally recognizes legacy series directories. Watch follows the active route.
            return Ok(None);
        }
        Ok(Some(super::ExplicitContestRoute {
            destination: location.path.clone(),
            config: Some(self.config.clone()),
            root_identity: self.identity.clone(),
            parent_identity: location.parent_identity.clone(),
            destination_identity: location.identity.clone(),
        }))
    }

    pub(crate) fn watch_parents(&self) -> io::Result<Vec<WatchParent>> {
        let root = open_workspace_root_nofollow(&self.root)?;
        if opened_directory_identity(&root, &self.root)? != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Workspace directory changed during discovery",
            ));
        }
        let prefixes: BTreeSet<_> = self
            .config
            .paths
            .iter()
            .flat_map(|rule| {
                (1..=rule.path.components.len()).map(|len| rule.path.components[..len].to_vec())
            })
            .collect();
        let mut parents = vec![WatchParent {
            path: self.root.clone(),
            identity: Ok(Some(self.identity.clone())),
        }];
        for components in prefixes {
            let parent = WorkspaceRelativePath { components };
            let path = parent.append_to(&self.root);
            let identity = self.open_parent(&root, &parent).and_then(|opened| {
                opened
                    .map(|directory| opened_directory_identity(&directory, &path))
                    .transpose()
            });
            parents.push(WatchParent { path, identity });
        }
        Ok(parents)
    }
    pub(crate) fn scan(
        root: &Path,
        cancellation: UpdateCancellation<'_>,
    ) -> io::Result<(Self, Discovery)> {
        Self::scan_layout(root, cancellation, false)
    }

    pub(crate) fn scan_watch(
        root: &Path,
        cancellation: UpdateCancellation<'_>,
    ) -> io::Result<(Self, Discovery)> {
        Self::scan_layout(root, cancellation, true)
    }

    fn scan_layout(
        root: &Path,
        cancellation: UpdateCancellation<'_>,
        active_routes_only: bool,
    ) -> io::Result<(Self, Discovery)> {
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
        if !active_routes_only {
            parents.extend(SERIES_DIRECTORIES.map(|(name, _)| vec![name.to_owned()]));
        }
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
            if cancellation.is_requested() {
                break;
            }
            if let Err(error) =
                workspace.scan_parent(&directory, parent, &mut candidates, cancellation)
            {
                errors.push((parent.append_to(root), error));
            }
        }

        let found = candidates.len();
        let contests = candidates
            .into_iter()
            .take_while(|_| !cancellation.is_requested())
            .filter_map(|(id, mut candidates)| {
                if active_routes_only {
                    candidates.retain(|candidate| {
                        let owner = candidate
                            .inspection
                            .as_ref()
                            .ok()
                            .and_then(|found| found.id.as_deref().ok())
                            .unwrap_or(&id);
                        match matching_workspace_path(&workspace.config, owner) {
                            Ok(Some(mapping)) => *mapping == candidate.parent,
                            Ok(None) => candidate.parent.components.is_empty(),
                            Err(_) => true, // Preserve the ambiguous-route diagnosis.
                        }
                    });
                }
                let path = candidates.first()?.path.clone();
                Some(match workspace.select(&id, candidates) {
                    Ok((id, location)) => (id, location.path.clone(), Ok(location)),
                    Err(error) => (id, path, Err(error)),
                })
            })
            .collect();
        let complete = !cancellation.is_requested();
        Ok((
            workspace,
            Discovery {
                contests,
                errors,
                found,
                complete,
            },
        ))
    }

    fn scan_parent(
        &self,
        root: &CapDir,
        parent: &WorkspaceRelativePath,
        candidates: &mut BTreeMap<String, Vec<Candidate>>,
        cancellation: UpdateCancellation<'_>,
    ) -> io::Result<()> {
        let Some(directory) = self.open_parent(root, parent)? else {
            return Ok(());
        };
        let path = parent.append_to(&self.root);
        let identity = opened_directory_identity(&directory, &path)?;
        let mut names = BTreeSet::new();
        for entry in directory.entries()? {
            #[cfg(test)]
            if let Some(checkpoint) = cancellation.checkpoint {
                checkpoint(super::update_cancellation::UpdateCheckpoint::Discovery);
            }
            if cancellation.is_requested() {
                return Ok(());
            }
            names.insert(entry?.file_name());
        }
        for name in names {
            if cancellation.is_requested() {
                break;
            }
            let Some(name) = name.to_str().filter(|name| is_safe_atcoder_id(name)) else {
                continue;
            };
            let folded = name.to_ascii_lowercase();
            let path = path.join(name);
            let result = match self.inspect_entry(&directory, parent, name, &folded, &path) {
                Ok(None) => continue,
                Ok(Some(identity)) => Ok(identity),
                Err(error) => Err(error),
            };
            candidates.entry(folded).or_default().push(Candidate {
                path,
                parent: parent.clone(),
                parent_identity: identity.clone(),
                inspection: result,
            });
        }
        Ok(())
    }

    fn inspect_entry(
        &self,
        directory: &CapDir,
        parent: &WorkspaceRelativePath,
        name: &str,
        folded: &str,
        path: &Path,
    ) -> io::Result<Option<DiscoveredContest>> {
        let metadata = match directory.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let standard_id = standard_contest_series(folded).is_some();
        let configured_id = self.config.paths.iter().any(|rule| {
            rule.path == *parent && (rule.pattern.is_match(name) || rule.pattern.is_match(folded))
        });
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

        let child = open_managed_directory(directory, name, path, "contest directory")?
            .ok_or_else(|| super::missing_contest_directory(path))?;
        if !super::entry_exists(&child, ".atc")?
            && !((standard_id || configured_id) && has_legacy_data(&child)?)
        {
            return Ok(None);
        }
        reject_nested_workspace(&child, path)?;
        Ok(Some(DiscoveredContest {
            identity: opened_directory_identity(&child, path)?,
            id: infer_contest_id(&child, path, name),
        }))
    }

    fn select(
        &self,
        folded: &str,
        mut candidates: Vec<Candidate>,
    ) -> io::Result<(String, ContestLocation)> {
        let first = &candidates[0];
        let first_identity = first.inspection.as_ref().ok().map(|found| &found.identity);
        if candidates[1..].iter().any(|candidate| {
            first.parent_identity != candidate.parent_identity
                || !candidate
                    .inspection
                    .as_ref()
                    .is_ok_and(|found| Some(&found.identity) == first_identity)
        }) {
            let paths = candidates
                .iter()
                .map(|candidate| candidate.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate contest ID {folded:?}: {paths}"),
            ));
        }
        let Candidate {
            mut path,
            mut parent,
            parent_identity,
            inspection,
        } = candidates.swap_remove(0);
        let DiscoveredContest { identity, id } = inspection?;
        let id = id?;
        let mapping = matching_workspace_path(&self.config, &id)?;
        // A configured spelling wins when both spellings open the same directory.
        if let Some(alias) = candidates
            .into_iter()
            .find(|candidate| mapping == Some(&candidate.parent))
        {
            path = alias.path;
            parent = alias.parent;
        }
        let allowed = match mapping {
            Some(mapping) => *mapping == parent,
            None => {
                parent.components.is_empty()
                    || SERIES_DIRECTORIES.iter().any(|(name, series)| {
                        parent.components == [*name]
                            && standard_contest_series(folded) == Some(*series)
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
        Ok((
            id,
            ContestLocation {
                path,
                parent,
                parent_identity,
                identity,
            },
        ))
    }

    fn open_parent(
        &self,
        root: &CapDir,
        parent: &WorkspaceRelativePath,
    ) -> io::Result<Option<CapDir>> {
        open_workspace_update_parent(&self.root, root, parent)
    }

    pub(crate) fn revalidate_location(
        &self,
        id: &str,
        location: &ContestLocation,
    ) -> io::Result<CapDir> {
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
        if config != self.config {
            return Err(changed());
        }
        let parent_path = location.parent.append_to(&self.root);
        let parent = self
            .open_parent(&root, &location.parent)?
            .ok_or_else(changed)?;
        if opened_directory_identity(&parent, &parent_path)? != location.parent_identity {
            return Err(changed());
        }
        let name = location
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(changed)?;
        let directory = open_managed_directory(&parent, name, &location.path, "contest directory")?
            .ok_or_else(changed)?;
        reject_nested_workspace(&directory, &location.path)?;
        if opened_directory_identity(&directory, &location.path)? != location.identity {
            return Err(changed());
        }
        validate_contest_id_match(&infer_contest_id(&directory, &location.path, name)?, id)?;
        Ok(root)
    }

    pub(crate) fn revalidate(&self, id: &str, location: &ContestLocation) -> io::Result<()> {
        let root = self.revalidate_location(id, location)?;
        // A new same-ID entry must not turn a previously unambiguous plan into a write.
        // Inaccessible scan locations are kept out of the operation.
        let folded = id.to_ascii_lowercase();
        for other in &self.parents {
            let Ok(Some(directory)) = self.open_parent(&root, other) else {
                continue;
            };
            let parent_path = other.append_to(&self.root);
            let same_parent =
                opened_directory_identity(&directory, &parent_path)? == location.parent_identity;
            for spelling in contest_spellings(&directory, id)? {
                if same_parent
                    && location
                        .path
                        .file_name()
                        .is_some_and(|name| name == spelling.as_str())
                {
                    continue;
                }
                let path = parent_path.join(&spelling);
                if !matches!(
                    self.inspect_entry(&directory, other, &spelling, &folded, &path),
                    Ok(None)
                ) {
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
        }
        Ok(())
    }
}

struct Candidate {
    path: PathBuf,
    parent: WorkspaceRelativePath,
    parent_identity: PhysicalDirectoryIdentity,
    inspection: io::Result<DiscoveredContest>,
}

struct DiscoveredContest {
    identity: PhysicalDirectoryIdentity,
    id: io::Result<String>,
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
