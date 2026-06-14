//! Real `LlmProvider` adapters (L1).
//!
//! The kernel's [`LlmProvider`](atomcode_kernel::provider::LlmProvider) trait is the
//! seam; these types implement it against real backends. Three adapters live here:
//!   - [`OpenAiCompatProvider`] — the **OpenAI-compatible** chat/completions surface
//!     (GLM / DeepSeek / any OpenAI-shaped endpoint);
//!   - [`AnthropicProvider`] — the **Anthropic Messages API** (`/v1/messages`, Claude),
//!     including the signed extended-thinking round-trip;
//!   - [`OllamaProvider`] — the **Ollama native** `/api/chat` (local models, NDJSON).
//!
//! Division of labour (mechanism vs policy):
//!   - the kernel owns the *mechanism* — neutral `Message`/`StreamEvent`/`ChatOptions`
//!     and lossless `reasoning` storage;
//!   - this adapter owns the *policy* — how each neutral knob maps onto the wire, how
//!     SSE deltas assemble into whole `ToolCall`s, and whether prior-turn reasoning is
//!     echoed back ([`ReasoningPolicy`]).

mod anthropic;
mod ollama;
mod openai_compat;
mod reasoning;
mod retry;
mod sign;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use ollama::{OllamaConfig, OllamaProvider};
pub use openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
pub use reasoning::{ReasoningPolicy, REASONING_PLACEHOLDER};
pub use retry::RetryPolicy;
pub use sign::{RequestSigner, SignedAuth};

use serde_json::{json, Value};

/// Push a `system` wire message, COALESCING it into the previous wire entry when that is
/// also a `system` message (joined with a blank line).
///
/// The kernel's neutral history can carry SEVERAL `Role::System` messages (persona +
/// `memory.md` + any future capability), but many OpenAI-compatible models / chat
/// templates accept only a SINGLE system message — extra ones are rejected outright or
/// silently honor just the first (dropping memory). Both `role:"system"`-on-the-wire
/// adapters (OpenAI-compatible and Ollama) route their `Role::System` arm through here so
/// a model never sees more than one. (The Anthropic adapter instead lifts+joins all System
/// messages into the top-level `system` field — same guarantee, different wire shape.)
///
/// Coalescing is over CONSECUTIVE system entries only; in practice every System message is
/// leading and contiguous, so this yields exactly one leading system block. It is pure and
/// deterministic, so the outgoing prefix stays byte-stable across rounds (cache-safe).
pub(crate) fn push_system_coalesced(out: &mut Vec<Value>, text: &str) {
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some("system") {
            let prev = last.get("content").and_then(Value::as_str).unwrap_or("");
            let joined = if prev.is_empty() || text.is_empty() {
                format!("{prev}{text}")
            } else {
                format!("{prev}\n\n{text}")
            };
            last["content"] = json!(joined);
            return;
        }
    }
    out.push(json!({ "role": "system", "content": text }));
}

#[cfg(test)]
mod coalesce_tests {
    use super::push_system_coalesced;
    use serde_json::json;

    #[test]
    fn merges_runs_and_preserves_non_system_boundaries() {
        let mut out = Vec::new();
        push_system_coalesced(&mut out, "persona");
        push_system_coalesced(&mut out, "memory");
        assert_eq!(out, vec![json!({"role":"system","content":"persona\n\nmemory"})]);
        // A non-system entry breaks the run: a later system would start a fresh block.
        out.push(json!({"role":"user","content":"hi"}));
        push_system_coalesced(&mut out, "late");
        assert_eq!(out.len(), 3, "system after a user is NOT merged into the leading block");
        assert_eq!(out[2], json!({"role":"system","content":"late"}));
    }

    #[test]
    fn empty_text_does_not_inject_blank_separator() {
        let mut out = Vec::new();
        push_system_coalesced(&mut out, "");
        push_system_coalesced(&mut out, "real");
        assert_eq!(out, vec![json!({"role":"system","content":"real"})]);
    }
}
