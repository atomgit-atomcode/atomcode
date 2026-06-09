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

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use ollama::{OllamaConfig, OllamaProvider};
pub use openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
pub use reasoning::{ReasoningPolicy, REASONING_PLACEHOLDER};
pub use retry::RetryPolicy;
