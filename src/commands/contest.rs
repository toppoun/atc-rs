use super::resolve_language;
use crate::atcoder;
use crate::config::Config;
use crate::error::AppError;
use crate::ui::Reporter;
use crate::workspace::{self, ContestMetadataHealth};
use std::io::{self, BufRead, Write};
use std::path::Path;

pub(crate) fn contest(contest_id: &str, reporter: &mut dyn Reporter) -> Result<(), AppError> {
    let cwd = std::env::current_dir()?;

    let destination = workspace::resolve_contest_path(&cwd, contest_id)?;

    contest_at(
        &destination,
        contest_id,
        reporter,
        |destination| confirm_repair(destination).map_err(AppError::from),
        |destination, contest_id, reporter| create_contest(&cwd, destination, contest_id, reporter),
        repair_contest,
        |destination, contest_id, _| super::watch_tui::watch_tui_at(destination, Some(contest_id)),
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
    if !workspace::contest_directory_exists(destination)? {
        create(destination, contest_id, reporter)?;
    } else {
        match workspace::inspect_contest_metadata(destination)? {
            ContestMetadataHealth::Healthy(contest) => {
                workspace::validate_contest_identity(&contest, contest_id)?;
                workspace::validate_contest_paths(&contest)?;
            }

            ContestMetadataHealth::Missing | ContestMetadataHealth::Invalid => {
                if !confirm(destination)? {
                    return Ok(());
                }

                repair(destination, contest_id, reporter)?;
            }

            ContestMetadataHealth::UnsupportedVersion(version) => {
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
    }

    watch(destination, contest_id, reporter)
}

fn create_contest(
    root: &Path,
    destination: &Path,
    contest_id: &str,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let config = Config::load()?;
    let language = resolve_language(None, &config);
    let atcoder = create_atcoder_client()?;

    super::new::new_at_in_workspace(root, destination, contest_id, language, &atcoder, reporter)
}

fn repair_contest(
    destination: &Path,
    contest_id: &str,
    reporter: &mut dyn Reporter,
) -> Result<(), AppError> {
    let atcoder = create_atcoder_client()?;

    super::refresh::refresh_at(destination, contest_id, true, &atcoder, reporter)
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
    let metadata = destination.join(".atc").join("contest.toml");

    write!(
        output,
        "contest metadata is missing or invalid:\n{}\nRepair contest metadata and samples? [y/N] ",
        metadata.display()
    )?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;

    Ok(matches!(answer.trim(), "y" | "Y"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Contest;
    use crate::ui::NullReporter;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    fn save_contest(destination: &Path, contest_id: &str) {
        workspace::save_metadata(
            destination,
            &Contest {
                contest_id: contest_id.to_string(),
                problems: Vec::new(),
            },
        )
        .unwrap();
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
