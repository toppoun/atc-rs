use std::fs;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::resolve_language;
use super::test::{find_problem, validate_debug_language};
use crate::config::{Config, RunnerConfig};
use crate::error::AppError;
use crate::language::Language;
use crate::model::Problem;
use crate::safe_file;
use crate::stress::{self, StressRequest};
use crate::template::{stress_brute_template, stress_generator_template};
use crate::ui::{Event, Reporter};
use crate::workspace;

const DEFAULT_STRESS_COUNT: NonZeroU64 = NonZeroU64::new(100).unwrap();
const STRESS_INIT_TEMPORARY_PREFIX: &str = ".atc-stress-init-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StressFileState {
    Missing,
    Exists,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StressFilesStatus {
    pub(crate) generator_missing: bool,
    pub(crate) brute_missing: bool,
}

impl StressFilesStatus {
    pub(crate) fn is_ready(self) -> bool {
        !self.generator_missing && !self.brute_missing
    }
}

struct StressFileTargets {
    generator: PathBuf,
    brute: PathBuf,
}

pub(crate) fn stress_init(
    problem_index: &str,
    cli_contest: Option<&str>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    stress_init_at(&cwd, problem_index, cli_contest, reporter)
}

pub(super) fn stress_init_at(
    cwd: &Path,
    problem_index: &str,
    cli_contest: Option<&str>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let destination = workspace::resolve_contest_target(cwd, cli_contest)?;

    workspace::validate_workspace_marker(&destination)?;

    let contest = workspace::load_metadata(&destination)?;
    workspace::validate_contest_paths(&contest)?;
    if let Some(contest_id) = cli_contest {
        workspace::validate_contest_identity(&contest, contest_id)?;
    }

    let problem = find_problem(&contest, problem_index)?;

    initialize_stress_files_at(&destination, problem, reporter)
}

pub(crate) fn initialize_stress_files_at(
    destination: &Path,
    problem: &Problem,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    initialize_stress_files_at_with(destination, problem, reporter, &mut |path, contents| {
        safe_file::install_noclobber(path, contents, STRESS_INIT_TEMPORARY_PREFIX)
    })
}

fn initialize_stress_files_at_with(
    destination: &Path,
    problem: &Problem,
    reporter: &mut dyn Reporter,
    installer: &mut dyn FnMut(&Path, &[u8]) -> io::Result<()>,
) -> Result<(), AppError> {
    let targets = stress_file_targets_at(destination, problem)?;

    let targets = [
        (
            targets.generator,
            stress_generator_template().as_bytes(),
            "stress generator",
        ),
        (
            targets.brute,
            stress_brute_template().as_bytes(),
            "stress brute-force solution",
        ),
    ];

    let states = [
        inspect_stress_file(&targets[0].0, targets[0].2)?,
        inspect_stress_file(&targets[1].0, targets[1].2)?,
    ];

    if states == [StressFileState::Exists, StressFileState::Exists] {
        reporter.report(Event::StressFilesAlreadyInitialized {
            problem_index: &problem.index,
        });
        return Ok(());
    }

    for ((path, contents, kind), state) in targets.iter().zip(states) {
        match state {
            StressFileState::Exists => {
                reporter.report(Event::StressFileExists { path });
            }
            StressFileState::Missing => match installer(path, contents) {
                Ok(()) => {
                    reporter.report(Event::StressFileCreated { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match inspect_stress_file(path, kind)? {
                        StressFileState::Exists => {
                            reporter.report(Event::StressFileExists { path });
                        }
                        StressFileState::Missing => return Err(error.into()),
                    }
                }
                Err(error) => return Err(error.into()),
            },
        }
    }

    Ok(())
}

pub(crate) fn inspect_stress_files_at(
    destination: &Path,
    problem: &Problem,
) -> Result<StressFilesStatus, AppError> {
    let targets = stress_file_targets_at(destination, problem)?;
    let generator = inspect_stress_file(&targets.generator, "stress generator")?;
    let brute = inspect_stress_file(&targets.brute, "stress brute-force solution")?;

    Ok(StressFilesStatus {
        generator_missing: generator == StressFileState::Missing,
        brute_missing: brute == StressFileState::Missing,
    })
}

fn stress_file_targets_at(
    destination: &Path,
    problem: &Problem,
) -> Result<StressFileTargets, AppError> {
    workspace::validate_problem_index(&problem.index)?;

    Ok(StressFileTargets {
        generator: destination.join(format!("{}_gen.py", problem.index)),
        brute: destination.join(format!("{}_brute.py", problem.index)),
    })
}

fn inspect_stress_file(path: &Path, kind: &str) -> io::Result<StressFileState> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(StressFileState::Exists),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} target is not a regular file: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(StressFileState::Missing),
        Err(error) => Err(error),
    }
}

