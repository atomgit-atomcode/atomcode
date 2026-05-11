// crates/atomcode-tuix/src/modals/onboarding_wizard.rs
//
// Multi-step first-run / `/welcome` onboarding wizard. Three real
// steps (Intro / Language / Setup) plus one synthetic Confirm step
// that fires only when `/welcome` runs mid-session and would clobber
// existing scrollback.
//
// Replaces `welcome_wizard.rs` (deleted in Task 9). Same `LoopCtx`
// post-close flag side-channel (`pending_run_codingplan`,
// `pending_open_provider_wizard`) as before — only the in-modal flow
// changes.
//
// This file lands in slices across the plan tasks:
//   * Task 2 (this slice): `draw_panel` box-drawing helper + tests.
//   * Task 3: `OnboardingWizard` struct + Step enum + transitions.
//   * Task 4-6: per-step `draw_*` + Modal trait impl.

use unicode_width::UnicodeWidthStr;

/// Build the lines of a Cyan-bordered panel.
///
/// Returns one string per terminal row: top border with title, content
/// lines with side borders + padding, bottom border with step indicator.
/// `width` is the total external width including both border columns;
/// inner content area is `width - 4` (2 padding cells on each side).
///
/// The returned strings include SGR colour codes so the renderer paints
/// the borders cyan and the title brand-magenta. Pass these strings to
/// `UiLine::CommandOutput`.
pub(super) fn draw_panel(
    title: &str,
    content: &[String],
    step_indicator: &str,
    width: usize,
) -> Vec<String> {
    use crossterm::style::{Color, ResetColor, SetForegroundColor};
    let border = Color::Cyan; // Palette::BORDER
    let brand = Color::Magenta; // Palette::BRAND

    let mut out = Vec::with_capacity(content.len() + 2);
    let inner_width = width.saturating_sub(4);

    // Top border: ┌─ <title> ─...─┐
    let title_seg = format!(" {title} ");
    let title_width = UnicodeWidthStr::width(title_seg.as_str());
    let dashes_after = inner_width.saturating_sub(title_width);
    let top = format!(
        "{b}┌─{r}{br}{tt}{r}{b}{dash}─┐{r}",
        b = SetForegroundColor(border),
        br = SetForegroundColor(brand),
        tt = title_seg,
        dash = "─".repeat(dashes_after),
        r = ResetColor,
    );
    out.push(top);

    // Content rows: │ <2 sp pad> <line padded to inner_width-2> <2 sp pad> │
    for line in content {
        let line_width = UnicodeWidthStr::width(line.as_str());
        let pad = (inner_width.saturating_sub(2)).saturating_sub(line_width);
        let row = format!(
            "{b}│{r}  {line}{pad}  {b}│{r}",
            b = SetForegroundColor(border),
            r = ResetColor,
            line = line,
            pad = " ".repeat(pad),
        );
        out.push(row);
    }

    // Bottom border: └─ <step_indicator> ─...─┘
    let step_seg = format!(" {step_indicator} ");
    let step_w = UnicodeWidthStr::width(step_seg.as_str());
    let dashes_after_step = inner_width.saturating_sub(step_w);
    let bot = format!(
        "{b}└─{step_seg}{dash}─┘{r}",
        b = SetForegroundColor(border),
        step_seg = step_seg,
        dash = "─".repeat(dashes_after_step),
        r = ResetColor,
    );
    out.push(bot);

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip every SGR escape so we can assert on the visible glyphs.
    fn strip_sgr(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == 'm' || n.is_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn top_border_has_title() {
        let lines = draw_panel("AtomCode", &[], "Step 1/3", 60);
        let plain = strip_sgr(&lines[0]);
        assert!(plain.starts_with("┌─ AtomCode "));
        assert!(plain.ends_with("─┐"));
    }

    #[test]
    fn bottom_border_has_step_indicator() {
        let lines = draw_panel("AtomCode", &[], "Step 1/3", 60);
        let plain = strip_sgr(lines.last().unwrap());
        assert!(plain.starts_with("└─ Step 1/3 "));
        assert!(plain.ends_with("─┘"));
    }

    #[test]
    fn content_lines_are_padded_to_width() {
        let content = vec!["hello".to_string(), "".to_string()];
        let lines = draw_panel("X", &content, "Y", 30);
        // Lines 1 & 2 are content. Each must be exactly `width` wide
        // when measured by visible-grid columns.
        for line in &lines[1..=2] {
            let plain = strip_sgr(line);
            assert_eq!(
                UnicodeWidthStr::width(plain.as_str()),
                30,
                "line not padded to 30: {plain:?}"
            );
        }
    }

    #[test]
    fn cjk_content_pads_correctly() {
        // Each CJK char is 2 cols. "中文" = 4 cols.
        let content = vec!["中文".to_string()];
        let lines = draw_panel("X", &content, "Y", 30);
        let plain = strip_sgr(&lines[1]);
        assert_eq!(UnicodeWidthStr::width(plain.as_str()), 30);
    }

    #[test]
    fn narrow_terminal_does_not_panic() {
        // width=10 has inner_width=6 which won't fit "AtomCode" title;
        // saturating_sub guards against underflow. We just assert it
        // doesn't panic and produces *some* output.
        let lines = draw_panel("AtomCode", &["x".into()], "S", 10);
        assert_eq!(lines.len(), 3); // top + 1 content + bottom
    }
}
