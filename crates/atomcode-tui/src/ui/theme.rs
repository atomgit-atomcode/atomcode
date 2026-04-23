//! Global color palette — single source of truth for all UI colors.
//! Every UI file imports from here. No hardcoded Color::Rgb() elsewhere.
//!
//! Supports both dark and light terminal backgrounds via Theme enum.
//! Automatically falls back to 256-color palette when true color is unavailable
//! (e.g. macOS Terminal.app).

use ratatui::style::Color;
use std::sync::OnceLock;

/// Terminal background theme (detected at startup).
static CURRENT_THEME: OnceLock<Theme> = OnceLock::new();

/// Whether the terminal supports 24-bit true color.
static TRUECOLOR: OnceLock<bool> = OnceLock::new();

/// Whether we're running inside macOS Terminal.app (needs special handling).
static APPLE_TERMINAL: OnceLock<bool> = OnceLock::new();

/// Theme type - dark or light background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    /// Detect terminal background color via OSC 11 query.
    /// Falls back to dark theme if detection fails.
    pub fn detect() -> Self {
        if let Some(bg) = query_terminal_background() {
            let luminance = 0.299 * bg.0 as f32 + 0.587 * bg.1 as f32 + 0.114 * bg.2 as f32;
            if luminance > 128.0 {
                return Theme::Light;
            }
        }
        Theme::Dark
    }

    /// Initialize the global theme (call once at startup).
    pub fn init() {
        let theme = Self::detect();
        let _ = CURRENT_THEME.set(theme);
        let _ = TRUECOLOR.set(detect_truecolor());
        let is_apple = std::env::var("TERM_PROGRAM")
            .map(|tp| tp.to_lowercase() == "apple_terminal")
            .unwrap_or(false);
        let _ = APPLE_TERMINAL.set(is_apple);
    }

    /// Get the current theme.
    pub fn current() -> Self {
        *CURRENT_THEME.get().unwrap_or(&Theme::Dark)
    }

    /// Whether we're running inside macOS Terminal.app.
    /// Terminal.app doesn't support true color and has issues with external
    /// processes writing to the terminal via SSH master connections, requiring
    /// full repaint on every frame.
    pub fn is_apple_terminal() -> bool {
        *APPLE_TERMINAL.get().unwrap_or(&false)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// TRUE COLOR DETECTION & FALLBACK
// ═══════════════════════════════════════════════════════════════════════════

/// Detect whether the terminal supports 24-bit true color.
/// Checks COLORTERM env var and known terminal identifiers.
fn detect_truecolor() -> bool {
    // COLORTERM=truecolor or 24bit is the standard signal
    if let Ok(ct) = std::env::var("COLORTERM") {
        let ct = ct.to_lowercase();
        if ct == "truecolor" || ct == "24bit" {
            return true;
        }
    }

    // Known true-color terminals by TERM_PROGRAM
    if let Ok(tp) = std::env::var("TERM_PROGRAM") {
        let tp = tp.to_lowercase();
        if tp.contains("iterm")
            || tp.contains("alacritty")
            || tp.contains("wezterm")
            || tp.contains("kitty")
            || tp.contains("hyper")
            || tp.contains("ghostty")
            || tp.contains("vscode")
            || tp.contains("warp")
        {
            return true;
        }
        // macOS Terminal.app explicitly does NOT support true color
        if tp == "apple_terminal" {
            return false;
        }
    }

    // TERM containing 24bit or direct also signals support
    if let Ok(term) = std::env::var("TERM") {
        if term.contains("24bit") || term.contains("direct") {
            return true;
        }
    }

    // SSH sessions: COLORTERM and TERM_PROGRAM are typically not forwarded,
    // so we land here. Default to false (256-color) — safe on all terminals.
    // Users on true-color-capable SSH clients can set COLORTERM=truecolor
    // in their remote shell profile to opt in.
    false
}

/// Check if terminal supports true color.
pub fn is_truecolor() -> bool {
    *TRUECOLOR.get().unwrap_or(&true)
}

/// Create a color, falling back to the nearest 256-color index when true color
/// is not supported by the terminal.
pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    if *TRUECOLOR.get().unwrap_or(&true) {
        Color::Rgb(r, g, b)
    } else {
        Color::Indexed(rgb_to_256(r, g, b))
    }
}

