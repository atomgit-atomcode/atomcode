//! Global color palette — single source of truth for all UI colors.
//! Every UI file imports from here. No hardcoded Color::Rgb() elsewhere.
//!
//! Supports both dark and light terminal backgrounds via Theme enum.

use ratatui::style::Color;
use std::sync::OnceLock;

/// Terminal background theme (detected at startup).
static CURRENT_THEME: OnceLock<Theme> = OnceLock::new();

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
    }
    
    /// Get the current theme.
    pub fn current() -> Self {
        *CURRENT_THEME.get().unwrap_or(&Theme::Dark)
    }
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
                            if buf[..read].windows(2).any(|w| w == b"\x1b\\" || w == b"\x07") {
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
            let color_part = color_part.trim_end_matches(|c| c == '\x07' || c == '\x1b' || c == '\\');
            
            if color_part.starts_with("rgb:") {
                let parts: Vec<&str> = color_part[4..].split('/').collect();
                if parts.len() == 3 {
                    let parse_hex = |s: &str| -> Option<u8> {
                        let s = if s.len() >= 4 { &s[..2] } else { s };
                        u8::from_str_radix(s, 16).ok()
                    };
                    
                    if let (Some(r), Some(g), Some(b)) = (parse_hex(parts[0]), parse_hex(parts[1]), parse_hex(parts[2])) {
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
        Theme::Dark => Color::Rgb(18, 18, 24),
        Theme::Light => Color::Rgb(240, 240, 245),
    }
}

pub fn bg_elevated() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(30, 30, 40),
        Theme::Light => Color::Rgb(220, 220, 230),
    }
}

pub fn bg_code() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(22, 22, 32),
        Theme::Light => Color::Rgb(248, 248, 252),
    }
}

pub fn bg_inline_code() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(40, 38, 50),
        Theme::Light => Color::Rgb(230, 230, 240),
    }
}

// ── Text ──
pub fn text_primary() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(220, 222, 230),
        Theme::Light => Color::Rgb(25, 25, 35),
    }
}

pub fn text_secondary() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(150, 153, 168),
        Theme::Light => Color::Rgb(80, 83, 98),
    }
}

pub fn text_muted() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(90, 93, 108),
        Theme::Light => Color::Rgb(130, 133, 148),
    }
}

pub fn text_on_accent() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(240, 235, 255),  // Light lavender text on dark purple
        Theme::Light => Color::Rgb(255, 255, 255),
    }
}

// ── Brand / Accent ──
pub fn accent() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(170, 145, 255),
        Theme::Light => Color::Rgb(120, 95, 205),
    }
}

pub fn accent_dim() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(60, 50, 110),
        Theme::Light => Color::Rgb(180, 170, 220),
    }
}

pub fn brand_fg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(195, 185, 250),
        Theme::Light => Color::Rgb(255, 255, 255),
    }
}

pub fn brand_bg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(55, 38, 120),
        Theme::Light => Color::Rgb(70, 50, 150),  // 更深的紫色，确保白色文字可见
    }
}

// ── Semantic ──
pub fn success() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(75, 195, 115),
        Theme::Light => Color::Rgb(45, 155, 85),
    }
}

pub fn error() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(235, 80, 80),
        Theme::Light => Color::Rgb(195, 50, 50),
    }
}

pub fn warning() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(215, 170, 45),
        Theme::Light => Color::Rgb(175, 130, 25),
    }
}

pub fn info() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(80, 165, 230),
        Theme::Light => Color::Rgb(50, 125, 190),
    }
}

// ── Diff ──
pub fn diff_add() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(95, 190, 115),
        Theme::Light => Color::Rgb(45, 150, 65),
    }
}

pub fn diff_remove() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(200, 95, 95),
        Theme::Light => Color::Rgb(170, 55, 55),
    }
}

// ── Selection ──
pub fn selection_bg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(45, 40, 80),
        Theme::Light => Color::Rgb(200, 195, 230),
    }
}

// ── Border / Separator ──
pub fn border() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(42, 44, 55),
        Theme::Light => Color::Rgb(180, 182, 195),
    }
}

pub fn separator() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(55, 58, 70),
        Theme::Light => Color::Rgb(200, 203, 215),
    }
}

// ── User Message ──
pub fn user_chevron() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(90, 170, 255),
        Theme::Light => Color::Rgb(50, 120, 200),
    }
}

