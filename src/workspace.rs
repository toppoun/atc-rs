use crate::language::Language;
use crate::model::{Contest, Problem, Sample};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use tempfile::TempDir;

const METADATA_VERSION: u32 = 1;

const WORKSPACE_CONFIG_FILE: &str = ".atc-workspace.toml";
const WORKSPACE_CONFIG_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContestMetadata {
    version: u32,
    contest_id: String,
    problems: Vec<Problem>,
}

#[derive(Deserialize)]
struct ContestMetadataHeader {
    version: u32,
}

pub enum ContestMetadataHealth {
    Healthy(Contest),
    Missing,
    Invalid,
    UnsupportedVersion(u32),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConfig {
    version: u32,
    paths: Vec<WorkspacePathRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspacePathRule {
    pattern: String,
    path: String,
}

fn load_workspace_config(root: &Path) -> io::Result<Option<WorkspaceConfig>> {
    let path = root.join(WORKSPACE_CONFIG_FILE);

    if !existing_regular_file(&path, "workspace config")? {
        return Ok(None);
    }

    let content = fs::read_to_string(&path)?;
    let config: WorkspaceConfig = toml::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    if config.version != WORKSPACE_CONFIG_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported workspace config version: {} (expected {WORKSPACE_CONFIG_VERSION})",
                config.version
            ),
        ));
    }

    Ok(Some(config))
}

pub fn resolve_contest_path(root: &Path, contest_id: &str) -> io::Result<PathBuf> {
    validate_path_component(contest_id, "contest ID")?;

    let Some(config) = load_workspace_config(root)? else {
        return contest_path(root, contest_id);
    };

    // rulesを検証・match
    let mut matched_path: Option<&str> = None;

    for rule in &config.paths {
        validate_path_component(&rule.path, "workspace path").map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid workspace path {:?}: {error}", rule.path),
            )
        })?;

        let regex = Regex::new(&rule.pattern).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid workspace regex {:?}: {error}", rule.pattern),
            )
        })?;

        if !regex.is_match(contest_id) {
            continue;
        }

        if matched_path.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("multiple workspace path rules match contest ID {contest_id:?}"),
            ));
        }

        matched_path = Some(&rule.path);
    }
    let path = matched_path.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no workspace path rule matches contest ID {contest_id:?}"),
        )
    })?;

    contest_path(&root.join(path), contest_id)
}

pub fn resolve_contest_target(root: &Path, contest_id: Option<&str>) -> io::Result<PathBuf> {
    match contest_id {
        Some(contest_id) => resolve_contest_path(root, contest_id),
        None => Ok(root.to_path_buf()),
    }
}

pub fn contest_path(root: &Path, contest_id: &str) -> io::Result<PathBuf> {
    validate_path_component(contest_id, "contest ID")?;
    Ok(root.join(contest_id))
}

pub fn save_metadata(destination: &Path, contest: &Contest) -> io::Result<()> {
    validate_contest_paths(contest)?;

    let atc_dir = destination.join(".atc");

    fs::create_dir_all(&atc_dir)?;

    let metadata = ContestMetadata {
        version: METADATA_VERSION,
        contest_id: contest.contest_id.clone(),
        problems: contest.problems.clone(),
    };

    let content = toml::to_string_pretty(&metadata).map_err(io::Error::other)?;

    fs::write(atc_dir.join("contest.toml"), content)?;

    Ok(())
}

pub fn load_metadata(destination: &Path) -> io::Result<Contest> {
    validate_workspace_marker(destination)?;
    let path = destination.join(".atc").join("contest.toml");

    if !existing_regular_file(&path, "contest metadata")? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("contest metadata not found: {}", path.display()),
        ));
    }

    let content = fs::read_to_string(&path)?;

    let metadata: ContestMetadata = toml::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    if metadata.version != METADATA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported contest metadata version: {} (expected {METADATA_VERSION})",
                metadata.version
            ),
        ));
    }

    Ok(Contest {
        contest_id: metadata.contest_id,
        problems: metadata.problems,
    })
}

pub fn contest_directory_exists(destination: &Path) -> io::Result<bool> {
    existing_real_directory(destination, "contest directory")
}

pub fn ensure_contest_parent(destination: &Path) -> io::Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "contest destination has no parent directory: {}",
                destination.display()
            ),
        )
    })?;

    if existing_real_directory(parent, "contest parent directory")? {
        return Ok(());
    }

    match fs::create_dir(parent) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if existing_real_directory(parent, "contest parent directory")? {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "contest parent is not a real directory: {}",
                        parent.display()
                    ),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

pub fn inspect_contest_metadata(destination: &Path) -> io::Result<ContestMetadataHealth> {
    validate_contest_directory(destination)?;
    let marker = destination.join(".atc");

    if !existing_real_directory(&marker, "workspace marker")? {
        return Ok(ContestMetadataHealth::Missing);
    }

    let path = marker.join("contest.toml");

    if !existing_regular_file(&path, "contest metadata")? {
        return Ok(ContestMetadataHealth::Missing);
    }

    let content = fs::read_to_string(&path)?;

    let header: ContestMetadataHeader = match toml::from_str(&content) {
        Ok(header) => header,
        Err(_) => {
            return Ok(ContestMetadataHealth::Invalid);
        }
    };

    if header.version != METADATA_VERSION {
        return Ok(ContestMetadataHealth::UnsupportedVersion(header.version));
    }

    let metadata: ContestMetadata = match toml::from_str(&content) {
        Ok(metadata) => metadata,
        Err(_) => {
            return Ok(ContestMetadataHealth::Invalid);
        }
    };

    Ok(ContestMetadataHealth::Healthy(Contest {
        contest_id: metadata.contest_id,
        problems: metadata.problems,
    }))
}

