//! Pure string/cell builders for the `/usage` modal — progress bar, sparkline,
//! heatmap, calendar layout, braille line chart. No I/O, no state.

const BLOCKS_V: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const BAR_PARTIAL: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// A `width`-cell horizontal bar filled to `percent` (0..=100), using 1/8-cell
/// partial blocks for a smooth edge.
pub fn progress_bar(percent: f64, width: usize) -> String {
    let p = percent.clamp(0.0, 100.0) / 100.0;
    let eighths = (p * width as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;
    let mut s = String::new();
    for _ in 0..full.min(width) {
        s.push('█');
    }
    let mut used = full.min(width);
    if used < width && rem > 0 {
        s.push(BAR_PARTIAL[rem]);
        used += 1;
    }
    for _ in used..width {
        s.push(' ');
    }
    s
}

/// A `width`-char sparkline of `values` (shared min→max scale). Empty values →
/// `width` spaces. `width` is downsampled/upsampled by even bucketing.
pub fn sparkline(values: &[u64], width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if values.is_empty() {
        return " ".repeat(width);
    }
    // Bucket values into `width` columns (average per bucket).
    let buckets: Vec<u64> = (0..width)
        .map(|i| {
            let lo = i * values.len() / width;
            let hi = ((i + 1) * values.len() / width).max(lo + 1).min(values.len());
            let slice = &values[lo..hi];
            slice.iter().sum::<u64>() / slice.len() as u64
        })
        .collect();
    let min = *buckets.iter().min().unwrap();
    let max = *buckets.iter().max().unwrap();
    buckets
        .iter()
        .map(|&v| {
            let level = if max == min {
                4 // flat → mid
            } else {
                1 + ((v - min) as f64 / (max - min) as f64 * 7.0).round() as usize
            };
            BLOCKS_V[level.min(8)]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bar_fills_proportionally() {
        assert_eq!(progress_bar(0.0, 10).chars().filter(|c| *c == '█').count(), 0);
        assert_eq!(progress_bar(100.0, 10), "██████████");
        // 9% of 10 cells ≈ 0.9 cell → at least one partial/edge cell, not full
        let b = progress_bar(9.0, 10);
        assert_eq!(b.chars().count(), 10);
        assert!(b.chars().next().unwrap() != ' ');
    }

    #[test]
    fn sparkline_maps_min_and_max() {
        let s = sparkline(&[0, 10], 2);
        let cs: Vec<char> = s.chars().collect();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0], '▁'); // min
        assert_eq!(cs[1], '█'); // max
        assert_eq!(sparkline(&[5, 5, 5], 3), "▄▄▄"); // all-equal → mid/flat, uniform
        assert_eq!(sparkline(&[], 3).chars().count(), 3); // empty → blanks, width kept
    }
}
