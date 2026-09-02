// crates/atomcode-tuix/src/think.rs
pub const THINK_BUF_MAX: usize = 64 * 1024; // 64KB cap to prevent OOM

/// Streaming stripper for `<think>...</think>` and `<thinking>...</thinking>`
/// blocks. Case-insensitive, attribute-tolerant, safe across feed boundaries
/// even when a tag is split mid-UTF-8 sequence.
pub struct ThinkStripper {
    /// Holds text that MIGHT be the start of a tag but we can't yet tell.
    /// Only contains bytes at char boundaries.
    carry: String,
    /// When true, we're inside a <think*> block and waiting for </>.
    inside: bool,
    /// True once any NON-whitespace visible text has been emitted this turn.
    /// A real inline `<think>` reasoning block always LEADS the response (the
    /// model thinks, then answers), so once visible content has streamed a
    /// later `<think>` is the model writing the tag as LITERAL text (e.g.
    /// describing a reasoning-model's output format). We stop stripping then
    /// — otherwise a literal, unclosed `<think>` swallows the rest of the
    /// answer, and the user sees the response cut off mid-sentence (only the
    /// full text on `/resume`, which re-renders the stored raw message).
    seen_visible: bool,
}

impl Default for ThinkStripper {
    fn default() -> Self {
        Self::new()
    }
}

impl ThinkStripper {
    pub fn new() -> Self {
        Self {
            carry: String::new(),
            inside: false,
            seen_visible: false,
        }
    }

    pub fn buffered_bytes(&self) -> usize {
        self.carry.len()
    }

    /// Reset to the pristine state. Call between turns — otherwise an
    /// unclosed `<think>` from a previous turn (model got cancelled, got
    /// an error mid-stream, switched provider, etc.) leaves `inside=true`
    /// and silently swallows every TextDelta of the next turn. Symptom:
    /// user sees blank assistant bubbles even though the provider returned
    /// normal text.
    pub fn reset(&mut self) {
        self.carry.clear();
        self.inside = false;
        self.seen_visible = false;
    }

    /// Feed a chunk, return the visible portion (outside of think blocks).
    pub fn feed(&mut self, delta: &str) -> String {
        // Enforce cap: if carry + delta would exceed THINK_BUF_MAX,
        // we flush the carry as-is (giving up on partial-tag detection)
        // so memory stays bounded.
        if self.carry.len() + delta.len() > THINK_BUF_MAX {
            let mut flushed = std::mem::take(&mut self.carry);
            flushed.push_str(delta);
            if self.inside {
                return String::new(); // still in block, discard
            }
            if flushed.chars().any(is_visible) {
                self.seen_visible = true;
            }
            return flushed;
        }

        self.carry.push_str(delta);
        let mut out = String::new();
        self.drain_into(&mut out);
        out
    }

    fn drain_into(&mut self, out: &mut String) {
        loop {
            if self.inside {
                // Look for closing tag.
                match find_close_tag(&self.carry) {
                    Some((_close_start, close_end)) => {
                        self.carry.drain(..close_end);
                        self.inside = false;
                        // continue loop to scan rest
                    }
                    None => {
                        // keep everything buffered; we might still see </...>
                        // but DO drop all but trailing 11 chars (len of "</thinking>")
                        // to keep memory bounded under streaming.
                        let keep = 11.min(self.carry.len());
                        let drop_end = self.carry.len() - keep;
                        let safe = prev_boundary(&self.carry, drop_end);
                        self.carry.drain(..safe);
                        return;
                    }
                }
            } else {
                // After real content has streamed, stop treating `<think>` as a
                // reasoning block — a leading block is the only real one, so this
                // is now literal text. Pass everything through untouched.
                if self.seen_visible {
                    out.push_str(&self.carry);
                    self.carry.clear();
                    return;
                }
                match find_open_tag(&self.carry) {
                    TagScan::None => {
                        if self.carry.chars().any(is_visible) {
                            self.seen_visible = true;
                        }
                        out.push_str(&self.carry);
                        self.carry.clear();
                        return;
                    }
                    TagScan::Complete { start, end } => {
                        // NON-whitespace content before this `<think>` means it is
                        // NOT a leading reasoning block — it's literal text. Latch
                        // `seen_visible` and pass the rest through untouched.
                        if self.carry[..start].chars().any(is_visible) {
                            self.seen_visible = true;
                            out.push_str(&self.carry);
                            self.carry.clear();
                            return;
                        }
                        out.push_str(&self.carry[..start]); // leading whitespace only
                        self.carry.drain(..end);
                        self.inside = true;
                    }
                    TagScan::PartialAt(pos) => {
                        if self.carry[..pos].chars().any(is_visible) {
                            self.seen_visible = true;
                        }
                        out.push_str(&self.carry[..pos]);
                        self.carry.drain(..pos);
                        return;
                    }
                }
            }
        }
    }
}

