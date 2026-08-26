use ratatui::{
    Frame,
    buffer::{Buffer, Cell},
    layout::Rect,
    style::Modifier,
};

use super::detail_layout::{DetailExactLayoutIdentity, DetailSectionVisualRow};

const TRACK_SYMBOL: &str = " ";
const THUMB_SYMBOL: &str = "█";
const LOWER_BLOCK_SYMBOLS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
const TOP_CAP_SYMBOL: &str = "↑";
const BOTTOM_CAP_SYMBOL: &str = "↓";
const MARKER_SYMBOL: &str = "•";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VerticalScrollbarGeometry {
    top_cap_row: Option<u16>,
    bottom_cap_row: Option<u16>,
    track_start_row: u16,
    track_len: u16,
    thumb_len: u16,
    thumb_start_row: u16,
    thumb_travel: u16,
}

impl VerticalScrollbarGeometry {
    pub(super) fn new(
        height: u16,
        max_scroll: usize,
        scroll: usize,
        viewport_height: usize,
    ) -> Option<Self> {
        if height == 0 || max_scroll == 0 {
            return None;
        }

        let top_cap_row = Some(0);
        let bottom_cap_row = (height >= 2).then_some(height.saturating_sub(1));
        let track_start_row = 1;
        let track_len = height.saturating_sub(2);
        let (thumb_len, thumb_travel) = thumb_metrics(max_scroll, viewport_height, track_len);
        let thumb_offset = scroll_to_thumb_offset(scroll, max_scroll, thumb_travel);

        Some(Self {
            top_cap_row,
            bottom_cap_row,
            track_start_row,
            track_len,
            thumb_len,
            thumb_start_row: track_start_row.saturating_add(thumb_offset),
            thumb_travel,
        })
    }

    pub(super) fn symbol_at(self, row: u16) -> &'static str {
        if self.top_cap_row == Some(row) {
            TOP_CAP_SYMBOL
        } else if self.bottom_cap_row == Some(row) {
            BOTTOM_CAP_SYMBOL
        } else if row >= self.thumb_start_row
            && row < self.thumb_start_row.saturating_add(self.thumb_len)
        {
            THUMB_SYMBOL
        } else {
            TRACK_SYMBOL
        }
    }
}

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
    scroll: usize,
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

impl DetailScrollbarPixelProjection {
    pub(super) fn thumb_top_px(self) -> u64 {
        self.thumb_top_px
    }

