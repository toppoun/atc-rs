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
    raw_range: Range<RawOffset>,
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

struct IncrementalDetailIndexBuilder<'a, D: DetailTextSource + ?Sized> {
    document: &'a D,
    raw_map: DetailRawMap,
    scan_position: RawOffset,
    current_line_start: RawOffset,
    current_line_kind: InProgressLineKind,
    pending_normal_start: RawOffset,
    pending_normal_line_count: usize,
    logical_line_count: usize,
    units: Vec<StructuralUnit>,
    finished: bool,
}

impl<'a, D: DetailTextSource + ?Sized> IncrementalDetailIndexBuilder<'a, D> {
    fn new(document: &'a D) -> Self {
        Self {
            document,
            raw_map: DetailRawMap::new(document),
            scan_position: RawOffset::default(),
            current_line_start: RawOffset::default(),
            current_line_kind: InProgressLineKind::Normal,
            pending_normal_start: RawOffset::default(),
            pending_normal_line_count: 0,
            logical_line_count: 0,
            units: Vec::new(),
            finished: false,
        }
    }

    fn advance(&mut self, byte_budget: usize) -> DetailIndexAdvance {
        if self.finished {
            return DetailIndexAdvance {
                scanned_bytes: 0,
                finished: true,
            };
        }

        let mut scanned_bytes = 0usize;
        while scanned_bytes < byte_budget && self.scan_position < self.raw_map.total_len() {
            let available = self.raw_map.bytes_from(self.document, self.scan_position);
            debug_assert!(!available.is_empty());
            let remaining_budget = byte_budget.saturating_sub(scanned_bytes);
            let scan_len = available.len().min(remaining_budget);
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
                self.update_current_line_kind();
            }
        }

        if self.scan_position == self.raw_map.total_len() {
            self.finish_at_eof();
        }

        DetailIndexAdvance {
            scanned_bytes,
            finished: self.finished,
        }
    }

    fn build_to_end(mut self) -> DetailDocumentStructure {
        while !self.finished {
            let progress = self.advance(usize::MAX);
            debug_assert!(progress.finished || progress.scanned_bytes > 0);
        }
        self.into_structure()
    }

    fn into_structure(self) -> DetailDocumentStructure {
        assert!(
            self.finished,
            "detail structure may only be taken after the document is complete"
        );
        DetailDocumentStructure {
            raw_map: self.raw_map,
            logical_line_count: self.logical_line_count,
            units: self.units,
        }
    }

    #[cfg(test)]
    fn current_line_is_known_giant(&self) -> bool {
        !self.finished && self.current_line_kind == InProgressLineKind::Giant
    }

    fn update_current_line_kind(&mut self) {
        // This state is intentionally retained before the line end is known.
        // A later phase can publish an open giant unit without changing the
        // closed DetailDocumentStructure produced by build_to_end().
        if self
            .scan_position
            .0
            .saturating_sub(self.current_line_start.0)
            >= GIANT_LOGICAL_LINE_BYTE_THRESHOLD
        {
            self.current_line_kind = InProgressLineKind::Giant;
        }
    }

    fn finish_current_line(&mut self, content_end: RawOffset, next_line_start: RawOffset) {
        let giant_line = self.current_line_kind == InProgressLineKind::Giant
            || content_end.0.saturating_sub(self.current_line_start.0)
                >= GIANT_LOGICAL_LINE_BYTE_THRESHOLD;
        self.logical_line_count = self.logical_line_count.saturating_add(1);

        if giant_line {
            self.flush_pending_normal(self.current_line_start);
            self.units.push(StructuralUnit::Giant(GiantLineUnit {
                raw_range: self.current_line_start..content_end,
            }));
            self.pending_normal_start = next_line_start;
            return;
        }

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
            self.units.push(StructuralUnit::Normal(NormalBlock {
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

        let eof = self.raw_map.total_len();
        self.finish_current_line(eof, eof);
        self.flush_pending_normal(eof);
        self.finished = true;
    }
}

impl DetailDocumentStructure {
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
                    let fragments = self.raw_map.fragments(document, line.raw_range.clone());
                    count_giant_logical_line_fragments(&fragments, width, &mut is_cancelled)?
                }
            };

            counts.push(visual_lines);
        }

        Some(counts)
    }
}

