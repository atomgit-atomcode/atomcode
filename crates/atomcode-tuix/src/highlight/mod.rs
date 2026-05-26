// crates/atomcode-tuix/src/highlight/mod.rs
//
// Public entry for fenced-code-block syntax highlight. Dispatch:
//
//   caps.colors == false             -> plain indented passthrough (no ANSI)
//   lang_hint == None                -> plain indented passthrough
//   syntect doesn't know the lang    -> plain indented passthrough
//   syntect highlights successfully  -> tinted ANSI with 2-space left indent
//
// Output is a multi-line string where every line is prefixed with 2 spaces
// (matches the pre-existing CC-style code-block indent). Caller (`markdown.rs`)
// inserts it verbatim into the body stream.

use std::str::FromStr;
use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::SyntaxSet;

use crate::terminal::TerminalCaps;

pub mod theme;

/// Lazily-built syntect syntax set (covers the ~120 default Sublime syntaxes
/// that ship with syntect). Loaded once on first use; cost is ~5-10ms and
/// happens before the first tinted code block.
static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();

/// Lazily-built syntect themes — one per palette. `theme.rs`'s runtime
/// `MODE` selects which is returned per highlight call. Both initialised
/// on first use; the unused one stays uncompiled until the user flips
/// to it (effectively never in single-session use).
static ATOMCODE_THEME_DARK: OnceLock<Theme> = OnceLock::new();
static ATOMCODE_THEME_LIGHT: OnceLock<Theme> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// RGB tuple for a single syntect scope. Kept in lockstep with the SGR
/// strings emitted by `theme.rs`'s accessor fns — the test
/// `palette_rgbs_match_theme_sgr_strings` pins this invariant so any
/// future drift breaks the build.
struct Rgb(u8, u8, u8);

struct CodePalette {
    keyword: Rgb,
    string: Rgb,
    number: Rgb,
    comment: Rgb,
    function: Rgb,
    type_: Rgb,
}

const DARK: CodePalette = CodePalette {
    keyword: Rgb(198, 120, 221),
    string: Rgb(152, 195, 121),
    number: Rgb(209, 154, 102),
    comment: Rgb(124, 132, 153),
    function: Rgb(97, 175, 239),
    type_: Rgb(229, 192, 123),
};

/// `Light` palette: dark, saturated variants hitting ≥ 11:1 contrast
/// on `#FFFFFF` (overshoots WCAG AA 4.5:1 by a wide margin — chosen
/// after a Mac Terminal user reported the earlier 8-9:1 values read
/// "soft" against the white background; the slightly-softer colours
/// stayed AA-compliant but didn't feel "saturated" the way users
/// expect light-theme code highlighting to look). Reproduces the same
/// scope→colour mapping as `DARK` so syntect's TextMate selectors
/// don't need re-tuning. Must stay in lockstep with `theme.rs`'s
/// per-token accessor SGR strings.
const LIGHT: CodePalette = CodePalette {
    keyword: Rgb(74, 0, 114),  // #4A0072
    string: Rgb(0, 100, 0),    // #006400
    number: Rgb(102, 51, 0),   // #663300
    comment: Rgb(74, 80, 96),  // #4A5060 (kept moderate — comments stay secondary)
    function: Rgb(0, 33, 113), // #002171
    type_: Rgb(91, 58, 0),     // #5B3A00
};

fn atomcode_theme() -> &'static Theme {
    // `theme::set_theme_mode(true)` flips `MODE`; we read it here to
    // pick the right OnceLock-cached Theme. Each variant is built at
    // most once per process.
    if theme::is_light_for_highlight() {
        ATOMCODE_THEME_LIGHT.get_or_init(|| build_atomcode_theme(&LIGHT))
    } else {
        ATOMCODE_THEME_DARK.get_or_init(|| build_atomcode_theme(&DARK))
    }
}

