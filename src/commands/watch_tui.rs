use crate::app_context::AppContext;
use crate::config::{Config, RunnerConfig};
use crate::error::AppError;
use crate::model::Contest;
use crate::workspace;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::contest::{ContestTargetHealth, inspect_contest_target};
use super::watch_worker::RunWorker;
use crate::tui::detail_analysis::DetailAnalysisWorker;
use crate::tui::message::Message;
use crate::ui::Reporter;
use crate::watcher;

use super::watch_source::{build_watched_sources, resolve_watched_source};

const WATCHER_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug)]
struct WatchCleanupError {
    primary_name: Option<&'static str>,
    primary: io::Error,
    cleanup: Vec<(&'static str, io::Error)>,
}

impl fmt::Display for WatchCleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = self.primary_name {
            write!(formatter, "{name} failed: {}", self.primary)?;
        } else {
            write!(formatter, "{}", self.primary)?;
        }
        for (name, error) in &self.cleanup {
            write!(formatter, "; {name} also failed: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for WatchCleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.primary)
    }
}

fn with_cleanup_errors(
    primary_name: Option<&'static str>,
    primary: io::Error,
    cleanup: Vec<(&'static str, io::Error)>,
) -> io::Error {
    io::Error::new(
        primary.kind(),
        WatchCleanupError {
            primary_name,
            primary,
            cleanup,
        },
    )
}

fn combine_primary_and_cleanup_results<const N: usize>(
    primary: io::Result<()>,
    cleanup_results: [(&'static str, io::Result<()>); N],
) -> io::Result<()> {
    let cleanup_errors = cleanup_results
        .into_iter()
        .filter_map(|(name, result)| result.err().map(|error| (name, error)))
        .collect::<Vec<_>>();

    match primary {
        Err(primary) if cleanup_errors.is_empty() => Err(primary),
        Err(primary) => Err(with_cleanup_errors(None, primary, cleanup_errors)),
        Ok(()) => {
            let mut errors = cleanup_errors.into_iter();
            let Some((first_name, first_error)) = errors.next() else {
                return Ok(());
            };
            let Some(second) = errors.next() else {
                return Err(first_error);
            };

            let cleanup = std::iter::once(second).chain(errors).collect();
            Err(with_cleanup_errors(Some(first_name), first_error, cleanup))
        }
    }
}

struct WatcherThread {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl WatcherThread {
    fn request_stop(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    fn join(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };

        handle.join().map_err(|_| {
            io::Error::other("filesystem watcher thread panicked before reporting")
        })??;
        Ok(())
    }

    fn stop(mut self) -> io::Result<()> {
        self.request_stop();
        self.join()
    }
}

impl Drop for WatcherThread {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.join();
    }
}

fn start_watcher(
    destination: &Path,
    contest: &Contest,
    tx: mpsc::Sender<Message>,
) -> io::Result<WatcherThread> {
    let watched_sources = build_watched_sources(destination, contest)?;

    let file_watcher = watcher::FileWatcher::new(destination)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let panic_tx = tx.clone();

    let handle = thread::Builder::new()
        .name("atc-watch-fs".to_string())
        .spawn(move || {
            run_watcher_guarded(panic_tx, || {
                while !thread_shutdown.load(Ordering::Acquire) {
                    let paths = match file_watcher
                        .next_batch_timeout_with_cancel(WATCHER_POLL_INTERVAL, &|| {
                            thread_shutdown.load(Ordering::Acquire)
                        }) {
                        Ok(Some(paths)) => paths,
                        Ok(None) => continue,

                        Err(error) => {
                            let _ = tx.send(Message::WatcherFailed(error));
                            return;
                        }
                    };

                    if !send_source_changes(paths, &watched_sources, &tx) {
                        return;
                    }
                }
            })
        })?;

    Ok(WatcherThread {
        shutdown,
        handle: Some(handle),
    })
}

fn run_watcher_guarded(panic_tx: mpsc::Sender<Message>, run: impl FnOnce()) -> io::Result<()> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    if result.is_ok() {
        return Ok(());
    }

    let error = io::Error::other("filesystem watcher thread panicked");
    let _ = panic_tx.send(Message::WatcherFailed(io::Error::new(
        error.kind(),
        error.to_string(),
    )));
    Err(error)
}

fn send_source_changes(
    paths: Vec<std::path::PathBuf>,
    watched_sources: &[super::watch_source::WatchedSource],
    tx: &mpsc::Sender<Message>,
) -> bool {
    for path in paths {
        if !path.is_file() {
            continue;
        }

        let Some(source) = resolve_watched_source(watched_sources, &path) else {
            continue;
        };

        let message = Message::SourceChanged {
            problem: source.problem,
            path,
            language: source.language,
        };

        if tx.send(message).is_err() {
            return false;
        }
    }

    true
}

pub(crate) fn watch_tui(cli_contest: Option<&str>) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let app_context = AppContext::from_launch_root(&cwd)?;
    let destination = workspace::resolve_contest_target(&cwd, cli_contest)?;

    watch_tui_at(&destination, cli_contest, app_context)
}

pub(super) fn watch_tui_at(
    destination: &Path,
    expected_contest_id: Option<&str>,
    app_context: AppContext,
) -> Result<(), AppError> {
    let initial_input = PreparedWatchInput::load(destination, expected_contest_id)?;

    // workerが使うrunner設定。
    // thread開始前に読み込んでおく。
    let config = Config::load()?;
    let mut session = Some(ContestSession::start(initial_input, &config.runner)?);

    let mut terminal = match crate::tui::TerminaSession::start() {
        Ok(terminal) => terminal,

        Err(error) => {
            let session_result = session
                .take()
                .expect("initial contest session must exist")
                .shutdown();

            return combine_primary_and_cleanup_results(
                Err(error),
                [("contest session shutdown", session_result)],
            )
            .map_err(AppError::from);
        }
    };

    let mut preferences = crate::tui::FrontendPreferences::default();
    let prepared_switch = Arc::new(Mutex::new(None));
    let switch_task = contest_switch_task(&app_context, Arc::clone(&prepared_switch));
    let result = loop {
        match prepared_switch.lock() {
            Ok(mut pending) => *pending = None,
            Err(_) => {
                break Err(io::Error::other(
                    "prepared contest switch state is poisoned",
                ));
            }
        }
        let active_session = session
            .as_mut()
            .expect("an active contest session must exist while the frontend is running");
        let resolver_prepared = Arc::clone(&prepared_switch);
        let frontend_result = active_session.run_frontend(
            &mut terminal,
            &app_context,
            &config,
            &mut preferences,
            |contest_id| {
                let Some(root) = app_context.workspace_root() else {
                    return crate::tui::ContestSwitchResolution::rejected(
                        None,
                        "contest switching is unavailable outside a workspace".to_string(),
                    );
                };

                match PreparedWatchInput::resolve_for_switch(root, contest_id) {
                    Ok(SwitchTargetPreparation::Existing(prepared)) => {
                        let destination = prepared.destination.clone();
                        if let Ok(mut pending) = resolver_prepared.lock() {
                            *pending = Some(prepared);
                        }
                        crate::tui::ContestSwitchResolution::accepted(destination)
                    }
                    Ok(SwitchTargetPreparation::Missing { destination }) => {
                        if let Ok(mut pending) = resolver_prepared.lock() {
                            *pending = None;
                        }
                        crate::tui::ContestSwitchResolution::missing(destination)
                    }
                    Ok(SwitchTargetPreparation::RepairRequired { destination }) => {
                        if let Ok(mut pending) = resolver_prepared.lock() {
                            *pending = None;
                        }
                        crate::tui::ContestSwitchResolution::repair_required(destination)
                    }
                    Err(SwitchPreparationError { destination, error }) => {
                        if let Ok(mut pending) = resolver_prepared.lock() {
                            *pending = None;
                        }
                        crate::tui::ContestSwitchResolution::rejected(
                            destination,
                            error.to_string(),
                        )
                    }
                }
            },
            Arc::clone(&switch_task),
        );

        match frontend_result {
            Err(error) => break Err(error),
            Ok(crate::tui::SessionExit::Quit) => break Ok(()),
            Ok(crate::tui::SessionExit::SwitchContest) => {
                let prepared = match prepared_switch.lock() {
                    Ok(mut pending) => match pending.take() {
                        Some(prepared) => prepared,
                        None => {
                            break Err(io::Error::other(
                                "a switch exit did not retain its validated prepared contest",
                            ));
                        }
                    },
                    Err(_) => {
                        break Err(io::Error::other(
                            "prepared contest switch state is poisoned",
                        ));
                    }
                };
                let old_session = session
                    .take()
                    .expect("the old contest session must still exist before switching");
                if let Err(error) = old_session.shutdown() {
                    break Err(error);
                }

                match ContestSession::start(prepared, &config.runner) {
                    Ok(new_session) => session = Some(new_session),
                    Err(error) => break Err(error),
                }
            }
        }
    };

    // sample/stress実行中だった場合、runnerまでcancelを先に伝える。
    if let Some(session) = session.as_ref() {
        session.request_stop();
    }

    // joinが予想外に長引いてもterminalは先に復元する。
    let mouse_mode_label = terminal.mouse_mode_label();
    let mouse_trace_line = terminal.mouse_trace_line();
    let restore_result = terminal.restore();
    // TerminaのDropで元のplatform mode/code pageまで戻してからjoinする。
    drop(terminal);
    if std::env::var_os("ATC_TUI_MOUSE_TRACE").is_some() {
        eprintln!("atc terminal mouse: {mouse_mode_label}");
        if let Some(trace) = mouse_trace_line {
            eprintln!("{trace}");
        }
    }

    let session_result = match session.take() {
        Some(session) => session.shutdown(),
        None => Ok(()),
    };

    combine_primary_and_cleanup_results(
        result,
        [
            ("terminal restoration", restore_result),
            ("contest session shutdown", session_result),
        ],
    )
    .map_err(AppError::from)
}

#[derive(Debug)]
struct PreparedWatchInput {
    destination: PathBuf,
    contest: Contest,
    sample_counts: Vec<usize>,
    stress_cases: Vec<Option<crate::model::Sample>>,
}

enum SwitchTargetPreparation {
    Existing(PreparedWatchInput),
    Missing { destination: PathBuf },
    RepairRequired { destination: PathBuf },
}

#[derive(Debug)]
struct SwitchPreparationError {
    destination: Option<PathBuf>,
    error: io::Error,
}

impl PreparedWatchInput {
    fn load(destination: &Path, expected_contest_id: Option<&str>) -> Result<Self, AppError> {
        let (contest, sample_counts, stress_cases) =
            load_watch_input(destination, expected_contest_id)?;
        Ok(Self {
            destination: destination.to_path_buf(),
            contest,
            sample_counts,
            stress_cases,
        })
    }

    fn resolve_for_switch(
        root: &Path,
        contest_id: &str,
    ) -> Result<SwitchTargetPreparation, SwitchPreparationError> {
        Self::resolve_for_switch_expected(root, contest_id, None)
    }

    fn resolve_for_switch_expected(
        root: &Path,
        contest_id: &str,
        expected_destination: Option<&Path>,
    ) -> Result<SwitchTargetPreparation, SwitchPreparationError> {
        Self::resolve_for_switch_expected_with_hook(root, contest_id, expected_destination, || {})
    }

    fn resolve_for_switch_expected_with_hook(
        root: &Path,
        contest_id: &str,
        expected_destination: Option<&Path>,
        after_healthy_inspection: impl FnOnce(),
    ) -> Result<SwitchTargetPreparation, SwitchPreparationError> {
        let destination = workspace::resolve_contest_path(root, contest_id).map_err(|error| {
            SwitchPreparationError {
                destination: None,
                error,
            }
        })?;

        if let Some(expected_destination) = expected_destination
            && destination != expected_destination
        {
            return Err(SwitchPreparationError {
                destination: Some(destination.clone()),
                error: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "workspace config changed while preparing contest {contest_id:?}: expected {}, now resolves to {}",
                        expected_destination.display(),
                        destination.display()
                    ),
                ),
            });
        }

        let target = inspect_contest_target(&destination, contest_id).map_err(|error| {
            SwitchPreparationError {
                destination: Some(destination.clone()),
                error,
            }
        })?;
        match target {
            ContestTargetHealth::Healthy => after_healthy_inspection(),
            ContestTargetHealth::MissingDirectory => {
                return Ok(SwitchTargetPreparation::Missing { destination });
            }
            ContestTargetHealth::RepairRequired => {
                return Ok(SwitchTargetPreparation::RepairRequired { destination });
            }
            ContestTargetHealth::UnsupportedVersion(version) => {
                return Err(SwitchPreparationError {
                    destination: Some(destination),
                    error: io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsupported contest metadata version: {version}"),
                    ),
                });
            }
        }

