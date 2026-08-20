use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq)]
pub struct Contest {
    pub contest_id: String,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Problem {
    pub index: String,
    pub title: String,
    pub task_id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub input: String,
    pub output: String,
}
