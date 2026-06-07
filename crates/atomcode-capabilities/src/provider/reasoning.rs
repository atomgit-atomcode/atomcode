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
pub const REASONING_PLACEHOLDER: &str = "(no reasoning recorded)";

/// Whether a model echoes prior-turn `reasoning_content` back on the next request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningPolicy {
    /// Echo `reasoning_content` on assistant messages (placeholder if missing).
    Include,
    /// Never echo `reasoning_content`.
    Exclude,
}

impl ReasoningPolicy {
    /// Derive the default policy from a model name. An explicit
    /// [`OpenAiCompatConfig::reasoning_policy`](super::OpenAiCompatConfig) override takes
    /// precedence over this.
    pub fn derive(model: &str) -> Self {
        let m = model.to_ascii_lowercase();
        if m.contains("deepseek-v4") {
            ReasoningPolicy::Include
        } else if m.contains("deepseek-r1") || m.contains("deepseek-reasoner") {
            ReasoningPolicy::Exclude
        } else {
            // GLM and everything else.
            ReasoningPolicy::Exclude
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_v4_includes() {
        assert_eq!(ReasoningPolicy::derive("deepseek-v4-flash"), ReasoningPolicy::Include);
        assert_eq!(ReasoningPolicy::derive("DeepSeek-V4"), ReasoningPolicy::Include);
    }

    #[test]
    fn deepseek_r1_excludes() {
        assert_eq!(ReasoningPolicy::derive("deepseek-r1"), ReasoningPolicy::Exclude);
        assert_eq!(ReasoningPolicy::derive("deepseek-reasoner"), ReasoningPolicy::Exclude);
    }

    #[test]
    fn glm_and_default_exclude() {
        assert_eq!(ReasoningPolicy::derive("glm-5.1"), ReasoningPolicy::Exclude);
        assert_eq!(ReasoningPolicy::derive("gpt-4o"), ReasoningPolicy::Exclude);
        assert_eq!(ReasoningPolicy::derive(""), ReasoningPolicy::Exclude);
    }
}