        let (contest, sample_counts, stress_cases) =
            load_watch_input(&destination, Some(contest_id)).map_err(|error| {
                SwitchPreparationError {
                    destination: Some(destination.clone()),
                    error,
                }
            })?;

        Ok(SwitchTargetPreparation::Existing(Self {
            destination,
            contest,
            sample_counts,
            stress_cases,
        }))
    }
}

fn contest_switch_task(
    app_context: &AppContext,
    prepared_switch: Arc<Mutex<Option<PreparedWatchInput>>>,
) -> crate::tui::ContestSwitchTask {
    let workspace_root = app_context.workspace_root().map(Path::to_path_buf);
    Arc::new(move |request, reporter| {
        let root = workspace_root.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "contest switching is unavailable outside a workspace",
            )
        })?;
        let prepared = match request.mutation {
            crate::tui::ContestSwitchMutation::Create => {
                create_and_prepare_contest_switch(root, &request, reporter)?
            }
            crate::tui::ContestSwitchMutation::Repair => {
                repair_and_prepare_contest_switch(root, &request, reporter)?
            }
        };
        *prepared_switch
            .lock()
            .map_err(|_| io::Error::other("prepared contest switch state is poisoned"))? =
            Some(prepared);
        Ok(())
    })
}

fn create_and_prepare_contest_switch(
    root: &Path,
    request: &crate::tui::ContestSwitchRequest,
    reporter: &mut dyn Reporter,
) -> Result<PreparedWatchInput, AppError> {
    create_and_prepare_contest_switch_with(
        root,
        request,
        reporter,
        |destination, contest_id, reporter| {
            super::contest::create_contest(root, destination, contest_id, reporter)
        },
    )
}

