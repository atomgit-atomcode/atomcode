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
        s.push('░');
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
            let hi = ((i + 1) * values.len() / width)
                .max(lo + 1)
                .min(values.len());
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
                if max == 0 {
                    0 // all-zero → blank (no phantom activity)
                } else {
                    4 // flat non-zero → mid
                }
            } else {
                1 + ((v - min) as f64 / (max - min) as f64 * 7.0).round() as usize
            };
            BLOCKS_V[level.min(8)]
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatCell {
    pub weekday: u8,     // 0=Sun .. 6=Sat
    pub week_col: usize, // 0-based column (week index from the first date)
    pub level: u8,       // 0..=5 intensity
    pub month_start: bool,
    pub month: u8, // 1..=12
}

/// 0..=5 intensity per day. Level 0 = zero activity.
///
/// Level by fraction of the window MAX (not quantiles): quantiles degenerate
/// when there are only a few active days (a lone active day would land in the
/// lowest bucket instead of the highest). Ratio-to-max keeps the busiest day
/// darkest and low days light, monotonically.
///
/// Returns levels in 0..=5; 0 = no activity, 5 = max activity.
pub fn heatmap_buckets(daily: &[u64]) -> Vec<u8> {
    let max = daily.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return daily.iter().map(|_| 0u8).collect();
    }
    daily
        .iter()
        .map(|&v| {
            if v == 0 {
                0
            } else {
                (((v as f64 / max as f64) * 5.0).ceil() as u8).clamp(1, 5)
            }
        })
        .collect()
}

/// Days since 1970-01-01 for a proleptic-Gregorian (y, m, d). Howard Hinnant's
/// algorithm. Used to derive weekday + week index without a date crate.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let mut it = s.split('-');
    let y = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    Some((y, m, d))
}

pub fn calendar_layout(rows: &[(String, u64)]) -> Vec<HeatCell> {
    let daily: Vec<u64> = rows.iter().map(|(_, t)| *t).collect();
    let levels = heatmap_buckets(&daily);
    // Week column = (epoch_day - first_week_start_epoch_day) / 7, where the
    // first week starts on the Sunday on/before the EARLIEST date. Anchor on the
    // MIN epoch across all rows (not `rows.first()`): the API is not guaranteed
    // sorted, and a row older than the first would yield a negative delta whose
    // `as usize` cast wraps to a huge index → OOM when the caller allocates by
    // max week_col.
    // weekday of epoch day: 1970-01-01 was a Thursday → (epoch + 4) % 7 gives 0=Sun.
    let weekday_of = |epoch: i64| (((epoch % 7) + 4).rem_euclid(7)) as u8;
    let min_epoch = rows
        .iter()
        .filter_map(|(d, _)| parse_ymd(d))
        .map(|(y, m, d)| days_from_civil(y, m, d))
        .min();
    let Some(min_epoch) = min_epoch else {
        return Vec::new();
    };
    let first_week_start = min_epoch - weekday_of(min_epoch) as i64;
    let mut out = Vec::with_capacity(rows.len());
    let mut prev_month = 0u32;
    for (i, (date, _)) in rows.iter().enumerate() {
        let Some((y, m, d)) = parse_ymd(date) else {
            continue;
        };
        let epoch = days_from_civil(y, m, d);
        out.push(HeatCell {
            weekday: weekday_of(epoch),
            week_col: ((epoch - first_week_start) / 7) as usize,
            level: levels[i],
            month_start: m != prev_month,
            month: m as u8,
        });
        prev_month = m;
    }
    out
}

pub fn braille_series_char(dots: u8) -> char {
    char::from_u32(0x2800 + dots as u32).unwrap_or(' ')
}

// dot bit index for (sub-column 0/1, sub-row 0..3 top→bottom)
const BRAILLE_DOT: [[u8; 4]; 2] = [[0, 1, 2, 6], [3, 4, 5, 7]];