pub fn save_samples(destination: &Path, problem: &Problem, samples: &[Sample]) -> io::Result<()> {
    if samples.is_empty() {
        return Ok(());
    }

    validate_path_component(&problem.index, "problem index")?;

    let test_dir = destination.join("tests").join(&problem.index);

    fs::create_dir_all(&test_dir)?;

    for (i, sample) in samples.iter().enumerate() {
        let number = i + 1;

        fs::write(test_dir.join(format!("sample-{number}.in")), &sample.input)?;

        fs::write(
            test_dir.join(format!("sample-{number}.out")),
            &sample.output,
        )?;
    }

    Ok(())
}

#[derive(Debug)]
enum SampleFileKind {
    Input,
    Output,
}

#[derive(Debug, Default)]
struct SampleFiles {
    input: Option<PathBuf>,
    output: Option<PathBuf>,
}

fn parse_sample_filename(name: &OsStr) -> io::Result<Option<(usize, SampleFileKind)>> {
    let Some(name) = name.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sample directory contains a non-UTF-8 file name",
        ));
    };

    let Some(rest) = name.strip_prefix("sample-") else {
        return Ok(None);
    };

    let Some((number, extension)) = rest.rsplit_once('.') else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid sample file name: {name}"),
        ));
    };

    let number = number.parse::<usize>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid sample number in file name: {name}"),
        )
    })?;

    if number == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sample number must start from 1: {name}"),
        ));
    }

    let kind = match extension {
        "in" => SampleFileKind::Input,
        "out" => SampleFileKind::Output,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid sample file extension: {name}"),
            ));
        }
    };

    Ok(Some((number, kind)))
}

pub fn load_samples(destination: &Path, problem_index: &str) -> io::Result<Vec<Sample>> {
    validate_path_component(problem_index, "problem index")?;

    let tests_dir = destination.join("tests");
    if !existing_real_directory(&tests_dir, "tests directory")? {
        return Ok(Vec::new());
    }

    let test_dir = tests_dir.join(problem_index);
    if !existing_real_directory(&test_dir, "problem tests directory")? {
        return Ok(Vec::new());
    }

    let mut files = BTreeMap::<usize, SampleFiles>::new();

    for entry in fs::read_dir(&test_dir)? {
        let entry = entry?;
        let path = entry.path();

        let Some((number, kind)) = parse_sample_filename(&entry.file_name())? else {
            continue;
        };

        if !existing_regular_file(&path, "sample file")? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sample file not found: {}", path.display()),
            ));
        }

        let sample = files.entry(number).or_default();

        match kind {
            SampleFileKind::Input => {
                if sample.input.replace(path).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("duplicate sample input for sample {number}"),
                    ));
                }
            }

            SampleFileKind::Output => {
                if sample.output.replace(path).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("duplicate sample output for sample {number}"),
                    ));
                }
            }
        }
    }

    let mut samples = Vec::with_capacity(files.len());

    for (expected_number, (number, sample_files)) in (1usize..).zip(files) {
        if number != expected_number {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "sample numbers must be consecutive from 1: expected {expected_number}, found {number}"
                ),
            ));
        }

        let input_path = sample_files.input.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sample-{number}.in is missing"),
            )
        })?;

        let output_path = sample_files.output.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sample-{number}.out is missing"),
            )
        })?;

        let input = fs::read_to_string(&input_path)?;
        let output = fs::read_to_string(&output_path)?;

        samples.push(Sample { input, output });
    }

    Ok(samples)
}

pub fn create_source_file(
    destination: &Path,
    name: &str,
    language: Language,
    template: &str,
) -> io::Result<PathBuf> {
    validate_path_component(name, "source name")?;

    let path = destination.join(format!("{}.{}", name, language.extension()));

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;

    file.write_all(template.as_bytes())?;

    Ok(path)
}