fn create_and_prepare_contest_switch_with(
    root: &Path,
    request: &crate::tui::ContestSwitchRequest,
    reporter: &mut dyn Reporter,
    create: impl FnOnce(&Path, &str, &mut dyn Reporter) -> Result<(), AppError>,
) -> Result<PreparedWatchInput, AppError> {
    if request.mutation != crate::tui::ContestSwitchMutation::Create {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "contest switch request is not a create operation",
        )
        .into());
    }

    let target = PreparedWatchInput::resolve_for_switch_expected(
        root,
        &request.contest_id,
        Some(&request.destination),
    )
    .map_err(|error| error.error)?;

    match target {
        SwitchTargetPreparation::Existing(_) => {}
        SwitchTargetPreparation::Missing { ref destination } => {
            create(destination, &request.contest_id, reporter)?;
        }
        SwitchTargetPreparation::RepairRequired { destination } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "contest creation was requested but metadata requires repair: {}",
                    destination.display()
                ),
            )
            .into());
        }
    }

    match PreparedWatchInput::resolve_for_switch_expected(
        root,
        &request.contest_id,
        Some(&request.destination),
    )
    .map_err(|error| error.error)?
    {
        SwitchTargetPreparation::Existing(prepared) => Ok(prepared),
        SwitchTargetPreparation::Missing { destination } => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "contest creation completed but the destination is still missing: {}",
                destination.display()
            ),
        )
        .into()),
        SwitchTargetPreparation::RepairRequired { destination } => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "contest creation completed but contest data requires repair: {}",
                destination.display()
            ),
        )
        .into()),
    }
}

fn repair_and_prepare_contest_switch(
    root: &Path,
    request: &crate::tui::ContestSwitchRequest,
    reporter: &mut dyn Reporter,
) -> Result<PreparedWatchInput, AppError> {
    repair_and_prepare_contest_switch_with(root, request, reporter, super::contest::repair_contest)
}

fn repair_and_prepare_contest_switch_with(
    root: &Path,
    request: &crate::tui::ContestSwitchRequest,
    reporter: &mut dyn Reporter,
    repair: impl FnOnce(&Path, &str, &mut dyn Reporter) -> Result<(), AppError>,
) -> Result<PreparedWatchInput, AppError> {
    repair_and_prepare_contest_switch_with_final_hook(root, request, reporter, repair, || {})
}

fn repair_and_prepare_contest_switch_with_final_hook(
    root: &Path,
    request: &crate::tui::ContestSwitchRequest,
    reporter: &mut dyn Reporter,
    repair: impl FnOnce(&Path, &str, &mut dyn Reporter) -> Result<(), AppError>,
    after_final_healthy_inspection: impl FnOnce(),
) -> Result<PreparedWatchInput, AppError> {
    if request.mutation != crate::tui::ContestSwitchMutation::Repair {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "contest switch request is not a repair operation",
        )
        .into());
    }

    match PreparedWatchInput::resolve_for_switch_expected(
        root,
        &request.contest_id,
        Some(&request.destination),
    )
    .map_err(|error| error.error)?
    {
        SwitchTargetPreparation::RepairRequired { ref destination } => {
            repair(destination, &request.contest_id, reporter)?;
        }
        SwitchTargetPreparation::Existing(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "contest no longer requires repair",
            )
            .into());
        }
        SwitchTargetPreparation::Missing { destination } => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "contest repair was requested but the destination is missing: {}",
                    destination.display()
                ),
            )
            .into());
        }
    }

    match PreparedWatchInput::resolve_for_switch_expected_with_hook(
        root,
        &request.contest_id,
        Some(&request.destination),
        after_final_healthy_inspection,
    )
    .map_err(|error| error.error)?
    {
        SwitchTargetPreparation::Existing(prepared) => Ok(prepared),
        SwitchTargetPreparation::Missing { destination } => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "contest repair completed but the destination is missing: {}",
                destination.display()
            ),
        )
        .into()),
        SwitchTargetPreparation::RepairRequired { destination } => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "contest repair completed but contest data still requires repair: {}",
                destination.display()
            ),
        )
        .into()),
    }
}

struct ContestSession {
    // After Drop::drop requests stop, field destruction joins these worker owners before the
    // session communication endpoints below are discarded.
    run_worker: Option<RunWorker>,
    watcher_thread: Option<WatcherThread>,
    detail_analysis_worker: Option<DetailAnalysisWorker>,
    input: PreparedWatchInput,
    message_rx: mpsc::Receiver<Message>,
    run_tx: mpsc::Sender<crate::tui::message::RunWorkerCommand>,
    detail_analysis_tx: mpsc::Sender<crate::tui::SessionDetailAnalysisCommand>,
    detail_analysis_rx: mpsc::Receiver<crate::tui::SessionDetailAnalysisResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStartStage {
    WatcherStarted,
    RunWorkerStarted,
}

impl ContestSession {
    fn start(input: PreparedWatchInput, runner_config: &RunnerConfig) -> io::Result<Self> {
        Self::start_with_hook(input, runner_config, |_| Ok(()))
    }

    fn start_with_hook(
        input: PreparedWatchInput,
        runner_config: &RunnerConfig,
        mut after_stage: impl FnMut(SessionStartStage) -> io::Result<()>,
    ) -> io::Result<Self> {
        let (message_tx, message_rx) = mpsc::channel();
        let watcher_thread = start_watcher(&input.destination, &input.contest, message_tx.clone())?;
        if let Err(error) = after_stage(SessionStartStage::WatcherStarted) {
            drop(message_rx);
            drop(message_tx);
            return Err(combine_primary_and_cleanup_results(
                Err(error),
                [("filesystem watcher shutdown", watcher_thread.stop())],
            )
            .expect_err("injected startup failure must remain an error"));
        }
        let run_worker = match RunWorker::start(
            input.destination.clone(),
            input.contest.contest_id.clone(),
            input.contest.problems.clone(),
            runner_config.clone(),
            message_tx.clone(),
        ) {
            Ok(worker) => worker,
            Err(error) => {
                drop(message_rx);
                drop(message_tx);
                return Err(combine_primary_and_cleanup_results(
                    Err(error),
                    [("filesystem watcher shutdown", watcher_thread.stop())],
                )
                .expect_err("worker startup failure must remain an error"));
            }
        };
        let run_tx = run_worker.sender();
        if let Err(error) = after_stage(SessionStartStage::RunWorkerStarted) {
            drop(message_rx);
            drop(message_tx);
            run_worker.request_stop();
            watcher_thread.request_stop();
            return Err(combine_primary_and_cleanup_results(
                Err(error),
                [
                    ("run worker shutdown", run_worker.stop_and_join()),
                    ("filesystem watcher shutdown", watcher_thread.stop()),
                ],
            )
            .expect_err("injected startup failure must remain an error"));
        }

        let mut detail_analysis_worker = match DetailAnalysisWorker::start() {
            Ok(worker) => worker,
            Err(error) => {
                drop(message_rx);
                drop(message_tx);
                run_worker.request_stop();
                watcher_thread.request_stop();
                return Err(combine_primary_and_cleanup_results(
                    Err(error),
                    [
                        ("run worker shutdown", run_worker.stop_and_join()),
                        ("filesystem watcher shutdown", watcher_thread.stop()),
                    ],
                )
                .expect_err("detail worker startup failure must remain an error"));
            }
        };
        let detail_analysis_tx = detail_analysis_worker.request_sender();
        let detail_analysis_rx = detail_analysis_worker.take_result_receiver();
        drop(message_tx);

        Ok(Self {
            run_worker: Some(run_worker),
            watcher_thread: Some(watcher_thread),
            detail_analysis_worker: Some(detail_analysis_worker),
            input,
            message_rx,
            run_tx,
            detail_analysis_tx,
            detail_analysis_rx,
        })
    }

