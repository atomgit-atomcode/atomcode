// crates/atomcode-tuix/src/width.rs
use std::sync::OnceLock;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// One-shot probe for "use `width_cjk` for ambiguous codepoints?"
///
/// `width_cjk` widens East Asian Ambiguous codepoints (◆ ○ ⓘ ✓ × → •, the
/// box-drawing block, …) from 1 col to 2 cols. The choice has to match
/// what the user's terminal *actually paints*; getting it wrong leaves
/// our cell model and the host's rendering off by 1 col per ambiguous
/// char and downstream cell-diff patches land in the wrong column.
///
/// Detection history:
///
///   * `LC_ALL`/`LANG` starting with `zh`/`ja`/`ko`/`yue` voted yes —
///     dropped in 6a1d42e8 after a macOS Terminal.app + `LANG=zh_CN.UTF-8`
///     user reported `●ddeepwiki__ask_question` char-duplication.
///     POSIX locale describes text encoding, not rendering geometry.
///
///   * Windows `GetACP() ∈ {936, 950, 932, 949}` voted yes on the
///     assumption (from 6d950270) that conhost in a CJK code page paints
///     ambiguous at 2 cols. Empirically wrong for the conhost / ConPTY
///     combo shipping in current Win10/Win11: they paint ambiguous at 1
///     col regardless of ACP. Forcing `width_cjk` on creates the
///     symmetric mismatch (model wider than reality) and shows up as
///     2x-stretched markdown table borders on every Windows host
///     (cmd / pwsh / VSCode pwsh) and Pondering-spinner left/right
///     shake on pwsh + ConPTY paths where the cell-diff patches land
///     1 col off ConPTY's screen buffer. Dropped here.
///
/// Final rule: pure opt-in.
///   * `ATOMCODE_CJK_WIDTH=1` / `=true` → width_cjk on. For users whose
///     terminal really does paint ambiguous at 2 cols (vintage conhost
///     configs, specific font/rendering setups, terminal vt mode flags).
///   * Anything else (default) → off. Matches every modern terminal we
///     know of: macOS Terminal.app, iTerm2, alacritty, kitty, wezterm,
///     Windows Terminal, current Win10/Win11 conhost & ConPTY, VSCode
///     integrated terminal.
fn is_cjk_locale() -> bool {
    static CJK: OnceLock<bool> = OnceLock::new();
    *CJK.get_or_init(|| {
        std::env::var("ATOMCODE_CJK_WIDTH")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Display width of a single code point as our cells should model it. Wraps
/// `UnicodeWidthChar::width`/`width_cjk` and picks the CJK variant when
/// [`is_cjk_locale`] votes yes so East Asian Ambiguous codepoints (`◆`,
/// `○`, `ⓘ`, `✓`, `×`, …) report 2 cols — matching what the user's
/// terminal actually paints. Keeping model and host on the same width
/// rule is what stops the direct-write / cell-diff drift described above.
pub(crate) fn cell_char_width(ch: char) -> Option<usize> {
    if is_cjk_locale() {
        UnicodeWidthChar::width_cjk(ch)
    } else {
        UnicodeWidthChar::width(ch)
    }
}

/// Display width of a single user-perceived character (grapheme cluster).
///
/// `UnicodeWidthChar::width` operates per code point and doesn't know about
/// ZWJ joiners, variation selectors, skin-tone modifiers, or regional
/// indicators. Summing per-char widths gives the wrong answer for emoji
/// sequences that modern terminals render as a single glyph:
///   - 👨‍👩‍👦 (man + ZWJ + woman + ZWJ + boy) renders as 1 emoji = 2 cols
///   - ❤️ (heart + VS-16) renders as 1 emoji = 2 cols (VS-16 forces emoji
///     presentation of an otherwise text-width 1 codepoint)
///   - 👍🏽 (thumb + skin-tone) renders as 1 emoji = 2 cols
///   - 🇺🇸 (regional indicators) renders as 1 flag = 2 cols
///
/// Cluster width strategy:
///   - Single code point: width per UnicodeWidthChar (handles CJK, plain
///     emoji, ASCII, combining marks-as-base, etc.).
///   - Multi code point + contains any emoji-presentation marker
///     (ZWJ U+200D, VS-16 U+FE0F, skin-tone U+1F3FB..=U+1F3FF, regional
///     indicator U+1F1E6..=U+1F1FF): treat as one emoji = 2 cols. This is
///     the convention iTerm2 / Kitty / WezTerm / Terminal.app (recent)
///     follow when rendering Unicode emoji clusters.
///   - Multi code point without an emoji marker (e.g. `a` + combining
///     grave): take the max per-char width — combining marks contribute 0,
///     so the result is the base character's width.
pub(crate) fn cluster_width(g: &str) -> usize {
    let mut iter = g.chars();
    let Some(first) = iter.next() else {
        return 0;
    };
    if iter.clone().next().is_none() {
        return cell_char_width(first).unwrap_or(0);
    }
    let mut has_emoji_marker = false;
    let mut max_w = cell_char_width(first).unwrap_or(0);
    for c in std::iter::once(first).chain(iter) {
        match c {
            '\u{200D}' | '\u{FE0F}' => has_emoji_marker = true,
            '\u{1F3FB}'..='\u{1F3FF}' => has_emoji_marker = true,
            '\u{1F1E6}'..='\u{1F1FF}' => has_emoji_marker = true,
            _ => {}
        }
        let w = cell_char_width(c).unwrap_or(0);
        if w > max_w {
            max_w = w;
        }
    }
    if has_emoji_marker {
        2
    } else {
        max_w
    }
}

/// Terminal column width of a string, CJK- and emoji-cluster-aware.
///
/// Walks user-perceived characters (grapheme clusters) rather than raw
/// code points so multi-codepoint emoji (ZWJ sequences, VS-16, skin-tone
/// modifiers, regional-indicator flags) report their rendered width — see
/// [`cluster_width`] for the per-cluster rule.
pub fn display_width(s: &str) -> usize {
    s.graphemes(true).map(cluster_width).sum()
}

/// Split a line (possibly containing SGR escape sequences) into chunks
/// whose visible display width is at most `max_cols`. SGR bytes pass
/// through without consuming display columns. Handles CJK/emoji width.
///
/// Cluster-aware: walks user-perceived characters (grapheme clusters)
/// not raw code points, so ZWJ-joined emoji families / VS-16 hearts /
/// skin-tone modifiers count as a single 2-col cluster and never get
/// split across a wrap boundary. Mirrors [`truncate_to_width`].
///
/// This is the renderer-side replacement for terminal autowrap: we cannot
/// trust the terminal to wrap consistently at scroll-region boundaries,
/// so we wrap ourselves before emitting.
pub fn wrap_line_to_width(line: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 || line.is_empty() {
        return vec![line.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut cur_width = 0usize;

    // Walk by byte cursor so we can special-case SGR escapes (which
    // span multiple ASCII chars, each its own grapheme) and consume
    // them as a single non-width unit. Outside SGR, advance grapheme
    // by grapheme and count `cluster_width`.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < line.len() {
        if bytes[i] == 0x1b {
            let start = i;
            i += 1;
            while i < line.len() {
                let c = bytes[i];
                i += 1;
                if c.is_ascii_alphabetic() || c == b'~' {
                    break;
                }
            }
            current.push_str(&line[start..i]);
            continue;
        }
        let next = line[i..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(idx, _)| idx + i)
            .unwrap_or(line.len());
        let g = &line[i..next];
        let w = cluster_width(g);
        if cur_width + w > max_cols && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            cur_width = 0;
        }
        current.push_str(g);
        cur_width += w;
        i = next;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Wrap `text` to `max_cols` columns AND locate the cursor's 2D position
/// within the wrapped layout. Honours explicit `\n` as a hard line break
/// (Shift+Enter in the input buffer). Returns `(lines, cursor_row, cursor_col)`
/// where `cursor_row` is 0-based within `lines` and `cursor_col` is the
/// display column within that row.
///
/// `cursor_byte` is a byte offset into `text`; `text.len()` (end-of-buffer)
/// is the expected maximum.
pub fn wrap_with_cursor(
    text: &str,
    max_cols: usize,
    cursor_byte: usize,
) -> (Vec<String>, usize, usize) {
    if max_cols == 0 {
        return (vec![String::new()], 0, 0);
    }
    let mut lines: Vec<String> = vec![String::new()];
    let mut col = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    let mut cursor_set = false;

    // Walk grapheme-by-grapheme so emoji clusters / CJK don't get
    // split mid-cluster at the wrap point. Honours explicit `\n` (a
    // single-codepoint, single-grapheme entity) as a hard break.
    for (byte_pos, g) in text.grapheme_indices(true) {
        let is_newline = g == "\n";
        // Wrap check BEFORE writing the cluster, so a cursor that
        // lands at byte_pos == cursor_byte right at the wrap boundary
        // appears on the new row at col 0 rather than pinned to col
        // `max_cols` on the old row (which would overlap the right
        // border).
        if !is_newline {
            let w = cluster_width(g);
            if col + w > max_cols && !lines.last().unwrap().is_empty() {
                lines.push(String::new());
                col = 0;
            }
        }
        if !cursor_set && byte_pos == cursor_byte {
            cursor_row = lines.len() - 1;
            cursor_col = col;
            cursor_set = true;
        }
        if is_newline {
            lines.push(String::new());
            col = 0;
        } else {
            let w = cluster_width(g);
            lines.last_mut().unwrap().push_str(g);
            col += w;
        }
    }

    // Cursor at end-of-buffer falls through.
    if !cursor_set {
        cursor_row = lines.len() - 1;
        cursor_col = col;
    }
    (lines, cursor_row, cursor_col)
}

/// Slice `s` starting at display column `start_col`, taking up to `max_cols`
/// columns. Characters that straddle the start boundary are skipped. Used to
/// implement horizontal scroll in the input prompt — keeps the cursor visible
/// when the buffer exceeds the viewport width.
pub fn slice_cols(s: &str, start_col: usize, max_cols: usize) -> String {
    let mut col = 0usize;
    let mut acc = String::new();
    let mut acc_w = 0usize;
    // Cluster-aware: never emit half a grapheme — a mid-cluster slice
    // would leak a dangling ZWJ/VS-16/skin-tone modifier into the
    // result and render as gibberish downstream.
    for g in s.graphemes(true) {
        let w = cluster_width(g);
        if col + w <= start_col {
            col += w;
        } else if col < start_col {
            col += w;
        } else {
            if acc_w + w > max_cols {
                break;
            }
            acc.push_str(g);
            acc_w += w;
            col += w;
        }
    }
    acc
}

/// Truncate `s` so its display width is at most `max_cols`.
/// Guaranteed to return a valid UTF-8 string that never splits a grapheme
/// cluster — important for multi-codepoint emoji where a mid-cluster cut
/// would leave a dangling ZWJ / VS-16 / skin-tone modifier visible in the
/// downstream string.
pub fn truncate_to_width(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut acc = String::with_capacity(s.len());
    let mut cols = 0usize;
    for g in s.graphemes(true) {
        let w = cluster_width(g);
        if cols + w > max_cols {
            break;
        }
        acc.push_str(g);
        cols += w;
    }
    acc
}

/// Truncate `s` to `max_cols` display columns, appending `…` when
/// truncation happened so the reader sees a visible "there was more"
/// marker instead of a silent cut mid-word. Reserves 1 column for the
/// ellipsis, so the actual content slice is `max_cols - 1` cols wide.
/// Strings that already fit are returned unchanged.
pub fn truncate_with_ellipsis(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    let budget = max_cols.saturating_sub(1).max(1);
    let mut acc = truncate_to_width(s, budget);
    acc.push('…');
    acc
}

/// Truncate a file-system path to `max_cols` display columns, using a
/// path-aware strategy that preserves the **last segment** (the project or
/// folder name — the most useful bit) and replaces leading segments with
/// `.../`.  Both `/` and `\` are treated as separators.
///
/// Examples (max_cols = 20):
///
///   ~/Documents/WPSDrive/NotLoginPage
///     → .../NotLoginPage          (keeps the last segment)
///
///   ~/a/b/c                       (max_cols = 6)
///     → .../c                     (keeps `.../` + last segment)
///
///   ~/foo                         (max_cols = 5)
///     → ~/foo                     (fits, no truncation)
///
/// If the last segment alone exceeds `max_cols`, the function falls back
/// to a plain `truncate_with_ellipsis` so the output always fits.
pub fn truncate_path(path: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if display_width(path) <= max_cols {
        return path.to_string();
    }

    // Find the last separator and take everything after it.
    let last_sep = path.rfind(|c: char| c == '/' || c == '\\');
    let last_segment = match last_sep {
        Some(i) => &path[i + 1..],
        None => path, // no separator — the whole string is the "segment"
    };

    // Build the candidate: ".../" + last_segment
    let ellipsis_prefix = ".../";
    let candidate = format!("{}{}", ellipsis_prefix, last_segment);

    if display_width(&candidate) <= max_cols {
        return candidate;
    }

    // Last segment is too long even with ".../" prefix — truncate it.
    // Reserve width for ".../" (4 cols).
    let prefix_w = display_width(ellipsis_prefix);
    let budget = max_cols.saturating_sub(prefix_w).max(1);
    let truncated_last = truncate_to_width(last_segment, budget);
    format!("{}{}", ellipsis_prefix, truncated_last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width_equals_len() {
        assert_eq!(display_width("hello"), 5);
    }

    #[test]
    fn cjk_char_is_width_two() {
        assert_eq!(display_width("你好"), 4);
        assert_eq!(display_width("a你b"), 4); // 1 + 2 + 1
    }

    #[test]
    fn emoji_width_is_two() {
        assert_eq!(display_width("👍"), 2);
    }

    #[test]
    fn truncate_to_width_respects_boundary() {
        // 15-char ASCII input, limit width 5 → first 5 chars
        assert_eq!(truncate_to_width("hello world", 5), "hello");
    }

    #[test]
    fn truncate_to_width_cjk_never_splits_char() {
        // "你好world" = 2+2+1+1+1+1+1 = 9 cols; limit 3 → "你" (width 2), not "你\xXX"
        let out = truncate_to_width("你好world", 3);
        assert_eq!(out, "你");
        assert_eq!(display_width(&out), 2);
    }

    #[test]
    fn truncate_to_width_zero_width_safe() {
        assert_eq!(truncate_to_width("abc", 0), "");
    }

    #[test]
    fn truncate_to_width_exact_fit() {
        assert_eq!(truncate_to_width("你好", 4), "你好");
    }

    #[test]
    fn truncate_to_width_preserves_under_limit() {
        assert_eq!(truncate_to_width("hi", 10), "hi");
    }

    #[test]
    fn slice_cols_window_midway() {
        // "abcdefghij" start 3, width 4 → "defg"
        assert_eq!(slice_cols("abcdefghij", 3, 4), "defg");
    }

    #[test]
    fn slice_cols_cjk_straddle_skipped() {
        // "你好world" = 2+2+1+1+1+1+1. start_col=1 straddles "你" → skip it.
        // Then start at col 2 with 4 cols → "好wo".
        assert_eq!(slice_cols("你好world", 1, 4), "好wo");
    }

    #[test]
    fn slice_cols_past_end_empty() {
        assert_eq!(slice_cols("abc", 10, 5), "");
    }

    #[test]
    fn slice_cols_start_zero_matches_truncate() {
        assert_eq!(slice_cols("hello world", 0, 5), "hello");
    }

    #[test]
    fn wrap_with_cursor_short_text_single_row() {
        let (lines, r, c) = wrap_with_cursor("hi", 10, 2);
        assert_eq!(lines, vec!["hi".to_string()]);
        assert_eq!((r, c), (0, 2));
    }

    #[test]
    fn wrap_with_cursor_overflow_moves_to_next_row() {
        let (lines, r, c) = wrap_with_cursor("abcdef", 3, 3);
        assert_eq!(lines, vec!["abc".to_string(), "def".to_string()]);
        // cursor at byte 3 (between abc and def) → start of row 1
        assert_eq!((r, c), (1, 0));
    }

    #[test]
    fn wrap_with_cursor_honours_explicit_newline() {
        let (lines, r, c) = wrap_with_cursor("ab\ncd", 10, 4);
        assert_eq!(lines, vec!["ab".to_string(), "cd".to_string()]);
        assert_eq!((r, c), (1, 1));
    }

    #[test]
    fn wrap_with_cursor_end_of_buffer() {
        let (lines, r, c) = wrap_with_cursor("hello", 10, 5);
        assert_eq!(lines, vec!["hello".to_string()]);
        assert_eq!((r, c), (0, 5));
    }

    #[test]
    fn wrap_with_cursor_cjk_widths() {
        // "你好" = 4 cols. max=3 → wraps after "你" (width 2 fits, next
        // char 好 (w=2) would overflow 2+2=4>3, so wrap).
        let (lines, _, _) = wrap_with_cursor("你好", 3, 0);
        assert_eq!(lines, vec!["你".to_string(), "好".to_string()]);
    }

    // --- truncate_path tests ---

    #[test]
    fn truncate_path_short_path_unchanged() {
        // Path fits within max_cols → returned as-is.
        assert_eq!(truncate_path("~/foo", 20), "~/foo");
    }

    #[test]
    fn truncate_path_keeps_last_segment() {
        // Long path: keep last segment with ".../" prefix.
        assert_eq!(
            truncate_path("~/Documents/WPSDrive/NotLoginPage", 20),
            ".../NotLoginPage"
        );
    }

    #[test]
    fn truncate_path_exact_fit() {
        // ".../NotLoginPage" = 16 cols. At max_cols = 16 it should fit.
        assert_eq!(
            truncate_path("~/Documents/WPSDrive/NotLoginPage", 16),
            ".../NotLoginPage"
        );
    }

    #[test]
    fn truncate_path_very_tight_budget() {
        // Even a single-char last segment + ".../" = 5 cols should fit.
        assert_eq!(truncate_path("~/a/b/c", 6), ".../c");
    }

    #[test]
    fn truncate_path_last_segment_too_long() {
        // Last segment itself exceeds budget after ".../" prefix.
        // ".../" = 4 cols, budget for last segment = 10 - 4 = 6 cols.
        // "NotLoginPage" = 12 cols → truncated to 6 cols.
        assert_eq!(
            truncate_path("~/Documents/WPSDrive/NotLoginPage", 10),
            ".../NotLog"
        );
    }

    #[test]
    fn truncate_path_no_separator() {
        // No path separators → treat entire string as the "last segment".
        // "verylongname" = 12 cols, max 8 → ".../" + 4 cols of name.
        assert_eq!(truncate_path("verylongname", 8), ".../very");
    }

    #[test]
    fn truncate_path_windows_backslash() {
        // Windows paths with backslash separators.
        assert_eq!(
            truncate_path(r"~\Documents\WPSDrive\NotLoginPage", 20),
            ".../NotLoginPage"
        );
    }

    #[test]
    fn truncate_path_zero_cols() {
        assert_eq!(truncate_path("~/foo", 0), "");
    }

    #[test]
    fn truncate_path_cjk_segment() {
        // CJK project name: "项目" = 4 cols, ".../项目" = 8 cols.
        assert_eq!(truncate_path("~/Documents/工作/项目", 20), ".../项目");
    }

    #[test]
    fn truncate_path_cjk_tight_budget() {
        // "项目" = 4 cols, ".../" = 4 cols, total = 8.
        assert_eq!(truncate_path("~/a/b/项目", 8), ".../项目");
    }
    #[test]
    fn wrap_line_to_width_truecolor_sgr_passthrough_zero_width() {
        // Truecolor open `\x1b[38;2;198;120;221m` is 18 bytes of escape sequence.
        // If the SGR-passthrough loop ever stops handling it correctly, those
        // bytes leak into column accounting and downstream wrapping shatters.
        // Pin the invariant: the visible content `let x = 1;` is 10 cols, so
        // it must fit in a 10-col budget with no wrap.
        let tinted = "\x1b[38;2;198;120;221mlet\x1b[23;39m x = 1;";
        let chunks = wrap_line_to_width(tinted, 10);
        assert_eq!(
            chunks.len(),
            1,
            "must not wrap when visible width fits, got: {:?}",
            chunks
        );
        // The tinted line is returned verbatim — escapes still present.
        assert!(chunks[0].contains("\x1b[38;2;198;120;221m"));
    }

    #[test]
    fn wrap_line_to_width_truecolor_with_italic_passthrough() {
        // `\x1b[3;38;2;124;132;153m` is the COMMENT SGR — 3 (italic) plus
        // truecolor fg. Same passthrough guarantee.
        let tinted = "\x1b[3;38;2;124;132;153m// comment\x1b[23;39m";
        let chunks = wrap_line_to_width(tinted, 10);
        assert_eq!(chunks.len(), 1);
    }

    // --- grapheme-cluster width tests ---
    //
    // These tests pin terminal-display semantics for multi-codepoint emoji
    // clusters: ZWJ sequences (family emoji), VS-16 (text→emoji presentation)
    // and skin-tone modifiers are rendered by modern terminals (iTerm2,
    // Kitty, WezTerm, recent Terminal.app) as a single emoji of width 2.
    // Summing per-char widths gives the wrong answer; cursor placement and
    // truncation must use the cluster-aggregated width.

    #[test]
    fn display_width_zwj_family_is_one_emoji() {
        // 👨‍👩‍👦 = U+1F468 U+200D U+1F469 U+200D U+1F466
        // Sum of per-char widths = 2+0+2+0+2 = 6. As one ZWJ-joined emoji = 2.
        let family = "👨\u{200D}👩\u{200D}👦";
        assert_eq!(display_width(family), 2);
    }

    #[test]
    fn display_width_heart_with_vs16_is_emoji_width() {
        // ❤️ = U+2764 (width 1, text presentation) + U+FE0F (VS-16, forces
        // emoji presentation). Rendered as an emoji = width 2.
        assert_eq!(display_width("❤\u{FE0F}"), 2);
    }

    #[test]
    fn display_width_emoji_with_skin_tone_modifier() {
        // 👍🏽 = U+1F44D U+1F3FD. Sum = 4; cluster = 2.
        assert_eq!(display_width("👍\u{1F3FD}"), 2);
    }

    #[test]
    fn truncate_to_width_does_not_split_zwj_cluster() {
        // Cluster has display width 2. Budget of 2 must accept the whole
        // cluster, not return "👨\u{200D}" (a broken cluster — the trailing
        // ZWJ leaks into whatever string is concatenated after).
        let family = "👨\u{200D}👩\u{200D}👦";
        assert_eq!(truncate_to_width(family, 2), family);
    }

    #[test]
    fn truncate_to_width_drops_cluster_that_does_not_fit() {
        // Budget < cluster width → drop the whole cluster (don't emit half).
        let family = "👨\u{200D}👩\u{200D}👦";
        assert_eq!(truncate_to_width(family, 1), "");
    }

    // --- cluster-aware tests for wrap / slice / wrap_with_cursor ---
    //
    // These pin the same "never split a grapheme cluster" property that
    // truncate_to_width already guarantees, now extended to the wrap and
    // slice paths. Pre-fix these were per-codepoint and would chop ZWJ
    // families / VS-16 hearts / skin-tone modifiers mid-cluster, leaving
    // a dangling joiner visible in the output and corrupting downstream
    // grapheme counts.

    #[test]
    fn wrap_line_to_width_does_not_split_zwj_cluster() {
        // Family = width 2. Budget = 2 → must fit on one chunk.
        let family = "👨\u{200D}👩\u{200D}👦";
        let chunks = wrap_line_to_width(family, 2);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], family);
    }

    #[test]
    fn wrap_line_to_width_pushes_zwj_cluster_to_next_chunk() {
        // "ab" (2 cols) + family (2 cols) at budget 2 → "ab" | family,
        // not "ab👨\u{200D}" with a dangling ZWJ.
        let family = "👨\u{200D}👩\u{200D}👦";
        let input = format!("ab{family}");
        let chunks = wrap_line_to_width(&input, 2);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "ab");
        assert_eq!(chunks[1], family);
    }

    #[test]
    fn wrap_line_to_width_sgr_passthrough_still_works() {
        // Regression: post-cluster-aware refactor must still treat SGR
        // as zero-width and pass it through. The visible content is 11
        // cols ("hello world") at budget 5 → 3 chunks; key assertion is
        // that chunk 0 contains "hello" AND the SGR open (i.e. the SGR
        // didn't get counted toward the 5-col budget and force an
        // earlier wrap).
        let tinted = "\x1b[38;2;198;120;221mhello\x1b[0m world";
        let chunks = wrap_line_to_width(tinted, 5);
        assert!(chunks[0].contains("\x1b[38;2;198;120;221m"));
        assert!(chunks[0].contains("hello"));
        // And the SGR open must be present BEFORE "hello" — proving it
        // was carried verbatim and not split off as its own chunk.
        let h_pos = chunks[0].find('h').unwrap();
        let sgr_pos = chunks[0].find("\x1b[").unwrap();
        assert!(sgr_pos < h_pos, "SGR must precede 'hello' in chunk 0");
    }

    #[test]
    fn slice_cols_does_not_split_zwj_cluster() {
        // "ab👨\u{200D}👩\u{200D}👦cd" — start_col=2 (after "ab"), width=2.
        // The cluster at col 2-3 must be selected whole, not chopped.
        let family = "👨\u{200D}👩\u{200D}👦";
        let input = format!("ab{family}cd");
        let out = slice_cols(&input, 2, 2);
        assert_eq!(out, family);
    }

    #[test]
    fn slice_cols_cluster_straddling_start_is_skipped() {
        // start_col=3 falls mid-cluster (cluster spans col 2-3). The
        // cluster gets skipped (the existing "straddle" branch). Next
        // chars at col 4 ("c") and 5 ("d") fit in width 2.
        let family = "👨\u{200D}👩\u{200D}👦";
        let input = format!("ab{family}cd");
        let out = slice_cols(&input, 3, 2);
        assert_eq!(out, "cd");
    }

    #[test]
    fn wrap_with_cursor_does_not_split_zwj_cluster() {
        // Family = width 2 = whole cluster. max_cols=3, prefix "a" (1
        // col) + family (2 col) = 3 = fits. Cursor after family.
        let family = "👨\u{200D}👩\u{200D}👦";
        let input = format!("a{family}");
        let cursor_byte = input.len();
        let (lines, r, _c) = wrap_with_cursor(&input, 3, cursor_byte);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], input);
        assert_eq!(r, 0);
    }

    #[test]
    fn wrap_with_cursor_pushes_zwj_cluster_to_new_row() {
        // max_cols=2, "ab" then family. Family needs 2 cols, "ab" took
        // both → family goes on row 1 intact.
        let family = "👨\u{200D}👩\u{200D}👦";
        let input = format!("ab{family}");
        let (lines, _r, _c) = wrap_with_cursor(&input, 2, 0);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "ab");
        assert_eq!(lines[1], family);
    }
}