pub fn create_source_files(
    destination: &Path,
    problems: &[Problem],
    language: Language,
    template: &str,
) -> io::Result<()> {
    for problem in problems {
        match create_source_file(destination, &problem.index, language, template) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

fn validate_path_component(value: &str, kind: &str) -> io::Result<()> {
    let contains_path_separator = value.contains('/') || value.contains('\\');

    let mut components = Path::new(value).components();
    let is_single_normal_component = matches!(
        components.next(),
        Some(Component::Normal(component)) if component == OsStr::new(value)
    ) && components.next().is_none();

    if is_single_normal_component
        && !contains_path_separator
        && is_safe_platform_path_component(value)
    {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid {kind} for a file name: {value:?}"),
    ))
}

fn is_safe_platform_path_component(value: &str) -> bool {
    #[cfg(not(windows))]
    {
        let _ = value;
        true
    }

    #[cfg(windows)]
    {
        if value.ends_with([' ', '.'])
            || value
                .chars()
                .any(|character| character < '\u{20}' || r#"<>:"|?*"#.contains(character))
        {
            return false;
        }

        let stem = value
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
            && !matches!(
                stem.as_str(),
                "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            )
            && !matches!(
                stem.as_str(),
                "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
            )
    }
}

pub fn validate_contest_paths(contest: &Contest) -> io::Result<()> {
    validate_path_component(&contest.contest_id, "contest ID")?;
    let mut problem_indices = HashSet::new();
    for problem in &contest.problems {
        validate_problem_index(&problem.index)?;
        if !problem_indices.insert(problem.index.to_ascii_lowercase()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "duplicate problem index ignoring ASCII case: {:?}",
                    problem.index
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_problem_index(problem_index: &str) -> io::Result<()> {
    validate_path_component(problem_index, "problem index")
}

pub fn validate_contest_identity(contest: &Contest, expected_contest_id: &str) -> io::Result<()> {
    validate_path_component(expected_contest_id, "contest ID")?;

    if contest.contest_id == expected_contest_id {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "contest ID mismatch: requested {expected_contest_id:?}, but metadata contains {:?}",
            contest.contest_id
        ),
    ))
}

fn validate_contest_directory(destination: &Path) -> io::Result<()> {
    if existing_real_directory(destination, "contest directory")? {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("contest directory not found: {}", destination.display()),
    ))
}

pub fn validate_workspace_marker(destination: &Path) -> io::Result<()> {
    validate_contest_directory(destination)?;

    let marker = destination.join(".atc");

    if existing_real_directory(&marker, "workspace marker")? {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("workspace marker not found: {}", marker.display()),
    ))
}

pub fn validate_refresh_destination(
    cwd: &Path,
    contest_id: &str,
    allow_missing_marker: bool,
) -> std::io::Result<()> {
    validate_path_component(contest_id, "contest ID")?;
    validate_contest_directory(cwd)?;

    let marker = cwd.join(".atc");
    if !existing_real_directory(&marker, "workspace marker")? && !allow_missing_marker {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("workspace marker not found: {}", marker.display()),
        ));
    }

    let directory_name = cwd.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "current directory has no directory name",
        )
    })?;

    if directory_name != std::ffi::OsStr::new(contest_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("current directory is not {contest_id}: {}", cwd.display()),
        ));
    }

    Ok(())
}

pub fn replace_refresh_data(
    destination: &Path,
    staging: TempDir,
    allow_missing_marker: bool,
) -> io::Result<()> {
    validate_contest_directory(destination)?;

    let staging_root = staging.path().to_path_buf();
    let destination_tests = destination.join("tests");
    let staged_tests = staging_root.join("tests");
    let backup_tests = staging_root.join("previous-tests");
    let destination_marker = destination.join(".atc");
    let destination_metadata = destination_marker.join("contest.toml");
    let staged_metadata = staging_root.join(".atc").join("contest.toml");
    let backup_metadata = staging_root.join("previous-contest.toml");

    let had_destination_marker = existing_real_directory(&destination_marker, "workspace marker")?;
    if !had_destination_marker && !allow_missing_marker {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "workspace marker not found: {}",
                destination_marker.display()
            ),
        ));
    }

    let had_destination_tests = existing_real_directory(&destination_tests, "existing tests path")?;
    let has_staged_tests = existing_real_directory(&staged_tests, "staged tests path")?;
    let had_destination_metadata = had_destination_marker
        && existing_regular_file(&destination_metadata, "existing metadata path")?;
    if !existing_regular_file(&staged_metadata, "staged metadata path")? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("staged metadata not found: {}", staged_metadata.display()),
        ));
    }

    if had_destination_tests {
        fs::rename(&destination_tests, &backup_tests)?;
    }

    if has_staged_tests && let Err(error) = fs::rename(&staged_tests, &destination_tests) {
        let rollback_errors = rollback_tests(
            &destination_tests,
            &staged_tests,
            &backup_tests,
            false,
            had_destination_tests,
        );
        return Err(refresh_update_error(staging, error, rollback_errors));
    }

    if had_destination_metadata
        && let Err(error) = fs::rename(&destination_metadata, &backup_metadata)
    {
        let rollback_errors = rollback_tests(
            &destination_tests,
            &staged_tests,
            &backup_tests,
            has_staged_tests,
            had_destination_tests,
        );
        return Err(refresh_update_error(staging, error, rollback_errors));
    }

    let created_destination_marker = if had_destination_marker {
        false
    } else if let Err(error) = fs::create_dir(&destination_marker) {
        let rollback_errors = rollback_tests(
            &destination_tests,
            &staged_tests,
            &backup_tests,
            has_staged_tests,
            had_destination_tests,
        );
        return Err(refresh_update_error(staging, error, rollback_errors));
    } else {
        true
    };

    if let Err(error) = fs::rename(&staged_metadata, &destination_metadata) {
        let mut rollback_errors = Vec::new();
        if had_destination_metadata
            && let Err(rollback_error) = fs::rename(&backup_metadata, &destination_metadata)
        {
            rollback_errors.push(format!(
                "failed to restore metadata {}: {rollback_error}",
                destination_metadata.display()
            ));
        }
        if created_destination_marker
            && let Err(rollback_error) = fs::remove_dir(&destination_marker)
        {
            rollback_errors.push(format!(
                "failed to remove new workspace marker {}: {rollback_error}",
                destination_marker.display()
            ));
        }
        rollback_errors.extend(rollback_tests(
            &destination_tests,
            &staged_tests,
            &backup_tests,
            has_staged_tests,
            had_destination_tests,
        ));
        return Err(refresh_update_error(staging, error, rollback_errors));
    }

    Ok(())
}

