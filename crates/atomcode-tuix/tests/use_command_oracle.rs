//! What tuix reports as a command's name — written down before tuix is gone.
//!
//! tuix is the only front end still counting commands
//! (`src/event_loop/commands.rs:1615`), and the `type_` it reports is not the
//! text the person typed. It is lowercased, resolved through
//! `COMMAND_ALIASES`, and stripped of its leading `/`. Two spellings of the
//! same command therefore arrive as one row in the dashboard, and that grouping
//! is the contract — a replacement that reports the raw input would silently
//! split every aliased command into two series, with no error anywhere.
//!
//! The replacement now reports too — `atomcode-tui/src/command.rs` tells an
//! observer, `atomcode-cli/src/tui_command_meter.rs` counts it — and it was
//! built to match this. This file stays because the rule has to be written
//! down somewhere other than inside the crate being deleted: when tuix goes,
//! the pair of criteria on the new side is what is left, and they were written
//! against this one.

/// Exactly what `handle_command` does to a name before it reports it.
fn reported_type(typed: &str) -> String {
    let lowered = typed.trim_start_matches('/').to_ascii_lowercase();
    atomcode_tuix::commands::canonical_command_name(&lowered)
        .trim_start_matches('/')
        .to_string()
}

#[test]
fn the_reported_command_name_is_canonical_lowercase_and_unslashed() {
    let cases = [
        // Plain.
        ("/model", "model"),
        // Case is folded: `/SESSION` must not become its own series.
        ("/SESSION", "session"),
        ("/Compact", "compact"),
        // Aliases collapse onto the canonical name. These two pairs are the
        // whole of `COMMAND_ALIASES` as of this build.
        ("/new", "session"),
        ("/session", "session"),
        ("/exit", "quit"),
        ("/quit", "quit"),
        // An unknown command is reported under what was typed (folded), which
        // is what makes the `not_found` shape useful — it names the miss.
        ("/nope", "nope"),
    ];
    for (typed, expected) in cases {
        assert_eq!(
            reported_type(typed),
            expected,
            "`{typed}` must be counted as `{expected}`"
        );
    }
}

/// No reported name may keep a leading slash.
///
/// Its own check because the strip happens at the emission site rather than in
/// `canonical_command_name`, so an emitter written from the alias table alone
/// would get every row wrong by one character.
#[test]
fn no_reported_command_name_keeps_its_slash() {
    for typed in ["/model", "/new", "/exit", "/unknown-thing"] {
        let reported = reported_type(typed);
        assert!(
            !reported.starts_with('/'),
            "`{typed}` was reported as `{reported}`"
        );
    }
}
