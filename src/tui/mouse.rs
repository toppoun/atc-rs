#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PixelCoordinateOrigin {
    ZeroBased,
    #[allow(
        dead_code,
        reason = "kept explicit for future terminals with a verified one-based origin"
    )]
    OneBased,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TerminalPixelMetrics {
    pub(super) columns: u16,
    pub(super) rows: u16,
    pub(super) area_width_px: u32,
    pub(super) area_height_px: u32,
    pub(super) cell_width_px: u32,
    pub(super) cell_height_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalPixelMetricsError {
    Malformed,
    Inconsistent,
}

impl TerminalPixelMetrics {
    #[cfg(test)]
    pub(super) fn validated(
        columns: u16,
        rows: u16,
        area_width_px: u32,
        area_height_px: u32,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Option<Self> {
        Self::validate(
            columns,
            rows,
            area_width_px,
            area_height_px,
            cell_width_px,
            cell_height_px,
        )
        .ok()
    }

    pub(super) fn validate(
        columns: u16,
        rows: u16,
        area_width_px: u32,
        area_height_px: u32,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<Self, TerminalPixelMetricsError> {
        if columns == 0
            || rows == 0
            || area_width_px == 0
            || area_height_px == 0
            || cell_width_px == 0
            || cell_height_px == 0
        {
            return Err(TerminalPixelMetricsError::Malformed);
        }

        let expected_width = u64::from(columns)
            .checked_mul(u64::from(cell_width_px))
            .ok_or(TerminalPixelMetricsError::Malformed)?;
        let expected_height = u64::from(rows)
            .checked_mul(u64::from(cell_height_px))
            .ok_or(TerminalPixelMetricsError::Malformed)?;
        if expected_width != u64::from(area_width_px)
            || expected_height != u64::from(area_height_px)
        {
            return Err(TerminalPixelMetricsError::Inconsistent);
        }

        Ok(Self {
            columns,
            rows,
            area_width_px,
            area_height_px,
            cell_width_px,
            cell_height_px,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MouseMode {
    Disabled,
    Cells,
    Pixels {
        metrics: TerminalPixelMetrics,
        origin: PixelCoordinateOrigin,
        generation: u64,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum HighResRetry {
    #[default]
    None,
    AfterInitialResize,
    ReadyAfterRedraw,
}

impl HighResRetry {
    pub(super) fn schedule_after_initial_resize(&mut self) {
        if *self == Self::None {
            *self = Self::AfterInitialResize;
        }
    }

    pub(super) fn observe_resize_boundary(&mut self) {
        if *self == Self::AfterInitialResize {
            *self = Self::ReadyAfterRedraw;
        }
    }

    pub(super) fn take_after_redraw(&mut self, resize_pending: bool) -> bool {
        if resize_pending || *self != Self::ReadyAfterRedraw {
            return false;
        }
        *self = Self::None;
        true
    }
}

impl MouseMode {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Disabled => "unavailable",
            Self::Cells => "standard (SGR 1006 cells)",
            Self::Pixels { .. } => "high-resolution (SGR 1016 pixels)",
        }
    }
}

pub(super) fn trusted_pixel_origin(term_program: Option<&str>) -> Option<PixelCoordinateOrigin> {
    // Phase 4 intentionally limits high-resolution input to VS Code's xterm.js implementation.
    // The zero-based policy has been dogfooded on Windows; it is kept platform-independent because
    // VS Code uses the same terminal implementation on macOS, where Pixels1016 dogfood is still a
    // separate validation gap. Other terminals retain full SGR 1006 support until their coordinate
    // origin and metric semantics are independently established.
    term_program
        .is_some_and(|program| program.eq_ignore_ascii_case("vscode"))
        .then_some(PixelCoordinateOrigin::ZeroBased)
}

pub(super) fn normalize_absolute_pixels(
    metrics: TerminalPixelMetrics,
    origin: PixelCoordinateOrigin,
    raw_x: u32,
    raw_y: u32,
) -> Option<(u32, u32)> {
    Some((
        normalize_axis(raw_x, metrics.area_width_px, origin)?,
        normalize_axis(raw_y, metrics.area_height_px, origin)?,
    ))
}

pub(super) fn project_absolute_pixels_to_cells(
    metrics: TerminalPixelMetrics,
    origin: PixelCoordinateOrigin,
    raw_x: u32,
    raw_y: u32,
) -> Option<(u16, u16)> {
    let (x, y) = normalize_absolute_pixels(metrics, origin, raw_x, raw_y)?;
    let column = x / metrics.cell_width_px;
    let row = y / metrics.cell_height_px;

    let column = u16::try_from(column).ok()?;
    let row = u16::try_from(row).ok()?;
    (column < metrics.columns && row < metrics.rows).then_some((column, row))
}

fn normalize_axis(raw: u32, area_size: u32, origin: PixelCoordinateOrigin) -> Option<u32> {
    match origin {
        PixelCoordinateOrigin::ZeroBased => (raw < area_size).then_some(raw),
        PixelCoordinateOrigin::OneBased => raw
            .checked_sub(1)
            .filter(|normalized| *normalized < area_size),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> TerminalPixelMetrics {
        TerminalPixelMetrics::validated(80, 24, 800, 480, 10, 20).unwrap()
    }

    #[test]
    fn metrics_require_positive_exact_cell_area_consistency() {
        assert!(TerminalPixelMetrics::validated(80, 24, 800, 480, 10, 20).is_some());
        assert!(TerminalPixelMetrics::validated(0, 24, 800, 480, 10, 20).is_none());
        assert!(TerminalPixelMetrics::validated(80, 24, 0, 480, 10, 20).is_none());
        assert!(TerminalPixelMetrics::validated(80, 24, 801, 480, 10, 20).is_none());
        assert!(TerminalPixelMetrics::validated(80, 24, 800, 479, 10, 20).is_none());
        assert!(
            TerminalPixelMetrics::validated(
                u16::MAX,
                u16::MAX,
                u32::MAX,
                u32::MAX,
                u32::MAX,
                u32::MAX
            )
            .is_none()
        );
    }

    #[test]
    fn zero_based_projection_has_exact_boundaries_without_clamping_or_wrapping() {
        let metrics = metrics();
        let origin = PixelCoordinateOrigin::ZeroBased;

        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 0, 0),
            Some((0, 0))
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 9, 19),
            Some((0, 0))
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 10, 20),
            Some((1, 1))
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 799, 479),
            Some((79, 23))
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 800, 0),
            None
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 0, 480),
            None
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, u32::MAX, u32::MAX),
            None
        );
    }

    #[test]
    fn adjacent_pixels_share_a_cell_but_remain_distinct_after_normalization() {
        let metrics = metrics();
        let origin = PixelCoordinateOrigin::ZeroBased;

        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 30, 101),
            Some((3, 5))
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 30, 102),
            Some((3, 5))
        );
        assert_eq!(
            normalize_absolute_pixels(metrics, origin, 30, 101),
            Some((30, 101))
        );
        assert_eq!(
            normalize_absolute_pixels(metrics, origin, 30, 102),
            Some((30, 102))
        );
    }

    #[test]
    fn one_based_policy_is_explicit_and_checked() {
        let metrics = metrics();
        let origin = PixelCoordinateOrigin::OneBased;

        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 1, 1),
            Some((0, 0))
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 800, 480),
            Some((79, 23))
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 0, 1),
            None
        );
        assert_eq!(
            project_absolute_pixels_to_cells(metrics, origin, 801, 1),
            None
        );
    }

    #[test]
    fn only_vscode_xterm_js_has_a_phase_four_origin_policy() {
        assert_eq!(
            trusted_pixel_origin(Some("vscode")),
            Some(PixelCoordinateOrigin::ZeroBased)
        );
        assert_eq!(
            trusted_pixel_origin(Some("VSCODE")),
            Some(PixelCoordinateOrigin::ZeroBased)
        );
        assert_eq!(trusted_pixel_origin(Some("WezTerm")), None);
        assert_eq!(trusted_pixel_origin(None), None);
    }

    #[test]
    fn initial_resize_retry_arms_at_boundary_and_is_consumed_exactly_once() {
        let mut retry = HighResRetry::default();
        retry.schedule_after_initial_resize();
        assert_eq!(retry, HighResRetry::AfterInitialResize);
        assert!(!retry.take_after_redraw(false));

        retry.observe_resize_boundary();
        assert_eq!(retry, HighResRetry::ReadyAfterRedraw);
        retry.observe_resize_boundary();
        assert!(!retry.take_after_redraw(true));
        assert_eq!(retry, HighResRetry::ReadyAfterRedraw);
        assert!(retry.take_after_redraw(false));
        assert_eq!(retry, HighResRetry::None);
        assert!(!retry.take_after_redraw(false));

        retry.observe_resize_boundary();
        assert_eq!(retry, HighResRetry::None);
        assert!(!retry.take_after_redraw(false));
    }
}
