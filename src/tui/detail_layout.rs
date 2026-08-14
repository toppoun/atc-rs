use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use ratatui::text::{Line, Text};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::detail::{DetailDocument, DetailSnapshot, DetailTextSource};

pub(super) const DETAIL_CHUNK_LINES: usize = 256;
const MATERIALIZED_CHUNK_CACHE_SIZE: usize = 3;
const GIANT_LOGICAL_LINE_BYTE_THRESHOLD: usize = 64 * 1024;
const NORMAL_BLOCK_MAX_RAW_BYTES: usize = GIANT_LOGICAL_LINE_BYTE_THRESHOLD;
// One demand-driven advance can cross one maximum-sized normal line and then
// reach the boundary that publishes its containing block. The builder still
// stops earlier as soon as it publishes a structural unit.
const FOREGROUND_STRUCTURE_SCAN_BUDGET: usize = 2 * GIANT_LOGICAL_LINE_BYTE_THRESHOLD;
pub(super) const BACKGROUND_STRUCTURE_SCAN_BUDGET: usize = GIANT_LOGICAL_LINE_BYTE_THRESHOLD;
const GIANT_LINE_PAGE_ROWS: usize = 128;
const GIANT_LINE_PAGE_CACHE_SIZE: usize = 3;
const GIANT_LINE_WINDOW_MIN_BYTES: usize = 4 * 1024;
const LAZY_DETAIL_BYTE_THRESHOLD: usize = 64 * 1024;
const LAZY_DETAIL_LINE_THRESHOLD: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    Eager,
    Lazy,
}

// Byte position in the virtual concatenation of all detail segments. Segment
// boundaries contribute no bytes and are not logical-line boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct RawOffset(usize);

#[derive(Debug, Default, PartialEq, Eq)]
struct DetailRawMap {
    segment_starts: Vec<RawOffset>,
    total_len: RawOffset,
}

impl DetailRawMap {
    fn new(document: &(impl DetailTextSource + ?Sized)) -> Self {
        let mut segment_starts = Vec::with_capacity(document.segment_count().saturating_add(1));
        let mut total_len = 0usize;

        for segment_index in 0..document.segment_count() {
            segment_starts.push(RawOffset(total_len));
            let segment_len = document
                .segment_text(segment_index)
                .expect("detail segment index must exist")
                .len();
            total_len = total_len
                .checked_add(segment_len)
                .expect("detail document byte length must fit in usize");
        }
        segment_starts.push(RawOffset(total_len));

        Self {
            segment_starts,
            total_len: RawOffset(total_len),
        }
    }

    fn total_len(&self) -> RawOffset {
        self.total_len
    }

    fn segment_position(&self, offset: RawOffset) -> Option<(usize, usize)> {
        if offset >= self.total_len {
            return None;
        }

        let segment = self
            .segment_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        Some((
            segment,
            offset.0.saturating_sub(self.segment_starts[segment].0),
        ))
    }

    fn bytes_from<'a>(
        &self,
        document: &'a (impl DetailTextSource + ?Sized),
        offset: RawOffset,
    ) -> &'a [u8] {
        let Some((segment, local_offset)) = self.segment_position(offset) else {
            return &[];
        };
        &document
            .segment_text(segment)
            .expect("raw detail position must reference an existing segment")
            .as_bytes()[local_offset..]
    }

    fn bounded_text<'a>(
        &self,
        document: &'a (impl DetailTextSource + ?Sized),
        range: Range<RawOffset>,
    ) -> Cow<'a, str> {
        let Some((start_segment, start, end_segment, end)) = self.segment_range(document, &range)
        else {
            return Cow::Borrowed("");
        };

        if start_segment == end_segment {
            let text = document
                .segment_text(start_segment)
                .expect("raw detail range must reference an existing segment");
            return Cow::Borrowed(
                text.get(start..end)
                    .expect("raw detail range must use UTF-8 boundaries"),
            );
        }

        let mut joined = String::with_capacity(range.end.0.saturating_sub(range.start.0));
        for segment in start_segment..=end_segment {
            let text = document
                .segment_text(segment)
                .expect("raw detail range must reference an existing segment");
            let local_start = if segment == start_segment { start } else { 0 };
            let local_end = if segment == end_segment {
                end
            } else {
                text.len()
            };
            joined.push_str(
                text.get(local_start..local_end)
                    .expect("raw detail range must use UTF-8 boundaries"),
            );
        }
        Cow::Owned(joined)
    }

    fn fragments<'a>(
        &self,
        document: &'a (impl DetailTextSource + ?Sized),
        range: Range<RawOffset>,
    ) -> Vec<&'a str> {
        let Some((start_segment, start, end_segment, end)) = self.segment_range(document, &range)
        else {
            return Vec::new();
        };

        let mut fragments =
            Vec::with_capacity(end_segment.saturating_sub(start_segment).saturating_add(1));
        for segment in start_segment..=end_segment {
            let text = document
                .segment_text(segment)
                .expect("raw detail range must reference an existing segment");
            let local_start = if segment == start_segment { start } else { 0 };
            let local_end = if segment == end_segment {
                end
            } else {
                text.len()
            };
            fragments.push(
                text.get(local_start..local_end)
                    .expect("raw detail range must use UTF-8 boundaries"),
            );
        }
        fragments
    }

    fn segment_range(
        &self,
        document: &(impl DetailTextSource + ?Sized),
        range: &Range<RawOffset>,
    ) -> Option<(usize, usize, usize, usize)> {
        assert!(range.start <= range.end);
        assert!(range.end <= self.total_len);
        if range.start == range.end {
            return None;
        }

        let (start_segment, start) = self
            .segment_position(range.start)
            .expect("non-empty raw range must start inside the document");
        let last_byte = RawOffset(range.end.0.saturating_sub(1));
        let (end_segment, _) = self
            .segment_position(last_byte)
            .expect("non-empty raw range must end inside the document");
        let end = range
            .end
            .0
            .saturating_sub(self.segment_starts[end_segment].0);

        debug_assert!(
            document
                .segment_text(start_segment)
                .is_some_and(|text| text.is_char_boundary(start))
        );
        debug_assert!(
            document
                .segment_text(end_segment)
                .is_some_and(|text| text.is_char_boundary(end))
        );
        Some((start_segment, start, end_segment, end))
    }

    fn is_char_boundary(
        &self,
        document: &(impl DetailTextSource + ?Sized),
        offset: RawOffset,
    ) -> bool {
        if offset == self.total_len {
            return true;
        }
        self.segment_position(offset)
            .is_some_and(|(segment, local)| {
                document
                    .segment_text(segment)
                    .is_some_and(|text| text.is_char_boundary(local))
            })
    }

    fn line_probe<'a>(
        &self,
        document: &'a (impl DetailTextSource + ?Sized),
        start: RawOffset,
        known_end: Option<RawOffset>,
        max_bytes: usize,
    ) -> RawLineProbe<'a> {
        assert!(start <= self.total_len);
        if let Some(end) = known_end {
            assert!(start <= end && end <= self.total_len);
        }

        let hard_end = known_end.unwrap_or(self.total_len);
        let mut end = RawOffset(start.0.saturating_add(max_bytes.max(1)).min(hard_end.0));
        while end > start && !self.is_char_boundary(document, end) {
            end.0 -= 1;
        }
        if end == start && end < hard_end {
            while end < hard_end {
                end.0 += 1;
                if self.is_char_boundary(document, end) {
                    break;
                }
            }
        }

        let inspected_bytes = end.0.saturating_sub(start.0);
        let text = self.bounded_text(document, start..end);
        if known_end.is_none()
            && let Some(newline) = text.as_bytes().iter().position(|byte| *byte == b'\n')
        {
            let content_end = RawOffset(start.0.saturating_add(newline));
            return RawLineProbe {
                text: truncate_window(text, newline),
                inspected_bytes,
                reaches_end: true,
                discovered_end: Some(GiantLineEnd {
                    content_end,
                    next_line_start: RawOffset(content_end.0.saturating_add(1)),
                    eof: false,
                }),
            };
        }

        let reaches_end = end == hard_end;
        let discovered_end =
            (known_end.is_none() && end == self.total_len).then_some(GiantLineEnd {
                content_end: self.total_len,
                next_line_start: self.total_len,
                eof: true,
            });
        RawLineProbe {
            text,
            inspected_bytes,
            reaches_end,
            discovered_end,
        }
    }
}

struct RawLineProbe<'a> {
    text: Cow<'a, str>,
    inspected_bytes: usize,
    reaches_end: bool,
    discovered_end: Option<GiantLineEnd>,
}

// A normal block owns all newline delimiters after its represented lines. Its
// raw range ends at the next logical-line start (or EOF), and
// logical_line_count disambiguates a trailing empty line from a block boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalBlock {
    raw_range: Range<RawOffset>,
    logical_line_count: usize,
}

// Giant ranges contain only logical-line content; a terminating newline is not
// part of the unit and is never passed to the E1 wrapping engine.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GiantLineUnit {
    raw_start: RawOffset,
    raw_end: Option<RawOffset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StructuralUnit {
    Normal(NormalBlock),
    Giant(GiantLineUnit),
}

