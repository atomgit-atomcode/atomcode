//! Real `LlmProvider` adapters (L1).
//!
//! The kernel's [`LlmProvider`](atomcode_kernel::provider::LlmProvider) trait is the
//! seam; these types implement it against real backends. The first adapter targets
//! the **OpenAI-compatible** chat/completions surface (GLM / DeepSeek / any
//! OpenAI-shaped endpoint).
//!
//! Division of labour (mechanism vs policy):
//!   - the kernel owns the *mechanism* — neutral `Message`/`StreamEvent`/`ChatOptions`
//!     and lossless `reasoning` storage;
//!   - this adapter owns the *policy* — how each neutral knob maps onto the wire, how
//!     SSE deltas assemble into whole `ToolCall`s, and whether prior-turn reasoning is
//!     echoed back ([`ReasoningPolicy`]).

mod openai_compat;
mod reasoning;

pub use openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
pub use reasoning::{ReasoningPolicy, REASONING_PLACEHOLDER};
