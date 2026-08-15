use crate::atcoder::{self, AtCoderError, AuthenticationStatus};
use crate::error::AppError;
use crate::paths;

use std::io::{self, Write};
use std::path::PathBuf;

pub(crate) fn login() -> Result<(), AppError> {
    let stdout = io::stdout();

    login_with(&mut stdout.lock(), atcoder::authentication_status, || {
        Ok(paths::cookie_file()?)
    })
}

fn login_with(
    output: &mut impl Write,
    check: impl FnOnce() -> Result<AuthenticationStatus, AtCoderError>,
    cookie_file: impl FnOnce() -> Result<PathBuf, AppError>,
) -> Result<(), AppError> {
    match check()? {
        AuthenticationStatus::Authenticated => {
            writeln!(output, "Authenticated.")?;
        }

        AuthenticationStatus::NotConfigured => {
            let path = cookie_file()?;

            writeln!(output, "Authentication cookie is not configured.")?;
            writeln!(output)?;
            writeln!(output, "Create:")?;
            writeln!(output, "{}", path.display())?;
            writeln!(output)?;
            writeln!(output, "with:")?;
            writeln!(output, "REVEL_SESSION=<value>")?;
        }

        AuthenticationStatus::Unauthenticated => {
            let path = cookie_file()?;

            writeln!(output, "Authentication failed.")?;
            writeln!(output, "Update REVEL_SESSION in:")?;
            writeln!(output, "{}", path.display())?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticated_session_reports_success() {
        let mut output = Vec::new();

        login_with(
            &mut output,
            || Ok(AuthenticationStatus::Authenticated),
            || panic!("authenticated status must not need cookie path"),
        )
        .unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), "Authenticated.\n");
    }

    #[test]
    fn missing_cookie_reports_how_to_configure_it() {
        let mut output = Vec::new();

        login_with(
            &mut output,
            || Ok(AuthenticationStatus::NotConfigured),
            || Ok(PathBuf::from("state/cookie")),
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("Authentication cookie is not configured."));
        assert!(output.contains("state/cookie"));
        assert!(output.contains("REVEL_SESSION=<value>"));
    }

    #[test]
    fn unauthenticated_session_reports_cookie_path() {
        let mut output = Vec::new();

        login_with(
            &mut output,
            || Ok(AuthenticationStatus::Unauthenticated),
            || Ok(PathBuf::from("state/cookie")),
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("Authentication failed."));
        assert!(output.contains("state/cookie"));
    }
}
