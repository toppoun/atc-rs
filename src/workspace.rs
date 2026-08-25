use crate::language::Language;
use crate::model::{Contest, Problem, Sample};
use crate::safe_file;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::ambient_authority;
#[cfg(windows)]
use cap_std::fs::MetadataExt as _;
use cap_std::fs::{Dir as CapDir, OpenOptions as CapOpenOptions};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use tempfile::TempDir;

const METADATA_VERSION: u32 = 1;
const WORKSPACE_CONFIG_FILE: &str = ".atc-workspace.toml";
const WORKSPACE_CONFIG_VERSION: u32 = 1;

const DEFAULT_WORKSPACE_CONFIG: &str = concat!(
    "# atc workspace configuration\n",
    "#\n",
    "# 各 contest ID を以下の pattern と照合し、保存先を振り分けます。\n",
    "#\n",
    "# 例:\n",
    "#   abc123 -> ABC/abc123\n",
    "#\n",
    "# 1つの pattern に一致した場合、その `path` 配下に contest を配置します。\n",
    "# どの pattern にも一致しない場合は、workspace 直下に配置します。\n",
    "# 複数の pattern に一致した場合はエラーになります。\n",
    "#\n",
    "# 不要な振り分けは、対応する [[paths]] を削除またはコメントアウトしてください。\n",
    "\n",
    "version = 1\n",
    "\n",
    "[[paths]]\n",
    "pattern = \"^abc[0-9]+$\"\n",
    "path = \"ABC\"\n",
    "\n",
    "[[paths]]\n",
    "pattern = \"^arc[0-9]+$\"\n",
    "path = \"ARC\"\n",
    "\n",
    "[[paths]]\n",
    "pattern = \"^agc[0-9]+$\"\n",
    "path = \"AGC\"\n",
);

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
struct WorkspaceConfigFile {
    version: u32,
    paths: Vec<WorkspacePathRuleFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspacePathRuleFile {
    pattern: String,
    path: String,
}

struct WorkspaceConfig {
    paths: Vec<WorkspacePathRule>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceConfigInspection {
    pub(crate) path: PathBuf,
    pub(crate) mapping_count: usize,
}

struct WorkspacePathRule {
    pattern: Regex,
    path: WorkspaceRelativePath,
}

#[derive(Debug, PartialEq, Eq)]
struct WorkspaceRelativePath {
    components: Vec<String>,
}

impl WorkspaceRelativePath {
    fn parse(value: &str) -> io::Result<Self> {
        if value.is_empty() {
            return Err(invalid_workspace_path(
                value,
                None,
                "path must not be empty",
            ));
        }

        let components = value
            .split('/')
            .map(|component| {
                validate_workspace_path_component(component)
                    .map_err(|reason| invalid_workspace_path(value, Some(component), reason))?;
                Ok(component.to_owned())
            })
            .collect::<io::Result<Vec<_>>>()?;

        Ok(Self { components })
    }

    fn append_to(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        for component in &self.components {
            path.push(component);
        }
        path
    }

    fn components(&self) -> impl Iterator<Item = &str> {
        self.components.iter().map(String::as_str)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum WorkspaceInitialization {
    Created(PathBuf),
    AlreadyInitialized(PathBuf),
}

#[derive(Debug)]
struct WorkspaceConfigContext {
    action: &'static str,
    path: PathBuf,
    source: Box<dyn Error + Send + Sync>,
}

impl std::fmt::Display for WorkspaceConfigContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "failed to {} workspace config {}: {}",
            self.action,
            self.path.display(),
            self.source
        )
    }
}

impl Error for WorkspaceConfigContext {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn workspace_config_error(
    path: &Path,
    action: &'static str,
    kind: io::ErrorKind,
    source: impl Error + Send + Sync + 'static,
) -> io::Error {
    io::Error::new(
        kind,
        WorkspaceConfigContext {
            action,
            path: path.to_path_buf(),
            source: Box::new(source),
        },
    )
}

fn parse_workspace_config(path: &Path, content: &str) -> io::Result<WorkspaceConfig> {
    let config: WorkspaceConfigFile = toml::from_str(content).map_err(|error| {
        workspace_config_error(path, "parse", io::ErrorKind::InvalidData, error)
    })?;

    if config.version != WORKSPACE_CONFIG_VERSION {
        return Err(workspace_config_error(
            path,
            "validate",
            io::ErrorKind::InvalidData,
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported workspace config version: {} (expected {WORKSPACE_CONFIG_VERSION})",
                    config.version
                ),
            ),
        ));
    }