/// Convert an RGB color to the nearest xterm-256 color index.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    // The 6x6x6 color cube uses these channel values
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

    fn nearest_cube(v: u8) -> usize {
        let mut best = 0;
        let mut best_d = i16::MAX;
        for (i, &c) in CUBE.iter().enumerate() {
            let d = (v as i16 - c as i16).abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    }

    let ri = nearest_cube(r);
    let gi = nearest_cube(g);
    let bi = nearest_cube(b);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;
    let cube_color = (CUBE[ri], CUBE[gi], CUBE[bi]);
    let cube_dist = color_dist(r, g, b, cube_color.0, cube_color.1, cube_color.2);

    // Check grayscale ramp (indices 232-255): 24 shades from 8 to 238, step 10
    let gray_approx = if r == g && g == b {
        r
    } else {
        // Use u32 to avoid overflow: max value is 255*299 + 255*587 + 255*114 = 255,000
        ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8
    };
    let gray_idx = if gray_approx < 4 {
        0usize
    } else if gray_approx > 243 {
        23
    } else {
        ((gray_approx as usize - 8 + 5) / 10).min(23)
    };
    let gray_value = (8 + 10 * gray_idx) as u8;
    let gray_dist = color_dist(r, g, b, gray_value, gray_value, gray_value);

    if gray_dist < cube_dist {
        (232 + gray_idx) as u8
    } else {
        cube_idx as u8
    }
}

/// Squared Euclidean distance in RGB space.
fn color_dist(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let dr = r1 as i32 - r2 as i32;
    let dg = g1 as i32 - g2 as i32;
    let db = b1 as i32 - b2 as i32;
    (dr * dr + dg * dg + db * db) as u32
}

/// Query terminal background color using OSC 11.
fn query_terminal_background() -> Option<(u8, u8, u8)> {
    #[cfg(unix)]
    {
        use std::io::{self, Read, Write};
        use std::time::Duration;

        let original_termios = unsafe {
            let mut termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) != 0 {
                return None;
            }
            termios
        };

        unsafe {
            let mut raw = original_termios;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
        }

        let query = "\x1b]11;?\x07";
        let _ = io::stdout().write_all(query.as_bytes());
        let _ = io::stdout().flush();

        let response = {
            let mut buf = [0u8; 64];
            let mut read = 0;
            let start = std::time::Instant::now();

            while read < buf.len() && start.elapsed() < Duration::from_millis(50) {
                let mut fds = [libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                }];

                let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 10) };

                if ready > 0 && (fds[0].revents & libc::POLLIN) != 0 {
                    match io::stdin().read(&mut buf[read..]) {
                        Ok(n) if n > 0 => {
                            read += n;
                            if buf[..read]
                                .windows(2)
                                .any(|w| w == b"\x1b\\" || w == b"\x07")
                            {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
            String::from_utf8_lossy(&buf[..read]).to_string()
        };

        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original_termios);
        }

        if let Some(color_part) = response.split("11;").nth(1) {
            let color_part =
                color_part.trim_end_matches(|c| c == '\x07' || c == '\x1b' || c == '\\');

            if color_part.starts_with("rgb:") {
                let parts: Vec<&str> = color_part[4..].split('/').collect();
                if parts.len() == 3 {
                    let parse_hex = |s: &str| -> Option<u8> {
                        let s = if s.len() >= 4 { &s[..2] } else { s };
                        u8::from_str_radix(s, 16).ok()
                    };

                    if let (Some(r), Some(g), Some(b)) = (
                        parse_hex(parts[0]),
                        parse_hex(parts[1]),
                        parse_hex(parts[2]),
                    ) {
                        return Some((r, g, b));
                    }
                }
            }
        }
    }

    None
}

// ═══════════════════════════════════════════════════════════════════════════
// THEME-AWARE COLOR ACCESSORS
// ═══════════════════════════════════════════════════════════════════════════

// ── Background ──
pub fn bg_surface() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(18, 18, 24),
        Theme::Light => rgb(240, 240, 245),
    }
}

pub fn bg_elevated() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(30, 30, 40),
        Theme::Light => rgb(220, 220, 230),
    }
}