pub(crate) fn stress(
    problem_index: &str,
    cli_contest: Option<&str>,
    cli_language: Option<Language>,
    debug: bool,
    count: Option<NonZeroU64>,
    forever: bool,
    seed: Option<u64>,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let destination = workspace::resolve_contest_target(&cwd, cli_contest)?;

    workspace::validate_workspace_marker(&destination)?;

    let contest = workspace::load_metadata(&destination)?;
    workspace::validate_contest_paths(&contest)?;
    if let Some(contest_id) = cli_contest {
        workspace::validate_contest_identity(&contest, contest_id)?;
    }

    let problem = find_problem(&contest, problem_index)?;
    let config = Config::load()?;
    let language = resolve_language(cli_language, &config);

    let base_seed = match seed {
        Some(seed) => seed,
        None => stress::automatic_seed()?,
    };
    let count = if forever {
        None
    } else {
        Some(count.unwrap_or(DEFAULT_STRESS_COUNT))
    };

    let request = build_request(
        &destination,
        &contest.contest_id,
        problem,
        language,
        &config.runner,
        debug,
        base_seed,
        count,
    )?;

    let _ = stress::run(&request, reporter, &|| false)?;
    Ok(())
}

pub(super) fn build_request(
    destination: &Path,
    contest_id: &str,
    problem: &Problem,
    language: Language,
    runner: &RunnerConfig,
    debug: bool,
    base_seed: u64,
    count: Option<NonZeroU64>,
) -> Result<StressRequest, AppError> {
    validate_debug_language(language, debug)?;
    validate_finite_seed_range(base_seed, count)?;

    let candidate_source = destination.join(format!("{}.{}", problem.index, language.extension()));
    let generator_source = destination.join(format!("{}_gen.py", problem.index));
    let reference_source = destination.join(format!("{}_brute.py", problem.index));

    require_file(&candidate_source, "candidate source")?;
    require_file(&generator_source, "stress generator")?;
    require_file(&reference_source, "stress reference")?;

    let timeout = duration_from_seconds(runner.timeout_seconds, "runner.timeout_seconds")?;
    let compile_timeout = duration_from_seconds(
        runner.compile_timeout_seconds,
        "runner.compile_timeout_seconds",
    )?;

    Ok(StressRequest {
        destination: destination.to_path_buf(),
        contest_id: contest_id.to_string(),
        problem_index: problem.index.clone(),
        candidate_source,
        candidate_language: language,
        generator_source,
        reference_source,
        python: runner.python.clone(),
        cpp_compiler: runner.cpp_compiler.clone(),
        cpp_flags: runner.cpp_flags.clone(),
        candidate_timeout: timeout,
        generator_timeout: timeout,
        reference_timeout: timeout,
        compile_timeout,
        base_seed,
        count,
        debug,
    })
}

fn validate_finite_seed_range(base_seed: u64, count: Option<NonZeroU64>) -> Result<(), AppError> {
    let Some(last_case_number) = count.map(NonZeroU64::get) else {
        return Ok(());
    };
    let last_offset = last_case_number - 1;
    if base_seed.checked_add(last_offset).is_some() {
        return Ok(());
    }

    Err(stress::StressError::SeedOverflow {
        base_seed,
        case_number: last_case_number,
    }
    .into())
}

fn require_file(path: &Path, kind: &str) -> io::Result<()> {
    if path.is_file() {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{kind} not found: {}", path.display()),
    ))
}

