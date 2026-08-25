use super::{FetchedContestData, fetch_contest_data, fetch_samples_for_manifest, resolve_language};
use crate::app_context::AppContext;
use crate::atcoder;
use crate::config::Config;
use crate::error::AppError;
use crate::model::{Contest, Sample};
use crate::template::resolve_source_template;
use crate::ui::{Event, Reporter};
use crate::workspace::{self, ContestDataReplacement, ContestMetadataHealth, TestsHealth};
use std::io::{self, BufRead, Write};
use std::path::Path;

pub(super) enum ContestTargetHealth {
    MissingDirectory,
    RepairRequired,
    UnsupportedVersion(u32),
    Healthy,
}

pub(super) fn inspect_contest_target(
    destination: &Path,
    contest_id: &str,
) -> io::Result<ContestTargetHealth> {
    if !workspace::contest_directory_exists(destination)? {
        return Ok(ContestTargetHealth::MissingDirectory);
    }

    match workspace::inspect_contest_metadata(destination)? {
        ContestMetadataHealth::Healthy(contest) => {
            workspace::validate_contest_identity(&contest, contest_id)?;
            workspace::validate_contest_paths(&contest)?;
            match workspace::inspect_tests_health(destination, &contest)? {
                TestsHealth::Healthy => Ok(ContestTargetHealth::Healthy),
                TestsHealth::Broken => Ok(ContestTargetHealth::RepairRequired),
            }
        }
        ContestMetadataHealth::Missing | ContestMetadataHealth::Invalid => {
            Ok(ContestTargetHealth::RepairRequired)
        }
        ContestMetadataHealth::UnsupportedVersion(version) => {
            Ok(ContestTargetHealth::UnsupportedVersion(version))
        }
    }
}

pub(crate) fn contest(contest_id: &str, reporter: &mut dyn Reporter) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;
    let app_context = AppContext::from_launch_root(&cwd)?;

    let destination = workspace::resolve_contest_path(&cwd, contest_id)?;

    contest_at(
        &destination,
        contest_id,
        reporter,
        |destination| confirm_repair(destination).map_err(AppError::from),
        |destination, contest_id, reporter| create_contest(&cwd, destination, contest_id, reporter),
        repair_contest,
        |destination, contest_id, _| {
            super::watch_tui::watch_tui_at(destination, Some(contest_id), app_context.clone())
        },
    )
}

fn contest_at<C, N, R, W>(
    destination: &Path,
    contest_id: &str,
    reporter: &mut dyn Reporter,
    mut confirm: C,
    mut create: N,
    mut repair: R,
    mut watch: W,
) -> Result<(), AppError>
where
    C: FnMut(&Path) -> Result<bool, AppError>,
    N: FnMut(&Path, &str, &mut dyn Reporter) -> Result<(), AppError>,
    R: FnMut(&Path, &str, &mut dyn Reporter) -> Result<(), AppError>,
    W: FnMut(&Path, &str, &mut dyn Reporter) -> Result<(), AppError>,
{
    match inspect_contest_target(destination, contest_id)? {
        ContestTargetHealth::MissingDirectory => create(destination, contest_id, reporter)?,
        ContestTargetHealth::Healthy => {}
        ContestTargetHealth::RepairRequired => {
            if !confirm(destination)? {
                return Ok(());
            }

            repair(destination, contest_id, reporter)?;
        }
        ContestTargetHealth::UnsupportedVersion(version) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported contest metadata version: {version}; \
                     refusing to repair automatically"
                ),
            )
            .into());
        }
    }

    watch(destination, contest_id, reporter)
}

pub(super) fn create_contest(
    root: &Path,
    destination: &Path,
    contest_id: &str,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    create_contest_with(
        root,
        destination,
        contest_id,
        reporter,
        Config::load,
        resolve_source_template,
        create_atcoder_client,
    )
}