/// Build the syntect Theme from a palette. Uses TextMate scope
/// selectors that match across most syntect-bundled syntaxes:
///
///   keyword / storage              -> KEYWORD (purple)
///   string                         -> STRING (green)
///   constant.numeric / .language   -> NUMBER (amber)
///   comment                        -> COMMENT (italic slate gray)
///   entity.name.function / support.function -> FUNCTION (blue)
///   entity.name.type / support.type / support.class -> TYPE (sand)
///
/// Default foreground is set to the sentinel Color { a: 0 } so that chunks
/// not matching any scope above can be detected and emitted without ANSI.
fn build_atomcode_theme(p: &CodePalette) -> Theme {
    let item = |scope_str: &str, c: &Rgb, italic: bool| ThemeItem {
        scope: ScopeSelectors::from_str(scope_str).expect("valid scope selector"),
        style: StyleModifier {
            foreground: Some(Color {
                r: c.0,
                g: c.1,
                b: c.2,
                a: 0xFF,
            }),
            background: None,
            font_style: if italic {
                Some(FontStyle::ITALIC)
            } else {
                None
            },
        },
    };
    Theme {
        name: Some("atomcode-mid-lightness".into()),
        author: None,
        settings: ThemeSettings {
            // Sentinel default fg. alpha=0 means "we don't paint this chunk."
            // The highlight loop reads style.foreground.a to distinguish
            // matched-scope text from passthrough.
            foreground: Some(Color {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            }),
            ..ThemeSettings::default()
        },
        scopes: vec![
            item("keyword, storage", &p.keyword, false),
            item("string", &p.string, false),
            item("constant.numeric, constant.language", &p.number, false),
            item("comment", &p.comment, true),
            item("entity.name.function, support.function", &p.function, false),
            item(
                "entity.name.type, support.type, support.class",
                &p.type_,
                false,
            ),
        ],
    }
}

/// Highlight a complete fenced code block and return the indented, ANSI-tinted
/// multi-line string ready for `push_markdown_body`.
pub fn highlight_block(lang_hint: Option<&str>, source: &str, caps: TerminalCaps) -> String {
    if !caps.colors {
        return indent_plain(source);
    }
    if let Some(lang) = lang_hint {
        if let Some(tinted) = highlight_with_syntect(source, lang) {
            return indent_lines(&tinted);
        }
    }
    indent_plain(source)
}

/// Highlight `source` using syntect's regex-based highlighter. Returns
/// `Some(tinted)` on success or `None` if the language isn't recognized.
/// Panics inside syntect are caught and converted to `None` so the renderer
/// can't crash a streaming reply.
fn highlight_with_syntect(source: &str, lang: &str) -> Option<String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let source_owned = source.to_string();
    let lang_owned = lang.to_string();
    let result = catch_unwind(AssertUnwindSafe(move || -> Option<String> {
        let ps = syntax_set();
        let syntax = ps
            .find_syntax_by_token(&lang_owned)
            .or_else(|| ps.find_syntax_by_token(&lang_owned.to_lowercase()))?;
        let theme = atomcode_theme();
        let mut h = HighlightLines::new(syntax, theme);

        let mut out = String::with_capacity(source_owned.len() + 64);
        for (i, line) in source_owned.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            // syntect expects newline-terminated input for context; add one
            // and strip from the per-chunk text so we control line breaks.
            let line_with_nl = format!("{}\n", line);
            let ranges = h.highlight_line(&line_with_nl, ps).ok()?;
            for (style, text) in ranges {
                let text = text.trim_end_matches('\n');
                if text.is_empty() {
                    continue;
                }
                let c = style.foreground;
                if c.a == 0 {
                    // Sentinel default fg -> emit text without ANSI wrapping.
                    out.push_str(text);
                } else {
                    let italic = style.font_style.contains(FontStyle::ITALIC);
                    if italic {
                        out.push_str("\x1b[3m");
                    }
                    out.push_str(&format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b));
                    out.push_str(text);
                    out.push_str(theme::RESET);
                }
            }
        }
        Some(out)
    }));
    match result {
        Ok(opt) => opt,
        Err(_) => {
            crate::tuix_trace!("HL", "syntect panicked while highlighting lang={}", lang);
            None
        }
    }
}

/// Per-line "  " indent for the no-color / unknown-lang path (matches
/// pre-existing `format!("  {}", line)` behavior in `markdown.rs`).
fn indent_plain(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 32);
    let mut first = true;
    for line in source.split('\n') {
        if !first {
            out.push('\n');
        }
        out.push_str("  ");
        out.push_str(line);
        first = false;
    }
    out
}

