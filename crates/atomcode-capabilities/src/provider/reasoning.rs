//! `reasoning_content` round-trip POLICY for OpenAI-compatible providers.
//!
//! The kernel STORES reasoning losslessly on `Message.reasoning`; THIS module decides
//! the per-model *wire* behaviour — whether the prior turn's reasoning is echoed back
//! on the next request. (Mechanism lives in L0; policy lives here in L1.)
//!
//! Why per-model: OpenAI-compatible "reasoning models" disagree on the round-trip.
//!
//! ```text
//! deepseek-v4*           REQUIRES reasoning_content echoed on assistant tool-call
//!                        turns (HTTP 400 "must be passed back" otherwise); an empty
//!                        string is rejected, so a non-empty REASONING_PLACEHOLDER is
//!                        sent when no reasoning was captured.
//! deepseek-r1/reasoner   FORBIDS echoing reasoning_content (HTTP 400 if sent).
//! GLM / everything else  safe default: do not echo (GLM does not error either way;
//!                        omitting keeps requests minimal).
//! ```
//!
//! There is NO opaque signature on this path — reasoning is plain text — so the flat
//! kernel `reasoning: Option<String>` is fully sufficient (see its FUTURE doc note for
//! the signed-provider extension).

/// Placeholder echoed when a model REQUIRES `reasoning_content` on a historical
/// assistant message but none was captured (resumed/compacted history, or a turn
/// produced by a non-thinking model). DeepSeek-V4 rejects an *empty* `reasoning_content`
/// on tool-call messages, so a non-empty placeholder is mandatory under [`ReasoningPolicy::Include`].
///
/// It is a single NON-PROSE sentinel (`·`), not an English sentence: at high context a
/// history full of an English placeholder *sentence* led DeepSeek-V4-Flash to MIMIC it and
/// emit it as its only assistant text, stalling the turn. A bare middle-dot satisfies the
/// non-empty requirement without giving the model prose to echo (ported from core 54c9e4bb).
pub const REASONING_PLACEHOLDER: &str = "·";

/// Whether a model echoes prior-turn `reasoning_content` back on the next request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningPolicy {
    /// Echo `reasoning_content` on **every** assistant message, sending
    /// [`REASONING_PLACEHOLDER`] when none was captured. For models that REJECT a
    /// missing/empty `reasoning_content` on tool-call turns (DeepSeek-V4/Flash,
    /// Moonshot/Kimi/MiMo) — the wire contract, not a preference.
    Include,
    /// Echo `reasoning_content` **only on turns that actually produced it**, and
    /// send nothing when a turn had none — no placeholder. For models that
    /// tolerate omission but benefit from keeping their train of thought across
    /// rounds (GLM, Qwen): retention without the `·` noise on non-thinking turns.
    /// This mirrors oh-my-pi's "replay only when reasoning is present".
    Preserve,
    /// Never echo `reasoning_content`. For models that FORBID it (DeepSeek-R1/
    /// reasoner, HTTP 400 if sent) and as the minimal default for plain models.
    Exclude,
}

impl ReasoningPolicy {
    /// Parse the user-facing `reasoning_history` config value into an explicit
    /// override. `None`/empty ⇒ `Ok(None)` (caller falls back to [`derive`]);
    /// `"include"`/`"preserve"`/`"exclude"` (case/space-insensitive) ⇒ the matching
    /// policy; any other value is a typo and fails fast — mirrors `atomcode-core`'s
    /// load-time validation so a bad config errors the same way on either engine.
    ///
    /// [`derive`]: ReasoningPolicy::derive
    pub fn from_config(value: Option<&str>) -> Result<Option<Self>, String> {
        match value.map(|s| s.trim().to_ascii_lowercase()) {
            None => Ok(None),
            Some(s) if s.is_empty() => Ok(None),
            Some(s) if s == "include" => Ok(Some(ReasoningPolicy::Include)),
            Some(s) if s == "preserve" => Ok(Some(ReasoningPolicy::Preserve)),
            Some(s) if s == "exclude" => Ok(Some(ReasoningPolicy::Exclude)),
            Some(other) => Err(format!(
                "invalid `reasoning_history` value {other:?} — expected \"include\", \
                 \"preserve\", or \"exclude\" (unset = auto-detect)"
            )),
        }
    }