fn create_contest_with<L, R, C>(
    root: &Path,
    destination: &Path,
    contest_id: &str,
    reporter: &mut dyn Reporter,
    load_config: L,
    resolve_template: R,
    create_client: C,
) -> Result<(), AppError>
where
    L: FnOnce() -> Result<Config, AppError>,
    R: FnOnce(crate::language::Language) -> Result<String, AppError>,
    C: FnOnce() -> Result<atcoder::AtCoderClient, AppError>,
{
    let config = load_config()?;
    let language = resolve_language(None, &config);
    let template = resolve_template(language)?;
    let atcoder = create_client()?;

    super::new::new_at_in_workspace(
        root,
        destination,
        contest_id,
        language,
        &template,
        &atcoder,
        reporter,
    )
}

pub(super) fn repair_contest(
    destination: &Path,
    contest_id: &str,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let atcoder = create_atcoder_client()?;

    repair_at(destination, contest_id, &atcoder, reporter)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairComponent {
    Keep,
    Restore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairBasis {
    HealthyManifest,
    MissingOrInvalidManifest,
}

struct RepairPlan {
    contest: Contest,
    samples_by_problem: Vec<Vec<Sample>>,
    metadata: RepairComponent,
    tests: RepairComponent,
    basis: RepairBasis,
}

fn repair_at(
    destination: &Path,
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    repair_at_with_before_install(destination, contest_id, atcoder, reporter, || {})
}

fn repair_at_with_before_install(
    destination: &Path,
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
    before_install: impl FnOnce(),
) -> Result<(), AppError> {
    workspace::validate_refresh_destination(destination, contest_id, true)?;
    let plan = plan_repair(destination, contest_id, atcoder, reporter)?;

    let staging = tempfile::Builder::new()
        .prefix(".atc-repair-")
        .tempdir_in(destination)?;
    workspace::save_metadata(staging.path(), &plan.contest)?;
    if plan.tests == RepairComponent::Restore {
        for (problem, samples) in plan.contest.problems.iter().zip(&plan.samples_by_problem) {
            workspace::save_samples(staging.path(), problem, samples)?;
        }
    }

    before_install();
    revalidate_repair_plan(destination, contest_id, &plan)?;

    let replacement = match (plan.metadata, plan.tests) {
        (RepairComponent::Restore, RepairComponent::Keep) => ContestDataReplacement::MetadataOnly,
        (RepairComponent::Keep, RepairComponent::Restore) => ContestDataReplacement::TestsOnly,
        (RepairComponent::Restore, RepairComponent::Restore) => {
            ContestDataReplacement::MetadataAndTests
        }
        (RepairComponent::Keep, RepairComponent::Keep) => {
            return Err(io::Error::other("contest no longer requires repair").into());
        }
    };

    workspace::replace_contest_data(destination, staging, true, replacement)?;

    match inspect_contest_target(destination, contest_id)? {
        ContestTargetHealth::Healthy => {
            reporter.report(Event::WorkspaceRepaired { destination });
            Ok(())
        }
        ContestTargetHealth::MissingDirectory => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "contest repair completed but the destination is missing",
        )
        .into()),
        ContestTargetHealth::RepairRequired => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "contest repair completed but contest data still requires repair",
        )
        .into()),
        ContestTargetHealth::UnsupportedVersion(version) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("contest repair installed unsupported metadata version {version}"),
        )
        .into()),
    }
}