pub(crate) type DetailCountGeneration = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailCountIdentity {
    pub(super) generation: DetailCountGeneration,
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
pub(crate) enum DetailCountCommand {
    Count(DetailCountRequest),
    Cancel { generation: DetailCountGeneration },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingCountCommand {
    Count,
    Cancel,
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
    raw_offset: usize,
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
    document_structure: Arc<DetailDocumentStructure>,
    chunks: Vec<ChunkMeta>,
    materialized_chunks: VecDeque<MaterializedChunk>,
    materialized_giant_pages: VecDeque<MaterializedGiantPage>,
    count_generation: DetailCountGeneration,
    pending_count_command: Option<PendingCountCommand>,
    ready_count_command: Option<DetailCountCommand>,

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

    pub(super) fn stage_count_command(&mut self, document: &DetailDocument<'_>) {
        if self.ready_count_command.is_some() {
            return;
        }

        let Some(pending) = self.pending_count_command.take() else {
            return;
        };

        self.ready_count_command = Some(match pending {
            PendingCountCommand::Count => DetailCountCommand::Count(DetailCountRequest {
                identity: self.count_identity(),
                snapshot: document.snapshot(),
                structure: Arc::clone(&self.document_structure),
            }),
            PendingCountCommand::Cancel => DetailCountCommand::Cancel {
                generation: self.count_generation,
            },
        });
    }

    pub(super) fn take_count_command(&mut self) -> Option<DetailCountCommand> {
        self.ready_count_command.take()
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
        if document_changed {
            let document_structure = Arc::new(build_document_structure(document));
            let raw_bytes = document_structure.raw_map.total_len().0;
            let mode = if raw_bytes >= LAZY_DETAIL_BYTE_THRESHOLD
                || document_structure.len() >= LAZY_DETAIL_LINE_THRESHOLD
            {
                LayoutMode::Lazy
            } else {
                LayoutMode::Eager
            };

            self.revision = Some(revision);
            self.segment_lengths = segment_lengths;
            self.document_structure = document_structure;
            self.mode = Some(mode);

            #[cfg(test)]
            {
                self.document_structure_builds = self.document_structure_builds.saturating_add(1);
            }
        }

        self.detail_width = detail_width;
        self.chunks = match self.mode.expect("detail layout must have a document") {
            LayoutMode::Lazy => self
                .document_structure
                .units
                .iter()
                .enumerate()
                .map(|(unit_index, unit)| ChunkMeta {
                    unit_index,
                    visual_lines: None,
                    checkpoints: if unit.is_giant() {
                        vec![WrapCheckpoint {
                            visual_row: 0,
                            raw_offset: 0,
                        }]
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
            LayoutMode::Eager => Vec::new(),
        };
        self.materialized_chunks.clear();
        self.materialized_giant_pages.clear();
        self.count_generation = self.count_generation.wrapping_add(1);
        self.pending_count_command = match self.mode.expect("detail layout must have a mode") {
            LayoutMode::Lazy => Some(PendingCountCommand::Count),
            LayoutMode::Eager if previous_mode == Some(LayoutMode::Lazy) => {
                Some(PendingCountCommand::Cancel)
            }
            LayoutMode::Eager => None,
        };
        self.ready_count_command = None;

        #[cfg(test)]
        {
            self.chunk_layout_operations = 0;
            self.giant_scanned_bytes = 0;
            self.giant_page_layout_operations = 0;
            self.invalidations = self.invalidations.saturating_add(1);
        }
    }

    fn lazy_viewport(
        &mut self,
        document: &impl DetailTextSource,
        viewport_height: usize,
        requested_scroll: usize,
    ) -> DetailViewport {
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
        let mut lines = self.materialize_viewport(document, effective_scroll, viewport_height);

        let mut exact_max = self
            .exact_total_height()
            .map(|height| max_scroll(height, viewport_height));

        if let Some(max) = exact_max {
            let clamped = requested_scroll.min(max);
            if clamped != effective_scroll {
                effective_scroll = clamped;
                lines = self.materialize_viewport(document, effective_scroll, viewport_height);
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
    ) -> Vec<Line<'static>> {
        let Some((mut chunk_index, mut offset_in_chunk)) =
            self.locate_visual_offset(document, absolute_scroll)
        else {
            return Vec::new();
        };

        let mut viewport = Vec::with_capacity(viewport_height);

        while viewport.len() < viewport_height && chunk_index < self.chunks.len() {
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
    ) -> Option<(usize, usize)> {
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

        None
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
        let StructuralUnit::Normal(block) = &self.document_structure.units[unit_index] else {
            unreachable!("normal detail chunk must reference a normal structural unit");
        };
        let text = self
            .document_structure
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
            let StructuralUnit::Giant(line) = &self.document_structure.units[unit_index] else {
                unreachable!("giant detail chunk must reference a giant structural unit");
            };
            let fragments = self
                .document_structure
                .raw_map
                .fragments(document, line.raw_range.clone());
            let mut sink_lines = Vec::new();
            let progress = wrap_giant_logical_line_page(
                &fragments,
                self.lazy_layout_width(),
                checkpoint.raw_offset,
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

            let next_checkpoint = WrapCheckpoint {
                visual_row: checkpoint.visual_row.saturating_add(progress.rows),
                raw_offset: progress.next_raw_offset,
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
        self.document_structure.units[unit_index].is_giant()
    }

    fn count_identity(&self) -> DetailCountIdentity {
        DetailCountIdentity {
            generation: self.count_generation,
            revision: self.revision.unwrap_or_default(),
            layout_width: self.lazy_layout_width(),
            segment_lengths: self.segment_lengths.clone(),
            chunk_count: self.chunks.len(),
        }
    }

    fn exact_total_height(&self) -> Option<usize> {
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
    IncrementalDetailIndexBuilder::new(document).build_to_end()
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
                StructuralUnit::Giant(line) => lines.push(
                    structure
                        .raw_map
                        .bounded_text(document, line.raw_range.clone())
                        .into_owned(),
                ),
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
                    let fragments = structure
                        .raw_map
                        .fragments(document, line.raw_range.clone());
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
                StructuralUnit::Giant(line) => (line.raw_range.clone(), 1, true),
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
            let budget = budgets[budget_index % budgets.len()];
            let progress = builder.advance(budget);
            assert!(progress.scanned_bytes <= budget);
            assert!(progress.finished || progress.scanned_bytes > 0);
            budget_index = budget_index.saturating_add(1);
        }
        builder.into_structure()
    }

    fn assert_pause_patterns_match_build_to_end(document: &DetailDocument<'_>) {
        let reference = IncrementalDetailIndexBuilder::new(document).build_to_end();
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
        layout.stage_count_command(document);
        match layout.take_count_command() {
            Some(DetailCountCommand::Count(request)) => request,
            other => panic!("expected detail count request, got {other:?}"),
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
        let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end();

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
            let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end();
            assert_eq!(
                unit_signature(&structure),
                [(RawOffset(0)..RawOffset(len), 1, giant)],
                "len={len}"
            );
        }

        let normal_lines = "x\n".repeat(DETAIL_CHUNK_LINES * 2);
        let segments = [normal_lines.as_str()];
        let document = make_document(&segments);
        let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end();
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
        let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end();
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
        let structure = IncrementalDetailIndexBuilder::new(&document).build_to_end();
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
        let reference = IncrementalDetailIndexBuilder::new(&document).build_to_end();
        let mut builder = IncrementalDetailIndexBuilder::new(&document);

        let progress = builder.advance(GIANT_LOGICAL_LINE_BYTE_THRESHOLD);

        assert_eq!(progress.scanned_bytes, GIANT_LOGICAL_LINE_BYTE_THRESHOLD);
        assert!(!progress.finished);
        assert!(builder.current_line_is_known_giant());
        assert_eq!(
            builder.scan_position,
            RawOffset(GIANT_LOGICAL_LINE_BYTE_THRESHOLD)
        );
        assert_eq!(builder.logical_line_count, 0);
        assert!(builder.units.is_empty());

        while !builder.finished {
            let resumed = builder.advance(7);
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
        assert!(layout.chunks.len() > 300);
        assert!(layout.materialized_visual_line_count() <= DETAIL_CHUNK_LINES);
        assert_eq!(layout.chunk_layout_operations, 1);
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
        let page_operations = layout.giant_page_layout_operations;
        let result = count_result(take_count_request(&mut layout, &document));
        assert_eq!(result.chunk_visual_lines, [reference_height]);
        assert!(layout.apply_count_result(result));

        let exact = layout.viewport(&document, 7, detail_width, height, 0);
        assert_eq!(exact.max_scroll, Some(reference_height - height));
        assert_eq!(layout.giant_page_layout_operations, page_operations);
    }

    #[test]
    fn giant_width_change_invalidates_pages_checkpoints_and_old_count_result() {
        let raw = "abcdefghij ".repeat(20_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        layout.viewport(&document, 1, 120, 20, 300);
        let document_structure = Arc::clone(&layout.document_structure);
        let stale = count_result(take_count_request(&mut layout, &document));
        assert!(layout.giant_checkpoint_count() > 1);

        layout.prepare(&document, 1, 80);
        assert!(Arc::ptr_eq(&document_structure, &layout.document_structure));
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
        let document_structure = Arc::clone(&layout.document_structure);
        assert_eq!(layout.known_chunk_count(), 1);
        assert!(!layout.materialized_chunks.is_empty());
        for width in [80, 100, 120] {
            layout.prepare(&document, 1, width);
            assert!(Arc::ptr_eq(&document_structure, &layout.document_structure));
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
        let old_structure = Arc::clone(&layout.document_structure);
        layout.prepare(&first, 1, 80);
        assert!(Arc::ptr_eq(&old_structure, &layout.document_structure));

        layout.prepare(&second, 2, 80);
        assert!(!Arc::ptr_eq(&old_structure, &layout.document_structure));
        assert_eq!(layout.document_structure_builds, 2);
        assert_eq!(layout.document_structure.logical_line_count, 2);
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
        let old_structure = Arc::clone(&layout.document_structure);
        layout.prepare(&split_segments, 1, 80);

        assert!(!Arc::ptr_eq(&old_structure, &layout.document_structure));
        assert_eq!(layout.document_structure_builds, 2);
        assert_eq!(
            layout.document_structure.logical_line_count,
            old_structure.logical_line_count
        );
    }

    #[test]
    fn newline_heavy_document_keeps_the_same_structure_across_widths() {
        let raw = many_lines(100_000);
        let segments = [raw.as_str()];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();

        layout.prepare(&document, 1, 120);
        let document_structure = Arc::clone(&layout.document_structure);
        for width in [80, 100, 120] {
            layout.prepare(&document, 1, width);
        }

        assert!(Arc::ptr_eq(&document_structure, &layout.document_structure));
        assert_eq!(layout.document_structure_builds, 1);
        assert_eq!(layout.document_structure.logical_line_count, 100_001);
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
    fn background_chunk_counts_match_materialized_chunk_counts() {
        let logical =
            "ASCII words  supercalifragilisticexpialidocious 日本語 e\u{301} 👩‍💻 \u{200b}\n\n";
        let raw = logical.repeat(3_000);
        let segments = ["prefix", raw.as_str(), "trailing\n"];
        let document = make_document(&segments);
        let mut layout = DetailLayout::default();
        layout.viewport(&document, 8, 14, 20, 0);
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
                .document_structure
                .units
                .iter()
                .any(StructuralUnit::is_giant)
        );
        assert!(
            layout
                .document_structure
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
            |identity: &mut DetailCountIdentity| identity.generation += 1,
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
        layout.stage_count_command(&large);
        assert!(matches!(
            layout.take_count_command(),
            Some(DetailCountCommand::Count(_))
        ));
        layout.stage_count_command(&large);
        assert!(layout.take_count_command().is_none());

        layout.viewport(&small, 2, 80, 20, 0);
        layout.stage_count_command(&small);
        assert!(matches!(
            layout.take_count_command(),
            Some(DetailCountCommand::Cancel { .. })
        ));
        layout.stage_count_command(&small);
        assert!(layout.take_count_command().is_none());

        let mut eager_only = DetailLayout::default();
        eager_only.viewport(&small, 1, 80, 20, 0);
        eager_only.stage_count_command(&small);
        assert!(eager_only.take_count_command().is_none());
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
        assert!(Arc::ptr_eq(&document_structure, &layout.document_structure));
        let old_result = count_result(old_request);

        layout.viewport(&document, 1, 60, 20, 0);
        let new_request = take_count_request(&mut layout, &document);
        assert!(Arc::ptr_eq(&document_structure, &layout.document_structure));
        assert!(Arc::ptr_eq(&document_structure, &new_request.structure));
        assert_eq!(layout.document_structure_builds, 1);
        assert_ne!(
            old_result.identity.generation,
            new_request.identity.generation
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