    /// Derive the default policy from the model name + base URL (some vendors are only
    /// identifiable by host, e.g. Moonshot/MiMo gateways). An explicit
    /// [`OpenAiCompatConfig::reasoning_policy`](super::OpenAiCompatConfig) override takes
    /// precedence over this. Faithfully ports `atomcode-core`'s `derive_reasoning_policy`.
    pub fn derive(model: &str, base_url: &str) -> Self {
        let m = model.to_ascii_lowercase();
        let u = base_url.to_ascii_lowercase();
        if m.contains("deepseek-reasoner") || m.contains("deepseek-r1") {
            // DeepSeek V3 family: rejects echoed reasoning_content (400).
            ReasoningPolicy::Exclude
        } else if deepseek_thinking_v4_plus(&m) {
            // DeepSeek V4-and-newer thinking family: REQUIRES reasoning_content on
            // tool-call turns. Version-parsed rather than a `contains("deepseek-v4")`
            // literal so a future `deepseek-v5` does not silently regress to dropping
            // reasoning the way the `deepseek-v4.1-flash` → `deepseek-flash` rename did.
            ReasoningPolicy::Include
        } else if m.starts_with("kimi-")
            || m.starts_with("moonshot")
            || m.starts_with("mimo-")
            || u.contains("moonshot")
            || u.contains("kimi")
            || u.contains("xiaomimimo")
            || u.contains("mimo")
        {
            // Moonshot/Kimi/MiMo: require reasoning_content on every assistant tool_call.
            ReasoningPolicy::Include
        } else if m.contains("glm") || m.contains("qwen") || m.contains("qwq") {
            // GLM / Qwen thinking models: they tolerate omission (no 400 either
            // way), but keeping the reasoning across rounds preserves the train of
            // thought — the "原地打转、想完就忘" symptom. `Preserve` echoes it only
            // on turns that actually produced it (no `·` placeholder), so a
            // non-thinking turn adds nothing. Mirrors oh-my-pi's GLM handling.
            ReasoningPolicy::Preserve
        } else {
            // Plain OpenAI-style models: safe minimal default — nothing to echo.
            // (A non-reasoning model produces no `reasoning_content` anyway.)
            ReasoningPolicy::Exclude
        }
    }
}

/// Whether `m` (already lowercased) names a DeepSeek **V4-or-newer** thinking
/// model — `deepseek-v4`, `deepseek-v4.1-flash`, a future `deepseek-v5`, … — or
/// the versionless `deepseek-flash` (the official rename of `deepseek-v4.1-flash`).
///
/// The version is read from the digits right after `deepseek-v`, so the match
/// tracks the family forward without a per-release code change. `deepseek-v3` and
/// older parse below 4 and fall through; `deepseek-coder-v2` has no `deepseek-v`
/// run at all. The caller has already excluded the R1/reasoner forbidders, so
/// only the V4+ requirers reach this.
pub(crate) fn deepseek_thinking_v4_plus(m: &str) -> bool {
    if m.contains("deepseek-flash") {
        return true;
    }
    match m.split("deepseek-v").nth(1) {
        Some(rest) => {
            let ver: u32 = rest
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            ver >= 4
        }
        None => false,
    }
}

// ── the inbound half: reasoning that arrived in the content channel ──────────

/// The most a partial tag is allowed to hold back before it is given up on.
///
/// A bound, not a tuning knob: without one, a stream whose `<think>` is never
/// closed buffers the whole response instead of showing it.
const MAX_CARRY: usize = 64 * 1024;

/// What one chunk of the content channel turned out to be.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Split {
    /// The answer, as the person should see it.
    pub visible: String,
    /// The model's thinking, to be sent on as reasoning rather than dropped.
    pub reasoning: String,
}

