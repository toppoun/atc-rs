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

fn normalize(text: &str) -> String {
    let text = text.replace("\r\n", "\n");

    let mut lines: Vec<&str> = text.lines().map(|line| line.trim_end()).collect();

    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
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
}
