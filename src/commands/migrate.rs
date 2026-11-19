use super::{ContestUpdateChecks, fetch_contest_data, refresh::create_atcoder_client};
use crate::atcoder::AtCoderClient;
use crate::cli::MigrateArgs;
use crate::error::AppError;
use crate::safe_file::PreservedTempDir;
use crate::ui::Reporter;
use crate::workspace::{self, migration::MigrationInspection};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

pub(crate) fn migrate(args: MigrateArgs, reporter: &mut dyn Reporter) -> Result<(), AppError> {
    migrate_from(
        &std::env::current_dir()?,
        &args,
        create_atcoder_client,
        reporter,
        &mut io::stderr(),
        |output| {
            let input = io::stdin();
            if !input.is_terminal() || !io::stderr().is_terminal() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "migration requires a terminal for confirmation or explicit approval with --yes",
                ));
            }
            confirm_migration(&mut input.lock(), output)
        },
    )
}

pub(super) fn confirm_migration(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> io::Result<bool> {
    write!(output, "{MIGRATION_PROMPT}")?;
    output.flush()?;
    read_migration_confirmation(input)
}

pub(super) const MIGRATION_PROMPT: &str = "Proceed with migration? [y/N] ";

pub(super) fn read_migration_confirmation(input: &mut dyn BufRead) -> io::Result<bool> {
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn migrate_from(
    cwd: &Path,
    args: &MigrateArgs,
    create_client: impl FnOnce() -> Result<AtCoderClient, AppError>,
    reporter: &mut dyn Reporter,
    output: &mut dyn Write,
    confirm: impl FnOnce(&mut dyn Write) -> io::Result<bool>,
) -> Result<(), AppError> {
    let specified_id = args.contest.as_deref();
    let route = specified_id
        .map(|id| workspace::ExplicitContestRoute::resolve(cwd, id))
        .transpose()?;
    let destination = route
        .as_ref()
        .map_or(cwd, |route| route.destination.as_path());
    if route.is_none() {
        workspace::validate_workspace_update_target(destination)?;
    }
    let contest_id = specified_id
        .or_else(|| cwd.file_name().and_then(|name| name.to_str()))
        .filter(|id| workspace::is_safe_atcoder_id(id))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot determine a safe contest ID; use -c <contest-id> from the workspace root",
            )
        })?;
    let MigrationInspection::Ready(plan) =
        workspace::migration::inspect(destination.to_path_buf(), contest_id.to_owned())?
    else {
        writeln!(
            output,
            "{contest_id}: local data healthy; migration skipped"
        )?;
        return Ok(());
    };

    plan.write_summary(output)?;
    if args.dry_run {
        return Ok(());
    }
    if !args.yes && !confirm(output)? {
        writeln!(output, "Migration cancelled.")?;
        return Ok(());
    }

    let revalidate_mapping = |destination: &Path| match &route {
        Some(route) => route.revalidate(cwd, contest_id),
        None => workspace::validate_workspace_update_target(destination),
    };

    let backup = execute_migration(
        *plan,
        create_client,
        reporter,
        ContestUpdateChecks {
            validate_route: &revalidate_mapping,
            ..ContestUpdateChecks::default()
        },
    )?;
    writeln!(output, "Migrated {contest_id}")?;
    writeln!(output, "Backup: {}", backup.display())?;
    Ok(())
}