    let paths = config
        .paths
        .into_iter()
        .map(|rule| {
            let workspace_path = WorkspaceRelativePath::parse(&rule.path).map_err(|error| {
                workspace_config_error(path, "validate", io::ErrorKind::InvalidData, error)
            })?;

            let pattern = Regex::new(&rule.pattern).map_err(|error| {
                workspace_config_error(path, "validate", io::ErrorKind::InvalidData, error)
            })?;

            Ok(WorkspacePathRule {
                pattern,
                path: workspace_path,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;

    Ok(WorkspaceConfig { paths })
}

fn load_workspace_config_file(path: &Path) -> io::Result<WorkspaceConfig> {
    let content = fs::read_to_string(path)
        .map_err(|error| workspace_config_error(path, "read", error.kind(), error))?;

    parse_workspace_config(path, &content)
}

fn load_workspace_config(root: &Path) -> io::Result<Option<WorkspaceConfig>> {
    let path = root.join(WORKSPACE_CONFIG_FILE);

    if !existing_regular_file(&path, "workspace config")
        .map_err(|error| workspace_config_error(&path, "inspect", error.kind(), error))?
    {
        return Ok(None);
    }

    load_workspace_config_file(&path).map(Some)
}

pub(crate) fn inspect_workspace_config(
    root: &Path,
) -> io::Result<Option<WorkspaceConfigInspection>> {
    let path = root.join(WORKSPACE_CONFIG_FILE);

    load_workspace_config(root).map(|config| {
        config.map(|config| WorkspaceConfigInspection {
            path,
            mapping_count: config.paths.len(),
        })
    })
}

fn load_existing_workspace_config(
    root: &Path,
    directory: &CapDir,
) -> io::Result<Option<WorkspaceConfig>> {
    let path = root.join(WORKSPACE_CONFIG_FILE);

    match directory.symlink_metadata(WORKSPACE_CONFIG_FILE) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("workspace config is not a regular file: {}", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(workspace_config_error(
                &path,
                "inspect",
                error.kind(),
                error,
            ));
        }
    }

    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);

    let mut file = match directory.open_with(WORKSPACE_CONFIG_FILE, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(workspace_config_error(&path, "open", error.kind(), error));
        }
    };

    let metadata = file
        .metadata()
        .map_err(|error| workspace_config_error(&path, "inspect open", error.kind(), error))?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("workspace config is not a regular file: {}", path.display()),
        ));
    }

    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|error| workspace_config_error(&path, "read", error.kind(), error))?;

    parse_workspace_config(&path, &content).map(Some)
}

fn create_default_workspace_config(path: &Path) -> io::Result<()> {
    safe_file::install_noclobber(path, DEFAULT_WORKSPACE_CONFIG.as_bytes(), ".atc-workspace-")
        .map_err(|error| workspace_config_error(path, "create", error.kind(), error))
}

pub fn initialize_workspace(root: &Path) -> io::Result<WorkspaceInitialization> {
    let path = root.join(WORKSPACE_CONFIG_FILE);
    let directory = CapDir::open_ambient_dir(root, ambient_authority()).map_err(|error| {
        workspace_config_error(&path, "open parent directory for", error.kind(), error)
    })?;

    if load_existing_workspace_config(root, &directory)?.is_some() {
        return Ok(WorkspaceInitialization::AlreadyInitialized(path));
    }

    match create_default_workspace_config(&path) {
        Ok(()) => Ok(WorkspaceInitialization::Created(path)),
        Err(create_error) if create_error.kind() == io::ErrorKind::AlreadyExists => {
            match load_existing_workspace_config(root, &directory)? {
                Some(_) => Ok(WorkspaceInitialization::AlreadyInitialized(path)),
                None => Err(create_error),
            }
        }
        Err(error) => Err(error),
    }
}

pub fn resolve_contest_path(root: &Path, contest_id: &str) -> io::Result<PathBuf> {
    validate_path_component(contest_id, "contest ID")?;

    let Some(config) = load_workspace_config(root)? else {
        return contest_path(root, contest_id);
    };

    match matching_workspace_path(&config, contest_id)? {
        Some(path) => {
            walk_workspace_mapping(root, path, false)?;
            contest_path(&path.append_to(root), contest_id)
        }
        None => contest_path(root, contest_id),
    }
}

#[derive(Debug)]
struct WorkspaceDirectoryContext {
    action: &'static str,
    path: PathBuf,
    source: io::Error,
}

impl std::fmt::Display for WorkspaceDirectoryContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "failed to {} {}: {}",
            self.action,
            self.path.display(),
            self.source
        )
    }
}

impl Error for WorkspaceDirectoryContext {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

fn workspace_directory_error(path: &Path, action: &'static str, source: io::Error) -> io::Error {
    let kind = source.kind();
    io::Error::new(
        kind,
        WorkspaceDirectoryContext {
            action,
            path: path.to_path_buf(),
            source,
        },
    )
}

pub fn ensure_workspace_contest_parent(
    root: &Path,
    contest_id: &str,
    expected_destination: &Path,
) -> io::Result<()> {
    validate_path_component(contest_id, "contest ID")?;

    let config = load_workspace_config(root)?;
    let mapping = match config.as_ref() {
        Some(config) => matching_workspace_path(config, contest_id)?,
        None => None,
    };

    let actual_destination = match mapping {
        Some(path) => contest_path(&path.append_to(root), contest_id)?,
        None => contest_path(root, contest_id)?,
    };
    if actual_destination != expected_destination {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "workspace config changed while preparing contest {contest_id:?}: expected {}, now resolves to {}",
                expected_destination.display(),
                actual_destination.display()
            ),
        ));
    }

    if let Some(path) = mapping {
        walk_workspace_mapping(root, path, true)?;
    }

    Ok(())
}

fn matching_workspace_path<'a>(
    config: &'a WorkspaceConfig,
    contest_id: &str,
) -> io::Result<Option<&'a WorkspaceRelativePath>> {
    let mut matched_path = None;

    for rule in &config.paths {
        if !rule.pattern.is_match(contest_id) {
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

    Ok(matched_path)
}

fn walk_workspace_mapping(
    root: &Path,
    mapping: &WorkspaceRelativePath,
    create_missing: bool,
) -> io::Result<()> {
    let mut current = CapDir::open_ambient_dir(root, ambient_authority())
        .map_err(|error| workspace_directory_error(root, "open workspace root", error))?;
    let mut current_path = root.to_path_buf();

    for component in mapping.components() {
        current_path.push(component);

        #[cfg(windows)]
        reject_windows_reparse_point(&current, component, &current_path)?;

        match current.open_dir_nofollow(component) {
            Ok(directory) => {
                current = directory;
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !create_missing => {
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match current.create_dir(component) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(workspace_directory_error(
                            &current_path,
                            "create workspace mapping directory",
                            error,
                        ));
                    }
                }
            }
            Err(error) => {
                return Err(workspace_directory_error(
                    &current_path,
                    "open workspace mapping directory without following links",
                    error,
                ));
            }
        }

        #[cfg(windows)]
        reject_windows_reparse_point(&current, component, &current_path)?;

        current = current.open_dir_nofollow(component).map_err(|error| {
            workspace_directory_error(
                &current_path,
                "open workspace mapping directory without following links",
                error,
            )
        })?;
    }

    Ok(())
}