fn plan_repair(
    destination: &Path,
    contest_id: &str,
    atcoder: &atcoder::AtCoderClient,
    reporter: &mut dyn Reporter,
) -> Result<RepairPlan, AppError> {
    match workspace::inspect_contest_metadata(destination)? {
        ContestMetadataHealth::Healthy(contest) => {
            workspace::validate_contest_identity(&contest, contest_id)?;
            workspace::validate_contest_paths(&contest)?;
            if workspace::inspect_tests_health(destination, &contest)? == TestsHealth::Healthy {
                return Err(io::Error::other("contest no longer requires repair").into());
            }

            workspace::preflight_tests_replacement(destination, &contest)?;
            let samples_by_problem = fetch_samples_for_manifest(&contest, atcoder, reporter)?;
            Ok(RepairPlan {
                contest,
                samples_by_problem,
                metadata: RepairComponent::Keep,
                tests: RepairComponent::Restore,
                basis: RepairBasis::HealthyManifest,
            })
        }
        ContestMetadataHealth::Missing | ContestMetadataHealth::Invalid => {
            let FetchedContestData {
                contest,
                samples_by_problem,
            } = fetch_contest_data(contest_id, atcoder, reporter)?;
            workspace::validate_contest_identity(&contest, contest_id)?;
            workspace::validate_contest_paths(&contest)?;

            let tests = match workspace::inspect_tests_health(destination, &contest)? {
                TestsHealth::Healthy => RepairComponent::Keep,
                TestsHealth::Broken => {
                    workspace::preflight_tests_replacement(destination, &contest)?;
                    RepairComponent::Restore
                }
            };
            Ok(RepairPlan {
                contest,
                samples_by_problem,
                metadata: RepairComponent::Restore,
                tests,
                basis: RepairBasis::MissingOrInvalidManifest,
            })
        }
        ContestMetadataHealth::UnsupportedVersion(version) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported contest metadata version: {version}; refusing to repair automatically"
            ),
        )
        .into()),
    }
}

fn revalidate_repair_plan(
    destination: &Path,
    contest_id: &str,
    plan: &RepairPlan,
) -> Result<(), AppError> {
    workspace::validate_refresh_destination(destination, contest_id, true)?;
    match (
        plan.basis,
        workspace::inspect_contest_metadata(destination)?,
    ) {
        (RepairBasis::HealthyManifest, ContestMetadataHealth::Healthy(current))
            if current == plan.contest => {}
        (
            RepairBasis::MissingOrInvalidManifest,
            ContestMetadataHealth::Missing | ContestMetadataHealth::Invalid,
        ) => {}
        _ => return Err(repair_plan_changed_error().into()),
    }

    let current_tests = workspace::inspect_tests_health(destination, &plan.contest)?;
    let expected_tests = match plan.tests {
        RepairComponent::Keep => TestsHealth::Healthy,
        RepairComponent::Restore => TestsHealth::Broken,
    };
    if current_tests != expected_tests {
        return Err(repair_plan_changed_error().into());
    }
    if plan.tests == RepairComponent::Restore {
        workspace::preflight_tests_replacement(destination, &plan.contest)?;
    }

    Ok(())
}

fn repair_plan_changed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "contest data changed while repair was being prepared; retry repair",
    )
}

fn create_atcoder_client() -> Result<atcoder::AtCoderClient, AppError> {
    if let Some(path) = std::env::var_os("ATC_FIXTURE_DIR") {
        Ok(atcoder::AtCoderClient::fixture(path))
    } else {
        Ok(atcoder::AtCoderClient::new()?)
    }
}

fn confirm_repair(destination: &Path) -> io::Result<bool> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    confirm_repair_with(destination, &mut stdin.lock(), &mut stdout.lock())
}