// ── Tool Call ──
pub fn tool_edit() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(100, 200, 150),
        Theme::Light => Color::Rgb(50, 150, 100),
    }
}

pub fn tool_bash() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(200, 180, 100),
        Theme::Light => Color::Rgb(160, 130, 50),
    }
}

pub fn tool_search() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(170, 140, 230),
        Theme::Light => Color::Rgb(120, 90, 190),
    }
}

// ── Markdown ──
pub fn md_h1() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(115, 170, 240),
        Theme::Light => Color::Rgb(65, 120, 190),
    }
}

pub fn md_h2() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(160, 140, 225),
        Theme::Light => Color::Rgb(110, 90, 175),
    }
}

pub fn md_h3() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(130, 190, 160),
        Theme::Light => Color::Rgb(80, 150, 120),
    }
}

pub fn md_link() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(100, 150, 230),
        Theme::Light => Color::Rgb(50, 100, 180),
    }
}

pub fn md_inline_code() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(220, 180, 120),
        Theme::Light => Color::Rgb(180, 120, 60),
    }
}

pub fn md_code_border() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(50, 54, 68),
        Theme::Light => Color::Rgb(200, 204, 218),
    }
}

pub fn md_code_lang() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(110, 120, 150),
        Theme::Light => Color::Rgb(100, 110, 140),
    }
}

pub fn md_code_linenum() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(70, 75, 95),
        Theme::Light => Color::Rgb(150, 155, 175),
    }
}

pub fn md_code_gutter() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(18, 18, 28),
        Theme::Light => Color::Rgb(245, 245, 255),
    }
}

pub fn md_bullet() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(85, 105, 140),
        Theme::Light => Color::Rgb(120, 140, 180),
    }
}

pub fn md_quote_bar() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(55, 55, 70),
        Theme::Light => Color::Rgb(180, 180, 195),
    }
}

pub fn md_quote_text() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(155, 157, 170),
        Theme::Light => Color::Rgb(90, 92, 105),
    }
}

// ── Status Bar ──
pub fn status_path() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(110, 112, 128),
        Theme::Light => Color::Rgb(100, 102, 118),
    }
}

pub fn status_model() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(175, 178, 200),
        Theme::Light => Color::Rgb(70, 73, 95),
    }
}

pub fn status_sep() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(42, 44, 55),
        Theme::Light => Color::Rgb(180, 182, 195),
    }
}

// ── Mode Badges ──
pub fn mode_streaming_fg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(28, 24, 10),
        Theme::Light => Color::Rgb(255, 255, 255),
    }
}

pub fn mode_streaming_bg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(215, 170, 40),
        Theme::Light => Color::Rgb(185, 140, 20),
    }
}

pub fn mode_running_fg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(12, 22, 38),
        Theme::Light => Color::Rgb(255, 255, 255),
    }
}

pub fn mode_running_bg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(65, 160, 225),
        Theme::Light => Color::Rgb(45, 130, 195),
    }
}

pub fn mode_approval_fg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(32, 18, 8),
        Theme::Light => Color::Rgb(255, 255, 255),
    }
}

pub fn mode_approval_bg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(230, 155, 45),
        Theme::Light => Color::Rgb(200, 125, 25),
    }
}

pub fn mode_overlay_fg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(225, 215, 255),
        Theme::Light => Color::Rgb(255, 255, 255),
    }
}

pub fn mode_overlay_bg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(75, 55, 150),
        Theme::Light => Color::Rgb(95, 75, 170),
    }
}

pub fn mode_exit_fg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(255, 225, 225),
        Theme::Light => Color::Rgb(255, 255, 255),
    }
}

pub fn mode_exit_bg() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(175, 45, 45),
        Theme::Light => Color::Rgb(165, 35, 35),
    }
}

// ── Wait Color (spinner) ──
pub fn wait_fast() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(75, 195, 115),
        Theme::Light => Color::Rgb(55, 175, 95),
    }
}

pub fn wait_normal() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(215, 170, 45),
        Theme::Light => Color::Rgb(175, 130, 25),
    }
}

pub fn wait_slow() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(230, 140, 50),
        Theme::Light => Color::Rgb(190, 100, 30),
    }
}