pub fn bg_code() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(22, 22, 32),
        Theme::Light => rgb(248, 248, 252),
    }
}

pub fn bg_inline_code() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(40, 38, 50),
        Theme::Light => rgb(230, 230, 240),
    }
}

// ── Text ──
// All three tiers bumped one notch brighter (more contrast vs background) per
// 2026-04-12 feedback that overall text felt dim.
pub fn text_primary() -> Color {
    if !is_truecolor() {
        return match Theme::current() {
            Theme::Dark => Color::White,
            Theme::Light => Color::Black,
        };
    }
    match Theme::current() {
        Theme::Dark => rgb(238, 240, 248),
        Theme::Light => rgb(10, 10, 20),
    }
}

pub fn text_secondary() -> Color {
    if !is_truecolor() {
        return match Theme::current() {
            Theme::Dark => Color::Gray,
            Theme::Light => Color::DarkGray,
        };
    }
    match Theme::current() {
        Theme::Dark => rgb(185, 188, 202),
        Theme::Light => rgb(55, 58, 73),
    }
}

pub fn text_muted() -> Color {
    if !is_truecolor() {
        return Color::DarkGray;
    }
    match Theme::current() {
        Theme::Dark => rgb(125, 128, 142),
        Theme::Light => rgb(100, 103, 118),
    }
}

pub fn text_on_accent() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(240, 235, 255), // Light lavender text on dark purple
        Theme::Light => rgb(255, 255, 255),
    }
}

// ── Brand / Accent ──
pub fn accent() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(170, 145, 255),
        Theme::Light => rgb(120, 95, 205),
    }
}

pub fn accent_dim() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(60, 50, 110),
        Theme::Light => rgb(180, 170, 220),
    }
}

pub fn brand_fg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(195, 185, 250),
        Theme::Light => rgb(255, 255, 255),
    }
}

pub fn brand_bg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(55, 38, 120),
        Theme::Light => rgb(70, 50, 150), // 更深的紫色，确保白色文字可见
    }
}

// ── Semantic ──
pub fn success() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(75, 195, 115),
        Theme::Light => rgb(45, 155, 85),
    }
}

pub fn error() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(235, 80, 80),
        Theme::Light => rgb(195, 50, 50),
    }
}

pub fn warning() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(215, 170, 45),
        Theme::Light => rgb(175, 130, 25),
    }
}

pub fn info() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(80, 165, 230),
        Theme::Light => rgb(50, 125, 190),
    }
}

// ── Diff ──
pub fn diff_add() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(95, 190, 115),
        Theme::Light => rgb(45, 150, 65),
    }
}

pub fn diff_remove() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(200, 95, 95),
        Theme::Light => rgb(170, 55, 55),
    }
}

// ── Selection ──
pub fn selection_bg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(45, 40, 80),
        Theme::Light => rgb(200, 195, 230),
    }
}

// ── Border / Separator ──
pub fn border() -> Color {
    if !is_truecolor() {
        return Color::DarkGray;
    }
    match Theme::current() {
        Theme::Dark => rgb(75, 78, 95),
        Theme::Light => rgb(160, 163, 180),
    }
}

pub fn separator() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(55, 58, 70),
        Theme::Light => rgb(200, 203, 215),
    }
}

// ── User Message ──
pub fn user_chevron() -> Color {
    if !is_truecolor() {
        return Color::Cyan;
    }
    match Theme::current() {
        Theme::Dark => rgb(90, 170, 255),
        Theme::Light => rgb(50, 120, 200),
    }
}

// ── Tool Call ──
// All tools share the same muted gray — distinction is in the tool name, not color.
// CC-aligned: information through layout, not hue.
pub fn tool_edit() -> Color {
    text_secondary()
}

pub fn tool_bash() -> Color {
    text_secondary()
}

pub fn tool_search() -> Color {
    text_secondary()
}

// ── Markdown ──
// Headings use white/gray gradient (no hue). Hierarchy via brightness + weight only.
// CC-aligned: h1 brightest primary text, h2 standard primary, h3 secondary.
pub fn md_h1() -> Color {
    text_primary()
}

pub fn md_h2() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(200, 202, 215),
        Theme::Light => rgb(45, 48, 65),
    }
}

