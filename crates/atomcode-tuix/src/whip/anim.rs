// crates/atomcode-tuix/src/whip/anim.rs
//
// Procedural frame generator for the whip sweep. Produces 15 frames
// (~500ms at 33ms/frame). Not a physics simulation — a sinusoidal curve
// whose amplitude follows a bell and whose horizontal reach grows
// linearly with frame index. Frame 11 is the "crack" (flash = true,
// 💥 at tip, phrase shown); frames 12..=14 decay.

use std::f32::consts::PI;

#[derive(Debug, Clone)]
pub struct FrameBuf {
    /// 5 rendered display strings, one per row (top to bottom).
    pub rows: [String; 5],
    /// The crack phrase to display under the whip. `None` until frame 11.
    pub phrase: Option<String>,
    /// When true the renderer should invert non-blank cells (crack flash).
    pub flash: bool,
}

/// Cap on the animation canvas width. Wider terminals draw the whip
/// inside `MAX_WIDTH` columns centred (centring is Task 10 polish).
pub const MAX_WIDTH: usize = 60;
/// Total frames before the overlay closes.
pub const TOTAL_FRAMES: u16 = 15;
/// Frame index of the crack: flash + 💥 + phrase first appears.
pub const CRACK_FRAME: u16 = 11;
/// Sweep is frames 0..=10; we divide by `SWEEP_END - 1` so progress∈[0,1].
const SWEEP_END: u16 = 11;

pub fn frame(idx: u16, terminal_width: u16, phrase: &str) -> FrameBuf {
    if idx >= TOTAL_FRAMES {
        return FrameBuf {
            rows: empty_rows(),
            phrase: None,
            flash: false,
        };
    }
    let width = (terminal_width as usize).min(MAX_WIDTH).max(10);
    let mut grid = empty_grid(width);

    if idx < CRACK_FRAME {
        draw_sweep(&mut grid, idx, width);
    } else if idx == CRACK_FRAME {
        draw_full_reach(&mut grid, width);
        place_tip(&mut grid, width.saturating_sub(1), '💥');
    } else {
        let decay_progress = (idx - CRACK_FRAME) as f32
            / (TOTAL_FRAMES - 1 - CRACK_FRAME).max(1) as f32;
        draw_decay(&mut grid, decay_progress, width);
    }

    let rows_strings = [
        grid[0].iter().collect::<String>(),
        grid[1].iter().collect::<String>(),
        grid[2].iter().collect::<String>(),
        grid[3].iter().collect::<String>(),
        grid[4].iter().collect::<String>(),
    ];

    let phrase_out = if idx >= CRACK_FRAME {
        Some(phrase.to_string())
    } else {
        None
    };
    let flash = idx == CRACK_FRAME;

    FrameBuf {
        rows: rows_strings,
        phrase: phrase_out,
        flash,
    }
}

fn empty_rows() -> [String; 5] {
    [
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ]
}

fn empty_grid(width: usize) -> Vec<Vec<char>> {
    vec![vec![' '; width]; 5]
}

/// Frames 0..=10: bell-shaped amplitude, linear reach.
fn draw_sweep(grid: &mut [Vec<char>], idx: u16, width: usize) {
    grid[2][0] = '╫';
    let progress = idx as f32 / SWEEP_END as f32;
    let amplitude = 1.5 * (PI * progress).sin();
    let reach = ((width as f32) * progress).round() as usize;
    for x in 1..reach.min(width) {
        let t = x as f32 / width as f32;
        let y_off = amplitude * (2.0 * PI * t - PI * progress).sin();
        let row = ((2.0 + y_off).round() as i32).clamp(0, 4) as usize;
        let ch = pick_body_char(y_off.abs(), idx);
        if grid[row][x] == ' ' {
            grid[row][x] = ch;
        }
    }
    if reach > 0 && reach <= width {
        let tip_col = (reach - 1).min(width - 1);
        grid[2][tip_col] = '»';
    }
}

/// Frame 11: whip fully extended at peak reach. Handle + straight body.
fn draw_full_reach(grid: &mut [Vec<char>], width: usize) {
    grid[2][0] = '╫';
    for x in 1..width.saturating_sub(1) {
        if grid[2][x] == ' ' {
            grid[2][x] = '─';
        }
    }
}

/// Frames 12..=14: amplitude collapses, characters thin out.
fn draw_decay(grid: &mut [Vec<char>], progress: f32, width: usize) {
    grid[2][0] = '╫';
    let dots_only = progress > 0.5;
    for x in 1..width.saturating_sub(1) {
        grid[2][x] = if dots_only { ' ' } else { '~' };
    }
}

fn place_tip(grid: &mut [Vec<char>], col: usize, ch: char) {
    if col < grid[2].len() {
        grid[2][col] = ch;
    }
}

fn pick_body_char(abs_y: f32, idx: u16) -> char {
    // Dense glyphs for slow early frames, thin/fast glyphs near the crack.
    if idx < 4 {
        if abs_y > 1.0 { '╱' } else { '─' }
    } else if idx < 8 {
        if abs_y > 1.0 { '╲' } else { '~' }
    } else {
        '≈'
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_0_has_coil_on_left() {
        let f = frame(0, 40, "FASTER");
        assert!(!f.flash);
        assert_eq!(f.phrase, None);
        // Handle on row 2 col 0.
        assert!(f.rows[2].starts_with('╫'));
    }

    #[test]
    fn frame_10_reaches_right_edge() {
        let f = frame(10, 40, "FASTER");
        let mid = &f.rows[2];
        let last_nonblank = mid
            .char_indices()
            .filter(|(_, c)| !c.is_whitespace())
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        assert!(
            last_nonblank >= 30,
            "tip should be past col 30 at frame 10, got {}",
            last_nonblank
        );
    }

    #[test]
    fn frame_11_is_the_crack() {
        let f = frame(11, 40, "FASTER");
        assert!(f.flash, "frame 11 must flash");
        assert_eq!(f.phrase.as_deref(), Some("FASTER"));
    }

    #[test]
    fn frame_15_is_empty() {
        let f = frame(15, 40, "FASTER");
        assert!(f.rows.iter().all(|r| r.trim().is_empty()));
        assert_eq!(f.phrase, None);
        assert!(!f.flash);
    }

    #[test]
    fn width_below_30_still_produces_5_rows() {
        let f = frame(5, 20, "快点");
        assert_eq!(f.rows.len(), 5);
    }

    #[test]
    fn narrow_terminal_is_floored_at_10() {
        // width 3 shouldn't panic and should still produce 5 rows.
        let f = frame(5, 3, "x");
        assert_eq!(f.rows.len(), 5);
    }
}
