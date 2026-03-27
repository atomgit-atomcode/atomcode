//! Global color palette — single source of truth for all UI colors.
//! Every UI file imports from here. No hardcoded Color::Rgb() elsewhere.

use ratatui::style::Color;

// ── Background ──
pub const BG_SURFACE: Color = Color::Rgb(18, 18, 24);       // status bar, code blocks
pub const BG_ELEVATED: Color = Color::Rgb(30, 30, 40);      // popups, selected state
pub const BG_CODE: Color = Color::Rgb(22, 22, 32);          // code block interior
pub const BG_INLINE_CODE: Color = Color::Rgb(40, 38, 50);   // inline `code`

// ── Text ──
pub const TEXT_PRIMARY: Color = Color::Rgb(220, 222, 230);   // main content
pub const TEXT_SECONDARY: Color = Color::Rgb(150, 153, 168); // descriptions, labels
pub const TEXT_MUTED: Color = Color::Rgb(90, 93, 108);       // metadata, timestamps
pub const TEXT_ON_ACCENT: Color = Color::Rgb(15, 15, 15);    // text on colored bg

// ── Brand / Accent ──
pub const ACCENT: Color = Color::Rgb(170, 145, 255);         // purple — AI identity
pub const ACCENT_DIM: Color = Color::Rgb(60, 50, 110);       // accent bar, borders
pub const BRAND_FG: Color = Color::Rgb(195, 185, 250);       // brand pill text
pub const BRAND_BG: Color = Color::Rgb(55, 38, 120);         // brand pill bg

// ── Semantic ──
pub const SUCCESS: Color = Color::Rgb(75, 195, 115);         // ✓ ok, green
pub const ERROR: Color = Color::Rgb(235, 80, 80);            // ✗ fail, red
pub const WARNING: Color = Color::Rgb(215, 170, 45);         // ⚠ caution, yellow
pub const INFO: Color = Color::Rgb(80, 165, 230);            // ℹ tool icon, blue

// ── Diff ──
pub const DIFF_ADD: Color = Color::Rgb(95, 190, 115);        // + lines
pub const DIFF_REMOVE: Color = Color::Rgb(200, 95, 95);      // - lines

// ── Border / Separator ──
pub const BORDER: Color = Color::Rgb(42, 44, 55);            // table lines, rules
pub const SEPARATOR: Color = Color::Rgb(55, 58, 70);         // turn separator, dividers

// ── User Message ──
pub const USER_CHEVRON: Color = Color::Rgb(90, 170, 255);    // ❯ prefix

// ── Tool Call ──
pub const TOOL_EDIT: Color = Color::Rgb(100, 200, 150);      // edit/write icon
pub const TOOL_BASH: Color = Color::Rgb(200, 180, 100);      // bash icon
pub const TOOL_SEARCH: Color = Color::Rgb(170, 140, 230);    // grep/glob/search icon

// ── Markdown ──
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

// ── Status Bar ──
pub const STATUS_PATH: Color = Color::Rgb(110, 112, 128);
pub const STATUS_MODEL: Color = Color::Rgb(175, 178, 200);
pub const STATUS_SEP: Color = Color::Rgb(42, 44, 55);

// ── Mode Badges ──
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

// ── Speed Indicator ──
pub const SPEED_FAST: Color = Color::Rgb(75, 165, 95);
pub const SPEED_NORMAL: Color = Color::Rgb(215, 170, 45);
pub const SPEED_SLOW: Color = Color::Rgb(230, 140, 50);
pub const SPEED_VERY_SLOW: Color = Color::Rgb(235, 80, 80);

// ── Wait Color (spinner) ──
pub const WAIT_FAST: Color = Color::Rgb(75, 195, 115);
pub const WAIT_NORMAL: Color = Color::Rgb(215, 170, 45);
pub const WAIT_SLOW: Color = Color::Rgb(230, 140, 50);
pub const WAIT_VERY_SLOW: Color = Color::Rgb(235, 80, 80);