/// Plot each series over a `width_cells`×`height_cells` grid of braille cells
/// (each cell = 2×4 dots). Shared Y-scale = global max across all series.
/// Returns rows[y][x] = merged dot bitmask (all series OR'd) for a monochrome
/// plot. For multi-colour, call once per series (pass a single-series slice) and
/// overlay by cell in the modal.
pub fn braille_plot(series: &[Vec<u64>], width_cells: usize, height_cells: usize) -> Vec<Vec<u8>> {
    let mut grid = vec![vec![0u8; width_cells.max(1)]; height_cells.max(1)];
    if width_cells == 0 || height_cells == 0 {
        return grid;
    }
    let dot_w = width_cells * 2;
    let dot_h = height_cells * 4;
    let gmax = series
        .iter()
        .flat_map(|s| s.iter().copied())
        .max()
        .unwrap_or(0)
        .max(1);
    for s in series {
        if s.is_empty() {
            continue;
        }
        for dx in 0..dot_w {
            // sample the series at this x (nearest)
            let idx = if s.len() == 1 {
                0
            } else {
                dx * (s.len() - 1) / (dot_w - 1).max(1)
            };
            let v = s[idx];
            // y in dot space: 0 = top, dot_h-1 = bottom. value 0 → bottom.
            let filled = ((v as f64 / gmax as f64) * (dot_h - 1) as f64).round() as usize;
            let dy = (dot_h - 1).saturating_sub(filled); // higher value → smaller dy (up)
            let cell_x = dx / 2;
            let cell_y = dy / 4;
            let sub_col = dx % 2;
            let sub_row = dy % 4;
            grid[cell_y][cell_x] |= 1 << BRAILLE_DOT[sub_col][sub_row];
        }
    }
    grid
}

