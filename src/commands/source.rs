use crate::cli::SourceArgs;
use crate::error::AppError;
use crate::language::Language;
use crate::{safe_file, workspace};
use cap_std::fs::Dir;
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_SOURCE_BYTES: u64 = 1024 * 1024;

#[derive(Serialize)]
struct SourceResponse<'a> {
    schema_version: u32,
    contest_id: &'a str,
    index: &'a str,
    task_id: &'a str,
    url: &'a str,
    language: &'static str,
    source_path: &'a str,
    source: &'a str,
}

pub(crate) fn source(args: SourceArgs) -> Result<(), AppError> {
    if let Some(index) = &args.index {
        workspace::validate_problem_index(index)?;
    }
    if let Some(task_id) = &args.task_id
        && !workspace::is_safe_atcoder_id(task_id)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid task ID: {task_id:?}"),
        )
        .into());
    }
    let root = match args.workspace {
        Some(path) if path.is_absolute() => path,
        Some(path) => std::path::absolute(path)?,
        None => std::env::current_dir()?,
    };
    let (destination, directory, contest) = workspace::open_local_contest(&root, &args.contest_id)?;
    let problem = if let Some(task_id) = args.task_id {
        contest
            .problems
            .iter()
            .find(|problem| problem.task_id == task_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "task ID {task_id:?} not found in contest {:?}",
                        contest.contest_id
                    ),
                )
            })?
    } else {
        super::test::find_problem(
            &contest,
            args.index
                .as_deref()
                .expect("clap requires a problem selector"),
        )?
    };

    let (language, path) = select_source(&directory, &destination, &problem.index, args.language)?;
    let source_path = path.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "source path is not valid UTF-8")
    })?;
    let name = path.file_name().expect("source path has a file name");
    let source =
        safe_file::read_bounded_regular_text_at(&directory, name, &path, MAX_SOURCE_BYTES)?;
    let response = SourceResponse {
        schema_version: 1,
        contest_id: &contest.contest_id,
        index: &problem.index,
        task_id: &problem.task_id,
        url: &problem.url,
        language: match language {
            Language::Cpp => "cpp",
            Language::Python => "python",
        },
        source_path,
        source: &source,
    };
    let mut output = serde_json::to_vec(&response).map_err(io::Error::other)?;
    output.push(b'\n');
    let mut stdout = io::stdout().lock();
    stdout.write_all(&output)?;
    stdout.flush()?;
    Ok(())
}

fn select_source(
    directory: &Dir,
    destination: &Path,
    index: &str,
    language: Option<Language>,
) -> io::Result<(Language, PathBuf)> {
    let languages = match &language {
        Some(language) => std::slice::from_ref(language),
        None => &Language::ALL,
    };
    let mut selected = None;
    for &language in languages {
        let path = workspace::source_file_path(destination, index, language)?;
        let name = path.file_name().expect("source path has a file name");
        if safe_file::regular_file_exists_at(directory, name, &path)? {
            if selected.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "multiple source files for problem {index}; select one with --language"
                    ),
                ));
            }
            selected = Some((language, path));
        }
    }
    selected.ok_or_else(|| {
        let detail = match language {
            Some(language) => format!("{index}.{}", language.extension()),
            None => format!("problem {index}"),
        };
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "source file not found for {detail} in {}",
                destination.display()
            ),
        )
    })
}
