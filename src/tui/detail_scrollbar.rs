use ratatui::{Frame, buffer::Buffer, layout::Rect};

use super::detail_layout::{DetailExactLayoutIdentity, DetailSectionVisualRow};

const TRACK_SYMBOL: &str = "│";
const THUMB_SYMBOL: &str = "█";
const TOP_CAP_SYMBOL: &str = "↑";
const BOTTOM_CAP_SYMBOL: &str = "↓";
const MARKER_SYMBOL: &str = "•";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetailScrollbarHit {
    TopCap,
    BottomCap,
    Thumb { grab_offset: u16 },
    Track,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DetailScrollbarGeometry {
    pub(super) gutter: Rect,
    pub(super) top_cap_row: Option<u16>,
    pub(super) bottom_cap_row: Option<u16>,
    pub(super) track_start_row: u16,
    pub(super) track_len: u16,
    pub(super) thumb_len: u16,
    pub(super) thumb_start_row: u16,
    pub(super) thumb_travel: u16,
    pub(super) max_scroll: usize,
    pub(super) viewport_height: usize,
    pub(super) marker_rows: Vec<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetailScrollbarPixelProjection {
    track_top_px: u64,
    thumb_top_px: u64,
    thumb_height_px: u64,
    thumb_travel_px: u64,
}

impl DetailScrollbarGeometry {
    pub(super) fn new(
        area: Rect,
        max_scroll: usize,
        scroll: usize,
        viewport_height: usize,
        section_rows: &[DetailSectionVisualRow],
    ) -> Option<Self> {
        if area.width == 0 || area.height == 0 || max_scroll == 0 {
            return None;
        }

        let gutter = Rect::new(
            area.x.saturating_add(area.width.saturating_sub(1)),
            area.y,
            1,
            area.height,
        );
        let top_cap_row = Some(area.y);
        let bottom_cap_row =
            (area.height >= 2).then_some(area.y.saturating_add(area.height.saturating_sub(1)));
        let track_start_row = area.y.saturating_add(1);
        let track_len = area.height.saturating_sub(2);

        let (thumb_len, thumb_travel) = thumb_metrics(max_scroll, viewport_height, track_len);
        let thumb_offset = scroll_to_thumb_offset(scroll.min(max_scroll), max_scroll, thumb_travel);
        let thumb_start_row = track_start_row.saturating_add(thumb_offset);

        let mut marker_rows = if track_len == 0 {
            Vec::new()
        } else {
            section_rows
                .iter()
                .map(|section| {
                    let target_scroll = section.visual_row.min(max_scroll);
                    track_start_row.saturating_add(scroll_to_thumb_offset(
                        target_scroll,
                        max_scroll,
                        thumb_travel,
                    ))
                })
                .collect::<Vec<_>>()
        };
        marker_rows.sort_unstable();
        marker_rows.dedup();

        Some(Self {
            gutter,
            top_cap_row,
            bottom_cap_row,
            track_start_row,
            track_len,
            thumb_len,
            thumb_start_row,
            thumb_travel,
            max_scroll,
            viewport_height,
            marker_rows,
        })
    }

    pub(super) fn is_interactive(&self) -> bool {
        self.max_scroll > 0 && self.track_len >= 2 && self.thumb_travel > 0
    }

    pub(super) fn thumb_end_row(&self) -> u16 {
        self.thumb_start_row.saturating_add(self.thumb_len)
    }

    pub(super) fn track_end_row(&self) -> u16 {
        self.track_start_row.saturating_add(self.track_len)
    }

    pub(super) fn hit_test(&self, column: u16, row: u16) -> Option<DetailScrollbarHit> {
        if !self.is_interactive() || column != self.gutter.x {
            return None;
        }
        if self.top_cap_row == Some(row) {
            return Some(DetailScrollbarHit::TopCap);
        }
        if self.bottom_cap_row == Some(row) {
            return Some(DetailScrollbarHit::BottomCap);
        }
        if row >= self.thumb_start_row && row < self.thumb_end_row() {
            return Some(DetailScrollbarHit::Thumb {
                grab_offset: row.saturating_sub(self.thumb_start_row),
            });
        }
        (row >= self.track_start_row && row < self.track_end_row())
            .then_some(DetailScrollbarHit::Track)
    }

    pub(super) fn scroll_for_track_click(&self, row: u16) -> usize {
        if !self.is_interactive() {
            return 0;
        }
        let pointer = self.clamped_track_offset(row);
        let desired_start = pointer
            .saturating_sub(self.thumb_len / 2)
            .min(self.thumb_travel);
        thumb_offset_to_scroll(desired_start, self.max_scroll, self.thumb_travel)
    }

    pub(super) fn scroll_for_drag(&self, row: u16, grab_offset: u16) -> usize {
        if !self.is_interactive() {
            return 0;
        }
        let desired_start = self
            .clamped_track_offset(row)
            .saturating_sub(grab_offset)
            .min(self.thumb_travel);
        thumb_offset_to_scroll(desired_start, self.max_scroll, self.thumb_travel)
    }

    pub(super) fn pixel_projection(
        &self,
        cell_height_px: u32,
    ) -> Option<DetailScrollbarPixelProjection> {
        if !self.is_interactive() || cell_height_px == 0 {
            return None;
        }
        let cell_height_px = u64::from(cell_height_px);
        Some(DetailScrollbarPixelProjection {
            track_top_px: u64::from(self.track_start_row).checked_mul(cell_height_px)?,
            thumb_top_px: u64::from(self.thumb_start_row).checked_mul(cell_height_px)?,
            thumb_height_px: u64::from(self.thumb_len).checked_mul(cell_height_px)?,
            thumb_travel_px: u64::from(self.thumb_travel).checked_mul(cell_height_px)?,
        })
    }

    pub(super) fn pixel_grab_offset(
        &self,
        normalized_y_px: u32,
        cell_height_px: u32,
    ) -> Option<u64> {
        let projection = self.pixel_projection(cell_height_px)?;
        let pointer = u64::from(normalized_y_px);
        let offset = pointer.checked_sub(projection.thumb_top_px)?;
        (offset < projection.thumb_height_px).then_some(offset)
    }

    pub(super) fn scroll_for_pixel_drag(
        &self,
        normalized_y_px: u32,
        grab_offset_px: u64,
        cell_height_px: u32,
    ) -> usize {
        let Some(projection) = self.pixel_projection(cell_height_px) else {
            return 0;
        };
        let desired_thumb_top = u64::from(normalized_y_px).saturating_sub(grab_offset_px);
        let desired_offset = desired_thumb_top
            .saturating_sub(projection.track_top_px)
            .min(projection.thumb_travel_px);
        pixel_thumb_offset_to_scroll(desired_offset, self.max_scroll, projection.thumb_travel_px)
    }

    fn clamped_track_offset(&self, row: u16) -> u16 {
        row.saturating_sub(self.track_start_row)
            .min(self.track_len.saturating_sub(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetailScrollbarStableIdentity {
    pub(super) layout: DetailExactLayoutIdentity,
    gutter: Rect,
    track_start_row: u16,
    track_len: u16,
    thumb_len: u16,
    thumb_travel: u16,
    max_scroll: usize,
    viewport_height: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DetailScrollbarInteraction {
    pub(super) identity: DetailScrollbarStableIdentity,
    pub(super) geometry: DetailScrollbarGeometry,
}

impl DetailScrollbarInteraction {
    pub(super) fn new(
        layout: DetailExactLayoutIdentity,
        geometry: DetailScrollbarGeometry,
    ) -> Option<Self> {
        geometry.is_interactive().then_some(Self {
            identity: DetailScrollbarStableIdentity {
                layout,
                gutter: geometry.gutter,
                track_start_row: geometry.track_start_row,
                track_len: geometry.track_len,
                thumb_len: geometry.thumb_len,
                thumb_travel: geometry.thumb_travel,
                max_scroll: geometry.max_scroll,
                viewport_height: geometry.viewport_height,
            },
            geometry,
        })
    }
}

pub(super) fn render_detail_scrollbar(frame: &mut Frame, geometry: &DetailScrollbarGeometry) {
    render_to_buffer(frame.buffer_mut(), geometry);
}

fn render_to_buffer(buffer: &mut Buffer, geometry: &DetailScrollbarGeometry) {
    let x = geometry.gutter.x;
    if let Some(row) = geometry.top_cap_row
        && let Some(cell) = buffer.cell_mut((x, row))
    {
        cell.set_symbol(TOP_CAP_SYMBOL);
    }
    if let Some(row) = geometry.bottom_cap_row
        && let Some(cell) = buffer.cell_mut((x, row))
    {
        cell.set_symbol(BOTTOM_CAP_SYMBOL);
    }
    for row in geometry.track_start_row..geometry.track_end_row() {
        if let Some(cell) = buffer.cell_mut((x, row)) {
            cell.set_symbol(TRACK_SYMBOL);
        }
    }
    for row in &geometry.marker_rows {
        if let Some(cell) = buffer.cell_mut((x, *row)) {
            cell.set_symbol(MARKER_SYMBOL);
        }
    }
    for row in geometry.thumb_start_row..geometry.thumb_end_row() {
        if let Some(cell) = buffer.cell_mut((x, row)) {
            cell.set_symbol(THUMB_SYMBOL);
        }
    }
}

fn thumb_metrics(max_scroll: usize, viewport_height: usize, track_len: u16) -> (u16, u16) {
    if max_scroll == 0 || track_len == 0 {
        return (0, 0);
    }
    if track_len == 1 {
        return (1, 0);
    }

    let content = (max_scroll as u128).saturating_add(viewport_height as u128);
    let rounded = round_ratio(
        (viewport_height as u128).saturating_mul(track_len as u128),
        content,
    );
    let thumb_len = u16::try_from(rounded)
        .unwrap_or(track_len)
        .clamp(1, track_len.saturating_sub(1));
    (thumb_len, track_len.saturating_sub(thumb_len))
}

fn scroll_to_thumb_offset(scroll: usize, max_scroll: usize, travel: u16) -> u16 {
    if max_scroll == 0 || travel == 0 {
        return 0;
    }
    let rounded = round_ratio(
        (scroll.min(max_scroll) as u128).saturating_mul(travel as u128),
        max_scroll as u128,
    );
    u16::try_from(rounded).unwrap_or(travel).min(travel)
}

fn thumb_offset_to_scroll(offset: u16, max_scroll: usize, travel: u16) -> usize {
    if max_scroll == 0 || travel == 0 {
        return 0;
    }
    let rounded = round_ratio(
        (offset.min(travel) as u128).saturating_mul(max_scroll as u128),
        travel as u128,
    );
    usize::try_from(rounded)
        .unwrap_or(max_scroll)
        .min(max_scroll)
}

fn pixel_thumb_offset_to_scroll(offset: u64, max_scroll: usize, travel: u64) -> usize {
    if max_scroll == 0 || travel == 0 {
        return 0;
    }
    let rounded = round_ratio(
        u128::from(offset.min(travel)).saturating_mul(max_scroll as u128),
        u128::from(travel),
    );
    usize::try_from(rounded)
        .unwrap_or(max_scroll)
        .min(max_scroll)
}

fn round_ratio(numerator: u128, denominator: u128) -> u128 {
    if denominator == 0 {
        return 0;
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    let half_rounded_up = denominator / 2 + denominator % 2;
    quotient + u128::from(remainder >= half_rounded_up)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::detail::DetailSectionKind;

    fn section(kind: DetailSectionKind, visual_row: usize) -> DetailSectionVisualRow {
        DetailSectionVisualRow { kind, visual_row }
    }

    fn geometry(height: u16, max_scroll: usize, scroll: usize) -> Option<DetailScrollbarGeometry> {
        DetailScrollbarGeometry::new(
            Rect::new(10, 5, 20, height),
            max_scroll,
            scroll,
            usize::from(height),
            &[],
        )
    }

    #[test]
    fn no_scroll_or_no_cells_has_no_geometry() {
        assert!(geometry(10, 0, 0).is_none());
        assert!(DetailScrollbarGeometry::new(Rect::new(0, 0, 0, 10), 1, 0, 10, &[]).is_none());
        assert!(DetailScrollbarGeometry::new(Rect::new(0, 0, 10, 0), 1, 0, 0, &[]).is_none());
    }

    #[test]
    fn tiny_heights_render_safely_but_require_two_track_cells_for_interaction() {
        for height in 1..=3 {
            let geometry = geometry(height, 100, 0).unwrap();
            assert!(!geometry.is_interactive());
            assert_eq!(geometry.hit_test(29, 5), None);
        }
        assert!(geometry(4, 100, 0).unwrap().is_interactive());
    }

    #[test]
    fn markers_without_a_track_never_escape_the_gutter_or_cover_caps() {
        let sections = [section(DetailSectionKind::Input, 50)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 10));

        let one_row =
            DetailScrollbarGeometry::new(Rect::new(2, 2, 4, 1), 100, 0, 1, &sections).unwrap();
        buffer
            .cell_mut((one_row.gutter.x, one_row.gutter.y + 1))
            .unwrap()
            .set_symbol("outside");
        render_to_buffer(&mut buffer, &one_row);
        assert!(one_row.marker_rows.is_empty());
        assert_eq!(
            buffer
                .cell((one_row.gutter.x, one_row.gutter.y))
                .unwrap()
                .symbol(),
            TOP_CAP_SYMBOL
        );
        assert_eq!(
            buffer
                .cell((one_row.gutter.x, one_row.gutter.y + 1))
                .unwrap()
                .symbol(),
            "outside"
        );

        let two_rows =
            DetailScrollbarGeometry::new(Rect::new(2, 5, 4, 2), 100, 0, 2, &sections).unwrap();
        render_to_buffer(&mut buffer, &two_rows);
        assert!(two_rows.marker_rows.is_empty());
        assert_eq!(
            buffer
                .cell((two_rows.gutter.x, two_rows.bottom_cap_row.unwrap()))
                .unwrap()
                .symbol(),
            BOTTOM_CAP_SYMBOL
        );
    }

    #[test]
    fn scroll_mapping_has_exact_endpoints_and_is_monotonic() {
        let top = geometry(20, 10_000, 0).unwrap();
        let bottom = geometry(20, 10_000, 10_000).unwrap();
        assert_eq!(top.thumb_start_row, top.track_start_row);
        assert_eq!(
            bottom.thumb_start_row,
            bottom.track_start_row + bottom.thumb_travel
        );

        let mut last = 0;
        for scroll in 0..=10_000 {
            let current = scroll_to_thumb_offset(scroll, 10_000, top.thumb_travel);
            assert!(current >= last);
            last = current;
        }
    }

    #[test]
    fn reverse_mapping_has_exact_endpoints_and_is_monotonic() {
        let geometry = geometry(40, usize::MAX, 0).unwrap();
        assert_eq!(
            thumb_offset_to_scroll(0, usize::MAX, geometry.thumb_travel),
            0
        );
        assert_eq!(
            thumb_offset_to_scroll(geometry.thumb_travel, usize::MAX, geometry.thumb_travel),
            usize::MAX
        );
        let mut last = 0;
        for offset in 0..=geometry.thumb_travel {
            let current = thumb_offset_to_scroll(offset, usize::MAX, geometry.thumb_travel);
            assert!(current >= last);
            last = current;
        }
    }

    #[test]
    fn multi_cell_thumb_reports_every_grab_offset_without_jumping() {
        let geometry = geometry(30, 10, 4).unwrap();
        assert!(geometry.thumb_len > 1);
        for grab_offset in 0..geometry.thumb_len {
            assert_eq!(
                geometry.hit_test(geometry.gutter.x, geometry.thumb_start_row + grab_offset),
                Some(DetailScrollbarHit::Thumb { grab_offset })
            );
            let mapped =
                geometry.scroll_for_drag(geometry.thumb_start_row + grab_offset, grab_offset);
            assert_eq!(
                mapped,
                thumb_offset_to_scroll(
                    geometry.thumb_start_row - geometry.track_start_row,
                    geometry.max_scroll,
                    geometry.thumb_travel,
                )
            );
        }
    }

    #[test]
    fn track_click_centers_the_thumb_and_caps_are_distinct() {
        let geometry = geometry(20, 1_000, 500).unwrap();
        assert_eq!(
            geometry.hit_test(geometry.gutter.x, geometry.top_cap_row.unwrap()),
            Some(DetailScrollbarHit::TopCap)
        );
        assert_eq!(
            geometry.hit_test(geometry.gutter.x, geometry.bottom_cap_row.unwrap()),
            Some(DetailScrollbarHit::BottomCap)
        );
        let click = geometry.track_start_row + geometry.track_len / 3;
        let target = geometry.scroll_for_track_click(click);
        let target_start =
            scroll_to_thumb_offset(target, geometry.max_scroll, geometry.thumb_travel);
        let expected = (click - geometry.track_start_row)
            .saturating_sub(geometry.thumb_len / 2)
            .min(geometry.thumb_travel);
        assert_eq!(target_start, expected);
    }

    #[test]
    fn drag_y_clamps_above_and_below_track() {
        let geometry = geometry(20, 1_000, 500).unwrap();
        assert_eq!(geometry.scroll_for_drag(0, 0), 0);
        assert_eq!(geometry.scroll_for_drag(u16::MAX, 0), 1_000);
    }

    #[test]
    fn pixel_projection_preserves_grab_offset_and_adjacent_pixel_resolution() {
        let geometry = geometry(20, 1_000_000, 0).unwrap();
        let cell_height = 20;
        let thumb_top = u32::from(geometry.thumb_start_row) * cell_height;
        let grab = geometry
            .pixel_grab_offset(thumb_top + 13, cell_height)
            .unwrap();
        assert_eq!(grab, 13);
        assert_eq!(
            geometry.scroll_for_pixel_drag(thumb_top + 13, grab, cell_height),
            0
        );
        let first = geometry.scroll_for_pixel_drag(thumb_top + 14, grab, cell_height);
        let second = geometry.scroll_for_pixel_drag(thumb_top + 15, grab, cell_height);
        assert!(first > 0);
        assert!(second > first);
    }

    #[test]
    fn pixel_drag_has_exact_clamped_endpoints_for_the_largest_scroll_range() {
        let geometry = geometry(40, usize::MAX, usize::MAX / 2).unwrap();
        let cell_height = 20;
        let projection = geometry.pixel_projection(cell_height).unwrap();
        assert_eq!(geometry.scroll_for_pixel_drag(0, 0, cell_height), 0);
        let bottom = projection
            .track_top_px
            .saturating_add(projection.thumb_travel_px);
        let bottom = u32::try_from(bottom).unwrap();
        assert_eq!(
            geometry.scroll_for_pixel_drag(bottom, 0, cell_height),
            usize::MAX
        );
    }

    #[test]
    fn semantic_markers_use_reachable_scroll_mapping_and_deduplicate() {
        let sections = [
            section(DetailSectionKind::Input, 0),
            section(DetailSectionKind::Expected, 50),
            section(DetailSectionKind::Actual, 9_999),
            section(DetailSectionKind::Stderr, 10_000),
        ];
        let geometry =
            DetailScrollbarGeometry::new(Rect::new(0, 0, 20, 20), 100, 0, 20, &sections).unwrap();
        assert_eq!(
            geometry.marker_rows.first(),
            Some(&geometry.track_start_row)
        );
        assert_eq!(
            geometry.marker_rows.last(),
            Some(&(geometry.track_start_row + geometry.thumb_travel))
        );
        assert_eq!(geometry.marker_rows.len(), 3);
    }

    #[test]
    fn renderer_layers_thumb_over_marker_over_track() {
        let sections = [
            section(DetailSectionKind::Input, 0),
            section(DetailSectionKind::Expected, 50),
        ];
        let geometry =
            DetailScrollbarGeometry::new(Rect::new(0, 0, 5, 10), 100, 0, 10, &sections).unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 10));
        render_to_buffer(&mut buffer, &geometry);
        assert_eq!(
            buffer
                .cell((geometry.gutter.x, geometry.thumb_start_row))
                .unwrap()
                .symbol(),
            THUMB_SYMBOL
        );
        let marker_row = *geometry
            .marker_rows
            .iter()
            .find(|row| **row >= geometry.thumb_end_row())
            .unwrap();
        assert_eq!(
            buffer
                .cell((geometry.gutter.x, marker_row))
                .unwrap()
                .symbol(),
            MARKER_SYMBOL
        );
    }
}