/// Per-line "  " indent for tinted output. ANSI escapes ride along inside
/// each line — terminals don't count escape bytes as columns.
fn indent_lines(tinted: &str) -> String {
    let mut out = String::with_capacity(tinted.len() + 32);
    let mut first = true;
    for line in tinted.split('\n') {
        if !first {
            out.push('\n');
        }
        out.push_str("  ");
        out.push_str(line);
        first = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{EnvView, TerminalCaps};

    fn caps_color() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
            lang: Some("en_US.UTF-8".to_string()),
            ..Default::default()
        })
    }

    fn caps_nocolor() -> TerminalCaps {
        TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: true,
            term: Some("xterm".to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn no_color_bypasses_highlight_returns_plain_indented() {
        let out = highlight_block(Some("rust"), "let x = 1;", caps_nocolor());
        assert_eq!(out, "  let x = 1;");
        assert!(!out.contains('\x1b'), "no_color path must emit zero ANSI");
    }

    #[test]
    fn no_color_multiline_each_line_indented() {
        let out = highlight_block(Some("rust"), "let x = 1;\nlet y = 2;", caps_nocolor());
        assert_eq!(out, "  let x = 1;\n  let y = 2;");
    }

    #[test]
    fn missing_lang_tag_falls_back_to_plain_indent() {
        let out = highlight_block(None, "x = 42", caps_color());
        assert_eq!(out, "  x = 42");
        assert!(!out.contains('\x1b'), "no lang tag means no ANSI for now");
    }

    #[test]
    fn unknown_lang_via_stub_falls_back_to_plain_indent() {
        // Task 3 stubs highlight_with_syntect to None; dispatch lands in
        // plain-indent for every lang. Task 4 upgrades the rust case below
        // to assert keyword highlighting; this test stays as the proof
        // that unknown / unrecognized langs always degrade safely.
        let out = highlight_block(Some("frobnicate"), "x = 42", caps_color());
        assert_eq!(out, "  x = 42");
    }

    #[test]
    fn empty_source_returns_indent_only() {
        let out = highlight_block(None, "", caps_nocolor());
        assert_eq!(out, "  ");
    }

    #[test]
    fn trailing_newline_preserved_in_output() {
        // source "a\n" -> "  a\n  " (split on \n yields ["a", ""]).
        // This pins the per-line indent contract for stream-formed input.
        let out = highlight_block(None, "a\n", caps_nocolor());
        assert_eq!(out, "  a\n  ");
    }

    #[test]
    fn rust_keyword_gets_keyword_color() {
        // `fn` and `let` should be highlighted as keywords via syntect.
        // Theme maps `keyword` AND `storage` scopes to KEYWORD color, so
        // both flow-control keywords and storage keywords land in purple.
        let out = highlight_block(Some("rust"), "fn main() { let x = 1; }", caps_color());
        assert!(
            out.contains(theme::keyword()),
            "expected keyword color in tinted rust output, got: {:?}",
            out
        );
    }

    #[test]
    fn python_keyword_gets_keyword_color() {
        let out = highlight_block(Some("python"), "def foo():\n    return 1", caps_color());
        assert!(
            out.contains(theme::keyword()),
            "expected keyword color in tinted python output, got: {:?}",
            out
        );
    }

    #[test]
    fn rust_string_literal_gets_string_color() {
        let out = highlight_block(Some("rust"), r#"let s = "hello";"#, caps_color());
        assert!(
            out.contains(theme::string()),
            "expected string color: {:?}",
            out
        );
    }

    #[test]
    fn rust_number_gets_number_color() {
        let out = highlight_block(Some("rust"), "let n = 42;", caps_color());
        assert!(
            out.contains(theme::number()),
            "expected number color: {:?}",
            out
        );
    }

    #[test]
    fn rust_comment_gets_comment_color() {
        let out = highlight_block(Some("rust"), "// a comment\nlet x = 1;", caps_color());
        // COMMENT is "\x1b[3;38;2;124;132;153m" — italic prefix is part of the constant,
        // BUT we may emit italic separately. Check for the truecolor body of COMMENT.
        let comment_body = "\x1b[38;2;124;132;153m";
        let comment_full = theme::comment();
        assert!(
            out.contains(comment_body) || out.contains(comment_full),
            "expected comment color in some form: {:?}",
            out
        );
    }

    #[test]
    fn rust_multiline_string_classified_as_single_string() {
        // syntect must keep multi-line context — both lines of a multi-line
        // raw string should be inside the string-color span.
        let src = "let s = \"line1\nline2\";";
        let out = highlight_block(Some("rust"), src, caps_color());
        let lines: Vec<_> = out.split('\n').collect();
        assert_eq!(lines.len(), 2, "expected 2 output lines, got: {:?}", out);
        assert!(
            lines[0].contains(theme::string()),
            "line0 missing string color: {:?}",
            lines[0]
        );
        assert!(
            lines[1].contains(theme::string()),
            "line1 missing string color: {:?}",
            lines[1]
        );
    }

    #[test]
    fn malformed_input_does_not_panic_returns_plain_indent() {
        // Deeply nested / garbage input historically tripped some highlighters.
        // Our catch_unwind wrapper must keep us alive and return plain indent.
        let nasty = "(".repeat(10_000);
        let out = highlight_block(Some("rust"), &nasty, caps_color());
        assert!(
            out.starts_with("  "),
            "must still produce indented output: {:?}",
            &out[..50.min(out.len())]
        );
    }

    #[test]
    fn unknown_lang_after_syntect_returns_plain_indent() {
        // syntect's find_syntax_by_token returns None for unknown -> plain indent.
        let out = highlight_block(
            Some("frobnicate-xyz-not-a-language"),
            "x = 42",
            caps_color(),
        );
        assert_eq!(out, "  x = 42");
    }
}