fn existing_real_directory(path: &Path, kind: &str) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a real directory: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn existing_regular_file(path: &Path, kind: &str) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a regular file: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn rollback_tests(
    destination_tests: &Path,
    staged_tests: &Path,
    backup_tests: &Path,
    new_tests_were_moved: bool,
    old_tests_were_moved: bool,
) -> Vec<String> {
    let mut errors = Vec::new();

    if new_tests_were_moved && let Err(error) = fs::rename(destination_tests, staged_tests) {
        errors.push(format!(
            "failed to move new tests out of {}: {error}",
            destination_tests.display()
        ));
    }
    if old_tests_were_moved && let Err(error) = fs::rename(backup_tests, destination_tests) {
        errors.push(format!(
            "failed to restore previous tests {}: {error}",
            destination_tests.display()
        ));
    }

    errors
}

fn refresh_update_error(
    staging: TempDir,
    original: io::Error,
    rollback_errors: Vec<String>,
) -> io::Error {
    if rollback_errors.is_empty() {
        return original;
    }

    let kind = original.kind();
    let recovery_path = staging.keep();
    io::Error::new(
        kind,
        format!(
            "refresh update failed: {original}; rollback also failed: {}; recovery data kept at {}",
            rollback_errors.join("; "),
            recovery_path.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_workspace_config(root: &Path, body: &str) {
        fs::write(root.join(WORKSPACE_CONFIG_FILE), body).unwrap();
    }

    fn write_metadata_text(destination: &Path, body: &str) {
        fs::create_dir_all(destination.join(".atc")).unwrap();
        fs::write(destination.join(".atc").join("contest.toml"), body).unwrap();
    }

    fn problem(index: &str) -> Problem {
        Problem {
            index: index.to_string(),
            title: format!("Problem {index}"),
            task_id: format!("abc466_{}", index.to_ascii_lowercase()),
            url: format!(
                "https://atcoder.jp/contests/abc466/tasks/abc466_{}",
                index.to_ascii_lowercase()
            ),
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

    #[test]
    fn contest_resolver_uses_only_the_explicit_root_config() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("cwd");
        fs::create_dir(&cwd).unwrap();

        assert_eq!(
            resolve_contest_path(&cwd, "abc466").unwrap(),
            cwd.join("abc466")
        );

        write_workspace_config(
            temp.path(),
            "version = 1\n[[paths]]\npattern = \"^abc\"\npath = \"parent-only\"\n",
        );
        assert_eq!(
            resolve_contest_path(&cwd, "abc466").unwrap(),
            cwd.join("abc466"),
            "the resolver must not search parent directories"
        );
    }

    #[test]
    fn contest_resolver_requires_exactly_one_authoritative_mapping() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_config(
            temp.path(),
            concat!(
                "version = 1\n",
                "[[paths]]\npattern = \"^abc\"\npath = \"abc\"\n",
                "[[paths]]\npattern = \"^arc\"\npath = \"arc\"\n",
            ),
        );
        assert_eq!(
            resolve_contest_path(temp.path(), "abc466").unwrap(),
            temp.path().join("abc").join("abc466")
        );
        assert!(resolve_contest_path(temp.path(), "agc001").is_err());

        write_workspace_config(
            temp.path(),
            concat!(
                "version = 1\n",
                "[[paths]]\npattern = \"abc\"\npath = \"first\"\n",
                "[[paths]]\npattern = \"^abc466$\"\npath = \"second\"\n",
            ),
        );
        assert!(resolve_contest_path(temp.path(), "abc466").is_err());
    }

    #[test]
    fn contest_resolver_rejects_invalid_rules_ids_and_versions() {
        let temp = tempfile::tempdir().unwrap();
        for id in ["..", "a/b", "a\\b"] {
            assert_eq!(
                resolve_contest_path(temp.path(), id).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        let absolute = temp.path().join("absolute").to_string_lossy().into_owned();
        assert_eq!(
            resolve_contest_path(temp.path(), &absolute)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );

        write_workspace_config(
            temp.path(),
            "version = 1\n[[paths]]\npattern = \"[\"\npath = \"abc\"\n",
        );
        assert_eq!(
            resolve_contest_path(temp.path(), "abc466")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        write_workspace_config(
            temp.path(),
            "version = 1\n[[paths]]\npattern = \".*\"\npath = \"../outside\"\n",
        );
        assert_eq!(
            resolve_contest_path(temp.path(), "abc466")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        write_workspace_config(temp.path(), "version = 2\npaths = []\n");
        assert_eq!(
            resolve_contest_path(temp.path(), "abc466")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(windows)]
    #[test]
    fn contest_resolver_rejects_windows_ads_ids() {
        let temp = tempfile::tempdir().unwrap();
        for id in ["abc466:stream", "CON", "nul.txt", "abc466.", "abc466?x"] {
            assert_eq!(
                resolve_contest_path(temp.path(), id).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "unsafe Windows component: {id:?}"
            );
        }
    }

    #[test]
    fn contest_resolver_rejects_non_file_workspace_config() {
        let directory_root = tempfile::tempdir().unwrap();
        fs::create_dir(directory_root.path().join(WORKSPACE_CONFIG_FILE)).unwrap();
        assert_eq!(
            resolve_contest_path(directory_root.path(), "abc466")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );

        let symlink_root = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        if !create_file_symlink(
            external.path(),
            &symlink_root.path().join(WORKSPACE_CONFIG_FILE),
        ) {
            return;
        }
        assert_eq!(
            resolve_contest_path(symlink_root.path(), "abc466")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn metadata_health_distinguishes_missing_invalid_unsupported_and_healthy() {
        let temp = tempfile::tempdir().unwrap();
        assert!(matches!(
            inspect_contest_metadata(temp.path()).unwrap(),
            ContestMetadataHealth::Missing
        ));

        fs::create_dir(temp.path().join(".atc")).unwrap();
        assert!(matches!(
            inspect_contest_metadata(temp.path()).unwrap(),
            ContestMetadataHealth::Missing
        ));

        for invalid in [
            "version = ???",
            "contest_id = \"abc466\"\nproblems = []\n",
            concat!(
                "version = 1\ncontest_id = \"abc466\"\nproblems = []\n",
                "source = \"A.py\"\ntests = \"tests/A\"\n"
            ),
        ] {
            write_metadata_text(temp.path(), invalid);
            assert!(matches!(
                inspect_contest_metadata(temp.path()).unwrap(),
                ContestMetadataHealth::Invalid
            ));
        }

        write_metadata_text(
            temp.path(),
            "version = 99\ncontest_id = \"abc466\"\nproblems = []\nunknown = true\n",
        );
        assert!(matches!(
            inspect_contest_metadata(temp.path()).unwrap(),
            ContestMetadataHealth::UnsupportedVersion(99)
        ));

        write_metadata_text(
            temp.path(),
            "version = 1\ncontest_id = \"abc466\"\nproblems = []\n",
        );
        assert!(matches!(
            inspect_contest_metadata(temp.path()).unwrap(),
            ContestMetadataHealth::Healthy(Contest { contest_id, problems })
                if contest_id == "abc466" && problems.is_empty()
        ));
    }

    #[test]
    fn metadata_health_rejects_symlinked_marker_and_metadata() {
        let marker_root = tempfile::tempdir().unwrap();
        let external_marker = tempfile::tempdir().unwrap();
        if !create_directory_symlink(external_marker.path(), &marker_root.path().join(".atc")) {
            return;
        }
        assert_eq!(
            inspect_contest_metadata(marker_root.path())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );

        let file_root = tempfile::tempdir().unwrap();
        let external_file = tempfile::NamedTempFile::new().unwrap();
        fs::create_dir(file_root.path().join(".atc")).unwrap();
        if !create_file_symlink(
            external_file.path(),
            &file_root.path().join(".atc").join("contest.toml"),
        ) {
            return;
        }
        assert_eq!(
            inspect_contest_metadata(file_root.path())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            load_metadata(file_root.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn contest_operations_reject_a_symlinked_destination_directory() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        save_metadata(
            external.path(),
            &Contest {
                contest_id: "abc466".to_string(),
                problems: Vec::new(),
            },
        )
        .unwrap();
        let destination = temp.path().join("abc466");
        if !create_directory_symlink(external.path(), &destination) {
            return;
        }

        for result in [
            validate_workspace_marker(&destination),
            validate_refresh_destination(&destination, "abc466", true),
            inspect_contest_metadata(&destination).map(|_| ()),
        ] {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn contest_parent_creation_rejects_a_symlinked_mapped_directory() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let mapped = temp.path().join("mapped");
        if !create_directory_symlink(external.path(), &mapped) {
            return;
        }

        let error = ensure_contest_parent(&mapped.join("abc466"))
            .expect_err("a mapped symlink must not be used as a staging parent");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!external.path().join("abc466").exists());
    }

    #[test]
    fn save_and_load_metadata() {
        let temp = tempfile::tempdir().unwrap();

        let contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![
                Problem {
                    index: "A".to_string(),
                    title: "Compromise".to_string(),
                    task_id: "abc466_a".to_string(),
                    url: "https://atcoder.jp/contests/abc466/tasks/abc466_a".to_string(),
                },
                Problem {
                    index: "B".to_string(),
                    title: "Representative Balls".to_string(),
                    task_id: "abc466_b".to_string(),
                    url: "https://atcoder.jp/contests/abc466/tasks/abc466_b".to_string(),
                },
            ],
        };

        save_metadata(temp.path(), &contest).unwrap();

        let loaded = load_metadata(temp.path()).unwrap();

        assert_eq!(loaded, contest);
    }

    #[test]
    fn contest_paths_reject_case_insensitive_problem_index_collisions() {
        let contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A"), problem("a")],
        };

        let error = validate_contest_paths(&contest).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("duplicate problem index"));
    }

    #[test]
    fn saves_samples_with_expected_names_and_contents() {
        let temp = tempfile::tempdir().unwrap();
        let samples = vec![
            Sample {
                input: "1 2\n".to_string(),
                output: "3\n".to_string(),
            },
            Sample {
                input: "4 5\n".to_string(),
                output: "9\n".to_string(),
            },
        ];

        save_samples(temp.path(), &problem("A"), &samples).unwrap();

        let test_dir = temp.path().join("tests").join("A");
        assert_eq!(
            fs::read_to_string(test_dir.join("sample-1.in")).unwrap(),
            "1 2\n"
        );
        assert_eq!(
            fs::read_to_string(test_dir.join("sample-1.out")).unwrap(),
            "3\n"
        );
        assert_eq!(
            fs::read_to_string(test_dir.join("sample-2.in")).unwrap(),
            "4 5\n"
        );
        assert_eq!(
            fs::read_to_string(test_dir.join("sample-2.out")).unwrap(),
            "9\n"
        );
    }

    #[test]
    fn empty_samples_do_not_create_tests_directory() {
        let temp = tempfile::tempdir().unwrap();

        save_samples(temp.path(), &problem("A"), &[]).unwrap();

        assert!(!temp.path().join("tests").exists());
    }

    #[test]
    fn loads_samples_in_numeric_order() {
        let temp = tempfile::tempdir().unwrap();
        let test_dir = temp.path().join("tests").join("A");
        fs::create_dir_all(&test_dir).unwrap();
        fs::write(test_dir.join("sample-2.out"), "second output\n").unwrap();
        fs::write(test_dir.join("sample-1.in"), "first input\n").unwrap();
        fs::write(test_dir.join("sample-2.in"), "second input\n").unwrap();
        fs::write(test_dir.join("sample-1.out"), "first output\n").unwrap();
        fs::write(test_dir.join("README.txt"), "ignored").unwrap();

        let samples = load_samples(temp.path(), "A").unwrap();

        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].input, "first input\n");
        assert_eq!(samples[0].output, "first output\n");
        assert_eq!(samples[1].input, "second input\n");
        assert_eq!(samples[1].output, "second output\n");
    }

    #[test]
    fn missing_or_empty_problem_tests_returns_no_samples() {
        let temp = tempfile::tempdir().unwrap();

        assert!(load_samples(temp.path(), "A").unwrap().is_empty());

        fs::create_dir_all(temp.path().join("tests").join("A")).unwrap();
        assert!(load_samples(temp.path(), "A").unwrap().is_empty());
    }

    #[test]
    fn load_samples_rejects_unsafe_problem_index_and_non_directories() {
        let temp = tempfile::tempdir().unwrap();

        let error = load_samples(temp.path(), "../outside").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        fs::write(temp.path().join("tests"), "not a directory").unwrap();
        let error = load_samples(temp.path(), "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let second = tempfile::tempdir().unwrap();
        fs::create_dir(second.path().join("tests")).unwrap();
        fs::write(second.path().join("tests").join("A"), "not a directory").unwrap();
        let error = load_samples(second.path(), "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_missing_sample_pair_and_number_gap() {
        let temp = tempfile::tempdir().unwrap();
        let missing_pair = temp.path().join("tests").join("A");
        fs::create_dir_all(&missing_pair).unwrap();
        fs::write(missing_pair.join("sample-1.in"), "input").unwrap();

        let error = load_samples(temp.path(), "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("sample-1.out is missing"));

        let gap = temp.path().join("tests").join("B");
        fs::create_dir_all(&gap).unwrap();
        for number in [1, 3] {
            fs::write(gap.join(format!("sample-{number}.in")), "input").unwrap();
            fs::write(gap.join(format!("sample-{number}.out")), "output").unwrap();
        }

        let error = load_samples(temp.path(), "B").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("expected 2, found 3"));
    }

    #[test]
    fn rejects_duplicate_and_invalid_sample_names() {
        let temp = tempfile::tempdir().unwrap();
        let duplicate = temp.path().join("tests").join("A");
        fs::create_dir_all(&duplicate).unwrap();
        fs::write(duplicate.join("sample-1.in"), "input").unwrap();
        fs::write(duplicate.join("sample-01.in"), "duplicate").unwrap();
        fs::write(duplicate.join("sample-1.out"), "output").unwrap();

        let error = load_samples(temp.path(), "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("duplicate sample input"));

        let invalid = temp.path().join("tests").join("B");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join("sample-0.in"), "input").unwrap();

        let error = load_samples(temp.path(), "B").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_directory_used_as_sample_file() {
        let temp = tempfile::tempdir().unwrap();
        let test_dir = temp.path().join("tests").join("A");
        fs::create_dir_all(test_dir.join("sample-1.in")).unwrap();
        fs::write(test_dir.join("sample-1.out"), "output").unwrap();

        let error = load_samples(temp.path(), "A").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_non_utf8_sample_contents() {
        let temp = tempfile::tempdir().unwrap();
        let test_dir = temp.path().join("tests").join("A");
        fs::create_dir_all(&test_dir).unwrap();
        fs::write(test_dir.join("sample-1.in"), [0xff]).unwrap();
        fs::write(test_dir.join("sample-1.out"), "output").unwrap();

        let error = load_samples(temp.path(), "A").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_symlinked_tests_directory_and_sample_file() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let external_problem = external.path().join("A");
        fs::create_dir(&external_problem).unwrap();
        fs::write(external_problem.join("sample-1.in"), "input").unwrap();
        fs::write(external_problem.join("sample-1.out"), "output").unwrap();

        if !create_directory_symlink(external.path(), &temp.path().join("tests")) {
            return;
        }
        let error = load_samples(temp.path(), "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let second = tempfile::tempdir().unwrap();
        let problem_dir = second.path().join("tests").join("A");
        fs::create_dir_all(&problem_dir).unwrap();
        if !create_file_symlink(
            &external_problem.join("sample-1.in"),
            &problem_dir.join("sample-1.in"),
        ) {
            return;
        }
        fs::write(problem_dir.join("sample-1.out"), "output").unwrap();

        let error = load_samples(second.path(), "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_file_name_in_sample_directory() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let test_dir = temp.path().join("tests").join("A");
        fs::create_dir_all(&test_dir).unwrap();
        let invalid_name = std::ffi::OsString::from_vec(vec![0xff]);
        fs::write(test_dir.join(invalid_name), "invalid").unwrap();

        let error = load_samples(temp.path(), "A").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(windows)]
    #[test]
    fn rejects_non_unicode_sample_file_name() {
        use std::os::windows::ffi::OsStringExt;

        let invalid_name = std::ffi::OsString::from_wide(&[0xd800]);

        let error = parse_sample_filename(&invalid_name).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn creates_a_source_file_for_each_problem() {
        let temp = tempfile::tempdir().unwrap();
        let problems = vec![problem("A"), problem("B"), problem("C")];

        create_source_files(temp.path(), &problems, Language::Cpp, "template").unwrap();

        for problem in problems {
            assert_eq!(
                fs::read_to_string(temp.path().join(format!("{}.cpp", problem.index))).unwrap(),
                "template"
            );
        }
    }

    #[test]
    fn creates_one_source_file_with_the_requested_extension_and_contents() {
        let cpp = tempfile::tempdir().unwrap();
        let cpp_path = create_source_file(cpp.path(), "A", Language::Cpp, "cpp template").unwrap();
        assert_eq!(cpp_path, cpp.path().join("A.cpp"));
        assert_eq!(fs::read_to_string(cpp_path).unwrap(), "cpp template");

        let python = tempfile::tempdir().unwrap();
        let python_path =
            create_source_file(python.path(), "A", Language::Python, "python template").unwrap();
        assert_eq!(python_path, python.path().join("A.py"));
        assert_eq!(fs::read_to_string(python_path).unwrap(), "python template");
    }

    #[test]
    fn creating_one_source_file_returns_already_exists_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("A.cpp");
        fs::write(&source, "complete user source").unwrap();

        let error = create_source_file(temp.path(), "A", Language::Cpp, "template")
            .expect_err("an existing source must not be overwritten");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(source).unwrap(), "complete user source");
    }

    #[test]
    fn creating_one_source_file_rejects_unsafe_names() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("cwd");
        fs::create_dir(&destination).unwrap();
        let absolute = temp.path().join("absolute");
        let unsafe_names = vec![
            "../outside".to_string(),
            "foo/bar".to_string(),
            "foo\\bar".to_string(),
            absolute
                .to_str()
                .expect("temporary path should be UTF-8")
                .to_string(),
        ];

        for name in unsafe_names {
            let error = create_source_file(&destination, &name, Language::Cpp, "template")
                .expect_err("an unsafe name must be rejected");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "name: {name:?}");
        }

        assert!(!temp.path().join("outside.cpp").exists());
        assert!(!absolute.with_extension("cpp").exists());
        assert!(!destination.join("foo").exists());
    }

    #[cfg(windows)]
    #[test]
    fn creating_one_source_file_rejects_windows_alternate_data_stream_names() {
        let temp = tempfile::tempdir().unwrap();

        let error = create_source_file(temp.path(), "base:stream", Language::Cpp, "template")
            .expect_err("an alternate data stream name must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!temp.path().join("base").exists());
    }

    #[test]
    fn creating_one_source_file_does_not_replace_a_directory_or_follow_a_symlink() {
        let directory_root = tempfile::tempdir().unwrap();
        let source_directory = directory_root.path().join("A.cpp");
        fs::create_dir(&source_directory).unwrap();

        create_source_file(directory_root.path(), "A", Language::Cpp, "template")
            .expect_err("a directory must not be replaced");
        assert!(source_directory.is_dir());

        let symlink_root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let external_source = external.path().join("user.cpp");
        fs::write(&external_source, "external user source").unwrap();
        let source_symlink = symlink_root.path().join("A.cpp");
        if !create_file_symlink(&external_source, &source_symlink) {
            return;
        }

        create_source_file(symlink_root.path(), "A", Language::Cpp, "template")
            .expect_err("a symlink must not be followed or replaced");
        assert_eq!(
            fs::read_to_string(external_source).unwrap(),
            "external user source"
        );
        assert!(
            fs::symlink_metadata(source_symlink)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(windows)]
    #[test]
    fn creating_one_source_file_respects_case_insensitive_existing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("A.cpp");
        fs::write(&source, "user source").unwrap();

        let error = create_source_file(temp.path(), "a", Language::Cpp, "template")
            .expect_err("a differently-cased existing source must not be overwritten");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(source).unwrap(), "user source");
    }

    #[test]
    fn existing_source_file_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("A.cpp");
        fs::write(&source, "user source").unwrap();

        create_source_files(temp.path(), &[problem("A")], Language::Cpp, "template").unwrap();

        assert_eq!(fs::read_to_string(source).unwrap(), "user source");
    }

    #[test]
    fn creates_python_source_files_with_the_python_extension() {
        let temp = tempfile::tempdir().unwrap();
        let problems = vec![problem("A"), problem("B")];

        create_source_files(temp.path(), &problems, Language::Python, "python template").unwrap();

        for problem in problems {
            assert_eq!(
                fs::read_to_string(temp.path().join(format!("{}.py", problem.index))).unwrap(),
                "python template"
            );
            assert!(!temp.path().join(format!("{}.cpp", problem.index)).exists());
        }
    }

    #[test]
    fn existing_python_source_file_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("A.py");
        fs::write(&source, "user source").unwrap();

        create_source_files(
            temp.path(),
            &[problem("A")],
            Language::Python,
            "python template",
        )
        .unwrap();

        assert_eq!(fs::read_to_string(source).unwrap(), "user source");
    }

    #[test]
    fn bulk_source_creation_propagates_non_already_exists_errors() {
        let temp = tempfile::tempdir().unwrap();
        let missing_destination = temp.path().join("missing");

        let error = create_source_files(
            &missing_destination,
            &[problem("A")],
            Language::Cpp,
            "template",
        )
        .expect_err("a missing destination error must be propagated");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!missing_destination.exists());
    }

    #[test]
    fn rejects_problem_index_that_escapes_destination() {
        let temp = tempfile::tempdir().unwrap();

        let error = create_source_files(
            temp.path(),
            &[problem("../outside")],
            Language::Cpp,
            "template",
        )
        .expect_err("unsafe problem index should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!temp.path().join("outside.cpp").exists());
    }

    #[test]
    fn rejects_unknown_metadata_version() {
        let temp = tempfile::tempdir().unwrap();
        let atc_dir = temp.path().join(".atc");
        fs::create_dir(&atc_dir).unwrap();
        fs::write(
            atc_dir.join("contest.toml"),
            "version = 2\ncontest_id = \"abc466\"\nproblems = []\n",
        )
        .unwrap();

        let error = load_metadata(temp.path()).expect_err("unknown version should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_contest_id_that_escapes_root() {
        let temp = tempfile::tempdir().unwrap();

        let error = contest_path(temp.path(), "../outside")
            .expect_err("unsafe contest ID should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn forced_refresh_rejects_a_symlinked_workspace_marker() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc350");
        let external = tempfile::tempdir().unwrap();
        fs::create_dir(&destination).unwrap();

        if !create_directory_symlink(external.path(), &destination.join(".atc")) {
            return;
        }

        let error = validate_refresh_destination(&destination, "abc350", true)
            .expect_err("force must not accept a symlinked marker");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn malformed_metadata_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let atc_dir = temp.path().join(".atc");
        fs::create_dir(&atc_dir).unwrap();

        fs::write(atc_dir.join("contest.toml"), "version = ???").unwrap();

        let error = load_metadata(temp.path()).expect_err("malformed metadata should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn missing_required_metadata_field_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let atc_dir = temp.path().join(".atc");
        fs::create_dir(&atc_dir).unwrap();

        fs::write(atc_dir.join("contest.toml"), "version = 1\nproblems = []\n").unwrap();

        let error = load_metadata(temp.path()).expect_err("missing required field should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn unknown_metadata_field_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let atc_dir = temp.path().join(".atc");
        fs::create_dir(&atc_dir).unwrap();

        fs::write(
            atc_dir.join("contest.toml"),
            concat!(
                "version = 1\n",
                "contest_id = \"abc466\"\n",
                "problems = []\n",
                "unexpected = true\n",
            ),
        )
        .unwrap();

        let error = load_metadata(temp.path()).expect_err("unknown field should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn refresh_replacement_rejects_tests_file_without_changing_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir(&destination).unwrap();
        let old_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("OLD")],
        };
        save_metadata(&destination, &old_contest).unwrap();
        fs::write(destination.join("tests"), "user file").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        let new_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("NEW")],
        };
        save_metadata(staging.path(), &new_contest).unwrap();

        let error = replace_refresh_data(&destination, staging, false)
            .expect_err("a tests file must not be deleted as a directory");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            fs::read_to_string(destination.join("tests")).unwrap(),
            "user file"
        );
        assert_eq!(load_metadata(&destination).unwrap(), old_contest);
    }
}