/// Pulls an inline `<think>…</think>` block out of the **content** channel.
///
/// Some OpenAI-compatible reasoning models (DeepSeek-R1, GLM, Qwen and the rest
/// of that lineage) were trained to write their thinking into the content they
/// return, wrapped in `<think>` tags. A serving layer with its reasoning parser
/// configured lifts that into `reasoning_content` before it ever reaches us;
/// one without it — a hand-rolled vLLM or SGLang deployment, the common case
/// for a self-hosted model — passes the tags straight through, and they land in
/// the middle of the answer.
///
/// **This does not throw the thinking away, it puts it back in the right
/// channel.** The block comes out as reasoning, which is the same place the
/// properly-parsed path puts it: the screen's collapsible thought block shows
/// it, the kernel stores it on `Message.reasoning`, and nothing downstream has
/// to know which of the two routes it took. Dropping it would make a
/// configuration difference silently cost a model its train of thought.
///
/// Note that a synthesised reasoning block is from here on indistinguishable
/// from a natively-parsed one — including to [`ReasoningPolicy`], which decides
/// whether it is echoed back on the next request. That is intended, and
/// `a_recovered_block_is_reasoning_like_any_other` says so.
///
/// **Per stream, not per process.** One of these lives on the decoder that owns
/// a single response, so an unclosed `<think>` cannot leak into the next turn.
/// `atomcode-tuix`'s equivalent was a long-lived object that had to be reset
/// between turns by hand, and forgetting left `inside = true` — every later
/// delta silently swallowed, the symptom being blank assistant replies against
/// a provider that was returning text the whole time.
#[derive(Debug, Default)]
pub struct InlineThink {
    /// Text held back because it might be the start of a tag.
    carry: String,
    /// Inside a block, waiting for the close.
    inside: bool,
    /// Whether any non-whitespace answer has been shown yet.
    ///
    /// A real thinking block **leads** the response — the model thinks, then
    /// answers. So once the answer has started, a later `<think>` is the model
    /// writing the tag as text (describing a reasoning model's output format,
    /// say), and taking it as a tag would swallow the rest of the reply: the
    /// person sees the answer stop mid-sentence, while `/resume` — which
    /// re-renders the stored message — shows all of it. Hard to diagnose,
    /// trivial to avoid.
    seen_visible: bool,
}

impl InlineThink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one delta from the content channel.
    pub fn feed(&mut self, delta: &str) -> Split {
        self.carry.push_str(delta);
        let mut split = Split::default();
        loop {
            if self.carry.is_empty() {
                break;
            }
            if self.inside {
                match find_tag(&self.carry, true) {
                    Tag::At { start, end } => {
                        split.reasoning.push_str(&self.carry[..start]);
                        self.carry.drain(..end);
                        self.inside = false;
                    }
                    Tag::Maybe(at) => {
                        split.reasoning.push_str(&self.carry[..at]);
                        self.carry.drain(..at);
                        break;
                    }
                    Tag::No => {
                        split.reasoning.push_str(&self.carry);
                        self.carry.clear();
                        break;
                    }
                }
            } else if self.seen_visible {
                // The answer has started, so nothing left is a tag and nothing
                // needs holding back — hand it all over and stop scanning.
                let all = self.carry.len();
                self.show(&mut split, all);
                break;
            } else {
                match find_tag(&self.carry, false) {
                    Tag::At { start, end } => {
                        // Show what comes before the tag **first**: it may be
                        // the beginning of the answer, and that is what decides
                        // whether this is a tag at all. Deciding beforehand
                        // read `models emit <think> tags…` as a block opening
                        // and swallowed the rest of the sentence.
                        self.show(&mut split, start);
                        if self.seen_visible {
                            continue;
                        }
                        self.carry.drain(..end - start);
                        self.inside = true;
                    }
                    Tag::Maybe(at) => {
                        self.show(&mut split, at);
                        break;
                    }
                    Tag::No => {
                        let all = self.carry.len();
                        self.show(&mut split, all);
                        break;
                    }
                }
            }
        }
        self.spill(&mut split);
        split
    }

    /// The stream ended: nothing more is coming, so let go of what was held.
    ///
    /// An unclosed `<think>` means the block never ended; what was held back is
    /// thinking, and it is sent on as thinking. Silence would be the one
    /// outcome worth avoiding — the turn would end having shown nothing at all.
    pub fn flush(&mut self) -> Split {
        let mut split = Split::default();
        let held = std::mem::take(&mut self.carry);
        if self.inside {
            split.reasoning = held;
        } else {
            split.visible = held;
        }
        split
    }

    /// Move `n` bytes of the carry into the visible half.
    fn show(&mut self, split: &mut Split, n: usize) {
        if n == 0 {
            return;
        }
        let text: String = self.carry.drain(..n).collect();
        if text.chars().any(|c| !c.is_whitespace()) {
            self.seen_visible = true;
        }
        split.visible.push_str(&text);
    }

    /// Give up holding a partial tag once it has grown past [`MAX_CARRY`].
    fn spill(&mut self, split: &mut Split) {
        if self.carry.len() <= MAX_CARRY {
            return;
        }
        let held = std::mem::take(&mut self.carry);
        if self.inside {
            split.reasoning.push_str(&held);
        } else {
            split.visible.push_str(&held);
            self.seen_visible = true;
        }
    }
}

