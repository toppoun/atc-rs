use std::path::{Path, PathBuf};

use crate::language::Language;
use crate::model::Contest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WatchedSource {
    pub problem: usize,
    pub path: PathBuf,
    pub language: Language,
}

pub(super) fn build_watched_sources(
    destination: &Path,
    contest: &Contest,
) -> std::io::Result<Vec<WatchedSource>> {
    let mut sources = Vec::new();

    for (problem_index, problem) in contest.problems.iter().enumerate() {
        for language in [Language::Cpp, Language::Python] {
            sources.push(WatchedSource {
                problem: problem_index,
                path: crate::workspace::source_file_path(destination, &problem.index, language)?,
                language,
            });
        }
    }

    Ok(sources)
}

pub(super) fn resolve_watched_source<'a>(
    sources: &'a [WatchedSource],
    path: &Path,
) -> Option<&'a WatchedSource> {
    sources.iter().find(|source| source.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Problem;

    fn problem(index: &str) -> Problem {
        Problem {
            index: index.to_string(),
            title: format!("Problem {index}"),
            task_id: format!("contest_{}", index.to_ascii_lowercase()),
            url: format!("https://example.invalid/{index}"),
            sample_count: 0,
        }
    }

    #[test]
    fn builds_cpp_and_python_sources_in_metadata_order() {
        let destination = Path::new("workspace");

        let contest = Contest {
            contest_id: "contest".to_string(),
            problems: vec![problem("A"), problem("D")],
        };

        let sources = build_watched_sources(destination, &contest).unwrap();

        assert_eq!(sources.len(), 4);

        assert_eq!(sources[0].problem, 0);
        assert_eq!(sources[0].path, destination.join("A.cpp"));
        assert_eq!(sources[0].language, Language::Cpp);

        assert_eq!(sources[1].problem, 0);
        assert_eq!(sources[1].path, destination.join("A.py"));
        assert_eq!(sources[1].language, Language::Python);

        assert_eq!(sources[2].problem, 1);
        assert_eq!(sources[2].path, destination.join("D.cpp"));
        assert_eq!(sources[2].language, Language::Cpp);

        assert_eq!(sources[3].problem, 1);
        assert_eq!(sources[3].path, destination.join("D.py"));
        assert_eq!(sources[3].language, Language::Python);
    }
}
