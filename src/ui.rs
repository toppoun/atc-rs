use std::path::Path;

pub enum Event<'a> {
    ContestFetching {
        contest_id: &'a str,
    },
    ContestFetched {
        contest_id: &'a str,
        problems: usize,
    },
    ProblemFetching {
        index: &'a str,
        current: usize,
        total: usize,
    },
    ProblemFetched {
        index: &'a str,
        samples: usize,
    },
    ProblemFetchFailed {
        index: &'a str,
        error: &'a str,
    },
    WorkspaceCreated {
        destination: &'a Path,
    },
    WorkspaceRefreshed {
        destination: &'a Path,
    },
}

pub trait Reporter {
    fn report(&mut self, event: Event<'_>);
}

pub struct TerminalReporter;

impl Reporter for TerminalReporter {
    fn report(&mut self, event: Event<'_>) {
        match event {
            Event::ContestFetching { contest_id } => {
                eprintln!("Fetching contest {contest_id}...");
            }

            Event::ContestFetched {
                contest_id,
                problems,
            } => {
                eprintln!("Found {problems} problems in {contest_id}");
            }

            Event::ProblemFetching {
                index,
                current,
                total,
            } => {
                eprintln!("[{current}/{total}] Fetching {index}...");
            }

            Event::ProblemFetched { index, samples } => {
                eprintln!("  {index}: {samples} samples");
            }

            Event::ProblemFetchFailed { index, error } => {
                eprintln!("  warning: {index}: {error}");
            }

            Event::WorkspaceCreated { destination } => {
                eprintln!("Created {}", destination.display());
            }

            Event::WorkspaceRefreshed { destination } => {
                eprintln!("Refreshed {}", destination.display());
            }
        }
    }
}

#[cfg(test)]
pub struct NullReporter;

#[cfg(test)]
impl Reporter for NullReporter {
    fn report(&mut self, _: Event<'_>) {}
}
