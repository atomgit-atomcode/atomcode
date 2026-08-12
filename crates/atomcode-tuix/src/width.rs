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
    let base = if is_cjk_locale() {
        UnicodeWidthChar::width_cjk(ch)
    } else {
        UnicodeWidthChar::width(ch)
    };
    // Pictographic emoji that live in the legacy symbol blocks (☀ U+2600,
    // ☎ U+260E, ✂ U+2702, ✈ U+2708, ❄ U+2744, …) are East Asian
    // Ambiguous/Narrow per UAX#11, so `unicode-width` reports 1 — but every
    // GUI terminal paints them as a 2-cell colour emoji. That undercount is
    // what skews markdown-table borders by 1 col per emoji (#table-align).
    // Widen to 2 to match the host, but only when the base width is 1 (never
    // shrink an already-wide glyph or a 0-width combining mark) and only when
    // the terminal is one we expect to paint emoji wide (see
    // `emoji_wide_enabled` — opt-out for emoji-incapable consoles).
    //
    // The legacy symbol block gap is handled by `is_wide_emoji_symbol` below.
    //
    // Emoji at U+1F000+ are *supposed* to be width-2 in `unicode-width`
    // (East Asian Width = W), but ~759 codepoints in that range have EA=N
    // (e.g. 🌤 U+1F324, 🌡 U+1F321, 🎖 U+1F396, 🧿 U+1F9FF, …) and
    // `unicode-width` reports width 1 for them. Every GUI terminal still
    // renders them as 2-cell colour emoji, so we widen them here too.
    // The gate `base == Some(1) && emoji_wide_enabled()` ensures we only
    // widen when the host actually paints emoji wide.
    if base == Some(1) && emoji_wide_enabled() && is_wide_emoji_symbol(ch) {
        return Some(2);
    }
    if base == Some(1) && emoji_wide_enabled() && is_narrow_emoji_1f000(ch) {
        return Some(2);
    }
    base
}

/// One-shot probe for "does this terminal paint legacy-block pictographic
/// emoji (☀ ☎ ✈ …) as a 2-cell colour glyph?"
///
/// Default yes — macOS Terminal.app / iTerm2 / WezTerm / kitty / alacritty /
/// VSCode all do, as does Windows Terminal. The one place it isn't safe is a
/// bare legacy Windows console with no emoji font, which paints these as a
/// single-cell box; widening there would mis-pad in the other direction. So
/// on Windows we only default-on when a modern terminal advertises itself
/// (`WT_SESSION` = Windows Terminal, `TERM_PROGRAM` = VSCode/etc.).
/// `ATOMCODE_EMOJI_WIDTH=narrow|0|false` / `wide|1|true` overrides either way.
fn emoji_wide_enabled() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| {
        if let Ok(v) = std::env::var("ATOMCODE_EMOJI_WIDTH") {
            let v = v.trim().to_ascii_lowercase();
            if v == "narrow" || v == "0" || v == "false" {
                return false;
            }
            if v == "wide" || v == "1" || v == "true" {
                return true;
            }
        }
        if cfg!(target_os = "windows") {
            return std::env::var_os("WT_SESSION").is_some()
                || std::env::var_os("TERM_PROGRAM").is_some();
        }
        true
    })
}

