use crate::error::AppError;
use crate::ui::{Event, Reporter};
use crate::workspace::{self, WorkspaceInitialization};
use std::path::Path;

pub(crate) fn init(reporter: &mut dyn Reporter) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    init_at(&cwd, reporter)
}

fn init_at(cwd: &Path, reporter: &mut dyn Reporter) -> Result<(), AppError> {
    match workspace::initialize_workspace(cwd)? {
        WorkspaceInitialization::Created(path) => {
            reporter.report(Event::WorkspaceInitialized { path: &path });
        }
        WorkspaceInitialization::AlreadyInitialized(path) => {
            reporter.report(Event::WorkspaceAlreadyInitialized { path: &path });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[derive(Default)]
    struct RecordingReporter {
        events: Vec<String>,
    }

    impl Reporter for RecordingReporter {
        fn report(&mut self, event: Event<'_>) {
            match event {
                Event::WorkspaceInitialized { path } => {
                    self.events.push(format!("initialized:{}", path.display()));
                }
                Event::WorkspaceAlreadyInitialized { path } => {
                    self.events
                        .push(format!("already-initialized:{}", path.display()));
                }
                _ => panic!("unexpected event"),
            }
        }
    }

    #[test]
    fn init_targets_only_the_explicit_current_directory() {
        let temp = tempfile::tempdir().unwrap();
        let parent_config = temp.path().join(".atc-workspace.toml");
        let parent_bytes = b"version = 1\npaths = []\n";
        fs::write(&parent_config, parent_bytes).unwrap();
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        let mut reporter = RecordingReporter::default();

        init_at(&child, &mut reporter).unwrap();

        let child_config = child.join(".atc-workspace.toml");
        assert!(child_config.is_file());
        assert_eq!(fs::read(&parent_config).unwrap(), parent_bytes);
        assert_eq!(
            reporter.events,
            [format!("initialized:{}", child_config.display())]
        );
    }

    #[test]
    fn init_reports_an_existing_valid_workspace_as_a_noop() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".atc-workspace.toml");
        fs::write(&config, "version = 1\npaths = []\n").unwrap();
        let mut reporter = RecordingReporter::default();

        init_at(temp.path(), &mut reporter).unwrap();

        assert_eq!(
            reporter.events,
            [format!("already-initialized:{}", config.display())]
        );
    }
}
