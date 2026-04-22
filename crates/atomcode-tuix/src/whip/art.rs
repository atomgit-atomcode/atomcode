// crates/atomcode-tuix/src/whip/art.rs
//
// Static "crack moment" ASCII whip art pushed to scrollback each time a
// whip fires. This is the permanent visual record that survives scroll
// navigation — the footer animation is the live flourish, this is what
// stays in the history.
//
// Shape (wide-form, ~70 cols):
//
//      ╔═══╗
//      ║▓▓▓╠══━━━━━━━━━╮
//      ║▓▓▓║            ╲━━━━~~~~~∼∼∼∼╮
//      ╚═══╝                            ╲⋯⋯⋯⋯·  ⚡💥  {PHRASE}
//                                         ∴∵∴
//
// Narrow-form single-line fallback for terminals < 50 cols.

/// Minimum terminal width (in columns) required for the 5-row wide art.
/// Below this we emit a single-line fallback. 50 is chosen so the base
/// art minus the phrase suffix fits without wrap.
pub const WIDE_FORM_MIN_COLS: u16 = 50;

/// Produce the scrollback crack art for this fire.
///
/// `phrase` is the selected encouragement; `suffix` is optional metadata
/// appended after the phrase (e.g. " (after 8.2s)" or " (no turn running)").
/// `terminal_cols` selects between the wide and narrow forms.
pub fn crack_art(phrase: &str, suffix: &str, terminal_cols: u16) -> Vec<String> {
    if terminal_cols < WIDE_FORM_MIN_COLS {
        return vec![format!("▓▓═━━━━~~~∼∼⋯·  💥  {}{}", phrase, suffix)];
    }
    vec![
        "   ╔═══╗".to_string(),
        "   ║▓▓▓╠══━━━━━━━━━╮".to_string(),
        "   ║▓▓▓║            ╲━━━━~~~~~∼∼∼∼╮".to_string(),
        format!(
            "   ╚═══╝                            ╲⋯⋯⋯⋯·  ⚡💥  {}{}",
            phrase, suffix
        ),
        "                                      ∴∵∴".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_form_has_five_rows() {
        let art = crack_art("FASTER", "", 80);
        assert_eq!(art.len(), 5);
    }

    #[test]
    fn wide_form_embeds_phrase_and_suffix() {
        let art = crack_art("FASTER", " (after 3.2s)", 80);
        assert!(art[3].contains("FASTER"));
        assert!(art[3].contains("(after 3.2s)"));
    }

    #[test]
    fn narrow_form_is_single_line() {
        let art = crack_art("快点", "", 30);
        assert_eq!(art.len(), 1);
        assert!(art[0].contains("快点"));
    }

    #[test]
    fn narrow_form_kicks_in_below_threshold() {
        assert_eq!(crack_art("x", "", 49).len(), 1);
        assert_eq!(crack_art("x", "", 50).len(), 5);
    }

    #[test]
    fn handles_cjk_phrase_in_wide_form() {
        let art = crack_art("赶紧的", " (after 1.5s)", 80);
        assert_eq!(art.len(), 5);
        assert!(art[3].contains("赶紧的"));
    }
}