    fn channels(&self) -> crate::tui::SessionChannels<'_> {
        crate::tui::SessionChannels::new(
            &self.message_rx,
            &self.run_tx,
            &self.detail_analysis_tx,
            &self.detail_analysis_rx,
        )
    }

    fn run_frontend(
        &mut self,
        terminal: &mut crate::tui::TerminaSession,
        app_context: &AppContext,
        config: &Config,
        preferences: &mut crate::tui::FrontendPreferences,
        resolve_contest_switch: impl FnMut(&str) -> crate::tui::ContestSwitchResolution,
        contest_switch_task: crate::tui::ContestSwitchTask,
    ) -> io::Result<crate::tui::SessionExit> {
        let sample_counts = std::mem::take(&mut self.input.sample_counts);
        let stress_cases = std::mem::take(&mut self.input.stress_cases);
        let channels = self.channels();
        let runtime = crate::tui::SessionRuntime::new(
            &self.input.destination,
            config,
            &self.input.contest,
            sample_counts,
            stress_cases,
            channels,
        );
        crate::tui::run(
            terminal,
            app_context,
            preferences,
            runtime,
            resolve_contest_switch,
            contest_switch_task,
        )
    }

    fn request_stop(&self) {
        if let Some(worker) = self.run_worker.as_ref() {
            worker.request_stop();
        }
        if let Some(watcher) = self.watcher_thread.as_ref() {
            watcher.request_stop();
        }
        if let Some(worker) = self.detail_analysis_worker.as_ref() {
            worker.request_stop();
        }
    }

    fn shutdown(mut self) -> io::Result<()> {
        self.request_stop();
        // `self` retains every channel endpoint until all three joins below have completed.
        combine_primary_and_cleanup_results(
            Ok(()),
            [
                (
                    "run worker shutdown",
                    self.run_worker
                        .take()
                        .map_or(Ok(()), RunWorker::stop_and_join),
                ),
                (
                    "filesystem watcher shutdown",
                    self.watcher_thread
                        .take()
                        .map_or(Ok(()), WatcherThread::stop),
                ),
                (
                    "detail analysis worker shutdown",
                    self.detail_analysis_worker
                        .take()
                        .map_or(Ok(()), DetailAnalysisWorker::stop_and_join),
                ),
            ],
        )
    }
}

impl Drop for ContestSession {
    fn drop(&mut self) {
        self.request_stop();
    }
}

type LoadedWatchInput = (Contest, Vec<usize>, Vec<Option<crate::model::Sample>>);

fn load_watch_input(
    destination: &Path,
    expected_contest_id: Option<&str>,
) -> io::Result<LoadedWatchInput> {
    workspace::validate_workspace_marker(destination)?;

    let contest = workspace::load_metadata(destination)?;
    workspace::validate_contest_paths(&contest)?;
    if let Some(contest_id) = expected_contest_id {
        workspace::validate_contest_identity(&contest, contest_id)?;
    }

    let (sample_counts, stress_cases) = load_watch_data(destination, &contest)?;

    Ok((contest, sample_counts, stress_cases))
}