#[cfg(windows)]
fn reject_windows_reparse_point(parent: &CapDir, component: &str, path: &Path) -> io::Result<()> {
    // FILE_ATTRIBUTE_REPARSE_POINT. Keep the numeric value local so the existing
    // windows-sys feature set does not need to expand for one stable attribute bit.
    const REPARSE_POINT_ATTRIBUTE: u32 = 0x0400;

    match parent.symlink_metadata(component) {
        Ok(metadata) if metadata.file_attributes() & REPARSE_POINT_ATTRIBUTE != 0 => {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "workspace mapping directory must not be a reparse point: {}",
                    path.display()
                ),
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(workspace_directory_error(
            path,
            "inspect workspace mapping directory for reparse points",
            error,
        )),
    }
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

    let number_text = number;
    let number = number_text.parse::<usize>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid sample number in file name: {name}"),
        )
    })?;

    if number == 0 || number.to_string() != number_text {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sample number must use its canonical positive form: {name}"),
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

fn invalid_workspace_path(value: &str, component: Option<&str>, reason: &'static str) -> io::Error {
    let message = match component {
        Some(component) => {
            format!("invalid workspace path component {component:?} in {value:?}: {reason}")
        }
        None => format!("invalid workspace path {value:?}: {reason}"),
    };

    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn validate_workspace_path_component(component: &str) -> Result<(), &'static str> {
    if component.is_empty() {
        return Err("components must not be empty");
    }
    if matches!(component, "." | "..") {
        return Err("components must not be `.` or `..`");
    }
    if component.contains(['/', '\\']) {
        return Err("`/` is the only separator and cannot appear inside a component");
    }
    if component
        .chars()
        .any(|character| character <= '\u{1f}' || r#"<>:\"|?*"#.contains(character))
    {
        return Err("component contains a character that is not portable to Windows");
    }
    if component.ends_with([' ', '.']) {
        return Err("components must not end with a space or period");
    }

    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || matches!(
        // Windows recognizes the ISO-8859-1 superscript digits as COM/LPT
        // device-number aliases as well.
        stem.as_str(),
        "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "COM¹"
            | "COM²"
            | "COM³"
    ) || matches!(
        stem.as_str(),
        "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
    ) {
        return Err("component uses a reserved Windows device name");
    }

    Ok(())
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
    replace_refresh_data_with_hooks(
        destination,
        staging,
        allow_missing_marker,
        || {},
        || {},
        || {},
    )
}

fn replace_refresh_data_with_hooks(
    destination: &Path,
    staging: TempDir,
    allow_missing_marker: bool,
    before_tests_backup: impl FnOnce(),
    before_tests_install: impl FnOnce(),
    before_recovery_cleanup: impl FnOnce(),
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

    let owned_problem_indices = if had_destination_tests {
        let indices = refresh_owned_problem_indices(destination, &staging_root)?;
        validate_refresh_owned_tests(&destination_tests, &indices)?;
        indices
    } else {
        HashSet::new()
    };

    before_tests_backup();

    if had_destination_tests {
        safe_file::rename_noclobber(&destination_tests, &backup_tests)?;
        if let Err(error) = validate_refresh_owned_tests(&backup_tests, &owned_problem_indices) {
            let rollback_errors = rollback_tests(
                &destination_tests,
                &staged_tests,
                &backup_tests,
                false,
                true,
            );
            return Err(refresh_update_error(staging, error, rollback_errors));
        }
    }

    before_tests_install();

    if has_staged_tests
        && let Err(error) = safe_file::rename_noclobber(&staged_tests, &destination_tests)
    {
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
        && let Err(error) = safe_file::rename_noclobber(&destination_metadata, &backup_metadata)
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

    if let Err(error) = safe_file::rename_noclobber(&staged_metadata, &destination_metadata) {
        let mut rollback_errors = Vec::new();
        if had_destination_metadata
            && let Err(rollback_error) =
                safe_file::rename_noclobber(&backup_metadata, &destination_metadata)
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

    before_recovery_cleanup();
    if had_destination_tests
        && let Err(error) = validate_refresh_owned_tests(&backup_tests, &owned_problem_indices)
    {
        let kind = error.kind();
        let recovery_path = staging.keep();
        return Err(io::Error::new(
            kind,
            format!(
                "refresh installed new data, but the previous tests changed before cleanup: {error}; recovery data kept at {}",
                recovery_path.display()
            ),
        ));
    }

    Ok(())
}

fn refresh_owned_problem_indices(
    destination: &Path,
    staging_root: &Path,
) -> io::Result<HashSet<String>> {
    let staged_contest = load_metadata(staging_root)?;
    validate_contest_paths(&staged_contest)?;

    let mut indices = staged_contest
        .problems
        .iter()
        .map(|problem| problem.index.to_ascii_lowercase())
        .collect::<HashSet<_>>();

    if let ContestMetadataHealth::Healthy(previous_contest) = inspect_contest_metadata(destination)?
        && validate_contest_paths(&previous_contest).is_ok()
        && validate_contest_identity(&previous_contest, &staged_contest.contest_id).is_ok()
    {
        indices.extend(
            previous_contest
                .problems
                .iter()
                .map(|problem| problem.index.to_ascii_lowercase()),
        );
    }

    Ok(indices)
}

fn validate_refresh_owned_tests(
    tests_directory: &Path,
    owned_problem_indices: &HashSet<String>,
) -> io::Result<()> {
    for problem_entry in fs::read_dir(tests_directory)? {
        let problem_entry = problem_entry?;
        let problem_path = problem_entry.path();
        let Some(problem_index) = problem_entry.file_name().to_str().map(str::to_owned) else {
            return Err(unowned_refresh_test_error(&problem_path));
        };
        if refresh_entry_is_reparse(&problem_path)?
            || !problem_entry.file_type()?.is_dir()
            || !owned_problem_indices.contains(&problem_index.to_ascii_lowercase())
        {
            return Err(unowned_refresh_test_error(&problem_path));
        }

        for sample_entry in fs::read_dir(&problem_path)? {
            let sample_entry = sample_entry?;
            let sample_path = sample_entry.path();
            if refresh_entry_is_reparse(&sample_path)?
                || !sample_entry.file_type()?.is_file()
                || !matches!(
                    parse_sample_filename(&sample_entry.file_name()),
                    Ok(Some(_))
                )
            {
                return Err(unowned_refresh_test_error(&sample_path));
            }
        }
    }

    Ok(())
}

#[cfg(windows)]
fn refresh_entry_is_reparse(path: &Path) -> io::Result<bool> {
    Ok(metadata_is_reparse(&fs::symlink_metadata(path)?))
}

#[cfg(not(windows))]
fn refresh_entry_is_reparse(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

fn unowned_refresh_test_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "refusing to refresh because tests contains an unowned entry: {}; move it outside tests and retry",
            path.display()
        ),
    )
}

fn existing_real_directory(path: &Path, kind: &str) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_reparse(&metadata) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is a reparse point: {}", path.display()),
        )),
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
        Ok(metadata) if metadata_is_reparse(&metadata) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is a reparse point: {}", path.display()),
        )),
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} is not a regular file: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

