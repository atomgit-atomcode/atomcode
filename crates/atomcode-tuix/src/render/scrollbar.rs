//! Pure compute for the right-edge scrollbar. Both renderers call into
//! `compute()` to decide thumb shape, then call `paint_row(...)` to emit
//! a single column's worth of cells per body row.

/// Vertical thumb position + height in body-region coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarShape {
    pub thumb_top: usize,     // 0-indexed body row
    pub thumb_height: usize,
}

/// Returns None when no thumb should be drawn (no overflow or disabled).
///
/// Thumb-height policy:
///   * Base size = `visible * visible / total` (proportional to the
///     fraction of total content currently in view).
///   * Floored at 1 so a thumb is always drawable.
///   * Capped at `visible / 2` (with a floor of 1) so the thumb never
///     occupies more than half the track. Without the cap, an overflow
///     of just 1–3 rows produces a thumb that covers ~95% of the track,
///     which reads visually as a solid block column running down the
///     right edge ("right-edge garbage") rather than a scrollbar — see
///     regression test `compute_thumb_capped_when_overflow_is_small`.
pub fn compute(
    total: usize,
    visible: usize,
    viewport_top: usize,
    sticky_bottom: bool,
    show: bool,
) -> Option<ScrollbarShape> {
    if !show || total <= visible || visible == 0 {
        return None;
    }
    let max_top = total - visible;
    let effective_top = if sticky_bottom { max_top } else { viewport_top };
    let raw_thumb_h = ((visible * visible) / total).max(1);
    let max_thumb_h = (visible / 2).max(1);
    let thumb_h = raw_thumb_h.min(max_thumb_h);
    let track_avail = visible.saturating_sub(thumb_h);
    let thumb_top = if max_top == 0 {
        0
    } else {
        effective_top * track_avail / max_top
    };
    Some(ScrollbarShape { thumb_top, thumb_height: thumb_h })
}

/// Whether the given body row index (0..visible) should paint a thumb char.
pub fn is_thumb_row(shape: &ScrollbarShape, body_row: usize) -> bool {
    body_row >= shape.thumb_top && body_row < shape.thumb_top + shape.thumb_height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_returns_none_when_no_overflow() {
        assert!(compute(10, 20, 0, true, true).is_none());
        assert!(compute(20, 20, 0, true, true).is_none());
    }

    #[test]
    fn compute_returns_none_when_disabled() {
        assert!(compute(50, 10, 5, false, false).is_none());
    }

    #[test]
    fn compute_thumb_height_proportional_to_visible_over_total() {
        let s = compute(30, 10, 0, true, true).unwrap();
        // 10 * 10 / 30 = 3
        assert_eq!(s.thumb_height, 3);
    }

    #[test]
    fn compute_thumb_at_bottom_when_sticky() {
        let s = compute(30, 10, 0, true, true).unwrap();
        // sticky_bottom => effective_top = max_top = 20
        // thumb_top = 20 * (10 - 3) / 20 = 7
        assert_eq!(s.thumb_top, 7);
    }

    #[test]
    fn compute_thumb_at_top_when_viewport_top_zero() {
        let s = compute(30, 10, 0, false, true).unwrap();
        assert_eq!(s.thumb_top, 0);
    }

    #[test]
    fn is_thumb_row_covers_thumb_range() {
        let shape = ScrollbarShape { thumb_top: 3, thumb_height: 4 };
        assert!(!is_thumb_row(&shape, 2));
        assert!(is_thumb_row(&shape, 3));
        assert!(is_thumb_row(&shape, 6));
        assert!(!is_thumb_row(&shape, 7));
    }

    /// Regression: when total only barely overflows visible (e.g.
    /// total=21, visible=20), the raw formula `visible*visible/total`
    /// yields a thumb that covers 19 of 20 rows — visually a solid
    /// column with only 1 row of track. The user perceives this as
    /// "right-edge garbage", a column of block characters running
    /// down most/all of the body height. Cap the thumb to leave at
    /// least half the track visible so the bar always reads as a
    /// scrollbar instead of a fill.
    #[test]
    fn compute_thumb_capped_when_overflow_is_small() {
        let s = compute(21, 20, 1, true, true).unwrap();
        // Without the cap, thumb_height would be (20*20)/21 = 19.
        // With the cap, thumb must leave ≥ visible/2 rows of track.
        assert!(
            s.thumb_height <= 20 / 2,
            "thumb_height must be capped to ≤ visible/2 to avoid the \
             near-full-column visual; got {}",
            s.thumb_height
        );
        assert!(s.thumb_height >= 1, "thumb_height must remain ≥ 1");
    }

    /// Regression: even at the capped size, the thumb must still sit
    /// at the BOTTOM of the track when `sticky_bottom=true` — the
    /// user is looking at the tail of the buffer, so the thumb's
    /// position must say "you are near the bottom".
    #[test]
    fn compute_capped_thumb_anchored_to_bottom_when_sticky() {
        let s = compute(21, 20, 1, true, true).unwrap();
        // thumb_top + thumb_height must equal the bottom of the track
        // (= visible) so the thumb's BOTTOM edge sits on the last row.
        assert_eq!(
            s.thumb_top + s.thumb_height,
            20,
            "sticky_bottom thumb must end at visible (got top={}, h={})",
            s.thumb_top,
            s.thumb_height
        );
    }

    /// Cap only kicks in for SMALL overflow. Large overflow (visible
    /// is a small fraction of total) keeps the proportional formula
    /// untouched so the thumb still communicates relative position.
    #[test]
    fn compute_thumb_uncapped_for_large_overflow() {
        let s = compute(200, 20, 90, true, true).unwrap();
        // 20 * 20 / 200 = 2 → already small, cap doesn't apply.
        assert_eq!(s.thumb_height, 2);
    }
}