/// Like `braille_plot` but draws a CONNECTED line: for each dot-column, fill the
/// vertical span of dots between the previous column's y and this column's y, so
/// consecutive samples join into a continuous line (pixel-level line chart).
///
/// Each series is plotted independently (single-series call for per-model colouring)
/// or merged (multi-series call). Y-scale is shared across all series via gmax.
pub fn braille_line_plot(
    series: &[Vec<u64>],
    width_cells: usize,
    height_cells: usize,
) -> Vec<Vec<u8>> {
    let mut grid = vec![vec![0u8; width_cells.max(1)]; height_cells.max(1)];
    if width_cells == 0 || height_cells == 0 {
        return grid;
    }
    let dot_w = width_cells * 2;
    let dot_h = height_cells * 4;
    let gmax = series
        .iter()
        .flat_map(|s| s.iter().copied())
        .max()
        .unwrap_or(0)
        .max(1);

    for s in series {
        if s.is_empty() {
            continue;
        }
        let sample = |dx: usize| -> usize {
            let idx = if s.len() == 1 {
                0
            } else {
                dx * (s.len() - 1) / (dot_w - 1).max(1)
            };
            let v = s[idx];
            let filled = ((v as f64 / gmax as f64) * (dot_h - 1) as f64).round() as usize;
            (dot_h - 1).saturating_sub(filled)
        };

        let mut dy_prev = sample(0);
        // Plot the first dot
        {
            let cell_x = 0 / 2;
            let cell_y = dy_prev / 4;
            let sub_col = 0 % 2;
            let sub_row = dy_prev % 4;
            if cell_y < height_cells && cell_x < width_cells {
                grid[cell_y][cell_x] |= 1 << BRAILLE_DOT[sub_col][sub_row];
            }
        }

        for dx in 1..dot_w {
            let dy_cur = sample(dx);
            // Fill vertical span from min to max between prev and cur
            let dy_lo = dy_prev.min(dy_cur);
            let dy_hi = dy_prev.max(dy_cur);
            for dy in dy_lo..=dy_hi {
                let cell_x = dx / 2;
                let cell_y = dy / 4;
                let sub_col = dx % 2;
                let sub_row = dy % 4;
                if cell_y < height_cells && cell_x < width_cells {
                    grid[cell_y][cell_x] |= 1 << BRAILLE_DOT[sub_col][sub_row];
                }
            }
            dy_prev = dy_cur;
        }
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bar_fills_proportionally() {
        // 0% → no filled blocks, entire track is ░
        let zero = progress_bar(0.0, 10);
        assert_eq!(zero.chars().filter(|c| *c == '█').count(), 0);
        assert!(
            zero.chars().all(|c| c == '░'),
            "0% bar should be all ░, got: {zero}"
        );
        assert_eq!(progress_bar(100.0, 10), "██████████");
        // 9% of 10 cells ≈ 0.9 cell → at least one partial/edge cell, not full
        let b = progress_bar(9.0, 10);
        assert_eq!(b.chars().count(), 10);
        assert!(b.chars().next().unwrap() != '░' || b.chars().any(|c| c != '░'));
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

    #[test]
    fn heatmap_buckets_zero_and_levels() {
        assert_eq!(heatmap_buckets(&[0, 0, 0]), vec![0, 0, 0]);
        let b = heatmap_buckets(&[0, 1, 50, 100, 1000]);
        assert_eq!(b[0], 0); // zero → level 0
        assert!(b[4] >= b[1] && b[4] <= 5); // monotone, capped at 5
        assert!(b.iter().all(|&l| l <= 5));
    }

    #[test]
    fn heatmap_buckets_five_levels() {
        // All-zero → all 0
        assert_eq!(heatmap_buckets(&[0, 0, 0]), vec![0, 0, 0]);
        // Single active day → max level 5
        assert_eq!(heatmap_buckets(&[0, 0, 717016, 0]), vec![0, 0, 5, 0]);
        // Monotone: levels must be non-decreasing for non-decreasing values
        let vals = vec![0u64, 100, 300, 600, 1000];
        let levels = heatmap_buckets(&vals);
        assert_eq!(levels[0], 0, "zero must be level 0");
        assert_eq!(levels[4], 5, "max value must be level 5");
        for w in levels.windows(2) {
            assert!(
                w[0] <= w[1],
                "levels must be non-decreasing; got {:?}",
                levels
            );
        }
    }

    #[test]
    fn braille_line_plot_connected_diagonal() {
        // Strictly rising single series should produce a connected diagonal:
        // every dot-column must have ≥1 dot set.
        let series = vec![vec![0u64, 1, 2, 3, 4, 5, 6, 7]];
        let grid = braille_line_plot(&series, 4, 2);
        assert_eq!(grid.len(), 2);
        assert_eq!(grid[0].len(), 4);
        // Verify every dot-column (dx) has at least one bit set in the grid
        let dot_w = 4 * 2;
        for dx in 0..dot_w {
            let cell_x = dx / 2;
            let has_dot = grid.iter().any(|row| row[cell_x] != 0);
            assert!(has_dot, "dot column {dx} has no dots — line has a gap");
        }
        // Rising series: bottom-left cell has dots, top-right cell has dots
        assert!(
            grid[1][0] != 0,
            "bottom-left cell should have dots for rising series"
        );
        assert!(
            grid[0][3] != 0,
            "top-right cell should have dots for rising series"
        );
    }

    #[test]
    fn braille_line_plot_flat_zero_stays_bottom() {
        let flat = braille_line_plot(&[vec![0, 0, 0, 0]], 2, 2);
        assert!(
            flat[1].iter().any(|&c| c != 0),
            "flat-zero series should land in bottom row"
        );
        assert!(
            flat[0].iter().all(|&c| c == 0),
            "flat-zero series should not touch top row"
        );
    }

    #[test]
    fn calendar_layout_maps_weekday_and_week() {
        // 2026-07-13 is a Monday. Give 3 consecutive days.
        let rows = vec![
            ("2026-07-13".to_string(), 10u64), // Mon
            ("2026-07-14".to_string(), 0u64),  // Tue
            ("2026-07-15".to_string(), 99u64), // Wed
        ];
        let cells = calendar_layout(&rows);
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].weekday, 1); // Mon (0=Sun)
        assert_eq!(cells[2].weekday, 3); // Wed
        assert_eq!(cells[0].week_col, cells[2].week_col); // same week
        assert_eq!(cells[1].level, 0); // zero day
    }

    #[test]
    fn braille_plot_rising_series_lands_dots() {
        // one rising series over a 4-cell-wide, 2-cell-high plot
        let grid = braille_plot(&[vec![0, 1, 2, 3, 4, 5, 6, 7]], 4, 2);
        assert_eq!(grid.len(), 2); // height rows
        assert_eq!(grid[0].len(), 4); // width cells
                                      // rising series: bottom-left cell has dots, top-right cell has dots
        assert!(grid[1][0] != 0, "bottom-left should have dots");
        assert!(grid[0][3] != 0, "top-right should have dots");
        // flat-zero series → only the bottom row lights up
        let flat = braille_plot(&[vec![0, 0, 0, 0]], 2, 2);
        assert!(flat[1].iter().any(|&c| c != 0));
        assert!(flat[0].iter().all(|&c| c == 0));
    }

    #[test]
    fn braille_char_offsets_from_2800() {
        assert_eq!(braille_series_char(0), '\u{2800}');
        assert_eq!(braille_series_char(0xFF), '\u{28FF}');
    }

    #[test]
    fn calendar_layout_handles_unsorted_rows_without_overflow() {
        // Rows newest-first: the older date must not produce a wrapped (huge)
        // week_col from a negative epoch delta.
        let rows = vec![
            ("2026-07-16".to_string(), 5u64),
            ("2026-05-18".to_string(), 9u64),
        ];
        let cells = calendar_layout(&rows);
        assert_eq!(cells.len(), 2);
        assert!(
            cells.iter().all(|c| c.week_col < 60),
            "week_col wrapped: {:?}",
            cells.iter().map(|c| c.week_col).collect::<Vec<_>>()
        );
    }

    #[test]
    fn heatmap_buckets_single_active_day_is_max_level() {
        // The lone active day must render darkest (5), not palest (1).
        assert_eq!(heatmap_buckets(&[0, 0, 717016, 0]), vec![0, 0, 5, 0]);
    }

    #[test]
    fn sparkline_all_zero_is_blank() {
        assert_eq!(sparkline(&[0, 0, 0, 0], 4), "    ");
    }
}
