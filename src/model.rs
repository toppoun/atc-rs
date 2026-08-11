use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq)]
pub struct Contest {
    pub contest_id: String,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Problem {
    pub index: String,
    pub title: String,
    pub task_id: String,
    pub url: String,
}

#[derive(Debug)]
pub struct Sample {
    pub input: String,
    pub output: String,
}