fn load_watch_data(
    destination: &Path,
    contest: &Contest,
) -> io::Result<(Vec<usize>, Vec<Option<crate::model::Sample>>)> {
    let sample_counts = contest
        .problems
        .iter()
        .map(|problem| {
            let sample_count = workspace::load_samples(destination, &problem.index)?.len();
            if sample_count != problem.sample_count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "problem {} has {sample_count} local samples, but contest metadata requires {}",
                        problem.index, problem.sample_count
                    ),
                ));
            }
            Ok(sample_count)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let stress_cases = contest
        .problems
        .iter()
        .map(|problem| {
            crate::stress::load_saved_case(destination, &contest.contest_id, &problem.index)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok((sample_counts, stress_cases))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;
    use crate::model::{Problem, Sample};
    use crate::tui::message::{RunKind, RunRequest, RunWorkerCommand, TestEvent};

    fn switch_error(root: &Path, contest_id: &str) -> SwitchPreparationError {
        match PreparedWatchInput::resolve_for_switch(root, contest_id) {
            Ok(_) => panic!("contest switch unexpectedly succeeded"),
            Err(error) => error,
        }
    }

    fn save_healthy_contest(destination: &Path, contest_id: &str) {
        workspace::save_metadata(
            destination,
            &Contest {
                contest_id: contest_id.to_string(),
                problems: vec![problem_for(contest_id, "A")],
            },
        )
        .unwrap();
    }

    fn problem(index: &str) -> Problem {
        problem_for("contest", index)
    }

    fn problem_for(contest_id: &str, index: &str) -> Problem {
        let task_id = format!("{contest_id}_{}", index.to_ascii_lowercase());
        Problem {
            index: index.to_string(),
            title: format!("Problem {index}"),
            url: format!("https://atcoder.jp/contests/{contest_id}/tasks/{task_id}"),
            task_id,
            sample_count: 0,
        }
    }

    fn write_empty_workspace(root: &Path) {
        std::fs::write(
            root.join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
    }

    fn repair_request(contest_id: &str, destination: PathBuf) -> crate::tui::ContestSwitchRequest {
        crate::tui::ContestSwitchRequest {
            mutation: crate::tui::ContestSwitchMutation::Repair,
            contest_id: contest_id.to_string(),
            destination,
        }
    }

    #[test]
    fn loads_sample_counts_in_metadata_problem_order() {
        let temp = tempfile::tempdir().unwrap();
        let contest = Contest {
            contest_id: "contest".to_string(),
            problems: [("A", 2), ("B", 0), ("C", 1)]
                .into_iter()
                .map(|(index, sample_count)| Problem {
                    sample_count,
                    ..problem(index)
                })
                .collect(),
        };
        workspace::save_metadata(temp.path(), &contest).unwrap();
        workspace::save_samples(
            temp.path(),
            &contest.problems[0],
            &[
                Sample {
                    input: "1\n".to_string(),
                    output: "2\n".to_string(),
                },
                Sample {
                    input: "3\n".to_string(),
                    output: "4\n".to_string(),
                },
            ],
        )
        .unwrap();
        workspace::save_samples(
            temp.path(),
            &contest.problems[2],
            &[Sample {
                input: "5\n".to_string(),
                output: "6\n".to_string(),
            }],
        )
        .unwrap();
        let stress_a = temp.path().join(".atc").join("stress").join("A");
        std::fs::create_dir_all(&stress_a).unwrap();
        std::fs::write(stress_a.join("failed.in"), "7\n").unwrap();
        std::fs::write(stress_a.join("actual.out"), "9\n").unwrap();
        std::fs::write(stress_a.join("expected.out"), "8\n").unwrap();
        std::fs::write(
            stress_a.join("meta.toml"),
            "version = 1\ncontest = \"contest\"\nproblem = \"A\"\nkind = \"wrong-answer\"\ncase = 1\nbase_seed = 10\nseed = 10\n",
        )
        .unwrap();

        let (loaded_contest, sample_counts, stress_cases) =
            load_watch_input(temp.path(), None).unwrap();

        assert_eq!(loaded_contest.contest_id, "contest");
        assert_eq!(
            loaded_contest
                .problems
                .iter()
                .map(|problem| problem.index.as_str())
                .collect::<Vec<_>>(),
            ["A", "B", "C"]
        );
        assert_eq!(sample_counts, [2, 0, 1]);
        assert_eq!(
            stress_cases,
            [
                Some(Sample {
                    input: "7\n".to_string(),
                    output: "8\n".to_string(),
                }),
                None,
                None,
            ]
        );
    }

    #[test]
    fn requested_contest_id_must_match_loaded_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let contest = Contest {
            contest_id: "arc001".to_string(),
            problems: vec![problem_for("arc001", "A")],
        };
        workspace::save_metadata(temp.path(), &contest).unwrap();

        let error = load_watch_input(temp.path(), Some("abc466"))
            .expect_err("resolved destination metadata must match the requested contest");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn healthy_mapped_existing_contest_is_prepared_for_switch() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = [{ pattern = \"^abc[0-9]+$\", path = \"AtCoder/ABC\" }]\n",
        )
        .unwrap();
        let destination = root.path().join("AtCoder/ABC/abc467");
        save_healthy_contest(&destination, "abc467");

        let SwitchTargetPreparation::Existing(prepared) =
            PreparedWatchInput::resolve_for_switch(root.path(), "abc467").unwrap()
        else {
            panic!("healthy contest must resolve as existing");
        };

        assert_eq!(prepared.destination, destination);
        assert_eq!(prepared.contest.contest_id, "abc467");
        assert_eq!(prepared.sample_counts, [0]);
    }

    #[test]
    fn missing_contests_preview_mapped_and_fallback_destinations_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = [{ pattern = \"^abc[0-9]+$\", path = \"AtCoder/ABC\" }]\n",
        )
        .unwrap();

        let mapped = PreparedWatchInput::resolve_for_switch(root.path(), "abc470").unwrap();
        let fallback = PreparedWatchInput::resolve_for_switch(root.path(), "typical90").unwrap();

        assert!(matches!(
            mapped,
            SwitchTargetPreparation::Missing { destination }
                if destination == root.path().join("AtCoder/ABC/abc470")
        ));
        assert!(matches!(
            fallback,
            SwitchTargetPreparation::Missing { destination }
                if destination == root.path().join("typical90")
        ));
        assert!(!root.path().join("AtCoder").exists());
        assert!(!root.path().join("typical90").exists());
    }

    #[test]
    fn create_switch_revalidates_mapping_before_calling_creation_core() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".atc-workspace.toml");
        std::fs::write(
            &config,
            "version = 1\npaths = [{ pattern = \"^abc\", path = \"one\" }]\n",
        )
        .unwrap();
        let request = crate::tui::ContestSwitchRequest {
            mutation: crate::tui::ContestSwitchMutation::Create,
            contest_id: "abc470".to_string(),
            destination: root.path().join("one/abc470"),
        };
        std::fs::write(
            &config,
            "version = 1\npaths = [{ pattern = \"^abc\", path = \"two\" }]\n",
        )
        .unwrap();
        let mut reporter = crate::ui::NullReporter;

        let error = create_and_prepare_contest_switch_with(
            root.path(),
            &request,
            &mut reporter,
            |_, _, _| panic!("stale preview must not start creation"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("workspace config changed"));
        assert!(!root.path().join("one").exists());
        assert!(!root.path().join("two").exists());
    }

    #[test]
    fn create_switch_loads_complete_session_input_before_success() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
        let destination = root.path().join("abc470");
        let request = crate::tui::ContestSwitchRequest {
            mutation: crate::tui::ContestSwitchMutation::Create,
            contest_id: "abc470".to_string(),
            destination: destination.clone(),
        };
        let mut reporter = crate::ui::NullReporter;

        let prepared = create_and_prepare_contest_switch_with(
            root.path(),
            &request,
            &mut reporter,
            |destination, contest_id, _| {
                let mut problem = problem_for(contest_id, "A");
                problem.sample_count = 1;
                workspace::save_metadata(
                    destination,
                    &Contest {
                        contest_id: contest_id.to_string(),
                        problems: vec![problem.clone()],
                    },
                )?;
                workspace::save_samples(
                    destination,
                    &problem,
                    &[Sample {
                        input: "1\n".to_string(),
                        output: "2\n".to_string(),
                    }],
                )?;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(prepared.destination, destination);
        assert_eq!(prepared.contest.contest_id, "abc470");
        assert_eq!(prepared.sample_counts, [1]);
        assert_eq!(prepared.stress_cases, [None]);
    }

    #[test]
    fn create_switch_sample_fetch_failure_installs_nothing() {
        #[derive(Default)]
        struct FetchReporter {
            failed_problems: Vec<String>,
            workspace_created: bool,
        }

        impl Reporter for FetchReporter {
            fn report(&mut self, event: crate::ui::Event<'_>) {
                match event {
                    crate::ui::Event::ProblemFetchFailed { index, .. } => {
                        self.failed_problems.push(index.to_string());
                    }
                    crate::ui::Event::WorkspaceCreated { .. } => self.workspace_created = true,
                    _ => {}
                }
            }
        }

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
        let request = crate::tui::ContestSwitchRequest {
            mutation: crate::tui::ContestSwitchMutation::Create,
            contest_id: "mini".to_string(),
            destination: root.path().join("mini"),
        };
        let fixtures = root.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("contests")).unwrap();
        std::fs::create_dir_all(fixtures.join("problems")).unwrap();
        std::fs::write(
            fixtures.join("contests/mini.html"),
            r#"<table><tbody>
                <tr><td><a href="/contests/mini/tasks/mini_a">A</a></td><td><a href="/contests/mini/tasks/mini_a">A</a></td></tr>
                <tr><td><a href="/contests/mini/tasks/mini_b">B</a></td><td><a href="/contests/mini/tasks/mini_b">B</a></td></tr>
            </tbody></table>"#,
        )
        .unwrap();
        std::fs::write(
            fixtures.join("problems/mini_a.html"),
            r#"<div id="task-statement"><span class="lang-en">
                <div class="part"><section><h3>Sample Input 1</h3><pre>1
</pre></section></div>
                <div class="part"><section><h3>Sample Output 1</h3><pre>2
</pre></section></div>
            </span></div>"#,
        )
        .unwrap();
        let client = crate::atcoder::AtCoderClient::fixture(&fixtures);
        let mut reporter = FetchReporter::default();

        let error = create_and_prepare_contest_switch_with(
            root.path(),
            &request,
            &mut reporter,
            |destination, contest_id, reporter| {
                super::super::new::new_at_in_workspace(
                    root.path(),
                    destination,
                    contest_id,
                    Language::Cpp,
                    crate::template::builtin_template(Language::Cpp),
                    &client,
                    reporter,
                )
            },
        )
        .expect_err("strict create must fail when any problem page is unavailable");

        assert!(matches!(error, AppError::AtCoder(_)));
        assert!(!request.destination.exists());
        assert!(!reporter.workspace_created);
        assert_eq!(reporter.failed_problems, ["B"]);
    }

    #[test]
    fn create_or_post_create_failure_never_touches_the_active_contest() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
        let active = root.path().join("abc123");
        save_healthy_contest(&active, "abc123");
        let active_before = std::fs::read(active.join(".atc/contest.toml")).unwrap();
        let request = crate::tui::ContestSwitchRequest {
            mutation: crate::tui::ContestSwitchMutation::Create,
            contest_id: "abc470".to_string(),
            destination: root.path().join("abc470"),
        };
        let mut reporter = crate::ui::NullReporter;

        let error = create_and_prepare_contest_switch_with(
            root.path(),
            &request,
            &mut reporter,
            |_, _, _| Err(io::Error::other("network unavailable").into()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("network unavailable"));
        assert_eq!(
            std::fs::read(active.join(".atc/contest.toml")).unwrap(),
            active_before
        );
    }

    #[test]
    fn post_create_validation_failure_preserves_old_and_partial_destinations() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
        let active = root.path().join("abc123");
        save_healthy_contest(&active, "abc123");
        let request = crate::tui::ContestSwitchRequest {
            mutation: crate::tui::ContestSwitchMutation::Create,
            contest_id: "abc470".to_string(),
            destination: root.path().join("abc470"),
        };
        let mut reporter = crate::ui::NullReporter;

        let error = create_and_prepare_contest_switch_with(
            root.path(),
            &request,
            &mut reporter,
            |destination, _, _| {
                std::fs::create_dir_all(destination.join(".atc"))?;
                std::fs::write(destination.join(".atc/contest.toml"), "invalid")?;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("contest data requires repair"));
        assert_eq!(
            workspace::load_metadata(&active).unwrap().contest_id,
            "abc123"
        );
        assert!(request.destination.join(".atc/contest.toml").exists());
    }

    #[test]
    fn invalid_targets_are_rejected_and_missing_targets_are_previewed_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
        let active = root.path().join("abc123");
        save_healthy_contest(&active, "abc123");
        let before = std::fs::read(active.join(".atc/contest.toml")).unwrap();

        let invalid = switch_error(root.path(), "../abc467");
        assert!(invalid.destination.is_none());
        assert_eq!(invalid.error.kind(), io::ErrorKind::InvalidInput);

        let missing = PreparedWatchInput::resolve_for_switch(root.path(), "abc467").unwrap();
        assert!(matches!(
            missing,
            SwitchTargetPreparation::Missing { destination }
                if destination == root.path().join("abc467")
        ));
        assert!(!root.path().join("abc467").exists());
        assert_eq!(
            std::fs::read(active.join(".atc/contest.toml")).unwrap(),
            before
        );
    }

    #[test]
    fn invalid_metadata_is_repair_required_without_mutation_or_touching_active_contest() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = []\n",
        )
        .unwrap();
        let active = root.path().join("abc123");
        save_healthy_contest(&active, "abc123");
        let missing_metadata = root.path().join("abc466");
        std::fs::create_dir(&missing_metadata).unwrap();
        let invalid = root.path().join("abc467");
        std::fs::create_dir_all(invalid.join(".atc")).unwrap();
        std::fs::write(invalid.join(".atc/contest.toml"), "invalid metadata").unwrap();
        let invalid_before = std::fs::read(invalid.join(".atc/contest.toml")).unwrap();

        assert!(matches!(
            PreparedWatchInput::resolve_for_switch(root.path(), "abc466").unwrap(),
            SwitchTargetPreparation::RepairRequired { destination }
                if destination == missing_metadata
        ));

        let target = PreparedWatchInput::resolve_for_switch(root.path(), "abc467").unwrap();

        assert!(matches!(
            target,
            SwitchTargetPreparation::RepairRequired { destination }
                if destination == invalid
        ));
        assert_eq!(
            std::fs::read(invalid.join(".atc/contest.toml")).unwrap(),
            invalid_before
        );
        assert_eq!(
            workspace::load_metadata(&active).unwrap().contest_id,
            "abc123"
        );
    }

    #[test]
    fn repair_switch_revalidates_mapping_before_calling_repair_core() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".atc-workspace.toml");
        std::fs::write(
            &config,
            "version = 1\npaths = [{ pattern = \"^abc\", path = \"one\" }]\n",
        )
        .unwrap();
        let destination = root.path().join("one/abc470");
        std::fs::create_dir_all(&destination).unwrap();
        let request = repair_request("abc470", destination.clone());
        std::fs::write(
            &config,
            "version = 1\npaths = [{ pattern = \"^abc\", path = \"two\" }]\n",
        )
        .unwrap();
        let mut reporter = crate::ui::NullReporter;

        let error = repair_and_prepare_contest_switch_with(
            root.path(),
            &request,
            &mut reporter,
            |_, _, _| panic!("stale preview must not start repair"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("workspace config changed"));
        assert!(destination.is_dir());
        assert!(!root.path().join("two").exists());
    }

    #[test]
    fn repair_switch_loads_metadata_samples_and_saved_stress_before_success() {
        let root = tempfile::tempdir().unwrap();
        write_empty_workspace(root.path());
        let destination = root.path().join("abc470");
        std::fs::create_dir(&destination).unwrap();
        let request = repair_request("abc470", destination.clone());
        let mut reporter = crate::ui::NullReporter;

        let prepared = repair_and_prepare_contest_switch_with(
            root.path(),
            &request,
            &mut reporter,
            |destination, contest_id, _| {
                let mut problem = problem_for(contest_id, "A");
                problem.sample_count = 1;
                workspace::save_metadata(
                    destination,
                    &Contest {
                        contest_id: contest_id.to_string(),
                        problems: vec![problem.clone()],
                    },
                )?;
                workspace::save_samples(
                    destination,
                    &problem,
                    &[Sample {
                        input: "1\n".to_string(),
                        output: "2\n".to_string(),
                    }],
                )?;
                let stress = destination.join(".atc/stress/A");
                std::fs::create_dir_all(&stress)?;
                std::fs::write(stress.join("failed.in"), "3\n")?;
                std::fs::write(stress.join("actual.out"), "5\n")?;
                std::fs::write(stress.join("expected.out"), "4\n")?;
                std::fs::write(
                    stress.join("meta.toml"),
                    "version = 1\ncontest = \"abc470\"\nproblem = \"A\"\nkind = \"wrong-answer\"\ncase = 1\nbase_seed = 9\nseed = 9\n",
                )?;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(prepared.destination, destination);
        assert_eq!(prepared.contest.contest_id, "abc470");
        assert_eq!(prepared.sample_counts, [1]);
        assert_eq!(
            prepared.stress_cases,
            [Some(Sample {
                input: "3\n".to_string(),
                output: "4\n".to_string(),
            })]
        );
    }

    #[test]
    fn repair_completion_requires_fresh_healthy_metadata_and_loadable_session_data() {
        let root = tempfile::tempdir().unwrap();
        write_empty_workspace(root.path());
        let mut reporter = crate::ui::NullReporter;

        let still_invalid = root.path().join("abc470");
        std::fs::create_dir(&still_invalid).unwrap();
        let error = repair_and_prepare_contest_switch_with(
            root.path(),
            &repair_request("abc470", still_invalid.clone()),
            &mut reporter,
            |_, _, _| Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("still requires repair"));
        assert!(still_invalid.is_dir());

        let metadata_race = root.path().join("abc473");
        std::fs::create_dir(&metadata_race).unwrap();
        let metadata_path = metadata_race.join(".atc/contest.toml");
        let error = repair_and_prepare_contest_switch_with_final_hook(
            root.path(),
            &repair_request("abc473", metadata_race.clone()),
            &mut reporter,
            |destination, contest_id, _| {
                workspace::save_metadata(
                    destination,
                    &Contest {
                        contest_id: contest_id.to_string(),
                        problems: vec![problem_for(contest_id, "A")],
                    },
                )?;
                Ok(())
            },
            || std::fs::write(&metadata_path, "invalid after inspection").unwrap(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == io::ErrorKind::InvalidData
        ));
        assert_eq!(
            std::fs::read_to_string(metadata_path).unwrap(),
            "invalid after inspection"
        );

        let invalid_samples = root.path().join("abc471");
        std::fs::create_dir(&invalid_samples).unwrap();
        let error = repair_and_prepare_contest_switch_with(
            root.path(),
            &repair_request("abc471", invalid_samples.clone()),
            &mut reporter,
            |destination, contest_id, _| {
                workspace::save_metadata(
                    destination,
                    &Contest {
                        contest_id: contest_id.to_string(),
                        problems: vec![problem_for(contest_id, "A")],
                    },
                )?;
                std::fs::create_dir_all(destination.join("tests/A"))?;
                std::fs::write(destination.join("tests/A/sample-1.in"), "1\n")?;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("still requires repair"));
        assert!(invalid_samples.join("tests/A/sample-1.in").exists());

        let invalid_stress = root.path().join("abc472");
        std::fs::create_dir(&invalid_stress).unwrap();
        let error = repair_and_prepare_contest_switch_with(
            root.path(),
            &repair_request("abc472", invalid_stress.clone()),
            &mut reporter,
            |destination, contest_id, _| {
                workspace::save_metadata(
                    destination,
                    &Contest {
                        contest_id: contest_id.to_string(),
                        problems: vec![problem_for(contest_id, "A")],
                    },
                )?;
                std::fs::write(destination.join(".atc/stress"), "not a directory")?;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        assert!(invalid_stress.join(".atc/stress").is_file());
    }

    #[test]
    fn repair_failure_preserves_the_active_session_destination_and_partial_target() {
        let root = tempfile::tempdir().unwrap();
        write_empty_workspace(root.path());
        let active = root.path().join("abc123");
        save_healthy_contest(&active, "abc123");
        let active_before = std::fs::read(active.join(".atc/contest.toml")).unwrap();
        let destination = root.path().join("abc470");
        std::fs::create_dir(&destination).unwrap();
        let partial = destination.join("repair-partial.txt");
        let mut reporter = crate::ui::NullReporter;

        let error = repair_and_prepare_contest_switch_with(
            root.path(),
            &repair_request("abc470", destination.clone()),
            &mut reporter,
            |destination, _, _| {
                std::fs::write(destination.join("repair-partial.txt"), "kept")?;
                Err(io::Error::other("repair failed after mutation").into())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("repair failed after mutation"));
        assert_eq!(std::fs::read_to_string(partial).unwrap(), "kept");
        assert_eq!(
            std::fs::read(active.join(".atc/contest.toml")).unwrap(),
            active_before
        );
    }

    #[test]
    fn ambiguous_workspace_mapping_remains_a_switch_error() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".atc-workspace.toml"),
            "version = 1\npaths = [\n  { pattern = \"^abc\", path = \"one\" },\n  { pattern = \"467$\", path = \"two\" }\n]\n",
        )
        .unwrap();

        let error = switch_error(root.path(), "abc467");

        assert!(error.destination.is_none());
        assert!(
            error
                .error
                .to_string()
                .contains("multiple workspace path rules")
        );
    }

    #[test]
    fn complete_contest_session_stops_and_joins_all_idle_workers() {
        let root = tempfile::tempdir().unwrap();
        save_healthy_contest(root.path(), "abc123");
        let input = PreparedWatchInput::load(root.path(), Some("abc123")).unwrap();
        let session = ContestSession::start(input, &RunnerConfig::default()).unwrap();

        session.shutdown().unwrap();
    }

    #[test]
    fn frontend_channel_borrow_survives_active_worker_shutdown_and_precedes_discard() {
        let root = tempfile::tempdir().unwrap();
        let old_destination = root.path().join("abc123");
        save_healthy_contest(&old_destination, "abc123");
        let input = PreparedWatchInput::load(&old_destination, Some("abc123")).unwrap();

        let (message_tx, message_rx) = mpsc::channel();
        let watcher_thread =
            start_watcher(&input.destination, &input.contest, message_tx.clone()).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (published_tx, published_rx) = mpsc::channel();
        let attempt_messages = message_tx.clone();
        let run_worker = RunWorker::start_with_test_attempt(
            message_tx.clone(),
            move |request, completion_tx| {
                let started_tx = started_tx.clone();
                let published_tx = published_tx.clone();
                let attempt_messages = attempt_messages.clone();
                crate::commands::attempt_executor::spawn_with(
                    request,
                    completion_tx,
                    move |cancellation| {
                        started_tx.send(()).unwrap();
                        while !cancellation.is_requested() {
                            thread::yield_now();
                        }

                        let published = attempt_messages
                            .send(Message::RunEvent {
                                run_id: request.run_id,
                                problem: request.problem,
                                event: TestEvent::NoSamples,
                            })
                            .is_ok();
                        published_tx.send(published).unwrap();

                        crate::attempt::run_attempt(&cancellation, |is_cancelled| {
                            assert!(is_cancelled());
                            Err(AppError::from(crate::attempt::clean_cancellation_io_error()))
                        })
                    },
                )
            },
        )
        .unwrap();
        let run_tx = run_worker.sender();
        let mut detail_analysis_worker = DetailAnalysisWorker::start().unwrap();
        let detail_analysis_tx = detail_analysis_worker.request_sender();
        let detail_analysis_rx = detail_analysis_worker.take_result_receiver();
        let old_probe_tx = message_tx.clone();
        drop(message_tx);

        let session = ContestSession {
            run_worker: Some(run_worker),
            watcher_thread: Some(watcher_thread),
            detail_analysis_worker: Some(detail_analysis_worker),
            input,
            message_rx,
            run_tx,
            detail_analysis_tx,
            detail_analysis_rx,
        };
        session
            .run_tx
            .send(RunWorkerCommand::Run(RunRequest {
                run_id: 1,
                problem: 0,
                language: Language::Cpp,
                debug: false,
                kind: RunKind::Samples,
            }))
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        // This borrowed view represents the endpoint access held by tui::run. Ending the
        // frontend borrow must not disconnect any session channel before shutdown begins.
        {
            let _frontend_channels = session.channels();
        }

        session.shutdown().unwrap();
        assert!(published_rx.recv_timeout(Duration::from_secs(1)).unwrap());

        let fresh_destination = root.path().join("abc467");
        save_healthy_contest(&fresh_destination, "abc467");
        let fresh_input = PreparedWatchInput::load(&fresh_destination, Some("abc467")).unwrap();
        let fresh_session = ContestSession::start(fresh_input, &RunnerConfig::default()).unwrap();

        assert!(
            old_probe_tx
                .send(Message::SourceChanged {
                    problem: 0,
                    path: old_destination.join("A.cpp"),
                    language: Language::Cpp,
                })
                .is_err()
        );
        assert!(matches!(
            fresh_session.message_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        fresh_session.shutdown().unwrap();
    }

    #[test]
    fn partial_new_session_startup_failure_cleans_started_workers() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("abc123");
        save_healthy_contest(&destination, "abc123");
        let input = PreparedWatchInput::load(&destination, Some("abc123")).unwrap();

        let error =
            match ContestSession::start_with_hook(input, &RunnerConfig::default(), |stage| {
                if stage == SessionStartStage::RunWorkerStarted {
                    Err(io::Error::other("injected detail startup failure"))
                } else {
                    Ok(())
                }
            }) {
                Ok(_) => panic!("injected partial startup must fail"),
                Err(error) => error,
            };

        assert!(
            error
                .to_string()
                .contains("injected detail startup failure")
        );
        // On Windows this rename also proves the filesystem watcher released its directory.
        std::fs::rename(&destination, root.path().join("released")).unwrap();
    }

    #[test]
    fn sends_only_existing_exact_sources_and_keeps_metadata_positions() {
        let temp = tempfile::tempdir().unwrap();
        let contest = Contest {
            contest_id: "contest".to_string(),
            problems: vec![problem("B"), problem("A")],
        };
        let watched_sources = build_watched_sources(temp.path(), &contest).unwrap();
        let b_cpp = temp.path().join("B.cpp");
        let a_py = temp.path().join("A.py");
        let helper = temp.path().join("A_brute.py");
        let nested = temp.path().join("nested").join("A.cpp");
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        for path in [&b_cpp, &a_py, &helper, &nested] {
            std::fs::write(path, "source").unwrap();
        }
        let (tx, rx) = mpsc::channel();

        assert!(send_source_changes(
            vec![helper, nested, b_cpp.clone(), a_py.clone()],
            &watched_sources,
            &tx,
        ));

        let messages: Vec<_> = rx.try_iter().collect();
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            Message::SourceChanged { problem: 0, path, language: crate::language::Language::Cpp }
                if path == &b_cpp
        ));
        assert!(matches!(
            &messages[1],
            Message::SourceChanged { problem: 1, path, language: crate::language::Language::Python }
                if path == &a_py
        ));
    }

    #[test]
    fn prebuilt_mapping_recognizes_a_source_created_later() {
        let temp = tempfile::tempdir().unwrap();
        let contest = Contest {
            contest_id: "contest".to_string(),
            problems: vec![problem("A")],
        };
        let watched_sources = build_watched_sources(temp.path(), &contest).unwrap();
        let source = temp.path().join("A.py");
        assert!(!source.exists());
        std::fs::write(&source, "source").unwrap();
        let (tx, rx) = mpsc::channel();

        assert!(send_source_changes(
            vec![source.clone()],
            &watched_sources,
            &tx,
        ));

        assert!(matches!(
            rx.try_recv().unwrap(),
            Message::SourceChanged { problem: 0, path, language: crate::language::Language::Python }
                if path == source
        ));
    }

    #[test]
    fn watcher_thread_stop_joins_the_thread() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Acquire) {
                thread::yield_now();
            }
            Ok(())
        });
        let worker = WatcherThread {
            shutdown,
            handle: Some(handle),
        };

        worker.stop().unwrap();
    }

    #[test]
    fn watcher_thread_panic_is_an_error() {
        let (tx, rx) = mpsc::channel();
        let worker = WatcherThread {
            shutdown: Arc::new(AtomicBool::new(false)),
            handle: Some(thread::spawn(move || {
                run_watcher_guarded(tx, || panic!("watcher panic"))
            })),
        };

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Message::WatcherFailed(error) if error.to_string().contains("panicked")
        ));

        let error = worker.stop().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("panicked"));
    }

    #[test]
    fn terminal_operation_error_remains_primary_when_cleanup_also_fails() {
        let error = combine_primary_and_cleanup_results(
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input failed",
            )),
            [
                (
                    "terminal restoration",
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "mouse reset failed",
                    )),
                ),
                ("worker shutdown", Ok(())),
            ],
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(error.to_string().starts_with("terminal input failed;"));
        assert!(
            error
                .to_string()
                .contains("terminal restoration also failed")
        );
        assert!(error.to_string().contains("mouse reset failed"));
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            "terminal input failed"
        );
    }

    #[test]
    fn cleanup_failure_is_returned_after_successful_terminal_run() {
        let error = combine_primary_and_cleanup_results(
            Ok(()),
            [
                ("terminal restoration", Ok(())),
                (
                    "worker shutdown",
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "worker did not stop",
                    )),
                ),
            ],
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "worker did not stop");
    }

    #[test]
    fn multiple_cleanup_failures_are_all_reported_in_execution_order() {
        let error = combine_primary_and_cleanup_results(
            Ok(()),
            [
                (
                    "terminal restoration",
                    Err(io::Error::other("cursor restore failed")),
                ),
                (
                    "worker shutdown",
                    Err(io::Error::other("worker join failed")),
                ),
            ],
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "terminal restoration failed: cursor restore failed; worker shutdown also failed: worker join failed"
        );
    }
}