fn rollback_tests(
    destination_tests: &Path,
    staged_tests: &Path,
    backup_tests: &Path,
    new_tests_were_moved: bool,
    old_tests_were_moved: bool,
) -> Vec<String> {
    let mut errors = Vec::new();

    if new_tests_were_moved
        && let Err(error) = safe_file::rename_noclobber(destination_tests, staged_tests)
    {
        errors.push(format!(
            "failed to move new tests out of {}: {error}",
            destination_tests.display()
        ));
    }
    if old_tests_were_moved
        && let Err(error) = safe_file::rename_noclobber(backup_tests, destination_tests)
    {
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

    #[derive(Serialize)]
    struct TestWorkspaceConfig<'a> {
        version: u32,
        paths: Vec<TestWorkspacePathRule<'a>>,
    }

    #[derive(Serialize)]
    struct TestWorkspacePathRule<'a> {
        pattern: &'a str,
        path: &'a str,
    }

    fn write_workspace_config(root: &Path, body: &str) {
        fs::write(root.join(WORKSPACE_CONFIG_FILE), body).unwrap();
    }

    fn workspace_config_with_path(path: &str) -> String {
        toml::to_string(&TestWorkspaceConfig {
            version: WORKSPACE_CONFIG_VERSION,
            paths: vec![TestWorkspacePathRule {
                pattern: "^abc[0-9]+$",
                path,
            }],
        })
        .unwrap()
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
    fn contest_resolver_falls_back_without_a_match_and_rejects_ambiguity() {
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
        assert_eq!(
            resolve_contest_path(temp.path(), "agc001").unwrap(),
            temp.path().join("agc001")
        );

        write_workspace_config(
            temp.path(),
            concat!(
                "version = 1\n",
                "[[paths]]\npattern = \"abc\"\npath = \"same\"\n",
                "[[paths]]\npattern = \"^abc466$\"\npath = \"same\"\n",
            ),
        );
        assert!(
            resolve_contest_path(temp.path(), "abc466").is_err(),
            "multiple matching rules remain ambiguous even when their paths are equal"
        );
    }

    #[test]
    fn contest_resolver_supports_portable_nested_workspace_paths() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_config(
            temp.path(),
            &workspace_config_with_path("AtCoder/Contests/ABC"),
        );

        assert_eq!(
            resolve_contest_path(temp.path(), "abc123").unwrap(),
            temp.path()
                .join("AtCoder")
                .join("Contests")
                .join("ABC")
                .join("abc123")
        );
        assert!(
            !temp.path().join("AtCoder").exists(),
            "resolution alone must not create mapped directories"
        );

        write_workspace_config(temp.path(), &workspace_config_with_path("競技/ABC"));
        assert_eq!(
            resolve_contest_path(temp.path(), "abc123").unwrap(),
            temp.path().join("競技").join("ABC").join("abc123")
        );
    }

    #[test]
    fn workspace_parser_rejects_nonportable_mapping_paths_on_every_os() {
        for mapping in [
            "",
            ".",
            "..",
            "../ABC",
            "ABC/../ARC",
            "ABC/./ARC",
            "/ABC",
            "ABC/",
            "ABC//ARC",
            "ABC\\Nested",
            "\\ABC",
            "\\\\server\\share",
            "//server/share",
            "C:/ABC",
            "C:ABC",
            "NUL",
            "NUL.txt",
            "CON",
            "con.cpp",
            "COM1",
            "COM¹",
            "com².txt",
            "COM³",
            "LPT¹",
            "lpt².log",
            "LPT³",
            "LPT9.log",
            "CONIN$",
            "CONOUT$.txt",
            "name.",
            "name ",
            "name<bad",
            "control\u{1f}name",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(WORKSPACE_CONFIG_FILE);
            let bytes = workspace_config_with_path(mapping).into_bytes();
            fs::write(&path, &bytes).unwrap();

            let error = resolve_contest_path(temp.path(), "abc123")
                .expect_err("nonportable mappings must fail during normal resolution");
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidData,
                "mapping {mapping:?}"
            );
            assert!(error.to_string().contains(&path.display().to_string()));
            assert!(error.to_string().contains(&format!("{mapping:?}")));

            assert_eq!(
                initialize_workspace(temp.path()).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "mapping {mapping:?}"
            );
            assert_eq!(fs::read(path).unwrap(), bytes, "mapping {mapping:?}");
        }
    }

    #[test]
    fn workspace_initializer_creates_the_exact_default_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(WORKSPACE_CONFIG_FILE);

        assert_eq!(
            initialize_workspace(temp.path()).unwrap(),
            WorkspaceInitialization::Created(path.clone())
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            DEFAULT_WORKSPACE_CONFIG.as_bytes()
        );
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert!(!bytes.contains(&b'\r'));
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains("#   abc123 -> ABC/abc123\n")
        );

        let raw: WorkspaceConfigFile = toml::from_str(DEFAULT_WORKSPACE_CONFIG).unwrap();
        assert_eq!(raw.version, WORKSPACE_CONFIG_VERSION);
        assert_eq!(raw.paths.len(), 3);
        assert_eq!(raw.paths[0].pattern, "^abc[0-9]+$");
        assert_eq!(raw.paths[0].path, "ABC");
        assert_eq!(raw.paths[1].pattern, "^arc[0-9]+$");
        assert_eq!(raw.paths[1].path, "ARC");
        assert_eq!(raw.paths[2].pattern, "^agc[0-9]+$");
        assert_eq!(raw.paths[2].path, "AGC");

        for (contest_id, relative) in [
            ("abc123", PathBuf::from("ABC").join("abc123")),
            ("arc123", PathBuf::from("ARC").join("arc123")),
            ("agc123", PathBuf::from("AGC").join("agc123")),
            ("typical90", PathBuf::from("typical90")),
        ] {
            assert_eq!(
                resolve_contest_path(temp.path(), contest_id).unwrap(),
                temp.path().join(relative)
            );
        }

        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [OsStr::new(WORKSPACE_CONFIG_FILE)]);
    }

    #[test]
    fn workspace_initializer_is_idempotent_without_changing_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(WORKSPACE_CONFIG_FILE);

        assert!(matches!(
            initialize_workspace(temp.path()).unwrap(),
            WorkspaceInitialization::Created(_)
        ));
        let original = fs::read(&path).unwrap();
        assert_eq!(
            initialize_workspace(temp.path()).unwrap(),
            WorkspaceInitialization::AlreadyInitialized(path.clone())
        );
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn workspace_initializer_preserves_custom_valid_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(WORKSPACE_CONFIG_FILE);
        let custom = concat!(
            "# user customization\n",
            "version = 1\n",
            "[[paths]]\n",
            "pattern = \"^abc[0-9]+$\"\n",
            "path = \"AtCoder/ABC\"\n",
        );
        fs::write(&path, custom).unwrap();

        assert_eq!(
            initialize_workspace(temp.path()).unwrap(),
            WorkspaceInitialization::AlreadyInitialized(path.clone())
        );
        assert_eq!(fs::read(&path).unwrap(), custom.as_bytes());
        assert_eq!(
            resolve_contest_path(temp.path(), "agc123").unwrap(),
            temp.path().join("agc123")
        );
    }

    #[test]
    fn workspace_initializer_rejects_invalid_toml_without_modifying_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(WORKSPACE_CONFIG_FILE);
        let invalid = b"version = [\n";
        fs::write(&path, invalid).unwrap();

        let error = initialize_workspace(temp.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(&path.display().to_string()));
        let context = error
            .get_ref()
            .expect("the path context should be retained");
        assert!(
            context.source().is_some(),
            "the parser error should remain in the source chain"
        );
        assert_eq!(fs::read(path).unwrap(), invalid);
    }

    #[test]
    fn workspace_initializer_rejects_unsupported_version_without_modifying_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(WORKSPACE_CONFIG_FILE);
        let unsupported = b"version = 999\npaths = []\n";
        fs::write(&path, unsupported).unwrap();

        let error = initialize_workspace(temp.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("unsupported workspace config version: 999")
        );
        assert!(error.to_string().contains(&path.display().to_string()));
        assert_eq!(fs::read(path).unwrap(), unsupported);
    }

    #[test]
    fn workspace_initializer_rejects_invalid_mapping_without_modifying_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(WORKSPACE_CONFIG_FILE);
        let invalid = b"version = 1\n[[paths]]\npattern = \"[\"\npath = \"ABC\"\n";
        fs::write(&path, invalid).unwrap();

        assert_eq!(
            initialize_workspace(temp.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(path).unwrap(), invalid);
    }

    #[test]
    fn workspace_initializer_rejects_a_directory_target_without_touching_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(WORKSPACE_CONFIG_FILE);
        fs::create_dir(&path).unwrap();

        let error = initialize_workspace(temp.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(path.is_dir());
        assert!(fs::read_dir(path).unwrap().next().is_none());
    }

    #[test]
    fn workspace_initializer_rejects_a_symlink_target_without_following_it() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        fs::write(external.path(), "external user data").unwrap();
        let path = temp.path().join(WORKSPACE_CONFIG_FILE);
        if !create_file_symlink(external.path(), &path) {
            return;
        }

        let error = initialize_workspace(temp.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            fs::read_to_string(external.path()).unwrap(),
            "external user data"
        );
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }

    #[test]
    fn default_creation_primitive_never_clobbers_an_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(WORKSPACE_CONFIG_FILE);
        let existing = b"user data that must not be truncated";
        fs::write(&path, existing).unwrap();

        let error = create_default_workspace_config(&path).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), existing);
        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [OsStr::new(WORKSPACE_CONFIG_FILE)]);
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
    fn workspace_parent_preparation_creates_a_fully_missing_nested_hierarchy() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_config(
            temp.path(),
            &workspace_config_with_path("AtCoder/Contests/ABC"),
        );
        let destination = temp
            .path()
            .join("AtCoder")
            .join("Contests")
            .join("ABC")
            .join("abc123");

        ensure_workspace_contest_parent(temp.path(), "abc123", &destination).unwrap();

        for directory in [
            temp.path().join("AtCoder"),
            temp.path().join("AtCoder").join("Contests"),
            temp.path().join("AtCoder").join("Contests").join("ABC"),
        ] {
            assert!(
                fs::symlink_metadata(directory)
                    .unwrap()
                    .file_type()
                    .is_dir()
            );
        }
        assert!(!destination.exists());
    }

    #[test]
    fn workspace_parent_preparation_accepts_partial_and_complete_real_hierarchies() {
        for existing_components in [1, 3] {
            let temp = tempfile::tempdir().unwrap();
            write_workspace_config(
                temp.path(),
                &workspace_config_with_path("AtCoder/Contests/ABC"),
            );
            let mapped = temp.path().join("AtCoder").join("Contests").join("ABC");
            let existing = match existing_components {
                1 => temp.path().join("AtCoder"),
                3 => mapped.clone(),
                _ => unreachable!(),
            };
            fs::create_dir_all(existing).unwrap();
            let destination = mapped.join("abc123");

            ensure_workspace_contest_parent(temp.path(), "abc123", &destination).unwrap();

            assert!(fs::symlink_metadata(&mapped).unwrap().file_type().is_dir());
            assert!(!destination.exists());
        }
    }

    #[test]
    fn workspace_parent_preparation_rejects_an_intermediate_file() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_config(
            temp.path(),
            &workspace_config_with_path("AtCoder/Contests/ABC"),
        );
        fs::create_dir(temp.path().join("AtCoder")).unwrap();
        let blocking_file = temp.path().join("AtCoder").join("Contests");
        fs::write(&blocking_file, "user data").unwrap();
        let destination = blocking_file.join("ABC").join("abc123");

        let resolution_error = resolve_contest_path(temp.path(), "abc123")
            .expect_err("resolution must reject an existing intermediate file");
        assert!(
            resolution_error
                .to_string()
                .contains(&blocking_file.display().to_string())
        );

        let error = ensure_workspace_contest_parent(temp.path(), "abc123", &destination)
            .expect_err("an intermediate file must not be traversed or replaced");

        assert!(
            error
                .to_string()
                .contains(&blocking_file.display().to_string())
        );
        assert_eq!(fs::read_to_string(blocking_file).unwrap(), "user data");
    }

    #[test]
    fn workspace_mapping_rejects_an_existing_intermediate_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        write_workspace_config(
            temp.path(),
            &workspace_config_with_path("AtCoder/Contests/ABC"),
        );
        let mapped_link = temp.path().join("AtCoder");
        if !create_directory_symlink(external.path(), &mapped_link) {
            return;
        }
        let destination = mapped_link.join("Contests").join("ABC").join("abc123");

        resolve_contest_path(temp.path(), "abc123")
            .expect_err("resolution must reject an existing mapped symlink");
        ensure_workspace_contest_parent(temp.path(), "abc123", &destination)
            .expect_err("preparation must reject an existing mapped symlink");

        assert!(fs::read_dir(external.path()).unwrap().next().is_none());
        assert!(
            fs::symlink_metadata(mapped_link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
    fn rejects_noncanonical_and_invalid_sample_names() {
        let temp = tempfile::tempdir().unwrap();
        let duplicate = temp.path().join("tests").join("A");
        fs::create_dir_all(&duplicate).unwrap();
        fs::write(duplicate.join("sample-1.in"), "input").unwrap();
        fs::write(duplicate.join("sample-01.in"), "duplicate").unwrap();
        fs::write(duplicate.join("sample-1.out"), "output").unwrap();

        let error = load_samples(temp.path(), "A").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("canonical positive form"));

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

    #[cfg(target_os = "linux")]
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

    #[test]
    fn refresh_replacement_preserves_unowned_entries_and_leaves_workspace_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir(&destination).unwrap();
        let old_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A")],
        };
        save_metadata(&destination, &old_contest).unwrap();
        let old_tests = destination.join("tests").join("A");
        fs::create_dir_all(&old_tests).unwrap();
        fs::write(old_tests.join("sample-1.in"), "old input\n").unwrap();
        fs::write(old_tests.join("sample-1.out"), "old output\n").unwrap();
        fs::write(old_tests.join("README.txt"), "user notes\n").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        let new_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A")],
        };
        save_metadata(staging.path(), &new_contest).unwrap();
        save_samples(
            staging.path(),
            &new_contest.problems[0],
            &[Sample {
                input: "new input\n".to_string(),
                output: "new output\n".to_string(),
            }],
        )
        .unwrap();

        let error = replace_refresh_data(&destination, staging, false)
            .expect_err("refresh must not delete an unowned test entry");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("README.txt"));
        assert_eq!(
            fs::read_to_string(old_tests.join("README.txt")).unwrap(),
            "user notes\n"
        );
        assert_eq!(
            fs::read_to_string(old_tests.join("sample-1.in")).unwrap(),
            "old input\n"
        );
        assert_eq!(load_metadata(&destination).unwrap(), old_contest);
    }

    #[test]
    fn refresh_replacement_does_not_claim_a_noncanonical_sample_name() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir(&destination).unwrap();
        let old_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A")],
        };
        save_metadata(&destination, &old_contest).unwrap();
        let old_tests = destination.join("tests").join("A");
        fs::create_dir_all(&old_tests).unwrap();
        fs::write(old_tests.join("sample-01.in"), "user input\n").unwrap();
        fs::write(old_tests.join("sample-01.out"), "user output\n").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        save_metadata(staging.path(), &old_contest).unwrap();
        save_samples(
            staging.path(),
            &old_contest.problems[0],
            &[Sample {
                input: "new input\n".to_string(),
                output: "new output\n".to_string(),
            }],
        )
        .unwrap();

        let error = replace_refresh_data(&destination, staging, false)
            .expect_err("a noncanonical sample name must not be treated as refresh-owned");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("sample-01.in"));
        assert_eq!(
            fs::read_to_string(old_tests.join("sample-01.in")).unwrap(),
            "user input\n"
        );
        assert_eq!(load_metadata(&destination).unwrap(), old_contest);
    }

    #[test]
    fn refresh_replacement_does_not_follow_or_claim_a_sample_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir(&destination).unwrap();
        let contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A")],
        };
        save_metadata(&destination, &contest).unwrap();
        let old_tests = destination.join("tests").join("A");
        fs::create_dir_all(&old_tests).unwrap();
        let external_input = temp.path().join("external-input.txt");
        fs::write(&external_input, "external user input\n").unwrap();
        let linked_input = old_tests.join("sample-1.in");
        if !create_file_symlink(&external_input, &linked_input) {
            return;
        }
        fs::write(old_tests.join("sample-1.out"), "old output\n").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        save_metadata(staging.path(), &contest).unwrap();
        save_samples(
            staging.path(),
            &contest.problems[0],
            &[Sample {
                input: "new input\n".to_string(),
                output: "new output\n".to_string(),
            }],
        )
        .unwrap();

        let error = replace_refresh_data(&destination, staging, false)
            .expect_err("a sample symlink must not be treated as refresh-owned");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(&external_input).unwrap(),
            "external user input\n"
        );
        assert!(
            fs::symlink_metadata(&linked_input)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(load_metadata(&destination).unwrap(), contest);
    }

    #[test]
    fn wrong_contest_prior_metadata_cannot_broaden_refresh_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir(&destination).unwrap();
        let unrelated_contest = Contest {
            contest_id: "other-contest".to_string(),
            problems: vec![problem("LOCAL")],
        };
        save_metadata(&destination, &unrelated_contest).unwrap();
        let local_tests = destination.join("tests").join("LOCAL");
        fs::create_dir_all(&local_tests).unwrap();
        fs::write(local_tests.join("sample-1.in"), "user input\n").unwrap();
        fs::write(local_tests.join("sample-1.out"), "user output\n").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        let new_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A")],
        };
        save_metadata(staging.path(), &new_contest).unwrap();
        save_samples(
            staging.path(),
            &new_contest.problems[0],
            &[Sample {
                input: "new input\n".to_string(),
                output: "new output\n".to_string(),
            }],
        )
        .unwrap();

        let error = replace_refresh_data(&destination, staging, false)
            .expect_err("wrong-contest metadata must not claim the LOCAL test namespace");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("LOCAL"));
        assert_eq!(
            fs::read_to_string(local_tests.join("sample-1.in")).unwrap(),
            "user input\n"
        );
        assert_eq!(load_metadata(&destination).unwrap(), unrelated_contest);
    }

    #[test]
    fn duplicate_prior_metadata_cannot_broaden_refresh_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir_all(destination.join(".atc")).unwrap();
        fs::write(
            destination.join(".atc").join("contest.toml"),
            concat!(
                "version = 1\n",
                "contest_id = \"abc466\"\n",
                "[[problems]]\n",
                "index = \"LOCAL\"\n",
                "title = \"Local\"\n",
                "task_id = \"local\"\n",
                "url = \"https://example.invalid/local\"\n",
                "[[problems]]\n",
                "index = \"local\"\n",
                "title = \"Duplicate\"\n",
                "task_id = \"duplicate\"\n",
                "url = \"https://example.invalid/duplicate\"\n",
            ),
        )
        .unwrap();
        let local_tests = destination.join("tests").join("LOCAL");
        fs::create_dir_all(&local_tests).unwrap();
        fs::write(local_tests.join("sample-1.in"), "user input\n").unwrap();
        fs::write(local_tests.join("sample-1.out"), "user output\n").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        let new_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A")],
        };
        save_metadata(staging.path(), &new_contest).unwrap();

        let error = replace_refresh_data(&destination, staging, false)
            .expect_err("duplicate prior indices must invalidate all prior ownership claims");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("LOCAL"));
        assert_eq!(
            fs::read_to_string(local_tests.join("sample-1.in")).unwrap(),
            "user input\n"
        );
    }

    #[test]
    fn refresh_post_move_reinspection_restores_a_late_unowned_entry() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir(&destination).unwrap();
        let contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A")],
        };
        save_metadata(&destination, &contest).unwrap();
        let old_tests = destination.join("tests").join("A");
        fs::create_dir_all(&old_tests).unwrap();
        fs::write(old_tests.join("sample-1.in"), "old input\n").unwrap();
        fs::write(old_tests.join("sample-1.out"), "old output\n").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        save_metadata(staging.path(), &contest).unwrap();
        save_samples(
            staging.path(),
            &contest.problems[0],
            &[Sample {
                input: "new input\n".to_string(),
                output: "new output\n".to_string(),
            }],
        )
        .unwrap();

        let error = replace_refresh_data_with_hooks(
            &destination,
            staging,
            false,
            || fs::write(old_tests.join("notes.txt"), "late user data\n").unwrap(),
            || {},
            || {},
        )
        .expect_err("post-move validation must detect the late unowned entry");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(old_tests.join("notes.txt")).unwrap(),
            "late user data\n"
        );
        assert_eq!(
            fs::read_to_string(old_tests.join("sample-1.in")).unwrap(),
            "old input\n"
        );
        assert_eq!(load_metadata(&destination).unwrap(), contest);
        assert!(fs::read_dir(&destination).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".atc-refresh-")
        }));
    }

    #[test]
    fn refresh_install_and_rollback_race_preserves_competitor_and_recovery_data() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir(&destination).unwrap();
        let contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A")],
        };
        save_metadata(&destination, &contest).unwrap();
        let old_tests = destination.join("tests").join("A");
        fs::create_dir_all(&old_tests).unwrap();
        fs::write(old_tests.join("sample-1.in"), "old input\n").unwrap();
        fs::write(old_tests.join("sample-1.out"), "old output\n").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        save_metadata(staging.path(), &contest).unwrap();
        save_samples(
            staging.path(),
            &contest.problems[0],
            &[Sample {
                input: "new input\n".to_string(),
                output: "new output\n".to_string(),
            }],
        )
        .unwrap();

        let competing_tests = destination.join("tests");
        let error = replace_refresh_data_with_hooks(
            &destination,
            staging,
            false,
            || {},
            || {
                fs::create_dir(&competing_tests).unwrap();
                fs::write(competing_tests.join("competitor.txt"), "keep me\n").unwrap();
            },
            || {},
        )
        .expect_err("a competing tests destination must block install and rollback");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("rollback also failed"));
        assert!(error.to_string().contains("recovery data kept at"));
        assert_eq!(
            fs::read_to_string(competing_tests.join("competitor.txt")).unwrap(),
            "keep me\n"
        );
        let recovery = fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".atc-refresh-")
            })
            .expect("failed rollback must retain the refresh staging directory");
        assert_eq!(
            fs::read_to_string(
                recovery
                    .join("previous-tests")
                    .join("A")
                    .join("sample-1.in")
            )
            .unwrap(),
            "old input\n"
        );
        assert_eq!(
            fs::read_to_string(recovery.join("tests").join("A").join("sample-1.in")).unwrap(),
            "new input\n"
        );
        assert_eq!(load_metadata(&destination).unwrap(), contest);
    }

    #[test]
    fn refresh_final_reinspection_keeps_a_late_changed_previous_generation() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("abc466");
        fs::create_dir(&destination).unwrap();
        let old_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("A")],
        };
        save_metadata(&destination, &old_contest).unwrap();
        let old_tests = destination.join("tests").join("A");
        fs::create_dir_all(&old_tests).unwrap();
        fs::write(old_tests.join("sample-1.in"), "old input\n").unwrap();
        fs::write(old_tests.join("sample-1.out"), "old output\n").unwrap();

        let staging = tempfile::Builder::new()
            .prefix(".atc-refresh-")
            .tempdir_in(&destination)
            .unwrap();
        let staging_path = staging.path().to_path_buf();
        let new_contest = Contest {
            contest_id: "abc466".to_string(),
            problems: vec![problem("B")],
        };
        save_metadata(staging.path(), &new_contest).unwrap();
        save_samples(
            staging.path(),
            &new_contest.problems[0],
            &[Sample {
                input: "new input\n".to_string(),
                output: "new output\n".to_string(),
            }],
        )
        .unwrap();

        let error = replace_refresh_data_with_hooks(
            &destination,
            staging,
            false,
            || {},
            || {},
            || {
                fs::write(
                    staging_path
                        .join("previous-tests")
                        .join("A")
                        .join("notes.txt"),
                    "late user data\n",
                )
                .unwrap();
            },
        )
        .expect_err("a changed previous generation must be retained instead of cleaned");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("recovery data kept at"));
        assert_eq!(
            fs::read_to_string(
                staging_path
                    .join("previous-tests")
                    .join("A")
                    .join("notes.txt")
            )
            .unwrap(),
            "late user data\n"
        );
        assert_eq!(
            fs::read_to_string(
                staging_path
                    .join("previous-tests")
                    .join("A")
                    .join("sample-1.in")
            )
            .unwrap(),
            "old input\n"
        );
        assert_eq!(
            fs::read_to_string(destination.join("tests").join("B").join("sample-1.in")).unwrap(),
            "new input\n"
        );
        assert_eq!(load_metadata(&destination).unwrap(), new_contest);
    }
}