/// Whether `ch` is a pictographic emoji in the legacy symbol blocks
/// (< U+1F000) that `unicode-width` reports as width 1 but GUI terminals
/// paint as a 2-cell colour emoji.
///
/// Source: the `Emoji=Yes` set from Unicode `emoji-data.txt`, restricted to
/// the symbol range and excluding the ASCII keycap bases (0-9 `#` `*`) and
/// the text-default ©/® which are normally narrow. Codepoints at U+1F000+
/// are omitted — `unicode-width` already widths them at 2. The gate in
/// `cell_char_width` (`base == Some(1)`) makes any overlap a no-op, so entries
/// that `unicode-width` already calls wide (✅ U+2705, ⭐ U+2B50, …) are
/// harmless to keep for completeness.
fn is_wide_emoji_symbol(ch: char) -> bool {
    let c = ch as u32;
    if !(0x203C..=0x3299).contains(&c) {
        return false;
    }
    // Sorted, non-overlapping inclusive ranges → binary search.
    const RANGES: &[(u32, u32)] = &[
        (0x203C, 0x203C),
        (0x2049, 0x2049),
        (0x2122, 0x2122),
        (0x2139, 0x2139),
        (0x2194, 0x2199),
        (0x21A9, 0x21AA),
        (0x231A, 0x231B),
        (0x2328, 0x2328),
        // NOTE: U+23F8..23FA (⏸⏹⏺ pause/stop/record) are deliberately NOT here.
        // They are Emoji_Presentation=No — bare (without VS16) they default to
        // TEXT presentation and render NARROW (width 1), like the text-default
        // ©/® excluded above. Listing them as wide made the status-line `⏸ plan`
        // badge over-reserve a cell, leaving a stale-glyph residual after ⏸.
        (0x23CF, 0x23CF),
        (0x23E9, 0x23F3),
        (0x24C2, 0x24C2),
        (0x25AA, 0x25AB),
        (0x25B6, 0x25B6),
        (0x25C0, 0x25C0),
        (0x25FB, 0x25FE),
        // NOTE: U+2611 (☑ ballot box with check) is deliberately NOT here. Like
        // ⏸ (see the 0x23F8 note above), it is Emoji_Presentation=No — bare (no
        // VS16) it renders NARROW (width 1). It is the `☑ Todos` panel marker
        // (`todo_marker`), emitted bare; listing it wide over-reserved a cell and
        // desynced the cursor, intermittently blanking a following glyph ("Todos"→
        // "To os"). Text-default symbols belong at their unicode-width (1).
        (0x2600, 0x2604),
        (0x260E, 0x260E),
        (0x2614, 0x2615),
        (0x2618, 0x2618),
        (0x261D, 0x261D),
        (0x2620, 0x2620),
        (0x2622, 0x2623),
        (0x2626, 0x2626),
        (0x262A, 0x262A),
        (0x262E, 0x262F),
        (0x2638, 0x263A),
        (0x2640, 0x2640),
        (0x2642, 0x2642),
        (0x2648, 0x2653),
        (0x265F, 0x2660),
        (0x2663, 0x2663),
        (0x2665, 0x2666),
        (0x2668, 0x2668),
        (0x267B, 0x267B),
        (0x267E, 0x267F),
        (0x2692, 0x2697),
        (0x2699, 0x2699),
        (0x269B, 0x269C),
        (0x26A0, 0x26A1),
        (0x26A7, 0x26A7),
        (0x26AA, 0x26AB),
        (0x26B0, 0x26B1),
        (0x26BD, 0x26BE),
        (0x26C4, 0x26C5),
        (0x26C8, 0x26C8),
        (0x26CE, 0x26CF),
        (0x26D1, 0x26D1),
        (0x26D3, 0x26D4),
        (0x26E9, 0x26EA),
        (0x26F0, 0x26F5),
        (0x26F7, 0x26FA),
        (0x26FD, 0x26FD),
        (0x2702, 0x2702),
        (0x2705, 0x2705),
        (0x2708, 0x270D),
        (0x270F, 0x270F),
        (0x2712, 0x2712),
        (0x2714, 0x2714),
        (0x2716, 0x2716),
        (0x271D, 0x271D),
        (0x2721, 0x2721),
        (0x2728, 0x2728),
        (0x2733, 0x2734),
        (0x2744, 0x2744),
        (0x2747, 0x2747),
        (0x274C, 0x274C),
        (0x274E, 0x274E),
        (0x2753, 0x2755),
        (0x2757, 0x2757),
        (0x2763, 0x2764),
        (0x2795, 0x2797),
        (0x27A1, 0x27A1),
        (0x27B0, 0x27B0),
        (0x27BF, 0x27BF),
        (0x2934, 0x2935),
        (0x2B05, 0x2B07),
        (0x2B1B, 0x2B1C),
        (0x2B50, 0x2B50),
        (0x2B55, 0x2B55),
        (0x3030, 0x3030),
        (0x303D, 0x303D),
        (0x3297, 0x3297),
        (0x3299, 0x3299),
    ];
    RANGES
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Whether `ch` is an emoji in the U+1F000+ range that `unicode-width`
/// reports as width 1 (East Asian Width = N) but GUI terminals paint as a
/// 2-cell colour emoji.
///
/// `unicode-width` assigns width 2 to most U+1F000+ emoji (those with
/// East Asian Width = W), but ~1021 codepoints in this range have EA=N and
/// get width 1 — yet every modern GUI terminal (iTerm2, WezTerm, kitty,
/// VSCode, macOS Terminal.app, Windows Terminal) renders them as a 2-cell
/// colour glyph. That undercount is what makes markdown-table borders
/// mis-align by 1 col per such emoji (issue #708).
///
/// The gate in `cell_char_width` (`base == Some(1) && emoji_wide_enabled()`)
/// ensures we only widen when the host terminal actually paints emoji wide,
/// so this is a no-op on bare legacy consoles.
///
/// Regional Indicator Symbols (U+1F1E6..=U+1F1FF) are **excluded** here:
/// they are used in pairs to form flag emoji (e.g. 🇺🇸), and `cluster_width`
/// already handles the pair-as-one-emoji case. Widening individual RI
/// codepoints in `cell_char_width` would cause `cluster_width` to compute
/// width 4 for a flag instead of 2.
fn is_narrow_emoji_1f000(ch: char) -> bool {
    let c = ch as u32;
    if !(0x1F000..=0x1FA6D).contains(&c) {
        return false;
    }
    // Sorted, non-overlapping inclusive ranges → binary search.
    // Generated from Unicode 15.0: all Category=So codepoints in
    // U+1F000..=U+1FA6D with East_Asian_Width=N, minus Regional
    // Indicator Symbols (U+1F1E6..=U+1F1FF).
    const RANGES: &[(u32, u32)] = &[
        (0x1F000, 0x1F003),
        (0x1F005, 0x1F02B),
        (0x1F030, 0x1F093),
        (0x1F0A0, 0x1F0AE),
        (0x1F0B1, 0x1F0BF),
        (0x1F0C1, 0x1F0CE),
        (0x1F0D1, 0x1F0F5),
        (0x1F10D, 0x1F10F),
        (0x1F12E, 0x1F12F),
        (0x1F16A, 0x1F16F),
        (0x1F1AD, 0x1F1AD),
        // 0x1F1E6..=0x1F1FF (Regional Indicator) excluded — handled
        // by cluster_width's has_emoji_marker path instead.
        (0x1F321, 0x1F32C),
        (0x1F336, 0x1F336),
        (0x1F37D, 0x1F37D),
        (0x1F394, 0x1F39F),
        (0x1F3CB, 0x1F3CE),
        (0x1F3D4, 0x1F3DF),
        (0x1F3F1, 0x1F3F3),
        (0x1F3F5, 0x1F3F7),
        (0x1F43F, 0x1F43F),
        (0x1F441, 0x1F441),
        (0x1F4FD, 0x1F4FE),
        (0x1F53E, 0x1F54A),
        (0x1F54F, 0x1F54F),
        (0x1F568, 0x1F579),
        (0x1F57B, 0x1F594),
        (0x1F597, 0x1F5A3),
        (0x1F5A5, 0x1F5FA),
        (0x1F650, 0x1F67F),
        (0x1F6C6, 0x1F6CB),
        (0x1F6CD, 0x1F6CF),
        (0x1F6D3, 0x1F6D4),
        (0x1F6E0, 0x1F6EA),
        (0x1F6F0, 0x1F6F3),
        (0x1F700, 0x1F776),
        (0x1F77B, 0x1F7D9),
        (0x1F800, 0x1F80B),
        (0x1F810, 0x1F847),
        (0x1F850, 0x1F859),
        (0x1F860, 0x1F887),
        (0x1F890, 0x1F8AD),
        (0x1F8B0, 0x1F8B1),
        (0x1F900, 0x1F90B),
        (0x1F93B, 0x1F93B),
        (0x1F946, 0x1F946),
        (0x1FA00, 0x1FA53),
        (0x1FA60, 0x1FA6D),
    ];
    RANGES
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
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
    if has_emoji_marker { 2 } else { max_w }
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

    // Display width of one grapheme in the SAME model the renderer uses.
    // A `\t` is drawn by `push_str_cells` as SOFT_TAB_WIDTH spaces, so it
    // must be measured as that many columns here — otherwise the input
    // caret renders SOFT_TAB_WIDTH cols left of the real insertion point
    // on every tab-indented (pasted) line. `cluster_width('\t')` is 0.
    let cell_w = |g: &str| -> usize {
        if g == "\t" {
            crate::render::cell::SOFT_TAB_WIDTH
        } else {
            cluster_width(g)
        }
    };

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
            let w = cell_w(g);
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
            let w = cell_w(g);
            lines.last_mut().unwrap().push_str(g);
            col += w;
        }
    }

    // Cursor at end-of-buffer falls through.
    if !cursor_set {
        if col >= max_cols {
            // The last row is filled to (or past) the edge and the caret sits
            // after it. Reporting `cursor_col >= max_cols` would place the caret
            // at/beyond the input box's right edge — which Windows Terminal /
            // conhost cannot represent (they clamp the caret back onto the last
            // glyph, so it appears "before the last character"). Soft-wrap it to
            // col 0 of a fresh next row instead, mirroring the mid-buffer
            // boundary case (a following grapheme would have wrapped here too;
            // at end-of-buffer there is none to trigger it).
            //
            // `>` (not just `==`) also covers the overshoot case: a single
            // grapheme wider than `max_cols` written onto an otherwise-empty row
            // (the wrap guard above refuses to wrap onto an empty line), e.g. a
            // pasted CJK char or a tab on a 1-col-wide input box.
            lines.push(String::new());
            cursor_row = lines.len() - 1;
            cursor_col = 0;
        } else {
            cursor_row = lines.len() - 1;
            cursor_col = col;
        }
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

/// Project a single-line editable value into `max_cols`, keeping a visible
/// caret at `cursor_byte` and adding ellipses on either hidden side. The cursor
/// must be on a UTF-8 boundary; callers that edit by grapheme naturally satisfy
/// that invariant.
pub fn editable_value_projection(value: &str, cursor_byte: usize, max_cols: usize) -> String {
    const CARET: &str = "│";
    const ELLIPSIS: &str = "…";

    if max_cols == 0 {
        return String::new();
    }
    if max_cols == 1 {
        return CARET.to_string();
    }

    let cursor = cursor_byte.min(value.len());
    let before = &value[..cursor];
    let after = &value[cursor..];
    let before_width = display_width(before);
    let after_width = display_width(after);
    let text_budget = max_cols - 1;
    if before_width + after_width <= text_budget {
        return format!("{before}{CARET}{after}");
    }

    let mut left_budget = text_budget / 2;
    let mut right_budget = text_budget - left_budget;
    if after_width < right_budget {
        left_budget += right_budget - after_width;
        right_budget = after_width;
    }
    if before_width < left_budget {
        right_budget += left_budget - before_width;
        left_budget = before_width;
    }

    let left_hidden = before_width > left_budget;
    let right_hidden = after_width > right_budget;
    if left_hidden {
        left_budget = left_budget.saturating_sub(1);
    }
    if right_hidden {
        right_budget = right_budget.saturating_sub(1);
    }

    let mut left_width = 0usize;
    let mut left_parts = Vec::new();
    for grapheme in before.graphemes(true).rev() {
        let width = display_width(grapheme);
        if left_width + width > left_budget {
            break;
        }
        left_parts.push(grapheme);
        left_width += width;
    }
    let left = left_parts.into_iter().rev().collect::<String>();
    let right = truncate_to_width(after, right_budget);
    format!(
        "{}{}{}{}{}",
        if left_hidden { ELLIPSIS } else { "" },
        left,
        CARET,
        right,
        if right_hidden { ELLIPSIS } else { "" }
    )
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
    fn pictographic_symbol_predicate() {
        // Legacy-block emoji `unicode-width` undercounts as 1 → recognised.
        assert!(is_wide_emoji_symbol('☀')); // U+2600 sun (the reported bug)
        assert!(is_wide_emoji_symbol('☁')); // U+2601 cloud
        assert!(is_wide_emoji_symbol('☎')); // U+260E telephone
        assert!(is_wide_emoji_symbol('✂')); // U+2702 scissors
        assert!(is_wide_emoji_symbol('✈')); // U+2708 airplane
        assert!(is_wide_emoji_symbol('❄')); // U+2744 snowflake
        assert!(is_wide_emoji_symbol('⭐')); // U+2B50 star
        assert!(is_wide_emoji_symbol('⚡')); // U+26A1 high voltage
        // NOT emoji — must stay narrow, or we'd regress ordinary ambiguous
        // text symbols (the whole point of scoping to the Emoji set).
        assert!(!is_wide_emoji_symbol('✓')); // U+2713 check mark (Emoji=No)
        assert!(!is_wide_emoji_symbol('°')); // U+00B0 degree sign
        assert!(!is_wide_emoji_symbol('◆')); // U+25C6 black diamond
        assert!(!is_wide_emoji_symbol('×')); // U+00D7 multiplication
        assert!(!is_wide_emoji_symbol('─')); // U+2500 box drawing
        assert!(!is_wide_emoji_symbol('a'));
        assert!(!is_wide_emoji_symbol('你'));
    }

    #[test]
    fn pictographic_symbol_width_matches_gate() {
        // On emoji-capable terminals (the default everywhere except a bare
        // legacy Windows console) ☀ occupies 2 cells. Gate on the same probe
        // the renderer uses so this passes regardless of host.
        let sun = if emoji_wide_enabled() { 2 } else { 1 };
        assert_eq!(display_width("☀"), sun);
        assert_eq!(display_width("☀ 晴"), sun + 1 + 2); // sun + space + CJK
        // Ambiguous-but-not-emoji content is never widened by this path.
        assert_eq!(display_width("✓"), 1);
        assert_eq!(display_width("20°C"), 4);
    }

    #[test]
    fn narrow_emoji_1f000_predicate() {
        // U+1F000+ emoji with EA=N that `unicode-width` undercounts as 1.
        assert!(is_narrow_emoji_1f000('\u{1F324}')); // 🌤 sun behind small cloud
        assert!(is_narrow_emoji_1f000('\u{1F321}')); // 🌡 thermometer
        assert!(is_narrow_emoji_1f000('\u{1F396}')); // 🎖 military medal
        assert!(is_narrow_emoji_1f000('\u{1F5FA}')); // 🗺 world map
        assert!(is_narrow_emoji_1f000('\u{1F700}')); // 🜀 alchemical symbol
        // U+1F1E6..=U+1F1FF (Regional Indicator) are excluded.
        assert!(!is_narrow_emoji_1f000('\u{1F1E6}')); // 🇦 Regional Indicator A
        assert!(!is_narrow_emoji_1f000('\u{1F1FF}')); // 🇿 Regional Indicator Z
        // Wide emoji (EA=W) are NOT in this set — they're already width 2.
        assert!(!is_narrow_emoji_1f000('\u{1F4C5}')); // 📅 calendar (EA=W)
        assert!(!is_narrow_emoji_1f000('\u{1F4A7}')); // 💧 droplet (EA=W)
        // Outside range.
        assert!(!is_narrow_emoji_1f000('a'));
        assert!(!is_narrow_emoji_1f000('你'));
    }

    #[test]
    fn narrow_emoji_1f000_display_width() {
        // On emoji-capable terminals (default), narrow U+1F000+ emoji
        // should report display_width == 2, matching terminal rendering.
        if emoji_wide_enabled() {
            assert_eq!(display_width("\u{1F324}"), 2); // 🌤
            assert_eq!(display_width("\u{1F321}"), 2); // 🌡
            assert_eq!(display_width("\u{1F324} 天气"), 7); // emoji(2) + space(1) + 天(2) + 气(2)
        }
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
    fn editable_projection_tracks_middle_cursor_without_splitting_graphemes() {
        assert_eq!(editable_value_projection("abcdefghij", 5, 8), "…de│fgh…");
        let text = "a👨‍👩‍👧‍👦好z";
        let cursor = "a👨‍👩‍👧‍👦".len();
        let shown = editable_value_projection(text, cursor, 8);
        assert!(shown.contains("👨‍👩‍👧‍👦│好"), "{shown}");
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
    fn wrap_with_cursor_end_of_buffer_full_line_wraps_to_next_row() {
        // Caret at end-of-buffer on a line filled EXACTLY to max_cols must land
        // at col 0 of a fresh next row — NOT col == max_cols. col == max_cols
        // maps (via paint_footer) to one column past the input box's right
        // edge; Windows Terminal / conhost cannot place the caret there and
        // clamps it back onto the last glyph, so it appears "before the last
        // character". Mirrors the mid-buffer boundary case
        // `wrap_with_cursor("abcdef", 3, 3) -> (row 1, col 0)`.
        let (lines, r, c) = wrap_with_cursor("abc", 3, 3);
        assert_eq!(lines, vec!["abc".to_string(), String::new()]);
        assert_eq!((r, c), (1, 0));
    }

    #[test]
    fn wrap_with_cursor_end_of_buffer_full_line_cjk() {
        // Same edge case with a wide (CJK) trailing glyph filling to the edge.
        // "ab你" = 1+1+2 = 4 cols, max=4, caret at end (byte 5).
        let (lines, r, c) = wrap_with_cursor("ab你", 4, 5);
        assert_eq!(lines, vec!["ab你".to_string(), String::new()]);
        assert_eq!((r, c), (1, 0));
    }

    #[test]
    fn wrap_with_cursor_end_of_buffer_overshoot_wide_glyph_on_narrow_box() {
        // A grapheme WIDER than max_cols written onto an otherwise-empty row
        // overshoots (`col > max_cols`) because the wrap guard refuses to wrap a
        // lone glyph off an empty line. The end-of-buffer caret must still land
        // on a fresh next row, not at `col > max_cols` (which is the same
        // caret-past-edge Windows bug). Reachable on a width-3 terminal
        // (text_budget == 1) with a pasted CJK char. "世" is 2 cols, max=1.
        let (lines, r, c) = wrap_with_cursor("世", 1, 3);
        assert_eq!(lines, vec!["世".to_string(), String::new()]);
        assert_eq!((r, c), (1, 0));
    }

    #[test]
    fn wrap_with_cursor_cjk_widths() {
        // "你好" = 4 cols. max=3 → wraps after "你" (width 2 fits, next
        // char 好 (w=2) would overflow 2+2=4>3, so wrap).
        let (lines, _, _) = wrap_with_cursor("你好", 3, 0);
        assert_eq!(lines, vec!["你".to_string(), "好".to_string()]);
    }

    #[test]
    fn wrap_with_cursor_tab_counts_as_soft_tab_width() {
        // Pasted, tab-indented code: a `\t` is DRAWN as SOFT_TAB_WIDTH (4)
        // columns by `push_str_cells`, so the cursor-column model must agree.
        // Regression: tabs were measured as 0 cols, so the caret rendered
        // SOFT_TAB_WIDTH columns left of the real insertion point on every
        // tab-indented line.
        let tab_w = crate::render::cell::SOFT_TAB_WIDTH;
        // Two physical lines; the 2nd is indented with two tabs then "cd".
        let text = "ab\n\t\tcd";
        let (lines, row, col) = wrap_with_cursor(text, 80, text.len());
        assert_eq!(lines.len(), 2);
        assert_eq!(row, 1, "cursor on the 2nd line");
        // end-of-line column = 2 tabs * tab_w + width("cd")
        assert_eq!(col, tab_w * 2 + 2);
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
    fn display_width_bare_media_control_symbols_are_narrow() {
        // ⏸⏹⏺ (U+23F8..23FA) are Emoji_Presentation=No: bare (no VS16) they
        // default to text presentation and render at width 1. The status-line
        // `⏸ plan` badge relies on this — a width-2 reservation left a residual.
        assert_eq!(display_width("\u{23F8}"), 1, "⏸ pause");
        assert_eq!(display_width("\u{23F9}"), 1, "⏹ stop");
        assert_eq!(display_width("\u{23FA}"), 1, "⏺ record");
        assert_eq!(display_width("\u{23F8} plan"), 6, "the whole plan badge");
        // ☑ U+2611 (todo panel marker, emitted bare) is likewise text-default → width 1.
        assert_eq!(display_width("\u{2611}"), 1, "☑ ballot box with check");
        assert_eq!(
            display_width("\u{2611} Todos"),
            7,
            "the todo header marker + title"
        );
        // With VS16 they DO become emoji (width 2) — the exclusion is bare-only.
        assert_eq!(display_width("\u{23F8}\u{FE0F}"), 2, "⏸️ with VS16");
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
        // Family = width 2 = whole cluster. max_cols=3, prefix "a" (1 col) +
        // family (2 col) = 3 = fills the line EXACTLY. The cluster must stay
        // intact on row 0 (never split); the end-of-buffer caret on a full line
        // then soft-wraps to col 0 of a fresh row 1 (see
        // `wrap_with_cursor_end_of_buffer_full_line_wraps_to_next_row` — the
        // caret must not land one column past the right edge).
        let family = "👨\u{200D}👩\u{200D}👦";
        let input = format!("a{family}");
        let cursor_byte = input.len();
        let (lines, r, c) = wrap_with_cursor(&input, 3, cursor_byte);
        assert_eq!(lines, vec![input.clone(), String::new()]);
        assert_eq!((r, c), (1, 0));
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