impl StructuralUnit {
    fn is_giant(&self) -> bool {
        matches!(self, Self::Giant(_))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DetailDocumentStructure {
    raw_map: DetailRawMap,
    logical_line_count: usize,
    units: Vec<StructuralUnit>,
    complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InProgressLineKind {
    Normal,
    Giant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DetailIndexAdvance {
    scanned_bytes: usize,
    finished: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GiantLineEnd {
    content_end: RawOffset,
    next_line_start: RawOffset,
    eof: bool,
}

#[derive(Debug)]
struct IncrementalDetailIndexBuilder {
    structure: DetailDocumentStructure,
    scan_position: RawOffset,
    current_line_start: RawOffset,
    current_line_kind: InProgressLineKind,
    pending_normal_start: RawOffset,
    pending_normal_line_count: usize,
    logical_line_count: usize,
    finished: bool,
}

impl IncrementalDetailIndexBuilder {
    fn new(document: &(impl DetailTextSource + ?Sized)) -> Self {
        Self {
            structure: DetailDocumentStructure {
                raw_map: DetailRawMap::new(document),
                logical_line_count: 0,
                units: Vec::new(),
                complete: false,
            },
            scan_position: RawOffset::default(),
            current_line_start: RawOffset::default(),
            current_line_kind: InProgressLineKind::Normal,
            pending_normal_start: RawOffset::default(),
            pending_normal_line_count: 0,
            logical_line_count: 0,
            finished: false,
        }
    }

    fn advance(
        &mut self,
        document: &(impl DetailTextSource + ?Sized),
        byte_budget: usize,
    ) -> DetailIndexAdvance {
        self.assert_document_identity(document);
        if self.finished {
            return DetailIndexAdvance {
                scanned_bytes: 0,
                finished: true,
            };
        }
        if self.current_line_kind == InProgressLineKind::Giant {
            return DetailIndexAdvance {
                scanned_bytes: 0,
                finished: false,
            };
        }

        let mut scanned_bytes = 0usize;
        let initial_units = self.structure.units.len();
        while scanned_bytes < byte_budget && self.scan_position < self.structure.raw_map.total_len()
        {
            let available = self
                .structure
                .raw_map
                .bytes_from(document, self.scan_position);
            debug_assert!(!available.is_empty());
            let remaining_budget = byte_budget.saturating_sub(scanned_bytes);
            let bytes_until_giant = GIANT_LOGICAL_LINE_BYTE_THRESHOLD.saturating_sub(
                self.scan_position
                    .0
                    .saturating_sub(self.current_line_start.0),
            );
            let scan_len = available.len().min(remaining_budget).min(bytes_until_giant);
            // The frontier may stop inside a UTF-8 code point. Structural
            // discovery only looks for ASCII newline bytes; published unit
            // ranges are created solely at newline/EOF boundaries.
            let candidate = &available[..scan_len];

            if let Some(relative_newline) = candidate.iter().position(|byte| *byte == b'\n') {
                let newline = RawOffset(self.scan_position.0.saturating_add(relative_newline));
                let consumed = relative_newline.saturating_add(1);
                let next_line_start = RawOffset(newline.0.saturating_add(1));

                self.finish_current_line(newline, next_line_start);
                self.scan_position = next_line_start;
                self.current_line_start = self.scan_position;
                self.current_line_kind = InProgressLineKind::Normal;
                scanned_bytes = scanned_bytes.saturating_add(consumed);
            } else {
                self.scan_position = RawOffset(self.scan_position.0.saturating_add(scan_len));
                scanned_bytes = scanned_bytes.saturating_add(scan_len);
                self.publish_open_giant_if_known();
            }

            if self.structure.units.len() > initial_units
                || self.current_line_kind == InProgressLineKind::Giant
            {
                break;
            }
        }

        if self.scan_position == self.structure.raw_map.total_len()
            && self.current_line_kind != InProgressLineKind::Giant
        {
            self.finish_at_eof();
        }

        DetailIndexAdvance {
            scanned_bytes,
            finished: self.finished,
        }
    }

    fn build_to_end(
        mut self,
        document: &(impl DetailTextSource + ?Sized),
    ) -> DetailDocumentStructure {
        while !self.finished {
            let progress = self.advance(document, usize::MAX);
            if self.current_line_kind == InProgressLineKind::Giant {
                self.resolve_open_giant_by_scanning(document);
            } else {
                debug_assert!(progress.finished || progress.scanned_bytes > 0);
            }
        }
        self.into_structure()
    }

    fn into_structure(self) -> DetailDocumentStructure {
        assert!(
            self.finished,
            "detail structure may only be taken after the document is complete"
        );
        self.structure
    }

    fn structure(&self) -> &DetailDocumentStructure {
        &self.structure
    }

    #[cfg(test)]
    fn current_line_is_known_giant(&self) -> bool {
        !self.finished && self.current_line_kind == InProgressLineKind::Giant
    }

    fn publish_open_giant_if_known(&mut self) {
        // This state is intentionally retained before the line end is known.
        // A later phase can publish an open giant unit without changing the
        // closed DetailDocumentStructure produced by build_to_end().
        if self
            .scan_position
            .0
            .saturating_sub(self.current_line_start.0)
            >= GIANT_LOGICAL_LINE_BYTE_THRESHOLD
        {
            self.flush_pending_normal(self.current_line_start);
            self.current_line_kind = InProgressLineKind::Giant;
            self.logical_line_count = self.logical_line_count.saturating_add(1);
            self.structure.logical_line_count = self.logical_line_count;
            self.structure
                .units
                .push(StructuralUnit::Giant(GiantLineUnit {
                    raw_start: self.current_line_start,
                    raw_end: None,
                }));
        }
    }

    fn finish_current_line(&mut self, content_end: RawOffset, next_line_start: RawOffset) {
        debug_assert_eq!(self.current_line_kind, InProgressLineKind::Normal);
        debug_assert!(
            content_end.0.saturating_sub(self.current_line_start.0)
                < GIANT_LOGICAL_LINE_BYTE_THRESHOLD
        );
        self.logical_line_count = self.logical_line_count.saturating_add(1);
        self.structure.logical_line_count = self.logical_line_count;

        let candidate_raw_bytes = next_line_start
            .0
            .saturating_sub(self.pending_normal_start.0);
        if self.pending_normal_line_count > 0 && candidate_raw_bytes > NORMAL_BLOCK_MAX_RAW_BYTES {
            self.flush_pending_normal(self.current_line_start);
        }

        self.pending_normal_line_count = self.pending_normal_line_count.saturating_add(1);
        if self.pending_normal_line_count == DETAIL_CHUNK_LINES {
            self.flush_pending_normal(next_line_start);
        }
    }

    fn flush_pending_normal(&mut self, end: RawOffset) {
        if self.pending_normal_line_count > 0 {
            debug_assert!(
                end.0.saturating_sub(self.pending_normal_start.0) <= NORMAL_BLOCK_MAX_RAW_BYTES
            );
            self.structure
                .units
                .push(StructuralUnit::Normal(NormalBlock {
                    raw_range: self.pending_normal_start..end,
                    logical_line_count: self.pending_normal_line_count,
                }));
        }
        self.pending_normal_start = end;
        self.pending_normal_line_count = 0;
    }

    fn finish_at_eof(&mut self) {
        if self.finished {
            return;
        }

        let eof = self.structure.raw_map.total_len();
        self.finish_current_line(eof, eof);
        self.flush_pending_normal(eof);
        self.structure.complete = true;
        self.finished = true;
    }

    fn resolve_open_giant(&mut self, end: GiantLineEnd) {
        assert_eq!(self.current_line_kind, InProgressLineKind::Giant);
        assert!(end.content_end >= self.scan_position);
        assert!(end.next_line_start >= end.content_end);
        assert!(end.next_line_start <= self.structure.raw_map.total_len());
        let Some(StructuralUnit::Giant(line)) = self.structure.units.last_mut() else {
            panic!("open giant must be the last published structural unit");
        };
        assert_eq!(line.raw_start, self.current_line_start);
        assert!(line.raw_end.is_none());
        line.raw_end = Some(end.content_end);

        self.scan_position = end.next_line_start;
        if end.eof {
            assert_eq!(end.content_end, self.structure.raw_map.total_len());
            self.structure.complete = true;
            self.finished = true;
            return;
        }

        self.current_line_start = end.next_line_start;
        self.pending_normal_start = end.next_line_start;
        self.current_line_kind = InProgressLineKind::Normal;
    }

    fn resolve_open_giant_by_scanning(&mut self, document: &(impl DetailTextSource + ?Sized)) {
        while !self.finished && self.current_line_kind == InProgressLineKind::Giant {
            let progress = self.advance_open_giant(document, usize::MAX);
            debug_assert!(progress.finished || progress.scanned_bytes > 0);
        }
    }

    fn advance_open_giant(
        &mut self,
        document: &(impl DetailTextSource + ?Sized),
        byte_budget: usize,
    ) -> DetailIndexAdvance {
        self.assert_document_identity(document);
        assert_eq!(self.current_line_kind, InProgressLineKind::Giant);

        let mut scanned_bytes = 0usize;
        while scanned_bytes < byte_budget && self.scan_position < self.structure.raw_map.total_len()
        {
            let available = self
                .structure
                .raw_map
                .bytes_from(document, self.scan_position);
            debug_assert!(!available.is_empty());
            let scan_len = available
                .len()
                .min(byte_budget.saturating_sub(scanned_bytes));
            let candidate = &available[..scan_len];

            if let Some(relative) = candidate.iter().position(|byte| *byte == b'\n') {
                let content_end = RawOffset(self.scan_position.0.saturating_add(relative));
                let consumed = relative.saturating_add(1);
                self.resolve_open_giant(GiantLineEnd {
                    content_end,
                    next_line_start: RawOffset(content_end.0.saturating_add(1)),
                    eof: false,
                });
                scanned_bytes = scanned_bytes.saturating_add(consumed);
                return DetailIndexAdvance {
                    scanned_bytes,
                    finished: self.finished,
                };
            }

            self.scan_position = RawOffset(self.scan_position.0.saturating_add(scan_len));
            scanned_bytes = scanned_bytes.saturating_add(scan_len);
        }

        if self.scan_position == self.structure.raw_map.total_len() {
            let eof = self.structure.raw_map.total_len();
            self.resolve_open_giant(GiantLineEnd {
                content_end: eof,
                next_line_start: eof,
                eof: true,
            });
        }

        DetailIndexAdvance {
            scanned_bytes,
            finished: self.finished,
        }
    }

    fn assert_document_identity(&self, document: &(impl DetailTextSource + ?Sized)) {
        assert_eq!(
            self.structure.raw_map,
            DetailRawMap::new(document),
            "detail structure builder used with a different document shape"
        );
    }
}

impl DetailDocumentStructure {
    #[cfg(test)]
    pub(super) fn is_complete(&self) -> bool {
        self.complete
    }

    pub(super) fn len(&self) -> usize {
        self.logical_line_count
    }

    pub(super) fn chunk_count(&self) -> usize {
        self.units.len()
    }

    pub(super) fn count_chunks(
        &self,
        document: &impl DetailTextSource,
        width: usize,
        mut is_cancelled: impl FnMut() -> bool,
    ) -> Option<Vec<usize>> {
        debug_assert!(self.complete);
        let mut counts = Vec::with_capacity(self.chunk_count());

        for unit in &self.units {
            if is_cancelled() {
                return None;
            }

            let visual_lines = match unit {
                StructuralUnit::Normal(block) => {
                    let text = self.raw_map.bounded_text(document, block.raw_range.clone());
                    let mut sink = CountSink::default();
                    if !wrap_normal_block(
                        &text,
                        block.logical_line_count,
                        width,
                        &mut sink,
                        &mut is_cancelled,
                    ) {
                        return None;
                    }
                    sink.lines
                }
                StructuralUnit::Giant(line) => {
                    let end = line
                        .raw_end
                        .expect("background count requires a closed giant line");
                    let fragments = self.raw_map.fragments(document, line.raw_start..end);
                    count_giant_logical_line_fragments(&fragments, width, &mut is_cancelled)?
                }
            };

            counts.push(visual_lines);
        }

        Some(counts)
    }
}

pub(crate) type DetailDocumentGeneration = u64;
pub(crate) type DetailLayoutGeneration = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailDocumentIdentity {
    pub(super) generation: DetailDocumentGeneration,
    pub(super) revision: u64,
    pub(super) segment_lengths: Vec<usize>,
}

#[derive(Debug)]
pub(crate) struct DetailStructureRequest {
    pub(super) identity: DetailDocumentIdentity,
    pub(super) snapshot: DetailSnapshot,
}

#[derive(Debug, Clone)]
pub(crate) struct DetailStructureResult {
    pub(super) identity: DetailDocumentIdentity,
    pub(super) structure: Arc<DetailDocumentStructure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailCountIdentity {
    pub(super) document_generation: DetailDocumentGeneration,
    pub(super) layout_generation: DetailLayoutGeneration,
    pub(super) revision: u64,
    pub(super) layout_width: usize,
    pub(super) segment_lengths: Vec<usize>,
    pub(super) chunk_count: usize,
}

#[derive(Debug)]
pub(crate) struct DetailCountRequest {
    pub(super) identity: DetailCountIdentity,
    pub(super) snapshot: DetailSnapshot,
    pub(super) structure: Arc<DetailDocumentStructure>,
}

#[derive(Debug, Clone)]
pub(crate) struct DetailCountResult {
    pub(super) identity: DetailCountIdentity,
    pub(super) chunk_visual_lines: Vec<usize>,
}

#[derive(Debug)]
pub(crate) enum DetailAnalysisCommand {
    BuildStructure(DetailStructureRequest),
    Count(DetailCountRequest),
    Cancel {
        layout_generation: DetailLayoutGeneration,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAnalysisCommand {
    BuildStructure,
    Count,
    Cancel,
}

#[derive(Debug, Clone)]
pub(crate) enum DetailAnalysisResult {
    StructureReady(DetailStructureResult),
    Count(DetailCountResult),
}

#[derive(Debug)]
enum DetailStructureState {
    Building(IncrementalDetailIndexBuilder),
    Complete(Arc<DetailDocumentStructure>),
}

#[derive(Debug)]
struct ChunkMeta {
    unit_index: usize,
    visual_lines: Option<usize>,
    checkpoints: Vec<WrapCheckpoint>,
}

#[derive(Debug)]
struct MaterializedChunk {
    index: usize,
    lines: Vec<Line<'static>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WrapCheckpoint {
    visual_row: usize,
    raw_position: RawOffset,
}

#[derive(Debug)]
struct MaterializedGiantPage {
    chunk_index: usize,
    start_row: usize,
    lines: Vec<Line<'static>>,
}

#[derive(Debug, Default)]
pub(super) struct DetailLayout {
    revision: Option<u64>,
    detail_width: u16,
    mode: Option<LayoutMode>,
    segment_lengths: Vec<usize>,
    structure_state: Option<DetailStructureState>,
    chunks: Vec<ChunkMeta>,
    materialized_chunks: VecDeque<MaterializedChunk>,
    materialized_giant_pages: VecDeque<MaterializedGiantPage>,
    document_generation: DetailDocumentGeneration,
    layout_generation: DetailLayoutGeneration,
    pending_analysis_command: Option<PendingAnalysisCommand>,
    ready_analysis_command: Option<DetailAnalysisCommand>,

    #[cfg(test)]
    chunk_layout_operations: usize,
    #[cfg(test)]
    giant_scanned_bytes: usize,
    #[cfg(test)]
    giant_page_layout_operations: usize,
    #[cfg(test)]
    invalidations: usize,
    #[cfg(test)]
    document_structure_builds: usize,
    #[cfg(test)]
    foreground_structural_scanned_bytes: usize,
}

pub(super) struct DetailViewport {
    pub(super) text: Text<'static>,
    pub(super) max_scroll: Option<usize>,
    pub(super) effective_scroll: usize,
}

impl DetailLayout {
    pub(super) fn viewport(
        &mut self,
        document: &impl DetailTextSource,
        revision: u64,
        detail_width: u16,
        viewport_height: usize,
        requested_scroll: usize,
    ) -> DetailViewport {
        self.prepare(document, revision, detail_width);

        match self.mode.expect("detail layout must be prepared") {
            LayoutMode::Eager => {
                eager_viewport(document, detail_width, viewport_height, requested_scroll)
            }
            LayoutMode::Lazy => self.lazy_viewport(document, viewport_height, requested_scroll),
        }
    }

    pub(super) fn stage_analysis_command(&mut self, document: &DetailDocument<'_>) {
        if self.ready_analysis_command.is_some() {
            return;
        }

        let Some(pending) = self.pending_analysis_command.take() else {
            return;
        };

        self.ready_analysis_command = Some(match pending {
            PendingAnalysisCommand::BuildStructure => {
                DetailAnalysisCommand::BuildStructure(DetailStructureRequest {
                    identity: self.document_identity(),
                    snapshot: document.snapshot(),
                })
            }
            PendingAnalysisCommand::Count => {
                let structure = match self
                    .structure_state
                    .as_ref()
                    .expect("detail structure state must exist")
                {
                    DetailStructureState::Complete(structure) => Arc::clone(structure),
                    DetailStructureState::Building(_) => {
                        panic!("incomplete detail structure cannot stage exact count")
                    }
                };
                DetailAnalysisCommand::Count(DetailCountRequest {
                    identity: self.count_identity(),
                    snapshot: document.snapshot(),
                    structure,
                })
            }
            PendingAnalysisCommand::Cancel => DetailAnalysisCommand::Cancel {
                layout_generation: self.layout_generation,
            },
        });
    }

    pub(super) fn take_analysis_command(&mut self) -> Option<DetailAnalysisCommand> {
        self.ready_analysis_command.take()
    }

    pub(super) fn apply_analysis_result(&mut self, result: DetailAnalysisResult) -> bool {
        match result {
            DetailAnalysisResult::StructureReady(result) => self.apply_structure_result(result),
            DetailAnalysisResult::Count(result) => self.apply_count_result(result),
        }
    }

    pub(super) fn apply_count_result(&mut self, result: DetailCountResult) -> bool {
        if self.mode != Some(LayoutMode::Lazy)
            || result.identity != self.count_identity()
            || result.chunk_visual_lines.len() != self.chunks.len()
        {
            return false;
        }

        if self
            .chunks
            .iter()
            .zip(&result.chunk_visual_lines)
            .any(|(chunk, result)| chunk.visual_lines.is_some_and(|known| known != *result))
        {
            return false;
        }

        let mut changed = false;
        for (chunk, visual_lines) in self.chunks.iter_mut().zip(result.chunk_visual_lines) {
            if chunk.visual_lines.is_none() {
                chunk.visual_lines = Some(visual_lines);
                changed = true;
            }
        }

        changed
    }

    fn apply_structure_result(&mut self, result: DetailStructureResult) -> bool {
        if result.identity != self.document_identity()
            || !result.structure.complete
            || result.structure.raw_map != self.structure().raw_map
        {
            return false;
        }

        match self
            .structure_state
            .as_ref()
            .expect("detail structure state must exist")
        {
            DetailStructureState::Building(builder) => {
                if !structure_prefix_is_compatible(builder.structure(), &result.structure) {
                    return false;
                }
            }
            DetailStructureState::Complete(current) => {
                if **current != *result.structure {
                    return false;
                }
                return false;
            }
        }

        self.structure_state = Some(DetailStructureState::Complete(result.structure));
        self.append_discovered_chunks();
        self.pending_analysis_command = Some(PendingAnalysisCommand::Count);
        self.ready_analysis_command = None;
        true
    }

    fn prepare(&mut self, document: &impl DetailTextSource, revision: u64, detail_width: u16) {
        let segment_lengths = (0..document.segment_count())
            .map(|index| document.segment_text(index).map_or(0, str::len))
            .collect::<Vec<_>>();
        let document_changed =
            self.revision != Some(revision) || self.segment_lengths != segment_lengths;
        let width_changed = self.detail_width != detail_width;

        if !document_changed && !width_changed {
            return;
        }

        let previous_mode = self.mode;
        let preserve_pending_build = !document_changed
            && matches!(
                self.structure_state,
                Some(DetailStructureState::Building(_))
            );
        if document_changed {
            let raw_bytes = segment_lengths.iter().copied().sum::<usize>();
            let (structure_state, mode) = if raw_bytes >= LAZY_DETAIL_BYTE_THRESHOLD {
                (
                    DetailStructureState::Building(IncrementalDetailIndexBuilder::new(document)),
                    LayoutMode::Lazy,
                )
            } else {
                let structure = Arc::new(build_document_structure(document));
                let mode = if structure.len() >= LAZY_DETAIL_LINE_THRESHOLD {
                    LayoutMode::Lazy
                } else {
                    LayoutMode::Eager
                };
                (DetailStructureState::Complete(structure), mode)
            };

            self.revision = Some(revision);
            self.segment_lengths = segment_lengths;
            self.structure_state = Some(structure_state);
            self.mode = Some(mode);
            self.document_generation = self.document_generation.wrapping_add(1);

            #[cfg(test)]
            {
                self.document_structure_builds = self.document_structure_builds.saturating_add(1);
            }
        }

        self.detail_width = detail_width;
        self.chunks = match self.mode.expect("detail layout must have a document") {
            LayoutMode::Lazy => Vec::new(),
            LayoutMode::Eager => Vec::new(),
        };
        self.append_discovered_chunks();
        self.materialized_chunks.clear();
        self.materialized_giant_pages.clear();
        self.layout_generation = self.layout_generation.wrapping_add(1);

        if !preserve_pending_build {
            self.pending_analysis_command = if document_changed {
                match self.mode.expect("detail layout must have a mode") {
                    LayoutMode::Lazy if self.structure_is_complete() => {
                        Some(PendingAnalysisCommand::Count)
                    }
                    LayoutMode::Lazy => Some(PendingAnalysisCommand::BuildStructure),
                    LayoutMode::Eager if previous_mode == Some(LayoutMode::Lazy) => {
                        Some(PendingAnalysisCommand::Cancel)
                    }
                    LayoutMode::Eager => None,
                }
            } else {
                match self.mode.expect("detail layout must have a mode") {
                    LayoutMode::Lazy if self.structure_is_complete() => {
                        Some(PendingAnalysisCommand::Count)
                    }
                    LayoutMode::Lazy => None,
                    LayoutMode::Eager => None,
                }
            };
            self.ready_analysis_command = None;
        }

        #[cfg(test)]
        {
            self.chunk_layout_operations = 0;
            self.giant_scanned_bytes = 0;
            self.giant_page_layout_operations = 0;
            self.invalidations = self.invalidations.saturating_add(1);
            if document_changed {
                self.foreground_structural_scanned_bytes = 0;
            }
        }
    }

    fn structure(&self) -> &DetailDocumentStructure {
        match self
            .structure_state
            .as_ref()
            .expect("detail structure state must exist")
        {
            DetailStructureState::Building(builder) => builder.structure(),
            DetailStructureState::Complete(structure) => structure,
        }
    }

    fn structure_is_complete(&self) -> bool {
        matches!(
            self.structure_state,
            Some(DetailStructureState::Complete(_))
        )
    }

    fn append_discovered_chunks(&mut self) {
        if self.mode != Some(LayoutMode::Lazy) {
            return;
        }

        let new_chunks = self.structure().units[self.chunks.len()..]
            .iter()
            .enumerate()
            .map(|(offset, unit)| {
                let unit_index = self.chunks.len().saturating_add(offset);
                let checkpoints = match unit {
                    StructuralUnit::Giant(line) => vec![WrapCheckpoint {
                        visual_row: 0,
                        raw_position: line.raw_start,
                    }],
                    StructuralUnit::Normal(_) => Vec::new(),
                };
                ChunkMeta {
                    unit_index,
                    visual_lines: None,
                    checkpoints,
                }
            })
            .collect::<Vec<_>>();
        self.chunks.extend(new_chunks);
    }

    fn advance_structure(
        &mut self,
        document: &impl DetailTextSource,
        structural_budget: &mut usize,
    ) -> bool {
        if *structural_budget == 0 {
            return false;
        }
        let Some(DetailStructureState::Building(builder)) = self.structure_state.as_mut() else {
            return false;
        };
        let old_units = builder.structure().units.len();
        let progress = builder.advance(document, *structural_budget);
        *structural_budget = structural_budget.saturating_sub(progress.scanned_bytes);

        #[cfg(not(test))]
        let _ = progress.scanned_bytes;

        #[cfg(test)]
        {
            self.foreground_structural_scanned_bytes = self
                .foreground_structural_scanned_bytes
                .saturating_add(progress.scanned_bytes);
        }

        let appended = builder.structure().units.len() > old_units;
        let finished = builder.finished;
        self.append_discovered_chunks();

        if finished {
            self.promote_finished_structure();
        }

        appended || finished
    }

    fn resolve_open_giant(&mut self, end: GiantLineEnd) {
        let Some(DetailStructureState::Building(builder)) = self.structure_state.as_mut() else {
            return;
        };
        builder.resolve_open_giant(end);
        if builder.finished {
            self.promote_finished_structure();
        }
    }

    fn promote_finished_structure(&mut self) {
        let state = self
            .structure_state
            .take()
            .expect("finished detail builder state must exist");
        let DetailStructureState::Building(builder) = state else {
            unreachable!("only a building structure can become complete");
        };
        assert!(builder.finished);
        self.structure_state = Some(DetailStructureState::Complete(Arc::new(
            builder.into_structure(),
        )));
        self.pending_analysis_command = Some(PendingAnalysisCommand::Count);
    }

    #[cfg(test)]
    pub(super) fn complete_structure_for_test(&mut self, document: &DetailDocument<'_>) {
        while !self.structure_is_complete() {
            let builder = match self.structure_state.as_mut().unwrap() {
                DetailStructureState::Building(builder) => builder,
                DetailStructureState::Complete(_) => unreachable!(),
            };
            if builder.current_line_is_known_giant() {
                builder.resolve_open_giant_by_scanning(document);
            } else {
                builder.advance(document, FOREGROUND_STRUCTURE_SCAN_BUDGET);
            }
            self.append_discovered_chunks();
            if matches!(
                self.structure_state.as_ref(),
                Some(DetailStructureState::Building(
                    IncrementalDetailIndexBuilder { finished: true, .. }
                ))
            ) {
                self.promote_finished_structure();
            }
        }
    }

    fn lazy_viewport(
        &mut self,
        document: &impl DetailTextSource,
        viewport_height: usize,
        requested_scroll: usize,
    ) -> DetailViewport {
        let mut structural_budget = FOREGROUND_STRUCTURE_SCAN_BUDGET;
        if viewport_height == 0 {
            let max_scroll = self
                .exact_total_height()
                .map(|height| max_scroll(height, viewport_height));
            let effective_scroll = max_scroll
                .map(|max| requested_scroll.min(max))
                .unwrap_or(requested_scroll);

            return DetailViewport {
                text: Text::from(Vec::<Line<'static>>::new()),
                max_scroll,
                effective_scroll,
            };
        }

        let mut effective_scroll = requested_scroll;
        let mut lines = self.materialize_viewport(
            document,
            effective_scroll,
            viewport_height,
            &mut structural_budget,
        );

        let mut exact_max = self
            .exact_total_height()
            .map(|height| max_scroll(height, viewport_height));

        if let Some(max) = exact_max {
            let clamped = requested_scroll.min(max);
            if clamped != effective_scroll {
                effective_scroll = clamped;
                lines = self.materialize_viewport(
                    document,
                    effective_scroll,
                    viewport_height,
                    &mut structural_budget,
                );
                exact_max = self
                    .exact_total_height()
                    .map(|height| max_scroll(height, viewport_height));
            }
        }

        DetailViewport {
            text: Text::from(lines),
            max_scroll: exact_max,
            effective_scroll,
        }
    }

    fn materialize_viewport(
        &mut self,
        document: &impl DetailTextSource,
        absolute_scroll: usize,
        viewport_height: usize,
        structural_budget: &mut usize,
    ) -> Vec<Line<'static>> {
        let Some((mut chunk_index, mut offset_in_chunk)) =
            self.locate_visual_offset(document, absolute_scroll, structural_budget)
        else {
            return Vec::new();
        };

        let mut viewport = Vec::with_capacity(viewport_height);

        while viewport.len() < viewport_height {
            if chunk_index >= self.chunks.len() {
                if self.advance_structure(document, structural_budget) {
                    continue;
                }
                break;
            }
            let remaining = viewport_height.saturating_sub(viewport.len());
            if self.chunk_is_giant(chunk_index) {
                self.append_giant_viewport(
                    document,
                    chunk_index,
                    offset_in_chunk,
                    remaining,
                    &mut viewport,
                );
            } else {
                self.ensure_chunk_materialized(document, chunk_index);

                let chunk_lines = self
                    .materialized_chunks
                    .iter()
                    .find(|chunk| chunk.index == chunk_index)
                    .expect("materialized detail chunk must be cached");
                let end = offset_in_chunk
                    .saturating_add(remaining)
                    .min(chunk_lines.lines.len());

                viewport.extend_from_slice(&chunk_lines.lines[offset_in_chunk..end]);
            }

            chunk_index = chunk_index.saturating_add(1);
            offset_in_chunk = 0;
        }

        viewport
    }

    fn locate_visual_offset(
        &mut self,
        document: &impl DetailTextSource,
        visual_offset: usize,
        structural_budget: &mut usize,
    ) -> Option<(usize, usize)> {
        loop {
            let mut prefix = 0usize;

            for chunk_index in 0..self.chunks.len() {
                let local_offset = visual_offset.saturating_sub(prefix);
                if self.chunk_is_giant(chunk_index)
                    && self.chunks[chunk_index].visual_lines.is_none()
                    && self.ensure_giant_page(document, chunk_index, local_offset)
                {
                    return Some((chunk_index, local_offset));
                }

                if self.chunks[chunk_index].visual_lines.is_none() {
                    self.ensure_chunk_materialized(document, chunk_index);
                }
                let visual_lines = self.chunks[chunk_index]
                    .visual_lines
                    .expect("known detail chunk must have a visual line count");
                let end = prefix.saturating_add(visual_lines);

                if visual_offset < end {
                    return Some((chunk_index, visual_offset.saturating_sub(prefix)));
                }

                prefix = end;
            }

            if !self.advance_structure(document, structural_budget) {
                return None;
            }
        }
    }

    fn ensure_chunk_materialized(&mut self, document: &impl DetailTextSource, chunk_index: usize) {
        debug_assert!(!self.chunk_is_giant(chunk_index));
        if let Some(position) = self
            .materialized_chunks
            .iter()
            .position(|chunk| chunk.index == chunk_index)
        {
            let chunk = self
                .materialized_chunks
                .remove(position)
                .expect("cached detail chunk position must exist");
            self.materialized_chunks.push_back(chunk);
            return;
        }

        let unit_index = self.chunks[chunk_index].unit_index;
        let StructuralUnit::Normal(block) = &self.structure().units[unit_index] else {
            unreachable!("normal detail chunk must reference a normal structural unit");
        };
        let text = self
            .structure()
            .raw_map
            .bounded_text(document, block.raw_range.clone());
        let mut lines = Vec::new();
        let wrapped = wrap_normal_block(
            &text,
            block.logical_line_count,
            self.lazy_layout_width(),
            &mut MaterializeSink::new(&mut lines),
            &mut || false,
        );
        debug_assert!(wrapped);

        let visual_lines = lines.len();
        if let Some(known) = self.chunks[chunk_index].visual_lines {
            debug_assert_eq!(known, visual_lines);
        } else {
            self.chunks[chunk_index].visual_lines = Some(visual_lines);
        }

        self.materialized_chunks.push_back(MaterializedChunk {
            index: chunk_index,
            lines,
        });

        while self.materialized_chunks.len() > MATERIALIZED_CHUNK_CACHE_SIZE {
            self.materialized_chunks.pop_front();
        }

        #[cfg(test)]
        {
            self.chunk_layout_operations = self.chunk_layout_operations.saturating_add(1);
        }
    }

    fn append_giant_viewport(
        &mut self,
        document: &impl DetailTextSource,
        chunk_index: usize,
        mut visual_row: usize,
        row_count: usize,
        viewport: &mut Vec<Line<'static>>,
    ) {
        let target_len = viewport.len().saturating_add(row_count);

        while viewport.len() < target_len
            && self.ensure_giant_page(document, chunk_index, visual_row)
        {
            let page = self
                .materialized_giant_pages
                .iter()
                .find(|page| {
                    page.chunk_index == chunk_index
                        && page.start_row <= visual_row
                        && visual_row < page.start_row.saturating_add(page.lines.len())
                })
                .expect("requested giant detail row must be materialized");
            let page_offset = visual_row.saturating_sub(page.start_row);
            let remaining = target_len.saturating_sub(viewport.len());
            let end = page_offset.saturating_add(remaining).min(page.lines.len());
            viewport.extend_from_slice(&page.lines[page_offset..end]);
            visual_row = visual_row.saturating_add(end.saturating_sub(page_offset));
        }
    }

    fn ensure_giant_page(
        &mut self,
        document: &impl DetailTextSource,
        chunk_index: usize,
        requested_row: usize,
    ) -> bool {
        if self.giant_page_is_cached(chunk_index, requested_row) {
            self.touch_giant_page(chunk_index, requested_row);
            return true;
        }

        if self.chunks[chunk_index]
            .visual_lines
            .is_some_and(|total| requested_row >= total)
        {
            return false;
        }

        loop {
            let checkpoint = self.chunks[chunk_index]
                .checkpoints
                .iter()
                .rev()
                .find(|checkpoint| checkpoint.visual_row <= requested_row)
                .copied()
                .expect("giant detail line must have an initial checkpoint");

            if checkpoint.visual_row > requested_row {
                return false;
            }

            let unit_index = self.chunks[chunk_index].unit_index;
            let StructuralUnit::Giant(line) = &self.structure().units[unit_index] else {
                unreachable!("giant detail chunk must reference a giant structural unit");
            };
            let raw_start = line.raw_start;
            let raw_end = line.raw_end;
            let source = DocumentGiantSource {
                document,
                raw_map: &self.structure().raw_map,
                raw_start,
                known_end: raw_end,
            };
            let mut sink_lines = Vec::new();
            let progress = wrap_document_giant_line_page(
                &source,
                checkpoint.raw_position,
                self.lazy_layout_width(),
                GIANT_LINE_PAGE_ROWS,
                &mut MaterializeSink::new(&mut sink_lines),
                &mut || false,
            )
            .expect("foreground giant detail layout is not cancellable");

            #[cfg(not(test))]
            let _ = progress.scanned_bytes;

            #[cfg(test)]
            {
                self.giant_scanned_bytes = self
                    .giant_scanned_bytes
                    .saturating_add(progress.scanned_bytes);
                self.giant_page_layout_operations =
                    self.giant_page_layout_operations.saturating_add(1);
            }

            if raw_end.is_none()
                && let Some(discovered_end) = progress.discovered_end
            {
                self.resolve_open_giant(discovered_end);
            }

            let next_checkpoint = WrapCheckpoint {
                visual_row: checkpoint.visual_row.saturating_add(progress.rows),
                raw_position: progress.next_raw_position,
            };

            if next_checkpoint.visual_row > checkpoint.visual_row {
                let checkpoints = &mut self.chunks[chunk_index].checkpoints;
                if checkpoints.last().copied() != Some(next_checkpoint) {
                    checkpoints.push(next_checkpoint);
                }
            }

            self.materialized_giant_pages
                .push_back(MaterializedGiantPage {
                    chunk_index,
                    start_row: checkpoint.visual_row,
                    lines: sink_lines,
                });
            while self.materialized_giant_pages.len() > GIANT_LINE_PAGE_CACHE_SIZE {
                self.materialized_giant_pages.pop_front();
            }

            if progress.finished {
                let exact = next_checkpoint.visual_row;
                if let Some(known) = self.chunks[chunk_index].visual_lines {
                    debug_assert_eq!(known, exact);
                } else {
                    self.chunks[chunk_index].visual_lines = Some(exact);
                }
            }

            if self.giant_page_is_cached(chunk_index, requested_row) {
                return true;
            }
            if progress.finished || progress.rows == 0 {
                return false;
            }
        }
    }

    fn giant_page_is_cached(&self, chunk_index: usize, visual_row: usize) -> bool {
        self.materialized_giant_pages.iter().any(|page| {
            page.chunk_index == chunk_index
                && page.start_row <= visual_row
                && visual_row < page.start_row.saturating_add(page.lines.len())
        })
    }

    fn touch_giant_page(&mut self, chunk_index: usize, visual_row: usize) {
        let Some(position) = self.materialized_giant_pages.iter().position(|page| {
            page.chunk_index == chunk_index
                && page.start_row <= visual_row
                && visual_row < page.start_row.saturating_add(page.lines.len())
        }) else {
            return;
        };
        let page = self
            .materialized_giant_pages
            .remove(position)
            .expect("cached giant detail page position must exist");
        self.materialized_giant_pages.push_back(page);
    }

    fn lazy_layout_width(&self) -> usize {
        usize::from(self.detail_width.saturating_sub(1))
    }

    fn chunk_is_giant(&self, chunk_index: usize) -> bool {
        let unit_index = self.chunks[chunk_index].unit_index;
        self.structure().units[unit_index].is_giant()
    }

    fn document_identity(&self) -> DetailDocumentIdentity {
        DetailDocumentIdentity {
            generation: self.document_generation,
            revision: self.revision.unwrap_or_default(),
            segment_lengths: self.segment_lengths.clone(),
        }
    }

    fn count_identity(&self) -> DetailCountIdentity {
        DetailCountIdentity {
            document_generation: self.document_generation,
            layout_generation: self.layout_generation,
            revision: self.revision.unwrap_or_default(),
            layout_width: self.lazy_layout_width(),
            segment_lengths: self.segment_lengths.clone(),
            chunk_count: self.chunks.len(),
        }
    }

    fn exact_total_height(&self) -> Option<usize> {
        if !self.structure_is_complete() {
            return None;
        }
        self.chunks.iter().try_fold(0usize, |total, chunk| {
            chunk.visual_lines.map(|lines| total.saturating_add(lines))
        })
    }

    #[cfg(test)]
    fn known_chunk_count(&self) -> usize {
        self.chunks
            .iter()
            .filter(|chunk| chunk.visual_lines.is_some())
            .count()
    }

    #[cfg(test)]
    fn materialized_visual_line_count(&self) -> usize {
        self.materialized_chunks
            .iter()
            .map(|chunk| chunk.lines.len())
            .sum()
    }

    #[cfg(test)]
    fn materialized_giant_visual_line_count(&self) -> usize {
        self.materialized_giant_pages
            .iter()
            .map(|page| page.lines.len())
            .sum()
    }

    #[cfg(test)]
    fn giant_checkpoint_count(&self) -> usize {
        self.chunks
            .iter()
            .map(|chunk| chunk.checkpoints.len())
            .sum()
    }
}

fn eager_viewport(
    document: &impl DetailTextSource,
    detail_width: u16,
    viewport_height: usize,
    requested_scroll: usize,
) -> DetailViewport {
    let mut wrapped_text = wrap_detail_document(document, detail_width);
    let mut exact_max = max_scroll(wrapped_text.height(), viewport_height);

    if exact_max > 0 && detail_width > 1 {
        wrapped_text = wrap_detail_document(document, detail_width.saturating_sub(1));
        exact_max = max_scroll(wrapped_text.height(), viewport_height);
    }

    let effective_scroll = requested_scroll.min(exact_max);

    DetailViewport {
        text: viewport_text(wrapped_text, effective_scroll, viewport_height),
        max_scroll: Some(exact_max),
        effective_scroll,
    }
}

fn build_document_structure(document: &impl DetailTextSource) -> DetailDocumentStructure {
    IncrementalDetailIndexBuilder::new(document).build_to_end(document)
}

pub(crate) fn build_document_structure_cancellable(
    document: &impl DetailTextSource,
    mut is_cancelled: impl FnMut() -> bool,
) -> Option<Arc<DetailDocumentStructure>> {
    let mut builder = IncrementalDetailIndexBuilder::new(document);

    while !builder.finished {
        if is_cancelled() {
            return None;
        }

        let progress = if builder.current_line_kind == InProgressLineKind::Giant {
            builder.advance_open_giant(document, BACKGROUND_STRUCTURE_SCAN_BUDGET)
        } else {
            builder.advance(document, BACKGROUND_STRUCTURE_SCAN_BUDGET)
        };
        debug_assert!(progress.finished || progress.scanned_bytes > 0);
    }

    if is_cancelled() {
        return None;
    }

    Some(Arc::new(builder.into_structure()))
}

fn structure_prefix_is_compatible(
    partial: &DetailDocumentStructure,
    complete: &DetailDocumentStructure,
) -> bool {
    if !complete.complete
        || partial.raw_map != complete.raw_map
        || partial.units.len() > complete.units.len()
        || partial.logical_line_count > complete.logical_line_count
    {
        return false;
    }

    partial
        .units
        .iter()
        .zip(&complete.units)
        .all(|(partial, complete)| match (partial, complete) {
            (StructuralUnit::Normal(partial), StructuralUnit::Normal(complete)) => {
                partial == complete
            }
            (StructuralUnit::Giant(partial), StructuralUnit::Giant(complete)) => {
                partial.raw_start == complete.raw_start
                    && match partial.raw_end {
                        Some(end) => complete.raw_end == Some(end),
                        None => complete.raw_end.is_some(),
                    }
            }
            _ => false,
        })
}

fn visit_normal_block_lines(
    text: &str,
    logical_line_count: usize,
    mut visit: impl FnMut(&str) -> bool,
) -> bool {
    let mut line_start = 0usize;
    let mut visited = 0usize;

    for (newline, _) in text.match_indices('\n') {
        if visited >= logical_line_count || !visit(&text[line_start..newline]) {
            return false;
        }
        visited = visited.saturating_add(1);
        line_start = newline.saturating_add(1);
    }

    if visited < logical_line_count {
        if !visit(&text[line_start..]) {
            return false;
        }
        visited = visited.saturating_add(1);
    }

    debug_assert_eq!(visited, logical_line_count);
    visited == logical_line_count
}

fn wrap_normal_block(
    text: &str,
    logical_line_count: usize,
    width: usize,
    sink: &mut impl WrapSink,
    is_cancelled: &mut impl FnMut() -> bool,
) -> bool {
    visit_normal_block_lines(text, logical_line_count, |logical_line| {
        wrap_logical_line(logical_line, width, sink, is_cancelled)
    })
}

pub(super) fn wrap_detail_document(document: &impl DetailTextSource, width: u16) -> Text<'static> {
    let width = usize::from(width);
    let mut lines = Vec::new();
    let mut logical_line_fragments = Vec::new();

    for segment_index in 0..document.segment_count() {
        let text = document
            .segment_text(segment_index)
            .expect("detail segment index must exist");
        let mut start = 0;

        for (newline, _) in text.match_indices('\n') {
            logical_line_fragments.push(&text[start..newline]);
            wrap_logical_line_fragments(&logical_line_fragments, width, &mut lines);
            logical_line_fragments.clear();
            start = newline + 1;
        }

        logical_line_fragments.push(&text[start..]);
    }

    wrap_logical_line_fragments(&logical_line_fragments, width, &mut lines);

    Text::from(lines)
}

fn wrap_logical_line_fragments(fragments: &[&str], width: usize, lines: &mut Vec<Line<'static>>) {
    let mut non_empty = fragments
        .iter()
        .copied()
        .filter(|fragment| !fragment.is_empty());
    let Some(first) = non_empty.next() else {
        lines.push(Line::from(""));
        return;
    };

    if non_empty.next().is_none() {
        wrap_logical_line(first, width, &mut MaterializeSink::new(lines), &mut || {
            false
        });
        return;
    }

    // segment境界がlogical lineの途中にある場合だけ、その1行を結合する。
    let capacity = fragments.iter().map(|fragment| fragment.len()).sum();
    let mut logical_line = String::with_capacity(capacity);
    for fragment in fragments {
        logical_line.push_str(fragment);
    }

    wrap_logical_line(
        &logical_line,
        width,
        &mut MaterializeSink::new(lines),
        &mut || false,
    );
}

fn count_giant_logical_line_fragments(
    fragments: &[&str],
    width: usize,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Option<usize> {
    let mut total_rows = 0usize;
    let mut raw_offset = 0usize;

    loop {
        let mut sink = CountSink::default();
        let progress = wrap_giant_logical_line_page(
            fragments,
            width,
            raw_offset,
            GIANT_LINE_PAGE_ROWS,
            &mut sink,
            is_cancelled,
        )?;
        let _ = progress.scanned_bytes;
        debug_assert_eq!(sink.lines, progress.rows);
        total_rows = total_rows.saturating_add(progress.rows);
        raw_offset = progress.next_raw_offset;

        if progress.finished {
            return Some(total_rows);
        }
        if progress.rows == 0 {
            return None;
        }
    }
}

struct DocumentGiantSource<'a> {
    document: &'a dyn DetailTextSource,
    raw_map: &'a DetailRawMap,
    raw_start: RawOffset,
    known_end: Option<RawOffset>,
}

fn wrap_document_giant_line_page(
    source: &DocumentGiantSource<'_>,
    start_raw_position: RawOffset,
    width: usize,
    row_budget: usize,
    sink: &mut impl WrapSink,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Option<DocumentGiantWrapProgress> {
    let window_bytes = row_budget
        .saturating_mul(width.max(1))
        .saturating_mul(4)
        .saturating_add(16)
        .max(GIANT_LINE_WINDOW_MIN_BYTES);
    wrap_document_giant_line_page_with_window_bytes(
        source,
        start_raw_position,
        width,
        row_budget,
        window_bytes,
        sink,
        is_cancelled,
    )
}

fn wrap_document_giant_line_page_with_window_bytes(
    source: &DocumentGiantSource<'_>,
    start_raw_position: RawOffset,
    width: usize,
    row_budget: usize,
    initial_window_bytes: usize,
    sink: &mut impl WrapSink,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Option<DocumentGiantWrapProgress> {
    let hard_end = source.known_end.unwrap_or(source.raw_map.total_len());
    assert!(source.raw_start <= start_raw_position && start_raw_position <= hard_end);

    if width == 0 {
        let probe = source.raw_map.line_probe(
            source.document,
            start_raw_position,
            source.known_end,
            hard_end.0.saturating_sub(start_raw_position.0),
        );
        if start_raw_position >= hard_end || probe.text.is_empty() {
            return Some(DocumentGiantWrapProgress {
                rows: 0,
                next_raw_position: hard_end,
                finished: true,
                scanned_bytes: probe.inspected_bytes,
                discovered_end: probe.discovered_end,
            });
        }
        sink.push(&probe.text);
        sink.emit();
        let next_raw_position = RawOffset(start_raw_position.0.saturating_add(probe.text.len()));
        return Some(DocumentGiantWrapProgress {
            rows: 1,
            next_raw_position,
            finished: probe.reaches_end,
            scanned_bytes: probe.inspected_bytes,
            discovered_end: probe.discovered_end,
        });
    }

    let mut raw_position = start_raw_position;
    let mut rows = 0usize;
    let mut scanned_bytes = 0usize;
    let mut window_bytes = initial_window_bytes.max(1);
    let mut discovered_end = None;

    loop {
        if is_cancelled() {
            return None;
        }

        let window = document_giant_line_window(
            source.document,
            source.raw_map,
            raw_position,
            source.known_end,
            window_bytes,
            width,
        );
        discovered_end = discovered_end.or(window.discovered_end);
        if window.reaches_end && window.text.is_empty() {
            return Some(DocumentGiantWrapProgress {
                rows,
                next_raw_position: raw_position,
                finished: true,
                scanned_bytes: scanned_bytes.saturating_add(window.inspected_bytes),
                discovered_end,
            });
        }
        let remaining_rows = row_budget.saturating_sub(rows);
        let progress = wrap_logical_line_window(
            window.text.as_ref(),
            width,
            sink,
            is_cancelled,
            remaining_rows,
            window.reaches_end,
        )?;
        scanned_bytes = scanned_bytes
            .saturating_add(window.inspected_bytes)
            .saturating_add(progress.scanned_bytes);
        rows = rows.saturating_add(progress.rows);
        raw_position = RawOffset(raw_position.0.saturating_add(progress.next_offset));

        if progress.finished {
            return Some(DocumentGiantWrapProgress {
                rows,
                next_raw_position: raw_position,
                finished: true,
                scanned_bytes,
                discovered_end,
            });
        }
        if rows >= row_budget {
            return Some(DocumentGiantWrapProgress {
                rows,
                next_raw_position: raw_position,
                finished: false,
                scanned_bytes,
                discovered_end,
            });
        }
        if progress.rows > 0 {
            window_bytes = initial_window_bytes.max(1);
            continue;
        }
        if window.reaches_end || window.text.is_empty() {
            return None;
        }

        window_bytes = window_bytes.saturating_mul(2).min(
            source
                .known_end
                .unwrap_or(source.raw_map.total_len())
                .0
                .saturating_sub(raw_position.0),
        );
    }
}

fn document_giant_line_window<'a>(
    document: &'a (impl DetailTextSource + ?Sized),
    raw_map: &DetailRawMap,
    raw_position: RawOffset,
    known_end: Option<RawOffset>,
    max_bytes: usize,
    width: usize,
) -> DocumentGiantWindow<'a> {
    let hard_end = known_end.unwrap_or(raw_map.total_len());
    let remaining_bytes = hard_end.0.saturating_sub(raw_position.0);
    let target_bytes = max_bytes.max(1).min(remaining_bytes);
    let mut probe_bytes = target_bytes.saturating_add(64).min(remaining_bytes);

    loop {
        let probe = raw_map.line_probe(document, raw_position, known_end, probe_bytes);
        if probe.reaches_end {
            return DocumentGiantWindow {
                text: probe.text,
                inspected_bytes: probe.inspected_bytes,
                reaches_end: true,
                discovered_end: probe.discovered_end,
            };
        }

        let (grapheme_boundary, token_boundary) =
            next_safe_window_boundaries(probe.text.as_ref(), target_bytes);
        if let Some(boundary) = token_boundary {
            return DocumentGiantWindow {
                text: truncate_window(probe.text, boundary),
                inspected_bytes: probe.inspected_bytes,
                reaches_end: false,
                discovered_end: None,
            };
        }

        let trailing_token_is_oversized =
            UnicodeSegmentation::split_word_bound_indices(probe.text.as_ref())
                .next_back()
                .is_some_and(|(offset, token)| {
                    offset < target_bytes && UnicodeWidthStr::width(token) > width
                });
        if trailing_token_is_oversized && let Some(boundary) = grapheme_boundary {
            return DocumentGiantWindow {
                text: truncate_window(probe.text, boundary),
                inspected_bytes: probe.inspected_bytes,
                reaches_end: false,
                discovered_end: None,
            };
        }

        let next_probe = probe_bytes.saturating_mul(2).min(remaining_bytes);
        if next_probe == probe_bytes {
            return DocumentGiantWindow {
                text: probe.text,
                inspected_bytes: probe.inspected_bytes,
                reaches_end: probe.reaches_end,
                discovered_end: probe.discovered_end,
            };
        }
        probe_bytes = next_probe;
    }
}

struct DocumentGiantWindow<'a> {
    text: Cow<'a, str>,
    inspected_bytes: usize,
    reaches_end: bool,
    discovered_end: Option<GiantLineEnd>,
}

fn wrap_giant_logical_line_page(
    fragments: &[&str],
    width: usize,
    start_raw_offset: usize,
    row_budget: usize,
    sink: &mut impl WrapSink,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Option<GiantWrapProgress> {
    let window_bytes = row_budget
        .saturating_mul(width.max(1))
        .saturating_mul(4)
        .saturating_add(16)
        .max(GIANT_LINE_WINDOW_MIN_BYTES);
    wrap_giant_logical_line_page_with_window_bytes(
        fragments,
        width,
        start_raw_offset,
        row_budget,
        window_bytes,
        sink,
        is_cancelled,
    )
}

fn wrap_giant_logical_line_page_with_window_bytes(
    fragments: &[&str],
    width: usize,
    start_raw_offset: usize,
    row_budget: usize,
    initial_window_bytes: usize,
    sink: &mut impl WrapSink,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Option<GiantWrapProgress> {
    let total_bytes = fragments
        .iter()
        .map(|fragment| fragment.len())
        .fold(0usize, usize::saturating_add);

    if width == 0 {
        if start_raw_offset >= total_bytes {
            return Some(GiantWrapProgress {
                rows: 0,
                next_raw_offset: total_bytes,
                finished: true,
                scanned_bytes: 0,
            });
        }
        for fragment in fragments {
            sink.push(fragment);
        }
        sink.emit();
        return Some(GiantWrapProgress {
            rows: 1,
            next_raw_offset: total_bytes,
            finished: true,
            scanned_bytes: total_bytes,
        });
    }

    let mut raw_offset = start_raw_offset.min(total_bytes);
    let mut rows = 0usize;
    let mut scanned_bytes = 0usize;
    let mut window_bytes = initial_window_bytes.max(1);

    loop {
        if is_cancelled() {
            return None;
        }

        let (window, inspected_bytes) =
            logical_line_window(fragments, raw_offset, window_bytes, width);
        let reaches_end = raw_offset.saturating_add(window.len()) >= total_bytes;
        let remaining_rows = row_budget.saturating_sub(rows);
        let progress = wrap_logical_line_window(
            window.as_ref(),
            width,
            sink,
            is_cancelled,
            remaining_rows,
            reaches_end,
        )?;
        scanned_bytes = scanned_bytes
            .saturating_add(inspected_bytes)
            .saturating_add(progress.scanned_bytes);
        rows = rows.saturating_add(progress.rows);
        raw_offset = raw_offset.saturating_add(progress.next_offset);

        if progress.finished {
            return Some(GiantWrapProgress {
                rows,
                next_raw_offset: raw_offset,
                finished: true,
                scanned_bytes,
            });
        }
        if rows >= row_budget {
            return Some(GiantWrapProgress {
                rows,
                next_raw_offset: raw_offset,
                finished: false,
                scanned_bytes,
            });
        }
        if progress.rows > 0 {
            window_bytes = initial_window_bytes.max(1);
            continue;
        }
        if reaches_end || window.is_empty() {
            return None;
        }

        window_bytes = window_bytes
            .saturating_mul(2)
            .min(total_bytes.saturating_sub(raw_offset));
    }
}

fn logical_line_window<'a>(
    fragments: &[&'a str],
    raw_offset: usize,
    max_bytes: usize,
    width: usize,
) -> (Cow<'a, str>, usize) {
    let total_bytes = fragments
        .iter()
        .map(|fragment| fragment.len())
        .fold(0usize, usize::saturating_add);
    let remaining_bytes = total_bytes.saturating_sub(raw_offset);
    let target_bytes = max_bytes.max(1).min(remaining_bytes);
    let mut probe_bytes = target_bytes.saturating_add(64).min(remaining_bytes);

    loop {
        let window = raw_logical_line_window(fragments, raw_offset, probe_bytes);
        let inspected_bytes = window.len();
        if window.len() >= remaining_bytes {
            return (window, inspected_bytes);
        }

        let (grapheme_boundary, token_boundary) =
            next_safe_window_boundaries(window.as_ref(), target_bytes);

        if let Some(boundary) = token_boundary {
            return (truncate_window(window, boundary), inspected_bytes);
        }

        // A storage window must not become an artificial soft-wrap boundary. If the
        // token crossing the target is already wider than a visual row, however,
        // the full token is necessarily oversized and the eager path hard-wraps it.
        // In that case any complete grapheme boundary is a safe resume point.
        let trailing_token_is_oversized =
            UnicodeSegmentation::split_word_bound_indices(window.as_ref())
                .next_back()
                .is_some_and(|(offset, token)| {
                    offset < target_bytes && UnicodeWidthStr::width(token) > width
                });
        if trailing_token_is_oversized && let Some(boundary) = grapheme_boundary {
            return (truncate_window(window, boundary), inspected_bytes);
        }

        // The target is still inside a short/incomplete token or an incomplete
        // grapheme. Grow only the lookahead until its semantic boundary is known.
        let next_probe = probe_bytes.saturating_mul(2).min(remaining_bytes);
        if next_probe == probe_bytes {
            return (window, inspected_bytes);
        }
        probe_bytes = next_probe;
    }
}

fn next_safe_window_boundaries(text: &str, target: usize) -> (Option<usize>, Option<usize>) {
    let mut grapheme_boundaries = UnicodeSegmentation::grapheme_indices(text, true)
        .map(|(offset, _)| offset)
        .filter(|offset| *offset >= target && *offset > 0)
        .peekable();
    let first_grapheme = grapheme_boundaries.peek().copied();

    for token_boundary in UnicodeSegmentation::split_word_bound_indices(text)
        .map(|(offset, _)| offset)
        .filter(|offset| *offset >= target && *offset > 0)
    {
        while grapheme_boundaries
            .peek()
            .is_some_and(|boundary| *boundary < token_boundary)
        {
            grapheme_boundaries.next();
        }
        if grapheme_boundaries.peek().copied() == Some(token_boundary) {
            return (first_grapheme, Some(token_boundary));
        }
    }

    (first_grapheme, None)
}

fn raw_logical_line_window<'a>(
    fragments: &[&'a str],
    raw_offset: usize,
    max_bytes: usize,
) -> Cow<'a, str> {
    let mut skip = raw_offset;
    let mut remaining = max_bytes.max(1);
    let mut pieces = Vec::new();

    for fragment in fragments {
        if skip >= fragment.len() {
            skip = skip.saturating_sub(fragment.len());
            continue;
        }

        let start = skip;
        skip = 0;
        let available = fragment.len().saturating_sub(start);
        let mut take = available.min(remaining);
        while take > 0 && !fragment.is_char_boundary(start.saturating_add(take)) {
            take -= 1;
        }
        if take == 0 && available > 0 {
            take = fragment[start..].chars().next().map_or(0, char::len_utf8);
        }

        pieces.push(&fragment[start..start.saturating_add(take)]);
        remaining = remaining.saturating_sub(take);
        if take < available || remaining == 0 {
            break;
        }
    }

    match pieces.as_slice() {
        [] => Cow::Borrowed(""),
        [single] => Cow::Borrowed(single),
        _ => {
            let capacity = pieces.iter().map(|piece| piece.len()).sum();
            let mut joined = String::with_capacity(capacity);
            for piece in pieces {
                joined.push_str(piece);
            }
            Cow::Owned(joined)
        }
    }
}

fn truncate_window(window: Cow<'_, str>, end: usize) -> Cow<'_, str> {
    match window {
        Cow::Borrowed(text) => Cow::Borrowed(&text[..end]),
        Cow::Owned(mut text) => {
            text.truncate(end);
            Cow::Owned(text)
        }
    }
}

trait WrapSink {
    fn has_content(&self) -> bool;
    fn push(&mut self, text: &str);
    fn emit(&mut self);
    fn discard_current(&mut self);
}

struct MaterializeSink<'a> {
    current: String,
    lines: &'a mut Vec<Line<'static>>,
}

impl<'a> MaterializeSink<'a> {
    fn new(lines: &'a mut Vec<Line<'static>>) -> Self {
        Self {
            current: String::new(),
            lines,
        }
    }
}

impl WrapSink for MaterializeSink<'_> {
    fn has_content(&self) -> bool {
        !self.current.is_empty()
    }

    fn push(&mut self, text: &str) {
        self.current.push_str(text);
    }

    fn emit(&mut self) {
        self.lines
            .push(Line::from(std::mem::take(&mut self.current)));
    }

    fn discard_current(&mut self) {
        self.current.clear();
    }
}

#[derive(Default)]
struct CountSink {
    has_content: bool,
    lines: usize,
}

impl WrapSink for CountSink {
    fn has_content(&self) -> bool {
        self.has_content
    }

    fn push(&mut self, text: &str) {
        self.has_content |= !text.is_empty();
    }

    fn emit(&mut self) {
        self.lines = self.lines.saturating_add(1);
        self.has_content = false;
    }

    fn discard_current(&mut self) {
        self.has_content = false;
    }
}

#[derive(Debug, Clone, Copy)]
struct WrapProgress {
    rows: usize,
    next_offset: usize,
    finished: bool,
    scanned_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct GiantWrapProgress {
    rows: usize,
    next_raw_offset: usize,
    finished: bool,
    scanned_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct DocumentGiantWrapProgress {
    rows: usize,
    next_raw_position: RawOffset,
    finished: bool,
    scanned_bytes: usize,
    discovered_end: Option<GiantLineEnd>,
}

fn wrap_logical_line(
    logical_line: &str,
    width: usize,
    sink: &mut impl WrapSink,
    is_cancelled: &mut impl FnMut() -> bool,
) -> bool {
    wrap_logical_line_window(logical_line, width, sink, is_cancelled, usize::MAX, true).is_some()
}

fn wrap_logical_line_window(
    logical_line: &str,
    width: usize,
    sink: &mut impl WrapSink,
    is_cancelled: &mut impl FnMut() -> bool,
    row_budget: usize,
    flush_at_end: bool,
) -> Option<WrapProgress> {
    if row_budget == 0 {
        return Some(WrapProgress {
            rows: 0,
            next_offset: 0,
            finished: flush_at_end && logical_line.is_empty(),
            scanned_bytes: 0,
        });
    }

    if width == 0 || logical_line.is_empty() {
        sink.push(logical_line);
        sink.emit();
        return Some(WrapProgress {
            rows: 1,
            next_offset: logical_line.len(),
            finished: flush_at_end,
            scanned_bytes: logical_line.len(),
        });
    }

    let mut current_width = 0usize;
    let mut rows = 0usize;
    let mut next_offset = 0usize;
    let mut scanned_bytes = 0usize;

    for (token_start, token) in UnicodeSegmentation::split_word_bound_indices(logical_line) {
        if is_cancelled() {
            return None;
        }

        let token_end = token_start.saturating_add(token.len());
        scanned_bytes = scanned_bytes.max(token_end);
        let token_width = UnicodeWidthStr::width(token);

        if token_width <= width {
            if sink.has_content() && current_width.saturating_add(token_width) > width {
                sink.emit();
                rows = rows.saturating_add(1);
                next_offset = token_start;
                current_width = 0;
                if rows >= row_budget {
                    return Some(WrapProgress {
                        rows,
                        next_offset,
                        finished: false,
                        scanned_bytes,
                    });
                }
            }

            sink.push(token);
            current_width = current_width.saturating_add(token_width);

            if current_width == width {
                sink.emit();
                rows = rows.saturating_add(1);
                next_offset = token_end;
                current_width = 0;
                if rows >= row_budget {
                    return Some(WrapProgress {
                        rows,
                        next_offset,
                        finished: flush_at_end && next_offset == logical_line.len(),
                        scanned_bytes,
                    });
                }
            }

            continue;
        }

        if current_width > 0 {
            sink.emit();
            rows = rows.saturating_add(1);
            next_offset = token_start;
            current_width = 0;
            if rows >= row_budget {
                return Some(WrapProgress {
                    rows,
                    next_offset,
                    finished: false,
                    scanned_bytes,
                });
            }
        }

        for (grapheme_start, grapheme) in UnicodeSegmentation::grapheme_indices(token, true) {
            if is_cancelled() {
                return None;
            }

            let grapheme_end = token_start
                .saturating_add(grapheme_start)
                .saturating_add(grapheme.len());
            let grapheme_width = UnicodeWidthStr::width(grapheme);

            if current_width > 0 && current_width.saturating_add(grapheme_width) > width {
                sink.emit();
                rows = rows.saturating_add(1);
                next_offset = token_start.saturating_add(grapheme_start);
                current_width = 0;
                if rows >= row_budget {
                    return Some(WrapProgress {
                        rows,
                        next_offset,
                        finished: false,
                        scanned_bytes,
                    });
                }
            }

            sink.push(grapheme);
            current_width = current_width.saturating_add(grapheme_width);

            if current_width >= width {
                sink.emit();
                rows = rows.saturating_add(1);
                next_offset = grapheme_end;
                current_width = 0;
                if rows >= row_budget {
                    return Some(WrapProgress {
                        rows,
                        next_offset,
                        finished: flush_at_end && next_offset == logical_line.len(),
                        scanned_bytes,
                    });
                }
            }
        }
    }

    if flush_at_end && sink.has_content() {
        sink.emit();
        rows = rows.saturating_add(1);
        next_offset = logical_line.len();
    } else if !flush_at_end {
        sink.discard_current();
    }

    Some(WrapProgress {
        rows,
        next_offset,
        finished: flush_at_end,
        scanned_bytes,
    })
}

pub(super) fn max_scroll(content_height: usize, viewport_height: usize) -> usize {
    content_height.saturating_sub(viewport_height)
}

pub(super) fn viewport_text(
    wrapped_text: Text<'static>,
    absolute_scroll: usize,
    viewport_height: usize,
) -> Text<'static> {
    Text::from(
        wrapped_text
            .lines
            .into_iter()
            .skip(absolute_scroll)
            .take(viewport_height)
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::detail::DetailDocument;
    use std::fmt::Write as _;
    use std::time::Instant;

    fn make_document<'a>(segments: &'a [&'a str]) -> DetailDocument<'a> {
        DetailDocument::from_borrowed_segments(segments)
    }

    fn completed_structure(layout: &DetailLayout) -> &Arc<DetailDocumentStructure> {
        match layout.structure_state.as_ref().unwrap() {
            DetailStructureState::Complete(structure) => structure,
            DetailStructureState::Building(_) => panic!("detail structure is still incomplete"),
        }
    }

    fn building_frontier(layout: &DetailLayout) -> RawOffset {
        match layout.structure_state.as_ref().unwrap() {
            DetailStructureState::Building(builder) => builder.scan_position,
            DetailStructureState::Complete(_) => panic!("detail structure is already complete"),
        }
    }

    fn complete_structure_via_foreground(layout: &mut DetailLayout, document: &DetailDocument<'_>) {
        while !layout.structure_is_complete() {
            if let Some(chunk_index) = layout.chunks.len().checked_sub(1)
                && layout.chunk_is_giant(chunk_index)
                && matches!(
                    &layout.structure().units[layout.chunks[chunk_index].unit_index],
                    StructuralUnit::Giant(GiantLineUnit { raw_end: None, .. })
                )
            {
                let next_row = layout.chunks[chunk_index]
                    .checkpoints
                    .last()
                    .unwrap()
                    .visual_row;
                assert!(layout.ensure_giant_page(document, chunk_index, next_row));
                continue;
            }
            let mut budget = FOREGROUND_STRUCTURE_SCAN_BUDGET;
            assert!(layout.advance_structure(document, &mut budget));
        }
    }

    fn text_lines(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn indexed_logical_lines(document: &DetailDocument<'_>) -> Vec<String> {
        let structure = build_document_structure(document);
        structure_logical_lines(document, &structure)
    }

    fn structure_logical_lines(
        document: &impl DetailTextSource,
        structure: &DetailDocumentStructure,
    ) -> Vec<String> {
        let mut lines = Vec::with_capacity(structure.logical_line_count);
        for unit in &structure.units {
            match unit {
                StructuralUnit::Normal(block) => {
                    let text = structure
                        .raw_map
                        .bounded_text(document, block.raw_range.clone());
                    assert!(visit_normal_block_lines(
                        &text,
                        block.logical_line_count,
                        |line| {
                            lines.push(line.to_string());
                            true
                        },
                    ));
                }
                StructuralUnit::Giant(line) => {
                    let end = line.raw_end.expect("test structure must be complete");
                    lines.push(
                        structure
                            .raw_map
                            .bounded_text(document, line.raw_start..end)
                            .into_owned(),
                    );
                }
            }
        }
        lines
    }

    fn sparse_materialized_lines(
        document: &impl DetailTextSource,
        structure: &DetailDocumentStructure,
        width: usize,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        for unit in &structure.units {
            match unit {
                StructuralUnit::Normal(block) => {
                    let text = structure
                        .raw_map
                        .bounded_text(document, block.raw_range.clone());
                    assert!(wrap_normal_block(
                        &text,
                        block.logical_line_count,
                        width,
                        &mut MaterializeSink::new(&mut lines),
                        &mut || false,
                    ));
                }
                StructuralUnit::Giant(line) => {
                    let end = line.raw_end.expect("test structure must be complete");
                    let fragments = structure.raw_map.fragments(document, line.raw_start..end);
                    wrap_logical_line_fragments(&fragments, width, &mut lines);
                }
            }
        }
        text_lines(&Text::from(lines))
    }

    fn unit_signature(structure: &DetailDocumentStructure) -> Vec<(Range<RawOffset>, usize, bool)> {
        structure
            .units
            .iter()
            .map(|unit| match unit {
                StructuralUnit::Normal(block) => {
                    (block.raw_range.clone(), block.logical_line_count, false)
                }
                StructuralUnit::Giant(line) => (
                    line.raw_start..line.raw_end.expect("test structure must be complete"),
                    1,
                    true,
                ),
            })
            .collect()
    }

    fn build_structure_with_budgets(
        document: &DetailDocument<'_>,
        budgets: &[usize],
    ) -> DetailDocumentStructure {
        assert!(!budgets.is_empty());
        assert!(budgets.iter().all(|budget| *budget > 0));

        let mut builder = IncrementalDetailIndexBuilder::new(document);
        let mut budget_index = 0usize;
        while !builder.finished {
            if builder.current_line_is_known_giant() {
                builder.resolve_open_giant_by_scanning(document);
                continue;
            }
            let budget = budgets[budget_index % budgets.len()];
            let progress = builder.advance(document, budget);
            assert!(progress.scanned_bytes <= budget);
            assert!(progress.finished || progress.scanned_bytes > 0);
            budget_index = budget_index.saturating_add(1);
        }
        builder.into_structure()
    }

    fn assert_pause_patterns_match_build_to_end(document: &DetailDocument<'_>) {
        let reference = IncrementalDetailIndexBuilder::new(document).build_to_end(document);
        let budgets = [
            1,
            2,
            7,
            63,
            64,
            4 * 1024,
            GIANT_LOGICAL_LINE_BYTE_THRESHOLD - 1,
            GIANT_LOGICAL_LINE_BYTE_THRESHOLD,
            GIANT_LOGICAL_LINE_BYTE_THRESHOLD + 1,
            usize::MAX,
        ];

        for budget in budgets {
            assert_eq!(
                build_structure_with_budgets(document, &[budget]),
                reference,
                "single repeated budget {budget} changed the final index"
            );
        }

        assert_eq!(
            build_structure_with_budgets(
                document,
                &[1, 64, 7, 4 * 1024, GIANT_LOGICAL_LINE_BYTE_THRESHOLD + 1, 2,],
            ),
            reference,
            "mixed budgets changed the final index"
        );
    }

    fn many_lines(count: usize) -> String {
        let mut raw = String::new();
        for line in 0..count {
            raw.push_str(&format!("line-{line}\n"));
        }
        raw
    }

    fn many_varied_lines(count: usize) -> String {
        let mut raw = String::new();
        for line in 0..count {
            if line % 3 == 0 {
                raw.push_str(&format!("long-token-{line}-abcdefghijklmnop\n"));
            } else {
                raw.push_str(&format!("short {line}\n"));
            }
        }
        raw
    }

    fn take_count_request(
        layout: &mut DetailLayout,
        document: &DetailDocument<'_>,
    ) -> DetailCountRequest {
        layout.stage_analysis_command(document);
        match layout.take_analysis_command() {
            Some(DetailAnalysisCommand::Count(request)) => request,
            other => panic!("expected detail count request, got {other:?}"),
        }
    }

    fn take_structure_request(
        layout: &mut DetailLayout,
        document: &DetailDocument<'_>,
    ) -> DetailStructureRequest {
        layout.stage_analysis_command(document);
        match layout.take_analysis_command() {
            Some(DetailAnalysisCommand::BuildStructure(request)) => request,
            other => panic!("expected detail structure request, got {other:?}"),
        }
    }

    fn structure_result(request: DetailStructureRequest) -> DetailStructureResult {
        let structure = Arc::new(build_document_structure(&request.snapshot));
        DetailStructureResult {
            identity: request.identity,
            structure,
        }
    }

    fn count_result(request: DetailCountRequest) -> DetailCountResult {
        let mut never_cancel = || false;
        let chunk_visual_lines = request
            .structure
            .count_chunks(
                &request.snapshot,
                request.identity.layout_width,
                &mut never_cancel,
            )
            .unwrap();

        DetailCountResult {
            identity: request.identity,
            chunk_visual_lines,
        }
    }

    fn streaming_fragment_lines(
        fragments: &[&str],
        width: usize,
        page_rows: usize,
        initial_window_bytes: usize,
    ) -> Vec<String> {
        let mut raw_offset = 0usize;
        let mut lines = Vec::new();
        loop {
            let mut page = Vec::new();
            let progress = wrap_giant_logical_line_page_with_window_bytes(
                fragments,
                width,
                raw_offset,
                page_rows,
                initial_window_bytes,
                &mut MaterializeSink::new(&mut page),
                &mut || false,
            )
            .unwrap();
            lines.extend(page.into_iter().map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            }));
            raw_offset = progress.next_raw_offset;
            if progress.finished {
                return lines;
            }
            assert!(progress.rows > 0);
        }
    }

    fn eager_fragment_lines(fragments: &[&str], width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        wrap_logical_line_fragments(fragments, width, &mut lines);
        text_lines(&Text::from(lines))
    }

    fn assert_streaming_matches_eager(
        fragments: &[&str],
        width: usize,
        page_rows: usize,
        window_bytes: &[usize],
    ) {
        let reference = eager_fragment_lines(fragments, width);
        for &window_bytes in window_bytes {
            assert_eq!(
                streaming_fragment_lines(fragments, width, page_rows, window_bytes),
                reference,
                "width={width}, rows={page_rows}, window={window_bytes}"
            );
        }
    }

    fn is_grapheme_boundary(text: &str, offset: usize) -> bool {
        offset == text.len()
            || UnicodeSegmentation::grapheme_indices(text, true)
                .any(|(boundary, _)| boundary == offset)
    }

    fn is_token_boundary(text: &str, offset: usize) -> bool {
        offset == text.len()
            || UnicodeSegmentation::split_word_bound_indices(text)
                .any(|(boundary, _)| boundary == offset)
    }

    #[test]
    fn logical_line_index_preserves_empty_and_trailing_lines() {
        for (raw, expected) in [
            ("abc", vec!["abc"]),
            ("abc\n", vec!["abc", ""]),
            ("abc\n\n", vec!["abc", "", ""]),
            ("", vec![""]),
        ] {
            let segments = [raw];
            let document = make_document(&segments);
            assert_eq!(indexed_logical_lines(&document), expected);
        }
    }

    #[test]
    fn logical_line_index_preserves_segment_boundary_semantics() {
        for (segments, expected) in [
            (vec!["ab", "cd"], vec!["abcd"]),
            (vec!["ab", "\ncd"], vec!["ab", "cd"]),
            (vec!["ab\n", "cd"], vec!["ab", "cd"]),
            (vec!["ab", "", "cd\n", ""], vec!["abcd", ""]),
        ] {
            let document = make_document(&segments);
            assert_eq!(indexed_logical_lines(&document), expected);
        }
    }

    #[test]
    fn global_raw_offsets_map_across_empty_and_non_empty_segments() {
        let segments = ["abc", "", "def\n", "ghi"];
        let document = make_document(&segments);
        let raw_map = DetailRawMap::new(&document);

        assert_eq!(raw_map.total_len(), RawOffset(10));
        assert_eq!(
            raw_map.bounded_text(&document, RawOffset(0)..RawOffset(3)),
            "abc"
        );
        assert_eq!(
            raw_map.bounded_text(&document, RawOffset(3)..RawOffset(7)),
            "def\n"
        );
        assert_eq!(
            raw_map.bounded_text(&document, RawOffset(0)..RawOffset(10)),
            "abcdef\nghi"
        );
        assert_eq!(raw_map.bytes_from(&document, RawOffset(3)), b"def\n");
        assert!(matches!(
            raw_map.bounded_text(&document, RawOffset(0)..RawOffset(3)),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            raw_map.bounded_text(&document, RawOffset(0)..RawOffset(10)),
            Cow::Owned(_)
        ));
    }

    #[test]
    fn resumable_builder_preserves_exact_lines_across_segment_boundaries() {
        let segments = ["ab\n", "", "日本", "語", "\n"];
        let document = make_document(&segments);
        let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end(&document);

        assert_eq!(
            structure_logical_lines(&document, &structure),
            ["ab", "日本語", ""]
        );
        assert_eq!(
            unit_signature(&structure),
            [(RawOffset(0)..RawOffset(13), 3, false)]
        );
    }

    #[test]
    fn builder_preserves_giant_threshold_and_normal_unit_partitioning() {
        for (len, giant) in [
            (GIANT_LOGICAL_LINE_BYTE_THRESHOLD - 1, false),
            (GIANT_LOGICAL_LINE_BYTE_THRESHOLD, true),
            (GIANT_LOGICAL_LINE_BYTE_THRESHOLD + 1, true),
        ] {
            let raw = "a".repeat(len);
            let segments = [raw.as_str()];
            let document = make_document(&segments);
            let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end(&document);
            assert_eq!(
                unit_signature(&structure),
                [(RawOffset(0)..RawOffset(len), 1, giant)],
                "len={len}"
            );
        }

        let normal_lines = "x\n".repeat(DETAIL_CHUNK_LINES * 2);
        let segments = [normal_lines.as_str()];
        let document = make_document(&segments);
        let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end(&document);
        assert_eq!(
            unit_signature(&structure),
            [
                (RawOffset(0)..RawOffset(512), DETAIL_CHUNK_LINES, false),
                (RawOffset(512)..RawOffset(1024), DETAIL_CHUNK_LINES, false,),
                (RawOffset(1024)..RawOffset(1024), 1, false),
            ]
        );

        let giant = "g".repeat(GIANT_LOGICAL_LINE_BYTE_THRESHOLD);
        let mixed = format!("normal\n{giant}\ntail");
        let segments = [mixed.as_str()];
        let document = make_document(&segments);
        let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end(&document);
        assert_eq!(
            structure_logical_lines(&document, &structure),
            ["normal", giant.as_str(), "tail"]
        );
        assert_eq!(structure.logical_line_count, 3);
        assert_eq!(
            structure
                .units
                .iter()
                .map(StructuralUnit::is_giant)
                .collect::<Vec<_>>(),
            [false, true, false]
        );

        let at_eof = format!("normal\n{giant}");
        let segments = [at_eof.as_str()];
        let document = make_document(&segments);
        let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end(&document);
        assert_eq!(structure.logical_line_count, 2);
        assert_eq!(
            structure
                .units
                .iter()
                .map(StructuralUnit::is_giant)
                .collect::<Vec<_>>(),
            [false, true]
        );
    }

    #[test]
    fn sparse_metadata_scales_with_normal_blocks_not_logical_lines() {
        let raw = many_lines(100_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let structure = build_document_structure(&document);
        let expected_blocks = 100_001usize.div_ceil(DETAIL_CHUNK_LINES);

        assert_eq!(structure.logical_line_count, 100_001);
        assert_eq!(structure.units.len(), expected_blocks);
        assert!(
            structure
                .units
                .iter()
                .all(|unit| matches!(unit, StructuralUnit::Normal(_)))
        );
        assert!(structure.units.len() < structure.logical_line_count / 200);
    }

    #[test]
    fn normal_blocks_respect_255_256_and_257_line_boundaries() {
        for (line_count, expected_counts) in [
            (255usize, vec![255usize]),
            (256, vec![256]),
            (257, vec![256, 1]),
        ] {
            let raw = std::iter::repeat_n("x", line_count)
                .collect::<Vec<_>>()
                .join("\n");
            let segments = [raw.as_str()];
            let document = make_document(&segments);
            let structure = build_document_structure(&document);
            let actual_counts = structure
                .units
                .iter()
                .map(|unit| match unit {
                    StructuralUnit::Normal(block) => block.logical_line_count,
                    StructuralUnit::Giant(_) => panic!("short lines must remain normal"),
                })
                .collect::<Vec<_>>();

            assert_eq!(actual_counts, expected_counts, "line_count={line_count}");
            assert_eq!(
                structure_logical_lines(&document, &structure).len(),
                line_count
            );
        }
    }

    #[test]
    fn normal_blocks_close_before_a_line_would_exceed_the_raw_byte_bound() {
        let half = "a".repeat(NORMAL_BLOCK_MAX_RAW_BYTES / 2 - 1);
        let raw = format!("{half}\n{half}\nz");
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let structure = build_document_structure(&document);

        assert_eq!(
            unit_signature(&structure),
            [
                (
                    RawOffset(0)..RawOffset(NORMAL_BLOCK_MAX_RAW_BYTES),
                    2,
                    false,
                ),
                (
                    RawOffset(NORMAL_BLOCK_MAX_RAW_BYTES)..RawOffset(raw.len()),
                    1,
                    false,
                ),
            ]
        );

        let near_threshold = "n".repeat(GIANT_LOGICAL_LINE_BYTE_THRESHOLD - 1);
        let segments = [near_threshold.as_str()];
        let document = make_document(&segments);
        let structure = build_document_structure(&document);
        assert_eq!(
            unit_signature(&structure),
            [(RawOffset(0)..RawOffset(near_threshold.len()), 1, false)]
        );
    }

    #[test]
    fn trailing_empty_line_after_a_full_block_is_reconstructed_once() {
        let raw = "\n".repeat(DETAIL_CHUNK_LINES);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let structure = build_document_structure(&document);

        assert_eq!(
            unit_signature(&structure),
            [
                (
                    RawOffset(0)..RawOffset(DETAIL_CHUNK_LINES),
                    DETAIL_CHUNK_LINES,
                    false,
                ),
                (
                    RawOffset(DETAIL_CHUNK_LINES)..RawOffset(DETAIL_CHUNK_LINES),
                    1,
                    false,
                ),
            ]
        );
        assert_eq!(
            structure_logical_lines(&document, &structure),
            vec![String::new(); DETAIL_CHUNK_LINES + 1]
        );
    }

    #[test]
    fn pause_resume_budgets_do_not_change_the_final_structure() {
        let newline_heavy = many_lines(DETAIL_CHUNK_LINES * 3 + 17);
        let segments = [newline_heavy.as_str()];
        assert_pause_patterns_match_build_to_end(&make_document(&segments));

        let giant = "giant-token ".repeat(GIANT_LOGICAL_LINE_BYTE_THRESHOLD / 6);
        let segments = [giant.as_str()];
        assert_pause_patterns_match_build_to_end(&make_document(&segments));

        let mixed = format!("first\n{}\nlast\n", "z".repeat(70 * 1024));
        let segments = ["prefix\n", mixed.as_str(), "tail"];
        assert_pause_patterns_match_build_to_end(&make_document(&segments));

        let unicode_segments = ["", "日", "本語🙂", "\n", "e\u{301}", "", "\n終"];
        assert_pause_patterns_match_build_to_end(&make_document(&unicode_segments));

        let half = "r".repeat(NORMAL_BLOCK_MAX_RAW_BYTES / 2 - 1);
        let raw_bound = format!("{half}\n{half}\nnext");
        let segments = [raw_bound.as_str()];
        assert_pause_patterns_match_build_to_end(&make_document(&segments));
    }

    #[test]
    fn sparse_normal_materialization_matches_eager_across_segments_and_widths() {
        let tail = "日本語 e\u{301} 👩‍💻 abcdefghijklmnopqrstuvwxyz\n\n".repeat(600);
        let segments = ["first\nsecond-", "", "続き\n", tail.as_str(), "last"];
        let document = make_document(&segments);
        let structure = build_document_structure(&document);

        assert!(structure.units.len() > 2);
        assert!(
            structure
                .units
                .iter()
                .all(|unit| matches!(unit, StructuralUnit::Normal(_)))
        );
        for width in [0usize, 1, 7, 19, 80] {
            assert_eq!(
                sparse_materialized_lines(&document, &structure, width),
                text_lines(&wrap_detail_document(
                    &document,
                    u16::try_from(width).unwrap()
                )),
                "width={width}"
            );
        }
    }

    #[test]
    fn builder_can_pause_with_a_known_open_giant_line() {
        let raw = format!(
            "{}\ntail",
            "界".repeat(GIANT_LOGICAL_LINE_BYTE_THRESHOLD / 3 + 128)
        );
        assert!(!raw.is_char_boundary(GIANT_LOGICAL_LINE_BYTE_THRESHOLD));
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let reference = IncrementalDetailIndexBuilder::new(&document).build_to_end(&document);
        let mut builder = IncrementalDetailIndexBuilder::new(&document);

        let progress = builder.advance(&document, GIANT_LOGICAL_LINE_BYTE_THRESHOLD);

        assert_eq!(progress.scanned_bytes, GIANT_LOGICAL_LINE_BYTE_THRESHOLD);
        assert!(!progress.finished);
        assert!(builder.current_line_is_known_giant());
        assert_eq!(
            builder.scan_position,
            RawOffset(GIANT_LOGICAL_LINE_BYTE_THRESHOLD)
        );
        assert_eq!(builder.logical_line_count, 1);
        assert!(matches!(
            builder.structure.units.as_slice(),
            [StructuralUnit::Giant(GiantLineUnit { raw_end: None, .. })]
        ));

        let blocked = builder.advance(&document, 7);
        assert_eq!(blocked.scanned_bytes, 0);
        builder.resolve_open_giant_by_scanning(&document);
        while !builder.finished {
            let resumed = builder.advance(&document, 7);
            assert!(resumed.finished || resumed.scanned_bytes > 0);
        }
        assert_eq!(builder.into_structure(), reference);
    }

    #[test]
    fn large_initial_viewport_does_not_layout_the_whole_document() {
        let raw = many_lines(100_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let viewport = layout.viewport(&document, 1, 80, 30, 0);

        assert_eq!(viewport.max_scroll, None);
        assert_eq!(viewport.text.height(), 30);
        assert_eq!(layout.known_chunk_count(), 1);
        assert!(!layout.structure_is_complete());
        assert!(layout.chunks.len() <= 2);
        assert!(layout.foreground_structural_scanned_bytes <= FOREGROUND_STRUCTURE_SCAN_BUDGET);
        assert!(layout.foreground_structural_scanned_bytes < raw.len() / 10);
        assert!(layout.materialized_visual_line_count() <= DETAIL_CHUNK_LINES);
        assert_eq!(layout.chunk_layout_operations, 1);
        assert_eq!(
            text_lines(&viewport.text),
            text_lines(&wrap_detail_document(&document, 79))[..30]
        );
    }

    #[test]
    fn repeated_same_viewport_does_not_advance_partial_structure() {
        let raw = many_lines(100_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 80, 30, 0);
        let frontier = building_frontier(&layout);
        let scanned = layout.foreground_structural_scanned_bytes;
        let units = layout.chunks.len();
        let operations = layout.chunk_layout_operations;

        for _ in 0..5 {
            layout.viewport(&document, 1, 80, 30, 0);
        }

        assert_eq!(building_frontier(&layout), frontier);
        assert_eq!(layout.foreground_structural_scanned_bytes, scanned);
        assert_eq!(layout.chunks.len(), units);
        assert_eq!(layout.chunk_layout_operations, operations);
    }

    #[test]
    fn sequential_scroll_appends_normal_units_without_rebuilding_existing_chunks() {
        let raw = many_lines(100_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let reference = text_lines(&wrap_detail_document(&document, 79));
        let mut layout = DetailLayout::default();

        let first = layout.viewport(&document, 1, 80, 20, 0);
        assert_eq!(text_lines(&first.text), reference[..20]);
        let first_frontier = building_frontier(&layout);
        let first_unit_count = layout.chunks.len();
        let first_operations = layout.chunk_layout_operations;

        let second_scroll = DETAIL_CHUNK_LINES + 5;
        let second = layout.viewport(&document, 1, 80, 20, second_scroll);
        assert_eq!(
            text_lines(&second.text),
            reference[second_scroll..second_scroll + 20]
        );
        assert!(building_frontier(&layout) > first_frontier);
        assert!(layout.chunks.len() > first_unit_count);
        assert_eq!(layout.chunk_layout_operations, first_operations + 1);
        assert!(layout.foreground_structural_scanned_bytes <= 2 * FOREGROUND_STRUCTURE_SCAN_BUDGET);
        assert!(!layout.structure_is_complete());
    }

    #[test]
    fn giant_single_line_initial_viewport_scans_only_a_bounded_prefix() {
        let raw = "0 1 2 3 4 5 6 7 8 9 ".repeat(150_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let viewport = layout.viewport(&document, 1, 80, 30, 0);

        assert_eq!(viewport.max_scroll, None);
        assert_eq!(viewport.text.height(), 30);
        assert_eq!(layout.chunks.len(), 1);
        assert!(layout.chunk_is_giant(0));
        assert_eq!(layout.giant_page_layout_operations, 1);
        assert!(layout.giant_scanned_bytes < raw.len() / 10);
        assert_eq!(layout.materialized_giant_visual_line_count(), 128);
        assert_eq!(layout.giant_checkpoint_count(), 2);
        assert!(!layout.structure_is_complete());
        assert_eq!(
            layout.foreground_structural_scanned_bytes,
            GIANT_LOGICAL_LINE_BYTE_THRESHOLD
        );
        let StructuralUnit::Giant(line) = &layout.structure().units[0] else {
            panic!("initial unit must be an open giant")
        };
        assert_eq!(line.raw_start, RawOffset(0));
        assert_eq!(line.raw_end, None);
        assert_eq!(layout.chunks[0].checkpoints[0].raw_position, RawOffset(0));
    }

    #[test]
    fn e1_closes_open_giant_and_builder_resumes_without_rescanning_its_interior() {
        let prefix = "header\n";
        let giant = "0123456789 ".repeat(12_000);
        let tail = "tail-one\ntail-two";
        let raw = format!("{prefix}{giant}\n{tail}");
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let reference = build_document_structure(&document);
        let giant_visual_rows = eager_fragment_lines(&[giant.as_str()], 39).len();
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 40, 20, 0);
        layout.viewport(&document, 1, 40, 20, 1);
        let giant_chunk = 1;
        assert!(matches!(
            &layout.structure().units[giant_chunk],
            StructuralUnit::Giant(GiantLineUnit { raw_end: None, .. })
        ));

        while layout.chunks[giant_chunk].visual_lines.is_none() {
            let next_row = layout.chunks[giant_chunk]
                .checkpoints
                .last()
                .unwrap()
                .visual_row;
            assert!(layout.ensure_giant_page(&document, giant_chunk, next_row));
        }

        let giant_end = RawOffset(prefix.len() + giant.len());
        assert!(matches!(
            &layout.structure().units[giant_chunk],
            StructuralUnit::Giant(GiantLineUnit {
                raw_end: Some(end),
                ..
            }) if *end == giant_end
        ));
        let structural_scan_before_tail = layout.foreground_structural_scanned_bytes;
        assert!(structural_scan_before_tail <= prefix.len() + GIANT_LOGICAL_LINE_BYTE_THRESHOLD);

        let tail_scroll = 1 + giant_visual_rows;
        let tail_view = layout.viewport(&document, 1, 40, 2, tail_scroll);
        assert_eq!(text_lines(&tail_view.text), ["tail-one", "tail-two"]);
        assert!(layout.structure_is_complete());
        assert_eq!(layout.structure(), &reference);
        assert!(
            layout
                .foreground_structural_scanned_bytes
                .saturating_sub(structural_scan_before_tail)
                <= tail.len()
        );
    }

    #[test]
    fn unresolved_open_giant_survives_width_changes_without_structural_rescan() {
        let prefix = "prefix\n";
        let giant = "👩‍💻token e\u{301}\u{323}token 日本語 ".repeat(20_000);
        let raw = format!("{prefix}{giant}");
        let segments = [
            "prefix\n👩‍",
            "💻token ",
            &raw[prefix.len() + "👩‍💻token ".len()..],
        ];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 120, 20, 1);
        let frontier = building_frontier(&layout);
        let scanned = layout.foreground_structural_scanned_bytes;
        let StructuralUnit::Giant(line) = &layout.structure().units[1] else {
            panic!("second unit must be giant")
        };
        let raw_start = line.raw_start;
        assert_eq!(line.raw_end, None);
        assert!(layout.chunks[1].checkpoints.len() > 1);

        for width in [80u16, 120] {
            layout.prepare(&document, 1, width);
            assert_eq!(layout.chunks[1].checkpoints.len(), 1);
            assert!(layout.materialized_giant_pages.is_empty());
            assert_eq!(layout.chunks[1].checkpoints[0].raw_position, raw_start);
            assert_eq!(building_frontier(&layout), frontier);
            assert_eq!(layout.foreground_structural_scanned_bytes, scanned);

            let viewport = layout.viewport(&document, 1, width, 20, 1);
            let reference = text_lines(&wrap_detail_document(&document, width - 1));
            assert_eq!(text_lines(&viewport.text), reference[1..21]);
            assert_eq!(building_frontier(&layout), frontier);
            assert_eq!(layout.foreground_structural_scanned_bytes, scanned);
            assert_eq!(layout.chunks[1].checkpoints[0].raw_position, raw_start);
            assert!(matches!(
                &layout.structure().units[1],
                StructuralUnit::Giant(GiantLineUnit { raw_end: None, .. })
            ));
        }
    }

    #[test]
    fn open_giant_frontier_inside_utf8_and_segmented_grapheme_wraps_correctly() {
        let rest = "界".repeat(80_000);
        let segments = ["👩‍", "💻", rest.as_str()];
        let document = make_document(&segments);
        let joined = segments.concat();
        assert!(!joined.is_char_boundary(GIANT_LOGICAL_LINE_BYTE_THRESHOLD));
        let mut layout = DetailLayout::default();

        let viewport = layout.viewport(&document, 1, 37, 25, 0);
        let reference = text_lines(&wrap_detail_document(&document, 36));

        assert_eq!(text_lines(&viewport.text), reference[..25]);
        assert_eq!(
            building_frontier(&layout),
            RawOffset(GIANT_LOGICAL_LINE_BYTE_THRESHOLD)
        );
        assert!(matches!(
            layout.structure().units.as_slice(),
            [StructuralUnit::Giant(GiantLineUnit {
                raw_start: RawOffset(0),
                raw_end: None,
            })]
        ));
        assert!(
            layout.giant_scanned_bytes <= GIANT_LOGICAL_LINE_BYTE_THRESHOLD / 2,
            "scanned={} raw={}",
            layout.giant_scanned_bytes,
            joined.len()
        );
    }

    #[test]
    fn unicode_window_edges_match_eager_reference() {
        let raw = [
            "👩‍💻",
            "👨‍👩‍👧‍👦",
            "e\u{301}\u{323}",
            "✈\u{fe0f}",
            "👍🏽",
            "🇯🇵",
            "abcdefghijklmnopqrstuvwxyz",
            " 日本語token 👩‍💻token e\u{301}\u{323}token ",
        ]
        .concat()
        .repeat(40);
        let fragments = [raw.as_str()];

        for width in [3, 5, 9, 13] {
            let reference = eager_fragment_lines(&fragments, width);
            for page_rows in [1, 2, 3, 7] {
                for window_bytes in 1..=96 {
                    assert_eq!(
                        streaming_fragment_lines(&fragments, width, page_rows, window_bytes,),
                        reference,
                        "width={width}, rows={page_rows}, window={window_bytes}"
                    );
                }
            }
        }
    }

    #[test]
    fn zwj_and_long_zwj_window_edges_match_eager_reference() {
        for cluster in ["👩‍💻", "👨‍👩‍👧‍👦"] {
            let raw = format!("{cluster} developer {cluster} family ").repeat(12);
            let fragments = [raw.as_str()];
            let internal_edges = raw
                .char_indices()
                .map(|(offset, _)| offset)
                .filter(|offset| *offset > 0 && *offset < cluster.len())
                .filter(|offset| !is_grapheme_boundary(&raw, *offset))
                .collect::<Vec<_>>();

            assert!(!internal_edges.is_empty());
            assert_streaming_matches_eager(&fragments, 7, 2, &internal_edges);
        }
    }

    #[test]
    fn combining_window_edges_match_eager_reference() {
        let cluster = "e\u{301}\u{323}\u{300}";
        let raw = format!("{cluster}combine {cluster}marks ").repeat(16);
        let fragments = [raw.as_str()];
        let base_mark_edge = "e".len();
        let mark_mark_edge = "e\u{301}".len();

        assert!(!is_grapheme_boundary(&raw, base_mark_edge));
        assert!(!is_grapheme_boundary(&raw, mark_mark_edge));
        assert_streaming_matches_eager(&fragments, 6, 2, &[base_mark_edge, mark_mark_edge]);
    }

    #[test]
    fn variation_modifier_and_regional_indicator_edges_match_eager_reference() {
        for (cluster, edge) in [
            ("✈\u{fe0f}", "✈".len()),
            ("👍🏽", "👍".len()),
            ("🇯🇵", "🇯".len()),
        ] {
            let raw = format!("{cluster}token {cluster} next ").repeat(16);
            let fragments = [raw.as_str()];

            assert!(!is_grapheme_boundary(&raw, edge));
            assert_streaming_matches_eager(&fragments, 5, 1, &[edge]);
        }
    }

    #[test]
    fn ascii_and_unicode_token_window_edges_do_not_become_soft_wrap_boundaries() {
        let raw = "ab abcdefghijkl 日本語token 👩‍💻token e\u{301}\u{323}token xyz ".repeat(20);
        let fragments = [raw.as_str()];
        let ascii_edge = raw.find("abcdefghijkl").unwrap() + 5;
        let unicode_edge = raw.find("日本語token").unwrap() + "日本語".len() + 2;

        assert!(is_grapheme_boundary(&raw, ascii_edge));
        assert!(!is_token_boundary(&raw, ascii_edge));
        assert!(is_grapheme_boundary(&raw, unicode_edge));
        assert!(!is_token_boundary(&raw, unicode_edge));
        assert_streaming_matches_eager(&fragments, 7, 3, &[ascii_edge, unicode_edge]);
    }

    #[test]
    fn oversized_unicode_token_resumes_across_window_and_page_boundaries() {
        let grapheme = "e\u{301}\u{323}";
        let raw = grapheme.repeat(400);
        let fragments = [raw.as_str()];
        let internal_grapheme_edge = "e".len();

        assert!(!is_grapheme_boundary(&raw, internal_grapheme_edge));
        assert_streaming_matches_eager(
            &fragments,
            5,
            1,
            &[1, 2, 3, 5, 8, 13, 21, internal_grapheme_edge],
        );
    }

    #[test]
    fn segment_and_window_edges_inside_graphemes_match_concatenated_reference() {
        let first = "abc👩‍";
        let second = "💻def e\u{301}";
        let third = "\u{323}ghi 🇯";
        let fourth = "🇵jkl ";
        let fragments = [first, second, third, fourth];
        let joined = fragments.concat().repeat(20);
        let repeated_fragments = fragments
            .iter()
            .cycle()
            .take(fragments.len() * 20)
            .copied()
            .collect::<Vec<_>>();
        let reference_fragments = [joined.as_str()];

        assert_eq!(
            streaming_fragment_lines(&repeated_fragments, 6, 2, 5),
            eager_fragment_lines(&reference_fragments, 6)
        );
    }

    #[test]
    fn unicode_giant_foreground_background_and_eager_counts_agree() {
        let raw = "👩‍💻token e\u{301}\u{323}token 🇯🇵 日本語 abcdefghijkl ".repeat(2_000);
        assert!(raw.len() >= GIANT_LOGICAL_LINE_BYTE_THRESHOLD);
        let fragments = [raw.as_str()];
        let eager = eager_fragment_lines(&fragments, 11);
        let streaming = streaming_fragment_lines(&fragments, 11, 7, 17);
        let background = count_giant_logical_line_fragments(&fragments, 11, &mut || false)
            .expect("background count must complete");

        assert_eq!(streaming, eager);
        assert_eq!(background, eager.len());
    }

    #[test]
    fn unicode_window_lookahead_keeps_initial_giant_viewport_bounded() {
        let raw = "👩‍💻token e\u{301}\u{323}token 🇯🇵 日本語 ".repeat(40_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let viewport = layout.viewport(&document, 1, 41, 30, 0);

        assert_eq!(viewport.text.height(), 30);
        assert_eq!(layout.giant_page_layout_operations, 1);
        assert!(layout.giant_scanned_bytes < raw.len() / 10);
        assert!(layout.materialized_giant_visual_line_count() <= GIANT_LINE_PAGE_ROWS);
    }

    #[test]
    fn giant_no_space_token_hard_wraps_without_scanning_the_whole_token() {
        let raw = "a".repeat(2 * 1024 * 1024);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let viewport = layout.viewport(&document, 1, 81, 30, 0);

        assert_eq!(viewport.text.height(), 30);
        assert!(
            text_lines(&viewport.text)
                .iter()
                .all(|line| line.len() == 80)
        );
        assert!(layout.giant_scanned_bytes < raw.len() / 10);
        assert_eq!(layout.giant_page_layout_operations, 1);
        assert!(layout.materialized_giant_visual_line_count() <= GIANT_LINE_PAGE_ROWS);
    }

    #[test]
    fn giant_single_segment_window_borrows_the_shared_raw_text() {
        let raw = "0123456789 ".repeat(100_000);
        let fragments = [raw.as_str()];

        let (window, inspected_bytes) = logical_line_window(&fragments, 1_000, 4_096, 80);

        assert!(matches!(window, Cow::Borrowed(_)));
        assert!(inspected_bytes <= 4_160);
        assert!((4_096..=4_160).contains(&window.len()));
        assert!(is_grapheme_boundary(&raw[1_000..], window.len()));
        assert!(is_token_boundary(&raw[1_000..], window.len()));
        assert_eq!(window.as_ptr(), raw[1_000..].as_ptr());
    }

    #[test]
    fn giant_zero_width_keeps_the_existing_single_visual_line_semantics() {
        let raw = "abc".repeat(30_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let viewport = layout.viewport(&document, 1, 1, 1, 0);

        assert_eq!(viewport.text.height(), 1);
        assert_eq!(text_lines(&viewport.text), [raw]);
        assert_eq!(viewport.max_scroll, Some(0));
    }

    #[test]
    fn sequential_giant_line_scroll_reuses_pages_and_keeps_the_cache_bounded() {
        let raw = "0123456789 ".repeat(200_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        for scroll in [0, 3, 6, 9, 12, 90] {
            let viewport = layout.viewport(&document, 1, 41, 20, scroll);
            assert_eq!(viewport.text.height(), 20);
        }
        assert_eq!(layout.giant_page_layout_operations, 1);

        for page in 1..=8 {
            let scroll = page * GIANT_LINE_PAGE_ROWS;
            let viewport = layout.viewport(&document, 1, 41, 20, scroll);
            assert_eq!(viewport.text.height(), 20);
        }

        assert!(layout.materialized_giant_pages.len() <= GIANT_LINE_PAGE_CACHE_SIZE);
        assert!(
            layout.materialized_giant_visual_line_count()
                <= GIANT_LINE_PAGE_ROWS * GIANT_LINE_PAGE_CACHE_SIZE
        );
        assert_eq!(layout.giant_checkpoint_count(), 10);
    }

    #[test]
    fn giant_streaming_matches_eager_for_words_unicode_and_checkpoint_boundaries() {
        let unit = "word boundary 日本語 e\u{301} 👩‍💻 \u{200b} abcdefghijklmnopqrstuvwxyz ";
        let raw = unit.repeat(2_000);
        assert!(raw.len() >= GIANT_LOGICAL_LINE_BYTE_THRESHOLD);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let detail_width = 14u16;
        let height = 25usize;
        let reference = wrap_detail_document(&document, detail_width - 1);
        let reference_lines = text_lines(&reference);
        let mut layout = DetailLayout::default();

        for scroll in [0, 100, 127, 128, 129, 255, 256, 700] {
            let viewport = layout.viewport(&document, 1, detail_width, height, scroll);
            assert_eq!(
                text_lines(&viewport.text),
                reference_lines[scroll..scroll + height],
                "streaming viewport differed at visual row {scroll}"
            );
        }
    }

    #[test]
    fn giant_oversized_token_resume_matches_eager_across_page_boundaries() {
        let raw = "a".repeat(100_003);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let detail_width = 18u16;
        let height = 20usize;
        let reference_lines = text_lines(&wrap_detail_document(&document, detail_width - 1));
        let mut layout = DetailLayout::default();

        for scroll in [0, 127, 128, 129, 255, 256, 4_000] {
            let viewport = layout.viewport(&document, 1, detail_width, height, scroll);
            assert_eq!(
                text_lines(&viewport.text),
                reference_lines[scroll..scroll + height]
            );
        }
    }

    #[test]
    fn giant_segment_boundaries_are_not_wrap_boundaries_when_resuming() {
        let first = "a".repeat(40_003);
        let second = "a".repeat(40_009);
        let segments = [first.as_str(), second.as_str()];
        let document = make_document(&segments);
        let detail_width = 18u16;
        let height = 20usize;
        let reference_lines = text_lines(&wrap_detail_document(&document, detail_width - 1));
        let boundary_row = first.len() / usize::from(detail_width - 1);
        let mut layout = DetailLayout::default();

        for scroll in [0, boundary_row - 2, boundary_row, boundary_row + 2] {
            let viewport = layout.viewport(&document, 1, detail_width, height, scroll);
            assert_eq!(
                text_lines(&viewport.text),
                reference_lines[scroll..scroll + height]
            );
        }
    }

    #[test]
    fn giant_background_count_makes_exact_scroll_without_extra_materialization() {
        let raw = "word 日本語 abcdefghijklmnopqrstuvwxyz ".repeat(4_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let detail_width = 20u16;
        let height = 30usize;
        let reference_height = wrap_detail_document(&document, detail_width - 1).height();
        let mut layout = DetailLayout::default();

        let initial = layout.viewport(&document, 7, detail_width, height, 0);
        assert_eq!(initial.max_scroll, None);
        assert!(!layout.structure_is_complete());
        complete_structure_via_foreground(&mut layout, &document);
        let page_operations = layout.giant_page_layout_operations;
        let result = count_result(take_count_request(&mut layout, &document));
        assert_eq!(result.chunk_visual_lines, [reference_height]);
        assert!(layout.apply_count_result(result));
        assert_eq!(layout.giant_page_layout_operations, page_operations);

        let exact = layout.viewport(&document, 7, detail_width, height, 0);
        assert_eq!(exact.max_scroll, Some(reference_height - height));
    }

    #[test]
    fn giant_width_change_invalidates_pages_checkpoints_and_old_count_result() {
        let raw = "abcdefghij ".repeat(20_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 120, 20, 300);
        complete_structure_via_foreground(&mut layout, &document);
        let document_structure = Arc::clone(completed_structure(&layout));
        let stale = count_result(take_count_request(&mut layout, &document));
        assert!(layout.giant_checkpoint_count() > 1);

        layout.prepare(&document, 1, 80);
        assert!(Arc::ptr_eq(
            &document_structure,
            completed_structure(&layout)
        ));
        assert_eq!(layout.document_structure_builds, 1);
        assert_eq!(layout.giant_checkpoint_count(), 1);
        assert!(layout.materialized_giant_pages.is_empty());
        assert!(!layout.apply_count_result(stale));
        assert_eq!(layout.invalidations, 2);
        assert_eq!(layout.lazy_layout_width(), 79);

        layout.viewport(&document, 1, 80, 20, 300);
        assert!(layout.materialized_giant_pages.len() <= GIANT_LINE_PAGE_CACHE_SIZE);
        assert!(layout.giant_checkpoint_count() > 1);
    }

    #[test]
    #[ignore = "manual giant single-line viewport benchmark"]
    fn measure_giant_number_line_initial_and_sequential_viewports() {
        let mut raw = String::with_capacity(8 * 1024 * 1024);
        for number in 0..1_000_000 {
            write!(raw, "{number} ").unwrap();
        }
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let initial_started = Instant::now();
        let initial = layout.viewport(&document, 1, 100, 30, 0);
        let initial_elapsed = initial_started.elapsed();
        let initial_scanned = layout.giant_scanned_bytes;

        let scroll_started = Instant::now();
        for scroll in (3..=300).step_by(3) {
            let viewport = layout.viewport(&document, 1, 100, 30, scroll);
            assert_eq!(viewport.text.height(), 30);
        }
        let scroll_elapsed = scroll_started.elapsed();

        eprintln!(
            "raw={} bytes, initial={initial_elapsed:?}, initial_scanned={initial_scanned}, 100 wheel viewports={scroll_elapsed:?}, pages={}, cached_rows={}",
            raw.len(),
            layout.giant_page_layout_operations,
            layout.materialized_giant_visual_line_count()
        );
        assert_eq!(initial.text.height(), 30);
    }

    #[test]
    fn sequential_scroll_reuses_known_and_materialized_chunks() {
        let raw = many_lines(4_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 80, 20, 0);
        assert_eq!(layout.chunk_layout_operations, 1);
        layout.viewport(&document, 1, 80, 20, 3);
        layout.viewport(&document, 1, 80, 20, 6);
        assert_eq!(layout.chunk_layout_operations, 1);

        layout.viewport(&document, 1, 80, 20, DETAIL_CHUNK_LINES - 5);
        assert_eq!(layout.known_chunk_count(), 2);
        assert_eq!(layout.chunk_layout_operations, 2);

        for chunk_index in 2..6 {
            layout.viewport(&document, 1, 80, 20, chunk_index * DETAIL_CHUNK_LINES);
            assert_eq!(layout.chunk_layout_operations, chunk_index + 1);
        }
        assert_eq!(
            layout.materialized_chunks.len(),
            MATERIALIZED_CHUNK_CACHE_SIZE
        );
    }

    #[test]
    fn lazy_viewports_match_the_eager_wrapper_across_chunks_and_at_eof() {
        let raw = many_varied_lines(3_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let width = 18u16;
        let height = 25usize;
        let reference = wrap_detail_document(&document, width - 1);
        let reference_lines = text_lines(&reference);
        let mut layout = DetailLayout::default();
        layout.viewport(&document, 1, width, height, 0);
        let first_chunk_end = layout.chunks[0].visual_lines.unwrap();

        for scroll in [0, first_chunk_end - 5, first_chunk_end, 1_500] {
            let viewport = layout.viewport(&document, 1, width, height, scroll);
            assert_eq!(
                text_lines(&viewport.text),
                reference_lines[scroll..scroll + height]
            );
        }

        let viewport = layout.viewport(&document, 1, width, height, usize::MAX);
        let exact_max = reference_lines.len() - height;
        assert_eq!(viewport.max_scroll, Some(exact_max));
        assert_eq!(viewport.effective_scroll, exact_max);
        assert_eq!(text_lines(&viewport.text), reference_lines[exact_max..]);
    }

    #[test]
    fn lazy_exact_max_is_not_truncated_to_u16() {
        let raw = many_lines(70_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let initial = layout.viewport(&document, 1, 80, 30, usize::MAX);
        assert_eq!(initial.max_scroll, None);
        complete_structure_via_foreground(&mut layout, &document);
        let result = count_result(take_count_request(&mut layout, &document));
        assert!(layout.apply_count_result(result));
        let viewport = layout.viewport(&document, 1, 80, 30, usize::MAX);

        assert_eq!(viewport.max_scroll, Some(69_971));
        assert_eq!(viewport.effective_scroll, 69_971);
        assert!(viewport.max_scroll.unwrap() > usize::from(u16::MAX));
        assert!(layout.materialized_chunks.len() <= MATERIALIZED_CHUNK_CACHE_SIZE);
    }

    #[test]
    fn lazy_layout_reserves_the_scrollbar_gutter_from_the_start() {
        let raw = many_lines(3_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let initial = layout.viewport(&document, 1, 10, 20, 0);
        assert_eq!(initial.max_scroll, None);
        assert_eq!(layout.lazy_layout_width(), 9);

        let finished = layout.viewport(&document, 1, 10, 20, usize::MAX);
        assert!(finished.max_scroll.is_some());
        assert_eq!(layout.lazy_layout_width(), 9);

        let zero = layout.viewport(&document, 1, 0, 1, 0);
        assert_eq!(layout.lazy_layout_width(), 0);
        assert_eq!(zero.text.height(), 1);

        let one = layout.viewport(&document, 1, 1, 1, 0);
        assert_eq!(layout.lazy_layout_width(), 0);
        assert_eq!(one.text.height(), 1);
    }

    #[test]
    fn revision_width_and_shape_changes_invalidate_lazy_metadata() {
        let raw = many_lines(3_000);
        let changed = raw.replacen("line-0", "other-", 1);
        assert_eq!(raw.len(), changed.len());
        let segments = [raw.as_str()];
        let changed_segments = [changed.as_str()];
        let document = make_document(&segments);
        let changed_document = make_document(&changed_segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 80, 20, 0);
        assert_eq!(layout.invalidations, 1);
        assert_eq!(layout.document_structure_builds, 1);
        layout.viewport(&document, 1, 80, 20, 3);
        assert_eq!(layout.invalidations, 1);
        assert_eq!(layout.document_structure_builds, 1);

        let revised = layout.viewport(&changed_document, 2, 80, 20, 0);
        assert_eq!(layout.invalidations, 2);
        assert_eq!(layout.document_structure_builds, 2);
        assert_eq!(text_lines(&revised.text)[0], "other-");

        layout.viewport(&changed_document, 2, 79, 20, 0);
        assert_eq!(layout.invalidations, 3);
        assert_eq!(layout.document_structure_builds, 2);

        let structurally_changed = [changed.as_str(), "x"];
        let structurally_changed = make_document(&structurally_changed);
        layout.viewport(&structurally_changed, 2, 79, 20, 0);
        assert_eq!(layout.invalidations, 4);
        assert_eq!(layout.document_structure_builds, 3);
    }

    #[test]
    fn repeated_width_changes_reuse_one_document_structure() {
        let raw = many_lines(4_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 120, 20, 0);
        let document_structure = Arc::clone(completed_structure(&layout));
        assert_eq!(layout.known_chunk_count(), 1);
        assert!(!layout.materialized_chunks.is_empty());
        for width in [80, 100, 120] {
            layout.prepare(&document, 1, width);
            assert!(Arc::ptr_eq(
                &document_structure,
                completed_structure(&layout)
            ));
            assert_eq!(layout.known_chunk_count(), 0);
            assert!(layout.materialized_chunks.is_empty());
        }

        assert_eq!(layout.document_structure_builds, 1);
        assert_eq!(layout.invalidations, 4);
    }

    #[test]
    fn revision_change_rebuilds_the_document_structure() {
        let first = ["a\nb"];
        let second = ["ab\n"];
        let first = make_document(&first);
        let second = make_document(&second);
        let mut layout = DetailLayout::default();

        layout.prepare(&first, 1, 120);
        let old_structure = Arc::clone(completed_structure(&layout));
        layout.prepare(&first, 1, 80);
        assert!(Arc::ptr_eq(&old_structure, completed_structure(&layout)));

        layout.prepare(&second, 2, 80);
        assert!(!Arc::ptr_eq(&old_structure, completed_structure(&layout)));
        assert_eq!(layout.document_structure_builds, 2);
        assert_eq!(layout.structure().logical_line_count, 2);
    }

    #[test]
    fn segment_shape_change_rebuilds_even_when_total_bytes_are_unchanged() {
        let raw = many_lines(3_000);
        let split = raw.len() / 3;
        let single_segment = [raw.as_str()];
        let split_segments = [&raw[..split], &raw[split..]];
        let single_segment = make_document(&single_segment);
        let split_segments = make_document(&split_segments);
        let mut layout = DetailLayout::default();

        layout.prepare(&single_segment, 1, 80);
        let old_structure = Arc::clone(completed_structure(&layout));
        layout.prepare(&split_segments, 1, 80);

        assert!(!Arc::ptr_eq(&old_structure, completed_structure(&layout)));
        assert_eq!(layout.document_structure_builds, 2);
        assert_eq!(
            layout.structure().logical_line_count,
            old_structure.logical_line_count
        );
    }

    #[test]
    fn newline_heavy_document_keeps_the_same_structure_across_widths() {
        let raw = many_lines(100_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 120, 20, 0);
        let frontier = building_frontier(&layout);
        let discovered_units = layout.structure().units.len();
        let discovered_lines = layout.structure().logical_line_count;
        for width in [80, 100, 120] {
            layout.prepare(&document, 1, width);
            assert_eq!(building_frontier(&layout), frontier);
            assert_eq!(layout.structure().units.len(), discovered_units);
        }

        assert_eq!(layout.document_structure_builds, 1);
        assert_eq!(layout.structure().logical_line_count, discovered_lines);
        assert!(!layout.structure_is_complete());
    }

    #[test]
    fn width_relayout_reuses_index_and_matches_eager_reference() {
        let raw = "word boundary 日本語 e\u{301} 👩‍💻 abcdefghijklmnopqrstuvwxyz\n".repeat(3_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();
        let height = 25usize;
        let scroll = 90usize;

        for width in [120u16, 80, 100, 120] {
            let reference = wrap_detail_document(&document, width - 1);
            let viewport = layout.viewport(&document, 1, width, height, scroll);
            assert_eq!(
                text_lines(&viewport.text),
                text_lines(&reference)[scroll..scroll + height]
            );
        }

        assert_eq!(layout.document_structure_builds, 1);
    }

    #[test]
    fn lazy_unicode_wrapping_matches_the_shared_eager_primitive() {
        let logical = "日本語 e\u{301} 👩‍💻 \u{200b} supercalifragilisticexpialidocious\n";
        let raw = logical.repeat(LAZY_DETAIL_LINE_THRESHOLD);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let width = 9u16;
        let reference = wrap_detail_document(&document, width - 1);
        let mut layout = DetailLayout::default();

        let viewport = layout.viewport(&document, 1, width, 40, 0);

        assert_eq!(text_lines(&viewport.text), text_lines(&reference)[..40]);
        assert_eq!(layout.known_chunk_count(), 1);
    }

    #[test]
    fn document_and_owned_snapshot_use_the_same_index_and_wrap_engine() {
        let raw = "日本語 e\u{301} 👩‍💻 long-token-abcdefghijklmnop\n".repeat(3_000);
        let segments = ["prefix", "\n", raw.as_str(), "trailing\n\n"];
        let document = make_document(&segments);
        let snapshot = document.snapshot();

        assert_eq!(
            indexed_logical_lines(&document),
            structure_logical_lines(&snapshot, &build_document_structure(&snapshot))
        );
        assert_eq!(
            text_lines(&wrap_detail_document(&document, 17)),
            text_lines(&wrap_detail_document(&snapshot, 17))
        );

        let mut document_layout = DetailLayout::default();
        let mut snapshot_layout = DetailLayout::default();
        for scroll in [0, 250, 2_000] {
            let document_view = document_layout.viewport(&document, 1, 18, 30, scroll);
            let snapshot_view = snapshot_layout.viewport(&snapshot, 1, 18, 30, scroll);
            assert_eq!(
                text_lines(&document_view.text),
                text_lines(&snapshot_view.text)
            );
            assert_eq!(document_view.max_scroll, snapshot_view.max_scroll);
            assert_eq!(
                document_view.effective_scroll,
                snapshot_view.effective_scroll
            );
        }
    }

    #[test]
    fn background_counts_make_lazy_max_exact_without_materializing_more_chunks() {
        let raw = many_lines(100_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let initial = layout.viewport(&document, 41, 80, 30, 0);
        assert_eq!(initial.max_scroll, None);
        assert_eq!(layout.known_chunk_count(), 1);
        complete_structure_via_foreground(&mut layout, &document);
        let request = take_count_request(&mut layout, &document);
        let result = count_result(request);

        assert!(layout.apply_count_result(result));
        assert_eq!(layout.known_chunk_count(), layout.chunks.len());
        assert_eq!(layout.chunk_layout_operations, 1);

        let finished = layout.viewport(&document, 41, 80, 30, 0);
        assert_eq!(finished.max_scroll, Some(99_971));
        assert!(finished.max_scroll.unwrap() > usize::from(u16::MAX));
        assert_eq!(layout.chunk_layout_operations, 1);
    }

    #[test]
    fn partial_structure_stages_no_count_until_foreground_reaches_eof() {
        let raw = many_lines(100_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        let initial = layout.viewport(&document, 23, 80, 30, 0);
        assert_eq!(initial.max_scroll, None);
        assert!(!layout.structure_is_complete());
        layout.stage_analysis_command(&document);
        assert!(matches!(
            layout.take_analysis_command(),
            Some(DetailAnalysisCommand::BuildStructure(_))
        ));
        layout.stage_analysis_command(&document);
        assert!(layout.take_analysis_command().is_none());

        complete_structure_via_foreground(&mut layout, &document);
        layout.stage_analysis_command(&document);
        let request = match layout.take_analysis_command() {
            Some(DetailAnalysisCommand::Count(request)) => request,
            other => panic!("completed structure must stage exact count, got {other:?}"),
        };
        let result = count_result(request);
        assert!(layout.apply_count_result(result));
        assert_eq!(
            layout.viewport(&document, 23, 80, 30, 0).max_scroll,
            Some(99_971)
        );
    }

    #[test]
    fn background_structure_completion_restores_exact_count_and_preserves_normal_cache() {
        let raw = Arc::new(many_lines(100_000));
        let shared = [&raw];
        let document = DetailDocument::from_shared_segments(&shared);
        let mut layout = DetailLayout::default();

        let initial = layout.viewport(&document, 50, 80, 30, 0);
        assert_eq!(initial.max_scroll, None);
        assert!(!layout.structure_is_complete());
        assert_eq!(layout.materialized_chunks.len(), 1);
        let initial_chunks = layout.chunks.len();
        let cached_lines = layout.materialized_chunks[0].lines.clone();

        let request = take_structure_request(&mut layout, &document);
        assert!(request.snapshot.shares_buffer(&raw));
        let result = structure_result(request);
        assert!(layout.apply_structure_result(result));

        assert!(layout.structure_is_complete());
        assert!(layout.chunks.len() > initial_chunks);
        assert_eq!(layout.materialized_chunks.len(), 1);
        assert_eq!(layout.materialized_chunks[0].lines, cached_lines);

        let count = take_count_request(&mut layout, &document);
        assert_eq!(count.identity.layout_width, 79);
        assert!(layout.apply_count_result(count_result(count)));
        assert_eq!(
            layout.viewport(&document, 50, 80, 30, 0).max_scroll,
            Some(99_971)
        );
    }

    #[test]
    fn width_changes_do_not_restart_structure_build_and_count_uses_latest_width() {
        let raw = Arc::new(many_lines(100_000));
        let shared = [&raw];
        let document = DetailDocument::from_shared_segments(&shared);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 60, 120, 30, 0);
        let build = take_structure_request(&mut layout, &document);
        let document_generation = build.identity.generation;
        let structure = structure_result(build);

        for width in [80, 100, 120] {
            layout.viewport(&document, 60, width, 30, 0);
            layout.stage_analysis_command(&document);
            assert!(layout.take_analysis_command().is_none());
            assert_eq!(layout.document_generation, document_generation);
        }

        assert!(layout.apply_structure_result(structure));
        let count = take_count_request(&mut layout, &document);
        assert_eq!(count.identity.document_generation, document_generation);
        assert_eq!(count.identity.layout_width, 119);
    }

    #[test]
    fn stale_structure_ready_after_document_switch_is_rejected() {
        let raw_a = Arc::new(many_lines(20_000));
        let raw_b = Arc::new(format!("{}tail-b\n", many_lines(20_000)));
        let shared_a = [&raw_a];
        let shared_b = [&raw_b];
        let document_a = DetailDocument::from_shared_segments(&shared_a);
        let document_b = DetailDocument::from_shared_segments(&shared_b);
        let mut layout = DetailLayout::default();

        layout.viewport(&document_a, 70, 80, 20, 0);
        let stale = structure_result(take_structure_request(&mut layout, &document_a));
        layout.viewport(&document_b, 71, 80, 20, 0);
        let current_generation = layout.document_generation;
        let current_frontier = building_frontier(&layout);

        assert!(!layout.apply_structure_result(stale));
        assert_eq!(layout.document_generation, current_generation);
        assert_eq!(building_frontier(&layout), current_frontier);
        assert!(!layout.structure_is_complete());
    }

    #[test]
    fn background_closes_open_giant_without_invalidating_e1_pages_or_checkpoints() {
        let raw = Arc::new(format!(
            "{}\ntail-one\ntail-two\n",
            "word-日本語-e\u{301}-👩‍💻 ".repeat(8_000)
        ));
        let shared = [&raw];
        let document = DetailDocument::from_shared_segments(&shared);
        let mut layout = DetailLayout::default();

        let viewport = layout.viewport(&document, 80, 31, 30, 0);
        let StructuralUnit::Giant(open) = &layout.structure().units[0] else {
            panic!("first unit must be the open giant");
        };
        assert_eq!(open.raw_end, None);
        let checkpoints = layout.chunks[0].checkpoints.clone();
        let cached_pages = layout.materialized_giant_pages.len();
        assert!(cached_pages > 0);

        let result = structure_result(take_structure_request(&mut layout, &document));
        assert!(layout.apply_structure_result(result));
        let StructuralUnit::Giant(closed) = &layout.structure().units[0] else {
            panic!("first unit must remain giant");
        };
        assert!(closed.raw_end.is_some());
        assert_eq!(layout.chunks[0].checkpoints, checkpoints);
        assert_eq!(layout.materialized_giant_pages.len(), cached_pages);
        assert_eq!(
            text_lines(&viewport.text),
            text_lines(&viewport_text(wrap_detail_document(&document, 30), 0, 30))
        );
        let after_promotion = layout.viewport(&document, 80, 31, 20, 200);
        assert_eq!(
            text_lines(&after_promotion.text),
            text_lines(&viewport_text(wrap_detail_document(&document, 30), 200, 20))
        );
    }

    #[test]
    fn foreground_giant_closure_before_structure_ready_is_a_cache_preserving_noop() {
        let raw = Arc::new(format!(
            "{}\nnormal-tail\n",
            "giant-token-日本語-e\u{301}-👩‍💻 ".repeat(5_000)
        ));
        let shared = [&raw];
        let document = DetailDocument::from_shared_segments(&shared);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 90, 28, 20, 0);
        let background = structure_result(take_structure_request(&mut layout, &document));
        complete_structure_via_foreground(&mut layout, &document);
        let current = Arc::clone(completed_structure(&layout));
        let checkpoints = layout.chunks[0].checkpoints.clone();
        let cached_pages = layout.materialized_giant_pages.len();

        assert!(!layout.apply_structure_result(background));
        assert!(Arc::ptr_eq(&current, completed_structure(&layout)));
        assert_eq!(layout.chunks[0].checkpoints, checkpoints);
        assert_eq!(layout.materialized_giant_pages.len(), cached_pages);
    }

    #[test]
    fn foreground_normal_completion_before_structure_ready_keeps_current_arc_and_count() {
        let raw = Arc::new(many_lines(20_000));
        let shared = [&raw];
        let document = DetailDocument::from_shared_segments(&shared);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 91, 80, 20, 0);
        let background = structure_result(take_structure_request(&mut layout, &document));
        complete_structure_via_foreground(&mut layout, &document);
        let current = Arc::clone(completed_structure(&layout));

        assert!(!layout.apply_structure_result(background));
        assert!(Arc::ptr_eq(&current, completed_structure(&layout)));
        let _count = take_count_request(&mut layout, &document);
    }

    #[test]
    fn background_chunk_counts_match_materialized_chunk_counts() {
        let logical =
            "ASCII words  supercalifragilisticexpialidocious 日本語 e\u{301} 👩‍💻 \u{200b}\n\n";
        let raw = logical.repeat(3_000);
        let segments = ["prefix", raw.as_str(), "trailing\n"];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();
        layout.viewport(&document, 8, 14, 20, 0);
        complete_structure_via_foreground(&mut layout, &document);
        let result = count_result(take_count_request(&mut layout, &document));

        for chunk_index in 0..layout.chunks.len() {
            layout.ensure_chunk_materialized(&document, chunk_index);
        }
        let materialized_counts = layout
            .chunks
            .iter()
            .map(|chunk| chunk.visual_lines.unwrap())
            .collect::<Vec<_>>();

        assert_eq!(result.chunk_visual_lines, materialized_counts);
        assert!(layout.materialized_chunks.len() <= MATERIALIZED_CHUNK_CACHE_SIZE);
    }

    #[test]
    fn mixed_sparse_units_foreground_and_background_counts_agree() {
        let normal_prefix = "ASCII 日本語 e\u{301} 👩‍💻\n\n".repeat(600);
        let giant = "giant-token ".repeat(GIANT_LOGICAL_LINE_BYTE_THRESHOLD / 6);
        let normal_suffix = "tail abcdefghijklmnopqrstuvwxyz\n".repeat(300);
        let raw = format!("{normal_prefix}{giant}\n{normal_suffix}");
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();
        layout.viewport(&document, 19, 21, 20, 0);
        complete_structure_via_foreground(&mut layout, &document);
        let background = count_result(take_count_request(&mut layout, &document));

        for chunk_index in 0..layout.chunks.len() {
            if layout.chunk_is_giant(chunk_index) {
                while layout.chunks[chunk_index].visual_lines.is_none() {
                    let next_row = layout.chunks[chunk_index]
                        .checkpoints
                        .last()
                        .expect("giant unit must retain a checkpoint")
                        .visual_row;
                    assert!(layout.ensure_giant_page(&document, chunk_index, next_row));
                }
            } else {
                layout.ensure_chunk_materialized(&document, chunk_index);
            }
        }

        let foreground = layout
            .chunks
            .iter()
            .map(|chunk| chunk.visual_lines.expect("all units must be known"))
            .collect::<Vec<_>>();
        assert_eq!(background.chunk_visual_lines, foreground);
        assert_eq!(
            foreground.iter().sum::<usize>(),
            wrap_detail_document(&document, 20).height()
        );
        assert!(
            layout
                .structure()
                .units
                .iter()
                .any(StructuralUnit::is_giant)
        );
        assert!(
            layout
                .structure()
                .units
                .iter()
                .any(|unit| matches!(unit, StructuralUnit::Normal(_)))
        );
    }

    #[test]
    fn count_result_rejects_stale_identity_and_mismatched_known_counts() {
        let raw = many_lines(3_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();
        layout.viewport(&document, 7, 80, 20, 0);

        let request = take_count_request(&mut layout, &document);
        let good = count_result(request);

        for mutate in [
            |identity: &mut DetailCountIdentity| identity.document_generation += 1,
            |identity: &mut DetailCountIdentity| identity.layout_generation += 1,
            |identity: &mut DetailCountIdentity| identity.revision += 1,
            |identity: &mut DetailCountIdentity| identity.layout_width += 1,
            |identity: &mut DetailCountIdentity| identity.chunk_count += 1,
        ] {
            let mut stale = good.clone();
            mutate(&mut stale.identity);
            assert!(!layout.apply_count_result(stale));
            assert_eq!(layout.known_chunk_count(), 1);
        }

        let mut mismatch = good.clone();
        mismatch.chunk_visual_lines[0] += 1;
        assert!(!layout.apply_count_result(mismatch));
        assert_eq!(layout.known_chunk_count(), 1);
        assert_eq!(layout.exact_total_height(), None);

        let mut wrong_shape = good;
        wrong_shape.chunk_visual_lines.pop();
        assert!(!layout.apply_count_result(wrong_shape));
        assert_eq!(layout.exact_total_height(), None);
    }

    #[test]
    fn count_result_merges_with_chunks_already_known_by_the_viewport() {
        let raw = many_lines(4_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();
        layout.viewport(&document, 3, 80, 20, DETAIL_CHUNK_LINES * 3);
        assert!(layout.known_chunk_count() >= 4);

        let result = count_result(take_count_request(&mut layout, &document));
        assert!(layout.apply_count_result(result));
        assert_eq!(layout.exact_total_height(), Some(4_001));
    }

    #[test]
    fn count_request_is_staged_once_and_eager_detail_stages_none() {
        let raw = many_lines(3_000);
        let large_segments = [raw.as_str()];
        let large = make_document(&large_segments);
        let small_segments = ["small"];
        let small = make_document(&small_segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&large, 1, 80, 20, 0);
        layout.stage_analysis_command(&large);
        assert!(matches!(
            layout.take_analysis_command(),
            Some(DetailAnalysisCommand::Count(_))
        ));
        layout.stage_analysis_command(&large);
        assert!(layout.take_analysis_command().is_none());

        layout.viewport(&small, 2, 80, 20, 0);
        layout.stage_analysis_command(&small);
        assert!(matches!(
            layout.take_analysis_command(),
            Some(DetailAnalysisCommand::Cancel { .. })
        ));
        layout.stage_analysis_command(&small);
        assert!(layout.take_analysis_command().is_none());

        let mut eager_only = DetailLayout::default();
        eager_only.viewport(&small, 1, 80, 20, 0);
        eager_only.stage_analysis_command(&small);
        assert!(eager_only.take_analysis_command().is_none());
    }

    #[test]
    fn width_change_supersedes_the_old_result_and_stages_one_new_request() {
        let raw = many_lines(3_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 80, 20, 0);
        let old_request = take_count_request(&mut layout, &document);
        let document_structure = Arc::clone(&old_request.structure);
        assert!(Arc::ptr_eq(
            &document_structure,
            completed_structure(&layout)
        ));
        let old_result = count_result(old_request);

        layout.viewport(&document, 1, 60, 20, 0);
        let new_request = take_count_request(&mut layout, &document);
        assert!(Arc::ptr_eq(
            &document_structure,
            completed_structure(&layout)
        ));
        assert!(Arc::ptr_eq(&document_structure, &new_request.structure));
        assert_eq!(layout.document_structure_builds, 1);
        assert_ne!(
            old_result.identity.layout_generation,
            new_request.identity.layout_generation
        );
        assert_ne!(
            old_result.identity.layout_width,
            new_request.identity.layout_width
        );
        assert!(!layout.apply_count_result(old_result));

        let new_result = count_result(new_request);
        assert!(layout.apply_count_result(new_result));
        assert!(layout.exact_total_height().is_some());
    }
}
