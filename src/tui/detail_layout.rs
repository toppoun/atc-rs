use std::collections::VecDeque;
use std::ops::Range;

use ratatui::text::{Line, Text};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::detail::DetailTextSource;

const DETAIL_CHUNK_LINES: usize = 256;
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
    logical_lines: Vec<LogicalLine>,
    chunks: Vec<ChunkMeta>,
    materialized_chunks: VecDeque<MaterializedChunk>,

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
            let logical_line = self.logical_lines[logical_index];
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

fn build_logical_line_index(document: &impl DetailTextSource) -> Vec<LogicalLine> {
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

    logical_lines
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
        wrap_logical_line(first, width, lines);
        return;
    }

    // segment境界がlogical lineの途中にある場合だけ、その1行を結合する。
    let capacity = fragments.iter().map(|fragment| fragment.len()).sum();
    let mut logical_line = String::with_capacity(capacity);
    for fragment in fragments {
        logical_line.push_str(fragment);
    }

    wrap_logical_line(&logical_line, width, lines);
}

fn wrap_logical_line(logical_line: &str, width: usize, lines: &mut Vec<Line<'static>>) {
    if width == 0 || logical_line.is_empty() {
        lines.push(Line::from(logical_line.to_owned()));
        return;
    }

    let mut current = String::new();
    let mut current_width = 0usize;

    for token in UnicodeSegmentation::split_word_bounds(logical_line) {
        let token_width = UnicodeWidthStr::width(token);

        if token_width <= width {
            if !current.is_empty() && current_width.saturating_add(token_width) > width {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }

            current.push_str(token);
            current_width = current_width.saturating_add(token_width);

            if current_width == width {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }

            continue;
        }

        if current_width > 0 {
            lines.push(Line::from(std::mem::take(&mut current)));
            current_width = 0;
        }

        for grapheme in UnicodeSegmentation::graphemes(token, true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);

            if current_width > 0 && current_width.saturating_add(grapheme_width) > width {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }

            current.push_str(grapheme);
            current_width = current_width.saturating_add(grapheme_width);

            if current_width >= width {
                lines.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }
        }
    }

    if !current.is_empty() {
        lines.push(Line::from(current));
    }
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
}
