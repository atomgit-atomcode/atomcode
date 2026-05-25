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
    let thumb_h = ((visible * visible) / total).max(1);
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
}