fn confirm_repair_with(
    destination: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    write!(
        output,
        "Contest data is missing or structurally inconsistent:\n{}\nRepair only unusable atc-managed components? [y/N] ",
        destination.display()
    )?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;

    Ok(matches!(answer.trim(), "y" | "Y"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Contest, Problem};
    use crate::ui::NullReporter;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn save_contest(destination: &Path, contest_id: &str) {
        workspace::save_metadata(
            destination,
            &Contest {
                contest_id: contest_id.to_string(),
                problems: vec![Problem {
                    index: "A".to_string(),
                    title: "Problem A".to_string(),
                    task_id: format!("{contest_id}_a"),
                    url: format!("https://atcoder.jp/contests/{contest_id}/tasks/{contest_id}_a"),
                    sample_count: 0,
                }],
            },
        )
        .unwrap();
    }

    fn abc466_snapshot() -> FetchedContestData {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let client = atcoder::AtCoderClient::fixture(fixtures);
        let mut reporter = NullReporter;
        fetch_contest_data("abc466", &client, &mut reporter).unwrap()
    }

    fn save_snapshot_tests(destination: &Path, fetched: &FetchedContestData) {
        for (problem, samples) in fetched
            .contest
            .problems
            .iter()
            .zip(&fetched.samples_by_problem)
        {
            workspace::save_samples(destination, problem, samples).unwrap();
        }
    }

    #[test]
    fn target_classification_matches_cli_repair_and_hard_error_boundaries() {
        let missing = tempfile::tempdir().unwrap();
        assert!(matches!(
            inspect_contest_target(missing.path(), "abc466").unwrap(),
            ContestTargetHealth::RepairRequired
        ));

        let invalid = tempfile::tempdir().unwrap();
        std::fs::create_dir(invalid.path().join(".atc")).unwrap();
        std::fs::write(invalid.path().join(".atc/contest.toml"), "invalid").unwrap();
        assert!(matches!(
            inspect_contest_target(invalid.path(), "abc466").unwrap(),
            ContestTargetHealth::RepairRequired
        ));

        let old_schema = tempfile::tempdir().unwrap();
        std::fs::create_dir(old_schema.path().join(".atc")).unwrap();
        std::fs::write(
            old_schema.path().join(".atc/contest.toml"),
            concat!(
                "version = 1\n",
                "contest_id = \"abc466\"\n",
                "[[problems]]\n",
                "index = \"A\"\n",
                "title = \"A\"\n",
                "task_id = \"abc466_a\"\n",
                "url = \"https://example.invalid/a\"\n",
            ),
        )
        .unwrap();
        assert!(matches!(
            inspect_contest_target(old_schema.path(), "abc466").unwrap(),
            ContestTargetHealth::RepairRequired
        ));

        let empty_manifest = tempfile::tempdir().unwrap();
        std::fs::create_dir(empty_manifest.path().join(".atc")).unwrap();
        std::fs::write(
            empty_manifest.path().join(".atc/contest.toml"),
            "version = 1\ncontest_id = \"abc466\"\nproblems = []\n",
        )
        .unwrap();
        assert!(matches!(
            inspect_contest_target(empty_manifest.path(), "abc466").unwrap(),
            ContestTargetHealth::RepairRequired
        ));

        let mismatched_task_url = tempfile::tempdir().unwrap();
        std::fs::create_dir(mismatched_task_url.path().join(".atc")).unwrap();
        std::fs::write(
            mismatched_task_url.path().join(".atc/contest.toml"),
            concat!(
                "version = 1\n",
                "contest_id = \"abc466\"\n",
                "[[problems]]\n",
                "index = \"A\"\n",
                "title = \"Problem A\"\n",
                "task_id = \"abc466_a\"\n",
                "url = \"https://atcoder.jp/contests/abc466/tasks/abc466_b\"\n",
                "sample_count = 3\n",
            ),
        )
        .unwrap();
        assert!(matches!(
            inspect_contest_target(mismatched_task_url.path(), "abc466").unwrap(),
            ContestTargetHealth::RepairRequired
        ));

        let unsupported = tempfile::tempdir().unwrap();
        std::fs::create_dir(unsupported.path().join(".atc")).unwrap();
        std::fs::write(
            unsupported.path().join(".atc/contest.toml"),
            "version = 99\ncontest_id = \"abc466\"\nproblems = []\n",
        )
        .unwrap();
        assert!(matches!(
            inspect_contest_target(unsupported.path(), "abc466").unwrap(),
            ContestTargetHealth::UnsupportedVersion(99)
        ));

        let mismatch = tempfile::tempdir().unwrap();
        save_contest(mismatch.path(), "arc001");
        let Err(error) = inspect_contest_target(mismatch.path(), "abc466") else {
            panic!("identity mismatch must be a hard error");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let unsafe_problem = tempfile::tempdir().unwrap();
        std::fs::create_dir(unsafe_problem.path().join(".atc")).unwrap();
        std::fs::write(
            unsafe_problem.path().join(".atc/contest.toml"),
            concat!(
                "version = 1\n",
                "contest_id = \"abc466\"\n",
                "[[problems]]\n",
                "index = \"../A\"\n",
                "title = \"unsafe\"\n",
                "task_id = \"abc466_a\"\n",
                "url = \"https://example.invalid/a\"\n",
                "sample_count = 0\n",
            ),
        )
        .unwrap();
        let Err(error) = inspect_contest_target(unsafe_problem.path(), "abc466") else {
            panic!("unsafe problem path must be a hard error");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let broken_tests = tempfile::tempdir().unwrap();
        workspace::save_metadata(
            broken_tests.path(),
            &Contest {
                contest_id: "abc466".to_string(),
                problems: vec![crate::model::Problem {
                    index: "A".to_string(),
                    title: "A".to_string(),
                    task_id: "abc466_a".to_string(),
                    url: "https://atcoder.jp/contests/abc466/tasks/abc466_a".to_string(),
                    sample_count: 1,
                }],
            },
        )
        .unwrap();
        assert!(matches!(
            inspect_contest_target(broken_tests.path(), "abc466").unwrap(),
            ContestTargetHealth::RepairRequired
        ));
    }

    #[test]
    fn healthy_workflow_watches_without_create_or_repair() {
        let temp = tempfile::tempdir().unwrap();
        save_contest(temp.path(), "abc466");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut reporter = NullReporter;

        contest_at(
            temp.path(),
            "abc466",
            &mut reporter,
            |_| panic!("healthy metadata must not prompt"),
            |_, _, _| panic!("healthy contest must not be created"),
            |_, _, _| panic!("healthy contest must not be repaired"),
            {
                let calls = Rc::clone(&calls);
                move |destination, id, _| {
                    calls
                        .borrow_mut()
                        .push(format!("watch:{}:{id}", destination.display()));
                    Ok(())
                }
            },
        )
        .unwrap();

        assert_eq!(
            calls.borrow().as_slice(),
            [format!("watch:{}:abc466", temp.path().display())]
        );
    }

    #[test]
    fn repair_with_healthy_metadata_restores_only_tests() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).unwrap();
        let fetched = abc466_snapshot();
        workspace::save_metadata(&destination, &fetched.contest).unwrap();
        let metadata_before = std::fs::read(destination.join(".atc/contest.toml")).unwrap();
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let client = atcoder::AtCoderClient::fixture(fixtures);
        let mut reporter = NullReporter;

        repair_at(&destination, "abc466", &client, &mut reporter).unwrap();

        assert_eq!(
            std::fs::read(destination.join(".atc/contest.toml")).unwrap(),
            metadata_before
        );
        assert_eq!(
            workspace::inspect_tests_health(&destination, &fetched.contest).unwrap(),
            TestsHealth::Healthy
        );
        assert!(!destination.join("tests/C").exists());
    }

    #[test]
    fn repair_treats_safe_task_url_mismatch_as_invalid_and_rebuilds_from_remote_outline() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        std::fs::create_dir_all(destination.join(".atc")).unwrap();
        std::fs::write(
            destination.join(".atc/contest.toml"),
            concat!(
                "version = 1\n",
                "contest_id = \"abc466\"\n",
                "[[problems]]\n",
                "index = \"A\"\n",
                "title = \"Compromise\"\n",
                "task_id = \"abc466_a\"\n",
                "url = \"https://atcoder.jp/contests/abc466/tasks/abc466_e\"\n",
                "sample_count = 3\n",
            ),
        )
        .unwrap();
        let fetched = abc466_snapshot();
        assert_eq!(
            fetched.contest.problems[0].sample_count, fetched.contest.problems[4].sample_count,
            "the mismatched A/E task URLs must have the same remote count for this regression"
        );
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let client = atcoder::AtCoderClient::fixture(fixtures);
        let mut reporter = NullReporter;

        repair_at(&destination, "abc466", &client, &mut reporter).unwrap();

        assert_eq!(
            workspace::load_metadata(&destination).unwrap(),
            fetched.contest
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("tests/A/sample-1.in")).unwrap(),
            fetched.samples_by_problem[0][0].input
        );
    }

    #[test]
    fn zero_count_tests_only_repair_uses_local_manifest_without_remote_fetch() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("mini");
        std::fs::create_dir(&destination).unwrap();
        let contest = Contest {
            contest_id: "mini".to_string(),
            problems: vec![Problem {
                index: "A".to_string(),
                title: "Problem A".to_string(),
                task_id: "mini_a".to_string(),
                url: "https://atcoder.jp/contests/mini/tasks/mini_a".to_string(),
                sample_count: 0,
            }],
        };
        workspace::save_metadata(&destination, &contest).unwrap();
        std::fs::create_dir_all(destination.join("tests/A")).unwrap();
        std::fs::write(destination.join("tests/A/sample-1.in"), "local input\n").unwrap();
        std::fs::write(destination.join("tests/A/sample-1.out"), "local output\n").unwrap();
        let client = atcoder::AtCoderClient::fixture(temp.path().join("no-fixtures"));
        let mut reporter = NullReporter;

        repair_at(&destination, "mini", &client, &mut reporter).unwrap();

        assert_eq!(workspace::load_metadata(&destination).unwrap(), contest);
        assert!(!destination.join("tests").exists());
    }

    #[test]
    fn repair_with_broken_metadata_and_healthy_tests_restores_only_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).unwrap();
        let fetched = abc466_snapshot();
        save_snapshot_tests(&destination, &fetched);
        let edited_sample = destination.join("tests/A/sample-1.in");
        std::fs::write(&edited_sample, "local structural edit\n").unwrap();
        std::fs::write(destination.join("tests/memo.txt"), "unrelated").unwrap();
        std::fs::create_dir(destination.join(".atc")).unwrap();
        std::fs::write(destination.join(".atc/contest.toml"), "invalid metadata").unwrap();
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let client = atcoder::AtCoderClient::fixture(fixtures);
        let mut reporter = NullReporter;

        repair_at(&destination, "abc466", &client, &mut reporter).unwrap();

        assert_eq!(
            std::fs::read_to_string(edited_sample).unwrap(),
            "local structural edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("tests/memo.txt")).unwrap(),
            "unrelated"
        );
        assert_eq!(
            workspace::load_metadata(&destination).unwrap(),
            fetched.contest
        );
    }

    #[test]
    fn repair_with_broken_metadata_and_missing_tests_restores_both() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).unwrap();
        std::fs::create_dir(destination.join(".atc")).unwrap();
        std::fs::write(destination.join(".atc/contest.toml"), "invalid metadata").unwrap();
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let client = atcoder::AtCoderClient::fixture(fixtures);
        let mut reporter = NullReporter;

        repair_at(&destination, "abc466", &client, &mut reporter).unwrap();

        let contest = workspace::load_metadata(&destination).unwrap();
        assert_eq!(
            workspace::inspect_tests_health(&destination, &contest).unwrap(),
            TestsHealth::Healthy
        );
        assert!(destination.join("tests/A/sample-3.out").is_file());
        assert!(!destination.join("tests/C").exists());
    }

    #[test]
    fn repair_refuses_unmanaged_broken_tests_before_metadata_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        std::fs::create_dir(&destination).unwrap();
        std::fs::create_dir(destination.join(".atc")).unwrap();
        let metadata = destination.join(".atc/contest.toml");
        std::fs::write(&metadata, "invalid metadata").unwrap();
        std::fs::create_dir_all(destination.join("tests/A")).unwrap();
        let memo = destination.join("tests/A/memo.txt");
        std::fs::write(&memo, "keep me").unwrap();
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let client = atcoder::AtCoderClient::fixture(fixtures);
        let mut reporter = NullReporter;

        let error = repair_at(&destination, "abc466", &client, &mut reporter)
            .expect_err("unmanaged tests content must refuse repair before mutation");

        assert!(error.to_string().contains("memo.txt"));
        assert_eq!(
            std::fs::read_to_string(metadata).unwrap(),
            "invalid metadata"
        );
        assert_eq!(std::fs::read_to_string(memo).unwrap(), "keep me");
    }

    #[test]
    fn tests_only_repair_refuses_remote_count_change_and_preserves_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("mini");
        std::fs::create_dir(&destination).unwrap();
        let contest = Contest {
            contest_id: "mini".to_string(),
            problems: vec![crate::model::Problem {
                index: "A".to_string(),
                title: "A".to_string(),
                task_id: "mini_a".to_string(),
                url: "https://atcoder.jp/contests/mini/tasks/mini_a".to_string(),
                sample_count: 2,
            }],
        };
        workspace::save_metadata(&destination, &contest).unwrap();
        let metadata_before = std::fs::read(destination.join(".atc/contest.toml")).unwrap();
        let fixtures = temp.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("problems")).unwrap();
        std::fs::write(
            fixtures.join("problems/mini_a.html"),
            r#"<div id="task-statement"><span class="lang-en">
                <div class="part"><section><h3>Sample Input 1</h3><pre>1</pre></section></div>
                <div class="part"><section><h3>Sample Output 1</h3><pre>2</pre></section></div>
            </span></div>"#,
        )
        .unwrap();
        let client = atcoder::AtCoderClient::fixture(fixtures);
        let mut reporter = NullReporter;

        let error = repair_at(&destination, "mini", &client, &mut reporter)
            .expect_err("repair must not update an authoritative local count");

        assert!(error.to_string().contains("`atc refresh`"));
        assert_eq!(
            std::fs::read(destination.join(".atc/contest.toml")).unwrap(),
            metadata_before
        );
        assert!(!destination.join("tests").exists());
    }

    #[test]
    fn repair_revalidates_metadata_and_tests_after_fetch_before_install() {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");

        let tests_only_root = tempfile::tempdir().unwrap();
        let tests_only = tests_only_root.path().join("abc466");
        std::fs::create_dir(&tests_only).unwrap();
        let fetched = abc466_snapshot();
        workspace::save_metadata(&tests_only, &fetched.contest).unwrap();
        let client = atcoder::AtCoderClient::fixture(&fixtures);
        let mut reporter = NullReporter;
        let mut changed = fetched.contest.clone();
        changed.problems[0].title = "changed while fetching".to_string();
        let error =
            repair_at_with_before_install(&tests_only, "abc466", &client, &mut reporter, || {
                workspace::save_metadata(&tests_only, &changed).unwrap()
            })
            .expect_err("changed live metadata must abort tests-only repair");
        assert!(error.to_string().contains("retry repair"));
        assert!(!tests_only.join("tests").exists());

        let metadata_only_root = tempfile::tempdir().unwrap();
        let metadata_only = metadata_only_root.path().join("abc466");
        std::fs::create_dir(&metadata_only).unwrap();
        save_snapshot_tests(&metadata_only, &fetched);
        std::fs::create_dir(metadata_only.join(".atc")).unwrap();
        let invalid_metadata = metadata_only.join(".atc/contest.toml");
        std::fs::write(&invalid_metadata, "invalid metadata").unwrap();
        let removed_sample = metadata_only.join("tests/A/sample-1.in");
        let client = atcoder::AtCoderClient::fixture(fixtures);
        let error =
            repair_at_with_before_install(&metadata_only, "abc466", &client, &mut reporter, || {
                std::fs::remove_file(&removed_sample).unwrap()
            })
            .expect_err("tests changing from healthy to broken must abort metadata-only repair");
        assert!(error.to_string().contains("retry repair"));
        assert_eq!(
            std::fs::read_to_string(invalid_metadata).unwrap(),
            "invalid metadata"
        );
    }

    #[test]
    fn missing_or_invalid_metadata_repairs_only_after_confirmation() {
        for (answer, expected) in [(false, Vec::<&str>::new()), (true, vec!["repair", "watch"])] {
            let temp = tempfile::tempdir().unwrap();
            if answer {
                std::fs::create_dir(temp.path().join(".atc")).unwrap();
                std::fs::write(
                    temp.path().join(".atc").join("contest.toml"),
                    "version = ???",
                )
                .unwrap();
            }
            let calls = Rc::new(RefCell::new(Vec::new()));
            let mut reporter = NullReporter;

            contest_at(
                temp.path(),
                "abc466",
                &mut reporter,
                move |_| Ok(answer),
                |_, _, _| panic!("existing directory must not be created"),
                {
                    let calls = Rc::clone(&calls);
                    move |destination, id, _| {
                        calls.borrow_mut().push("repair");
                        save_contest(destination, id);
                        Ok(())
                    }
                },
                {
                    let calls = Rc::clone(&calls);
                    move |_, _, _| {
                        calls.borrow_mut().push("watch");
                        Ok(())
                    }
                },
            )
            .unwrap();

            assert_eq!(calls.borrow().as_slice(), expected);
        }
    }

    #[test]
    fn missing_destination_is_created_at_the_resolved_path_then_watched() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("mapped").join("abc466");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut reporter = NullReporter;

        contest_at(
            &destination,
            "abc466",
            &mut reporter,
            |_| panic!("a missing destination must not prompt for repair"),
            {
                let calls = Rc::clone(&calls);
                move |destination, id, _| {
                    calls.borrow_mut().push("create");
                    workspace::ensure_contest_parent(destination)?;
                    std::fs::create_dir(destination)?;
                    save_contest(destination, id);
                    Ok(())
                }
            },
            |_, _, _| panic!("a missing destination must not be repaired"),
            {
                let calls = Rc::clone(&calls);
                move |destination, _, _| {
                    assert!(destination.is_dir());
                    calls.borrow_mut().push("watch");
                    Ok(())
                }
            },
        )
        .unwrap();

        assert_eq!(calls.borrow().as_slice(), ["create", "watch"]);
    }

    #[test]
    fn missing_contest_creation_uses_the_source_template_resolver() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        let templates_dir = temp.path().join("templates");
        std::fs::create_dir(&templates_dir).unwrap();
        std::fs::write(templates_dir.join("cpp.cpp"), "// contest custom\n").unwrap();
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let mut reporter = NullReporter;

        create_contest_with(
            temp.path(),
            &destination,
            "abc466",
            &mut reporter,
            || Ok(Config::default()),
            |language| crate::template::resolve_source_template_in(&templates_dir, language),
            || Ok(atcoder::AtCoderClient::fixture(&fixtures)),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(destination.join("A.cpp")).unwrap(),
            "// contest custom\n"
        );
    }

    #[test]
    fn mismatch_and_newer_metadata_are_hard_errors_without_repair_or_watch() {
        let mismatch = tempfile::tempdir().unwrap();
        save_contest(mismatch.path(), "arc001");
        let mut reporter = NullReporter;
        let result = contest_at(
            mismatch.path(),
            "abc466",
            &mut reporter,
            |_| panic!("mismatch must not prompt"),
            |_, _, _| panic!("mismatch must not create"),
            |_, _, _| panic!("mismatch must not repair"),
            |_, _, _| panic!("mismatch must not watch"),
        );
        assert!(result.is_err());

        let newer = tempfile::tempdir().unwrap();
        std::fs::create_dir(newer.path().join(".atc")).unwrap();
        std::fs::write(
            newer.path().join(".atc").join("contest.toml"),
            "version = 99\ncontest_id = \"abc466\"\nproblems = []\n",
        )
        .unwrap();
        let result = contest_at(
            newer.path(),
            "abc466",
            &mut reporter,
            |_| panic!("newer metadata must not prompt"),
            |_, _, _| panic!("newer metadata must not create"),
            |_, _, _| panic!("newer metadata must not repair"),
            |_, _, _| panic!("newer metadata must not watch"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn repair_prompt_defaults_to_no_on_eof() {
        let temp = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        assert!(!confirm_repair_with(temp.path(), &mut input, &mut output).unwrap());
        assert!(String::from_utf8(output).unwrap().contains("[y/N]"));
    }
}
