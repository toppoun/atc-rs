use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Range;

use ratatui::text::{Line, Text};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::detail::{DetailDocument, DetailSnapshot, DetailTextSource};

pub(super) const DETAIL_CHUNK_LINES: usize = 256;
const MATERIALIZED_CHUNK_CACHE_SIZE: usize = 3;
const GIANT_LOGICAL_LINE_BYTE_THRESHOLD: usize = 64 * 1024;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SegmentCursor {
    segment: usize,
    offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogicalLine {
    start: SegmentCursor,
    end: SegmentCursor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LayoutUnitIndex {
    logical_range: Range<usize>,
    giant_line: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DetailLineIndex {
    lines: Vec<LogicalLine>,
    units: Vec<LayoutUnitIndex>,
}

impl DetailLineIndex {
    pub(super) fn len(&self) -> usize {
        self.lines.len()
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

            let mut visual_lines = 0usize;
            for logical_index in unit.logical_range.clone() {
                if is_cancelled() {
                    return None;
                }

                let fragments = logical_line_fragments(document, self.lines[logical_index]);
                let line_count = if unit.giant_line {
                    count_giant_logical_line_fragments(&fragments, width, &mut is_cancelled)?
                } else {
                    count_logical_line_fragments(&fragments, width, &mut is_cancelled)?
                };
                visual_lines = visual_lines.saturating_add(line_count);
            }

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
    pub(super) line_index: DetailLineIndex,
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
    logical_range: Range<usize>,
    giant_line: bool,
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
    logical_lines: DetailLineIndex,
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
                line_index: self.logical_lines.clone(),
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

        if self.revision == Some(revision)
            && self.detail_width == detail_width
            && self.segment_lengths == segment_lengths
        {
            return;
        }

        let previous_mode = self.mode;
        let logical_lines = build_logical_line_index(document);
        let raw_bytes = segment_lengths
            .iter()
            .copied()
            .fold(0usize, usize::saturating_add);
        let mode = if raw_bytes >= LAZY_DETAIL_BYTE_THRESHOLD
            || logical_lines.len() >= LAZY_DETAIL_LINE_THRESHOLD
        {
            LayoutMode::Lazy
        } else {
            LayoutMode::Eager
        };

        let chunks = if mode == LayoutMode::Lazy {
            logical_lines
                .units
                .iter()
                .map(|unit| ChunkMeta {
                    logical_range: unit.logical_range.clone(),
                    giant_line: unit.giant_line,
                    visual_lines: None,
                    checkpoints: if unit.giant_line {
                        vec![WrapCheckpoint {
                            visual_row: 0,
                            raw_offset: 0,
                        }]
                    } else {
                        Vec::new()
                    },
                })
                .collect()
        } else {
            Vec::new()
        };

        self.revision = Some(revision);
        self.detail_width = detail_width;
        self.mode = Some(mode);
        self.segment_lengths = segment_lengths;
        self.logical_lines = logical_lines;
        self.chunks = chunks;
        self.materialized_chunks.clear();
        self.materialized_giant_pages.clear();
        self.count_generation = self.count_generation.wrapping_add(1);
        self.pending_count_command = match mode {
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
            if self.chunks[chunk_index].giant_line {
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
            if self.chunks[chunk_index].giant_line
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
        debug_assert!(!self.chunks[chunk_index].giant_line);
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

        let logical_range = self.chunks[chunk_index].logical_range.clone();
        let mut lines = Vec::new();

        for logical_index in logical_range {
            let logical_line = self.logical_lines.lines[logical_index];
            let fragments = logical_line_fragments(document, logical_line);
            wrap_logical_line_fragments(&fragments, self.lazy_layout_width(), &mut lines);
        }

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

            let logical_index = self.chunks[chunk_index].logical_range.start;
            let logical_line = self.logical_lines.lines[logical_index];
            let fragments = logical_line_fragments(document, logical_line);
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

fn build_logical_line_index(document: &impl DetailTextSource) -> DetailLineIndex {
    let mut logical_lines = Vec::new();
    let mut start = SegmentCursor {
        segment: 0,
        offset: 0,
    };

    for segment_index in 0..document.segment_count() {
        let text = document
            .segment_text(segment_index)
            .expect("detail segment index must exist");
        for (newline, _) in text.match_indices('\n') {
            logical_lines.push(LogicalLine {
                start,
                end: SegmentCursor {
                    segment: segment_index,
                    offset: newline,
                },
            });
            start = SegmentCursor {
                segment: segment_index,
                offset: newline.saturating_add(1),
            };
        }
    }

    logical_lines.push(LogicalLine {
        start,
        end: SegmentCursor {
            segment: document.segment_count(),
            offset: 0,
        },
    });

    let mut units = Vec::new();
    let mut normal_start = 0usize;

    for (logical_index, logical_line) in logical_lines.iter().copied().enumerate() {
        let giant_line =
            logical_line_byte_len(document, logical_line) >= GIANT_LOGICAL_LINE_BYTE_THRESHOLD;
        let normal_len = logical_index.saturating_sub(normal_start);

        if giant_line || normal_len == DETAIL_CHUNK_LINES {
            if normal_start < logical_index {
                units.push(LayoutUnitIndex {
                    logical_range: normal_start..logical_index,
                    giant_line: false,
                });
            }
            normal_start = logical_index;
        }

        if giant_line {
            units.push(LayoutUnitIndex {
                logical_range: logical_index..logical_index.saturating_add(1),
                giant_line: true,
            });
            normal_start = logical_index.saturating_add(1);
        }
    }

    if normal_start < logical_lines.len() {
        units.push(LayoutUnitIndex {
            logical_range: normal_start..logical_lines.len(),
            giant_line: false,
        });
    }

    DetailLineIndex {
        lines: logical_lines,
        units,
    }
}

fn logical_line_byte_len(document: &impl DetailTextSource, logical_line: LogicalLine) -> usize {
    logical_line_fragments(document, logical_line)
        .iter()
        .map(|fragment| fragment.len())
        .fold(0usize, usize::saturating_add)
}

fn logical_line_fragments(
    document: &impl DetailTextSource,
    logical_line: LogicalLine,
) -> Vec<&str> {
    let segment_count = document.segment_count();
    if logical_line.start.segment >= segment_count {
        return Vec::new();
    }

    let last_segment = if logical_line.end.segment < segment_count {
        logical_line.end.segment
    } else {
        segment_count.saturating_sub(1)
    };
    let mut fragments = Vec::with_capacity(
        last_segment
            .saturating_sub(logical_line.start.segment)
            .saturating_add(1),
    );

    for segment_index in logical_line.start.segment..=last_segment {
        let text = document
            .segment_text(segment_index)
            .expect("logical line index must reference an existing segment");
        let start = if segment_index == logical_line.start.segment {
            logical_line.start.offset
        } else {
            0
        };
        let end = if segment_index == logical_line.end.segment {
            logical_line.end.offset
        } else {
            text.len()
        };

        if let Some(fragment) = text.get(start..end) {
            fragments.push(fragment);
        } else {
            debug_assert!(false, "logical line index must use UTF-8 boundaries");
            return Vec::new();
        }
    }

    fragments
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

fn count_logical_line_fragments(
    fragments: &[&str],
    width: usize,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Option<usize> {
    let mut non_empty = fragments
        .iter()
        .copied()
        .filter(|fragment| !fragment.is_empty());
    let Some(first) = non_empty.next() else {
        return Some(1);
    };

    let mut sink = CountSink::default();
    if non_empty.next().is_none() {
        return wrap_logical_line(first, width, &mut sink, is_cancelled).then_some(sink.lines);
    }

    // segment境界がlogical lineの途中にある場合だけ、その1行を結合する。
    let capacity = fragments.iter().map(|fragment| fragment.len()).sum();
    let mut logical_line = String::with_capacity(capacity);
    for fragment in fragments {
        logical_line.push_str(fragment);
    }

    wrap_logical_line(&logical_line, width, &mut sink, is_cancelled).then_some(sink.lines)
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
    let estimated_bytes = row_budget
        .saturating_mul(width.max(1))
        .saturating_mul(4)
        .saturating_add(16);
    let mut window_bytes = GIANT_LINE_WINDOW_MIN_BYTES.max(estimated_bytes);

    loop {
        if is_cancelled() {
            return None;
        }

        let window = logical_line_window(fragments, raw_offset, window_bytes);
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
        scanned_bytes = scanned_bytes.saturating_add(progress.scanned_bytes);
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
            window_bytes = GIANT_LINE_WINDOW_MIN_BYTES.max(estimated_bytes);
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
        build_logical_line_index(document)
            .lines
            .into_iter()
            .map(|line| logical_line_fragments(document, line).concat())
            .collect()
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
            .line_index
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
        assert!(layout.chunks[0].giant_line);
        assert_eq!(layout.giant_page_layout_operations, 1);
        assert!(layout.giant_scanned_bytes < raw.len() / 10);
        assert_eq!(layout.materialized_giant_visual_line_count(), 128);
        assert_eq!(layout.giant_checkpoint_count(), 2);
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

        let window = logical_line_window(&fragments, 1_000, 4_096);

        assert!(matches!(window, Cow::Borrowed(_)));
        assert_eq!(window.len(), 4_096);
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
        let stale = count_result(take_count_request(&mut layout, &document));
        assert!(layout.giant_checkpoint_count() > 1);

        layout.viewport(&document, 1, 80, 20, 300);
        assert!(!layout.apply_count_result(stale));
        assert_eq!(layout.invalidations, 2);
        assert_eq!(layout.lazy_layout_width(), 79);
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
        layout.viewport(&document, 1, 80, 20, 3);
        assert_eq!(layout.invalidations, 1);

        let revised = layout.viewport(&changed_document, 2, 80, 20, 0);
        assert_eq!(layout.invalidations, 2);
        assert_eq!(text_lines(&revised.text)[0], "other-");

        layout.viewport(&changed_document, 2, 79, 20, 0);
        assert_eq!(layout.invalidations, 3);

        let structurally_changed = [changed.as_str(), "x"];
        let structurally_changed = make_document(&structurally_changed);
        layout.viewport(&structurally_changed, 2, 79, 20, 0);
        assert_eq!(layout.invalidations, 4);
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
            build_logical_line_index(&snapshot)
                .lines
                .into_iter()
                .map(|line| logical_line_fragments(&snapshot, line).concat())
                .collect::<Vec<_>>()
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
        let old_result = count_result(take_count_request(&mut layout, &document));

        layout.viewport(&document, 1, 60, 20, 0);
        let new_request = take_count_request(&mut layout, &document);
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