enum TagScan {
    None,
    Complete { start: usize, end: usize },
    PartialAt(usize),
}

/// Scan `s` for a `<think>` or `<thinking>` open tag (case-insensitive,
/// tolerant of attributes).
fn find_open_tag(s: &str) -> TagScan {
    let mut search_start = 0;
    while let Some(lt) = s[search_start..].find('<') {
        let abs = search_start + lt;
        let rest = &s[abs..];
        if let Some(end) = parse_open_tag(rest) {
            return TagScan::Complete {
                start: abs,
                end: abs + end,
            };
        }
        // Could it be a partial prefix of an open tag?
        let lower: String = rest.chars().map(|c| c.to_ascii_lowercase()).collect();
        let could_be_partial = lower.len() < 9 && "<thinking".starts_with(lower.as_str())
            || lower.len() < 6 && "<think".starts_with(lower.as_str())
            || lower.starts_with("<think") && !lower.contains('>')
            || lower.starts_with("<thinking") && !lower.contains('>');
        if could_be_partial {
            return TagScan::PartialAt(abs);
        }
        search_start = abs + 1;
    }
    TagScan::None
}

/// If `s` starts with a complete `<think...>` or `<thinking...>` open tag,
/// returns byte end position (just after `>`).
///
/// NOTE: attribute values are matched by finding the next literal `>`.
/// A quoted attribute containing a literal `>` (e.g. `<think foo="a>b">`)
/// would confuse this parser. LLM-generated think tags in practice carry
/// no meaningful attributes, so this trade-off is intentional.
fn parse_open_tag(s: &str) -> Option<usize> {
    if !s.starts_with('<') {
        return None;
    }
    let lower_head: String = s.chars().take(10).map(|c| c.to_ascii_lowercase()).collect();
    let name_end = if lower_head.starts_with("<thinking") {
        9
    } else if lower_head.starts_with("<think") {
        6
    } else {
        return None;
    };
    let after = &s[name_end..];
    let first = after.chars().next()?;
    if first == '>' {
        return Some(name_end + 1);
    }
    if first.is_ascii_whitespace() {
        if let Some(gt) = after.find('>') {
            return Some(name_end + gt + 1);
        }
    }
    None
}

