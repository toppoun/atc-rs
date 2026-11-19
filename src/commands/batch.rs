use super::ContestUpdateChecks;
#[cfg(test)]
use super::migrate::confirm_migration;
use super::migrate::{MIGRATION_PROMPT, execute_migration};
use super::refresh::{RefreshBaseline, execute_cli_refresh, plan_refresh_update};
use crate::atcoder::{AtCoderClient, AtCoderError, BATCH_REQUEST_INTERVAL};
use crate::error::AppError;
use crate::ui::Reporter;
use crate::workspace::batch::{BatchWorkspace, ContestLocation};
use crate::workspace::diagnosis::{
    ContestDiagnosis, MigrationReason, RepairReason, diagnose_contest,
};
use crate::workspace::migration::{self, MigrationInspection, MigrationPlan};
use crate::workspace::update_cancellation::{UpdateCancellation, UpdateCancelled};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use terminal::{BatchOutput, wait_for_approval};

mod terminal;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
pub(crate) enum BatchOperation {
    Refresh { force: bool },
    Migrate,
}

pub(crate) fn run_batch(
    operation: BatchOperation,
    dry_run: bool,
    yes: bool,
) -> Result<ExitCode, AppError> {
    install_interrupt_handler()?;
    let cancellation = UpdateCancellation::for_batch(&INTERRUPTED);
    let mut output = BatchOutput::new(cancellation)?;
    let mut reporter = output.clone();
    let result = run_batch_with(
        operation,
        dry_run,
        yes,
        cancellation,
        &mut reporter,
        &mut output,
    );
    if cancellation.is_requested() {
        return Ok(ExitCode::from(130));
    }
    match result {
        Ok(code) => Ok(code),
        Err(error) => {
            // The same pipe may have caused the error. Avoid a second blocking write
            // or a panic in main's normal error handler.
            let _ = writeln!(output, "error: {error}");
            Ok(if cancellation.is_requested() {
                ExitCode::from(130)
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

fn run_batch_with(
    operation: BatchOperation,
    dry_run: bool,
    yes: bool,
    cancellation: UpdateCancellation<'_>,
    reporter: &mut dyn Reporter,
    output: &mut dyn Write,
) -> Result<ExitCode, AppError> {
    writeln!(output, "Scanning workspace...")?;
    let plan = BatchPlan::scan(&std::env::current_dir()?, operation, cancellation)?;
    plan.write(output, cancellation)?;

    let mut run = false;
    let mut approval_error = None;
    if !cancellation.is_requested() && !dry_run && plan.has_updates() {
        run = if yes {
            true
        } else if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            approval_error = Some("batch changes in a non-interactive environment require --yes");
            false
        } else {
            match operation {
                BatchOperation::Refresh { .. } => true,
                BatchOperation::Migrate => {
                    write!(output, "{MIGRATION_PROMPT}")?;
                    output.flush()?;
                    match wait_for_approval(cancellation) {
                        Ok(approved) => approved,
                        Err(_) if cancellation.is_requested() => false,
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        };
        if !run && approval_error.is_none() && !cancellation.is_requested() {
            writeln!(output, "Migration cancelled.")?;
        }
    }

    let result = plan.run(
        run,
        dry_run,
        cancellation,
        || {
            Ok(AtCoderClient::for_batch(
                BATCH_REQUEST_INTERVAL,
                &INTERRUPTED,
            )?)
        },
        reporter,
        output,
    )?;
    if let Some(message) = approval_error
        && !cancellation.is_requested()
    {
        writeln!(output, "{message}")?;
        return Ok(ExitCode::FAILURE);
    }
    Ok(result)
}

struct BatchPlan {
    workspace: BatchWorkspace,
    entries: Vec<PlanEntry>,
    discovery_errors: Vec<(PathBuf, io::Error)>,
    found: usize,
    discovery_complete: bool,
}

struct PlanEntry {
    id: String,
    path: PathBuf,
    diagnosis: &'static str,
    action: Action,
}

enum Action {
    Report(Status, String),
    Update {
        location: ContestLocation,
        update: Update,
    },
}

enum Update {
    Refresh {
        baseline: Box<RefreshBaseline>,
        force: bool,
        healthy: bool,
        reason: String,
    },
    Migrate(Box<MigrationPlan>),
}

impl BatchPlan {
    fn scan(
        cwd: &Path,
        operation: BatchOperation,
        cancellation: UpdateCancellation<'_>,
    ) -> io::Result<Self> {
        let (workspace, discovery) = BatchWorkspace::scan(cwd, cancellation)?;
        let entries = discovery
            .contests
            .into_iter()
            .take_while(|_| !cancellation.is_requested())
            .map(|(id, path, location)| {
                let mut diagnosis = "Blocked";
                let planned = (|| {
                    let location = location?;
                    workspace.revalidate_location(&id, &location)?;
                    let local = diagnose_contest(&path, &id);
                    #[cfg(test)]
                    if let Some(checkpoint) = cancellation.checkpoint {
                        checkpoint(
                            crate::workspace::update_cancellation::UpdateCheckpoint::AfterDiagnosis,
                        );
                    }
                    diagnosis = diagnosis_label(&local);
                    let action = plan_action(operation, &path, &id, location, local)?;
                    if let Action::Update { location, .. } = &action {
                        workspace.revalidate_location(&id, location)?;
                    }
                    Ok::<_, AppError>(action)
                })();
                let action = planned
                    .unwrap_or_else(|error| Action::Report(Status::Failed, error.to_string()));
                PlanEntry {
                    id,
                    path,
                    diagnosis,
                    action,
                }
            })
            .collect();
        Ok(Self {
            workspace,
            entries,
            discovery_errors: discovery.errors,
            found: discovery.found,
            discovery_complete: discovery.complete,
        })
    }

    fn has_updates(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.action, Action::Update { .. }))
    }

    fn write(
        &self,
        output: &mut dyn Write,
        cancellation: UpdateCancellation<'_>,
    ) -> io::Result<()> {
        if cancellation.is_requested() {
            return Ok(());
        }
        writeln!(output, "Found {} contests.", self.found)?;
        writeln!(output, "Plan:")?;
        for (path, error) in &self.discovery_errors {
            if cancellation.is_requested() {
                return Ok(());
            }
            writeln!(output, "  {}  BLOCKED  {error}", path.display())?;
        }
        for entry in &self.entries {
            if cancellation.is_requested() {
                return Ok(());
            }
            write!(
                output,
                "  {}  {}  {}  ",
                entry.id,
                entry.path.display(),
                entry.diagnosis
            )?;
            match &entry.action {
                Action::Report(status, reason) => writeln!(output, "{}  {reason}", status.label())?,
                Action::Update {
                    update: Update::Refresh { reason, .. },
                    ..
                } => {
                    writeln!(output, "REFRESH PLANNED  {reason}")?;
                }
                Action::Update {
                    update: Update::Migrate(plan),
                    ..
                } => {
                    writeln!(output, "MIGRATION PLANNED")?;
                    plan.write_summary(output)?;
                }
            }
        }
        output.flush()
    }

    fn run(
        self,
        run: bool,
        dry_run: bool,
        cancellation: UpdateCancellation<'_>,
        create_client: impl Fn() -> Result<AtCoderClient, AppError>,
        reporter: &mut dyn Reporter,
        output: &mut dyn Write,
    ) -> Result<ExitCode, AppError> {
        let diagnosed = self.entries.len();
        let mut counts = BTreeMap::<Status, usize>::new();
        let mut failures = Vec::new();
        let mut stopped = None;
        let mut client = None;
        for (index, entry) in self.entries.into_iter().enumerate() {
            let PlanEntry {
                id, path, action, ..
            } = entry;
            let progress = format!("[{}/{}] {id}", index + 1, self.found);
            if cancellation.is_requested() {
                stopped = Some(StopReason::Interrupted);
            }
            let (status, reason) = match action {
                Action::Report(status, reason) => (status, reason),
                Action::Update { .. } if stopped.is_some() || (!run && !dry_run) => (
                    Status::NotRun,
                    stopped
                        .map_or("changes were not approved", StopReason::message)
                        .into(),
                ),
                Action::Update { update, .. } if dry_run => {
                    (update.planned_status(), String::new())
                }
                Action::Update { location, update } => {
                    writeln!(output, "{progress}  {}", update.running_label())?;
                    let validate_route = |_: &Path| self.workspace.revalidate(&id, &location);
                    let result = update.execute(
                        &path,
                        &id,
                        || get_client(&mut client, &create_client),
                        reporter,
                        ContestUpdateChecks {
                            validate_route: &validate_route,
                            cancellation,
                        },
                    );
                    match result {
                        Ok(result) => result,
                        Err(AppError::AtCoder(AtCoderError::BatchInterrupted)) => {
                            stopped = Some(StopReason::Interrupted);
                            (Status::NotRun, StopReason::Interrupted.message().into())
                        }
                        Err(AppError::Io(error)) if UpdateCancelled::is(&error) => {
                            stopped = Some(StopReason::Interrupted);
                            (Status::NotRun, error.to_string())
                        }
                        Err(error) => {
                            match &error {
                                AppError::AtCoder(AtCoderError::RateLimited { .. }) => {
                                    stopped = Some(StopReason::RateLimited);
                                }
                                AppError::AtCoder(AtCoderError::RetryAfterExceedsLimit {
                                    ..
                                }) => {
                                    stopped = Some(StopReason::RetryAfterExceedsLimit);
                                }
                                _ => {}
                            }
                            (Status::Failed, error.to_string())
                        }
                    }
                }
            };
            if reason.is_empty() {
                writeln!(output, "{progress}  {}", status.label())?;
            } else {
                writeln!(output, "{progress}  {}  {reason}", status.label())?;
            }
            *counts.entry(status).or_default() += 1;
            if status == Status::Failed {
                failures.push((id, path, reason));
            }
        }

        writeln!(output, "\nSummary:")?;
        if self.discovery_complete {
            writeln!(output, "  {:<22} {:>5}", "Scanned:", self.found)?;
        } else {
            writeln!(
                output,
                "  {:<22} {:>5}",
                "Discovered (partial):", self.found
            )?;
        }
        if !self.discovery_complete || diagnosed != self.found || cancellation.is_requested() {
            writeln!(output, "  {:<22} {diagnosed:>5}", "Diagnosed:")?;
            writeln!(
                output,
                "  {:<22} {:>5}",
                "Undiagnosed:",
                self.found - diagnosed
            )?;
        }
        for (status, label) in Status::SUMMARY {
            let count = counts.get(&status).copied().unwrap_or_default();
            if count != 0
                || matches!(status, Status::Healthy | Status::Failed)
                || (status == Status::NotRun && cancellation.is_requested())
            {
                writeln!(output, "  {label:<22} {count:>5}")?;
            }
        }
        if !self.discovery_errors.is_empty() {
            writeln!(
                output,
                "  {:<22} {:>5}",
                "Discovery errors:",
                self.discovery_errors.len()
            )?;
        }
        if !failures.is_empty() || !self.discovery_errors.is_empty() {
            writeln!(output, "Failures:")?;
            for (path, error) in &self.discovery_errors {
                writeln!(output, "  {}: {error}", path.display())?;
            }
            for (id, path, reason) in &failures {
                writeln!(output, "  {id} at {}: {reason}", path.display())?;
            }
        }
        if cancellation.is_requested() {
            stopped = Some(StopReason::Interrupted);
        }
        if let Some(reason) = stopped {
            writeln!(output, "Batch stopped: {}.", reason.message())?;
        }
        output.flush()?;
        Ok(
            if cancellation.is_requested() || matches!(stopped, Some(StopReason::Interrupted)) {
                ExitCode::from(130)
            } else if failures.is_empty() && self.discovery_errors.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            },
        )
    }
}

fn get_client<'a>(
    client: &'a mut Option<AtCoderClient>,
    create_client: &impl Fn() -> Result<AtCoderClient, AppError>,
) -> Result<&'a AtCoderClient, AppError> {
    if client.is_none() {
        *client = Some(create_client()?);
    }
    Ok(client.as_ref().expect("batch client has been initialized"))
}

fn diagnosis_label(diagnosis: &ContestDiagnosis) -> &'static str {
    match diagnosis {
        ContestDiagnosis::Healthy { .. } => "Healthy",
        ContestDiagnosis::RepairRequired(_) => "RepairRequired",
        ContestDiagnosis::MigrationRequired(_) => "MigrationRequired",
        ContestDiagnosis::Blocked(_) => "Blocked",
        ContestDiagnosis::MissingDirectory => "MissingDirectory",
    }
}

fn plan_action(
    operation: BatchOperation,
    path: &Path,
    id: &str,
    location: ContestLocation,
    local: ContestDiagnosis,
) -> Result<Action, AppError> {
    let update = match operation {
        BatchOperation::Migrate => match migration::inspect_with_diagnosis(path, id, local)? {
            MigrationInspection::Healthy => {
                return Ok(Action::Report(Status::Healthy, "healthy".into()));
            }
            MigrationInspection::RefreshRequired => {
                return Ok(Action::Report(
                    Status::RefreshRequired,
                    "only samples need repair; run `atc refresh --all`".into(),
                ));
            }
            MigrationInspection::Ready(plan) => Update::Migrate(plan),
        },
        BatchOperation::Refresh { force } => {
            let (healthy, reason) = match &local {
                ContestDiagnosis::Healthy { .. } if !force => {
                    return Ok(Action::Report(Status::Healthy, "healthy".into()));
                }
                ContestDiagnosis::Healthy { .. } => (true, "forced update of healthy data".into()),
                ContestDiagnosis::RepairRequired(reason) => (false, repair_description(reason)),
                ContestDiagnosis::MigrationRequired(MigrationReason::UnsupportedVersion(
                    version,
                )) => {
                    return Ok(Action::Report(
                        Status::MigrationRequired,
                        format!(
                            "unsupported contest metadata version {version}; inspect with `atc migrate --all --dry-run`"
                        ),
                    ));
                }
                ContestDiagnosis::Blocked(reason) => {
                    let (kind, message) = reason.error_details();
                    return Err(io::Error::new(kind, message).into());
                }
                ContestDiagnosis::MissingDirectory => {
                    return Err(crate::workspace::missing_contest_directory(path).into());
                }
            };
            let baseline = plan_refresh_update(path, id, force, local)?;
            Update::Refresh {
                baseline: Box::new(baseline),
                force,
                healthy,
                reason,
            }
        }
    };
    Ok(Action::Update { location, update })
}

fn repair_description(reason: &RepairReason) -> String {
    match reason {
        RepairReason::Metadata(issue) => format!("contest metadata needs repair ({issue:?})"),
        RepairReason::TestsDirectoryMissing => "tests directory is missing".into(),
        RepairReason::ProblemTestsDirectoryMissing { index } => {
            format!("tests for problem {index} are missing")
        }
        RepairReason::SampleLayout { index, issue } => {
            format!("problem {index} has an invalid sample layout ({issue:?})")
        }
        RepairReason::SampleCountMismatch {
            index,
            expected,
            found,
        } => format!("problem {index} has {found} sample pairs; expected {expected}"),
        RepairReason::InvalidSamples { index } => format!("problem {index} has invalid samples"),
    }
}

impl Update {
    fn planned_status(&self) -> Status {
        match self {
            Self::Refresh { .. } => Status::PlannedRefresh,
            Self::Migrate(_) => Status::PlannedMigration,
        }
    }

    fn running_label(&self) -> &'static str {
        match self {
            Self::Refresh { .. } => "REFRESHING",
            Self::Migrate(_) => "MIGRATING",
        }
    }

    fn execute<'a>(
        self,
        path: &Path,
        id: &str,
        client: impl FnOnce() -> Result<&'a AtCoderClient, AppError>,
        reporter: &mut dyn Reporter,
        checks: ContestUpdateChecks<'_>,
    ) -> Result<(Status, String), AppError> {
        match self {
            Self::Refresh {
                baseline,
                force,
                healthy,
                ..
            } => {
                execute_cli_refresh(path, id, force, *baseline, client, reporter, checks)?;
                Ok((
                    if healthy {
                        Status::Refreshed
                    } else {
                        Status::Repaired
                    },
                    String::new(),
                ))
            }
            Self::Migrate(plan) => {
                let backup = execute_migration(*plan, client, reporter, checks)?;
                Ok((Status::Migrated, format!("backup at {}", backup.display())))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum StopReason {
    Interrupted,
    RateLimited,
    RetryAfterExceedsLimit,
}

impl StopReason {
    fn message(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted by Ctrl+C",
            Self::RateLimited => "HTTP 429 persisted after retries",
            Self::RetryAfterExceedsLimit => "HTTP 429 Retry-After exceeds the wait limit",
        }
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Status {
    Healthy,
    PlannedRefresh,
    PlannedMigration,
    Repaired,
    Refreshed,
    Migrated,
    MigrationRequired,
    RefreshRequired,
    Failed,
    NotRun,
}

impl Status {
    const SUMMARY: [(Self, &'static str); 10] = [
        (Self::Healthy, "Healthy skipped:"),
        (Self::PlannedRefresh, "Refresh planned:"),
        (Self::PlannedMigration, "Migration planned:"),
        (Self::Repaired, "Repaired:"),
        (Self::Refreshed, "Refreshed:"),
        (Self::Migrated, "Migrated:"),
        (Self::MigrationRequired, "Migration required:"),
        (Self::RefreshRequired, "Refresh required:"),
        (Self::Failed, "Failed:"),
        (Self::NotRun, "Not run:"),
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Healthy => "SKIPPED",
            Self::PlannedRefresh => "REFRESH PLANNED",
            Self::PlannedMigration => "MIGRATION PLANNED",
            Self::Repaired => "REPAIRED",
            Self::Refreshed => "REFRESHED",
            Self::Migrated => "MIGRATED",
            Self::MigrationRequired => "MIGRATION REQUIRED",
            Self::RefreshRequired => "REFRESH REQUIRED",
            Self::Failed => "FAILED",
            Self::NotRun => "NOT RUN",
        }
    }
}

#[cfg(unix)]
fn install_interrupt_handler() -> io::Result<()> {
    extern "C" fn on_interrupt(_: libc::c_int) {
        INTERRUPTED.store(true, Ordering::SeqCst);
    }

    // SAFETY: action is initialized before installation and the handler only uses an atomic store.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_interrupt as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn install_interrupt_handler() -> io::Result<()> {
    use windows_sys::Win32::System::Console::{CTRL_C_EVENT, SetConsoleCtrlHandler};

    unsafe extern "system" fn on_interrupt(event: u32) -> windows_sys::core::BOOL {
        if event != CTRL_C_EVENT {
            return 0;
        }
        INTERRUPTED.store(true, Ordering::SeqCst);
        1
    }

    // SAFETY: the handler has the required system ABI and its function pointer remains valid.
    if unsafe { SetConsoleCtrlHandler(Some(on_interrupt), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn install_interrupt_handler() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "batch updates require Ctrl+C handling on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safe_file::{self, read_tree_snapshot};
    use crate::ui::{Event, NullReporter};
    use crate::workspace::{lock_contest_update, update_cancellation::UpdateCheckpoint};
    use std::cell::Cell;
    use std::fs;

    fn workspace() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".atc-workspace.toml"), "version = 1\n").unwrap();
        root
    }

    fn repairable(root: &Path, id: &str, count: usize) -> PathBuf {
        let destination = root.join(id);
        fs::create_dir_all(destination.join(".atc")).unwrap();
        fs::write(
            destination.join(".atc/contest.toml"),
            format!(
                "version = 1\ncontest_id = '{id}'\n[[problems]]\nindex = 'A'\ntitle = 'A'\n\
                 task_id = '{id}_a'\nurl = 'https://atcoder.jp/contests/{id}/tasks/{id}_a'\n\
                 sample_count = {count}\n"
            ),
        )
        .unwrap();
        destination
    }

    fn fixture_client() -> AtCoderClient {
        AtCoderClient::fixture(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures"))
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InterruptionPoint {
        BeforeStaging,
        BeforeApply,
        BeforeIntent,
        AfterIntent,
    }

    fn assert_batch_interruption(interrupt_at: InterruptionPoint) {
        let interrupted = AtomicBool::new(false);
        assert_batch_interruption_with_signal(interrupt_at, &interrupted, || {
            interrupted.store(true, Ordering::SeqCst);
        });
    }

    fn assert_batch_interruption_with_signal(
        interrupt_at: InterruptionPoint,
        interrupted: &AtomicBool,
        signal: impl Fn(),
    ) {
        for (operation, name, completed) in [
            (
                BatchOperation::Refresh { force: false },
                "refresh",
                "REPAIRED",
            ),
            (BatchOperation::Migrate, "migrate", "MIGRATED"),
        ] {
            interrupted.store(false, Ordering::SeqCst);
            let root = workspace();
            let first = repairable(root.path(), "abc466", 2);
            let next = repairable(root.path(), "abc467", 2);
            for contest in [&first, &next] {
                if matches!(operation, BatchOperation::Migrate) {
                    let id = contest.file_name().unwrap().to_str().unwrap();
                    fs::write(
                        contest.join(".atc/contest.toml"),
                        format!("version = 99\ncontest_id = '{id}'\n"),
                    )
                    .unwrap();
                }
                fs::create_dir_all(contest.join("tests/A")).unwrap();
                fs::write(contest.join("tests/A/sample-1.in"), b"old input\n").unwrap();
                fs::write(contest.join("tests/A/sample-1.out"), b"old output\n").unwrap();
                fs::write(contest.join("A.cpp"), b"source\r\n").unwrap();
                fs::write(contest.join(".atc/user-data"), b"user data\n").unwrap();
            }
            let config = fs::read(root.path().join(".atc-workspace.toml")).unwrap();
            let first_before = read_tree_snapshot(&first).unwrap();
            let next_before = read_tree_snapshot(&next).unwrap();
            let metadata = fs::read(first.join(".atc/contest.toml")).unwrap();
            let tests = read_tree_snapshot(&first.join("tests")).unwrap();
            let diagnosis = diagnose_contest(&first, "abc466");
            let checkpoint = |checkpoint: UpdateCheckpoint<'_>| {
                let observed = match checkpoint {
                    UpdateCheckpoint::Discovery
                    | UpdateCheckpoint::AfterDiagnosis
                    | UpdateCheckpoint::UnderLock => return,
                    UpdateCheckpoint::BeforeStaging => {
                        assert_eq!(read_tree_snapshot(&first).unwrap(), first_before);
                        InterruptionPoint::BeforeStaging
                    }
                    UpdateCheckpoint::BeforeApply(staging) => {
                        assert!(staging.join(".atc/contest.toml").is_file());
                        assert!(!first.join(".atc-update.lock").exists());
                        assert!(!staging.join(safe_file::RECOVERY_INTENT).exists());
                        InterruptionPoint::BeforeApply
                    }
                    UpdateCheckpoint::BeforeIntent(staging) => {
                        assert!(!staging.join(safe_file::RECOVERY_INTENT).exists());
                        assert_eq!(
                            lock_contest_update(&first, false).unwrap_err().kind(),
                            io::ErrorKind::WouldBlock
                        );
                        InterruptionPoint::BeforeIntent
                    }
                    UpdateCheckpoint::AfterIntent(staging) => {
                        assert!(staging.join(safe_file::RECOVERY_INTENT).is_file());
                        assert_eq!(fs::read(first.join(".atc/contest.toml")).unwrap(), metadata);
                        assert_eq!(read_tree_snapshot(&first.join("tests")).unwrap(), tests);
                        InterruptionPoint::AfterIntent
                    }
                };
                if observed == interrupt_at {
                    signal();
                }
            };
            let mut cancellation = UpdateCancellation::for_batch(interrupted);
            cancellation.checkpoint = Some(&checkpoint);
            let mut output = Vec::new();
            let code = BatchPlan::scan(root.path(), operation, UpdateCancellation::default())
                .unwrap()
                .run(
                    true,
                    false,
                    cancellation,
                    || Ok(fixture_client()),
                    &mut NullReporter,
                    &mut output,
                )
                .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert_eq!(
                code,
                ExitCode::from(130),
                "{name} at {interrupt_at:?}: {output}"
            );
            assert!(
                output.contains("[2/2] abc467  NOT RUN  interrupted by Ctrl+C"),
                "{output}"
            );
            assert_eq!(
                output
                    .lines()
                    .filter(|line| line.starts_with("[2/2]"))
                    .count(),
                1,
                "{output}"
            );
            let summary = output.split_once("Summary:\n").unwrap().1;
            let summary_count = |label: &str| {
                summary
                    .lines()
                    .find_map(|line| line.trim_start().strip_prefix(label))
                    .unwrap_or_else(|| panic!("{label} missing from {summary}"))
                    .trim()
                    .parse::<usize>()
                    .unwrap()
            };
            assert_eq!(summary_count("Failed:"), 0, "{output}");
            assert_eq!(read_tree_snapshot(&next).unwrap(), next_before);
            assert_eq!(
                fs::read(root.path().join(".atc-workspace.toml")).unwrap(),
                config
            );
            assert_eq!(fs::read(first.join("A.cpp")).unwrap(), b"source\r\n");
            assert_eq!(
                fs::read(first.join(".atc/user-data")).unwrap(),
                b"user data\n"
            );

            let prefixed_directories = |prefix: &str| {
                fs::read_dir(&first)
                    .unwrap()
                    .map(|entry| entry.unwrap())
                    .filter(|entry| entry.file_name().to_str().unwrap().starts_with(prefix))
                    .map(|entry| entry.path())
                    .collect::<Vec<_>>()
            };
            let staging = prefixed_directories(&format!(".atc-{name}-"));
            let archives = prefixed_directories(&format!(".atc-recovery-{name}-"));
            if interrupt_at == InterruptionPoint::AfterIntent {
                assert!(
                    output.contains(&format!("[1/2] abc466  {completed}")),
                    "{output}"
                );
                assert_eq!(summary_count("Not run:"), 1, "{output}");
                let completed_label = match operation {
                    BatchOperation::Refresh { .. } => "Repaired:",
                    BatchOperation::Migrate => "Migrated:",
                };
                assert_eq!(summary_count(completed_label), 1, "{output}");
                assert!(staging.is_empty());
                let [archive] = archives.as_slice() else {
                    panic!("expected one completed recovery archive")
                };
                assert_eq!(
                    safe_file::read_completed_recovery(archive).unwrap(),
                    safe_file::RecoveryIdentity::new("contest", "abc466", None)
                );
                assert_eq!(
                    fs::read(archive.join("previous-contest.toml")).unwrap(),
                    metadata
                );
                assert_eq!(
                    read_tree_snapshot(&archive.join("previous-tests")).unwrap(),
                    tests
                );
                assert!(matches!(
                    diagnose_contest(&first, "abc466"),
                    ContestDiagnosis::Healthy { .. }
                ));
            } else {
                assert!(
                    output.contains("[1/2] abc466  NOT RUN  interrupted by Ctrl+C"),
                    "{output}"
                );
                assert_eq!(summary_count("Not run:"), 2, "{output}");
                assert!(archives.is_empty());
                assert_eq!(fs::read(first.join(".atc/contest.toml")).unwrap(), metadata);
                assert_eq!(read_tree_snapshot(&first.join("tests")).unwrap(), tests);
                assert_eq!(diagnose_contest(&first, "abc466"), diagnosis);
                assert_eq!(
                    first.join(".atc-update.lock").is_file(),
                    interrupt_at == InterruptionPoint::BeforeIntent
                );
                if interrupt_at == InterruptionPoint::BeforeStaging {
                    assert!(staging.is_empty());
                    assert_eq!(read_tree_snapshot(&first).unwrap(), first_before);
                } else {
                    let [staging] = staging.as_slice() else {
                        panic!("expected one preserved staging directory")
                    };
                    assert!(!staging.join(safe_file::RECOVERY_INTENT).exists());
                    assert!(!staging.join(safe_file::RECOVERY_COMPLETE).exists());
                    assert!(!staging.join("previous-tests").exists());
                    assert!(!staging.join("previous-contest.toml").exists());
                }
            }

            // Retry just the interrupted target through its normal update path. The
            // staged-but-uncommitted data and completed archives must allow reopening it.
            let single_root = first.parent().unwrap();
            let tree_before_rerun = read_tree_snapshot(&next).unwrap();
            if matches!(operation, BatchOperation::Migrate) {
                let plan = migration::inspect(first.clone(), "abc466".into()).unwrap();
                if interrupt_at == InterruptionPoint::AfterIntent {
                    assert!(matches!(plan, MigrationInspection::Healthy));
                } else {
                    let MigrationInspection::Ready(plan) = plan else {
                        panic!("interrupted migration must be eligible for retry");
                    };
                    execute_migration(
                        *plan,
                        || Ok(fixture_client()),
                        &mut NullReporter,
                        ContestUpdateChecks::default(),
                    )
                    .unwrap();
                }
            } else {
                super::super::refresh::refresh_from(
                    single_root,
                    Some("abc466"),
                    false,
                    || Ok(fixture_client()),
                    &mut NullReporter,
                )
                .unwrap();
            }
            assert!(matches!(
                diagnose_contest(&first, "abc466"),
                ContestDiagnosis::Healthy { .. }
            ));
            assert_eq!(read_tree_snapshot(&next).unwrap(), tree_before_rerun);
        }
    }

    #[test]
    fn batch_interrupt_before_staging_creates_no_files() {
        assert_batch_interruption(InterruptionPoint::BeforeStaging);
    }

    #[test]
    fn batch_interrupt_after_staging_skips_taking_the_update_lock() {
        assert_batch_interruption(InterruptionPoint::BeforeApply);
    }

    #[test]
    fn batch_interrupt_under_lock_skips_recording_intent_and_replacing_data() {
        assert_batch_interruption(InterruptionPoint::BeforeIntent);
    }

    #[test]
    fn batch_interrupt_after_intent_finishes_the_current_contest_only() {
        assert_batch_interruption(InterruptionPoint::AfterIntent);
    }

    #[cfg(windows)]
    #[test]
    fn windows_console_ctrl_c_during_discovery_diagnosis_and_replacement() {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        use wait_timeout::ChildExt;
        use windows_sys::Win32::System::Console::{
            CTRL_C_EVENT, GenerateConsoleCtrlEvent, GetConsoleProcessList,
        };
        use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

        const CHILD: &str = "ATC_BATCH_UNIT_CONSOLE_TEST";
        if std::env::var_os(CHILD).is_none() {
            let mut driver = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::batch::tests::windows_console_ctrl_c_during_discovery_diagnosis_and_replacement", "--nocapture"])
                .env(CHILD, "1")
                .creation_flags(CREATE_NEW_CONSOLE)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            if driver
                .wait_timeout(Duration::from_secs(30))
                .unwrap()
                .is_none()
            {
                let _ = driver.kill();
                let _ = driver.wait();
                panic!("isolated console test did not finish");
            }
            let output = driver.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        install_interrupt_handler().unwrap();
        let mut process = 0u32;
        // SAFETY: the single process ID buffer is writable.
        assert_eq!(unsafe { GetConsoleProcessList(&mut process, 1) }, 1);
        assert_eq!(process, std::process::id());
        let signal = || {
            // SAFETY: group zero is confined to the private console created above.
            assert_ne!(unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) }, 0);
            let deadline = Instant::now() + Duration::from_secs(3);
            while !INTERRUPTED.load(Ordering::SeqCst) {
                assert!(
                    Instant::now() < deadline,
                    "Windows handler did not receive CTRL_C_EVENT"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        for discovery in [true, false] {
            INTERRUPTED.store(false, Ordering::SeqCst);
            let root = workspace();
            repairable(root.path(), "abc466", 2);
            repairable(root.path(), "abc467", 2);
            let before = read_tree_snapshot(root.path()).unwrap();
            let checkpoint = |point: UpdateCheckpoint<'_>| match point {
                UpdateCheckpoint::Discovery if discovery => signal(),
                UpdateCheckpoint::AfterDiagnosis if !discovery => signal(),
                _ => (),
            };
            let mut cancellation = UpdateCancellation::for_batch(&INTERRUPTED);
            cancellation.checkpoint = Some(&checkpoint);
            let plan = BatchPlan::scan(
                root.path(),
                BatchOperation::Refresh { force: false },
                cancellation,
            )
            .unwrap();
            let mut output = Vec::new();
            plan.write(&mut output, cancellation).unwrap();
            assert!(output.is_empty());
            let code = plan
                .run(
                    true,
                    false,
                    cancellation,
                    || panic!("interrupted planning must not initialize a client"),
                    &mut NullReporter,
                    &mut output,
                )
                .unwrap();
            assert_eq!(code, ExitCode::from(130));
            let output = String::from_utf8(output).unwrap();
            if discovery {
                assert!(output.contains("Discovered (partial):      0"), "{output}");
                assert!(output.contains("Diagnosed:                 0"), "{output}");
            } else {
                assert!(output.contains("Scanned:                   2"), "{output}");
                assert!(output.contains("Diagnosed:                 1"), "{output}");
                assert!(output.contains("Undiagnosed:               1"), "{output}");
                assert!(output.contains("Not run:                   1"), "{output}");
            }
            assert_eq!(read_tree_snapshot(root.path()).unwrap(), before);
        }
        INTERRUPTED.store(false, Ordering::SeqCst);
        let cancellation = UpdateCancellation::for_batch(&INTERRUPTED);
        let root = workspace();
        repairable(root.path(), "abc466", 2);
        repairable(root.path(), "abc467", 2);
        let before = read_tree_snapshot(root.path()).unwrap();
        let plan = BatchPlan::scan(
            root.path(),
            BatchOperation::Refresh { force: false },
            cancellation,
        )
        .unwrap();

        struct InterruptOnPlan<F>(Vec<u8>, F);
        impl<F: Fn()> Write for InterruptOnPlan<F> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes == b"Plan:\n" {
                    self.1();
                }
                self.0.write(bytes)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut output = InterruptOnPlan(Vec::new(), signal);
        plan.write(&mut output, cancellation).unwrap();
        let mut output = output.0;
        assert!(output.ends_with(b"Plan:\n"));
        assert_eq!(
            plan.run(
                true,
                false,
                cancellation,
                || panic!("interrupted plan must not initialize a client"),
                &mut NullReporter,
                &mut output
            )
            .unwrap(),
            ExitCode::from(130),
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Diagnosed:                 2"), "{output}");
        assert!(output.contains("Undiagnosed:               0"), "{output}");
        assert!(output.contains("Not run:                   2"), "{output}");
        assert_eq!(read_tree_snapshot(root.path()).unwrap(), before);

        for point in [
            InterruptionPoint::BeforeApply,
            InterruptionPoint::BeforeIntent,
            InterruptionPoint::AfterIntent,
        ] {
            assert_batch_interruption_with_signal(point, &INTERRUPTED, signal);
        }
    }

    #[test]
    fn one_hundred_healthy_contests_never_initialize_the_http_client() {
        let root = workspace();
        for index in 1..=100 {
            repairable(root.path(), &format!("abc{index:03}"), 0);
        }
        let before = read_tree_snapshot(root.path()).unwrap();
        for operation in [
            BatchOperation::Refresh { force: false },
            BatchOperation::Migrate,
        ] {
            let mut output = Vec::new();
            let ok = BatchPlan::scan(root.path(), operation, UpdateCancellation::default())
                .unwrap()
                .run(
                    true,
                    false,
                    UpdateCancellation::default(),
                    || panic!("healthy workspace must not initialize a client"),
                    &mut NullReporter,
                    &mut output,
                )
                .unwrap();
            assert_eq!(ok, ExitCode::SUCCESS);
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("Healthy skipped:         100"), "{output}");
        }
        assert_eq!(read_tree_snapshot(root.path()).unwrap(), before);
    }

    #[test]
    fn declining_the_batch_preserves_the_entire_workspace() {
        let root = workspace();
        for id in ["abc466", "abc467"] {
            let tests = root.path().join(id).join("tests");
            fs::create_dir_all(&tests).unwrap();
            fs::write(tests.join("legacy"), b"old tests").unwrap();
        }
        let before = read_tree_snapshot(root.path()).unwrap();
        let plan = BatchPlan::scan(
            root.path(),
            BatchOperation::Migrate,
            UpdateCancellation::default(),
        )
        .unwrap();
        let mut output = Vec::new();
        plan.write(&mut output, UpdateCancellation::default())
            .unwrap();
        let approved = confirm_migration(&mut b"n\n".as_slice(), &mut output).unwrap();
        assert!(!approved);
        let code = plan
            .run(
                approved,
                false,
                UpdateCancellation::default(),
                || panic!("declined plan must not initialize a client"),
                &mut NullReporter,
                &mut output,
            )
            .unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(read_tree_snapshot(root.path()).unwrap(), before);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("Proceed with migration?").count(), 1);
        assert!(
            output.find("Migrate abc467").unwrap()
                < output.find("Proceed with migration?").unwrap()
        );
    }

    struct DuringFetch<'a>(Box<dyn FnOnce() + 'a>);

    impl Reporter for DuringFetch<'_> {
        fn report(&mut self, event: Event<'_>) {
            if matches!(
                event,
                Event::ContestFetching {
                    contest_id: "abc460"
                }
            ) {
                let action = std::mem::replace(&mut self.0, Box::new(|| {}));
                action();
            }
        }
    }

    #[test]
    fn single_and_batch_refresh_reject_changes_to_the_planned_generation_during_fetch() {
        struct OnLastSample<F>(Option<F>);
        impl<F: FnOnce()> Reporter for OnLastSample<F> {
            fn report(&mut self, event: Event<'_>) {
                if matches!(event, Event::ProblemFetched { index: "G", .. }) {
                    self.0.take().unwrap()();
                }
            }
        }

        let mapping = |pattern: &str, path: &str| {
            format!("version = 1\n[[paths]]\npattern = '{pattern}'\npath = '{path}'\n")
        };
        let add_samples = |contest: &Path| {
            let tests = contest.join("tests/A");
            fs::create_dir_all(&tests).unwrap();
            // The first missing output keeps the diagnosis identical across edits to other files.
            fs::write(tests.join("sample-1.in"), "local input").unwrap();
            fs::write(tests.join("sample-2.in"), "second input").unwrap();
            fs::write(tests.join("sample-2.out"), "second output").unwrap();
        };

        for batch in [false, true] {
            for change in [
                "body",
                "add",
                "delete",
                "replace",
                "route",
                "rules",
                "directory",
                "boundary",
            ] {
                let root = workspace();
                let root = root.path();
                let config = root.join(".atc-workspace.toml");
                fs::write(&config, mapping("^abc466$", "old/series")).unwrap();
                let contest = repairable(&root.join("old/series"), "abc466", 3);
                add_samples(&contest);
                let mut edited = None;
                let mut reporter = OnLastSample(Some(|| {
                    let tests = contest.join("tests/A");
                    match change {
                        "body" => fs::write(tests.join("sample-1.in"), b"edited input").unwrap(),
                        "add" => {
                            fs::write(tests.join("sample-3.in"), b"new input").unwrap();
                            fs::write(tests.join("sample-3.out"), b"new output").unwrap();
                        }
                        "delete" => fs::remove_file(tests.join("sample-2.out")).unwrap(),
                        "replace" => {
                            fs::rename(tests.join("sample-2.in"), contest.join("saved-input"))
                                .unwrap();
                            fs::write(tests.join("sample-2.in"), b"second input").unwrap();
                        }
                        "route" => {
                            repairable(&root.join("new"), "abc466", 3);
                            fs::write(&config, mapping("^abc466$", "new")).unwrap();
                        }
                        "rules" => fs::write(&config, mapping("^abc", "old/series")).unwrap(),
                        "directory" => {
                            fs::rename(&contest, root.join("saved-contest")).unwrap();
                            add_samples(&repairable(&root.join("old/series"), "abc466", 3));
                        }
                        "boundary" => {
                            fs::write(root.join("old/.atc-workspace.toml"), "version = 1\n")
                                .unwrap()
                        }
                        _ => unreachable!(),
                    }
                    edited = Some(read_tree_snapshot(root).unwrap());
                }));

                let message = if batch {
                    let plan = BatchPlan::scan(
                        root,
                        BatchOperation::Refresh { force: false },
                        UpdateCancellation::default(),
                    )
                    .unwrap();
                    let mut output = Vec::new();
                    let code = plan
                        .run(
                            true,
                            false,
                            UpdateCancellation::default(),
                            || Ok(fixture_client()),
                            &mut reporter,
                            &mut output,
                        )
                        .unwrap();
                    let output = String::from_utf8(output).unwrap();
                    assert_eq!(code, ExitCode::FAILURE, "{change}: {output}");
                    output
                } else {
                    super::super::refresh::refresh_from(
                        root,
                        Some("abc466"),
                        false,
                        || Ok(fixture_client()),
                        &mut reporter,
                    )
                    .expect_err("changed data must not be replaced")
                    .to_string()
                };
                let reason = match change {
                    "route" | "rules" | "directory" if batch => {
                        "Workspace route or contest directory changed"
                    }
                    "route" | "rules" => "Workspace route changed",
                    "directory" => "contest destination changed",
                    "boundary" => "Workspace boundary at",
                    _ => "local tests changed",
                };
                assert!(
                    message.contains(reason),
                    "batch={batch}, {change}: {message}"
                );
                assert_eq!(
                    read_tree_snapshot(root).unwrap(),
                    edited.expect("edit must happen during fetch")
                );
            }
        }
    }

    #[test]
    fn refresh_preserves_sample_edits_at_the_locked_replacement_boundary() {
        for after_intent in [false, true] {
            let root = workspace();
            let contest = repairable(root.path(), "abc466", 2);
            let metadata = fs::read(contest.join(".atc/contest.toml")).unwrap();
            let tests = contest.join("tests/A");
            fs::create_dir_all(&tests).unwrap();
            let input = tests.join("sample-1.in");
            fs::write(&input, b"planned input").unwrap();
            fs::write(tests.join("sample-1.out"), b"planned output").unwrap();
            let touched_staging = Cell::new(None);
            let checkpoint = |checkpoint: UpdateCheckpoint<'_>| match checkpoint {
                UpdateCheckpoint::BeforeApply(staging) if !after_intent => {
                    fs::write(&input, b"edited input").unwrap();
                    touched_staging.set(Some(staging.to_path_buf()));
                }
                UpdateCheckpoint::AfterIntent(staging) if after_intent => {
                    fs::write(&input, b"edited input").unwrap();
                    touched_staging.set(Some(staging.to_path_buf()));
                }
                _ => {}
            };
            let interrupted = AtomicBool::new(false);
            let mut cancellation = UpdateCancellation::for_batch(&interrupted);
            cancellation.checkpoint = Some(&checkpoint);
            let plan = BatchPlan::scan(
                root.path(),
                BatchOperation::Refresh { force: false },
                UpdateCancellation::default(),
            )
            .unwrap();
            let mut output = Vec::new();
            let code = plan
                .run(
                    true,
                    false,
                    cancellation,
                    || Ok(fixture_client()),
                    &mut NullReporter,
                    &mut output,
                )
                .unwrap();
            let message = String::from_utf8(output).unwrap();
            assert_eq!(code, ExitCode::FAILURE, "{message}");
            let reason = if after_intent {
                "previous tests changed during replacement"
            } else {
                "local tests changed"
            };
            assert!(message.contains(reason), "{message}");
            let staging = touched_staging
                .into_inner()
                .expect("the edit must reach the update boundary");
            assert!(contest.join(".atc-update.lock").is_file());
            assert_eq!(
                staging.join(safe_file::RECOVERY_INTENT).is_file(),
                after_intent
            );
            assert!(!staging.join("previous-tests").exists());
            assert_eq!(fs::read(&input).unwrap(), b"edited input");
            assert_eq!(
                fs::read(contest.join(".atc/contest.toml")).unwrap(),
                metadata
            );
        }
    }

    #[test]
    fn locked_updates_reject_a_new_intermediate_workspace_without_replacing_data() {
        for operation in [
            BatchOperation::Refresh { force: false },
            BatchOperation::Migrate,
        ] {
            let root = workspace();
            let root = root.path();
            fs::write(
                root.join(".atc-workspace.toml"),
                "version = 1\n[[paths]]\npattern = '^abc466$'\npath = 'inner/series'\n",
            )
            .unwrap();
            let contest = repairable(&root.join("inner/series"), "abc466", 2);
            let manifest = contest.join(".atc/contest.toml");
            if matches!(operation, BatchOperation::Migrate) {
                fs::write(&manifest, "version = 99\ncontest_id = 'abc466'\n").unwrap();
            }
            let tests = contest.join("tests/A");
            fs::create_dir_all(&tests).unwrap();
            fs::write(tests.join("sample-1.in"), b"old input").unwrap();
            let metadata_before = fs::read(&manifest).unwrap();
            let tests_before = read_tree_snapshot(&tests).unwrap();
            let checkpoint = |point: UpdateCheckpoint<'_>| {
                if matches!(point, UpdateCheckpoint::UnderLock) {
                    assert_eq!(
                        lock_contest_update(&contest, false).unwrap_err().kind(),
                        io::ErrorKind::WouldBlock
                    );
                    fs::write(root.join("inner/.atc-workspace.toml"), "version = 1\n").unwrap();
                }
            };
            let mut cancellation = UpdateCancellation::default();
            cancellation.checkpoint = Some(&checkpoint);
            let plan = BatchPlan::scan(root, operation, cancellation).unwrap();
            let mut output = Vec::new();
            let status = plan
                .run(
                    true,
                    false,
                    cancellation,
                    || Ok(fixture_client()),
                    &mut NullReporter,
                    &mut output,
                )
                .unwrap();
            let message = String::from_utf8(output).unwrap();
            assert_eq!(status, ExitCode::FAILURE, "{message}");
            assert!(message.contains("Workspace boundary at"), "{message}");
            assert_eq!(fs::read(&manifest).unwrap(), metadata_before);
            assert_eq!(read_tree_snapshot(&tests).unwrap(), tests_before);
            let staging = fs::read_dir(&contest)
                .unwrap()
                .map(Result::unwrap)
                .find(|entry| {
                    let name = entry.file_name();
                    let name = name.to_str().unwrap();
                    name.starts_with(".atc-refresh-") || name.starts_with(".atc-migrate-")
                })
                .unwrap()
                .path();
            assert!(!staging.join(safe_file::RECOVERY_INTENT).exists());
            assert!(!staging.join("previous-tests").exists());
        }
    }

    #[test]
    fn refresh_revalidates_tests_changed_by_an_independent_edit_after_planning() {
        let root = workspace();
        repairable(root.path(), "abc460", 1);
        let changed = repairable(root.path(), "abc461", 2).join("tests/A");
        fs::create_dir_all(&changed).unwrap();
        fs::write(changed.join("sample-1.in"), b"original").unwrap();
        fs::write(changed.join("sample-1.out"), b"original output").unwrap();
        let repairable = repairable(root.path(), "abc466", 1);
        let mut reporter = DuringFetch(Box::new(|| {
            fs::write(changed.join("sample-1.in"), b"modified").unwrap()
        }));
        let plan = BatchPlan::scan(
            root.path(),
            BatchOperation::Refresh { force: false },
            UpdateCancellation::default(),
        )
        .unwrap();
        let mut output = Vec::new();
        let client_initializations = Cell::new(0);
        let code = plan
            .run(
                true,
                false,
                UpdateCancellation::default(),
                || {
                    client_initializations.set(client_initializations.get() + 1);
                    Ok(fixture_client())
                },
                &mut reporter,
                &mut output,
            )
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(client_initializations.get(), 1);
        assert!(
            output.contains("local tests changed while refresh was being prepared"),
            "{output}"
        );
        assert!(!root.path().join("abc461/.atc-update.lock").exists());
        assert_eq!(fs::read(changed.join("sample-1.in")).unwrap(), b"modified");
        assert!(repairable.join("tests/A/sample-1.in").is_file());
    }

    #[test]
    fn migration_revalidates_legacy_tests_changed_after_planning() {
        let root = workspace();
        for id in ["abc460", "abc461", "abc466"] {
            let tests = root.path().join(id).join("tests");
            fs::create_dir_all(&tests).unwrap();
            fs::write(tests.join("legacy"), b"old tests").unwrap();
        }
        let changed_file = root.path().join("abc461/tests/legacy");
        let mut reporter = DuringFetch(Box::new(|| {
            fs::write(&changed_file, b"new local input").unwrap()
        }));
        let plan = BatchPlan::scan(
            root.path(),
            BatchOperation::Migrate,
            UpdateCancellation::default(),
        )
        .unwrap();
        let mut output = Vec::new();
        let code = plan
            .run(
                true,
                false,
                UpdateCancellation::default(),
                || Ok(fixture_client()),
                &mut reporter,
                &mut output,
            )
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            output.contains("local contest data changed during migration"),
            "{output}"
        );
        assert!(!root.path().join("abc461/.atc").exists());
        assert_eq!(fs::read(&changed_file).unwrap(), b"new local input");
        assert!(root.path().join("abc466/tests/A/sample-1.in").is_file());
    }

    #[test]
    fn a_duplicate_created_after_planning_refuses_the_update() {
        let root = workspace();
        let original = repairable(root.path(), "abc466", 1);
        let plan = BatchPlan::scan(
            root.path(),
            BatchOperation::Refresh { force: false },
            UpdateCancellation::default(),
        )
        .unwrap();
        let series = root.path().join("ABC");
        fs::create_dir(&series).unwrap();
        let duplicate = series.join("ABC466");
        fs::create_dir_all(duplicate.join(".atc")).unwrap();
        fs::copy(
            original.join(".atc/contest.toml"),
            duplicate.join(".atc/contest.toml"),
        )
        .unwrap();
        let before = read_tree_snapshot(root.path()).unwrap();
        let mut output = Vec::new();
        let code = plan
            .run(
                true,
                false,
                UpdateCancellation::default(),
                || panic!("a duplicate must be detected before initializing the client"),
                &mut NullReporter,
                &mut output,
            )
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            output.contains("duplicate contest ID \"abc466\" after planning"),
            "{output}"
        );
        assert!(!original.join(".atc-update.lock").exists());
        assert!(!duplicate.join(".atc-update.lock").exists());
        assert_eq!(read_tree_snapshot(root.path()).unwrap(), before);
    }

    #[test]
    fn losing_case_variant_ownership_after_planning_refuses_migration() {
        let root = workspace();
        let original = repairable(root.path(), "abc466", 1);
        let destination = root.path().join("ABC466");
        fs::rename(&original, root.path().join("intermediate")).unwrap();
        fs::rename(root.path().join("intermediate"), &destination).unwrap();
        fs::create_dir(destination.join("tests")).unwrap();
        fs::write(destination.join("tests/legacy.in"), b"legacy").unwrap();
        let plan = BatchPlan::scan(
            root.path(),
            BatchOperation::Migrate,
            UpdateCancellation::default(),
        )
        .unwrap();
        assert!(plan.has_updates());

        fs::remove_file(destination.join(".atc/contest.toml")).unwrap();
        let before = read_tree_snapshot(root.path()).unwrap();
        let mut output = Vec::new();
        let code = plan
            .run(
                true,
                false,
                UpdateCancellation::default(),
                || {
                    panic!(
                        "lost ownership evidence must be detected before initializing the client"
                    )
                },
                &mut NullReporter,
                &mut output,
            )
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            output.contains("cannot confirm contest ownership"),
            "{output}"
        );
        assert_eq!(read_tree_snapshot(root.path()).unwrap(), before);
    }

    #[test]
    fn persistent_rate_limit_marks_later_updates_not_run() {
        for (error, reason) in [
            (
                AtCoderError::RateLimited {
                    url: "https://atcoder.jp/contests/abc460/tasks".into(),
                },
                "HTTP 429 persisted after retries",
            ),
            (
                AtCoderError::RetryAfterExceedsLimit {
                    url: "https://atcoder.jp/contests/abc460/tasks".into(),
                },
                "HTTP 429 Retry-After exceeds the wait limit",
            ),
        ] {
            let root = workspace();
            repairable(root.path(), "abc460", 1);
            repairable(root.path(), "abc466", 1);
            let before = read_tree_snapshot(root.path()).unwrap();
            let plan = BatchPlan::scan(
                root.path(),
                BatchOperation::Refresh { force: false },
                UpdateCancellation::default(),
            )
            .unwrap();
            let mut output = Vec::new();
            let error = Cell::new(Some(error));
            let code = plan
                .run(
                    true,
                    false,
                    UpdateCancellation::default(),
                    || {
                        Err(error
                            .take()
                            .expect("batch should stop after the first update fails")
                            .into())
                    },
                    &mut NullReporter,
                    &mut output,
                )
                .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert_eq!(code, ExitCode::FAILURE);
            assert!(error.into_inner().is_none());
            assert!(
                output.contains(&format!("abc466  NOT RUN  {reason}")),
                "{output}"
            );
            assert_eq!(read_tree_snapshot(root.path()).unwrap(), before);
        }
    }

    #[test]
    fn migration_revalidates_empty_directory_changes_after_planning_without_downloading() {
        for change in ["add", "remove", "replace"] {
            let root = workspace();
            let contest = root.path().join("abc466");
            let tests = contest.join("tests/legacy");
            let empty = tests.join("empty");
            fs::create_dir_all(&empty).unwrap();
            fs::write(tests.join("custom.in"), b"preserve me").unwrap();
            let plan = BatchPlan::scan(
                root.path(),
                BatchOperation::Migrate,
                UpdateCancellation::default(),
            )
            .unwrap();
            match change {
                "add" => fs::create_dir(tests.join("extra")).unwrap(),
                "remove" => fs::remove_dir(&empty).unwrap(),
                "replace" => {
                    fs::rename(&empty, root.path().join("original-empty")).unwrap();
                    fs::create_dir(&empty).unwrap();
                }
                _ => unreachable!(),
            }
            let edited = read_tree_snapshot(root.path()).unwrap();
            let mut output = Vec::new();
            let code = plan
                .run(
                    true,
                    false,
                    UpdateCancellation::default(),
                    || panic!("changed tests must be rejected before the client is initialized"),
                    &mut NullReporter,
                    &mut output,
                )
                .unwrap();
            let message = String::from_utf8(output).unwrap();
            assert_eq!(code, ExitCode::FAILURE, "{change}: {message}");
            assert!(
                message.contains("local contest data changed during migration"),
                "{change}: {message}"
            );
            assert_eq!(read_tree_snapshot(root.path()).unwrap(), edited);
        }
    }
}