pub fn md_h3() -> Color {
    text_secondary()
}

pub fn md_link() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(100, 150, 230),
        Theme::Light => rgb(50, 100, 180),
    }
}

pub fn md_inline_code() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(220, 180, 120),
        Theme::Light => rgb(180, 120, 60),
    }
}

pub fn md_code_border() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(50, 54, 68),
        Theme::Light => rgb(200, 204, 218),
    }
}

pub fn md_code_lang() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(110, 120, 150),
        Theme::Light => rgb(100, 110, 140),
    }
}

pub fn md_code_linenum() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(70, 75, 95),
        Theme::Light => rgb(150, 155, 175),
    }
}

pub fn md_code_gutter() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(18, 18, 28),
        Theme::Light => rgb(245, 245, 255),
    }
}

pub fn md_bullet() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(85, 105, 140),
        Theme::Light => rgb(120, 140, 180),
    }
}

pub fn md_quote_bar() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(55, 55, 70),
        Theme::Light => rgb(180, 180, 195),
    }
}

pub fn md_quote_text() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(155, 157, 170),
        Theme::Light => rgb(90, 92, 105),
    }
}

// ── Status Bar ──
pub fn status_path() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(110, 112, 128),
        Theme::Light => rgb(100, 102, 118),
    }
}

pub fn status_model() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(175, 178, 200),
        Theme::Light => rgb(70, 73, 95),
    }
}

pub fn status_sep() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(42, 44, 55),
        Theme::Light => rgb(180, 182, 195),
    }
}

// ── Mode Badges ──
pub fn mode_streaming_fg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(28, 24, 10),
        Theme::Light => rgb(255, 255, 255),
    }
}

pub fn mode_streaming_bg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(215, 170, 40),
        Theme::Light => rgb(185, 140, 20),
    }
}

pub fn mode_running_fg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(12, 22, 38),
        Theme::Light => rgb(255, 255, 255),
    }
}

pub fn mode_running_bg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(65, 160, 225),
        Theme::Light => rgb(45, 130, 195),
    }
}

pub fn mode_approval_fg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(32, 18, 8),
        Theme::Light => rgb(255, 255, 255),
    }
}

pub fn mode_approval_bg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(230, 155, 45),
        Theme::Light => rgb(200, 125, 25),
    }
}

pub fn mode_overlay_fg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(225, 215, 255),
        Theme::Light => rgb(255, 255, 255),
    }
}

pub fn mode_overlay_bg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(75, 55, 150),
        Theme::Light => rgb(95, 75, 170),
    }
}

pub fn mode_exit_fg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(255, 225, 225),
        Theme::Light => rgb(255, 255, 255),
    }
}

pub fn mode_exit_bg() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(175, 45, 45),
        Theme::Light => rgb(165, 35, 35),
    }
}

// ── Wait Color (spinner) ──
pub fn wait_fast() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(75, 195, 115),
        Theme::Light => rgb(55, 175, 95),
    }
}

pub fn wait_normal() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(215, 170, 45),
        Theme::Light => rgb(175, 130, 25),
    }
}

pub fn wait_slow() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(230, 140, 50),
        Theme::Light => rgb(190, 100, 30),
    }
}