fn duration_from_seconds(seconds: f64, name: &str) -> io::Result<Duration> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} must be a positive finite duration"),
        ));
    }

    let duration = Duration::try_from_secs_f64(seconds).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {name}: {error}"),
        )
    })?;

    if duration.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} is too small to represent as a positive duration"),
        ));
    }

    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Contest;
    use std::path::PathBuf;

    #[derive(Default)]
    struct RecordingReporter {
        events: Vec<String>,
    }

    impl Reporter for RecordingReporter {
        fn report(&mut self, event: Event<'_>) {
            match event {
                Event::StressFileCreated { path } => {
                    self.events.push(format!("created:{}", path.display()));
                }
                Event::StressFileExists { path } => {
                    self.events.push(format!("exists:{}", path.display()));
                }
                Event::StressFilesAlreadyInitialized { problem_index } => {
                    self.events.push(format!("already:{problem_index}"));
                }
                _ => panic!("unexpected event"),
            }
        }
    }

    fn problem(index: &str) -> Problem {
        Problem {
            index: index.to_string(),
            title: format!("Problem {index}"),
            task_id: format!("abc466_{}", index.to_ascii_lowercase()),
            url: format!("https://example.invalid/{index}"),
        }
    }

    fn create_contest(destination: &Path, contest_id: &str, indices: &[&str]) {
        fs::create_dir_all(destination).unwrap();
        workspace::save_metadata(
            destination,
            &Contest {
                contest_id: contest_id.to_string(),
                problems: indices.iter().map(|index| problem(index)).collect(),
            },
        )
        .unwrap();
    }

    fn stress_paths(destination: &Path, index: &str) -> (PathBuf, PathBuf) {
        (
            destination.join(format!("{index}_gen.py")),
            destination.join(format!("{index}_brute.py")),
        )
    }

    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(target, link);

        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("failed to create file symlink: {error}"),
        }
    }

    fn create_directory_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(target, link);

        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("failed to create directory symlink: {error}"),
        }
    }

    fn assert_invalid_input(error: AppError) {
        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn default_count_is_finite_and_nonzero() {
        assert_eq!(DEFAULT_STRESS_COUNT.get(), 100);
    }

    #[test]
    fn finite_seed_range_is_validated_before_execution() {
        assert!(validate_finite_seed_range(u64::MAX, NonZeroU64::new(1)).is_ok());
        assert!(validate_finite_seed_range(u64::MAX - 1, NonZeroU64::new(2)).is_ok());
        assert!(validate_finite_seed_range(u64::MAX, None).is_ok());

        let error = validate_finite_seed_range(u64::MAX, NonZeroU64::new(2)).unwrap_err();
        assert!(matches!(
            error,
            AppError::Stress(stress::StressError::SeedOverflow {
                base_seed: u64::MAX,
                case_number: 2,
            })
        ));
    }

    #[test]
    fn missing_required_file_is_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let error = require_file(&temp.path().join("missing.py"), "stress generator").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("stress generator not found"));
    }

    #[test]
    fn inspection_reports_the_regular_file_readiness_matrix() {
        for (generator_exists, brute_exists, expected) in [
            (
                false,
                false,
                StressFilesStatus {
                    generator_missing: true,
                    brute_missing: true,
                },
            ),
            (
                true,
                false,
                StressFilesStatus {
                    generator_missing: false,
                    brute_missing: true,
                },
            ),
            (
                false,
                true,
                StressFilesStatus {
                    generator_missing: true,
                    brute_missing: false,
                },
            ),
            (
                true,
                true,
                StressFilesStatus {
                    generator_missing: false,
                    brute_missing: false,
                },
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (generator, brute) = stress_paths(temp.path(), "A");
            if generator_exists {
                fs::write(generator, b"generator").unwrap();
            }
            if brute_exists {
                fs::write(brute, b"brute").unwrap();
            }

            let status = inspect_stress_files_at(temp.path(), &problem("A")).unwrap();

            assert_eq!(status, expected);
            assert_eq!(status.is_ready(), generator_exists && brute_exists);
        }
    }

    #[test]
    fn inspection_uses_the_canonical_problem_index_and_is_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let missing_destination = temp.path().join("does-not-exist");

        let status = inspect_stress_files_at(&missing_destination, &problem("Z")).unwrap();

        assert_eq!(
            status,
            StressFilesStatus {
                generator_missing: true,
                brute_missing: true,
            }
        );
        assert!(!missing_destination.exists());
        assert!(!temp.path().join("A_gen.py").exists());
        assert!(!temp.path().join("A_brute.py").exists());
    }

    #[test]
    fn inspection_rejects_directories_and_symlinks_without_following_them() {
        for invalid_kind in [
            "directory",
            "file-symlink",
            "directory-symlink",
            "dangling-symlink",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (generator, _) = stress_paths(temp.path(), "A");
            let created = match invalid_kind {
                "directory" => {
                    fs::create_dir(&generator).unwrap();
                    true
                }
                "file-symlink" => {
                    let target = temp.path().join("external-file");
                    fs::write(&target, b"external").unwrap();
                    create_file_symlink(&target, &generator)
                }
                "directory-symlink" => {
                    let target = temp.path().join("external-directory");
                    fs::create_dir(&target).unwrap();
                    create_directory_symlink(&target, &generator)
                }
                "dangling-symlink" => {
                    create_file_symlink(&temp.path().join("missing-target"), &generator)
                }
                _ => unreachable!(),
            };
            if !created {
                continue;
            }

            let error = inspect_stress_files_at(temp.path(), &problem("A")).unwrap_err();

            assert_invalid_input(error);
            assert!(fs::symlink_metadata(&generator).is_ok());
        }
    }

    #[cfg(unix)]
    #[test]
    fn inspection_rejects_a_special_filesystem_object() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        let (generator, _) = stress_paths(temp.path(), "A");
        let _listener = UnixListener::bind(&generator).unwrap();

        let error = inspect_stress_files_at(temp.path(), &problem("A")).unwrap_err();

        assert_invalid_input(error);
    }

    #[test]
    fn initializes_exact_files_without_candidate_config_or_stress_state() {
        let temp = tempfile::tempdir().unwrap();
        create_contest(temp.path(), "abc466", &["A"]);
        let (generator, brute) = stress_paths(temp.path(), "A");
        let mut reporter = RecordingReporter::default();

        stress_init_at(temp.path(), "a", None, &mut reporter).unwrap();

        assert_eq!(
            fs::read(&generator).unwrap(),
            stress_generator_template().as_bytes()
        );
        assert_eq!(
            fs::read(&brute).unwrap(),
            stress_brute_template().as_bytes()
        );
        assert!(!temp.path().join("A.cpp").exists());
        assert!(!temp.path().join("A.py").exists());
        assert!(!temp.path().join(".atc").join("stress").exists());
        assert_eq!(
            reporter.events,
            [
                format!("created:{}", generator.display()),
                format!("created:{}", brute.display()),
            ]
        );
        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(STRESS_INIT_TEMPORARY_PREFIX)
        }));
    }

    #[test]
    fn creation_matrix_preserves_every_existing_regular_file_byte_for_byte() {
        for (generator_bytes, brute_bytes, expected_events) in [
            (
                Some(&b"\xff\x00user generator\r\n"[..]),
                None,
                ["exists-generator", "created-brute"],
            ),
            (
                None,
                Some(&b"\x80\x00user brute\n"[..]),
                ["created-generator", "exists-brute"],
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            create_contest(temp.path(), "abc466", &["A"]);
            let (generator, brute) = stress_paths(temp.path(), "A");
            if let Some(bytes) = generator_bytes {
                fs::write(&generator, bytes).unwrap();
            }
            if let Some(bytes) = brute_bytes {
                fs::write(&brute, bytes).unwrap();
            }
            let mut reporter = RecordingReporter::default();

            initialize_stress_files_at(temp.path(), &problem("A"), &mut reporter).unwrap();

            assert_eq!(
                fs::read(&generator).unwrap(),
                generator_bytes.unwrap_or_else(|| stress_generator_template().as_bytes())
            );
            assert_eq!(
                fs::read(&brute).unwrap(),
                brute_bytes.unwrap_or_else(|| stress_brute_template().as_bytes())
            );
            let semantic_events = reporter
                .events
                .iter()
                .map(|event| {
                    if event == &format!("created:{}", generator.display()) {
                        "created-generator"
                    } else if event == &format!("exists:{}", generator.display()) {
                        "exists-generator"
                    } else if event == &format!("created:{}", brute.display()) {
                        "created-brute"
                    } else if event == &format!("exists:{}", brute.display()) {
                        "exists-brute"
                    } else {
                        panic!("unexpected event: {event}")
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(semantic_events, expected_events);
        }

        let temp = tempfile::tempdir().unwrap();
        create_contest(temp.path(), "abc466", &["A"]);
        let (generator, brute) = stress_paths(temp.path(), "A");
        let generator_bytes = b"existing generator\x00\xff";
        let brute_bytes = b"existing brute\x00\x80";
        fs::write(&generator, generator_bytes).unwrap();
        fs::write(&brute, brute_bytes).unwrap();
        let mut reporter = RecordingReporter::default();

        initialize_stress_files_at(temp.path(), &problem("A"), &mut reporter).unwrap();

        assert_eq!(fs::read(generator).unwrap(), generator_bytes);
        assert_eq!(fs::read(brute).unwrap(), brute_bytes);
        assert_eq!(reporter.events, ["already:A"]);
    }

    #[test]
    fn preflights_both_targets_before_creating_either_file() {
        for invalid_index in 0..2 {
            let temp = tempfile::tempdir().unwrap();
            create_contest(temp.path(), "abc466", &["A"]);
            let paths = stress_paths(temp.path(), "A");
            let invalid = if invalid_index == 0 {
                &paths.0
            } else {
                &paths.1
            };
            let other = if invalid_index == 0 {
                &paths.1
            } else {
                &paths.0
            };
            fs::create_dir(invalid).unwrap();
            let mut reporter = RecordingReporter::default();

            let error =
                initialize_stress_files_at(temp.path(), &problem("A"), &mut reporter).unwrap_err();

            assert_invalid_input(error);
            assert!(!other.exists());
            assert!(reporter.events.is_empty());
        }
    }

    #[test]
    fn preflight_rejects_file_and_dangling_symlinks_without_following_them() {
        for symlink_index in 0..2 {
            let temp = tempfile::tempdir().unwrap();
            create_contest(temp.path(), "abc466", &["A"]);
            let external = temp.path().join("external-user-file");
            fs::write(&external, b"external bytes").unwrap();
            let paths = stress_paths(temp.path(), "A");
            let link = if symlink_index == 0 {
                &paths.0
            } else {
                &paths.1
            };
            let other = if symlink_index == 0 {
                &paths.1
            } else {
                &paths.0
            };
            if !create_file_symlink(&external, link) {
                return;
            }
            let mut reporter = RecordingReporter::default();

            let error =
                initialize_stress_files_at(temp.path(), &problem("A"), &mut reporter).unwrap_err();

            assert_invalid_input(error);
            assert_eq!(fs::read(&external).unwrap(), b"external bytes");
            assert!(!other.exists());
            assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        }

        let temp = tempfile::tempdir().unwrap();
        create_contest(temp.path(), "abc466", &["A"]);
        let (generator, brute) = stress_paths(temp.path(), "A");
        if !create_file_symlink(&temp.path().join("missing-target"), &generator) {
            return;
        }
        let mut reporter = RecordingReporter::default();

        let error =
            initialize_stress_files_at(temp.path(), &problem("A"), &mut reporter).unwrap_err();

        assert_invalid_input(error);
        assert!(!brute.exists());
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_a_special_filesystem_object() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        create_contest(temp.path(), "abc466", &["A"]);
        let (generator, brute) = stress_paths(temp.path(), "A");
        let _listener = UnixListener::bind(&generator).unwrap();
        let mut reporter = RecordingReporter::default();

        let error =
            initialize_stress_files_at(temp.path(), &problem("A"), &mut reporter).unwrap_err();

        assert_invalid_input(error);
        assert!(!brute.exists());
    }

    #[test]
    fn race_created_regular_file_is_reclassified_as_existing() {
        let temp = tempfile::tempdir().unwrap();
        create_contest(temp.path(), "abc466", &["A"]);
        let (generator, brute) = stress_paths(temp.path(), "A");
        let competitor = b"competitor generator bytes\x00\xff";
        let mut first = true;
        let mut reporter = RecordingReporter::default();
        let mut installer = |path: &Path, contents: &[u8]| {
            if first {
                first = false;
                fs::write(path, competitor).unwrap();
            }
            safe_file::install_noclobber(path, contents, STRESS_INIT_TEMPORARY_PREFIX)
        };

        initialize_stress_files_at_with(temp.path(), &problem("A"), &mut reporter, &mut installer)
            .unwrap();

        assert_eq!(fs::read(&generator).unwrap(), competitor);
        assert_eq!(
            fs::read(&brute).unwrap(),
            stress_brute_template().as_bytes()
        );
        assert_eq!(
            reporter.events,
            [
                format!("exists:{}", generator.display()),
                format!("created:{}", brute.display()),
            ]
        );
    }

    #[test]
    fn race_created_directory_is_an_error_and_is_not_replaced() {
        let temp = tempfile::tempdir().unwrap();
        create_contest(temp.path(), "abc466", &["A"]);
        let (generator, brute) = stress_paths(temp.path(), "A");
        let mut reporter = RecordingReporter::default();
        let mut installer = |path: &Path, contents: &[u8]| {
            fs::create_dir(path).unwrap();
            safe_file::install_noclobber(path, contents, STRESS_INIT_TEMPORARY_PREFIX)
        };

        let error = initialize_stress_files_at_with(
            temp.path(),
            &problem("A"),
            &mut reporter,
            &mut installer,
        )
        .unwrap_err();

        assert_invalid_input(error);
        assert!(generator.is_dir());
        assert!(!brute.exists());
        assert!(reporter.events.is_empty());
    }

    #[test]
    fn race_created_symlink_is_an_error_and_is_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        create_contest(temp.path(), "abc466", &["A"]);
        let (generator, brute) = stress_paths(temp.path(), "A");
        let external = temp.path().join("external");
        fs::write(&external, b"external user bytes").unwrap();
        let probe = temp.path().join("symlink-probe");
        if !create_file_symlink(&external, &probe) {
            return;
        }
        fs::remove_file(probe).unwrap();
        let mut reporter = RecordingReporter::default();
        let mut installer = |path: &Path, contents: &[u8]| {
            assert!(create_file_symlink(&external, path));
            safe_file::install_noclobber(path, contents, STRESS_INIT_TEMPORARY_PREFIX)
        };

        let error = initialize_stress_files_at_with(
            temp.path(),
            &problem("A"),
            &mut reporter,
            &mut installer,
        )
        .unwrap_err();

        assert_invalid_input(error);
        assert_eq!(fs::read(external).unwrap(), b"external user bytes");
        assert!(
            fs::symlink_metadata(generator)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!brute.exists());
    }

    #[test]
    fn completed_first_install_remains_after_second_race_failure_and_rerun_fills_gap() {
        let temp = tempfile::tempdir().unwrap();
        create_contest(temp.path(), "abc466", &["A"]);
        let (generator, brute) = stress_paths(temp.path(), "A");
        let mut calls = 0;
        let mut reporter = RecordingReporter::default();
        let mut installer = |path: &Path, contents: &[u8]| {
            calls += 1;
            if calls == 2 {
                fs::create_dir(path).unwrap();
            }
            safe_file::install_noclobber(path, contents, STRESS_INIT_TEMPORARY_PREFIX)
        };

        let error = initialize_stress_files_at_with(
            temp.path(),
            &problem("A"),
            &mut reporter,
            &mut installer,
        )
        .unwrap_err();

        assert_invalid_input(error);
        assert_eq!(
            fs::read(&generator).unwrap(),
            stress_generator_template().as_bytes()
        );
        assert!(brute.is_dir());
        assert_eq!(
            reporter.events,
            [format!("created:{}", generator.display())]
        );

        fs::remove_dir(&brute).unwrap();
        let mut rerun_reporter = RecordingReporter::default();
        initialize_stress_files_at(temp.path(), &problem("A"), &mut rerun_reporter).unwrap();

        assert_eq!(
            fs::read(&brute).unwrap(),
            stress_brute_template().as_bytes()
        );
        assert_eq!(
            rerun_reporter.events,
            [
                format!("exists:{}", generator.display()),
                format!("created:{}", brute.display()),
            ]
        );
    }

    #[test]
    fn resolves_explicit_nested_workspace_without_creating_missing_mappings() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join(".atc-workspace.toml"),
            concat!(
                "version = 1\n",
                "[[paths]]\n",
                "pattern = \"^abc[0-9]+$\"\n",
                "path = \"AtCoder/Contests/ABC\"\n",
            ),
        )
        .unwrap();
        let destination = root
            .path()
            .join("AtCoder")
            .join("Contests")
            .join("ABC")
            .join("abc466");
        create_contest(&destination, "abc466", &["A"]);
        let mut reporter = RecordingReporter::default();

        stress_init_at(root.path(), "A", Some("abc466"), &mut reporter).unwrap();

        assert!(destination.join("A_gen.py").is_file());
        assert!(destination.join("A_brute.py").is_file());

        let missing_root = tempfile::tempdir().unwrap();
        fs::write(
            missing_root.path().join(".atc-workspace.toml"),
            concat!(
                "version = 1\n",
                "[[paths]]\n",
                "pattern = \"^abc[0-9]+$\"\n",
                "path = \"AtCoder/Contests/ABC\"\n",
            ),
        )
        .unwrap();
        let mut reporter = RecordingReporter::default();
        let error =
            stress_init_at(missing_root.path(), "A", Some("abc466"), &mut reporter).unwrap_err();

        assert!(matches!(
            error,
            AppError::Io(source) if source.kind() == io::ErrorKind::NotFound
        ));
        assert!(!missing_root.path().join("AtCoder").exists());
        assert!(reporter.events.is_empty());
    }

    #[test]
    fn direct_mode_never_searches_a_parent_workspace() {
        let parent = tempfile::tempdir().unwrap();
        fs::write(
            parent.path().join(".atc-workspace.toml"),
            "version = 1\n[[paths]]\npattern = \"^abc\"\npath = \"parent-only\"\n",
        )
        .unwrap();
        let contest = parent.path().join("child-contest");
        create_contest(&contest, "abc466", &["A"]);
        let mut reporter = RecordingReporter::default();

        stress_init_at(&contest, "A", None, &mut reporter).unwrap();

        assert!(contest.join("A_gen.py").is_file());
        assert!(!parent.path().join("parent-only").exists());

        let nested_cwd = parent.path().join("nested-cwd");
        fs::create_dir(&nested_cwd).unwrap();
        let mut reporter = RecordingReporter::default();
        stress_init_at(&nested_cwd, "A", Some("abc466"), &mut reporter)
            .expect_err("the parent workspace mapping must not be discovered");
        assert!(!parent.path().join("parent-only").exists());
        assert!(!nested_cwd.join("abc466").exists());
    }

    #[test]
    fn resolution_rejects_missing_contest_identity_mismatch_and_unknown_problem() {
        let root = tempfile::tempdir().unwrap();
        let mut reporter = RecordingReporter::default();
        let missing = stress_init_at(root.path(), "A", Some("abc466"), &mut reporter).unwrap_err();
        assert!(matches!(
            missing,
            AppError::Io(source) if source.kind() == io::ErrorKind::NotFound
        ));
        assert!(!root.path().join("abc466").exists());

        let destination = root.path().join("abc466");
        create_contest(&destination, "arc001", &["A"]);
        let mismatch = stress_init_at(root.path(), "A", Some("abc466"), &mut reporter).unwrap_err();
        assert!(matches!(
            mismatch,
            AppError::Io(source) if source.kind() == io::ErrorKind::InvalidData
        ));
        assert!(!destination.join("A_gen.py").exists());

        let unknown = stress_init_at(&destination, "Z", None, &mut reporter).unwrap_err();
        assert!(matches!(
            unknown,
            AppError::Io(source) if source.kind() == io::ErrorKind::NotFound
        ));
        assert!(!destination.join("Z_gen.py").exists());
    }

    #[test]
    fn invalid_and_duplicate_metadata_indices_are_rejected_before_creation() {
        for indices in [&["../outside"][..], &["A", "a"][..]] {
            let temp = tempfile::tempdir().unwrap();
            let destination = temp.path().join("contest");
            fs::create_dir_all(destination.join(".atc")).unwrap();
            let mut metadata = String::from("version = 1\ncontest_id = \"abc466\"\n");
            for index in indices {
                metadata.push_str(&format!(
                    concat!(
                        "[[problems]]\n",
                        "index = {index:?}\n",
                        "title = \"problem\"\n",
                        "task_id = \"task\"\n",
                        "url = \"https://example.invalid\"\n",
                    ),
                    index = index,
                ));
            }
            fs::write(destination.join(".atc").join("contest.toml"), metadata).unwrap();
            let mut reporter = RecordingReporter::default();

            let error = stress_init_at(&destination, "A", None, &mut reporter).unwrap_err();

            assert!(matches!(
                error,
                AppError::Io(source)
                    if matches!(source.kind(), io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData)
            ));
            assert!(!destination.join("A_gen.py").exists());
            assert!(!destination.join("A_brute.py").exists());
            assert!(!temp.path().join("outside_gen.py").exists());
            assert!(reporter.events.is_empty());
        }
    }
}
