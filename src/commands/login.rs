use crate::auth;
use crate::error::AppError;
use crate::paths;
use std::io::{self, Write};

pub(crate) fn login() -> Result<(), AppError> {
    print!("Paste AtCoder cookie: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    auth::save_cookie(input.trim())?;

    let path = paths::cookie_file().map_err(io::Error::other)?;
    println!("Cookie saved to {}", path.display());

    Ok(())
}
