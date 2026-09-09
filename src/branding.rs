pub(crate) const ASCII_LOGO: &str = r#"

 █████╗ ████████╗ ██████╗
██╔══██╗╚══██╔══╝██╔════╝
███████║   ██║   ██║     
██╔══██║   ██║   ██║     
██║  ██║   ██║   ╚██████╗
╚═╝  ╚═╝   ╚═╝    ╚═════╝"#;

pub(crate) fn ascii_logo_lines() -> impl Iterator<Item = &'static str> {
    ASCII_LOGO.lines().skip_while(|line| line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_logo_has_one_leading_help_separator_and_six_visible_rows() {
        assert!(ASCII_LOGO.starts_with('\n'));
        assert_eq!(ascii_logo_lines().count(), 6);
        assert!(ascii_logo_lines().all(|line| !line.is_empty()));
    }
}
