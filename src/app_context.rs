use std::io;
use std::path::{Path, PathBuf};

use crate::workspace;

/// Frontend capabilities are determined once from the exact command launch root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppContext {
    Workspace { root: PathBuf },
    Standalone { launch_root: PathBuf },
}

impl AppContext {
    pub(crate) fn from_launch_root(launch_root: &Path) -> io::Result<Self> {
        if workspace::inspect_workspace_config(launch_root)?.is_some() {
            Ok(Self::Workspace {
                root: launch_root.to_path_buf(),
            })
        } else {
            Ok(Self::Standalone {
                launch_root: launch_root.to_path_buf(),
            })
        }
    }

    pub(crate) fn workspace_root(&self) -> Option<&Path> {
        match self {
            Self::Workspace { root } => Some(root),
            Self::Standalone { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_workspace_config(root: &Path, paths: &str) {
        fs::write(
            root.join(".atc-workspace.toml"),
            format!("version = 1\npaths = {paths}\n"),
        )
        .unwrap();
    }

    #[test]
    fn exact_root_workspace_is_workspace_context() {
        let root = tempfile::tempdir().unwrap();
        write_workspace_config(root.path(), "[]");

        assert_eq!(
            AppContext::from_launch_root(root.path()).unwrap(),
            AppContext::Workspace {
                root: root.path().to_path_buf()
            }
        );
    }

    #[test]
    fn root_without_workspace_config_is_standalone_context() {
        let root = tempfile::tempdir().unwrap();

        assert_eq!(
            AppContext::from_launch_root(root.path()).unwrap(),
            AppContext::Standalone {
                launch_root: root.path().to_path_buf()
            }
        );
    }

    #[test]
    fn parent_workspace_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        write_workspace_config(root.path(), "[]");
        let child = root.path().join("contest");
        fs::create_dir(&child).unwrap();

        assert_eq!(
            AppContext::from_launch_root(&child).unwrap(),
            AppContext::Standalone { launch_root: child }
        );
    }

    #[test]
    fn mapped_destination_does_not_replace_the_workspace_root() {
        let root = tempfile::tempdir().unwrap();
        write_workspace_config(
            root.path(),
            "[{ pattern = \"^abc[0-9]+$\", path = \"AtCoder/ABC\" }]",
        );
        let context = AppContext::from_launch_root(root.path()).unwrap();
        let destination = workspace::resolve_contest_path(root.path(), "abc467").unwrap();

        assert_eq!(
            destination,
            root.path().join("AtCoder").join("ABC").join("abc467")
        );
        assert_eq!(context.workspace_root(), Some(root.path()));
    }

    #[test]
    fn invalid_exact_root_workspace_config_is_a_hard_error() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".atc-workspace.toml"), "invalid").unwrap();

        let error = AppContext::from_launch_root(root.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("workspace config"));
    }
}
