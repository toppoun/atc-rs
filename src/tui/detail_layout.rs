use std::collections::VecDeque;
use std::ops::Range;

use ratatui::text::{Line, Text};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::detail::{DetailDocument, DetailSnapshot, DetailTextSource};

pub(super) const DETAIL_CHUNK_LINES: usize = 256;
const MATERIALIZED_CHUNK_CACHE_SIZE: usize = 3;
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

#[derive(Debug, Clone, Default)]
pub(crate) struct DetailLineIndex {
    lines: Vec<LogicalLine>,
}

impl DetailLineIndex {
    pub(super) fn len(&self) -> usize {
        self.lines.len()
    }

    pub(super) fn chunk_count(&self) -> usize {
        self.lines.len().div_ceil(DETAIL_CHUNK_LINES)
    }

    pub(super) fn count_chunks(
        &self,
        document: &impl DetailTextSource,
        width: usize,
        mut is_cancelled: impl FnMut() -> bool,
    ) -> Option<Vec<usize>> {
        let mut counts = Vec::with_capacity(self.chunk_count());

        for logical_range in (0..self.lines.len())
            .step_by(DETAIL_CHUNK_LINES)
            .map(|start| {
                start
                    ..start
                        .saturating_add(DETAIL_CHUNK_LINES)
                        .min(self.lines.len())
            })
        {
            if is_cancelled() {
                return None;
            }

            let mut visual_lines = 0usize;
            for logical_index in logical_range {
                if is_cancelled() {
                    return None;
                }

                let fragments = logical_line_fragments(document, self.lines[logical_index]);
                visual_lines = visual_lines.saturating_add(count_logical_line_fragments(
                    &fragments,
                    width,
                    &mut is_cancelled,
                )?);
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
    visual_lines: Option<usize>,
}

#[derive(Debug)]
struct MaterializedChunk {
    index: usize,
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
    count_generation: DetailCountGeneration,
    pending_count_command: Option<PendingCountCommand>,
    ready_count_command: Option<DetailCountCommand>,

    #[cfg(test)]
    chunk_layout_operations: usize,
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
            (0..logical_lines.len())
                .step_by(DETAIL_CHUNK_LINES)
                .map(|start| ChunkMeta {
                    logical_range: start
                        ..start
                            .saturating_add(DETAIL_CHUNK_LINES)
                            .min(logical_lines.len()),
                    visual_lines: None,
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
            self.ensure_chunk_materialized(document, chunk_index);

            let remaining = viewport_height.saturating_sub(viewport.len());
            let chunk_lines = self
                .materialized_chunks
                .iter()
                .find(|chunk| chunk.index == chunk_index)
                .expect("materialized detail chunk must be cached");
            let end = offset_in_chunk
                .saturating_add(remaining)
                .min(chunk_lines.lines.len());

            viewport.extend_from_slice(&chunk_lines.lines[offset_in_chunk..end]);

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

    DetailLineIndex {
        lines: logical_lines,
    }
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

trait WrapSink {
    fn has_content(&self) -> bool;
    fn push(&mut self, text: &str);
    fn emit(&mut self);
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
}

fn wrap_logical_line(
    logical_line: &str,
    width: usize,
    sink: &mut impl WrapSink,
    is_cancelled: &mut impl FnMut() -> bool,
) -> bool {
    if width == 0 || logical_line.is_empty() {
        sink.push(logical_line);
        sink.emit();
        return true;
    }

    let mut current_width = 0usize;

    for token in UnicodeSegmentation::split_word_bounds(logical_line) {
        if is_cancelled() {
            return false;
        }

        let token_width = UnicodeWidthStr::width(token);

        if token_width <= width {
            if sink.has_content() && current_width.saturating_add(token_width) > width {
                sink.emit();
                current_width = 0;
            }

            sink.push(token);
            current_width = current_width.saturating_add(token_width);

            if current_width == width {
                sink.emit();
                current_width = 0;
            }

            continue;
        }

        if current_width > 0 {
            sink.emit();
            current_width = 0;
        }

        for grapheme in UnicodeSegmentation::graphemes(token, true) {
            if is_cancelled() {
                return false;
            }

            let grapheme_width = UnicodeWidthStr::width(grapheme);

            if current_width > 0 && current_width.saturating_add(grapheme_width) > width {
                sink.emit();
                current_width = 0;
            }

            sink.push(grapheme);
            current_width = current_width.saturating_add(grapheme_width);

            if current_width >= width {
                sink.emit();
                current_width = 0;
            }
        }
    }

    if sink.has_content() {
        sink.emit();
    }

    true
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
