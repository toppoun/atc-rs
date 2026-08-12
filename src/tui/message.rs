use std::io;
use std::path::PathBuf;

use crate::language::Language;

#[derive(Debug)]
pub enum Message {
    SourceChanged {
        problem: usize,
        path: PathBuf,
        language: Language,
    },

    WatcherFailed(io::Error),
}