    pub(super) fn thumb_bottom_px(self) -> u64 {
        self.thumb_top_px.saturating_add(self.thumb_height_px)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetailScrollbarPixelGeometry {
    projection: DetailScrollbarPixelProjection,
    cell_height_px: u32,
    generation: u64,
}

impl DetailScrollbarPixelGeometry {
    pub(super) fn new(
        geometry: &DetailScrollbarGeometry,
        cell_height_px: u32,
        generation: u64,
    ) -> Option<Self> {
        Some(Self {
            projection: geometry.pixel_projection(cell_height_px)?,
            cell_height_px,
            generation,
        })
    }

    fn matches(self, cell_height_px: u32, generation: u64) -> bool {
        self.cell_height_px == cell_height_px && self.generation == generation
    }

    fn grab_offset(self, normalized_y_px: u32) -> Option<u64> {
        let pointer = u64::from(normalized_y_px);
        let offset = pointer.checked_sub(self.projection.thumb_top_px())?;
        (pointer < self.projection.thumb_bottom_px()).then_some(offset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThumbCellFill {
    None,
    Lower(u8),
    Full,
    Upper(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FractionalThumbGeometry {
    top_row: u16,
    thumb_len: u16,
    edge_level: u8,
}

impl FractionalThumbGeometry {
    fn new(projection: DetailScrollbarPixelProjection, cell_height_px: u32) -> Option<Self> {
        if cell_height_px == 0 || projection.thumb_height_px == 0 {
            return None;
        }

        let cell_height_px = u64::from(cell_height_px);
        let top_row = u16::try_from(projection.thumb_top_px / cell_height_px).ok()?;
        let thumb_len = u16::try_from(projection.thumb_height_px / cell_height_px).ok()?;
        if thumb_len == 0 || u64::from(thumb_len) * cell_height_px != projection.thumb_height_px {
            return None;
        }

        let offset_in_cell = projection.thumb_top_px % cell_height_px;
        let edge_level = quantize_edge_offset(offset_in_cell, cell_height_px);
        Some(Self {
            top_row,
            thumb_len,
            edge_level,
        })
    }

    fn fill(self, row: u16) -> ThumbCellFill {
        if self.edge_level == 0 {
            return if row >= self.top_row && row < self.top_row.saturating_add(self.thumb_len) {
                ThumbCellFill::Full
            } else {
                ThumbCellFill::None
            };
        }

        if row == self.top_row {
            return ThumbCellFill::Lower(8 - self.edge_level);
        }
        let bottom_row = self.top_row.saturating_add(self.thumb_len);
        if row == bottom_row {
            return ThumbCellFill::Upper(self.edge_level);
        }
        if row > self.top_row && row < bottom_row {
            return ThumbCellFill::Full;
        }
        ThumbCellFill::None
    }
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
        let visual =
            VerticalScrollbarGeometry::new(area.height, max_scroll, scroll, viewport_height)?;
        let top_cap_row = visual.top_cap_row.map(|row| area.y.saturating_add(row));
        let bottom_cap_row = visual.bottom_cap_row.map(|row| area.y.saturating_add(row));
        let track_start_row = area.y.saturating_add(visual.track_start_row);
        let track_len = visual.track_len;
        let thumb_len = visual.thumb_len;
        let thumb_travel = visual.thumb_travel;
        let scroll = scroll.min(max_scroll);
        let thumb_start_row = area.y.saturating_add(visual.thumb_start_row);

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
            scroll,
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
        if cell_height_px == 0 || self.thumb_len == 0 {
            return None;
        }
        let cell_height_px = u64::from(cell_height_px);
        let thumb_travel_px = u64::from(self.thumb_travel).checked_mul(cell_height_px)?;
        let thumb_offset_px = proportional_map(self.scroll, self.max_scroll, thumb_travel_px);
        let track_top_px = u64::from(self.track_start_row).checked_mul(cell_height_px)?;
        Some(DetailScrollbarPixelProjection {
            track_top_px,
            thumb_top_px: track_top_px.checked_add(thumb_offset_px)?,
            thumb_height_px: u64::from(self.thumb_len).checked_mul(cell_height_px)?,
            thumb_travel_px,
        })
    }

    #[cfg(test)]
    pub(super) fn pixel_grab_offset(
        &self,
        normalized_y_px: u32,
        cell_height_px: u32,
    ) -> Option<u64> {
        if !self.is_interactive() {
            return None;
        }
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
        if !self.is_interactive() {
            return 0;
        }
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
    pub(super) pixel_geometry: Option<DetailScrollbarPixelGeometry>,
}

impl DetailScrollbarInteraction {
    pub(super) fn new(
        layout: DetailExactLayoutIdentity,
        geometry: DetailScrollbarGeometry,
        pixel_geometry: Option<DetailScrollbarPixelGeometry>,
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
            pixel_geometry,
        })
    }

    pub(super) fn hit_test_pixels(
        &self,
        column: u16,
        row: u16,
        normalized_y_px: u32,
        cell_height_px: u32,
        generation: u64,
    ) -> Option<DetailScrollbarHit> {
        let Some(pixel_geometry) = self.pixel_geometry else {
            return self.geometry.hit_test(column, row);
        };
        if !pixel_geometry.matches(cell_height_px, generation)
            || !self.geometry.is_interactive()
            || column != self.geometry.gutter.x
        {
            return None;
        }
        if self.geometry.top_cap_row == Some(row) {
            return Some(DetailScrollbarHit::TopCap);
        }
        if self.geometry.bottom_cap_row == Some(row) {
            return Some(DetailScrollbarHit::BottomCap);
        }

        let pointer = u64::from(normalized_y_px);
        if pointer >= pixel_geometry.projection.thumb_top_px()
            && pointer < pixel_geometry.projection.thumb_bottom_px()
        {
            return Some(DetailScrollbarHit::Thumb {
                grab_offset: row.saturating_sub(self.geometry.track_start_row),
            });
        }
        (row >= self.geometry.track_start_row && row < self.geometry.track_end_row())
            .then_some(DetailScrollbarHit::Track)
    }

    pub(super) fn pixel_grab_offset(
        &self,
        normalized_y_px: u32,
        cell_height_px: u32,
        generation: u64,
    ) -> Option<u64> {
        let Some(pixel_geometry) = self.pixel_geometry else {
            let cell_height_px = u64::from(cell_height_px);
            let thumb_top_px =
                u64::from(self.geometry.thumb_start_row).checked_mul(cell_height_px)?;
            let thumb_height_px = u64::from(self.geometry.thumb_len).checked_mul(cell_height_px)?;
            let offset = u64::from(normalized_y_px).checked_sub(thumb_top_px)?;
            return (offset < thumb_height_px).then_some(offset);
        };
        if !pixel_geometry.matches(cell_height_px, generation) {
            return None;
        }
        pixel_geometry.grab_offset(normalized_y_px)
    }
}

pub(super) fn render_detail_scrollbar(
    frame: &mut Frame,
    geometry: &DetailScrollbarGeometry,
    pixel_geometry: Option<DetailScrollbarPixelGeometry>,
) {
    render_to_buffer(frame.buffer_mut(), geometry, pixel_geometry);
}

fn render_to_buffer(
    buffer: &mut Buffer,
    geometry: &DetailScrollbarGeometry,
    pixel_geometry: Option<DetailScrollbarPixelGeometry>,
) {
    let x = geometry.gutter.x;
    if let Some(row) = geometry.top_cap_row
        && let Some(cell) = buffer.cell_mut((x, row))
    {
        render_base_symbol(cell, TOP_CAP_SYMBOL);
    }
    if let Some(row) = geometry.bottom_cap_row
        && let Some(cell) = buffer.cell_mut((x, row))
    {
        render_base_symbol(cell, BOTTOM_CAP_SYMBOL);
    }
    for row in geometry.track_start_row..geometry.track_end_row() {
        if let Some(cell) = buffer.cell_mut((x, row)) {
            render_base_symbol(cell, TRACK_SYMBOL);
        }
    }
    for row in &geometry.marker_rows {
        if let Some(cell) = buffer.cell_mut((x, *row)) {
            render_base_symbol(cell, MARKER_SYMBOL);
        }
    }
    let fractional_thumb = pixel_geometry.and_then(|pixel_geometry| {
        FractionalThumbGeometry::new(pixel_geometry.projection, pixel_geometry.cell_height_px)
    });
    if let Some(fractional_thumb) = fractional_thumb {
        for row in geometry.track_start_row..geometry.track_end_row() {
            if let Some(cell) = buffer.cell_mut((x, row)) {
                render_thumb_fill(cell, fractional_thumb.fill(row));
            }
        }
    } else {
        for row in geometry.thumb_start_row..geometry.thumb_end_row() {
            if let Some(cell) = buffer.cell_mut((x, row)) {
                cell.set_symbol(THUMB_SYMBOL);
            }
        }
    }
}

fn render_base_symbol(cell: &mut Cell, symbol: &str) {
    cell.set_symbol(symbol);
    cell.modifier.remove(Modifier::REVERSED);
}

fn render_thumb_fill(cell: &mut Cell, fill: ThumbCellFill) {
    let (symbol, reverse) = match fill {
        ThumbCellFill::None => return,
        ThumbCellFill::Lower(level) => (lower_block_symbol(level), false),
        ThumbCellFill::Full => (THUMB_SYMBOL, false),
        ThumbCellFill::Upper(level) => (lower_block_symbol(8 - level), true),
    };
    cell.set_symbol(symbol);
    if reverse {
        cell.modifier.toggle(Modifier::REVERSED);
    }
}

fn lower_block_symbol(level: u8) -> &'static str {
    LOWER_BLOCK_SYMBOLS[usize::from(level.clamp(1, 8) - 1)]
}

fn quantize_edge_offset(offset_px: u64, cell_height_px: u64) -> u8 {
    if offset_px == 0 || cell_height_px == 0 {
        return 0;
    }
    if offset_px >= cell_height_px {
        return 8;
    }
    u8::try_from(round_ratio(
        u128::from(offset_px).saturating_mul(8),
        u128::from(cell_height_px),
    ))
    .unwrap_or(7)
    .clamp(1, 7)
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

fn proportional_map(value: usize, maximum: usize, extent: u64) -> u64 {
    if maximum == 0 || extent == 0 {
        return 0;
    }
    let rounded = round_ratio(
        (value.min(maximum) as u128).saturating_mul(u128::from(extent)),
        maximum as u128,
    );
    u64::try_from(rounded).unwrap_or(extent).min(extent)
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
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

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

    fn projection(thumb_top_px: u64, thumb_height_px: u64) -> DetailScrollbarPixelProjection {
        DetailScrollbarPixelProjection {
            track_top_px: 0,
            thumb_top_px,
            thumb_height_px,
            thumb_travel_px: 0,
        }
    }

    fn interaction(
        geometry: DetailScrollbarGeometry,
        cell_height_px: u32,
        generation: u64,
    ) -> DetailScrollbarInteraction {
        let pixel_geometry =
            DetailScrollbarPixelGeometry::new(&geometry, cell_height_px, generation).unwrap();
        DetailScrollbarInteraction::new(
            DetailExactLayoutIdentity {
                document_generation: 1,
                layout_generation: 2,
                revision: 3,
            },
            geometry,
            Some(pixel_geometry),
        )
        .unwrap()
    }

    #[test]
    fn no_scroll_or_no_cells_has_no_geometry() {
        assert!(geometry(10, 0, 0).is_none());
        assert!(DetailScrollbarGeometry::new(Rect::new(0, 0, 0, 10), 1, 0, 10, &[]).is_none());
        assert!(DetailScrollbarGeometry::new(Rect::new(0, 0, 10, 0), 1, 0, 0, &[]).is_none());
    }

    #[test]
    fn shared_vertical_geometry_keeps_detail_caps_and_thumb_mapping() {
        let top = VerticalScrollbarGeometry::new(6, 20, 0, 6).unwrap();
        let bottom = VerticalScrollbarGeometry::new(6, 20, 20, 6).unwrap();
        assert_eq!(top.symbol_at(0), TOP_CAP_SYMBOL);
        assert_eq!(top.symbol_at(5), BOTTOM_CAP_SYMBOL);
        assert_eq!(top.symbol_at(top.thumb_start_row), THUMB_SYMBOL);
        assert_eq!(
            bottom.thumb_start_row,
            bottom.track_start_row + bottom.thumb_travel
        );

        let detail = geometry(6, 20, 20).unwrap();
        assert_eq!(
            detail.thumb_start_row.saturating_sub(detail.gutter.y),
            bottom.thumb_start_row
        );
        assert_eq!(detail.thumb_len, bottom.thumb_len);
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
        render_to_buffer(&mut buffer, &one_row, None);
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
        render_to_buffer(&mut buffer, &two_rows, None);
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
    fn exact_pixel_projection_has_endpoints_midpoint_constant_height_and_cell_height_17() {
        let cell_height = 17;
        let top = geometry(20, 1_000, 0).unwrap();
        let middle = geometry(20, 1_000, 500).unwrap();
        let bottom = geometry(20, 1_000, 1_000).unwrap();
        let top_projection = top.pixel_projection(cell_height).unwrap();
        let middle_projection = middle.pixel_projection(cell_height).unwrap();
        let bottom_projection = bottom.pixel_projection(cell_height).unwrap();

        assert_eq!(top_projection.thumb_top_px, top_projection.track_top_px);
        assert_eq!(
            middle_projection.thumb_top_px,
            middle_projection.track_top_px
                + proportional_map(500, 1_000, middle_projection.thumb_travel_px)
        );
        assert_eq!(
            bottom_projection.thumb_top_px,
            bottom_projection.track_top_px + bottom_projection.thumb_travel_px
        );
        assert_eq!(
            top_projection.thumb_height_px,
            u64::from(top.thumb_len) * u64::from(cell_height)
        );
        assert_eq!(
            top_projection.thumb_height_px,
            middle_projection.thumb_height_px
        );
        assert_eq!(
            middle_projection.thumb_height_px,
            bottom_projection.thumb_height_px
        );
    }

    #[test]
    fn exact_pixel_projection_is_monotonic_for_huge_and_usize_max_scroll_ranges() {
        for maximum in [10_000_000_000usize, usize::MAX] {
            let positions = [
                0,
                maximum / 4,
                maximum / 2,
                maximum.saturating_sub(maximum / 4),
                maximum,
            ];
            let mut previous = 0;
            for scroll in positions {
                let projection = geometry(40, maximum, scroll)
                    .unwrap()
                    .pixel_projection(17)
                    .unwrap();
                assert!(projection.thumb_top_px >= previous);
                previous = projection.thumb_top_px;
            }
        }
    }

    #[test]
    fn zero_travel_and_full_track_thumb_have_stable_aligned_pixel_projection() {
        let geometry = geometry(3, usize::MAX, usize::MAX / 2).unwrap();
        assert_eq!(geometry.track_len, 1);
        assert_eq!(geometry.thumb_len, geometry.track_len);
        assert_eq!(geometry.thumb_travel, 0);

        let projection = geometry.pixel_projection(17).unwrap();
        assert_eq!(projection.thumb_top_px, projection.track_top_px);
        assert_eq!(projection.thumb_height_px, 17);
        assert_eq!(projection.thumb_travel_px, 0);
        assert_eq!(proportional_map(usize::MAX, usize::MAX, 0), 0);
        assert_eq!(proportional_map(1, 0, u64::MAX), 0);
    }

    #[test]
    fn proportional_pixel_mapping_resists_largest_integer_products() {
        assert_eq!(proportional_map(0, usize::MAX, u64::MAX), 0);
        assert_eq!(proportional_map(usize::MAX, usize::MAX, u64::MAX), u64::MAX);
        let before_end = proportional_map(usize::MAX - 1, usize::MAX, u64::MAX);
        assert!(before_end < u64::MAX);

        let geometry = DetailScrollbarGeometry::new(
            Rect::new(0, 60_000, 1, 20),
            usize::MAX,
            usize::MAX / 2,
            20,
            &[],
        )
        .unwrap();
        let projection = geometry.pixel_projection(u32::MAX).unwrap();
        assert!(projection.thumb_top_px >= projection.track_top_px);
        assert!(projection.thumb_bottom_px() > projection.thumb_top_px);
    }

    #[test]
    fn fractional_cell_coverage_handles_full_zero_and_exact_boundaries() {
        let aligned = FractionalThumbGeometry::new(projection(34, 34), 17).unwrap();
        assert_eq!(aligned.fill(1), ThumbCellFill::None);
        assert_eq!(aligned.fill(2), ThumbCellFill::Full);
        assert_eq!(aligned.fill(3), ThumbCellFill::Full);
        assert_eq!(aligned.fill(4), ThumbCellFill::None);

        let starts_one_pixel_below = FractionalThumbGeometry::new(projection(35, 17), 17).unwrap();
        assert_eq!(starts_one_pixel_below.fill(2), ThumbCellFill::Lower(7));
        assert_eq!(starts_one_pixel_below.fill(3), ThumbCellFill::Upper(1));

        let ends_one_pixel_before = FractionalThumbGeometry::new(projection(33, 17), 17).unwrap();
        assert_eq!(ends_one_pixel_before.fill(1), ThumbCellFill::Lower(1));
        assert_eq!(ends_one_pixel_before.fill(2), ThumbCellFill::Upper(7));
    }

    #[test]
    fn every_fractional_level_is_continuous_and_keeps_quantized_thumb_extent_constant() {
        for edge_level in 1..=7 {
            let fractional = FractionalThumbGeometry {
                top_row: 10,
                thumb_len: 3,
                edge_level,
            };
            assert_eq!(fractional.fill(10), ThumbCellFill::Lower(8 - edge_level));
            assert_eq!(fractional.fill(11), ThumbCellFill::Full);
            assert_eq!(fractional.fill(12), ThumbCellFill::Full);
            assert_eq!(fractional.fill(13), ThumbCellFill::Upper(edge_level));
            assert_eq!(fractional.fill(9), ThumbCellFill::None);
            assert_eq!(fractional.fill(14), ThumbCellFill::None);

            let total_eighths = u16::from(8 - edge_level) + u16::from(edge_level) + 2 * 8;
            assert_eq!(total_eighths, 3 * 8);
        }
    }

    #[test]
    fn edge_quantization_is_deterministic_for_small_target_and_large_cell_heights() {
        assert_eq!(quantize_edge_offset(1, 3), 3);
        assert_eq!(quantize_edge_offset(2, 3), 5);
        assert_eq!(quantize_edge_offset(1, 17), 1);
        assert_eq!(quantize_edge_offset(8, 17), 4);
        assert_eq!(quantize_edge_offset(16, 17), 7);
        assert_eq!(quantize_edge_offset(1, 10_000), 1);
        assert_eq!(quantize_edge_offset(9_999, 10_000), 7);

        let mut previous = 0;
        for offset in 0..17 {
            let current = quantize_edge_offset(offset, 17);
            assert!(current >= previous);
            assert!(current <= previous.saturating_add(1));
            previous = current;

            let fractional = FractionalThumbGeometry::new(projection(offset, 34), 17).unwrap();
            if offset == 0 {
                assert_eq!(fractional.fill(0), ThumbCellFill::Full);
                assert_eq!(fractional.fill(2), ThumbCellFill::None);
            } else {
                assert!(matches!(fractional.fill(0), ThumbCellFill::Lower(1..=7)));
                assert!(matches!(fractional.fill(2), ThumbCellFill::Upper(1..=7)));
            }
        }
    }

    #[test]
    fn lower_and_complementary_upper_glyph_mapping_preserves_existing_colors() {
        for level in 1..=8 {
            let mut cell = Cell::default();
            cell.fg = Color::Red;
            cell.bg = Color::Blue;
            cell.modifier = Modifier::BOLD;
            render_thumb_fill(
                &mut cell,
                if level == 8 {
                    ThumbCellFill::Full
                } else {
                    ThumbCellFill::Lower(level)
                },
            );
            assert_eq!(cell.symbol(), LOWER_BLOCK_SYMBOLS[usize::from(level - 1)]);
            assert_eq!(cell.fg, Color::Red);
            assert_eq!(cell.bg, Color::Blue);
            assert_eq!(cell.modifier, Modifier::BOLD);
        }

        for upper_level in 1..=7 {
            let mut cell = Cell::default();
            cell.fg = Color::Green;
            cell.bg = Color::Black;
            cell.modifier = Modifier::UNDERLINED;
            render_thumb_fill(&mut cell, ThumbCellFill::Upper(upper_level));
            assert_eq!(cell.symbol(), lower_block_symbol(8 - upper_level));
            assert_eq!(cell.fg, Color::Green);
            assert_eq!(cell.bg, Color::Black);
            assert!(cell.modifier.contains(Modifier::UNDERLINED));
            assert!(cell.modifier.contains(Modifier::REVERSED));
        }

        let mut already_reversed = Cell::default();
        already_reversed.modifier = Modifier::BOLD | Modifier::REVERSED;
        render_thumb_fill(&mut already_reversed, ThumbCellFill::Upper(4));
        assert_eq!(already_reversed.modifier, Modifier::BOLD);
    }

    #[test]
    fn all_scrollbar_glyphs_are_one_terminal_column() {
        for symbol in LOWER_BLOCK_SYMBOLS {
            assert_eq!(UnicodeWidthStr::width(symbol), 1, "{symbol:?}");
        }
        for symbol in [
            TRACK_SYMBOL,
            TOP_CAP_SYMBOL,
            BOTTOM_CAP_SYMBOL,
            MARKER_SYMBOL,
        ] {
            assert_eq!(UnicodeWidthStr::width(symbol), 1, "{symbol:?}");
        }
    }

    #[test]
    fn reused_buffer_clears_owned_reverse_for_every_non_upper_fill() {
        const CELL_HEIGHT_PX: u32 = 17;

        let mut geometry = geometry(20, 1_000, 0).unwrap();
        geometry.marker_rows.clear();
        let x = geometry.gutter.x;
        let target_row = geometry.track_start_row + 4;
        let pixel_geometry = |thumb_top_px| DetailScrollbarPixelGeometry {
            projection: projection(thumb_top_px, u64::from(CELL_HEIGHT_PX)),
            cell_height_px: CELL_HEIGHT_PX,
            generation: 1,
        };
        let upper = pixel_geometry(u64::from(target_row - 1) * u64::from(CELL_HEIGHT_PX) + 5);
        let away = pixel_geometry(u64::from(geometry.track_start_row) * u64::from(CELL_HEIGHT_PX));
        let full = pixel_geometry(u64::from(target_row) * u64::from(CELL_HEIGHT_PX));
        let lower = pixel_geometry(u64::from(target_row) * u64::from(CELL_HEIGHT_PX) + 5);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 40));
        let target = buffer.cell_mut((x, target_row)).unwrap();
        target.fg = Color::Red;
        target.bg = Color::Blue;
        target.modifier = Modifier::BOLD;

        render_to_buffer(&mut buffer, &geometry, Some(upper));
        let target = buffer.cell((x, target_row)).unwrap();
        assert!(target.modifier.contains(Modifier::BOLD));
        assert!(target.modifier.contains(Modifier::REVERSED));

        render_to_buffer(&mut buffer, &geometry, Some(away));
        let target = buffer.cell((x, target_row)).unwrap();
        assert_eq!(target.symbol(), TRACK_SYMBOL);
        assert_eq!(target.fg, Color::Red);
        assert_eq!(target.bg, Color::Blue);
        assert_eq!(target.modifier, Modifier::BOLD);

        render_to_buffer(&mut buffer, &geometry, Some(upper));
        render_to_buffer(&mut buffer, &geometry, Some(full));
        let target = buffer.cell((x, target_row)).unwrap();
        assert_eq!(target.symbol(), THUMB_SYMBOL);
        assert_eq!(target.modifier, Modifier::BOLD);

        render_to_buffer(&mut buffer, &geometry, Some(upper));
        render_to_buffer(&mut buffer, &geometry, Some(lower));
        let target = buffer.cell((x, target_row)).unwrap();
        assert!(LOWER_BLOCK_SYMBOLS.contains(&target.symbol()));
        assert_eq!(target.modifier, Modifier::BOLD);

        render_to_buffer(&mut buffer, &geometry, Some(upper));
        let mut with_marker = geometry.clone();
        with_marker.marker_rows = vec![target_row];
        render_to_buffer(&mut buffer, &with_marker, Some(away));
        let target = buffer.cell((x, target_row)).unwrap();
        assert_eq!(target.symbol(), MARKER_SYMBOL);
        assert_eq!(target.modifier, Modifier::BOLD);

        render_to_buffer(&mut buffer, &geometry, Some(upper));
        let cap_geometry =
            DetailScrollbarGeometry::new(Rect::new(x - 19, target_row, 20, 10), 1_000, 0, 10, &[])
                .unwrap();
        assert_eq!(cap_geometry.gutter.x, x);
        render_to_buffer(&mut buffer, &cap_geometry, None);
        let target = buffer.cell((x, target_row)).unwrap();
        assert_eq!(target.symbol(), TOP_CAP_SYMBOL);
        assert_eq!(target.modifier, Modifier::BOLD);
    }

    #[test]
    fn fractional_rendering_layers_partial_thumb_over_markers_and_preserves_caps_and_track() {
        let mut geometry = geometry(20, 10_000, 150).unwrap();
        let pixel_geometry = DetailScrollbarPixelGeometry::new(&geometry, 17, 9).unwrap();
        let fractional = FractionalThumbGeometry::new(pixel_geometry.projection, 17).unwrap();
        assert_ne!(fractional.edge_level, 0);
        let bottom_row = fractional.top_row + fractional.thumb_len;
        let untouched_marker = bottom_row + 2;
        geometry.marker_rows = vec![fractional.top_row, bottom_row, untouched_marker];

        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 40));
        render_to_buffer(&mut buffer, &geometry, Some(pixel_geometry));

        assert_eq!(
            buffer
                .cell((geometry.gutter.x, fractional.top_row))
                .unwrap()
                .symbol(),
            lower_block_symbol(8 - fractional.edge_level)
        );
        assert_eq!(
            buffer
                .cell((geometry.gutter.x, bottom_row))
                .unwrap()
                .symbol(),
            lower_block_symbol(8 - fractional.edge_level)
        );
        assert!(
            buffer
                .cell((geometry.gutter.x, bottom_row))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(
            buffer
                .cell((geometry.gutter.x, untouched_marker))
                .unwrap()
                .symbol(),
            MARKER_SYMBOL
        );
        assert_eq!(
            buffer
                .cell((geometry.gutter.x, geometry.top_cap_row.unwrap()))
                .unwrap()
                .symbol(),
            TOP_CAP_SYMBOL
        );
        assert_eq!(
            buffer
                .cell((geometry.gutter.x, geometry.bottom_cap_row.unwrap()))
                .unwrap()
                .symbol(),
            BOTTOM_CAP_SYMBOL
        );
    }

    #[test]
    fn fractional_renderer_has_no_stray_edge_cells_at_exact_scroll_endpoints() {
        for scroll in [0, 10_000] {
            let geometry = geometry(20, 10_000, scroll).unwrap();
            let pixel_geometry = DetailScrollbarPixelGeometry::new(&geometry, 17, 1).unwrap();
            let projection = pixel_geometry.projection;
            assert!(projection.thumb_top_px().is_multiple_of(17));
            let expected_start = u16::try_from(projection.thumb_top_px() / 17).unwrap();
            let expected_end = expected_start + geometry.thumb_len;

            let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 40));
            render_to_buffer(&mut buffer, &geometry, Some(pixel_geometry));
            for row in geometry.track_start_row..geometry.track_end_row() {
                let cell = buffer.cell((geometry.gutter.x, row)).unwrap();
                if row >= expected_start && row < expected_end {
                    assert_eq!(cell.symbol(), THUMB_SYMBOL);
                    assert!(!cell.modifier.contains(Modifier::REVERSED));
                } else {
                    assert_eq!(cell.symbol(), TRACK_SYMBOL);
                }
            }
        }

        let geometry = geometry(20, 10_000, 150).unwrap();
        assert!(DetailScrollbarPixelGeometry::new(&geometry, 0, 1).is_none());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 40));
        render_to_buffer(&mut buffer, &geometry, None);
        assert_eq!(
            buffer
                .cell((geometry.gutter.x, geometry.thumb_start_row))
                .unwrap()
                .symbol(),
            THUMB_SYMBOL
        );
    }

    #[test]
    fn fractional_descriptor_moves_while_whole_cell_thumb_row_is_unchanged() {
        let first = geometry(20, 10_000, 40).unwrap();
        let second = geometry(20, 10_000, 150).unwrap();
        assert_eq!(first.thumb_start_row, second.thumb_start_row);

        let first_fractional =
            FractionalThumbGeometry::new(first.pixel_projection(17).unwrap(), 17).unwrap();
        let second_fractional =
            FractionalThumbGeometry::new(second.pixel_projection(17).unwrap(), 17).unwrap();
        assert_eq!(first_fractional.top_row, second_fractional.top_row);
        assert_ne!(first_fractional.edge_level, second_fractional.edge_level);
        assert_ne!(
            first_fractional.fill(first_fractional.top_row),
            second_fractional.fill(second_fractional.top_row)
        );
    }

    #[test]
    fn pixel_hit_testing_uses_exact_half_open_fractional_thumb_interval() {
        let geometry = geometry(20, 10_000, 150).unwrap();
        let gutter = geometry.gutter.x;
        let interaction = interaction(geometry, 17, 41);
        let projection = interaction.pixel_geometry.unwrap().projection;
        assert_ne!(projection.thumb_top_px % 17, 0);
        let top = u32::try_from(projection.thumb_top_px).unwrap();
        let bottom = u32::try_from(projection.thumb_bottom_px()).unwrap();
        let top_row = u16::try_from(top / 17).unwrap();
        let bottom_row = u16::try_from(bottom / 17).unwrap();

        assert_eq!(
            interaction.hit_test_pixels(gutter, top_row, top, 17, 41),
            Some(DetailScrollbarHit::Thumb { grab_offset: 0 })
        );
        assert_eq!(
            interaction.hit_test_pixels(gutter, top_row, top - 1, 17, 41),
            Some(DetailScrollbarHit::Track)
        );
        assert!(matches!(
            interaction.hit_test_pixels(gutter, bottom_row, bottom - 1, 17, 41),
            Some(DetailScrollbarHit::Thumb { .. })
        ));
        assert_eq!(
            interaction.hit_test_pixels(gutter, bottom_row, bottom, 17, 41),
            Some(DetailScrollbarHit::Track)
        );
        assert_eq!(
            interaction.hit_test_pixels(gutter, top_row, top, 17, 42),
            None
        );
    }

    #[test]
    fn non_aligned_pixel_grab_offset_is_exact_and_same_pointer_drag_does_not_jump() {
        let base = geometry(20, 1_000, 0).unwrap();
        let travel_px = base.pixel_projection(17).unwrap().thumb_travel_px;
        let desired_offset = 37;
        assert!(desired_offset < travel_px);
        let scroll = pixel_thumb_offset_to_scroll(desired_offset, 1_000, travel_px);
        let geometry = geometry(20, 1_000, scroll).unwrap();
        let interaction = interaction(geometry.clone(), 17, 7);
        let projection = interaction.pixel_geometry.unwrap().projection;
        assert_eq!(
            projection.thumb_top_px - projection.track_top_px,
            desired_offset
        );
        assert_ne!(projection.thumb_top_px % 17, 0);

        let pointer = u32::try_from(projection.thumb_top_px + 3).unwrap();
        let grab_offset = interaction.pixel_grab_offset(pointer, 17, 7).unwrap();
        assert_eq!(grab_offset, 3);
        assert_eq!(
            geometry.scroll_for_pixel_drag(pointer, grab_offset, 17),
            scroll
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
        render_to_buffer(&mut buffer, &geometry, None);
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