pub(super) fn execute_migration<C: std::borrow::Borrow<AtCoderClient>>(
    plan: workspace::migration::MigrationPlan,
    client: impl FnOnce() -> Result<C, AppError>,
    reporter: &mut dyn Reporter,
    checks: ContestUpdateChecks<'_>,
) -> Result<PathBuf, AppError> {
    let revalidate_mapping = checks.validate_route;
    revalidate_mapping(plan.destination())?;
    plan.revalidate()?;
    checks.cancellation.before_fetch()?;
    let client = client()?;
    let data = fetch_contest_data(plan.contest_id(), client.borrow(), reporter)?;
    workspace::validate_contest_identity(&data.contest, plan.contest_id())?;
    workspace::validate_contest_paths(&data.contest)?;
    revalidate_mapping(plan.destination())?;
    plan.revalidate()?;
    checks.cancellation.before_staging()?;

    let staging = PreservedTempDir::new(
        tempfile::Builder::new()
            .prefix(".atc-migrate-")
            .tempdir_in(plan.destination())?,
    );
    revalidate_mapping(plan.destination())
        .and_then(|()| plan.revalidate_directories())
        .and_then(|()| data.stage(staging.path()))
        .and_then(|()| revalidate_mapping(plan.destination()))
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to stage official migration data: {error}; staging kept at {}",
                    staging.path().display()
                ),
            )
        })?;

    Ok(plan.apply_checked(staging, revalidate_mapping, checks.cancellation)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{Event, NullReporter};
    use crate::workspace::diagnosis::{ContestDiagnosis, diagnose_contest};
    use std::fs;

    fn fixture_client() -> Result<AtCoderClient, AppError> {
        Ok(AtCoderClient::fixture(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures"),
        ))
    }

    #[test]
    fn declining_interactive_migration_preserves_the_entire_tree() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir_all(destination.join("tests")).unwrap();
        fs::write(destination.join("tests/legacy"), b"preserve").unwrap();
        let before = crate::safe_file::read_tree_snapshot(&destination).unwrap();
        let args = MigrateArgs {
            contest: None,
            dry_run: false,
            all: false,
            yes: false,
        };
        let mut output = Vec::new();
        migrate_from(
            &destination,
            &args,
            || panic!("declined migration must not initialize AtCoder"),
            &mut NullReporter,
            &mut output,
            |output| confirm_migration(&mut &b"n\n"[..], output),
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Migrate abc466"));
        assert!(output.contains("[y/N]"));
        assert!(output.contains("Migration cancelled."));
        assert!(crate::safe_file::read_tree_snapshot(&destination).unwrap() == before);
    }

    #[test]
    fn official_fetch_cannot_authorize_migration_of_changed_local_data() {
        struct EditDuringFetch<'a> {
            path: &'a Path,
            replacement: &'a [u8],
            edited: bool,
        }

        impl Reporter for EditDuringFetch<'_> {
            fn report(&mut self, event: Event<'_>) {
                if !self.edited && matches!(event, Event::ProblemFetched { .. }) {
                    fs::write(self.path, self.replacement).unwrap();
                    self.edited = true;
                }
            }
        }

        for (relative, original, edited) in [
            ("tests/a/notes", &b"first"[..], &b"other"[..]),
            (
                ".atc/contest.toml",
                &b"version = 99\ncontest_id = 'abc466'\n"[..],
                &b"version = 99\ncontest_id = 'abc466'\n# comment\n"[..],
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let destination = temp.path().join("abc466");
            let path = destination.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, original).unwrap();
            let args = MigrateArgs {
                contest: None,
                dry_run: false,
                all: false,
                yes: true,
            };
            let error = migrate_from(
                &destination,
                &args,
                fixture_client,
                &mut EditDuringFetch {
                    path: &path,
                    replacement: edited,
                    edited: false,
                },
                &mut Vec::new(),
                |_| panic!("--yes suppresses confirmation"),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("local contest data changed"),
                "{error}"
            );
            assert_eq!(fs::read(&path).unwrap(), edited);
            assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
        }
    }

    #[test]
    fn explicit_migration_rejects_an_intermediate_workspace_added_during_fetch() {
        struct AddBoundary<'a>(&'a Path);

        impl Reporter for AddBoundary<'_> {
            fn report(&mut self, event: Event<'_>) {
                if matches!(event, Event::ContestFetched { .. }) {
                    fs::write(self.0, b"version = 1\n").unwrap();
                }
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(
            root.join(".atc-workspace.toml"),
            "version = 1\n[[paths]]\npattern = '^abc466$'\npath = 'inner/series'\n",
        )
        .unwrap();
        let contest = root.join("inner/series/abc466");
        fs::create_dir_all(contest.join("tests")).unwrap();
        fs::write(contest.join("tests/legacy"), b"preserve").unwrap();
        let before = crate::safe_file::read_tree_snapshot(&contest).unwrap();
        let args = MigrateArgs {
            contest: Some("abc466".into()),
            dry_run: false,
            all: false,
            yes: true,
        };
        let error = migrate_from(
            root,
            &args,
            fixture_client,
            &mut AddBoundary(&root.join("inner/.atc-workspace.toml")),
            &mut Vec::new(),
            |_| panic!("--yes suppresses confirmation"),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("Workspace boundary at"),
            "{error}"
        );
        assert_eq!(
            crate::safe_file::read_tree_snapshot(&contest).unwrap(),
            before
        );
    }

    #[test]
    fn migration_finishes_with_a_healthy_diagnosis() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir_all(destination.join("tests")).unwrap();
        let args = MigrateArgs {
            contest: None,
            dry_run: false,
            all: false,
            yes: false,
        };
        migrate_from(
            &destination,
            &args,
            fixture_client,
            &mut NullReporter,
            &mut Vec::new(),
            |output| confirm_migration(&mut &b"yes\n"[..], output),
        )
        .unwrap();
        let ContestDiagnosis::Healthy { contest, .. } = diagnose_contest(&destination, "abc466")
        else {
            panic!("migration should produce a healthy contest");
        };
        assert_eq!(contest.problems.len(), 7);
        migrate_from(
            &destination,
            &args,
            || panic!("a healthy contest must skip before initializing AtCoder"),
            &mut NullReporter,
            &mut Vec::new(),
            |_| panic!("a healthy contest needs no approval"),
        )
        .unwrap();
    }
}