pub fn wait_very_slow() -> Color {
    match Theme::current() {
        Theme::Dark => Color::Rgb(235, 80, 80),
        Theme::Light => Color::Rgb(195, 50, 50),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// LEGACY CONSTANTS (deprecated - use functions above for theme support)
// ═══════════════════════════════════════════════════════════════════════════

pub const BG_SURFACE: Color = Color::Rgb(18, 18, 24);
pub const BG_ELEVATED: Color = Color::Rgb(30, 30, 40);
pub const BG_CODE: Color = Color::Rgb(22, 22, 32);
pub const BG_INLINE_CODE: Color = Color::Rgb(40, 38, 50);

pub const TEXT_PRIMARY: Color = Color::Rgb(220, 222, 230);
pub const TEXT_SECONDARY: Color = Color::Rgb(150, 153, 168);
pub const TEXT_MUTED: Color = Color::Rgb(90, 93, 108);
pub const TEXT_ON_ACCENT: Color = Color::Rgb(15, 15, 15);

pub const ACCENT: Color = Color::Rgb(170, 145, 255);
pub const ACCENT_DIM: Color = Color::Rgb(60, 50, 110);
pub const BRAND_FG: Color = Color::Rgb(195, 185, 250);
pub const BRAND_BG: Color = Color::Rgb(55, 38, 120);

pub const SUCCESS: Color = Color::Rgb(75, 195, 115);
pub const ERROR: Color = Color::Rgb(235, 80, 80);
pub const WARNING: Color = Color::Rgb(215, 170, 45);
pub const INFO: Color = Color::Rgb(80, 165, 230);

pub const DIFF_ADD: Color = Color::Rgb(95, 190, 115);
pub const DIFF_REMOVE: Color = Color::Rgb(200, 95, 95);

pub const BORDER: Color = Color::Rgb(42, 44, 55);
pub const SEPARATOR: Color = Color::Rgb(55, 58, 70);

pub const USER_CHEVRON: Color = Color::Rgb(90, 170, 255);

pub const TOOL_EDIT: Color = Color::Rgb(100, 200, 150);
pub const TOOL_BASH: Color = Color::Rgb(200, 180, 100);
pub const TOOL_SEARCH: Color = Color::Rgb(170, 140, 230);

pub const MD_H1: Color = Color::Rgb(115, 170, 240);
pub const MD_H2: Color = Color::Rgb(160, 140, 225);
pub const MD_H3: Color = Color::Rgb(130, 190, 160);
pub const MD_LINK: Color = Color::Rgb(100, 150, 230);
pub const MD_INLINE_CODE: Color = Color::Rgb(220, 180, 120);
pub const MD_CODE_BORDER: Color = Color::Rgb(50, 54, 68);
pub const MD_CODE_LANG: Color = Color::Rgb(110, 120, 150);
pub const MD_CODE_LINENUM: Color = Color::Rgb(70, 75, 95);
pub const MD_CODE_GUTTER: Color = Color::Rgb(18, 18, 28);
pub const MD_BULLET: Color = Color::Rgb(85, 105, 140);
pub const MD_QUOTE_BAR: Color = Color::Rgb(55, 55, 70);
pub const MD_QUOTE_TEXT: Color = Color::Rgb(155, 157, 170);

pub const STATUS_PATH: Color = Color::Rgb(110, 112, 128);
pub const STATUS_MODEL: Color = Color::Rgb(175, 178, 200);
pub const STATUS_SEP: Color = Color::Rgb(42, 44, 55);

pub const MODE_STREAMING_FG: Color = Color::Rgb(28, 24, 10);
pub const MODE_STREAMING_BG: Color = Color::Rgb(215, 170, 40);
pub const MODE_RUNNING_FG: Color = Color::Rgb(12, 22, 38);
pub const MODE_RUNNING_BG: Color = Color::Rgb(65, 160, 225);
pub const MODE_APPROVAL_FG: Color = Color::Rgb(32, 18, 8);
pub const MODE_APPROVAL_BG: Color = Color::Rgb(230, 155, 45);
pub const MODE_OVERLAY_FG: Color = Color::Rgb(225, 215, 255);
pub const MODE_OVERLAY_BG: Color = Color::Rgb(75, 55, 150);
pub const MODE_EXIT_FG: Color = Color::Rgb(255, 225, 225);
pub const MODE_EXIT_BG: Color = Color::Rgb(175, 45, 45);

pub const WAIT_FAST: Color = Color::Rgb(75, 195, 115);
pub const WAIT_NORMAL: Color = Color::Rgb(215, 170, 45);
pub const WAIT_SLOW: Color = Color::Rgb(230, 140, 50);
pub const WAIT_VERY_SLOW: Color = Color::Rgb(235, 80, 80);
