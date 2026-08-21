use std::fmt;

const PREFIX: &[u8] = b"\x1b[<";
const MAX_SEQUENCE_LEN: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MouseKind {
    Down,
    Drag,
    Up,
}

impl fmt::Display for MouseKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Down => formatter.write_str("DOWN"),
            Self::Drag => formatter.write_str("DRAG"),
            Self::Up => formatter.write_str("UP"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MouseReport {
    pub(crate) kind: MouseKind,
    pub(crate) raw_x: u32,
    pub(crate) raw_y: u32,
    pub(crate) raw: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct SgrMouseParser {
    pending: Vec<u8>,
}

impl SgrMouseParser {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<MouseReport> {
        self.pending.extend_from_slice(bytes);
        let mut reports = Vec::new();

        loop {
            let Some(start) = find_subslice(&self.pending, PREFIX, 0) else {
                self.retain_possible_prefix();
                break;
            };
            if start != 0 {
                self.pending.drain(..start);
            }

            let terminator = self.pending[PREFIX.len()..]
                .iter()
                .position(|byte| matches!(byte, b'M' | b'm'))
                .map(|position| position + PREFIX.len());
            let nested = find_subslice(&self.pending, PREFIX, PREFIX.len());

            if let Some(nested) = nested
                && terminator.is_none_or(|terminator| nested < terminator)
            {
                self.pending.drain(..nested);
                continue;
            }

            let Some(end) = terminator else {
                if self.pending.len() > MAX_SEQUENCE_LEN {
                    self.pending.drain(..PREFIX.len());
                    continue;
                }
                break;
            };

            let raw: Vec<u8> = self.pending.drain(..=end).collect();
            if let Some(report) = parse_complete(&raw) {
                reports.push(report);
            }
        }

        reports
    }

    fn retain_possible_prefix(&mut self) {
        if self.pending.ends_with(b"\x1b[") {
            self.pending.drain(..self.pending.len() - 2);
        } else if self.pending.ends_with(b"\x1b") {
            self.pending.drain(..self.pending.len() - 1);
        } else {
            self.pending.clear();
        }
    }
}

fn parse_complete(raw: &[u8]) -> Option<MouseReport> {
    if !raw.starts_with(PREFIX) {
        return None;
    }
    let final_byte = *raw.last()?;
    if !matches!(final_byte, b'M' | b'm') {
        return None;
    }

    let body = std::str::from_utf8(&raw[PREFIX.len()..raw.len() - 1]).ok()?;
    let mut fields = body.split(';');
    let cb: u32 = fields.next()?.parse().ok()?;
    let raw_x: u32 = fields.next()?.parse().ok()?;
    let raw_y: u32 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }

    // Bits 6 and 7 identify wheel/extended-button reports. The low two bits
    // identify the left button when zero; bit 5 marks button motion.
    if cb & (64 | 128) != 0 || cb & 0b11 != 0 {
        return None;
    }
    let kind = match (final_byte, cb & 32 != 0) {
        (b'M', false) => MouseKind::Down,
        (b'M', true) => MouseKind::Drag,
        (b'm', _) => MouseKind::Up,
        _ => return None,
    };

    Some(MouseReport {
        kind,
        raw_x,
        raw_y,
        raw: raw.to_vec(),
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| position + from)
}

#[cfg(test)]
mod tests {
    use super::{MouseKind, SgrMouseParser};

    #[test]
    fn parses_left_down() {
        let reports = SgrMouseParser::default().feed(b"\x1b[<0;800;401M");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].kind, MouseKind::Down);
        assert_eq!((reports[0].raw_x, reports[0].raw_y), (800, 401));
        assert_eq!(reports[0].raw, b"\x1b[<0;800;401M");
    }

    #[test]
    fn parses_left_drag() {
        let reports = SgrMouseParser::default().feed(b"\x1b[<32;800;402M");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].kind, MouseKind::Drag);
    }

    #[test]
    fn parses_left_up() {
        let reports = SgrMouseParser::default().feed(b"\x1b[<0;800;404m");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].kind, MouseKind::Up);
    }

    #[test]
    fn parses_multiple_events_in_one_stream() {
        let reports = SgrMouseParser::default().feed(b"noise\x1b[<0;2;3M\x1b[<32;2;4M\x1b[<0;2;4m");
        assert_eq!(reports.len(), 3);
        assert_eq!(reports[0].kind, MouseKind::Down);
        assert_eq!(reports[1].kind, MouseKind::Drag);
        assert_eq!(reports[2].kind, MouseKind::Up);
    }

    #[test]
    fn handles_sequence_split_across_chunks() {
        let mut parser = SgrMouseParser::default();
        assert!(parser.feed(b"prefix\x1b[").is_empty());
        assert!(parser.feed(b"<32;10;").is_empty());
        let reports = parser.feed(b"11M");
        assert_eq!(reports.len(), 1);
        assert_eq!((reports[0].raw_x, reports[0].raw_y), (10, 11));
    }

    #[test]
    fn preserves_zero_coordinates() {
        let reports = SgrMouseParser::default().feed(b"\x1b[<0;0;0M");
        assert_eq!((reports[0].raw_x, reports[0].raw_y), (0, 0));
    }

    #[test]
    fn ignores_malformed_and_retains_incomplete_input() {
        let mut parser = SgrMouseParser::default();
        assert!(parser.feed(b"\x1b[<bad;1;2M").is_empty());
        assert!(parser.feed(b"\x1b[<0;7").is_empty());
        let reports = parser.feed(b";9M");
        assert_eq!(reports.len(), 1);
        assert_eq!((reports[0].raw_x, reports[0].raw_y), (7, 9));
    }
}