pub fn wait_very_slow() -> Color {
    match Theme::current() {
        Theme::Dark => rgb(235, 80, 80),
        Theme::Light => rgb(195, 50, 50),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// LEGACY CONSTANTS (deprecated - use functions above for theme support)
// ═══════════════════════════════════════════════════════════════════════════

// CC-aligned minimal palette: white/gray base, muted blue accent.
// Principle: information through layout, not color. Color only for status.
pub const BG_SURFACE: Color = Color::Rgb(18, 18, 22);
pub const BG_ELEVATED: Color = Color::Rgb(28, 28, 34);
pub const BG_CODE: Color = Color::Rgb(22, 22, 28);
pub const BG_INLINE_CODE: Color = Color::Rgb(38, 38, 46);

pub const TEXT_PRIMARY: Color = Color::Rgb(230, 230, 235);
pub const TEXT_SECONDARY: Color = Color::Rgb(140, 142, 155);
pub const TEXT_MUTED: Color = Color::Rgb(85, 88, 100);
pub const TEXT_ON_ACCENT: Color = Color::Rgb(15, 15, 15);

// Accent: muted blue (CC-style), not purple
pub const ACCENT: Color = Color::Rgb(100, 160, 240);
pub const ACCENT_DIM: Color = Color::Rgb(40, 55, 80);
pub const BRAND_FG: Color = Color::Rgb(140, 175, 230);
pub const BRAND_BG: Color = Color::Rgb(30, 42, 65);

// Status: muted versions — not screaming
pub const SUCCESS: Color = Color::Rgb(70, 170, 100);
pub const ERROR: Color = Color::Rgb(210, 75, 75);
pub const WARNING: Color = Color::Rgb(195, 155, 45);
pub const INFO: Color = Color::Rgb(90, 155, 210);

// Diff: subdued
pub const DIFF_ADD: Color = Color::Rgb(80, 165, 100);
pub const DIFF_REMOVE: Color = Color::Rgb(180, 85, 85);

pub const BORDER: Color = Color::Rgb(38, 40, 48);
pub const SEPARATOR: Color = Color::Rgb(48, 50, 58);

// User chevron: same muted blue as accent
pub const USER_CHEVRON: Color = Color::Rgb(100, 160, 240);

// Tool colors: all same muted gray — no per-tool color
pub const TOOL_EDIT: Color = Color::Rgb(140, 142, 155);
pub const TOOL_BASH: Color = Color::Rgb(140, 142, 155);
pub const TOOL_SEARCH: Color = Color::Rgb(140, 142, 155);

// Markdown: mostly white/gray, minimal color
pub const MD_H1: Color = Color::Rgb(230, 230, 235);
pub const MD_H2: Color = Color::Rgb(200, 200, 210);
pub const MD_H3: Color = Color::Rgb(180, 180, 190);
pub const MD_LINK: Color = Color::Rgb(100, 150, 210);
pub const MD_INLINE_CODE: Color = Color::Rgb(200, 175, 120);
pub const MD_CODE_BORDER: Color = Color::Rgb(45, 48, 58);
pub const MD_CODE_LANG: Color = Color::Rgb(100, 105, 120);
pub const MD_CODE_LINENUM: Color = Color::Rgb(60, 65, 80);
pub const MD_CODE_GUTTER: Color = Color::Rgb(18, 18, 24);
pub const MD_BULLET: Color = Color::Rgb(85, 88, 100);
pub const MD_QUOTE_BAR: Color = Color::Rgb(48, 50, 58);
pub const MD_QUOTE_TEXT: Color = Color::Rgb(155, 157, 165);

pub const STATUS_PATH: Color = Color::Rgb(100, 102, 118);
pub const STATUS_MODEL: Color = Color::Rgb(160, 162, 178);
pub const STATUS_SEP: Color = Color::Rgb(38, 40, 48);

// Mode badges: less saturated
pub const MODE_STREAMING_FG: Color = Color::Rgb(20, 20, 10);
pub const MODE_STREAMING_BG: Color = Color::Rgb(195, 155, 40);
pub const MODE_RUNNING_FG: Color = Color::Rgb(12, 18, 30);
pub const MODE_RUNNING_BG: Color = Color::Rgb(70, 145, 200);
pub const MODE_APPROVAL_FG: Color = Color::Rgb(28, 18, 8);
pub const MODE_APPROVAL_BG: Color = Color::Rgb(210, 145, 45);
pub const MODE_OVERLAY_FG: Color = Color::Rgb(210, 210, 230);
pub const MODE_OVERLAY_BG: Color = Color::Rgb(50, 55, 85);
pub const MODE_EXIT_FG: Color = Color::Rgb(240, 220, 220);
pub const MODE_EXIT_BG: Color = Color::Rgb(160, 50, 50);

// Wait colors: green → yellow → orange → red (keep functional)
pub const WAIT_FAST: Color = Color::Rgb(70, 170, 100);
pub const WAIT_NORMAL: Color = Color::Rgb(195, 155, 45);
pub const WAIT_SLOW: Color = Color::Rgb(210, 130, 50);
pub const WAIT_VERY_SLOW: Color = Color::Rgb(210, 75, 75);