/// Where a tag is in `s`, as far as `s` can say.
enum Tag {
    /// A whole tag, `s[start..end]`.
    At { start: usize, end: usize },
    /// From `at` on, `s` could still become one — hold it back.
    Maybe(usize),
    /// No tag and no possibility of one.
    No,
}

/// Find `<think…>` (or `</think…>` when `closing`), tolerating attributes and
/// any case, and reporting a partial match so a tag split across two chunks is
/// not mistaken for text.
fn find_tag(s: &str, closing: bool) -> Tag {
    let opener = if closing { "</think" } else { "<think" };
    let lower = s.to_ascii_lowercase();
    let mut from = 0;
    while let Some(hit) = lower[from..].find('<') {
        let start = from + hit;
        let rest = &lower[start..];
        // Compared as bytes: slicing `rest` to a fixed length could land inside
        // a multi-byte character (`<中…`) and panic. The opener is ASCII, so a
        // byte comparison answers the same question and cannot.
        let (rb, ob) = (rest.as_bytes(), opener.as_bytes());
        let shared = rb.len().min(ob.len());
        if rb[..shared] != ob[..shared] {
            // This `<` cannot begin the tag we want. Step over it — `from`
            // advances by one byte, and `<` is one byte in UTF-8.
            from = start + 1;
            continue;
        }
        if rest.len() < opener.len() {
            // Ends mid-name: it might still become the tag.
            return Tag::Maybe(start);
        }
        // The name matched. What follows decides: `>` ends it, a letter
        // continues the name (`think` → `thinking`), whitespace begins
        // attributes, anything else means this was a different tag.
        match rest[opener.len()..].find('>') {
            Some(gt) => {
                let between = &rest[opener.len()..opener.len() + gt];
                let plausible = between.is_empty()
                    || between
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || c.is_whitespace());
                if plausible {
                    return Tag::At {
                        start,
                        end: start + opener.len() + gt + 1,
                    };
                }
                from = start + 1;
            }
            // No `>` yet — the tag may still be completed by the next chunk.
            None => return Tag::Maybe(start),
        }
    }
    Tag::No
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_v4_includes() {
        assert_eq!(
            ReasoningPolicy::derive("deepseek-v4-flash", ""),
            ReasoningPolicy::Include
        );
        assert_eq!(
            ReasoningPolicy::derive("DeepSeek-V4", ""),
            ReasoningPolicy::Include
        );
        // The official rename of `deepseek-v4.1-flash` — no longer contains
        // `deepseek-v4`, but is the same thinking family and must still Include.
        assert_eq!(
            ReasoningPolicy::derive("deepseek-flash", ""),
            ReasoningPolicy::Include
        );
        assert_eq!(
            ReasoningPolicy::derive("DeepSeek-Flash", ""),
            ReasoningPolicy::Include
        );
        // Version-parsed, so a future release Includes without a code change.
        assert_eq!(
            ReasoningPolicy::derive("deepseek-v5", ""),
            ReasoningPolicy::Include
        );
        assert_eq!(
            ReasoningPolicy::derive("deepseek-v4.1-flash", ""),
            ReasoningPolicy::Include
        );
        // …but the pre-V4 family and unrelated `-v` models do NOT Include.
        assert_eq!(
            ReasoningPolicy::derive("deepseek-v3", ""),
            ReasoningPolicy::Exclude
        );
        assert_eq!(
            ReasoningPolicy::derive("deepseek-coder-v2", ""),
            ReasoningPolicy::Exclude
        );
    }

    #[test]
    fn deepseek_r1_excludes() {
        assert_eq!(
            ReasoningPolicy::derive("deepseek-r1", ""),
            ReasoningPolicy::Exclude
        );
        assert_eq!(
            ReasoningPolicy::derive("deepseek-reasoner", ""),
            ReasoningPolicy::Exclude
        );
        // r1 wins even if the URL looks like a moonshot host.
        assert_eq!(
            ReasoningPolicy::derive("deepseek-r1", "https://api.moonshot.cn/v1"),
            ReasoningPolicy::Exclude
        );
    }

    #[test]
    fn moonshot_kimi_mimo_include() {
        assert_eq!(
            ReasoningPolicy::derive("kimi-k2", ""),
            ReasoningPolicy::Include
        );
        assert_eq!(
            ReasoningPolicy::derive("moonshot-v1-8k", ""),
            ReasoningPolicy::Include
        );
        // MiMo by MODEL NAME (reuses DeepSeek-V4 thinking protocol) — even on a generic
        // gateway URL that doesn't contain "mimo".
        assert_eq!(
            ReasoningPolicy::derive("mimo-v2.5-pro", "https://generic-gateway.example/v1"),
            ReasoningPolicy::Include
        );
        // identifiable only by host:
        assert_eq!(
            ReasoningPolicy::derive("some-model", "https://api.moonshot.cn/v1"),
            ReasoningPolicy::Include
        );
        assert_eq!(
            ReasoningPolicy::derive("some-model", "https://api-inference.xiaomimimo.com/v1"),
            ReasoningPolicy::Include
        );
    }

    #[test]
    fn from_config_parses_override_or_errors() {
        assert_eq!(ReasoningPolicy::from_config(None), Ok(None));
        assert_eq!(ReasoningPolicy::from_config(Some("")), Ok(None));
        assert_eq!(ReasoningPolicy::from_config(Some("  ")), Ok(None));
        assert_eq!(
            ReasoningPolicy::from_config(Some("include")),
            Ok(Some(ReasoningPolicy::Include))
        );
        assert_eq!(
            ReasoningPolicy::from_config(Some("preserve")),
            Ok(Some(ReasoningPolicy::Preserve))
        );
        assert_eq!(
            ReasoningPolicy::from_config(Some(" Exclude ")),
            Ok(Some(ReasoningPolicy::Exclude))
        );
        assert!(ReasoningPolicy::from_config(Some("sometimes")).is_err());
    }

    #[test]
    fn glm_and_qwen_preserve() {
        // GLM / Qwen: retain the train of thought (echoed only when a turn had it).
        assert_eq!(
            ReasoningPolicy::derive("glm-5.1", "https://open.bigmodel.cn/api/paas/v4"),
            ReasoningPolicy::Preserve
        );
        assert_eq!(
            ReasoningPolicy::derive("qwen3-max", ""),
            ReasoningPolicy::Preserve
        );
        assert_eq!(
            ReasoningPolicy::derive("qwq-32b", ""),
            ReasoningPolicy::Preserve
        );
    }

    #[test]
    fn plain_models_and_default_exclude() {
        // A non-reasoning OpenAI model produces no reasoning_content anyway.
        assert_eq!(
            ReasoningPolicy::derive("gpt-4o", "https://api.openai.com/v1"),
            ReasoningPolicy::Exclude
        );
        assert_eq!(ReasoningPolicy::derive("", ""), ReasoningPolicy::Exclude);
    }
}

