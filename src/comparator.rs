#[derive(Debug, PartialEq, Eq)]
pub enum ComparisonResult {
    Accepted,
    WrongAnswer,
}

pub fn compare(expected: &str, actual: &str) -> ComparisonResult {
    if normalize(expected) == normalize(actual) {
        ComparisonResult::Accepted
    } else {
        ComparisonResult::WrongAnswer
    }
}

fn normalize(text: &str) -> Vec<String> {
    let text = text.replace("\r\n", "\n");

    text.split_terminator('\n')
        .map(|line| line.trim_end().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_different_line_endings() {
        assert_eq!(
            compare("1 2\n3 4\n", "1 2\r\n3 4\r\n"),
            ComparisonResult::Accepted
        );
    }

    #[test]
    fn accepts_missing_final_newline() {
        assert_eq!(compare("3\n", "3"), ComparisonResult::Accepted);
    }

    #[test]
    fn accepts_trailing_spaces() {
        assert_eq!(
            compare("1 2\n3 4\n", "1 2   \n3 4\t\n"),
            ComparisonResult::Accepted
        );
    }

    #[test]
    fn rejects_newline_as_space() {
        assert_eq!(
            compare("1 2\n3 4\n", "1 2 3 4\n"),
            ComparisonResult::WrongAnswer
        );
    }

    #[test]
    fn rejects_different_output() {
        assert_eq!(compare("10\n", "11\n"), ComparisonResult::WrongAnswer);
    }

    #[test]
    fn rejects_extra_trailing_blank_line() {
        assert_eq!(compare("1\n", "1\n\n"), ComparisonResult::WrongAnswer);
        assert_eq!(compare("", "\n"), ComparisonResult::WrongAnswer);
    }

    #[test]
    fn preserves_blank_lines_while_ignoring_their_trailing_whitespace() {
        assert_eq!(
            compare("1\n\n2\n", "1\r\n \t\r\n2"),
            ComparisonResult::Accepted
        );
        assert_eq!(compare("1\n\n2\n", "1\n2\n"), ComparisonResult::WrongAnswer);
    }

    #[test]
    fn rejects_leading_and_internal_whitespace_differences() {
        assert_eq!(compare("1 2\n", " 1 2\n"), ComparisonResult::WrongAnswer);
        assert_eq!(compare("1 2\n", "1  2\n"), ComparisonResult::WrongAnswer);
    }
}
