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
fn deepseek_thinking_v4_plus(m: &str) -> bool {
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
        assert_eq!(ReasoningPolicy::derive("qwq-32b", ""), ReasoningPolicy::Preserve);
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