#[cfg(test)]
mod inline_think_tests {
    use super::*;

    /// Feed a whole response one chunk at a time and collect both halves.
    fn run(chunks: &[&str]) -> Split {
        let mut think = InlineThink::new();
        let mut all = Split::default();
        for chunk in chunks {
            let got = think.feed(chunk);
            all.visible.push_str(&got.visible);
            all.reasoning.push_str(&got.reasoning);
        }
        let last = think.flush();
        all.visible.push_str(&last.visible);
        all.reasoning.push_str(&last.reasoning);
        all
    }

    #[test]
    fn a_think_block_in_content_becomes_reasoning() {
        let got = run(&["<think>weighing it up</think>the answer"]);
        assert_eq!(got.visible, "the answer");
        assert_eq!(
            got.reasoning, "weighing it up",
            "the thinking is moved, not dropped: it is what the reasoning channel is for"
        );
    }

    #[test]
    fn the_tag_is_recognised_however_it_is_written() {
        // Upper case, the longer spelling, and attributes — all seen in the
        // wild, all the same block.
        for opening in ["<think>", "<THINK>", "<thinking>", "<think kind=\"x\">"] {
            let closing = if opening.to_ascii_lowercase().starts_with("<thinking") {
                "</thinking>"
            } else {
                "</think>"
            };
            let got = run(&[&format!("{opening}hm{closing}done")]);
            assert_eq!(got.visible, "done", "{opening}");
            assert_eq!(got.reasoning, "hm", "{opening}");
        }
    }

