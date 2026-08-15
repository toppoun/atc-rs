use crate::auth;
use crate::error::AppError;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

pub(crate) fn login() -> Result<(), AppError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    login_with(&mut stdin.lock(), &mut stdout.lock(), auth::save_cookie)
}

fn login_with(
    input: &mut impl BufRead,
    output: &mut impl Write,
    save: impl FnOnce(&str) -> io::Result<PathBuf>,
) -> Result<(), AppError> {
    write!(output, "Paste AtCoder cookie: ")?;
    output.flush()?;

    let mut cookie = String::new();
    input.read_line(&mut cookie)?;
    let cookie = cookie.trim();
    if cookie.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "cookie must not be empty").into());
    }

    let path = save(cookie)?;
    writeln!(output, "Cookie saved to {}", path.display())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn login_prints_only_the_saved_path_after_success() {
        let secret = "REVEL_SESSION=do-not-print";
        let mut input = Cursor::new(format!("{secret}\n"));
        let mut output = Vec::new();
        let path = PathBuf::from("state/cookie");

        login_with(&mut input, &mut output, |cookie| {
            assert_eq!(cookie, secret);
            Ok(path.clone())
        })
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains(secret));
        assert!(output.contains("Cookie saved to state/cookie"));
    }

    #[test]
    fn login_eof_and_empty_input_do_not_report_success() {
        for input in [Vec::new(), b"  \r\n".to_vec()] {
            let mut input = Cursor::new(input);
            let mut output = Vec::new();

            let result = login_with(&mut input, &mut output, |_| {
                panic!("empty input must not reach cookie storage")
            });

            assert!(result.is_err());
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("Paste AtCoder cookie"));
            assert!(!output.contains("Cookie saved"));
        }
    }

    #[test]
    fn login_does_not_report_success_when_saving_fails() {
        let secret = "REVEL_SESSION=secret";
        let mut input = Cursor::new(format!("{secret}\n"));
        let mut output = Vec::new();

        let result = login_with(&mut input, &mut output, |_| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        });

        assert!(result.is_err());
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains(secret));
        assert!(!output.contains("Cookie saved"));
    }
}
