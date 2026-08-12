use crate::error::AppError;
use std::path::PathBuf;

const DEBUG_HPP: &str = include_str!("../assets/debug.hpp");

pub fn materialize_debug_header() -> Result<PathBuf, AppError> {
    let include_dir = crate::paths::debug_include_dir()?;
    let atc_dir = include_dir.join("atc");
    let header = atc_dir.join("debug.hpp");

    std::fs::create_dir_all(&atc_dir)?;
    std::fs::write(&header, DEBUG_HPP)?;

    Ok(include_dir)
}