    #[test]
    fn a_tag_split_across_chunks_is_still_stripped() {
        // The reason this is a state machine and not a `replace`: a delta ends
        // wherever the network said it did, including inside a tag — and
        // including beside a multi-byte character.
        let got = run(&["<thi", "nk>阿", "巴</thin", "k>答案"]);
        assert_eq!(got.visible, "答案");
        assert_eq!(got.reasoning, "阿巴");
    }

    #[test]
    fn a_literal_think_tag_after_the_answer_started_is_left_alone() {
        // The model explaining what a reasoning model's output looks like. Read
        // as a tag, the rest of the reply disappears: on screen it stops
        // mid-sentence, while `/resume` — which re-renders the stored message —
        // shows all of it.
        let got = run(&["models emit <think> tags, like this, and never close them"]);
        assert_eq!(
            got.visible, "models emit <think> tags, like this, and never close them",
            "an answer that has already started is not re-read as thinking"
        );
        assert_eq!(got.reasoning, "");
    }

    #[test]
    fn an_unclosed_block_is_still_handed_over_at_the_end() {
        // Cut off mid-thought: what was held back is thinking and goes on as
        // thinking. Ending the turn having shown nothing would be the one
        // outcome worth avoiding.
        let got = run(&["<think>I was in the middle of"]);
        assert_eq!(got.visible, "");
        assert_eq!(got.reasoning, "I was in the middle of");
    }

    #[test]
    fn content_with_no_tags_passes_through_untouched() {
        // The negative control. Most models never do this and must not pay for
        // it: nothing held back, nothing rewritten, `<` still a `<`.
        let mut think = InlineThink::new();
        let got = think.feed("a < b, and 1<2 is also true");
        assert_eq!(got.visible, "a < b, and 1<2 is also true");
        assert_eq!(got.reasoning, "");
    }

    #[test]
    fn the_held_back_text_never_grows_past_the_cap() {
        // A `<think>` that never closes must not buffer the whole response.
        let mut think = InlineThink::new();
        think.feed("<think>");
        for _ in 0..40 {
            think.feed(&"x".repeat(4 * 1024));
        }
        assert!(
            think.carry.len() <= MAX_CARRY,
            "held {} bytes, cap is {MAX_CARRY}",
            think.carry.len()
        );
    }
}