/// Find the next `</think>` or `</thinking>` close tag (case-insensitive).
fn find_close_tag(s: &str) -> Option<(usize, usize)> {
    let lower: String = s.chars().map(|c| c.to_ascii_lowercase()).collect();
    let p1 = lower
        .find("</thinking>")
        .map(|p| (p, p + "</thinking>".len()));
    let p2 = lower.find("</think>").map(|p| (p, p + "</think>".len()));
    match (p1, p2) {
        (Some(a), Some(b)) => Some(if a.0 < b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// True for a char a reader actually SEES. Used to decide whether real content
/// has streamed (which latches off `<think>` stripping — see `seen_visible`).
/// Excludes whitespace, control chars, AND zero-width / format chars (BOM,
/// ZWSP/ZWNJ/ZWJ, word-joiner) that some providers prepend to the first chunk:
/// those are invisible, so they must NOT count as content or a genuine LEADING
/// reasoning block after them would leak through unstripped.
fn is_visible(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_control()
        && !matches!(
            c,
            '\u{200B}' // zero-width space
                | '\u{200C}' // zero-width non-joiner
                | '\u{200D}' // zero-width joiner
                | '\u{2060}' // word joiner
                | '\u{FEFF}' // BOM / zero-width no-break space
        )
}

fn prev_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tags_passes_through() {
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("hello world"), "hello world");
    }

    #[test]
    fn complete_block_in_one_feed() {
        // A LEADING reasoning block (nothing but the tag first) is stripped.
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("<think>secret</think>b"), "b");
    }

    #[test]
    fn tag_split_across_feeds() {
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("<thi"), "");
        assert_eq!(s.feed("nk>secret</think> world"), " world");
    }

    #[test]
    fn utf8_boundary_at_feed_edge_no_panic() {
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("<thi"), "");
        assert_eq!(s.feed("nk>密</think>你好"), "你好");
    }

    #[test]
    fn case_insensitive_tag() {
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("<THINK>a</THINK>b"), "b");
        let mut s2 = ThinkStripper::new();
        assert_eq!(s2.feed("<Think>a</Think>b"), "b");
    }

    #[test]
    fn thinking_tag_also_stripped() {
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("<thinking>a</thinking>b"), "b");
    }

    #[test]
    fn tag_with_attributes() {
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("<think key=\"v\">a</think>b"), "b");
    }

    #[test]
    fn unclosed_block_capped_at_buf_limit() {
        let mut s = ThinkStripper::new();
        let junk = "x".repeat(100_000);
        let input = format!("<think>{}", junk);
        let _ = s.feed(&input);
        assert!(s.buffered_bytes() <= THINK_BUF_MAX);
    }

    #[test]
    fn literal_angle_bracket_outside_tag_preserved() {
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("a < b > c"), "a < b > c");
    }

    #[test]
    fn literal_think_in_answer_is_not_swallowed() {
        // Real-world bug: a model DESCRIBING reasoning-model output writes a
        // literal, UNCLOSED `<think>` deep in its answer. The old stripper
        // entered `inside` and swallowed the rest — the response cut off
        // mid-sentence live, only whole on /resume. It must pass through now.
        let mut s = ThinkStripper::new();
        let ans = "完整生成文本 (<think> 推理链 + <answer> 答案):";
        assert_eq!(s.feed(ans), ans);
    }

    #[test]
    fn leading_bom_or_zero_width_does_not_defeat_stripping() {
        // A provider prepending a BOM / zero-width char before a genuine
        // LEADING reasoning block must not count as visible content — the block
        // still strips (these chars are not `is_whitespace()`, so a naive check
        // would latch `seen_visible` and leak the reasoning).
        let mut s = ThinkStripper::new();
        assert_eq!(
            s.feed("\u{FEFF}<think>secret</think>answer"),
            "\u{FEFF}answer"
        );
        let mut s2 = ThinkStripper::new();
        assert_eq!(s2.feed("\u{200B}<think>x</think>y"), "\u{200B}y");
    }

    #[test]
    fn literal_think_across_streaming_feeds_is_not_swallowed() {
        // The tag arrives in a LATER delta than the preceding content — the
        // common streaming shape. `seen_visible` from the first delta keeps
        // the `<think>` in the second delta from truncating the answer.
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("每题提取完整 "), "每题提取完整 ");
        assert_eq!(s.feed("<think> 推理链"), "<think> 推理链");
    }

    #[test]
    fn only_the_leading_block_is_stripped() {
        // The leading `<think>x</think>` is stripped; once real content ("b")
        // has streamed, a later `<think>` is literal text and passes through
        // (a reasoning model emits its ONE reasoning block up front, so a
        // second, mid-answer tag is the model quoting `<think>`, not thinking).
        let mut s = ThinkStripper::new();
        assert_eq!(
            s.feed("<think>x</think>b<think>y</think>c"),
            "b<think>y</think>c"
        );
    }

    /// Regression: an unclosed `<think>` from a previous turn would leave
    /// `inside=true` and swallow the entire next turn's text. The real-world
    /// trigger is a provider switch mid-turn (e.g. GLM → Kimi): GLM embeds
    /// thinking as `<think>…</think>` in content, Kimi routes it through
    /// `reasoning_content` (plain content with no `<think>` tag). If the
    /// GLM turn cancels with an open tag and no one calls `reset()`, every
    /// Kimi TextDelta afterward disappears — user sees blank assistant
    /// bubbles while datalog shows the LLM actually returned text.
    #[test]
    fn reset_clears_stuck_inside_state() {
        let mut s = ThinkStripper::new();
        // Turn 1: opens a think block but never closes it (stream ended /
        // got cancelled).
        let _ = s.feed("<think>still thinking when we got cut");
        // Turn 2 from a different provider that doesn't use <think> tags.
        // Without reset, this text gets swallowed.
        assert_eq!(
            s.feed("hello from the next model"),
            "",
            "without reset, text leaks through the stuck inside=true state",
        );
        // Apply the fix.
        s.reset();
        // Turn 3: now the stripper is pristine and plain text passes.
        assert_eq!(
            s.feed("hello from the next model"),
            "hello from the next model"
        );
    }

    #[test]
    fn reset_from_pristine_state_is_a_noop() {
        // Calling reset on a fresh stripper shouldn't break subsequent feeds.
        let mut s = ThinkStripper::new();
        s.reset();
        assert_eq!(s.feed("plain text"), "plain text");
    }

    #[test]
    fn reset_clears_partial_carry_at_feed_boundary() {
        // A feed that ends mid-tag leaves bytes in `carry` awaiting the
        // rest of the tag. Reset should flush that too so the next turn
        // starts clean.
        let mut s = ThinkStripper::new();
        assert_eq!(s.feed("hello <thi"), "hello "); // carry now holds "<thi"
        assert!(s.buffered_bytes() > 0);
        s.reset();
        assert_eq!(s.buffered_bytes(), 0);
        // Without reset the next feed would try to complete the tag;
        // with reset, "<think>" is treated as the start of a new block.
        assert_eq!(s.feed("not a tag: <3"), "not a tag: <3");
    }
}
