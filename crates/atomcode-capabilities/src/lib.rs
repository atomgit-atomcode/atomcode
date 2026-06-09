//! atomcode-capabilities — **L1 capabilities** layered on the neutral
//! `atomcode-kernel` (L0).
//!
//! # Layering rule (compile-enforced)
//!
//! This crate depends ONLY on `atomcode-kernel` (L0) + third-party crates. It must
//! NEVER depend on `atomcode-core` or any L2/L3 crate. That one-directional edge is
//! what keeps the kernel neutral: every *concrete* capability (a real provider, a
//! real tool, an MCP client, a skill loader) lives up here, never down in the
//! kernel. `cargo tree -p atomcode-capabilities` must not contain `atomcode-core`.
//!
//! # Capabilities are cargo-feature-gated
//!
//! A downstream embedder pulls in only what it needs, so e.g. a build that only
//! wants providers never compiles the (future) MCP/skills transitive deps:
//!   - `provider` (default): real [`LlmProvider`](atomcode_kernel::provider::LlmProvider)
//!     adapters — OpenAI-compatible (GLM / DeepSeek / …), Anthropic Messages (Claude),
//!     and Ollama native (`/api/chat`).
//!   - (future) `tools`, `mcp`, `skills`, `codeintel`.

/// Reusable, provider-agnostic [`atomcode_kernel::hook::LifecycleHooks`]
/// implementations (e.g. [`hooks::WireLogHooks`]). Depends only on the kernel, so it
/// is always available regardless of which capability features are enabled.
pub mod hooks;

#[cfg(feature = "provider")]
pub mod provider;

/// Real, NEUTRAL coding [`Tool`](atomcode_kernel::tool::Tool)s — fs `read`/`write`/
/// `edit`/`list` + `bash` + `grep`/`glob` — plus a generic
/// [`ApprovalMiddleware`](tools::ApprovalMiddleware). Each runs against the kernel's
/// minimal `ToolContext` with NO coding enrichments; see [`tools`] for the trust model.
#[cfg(feature = "tools")]
pub mod tools;

/// Code-intelligence capability: tree-sitter `list_symbols` / `read_symbol` over 12
/// languages. Single-file + stateless (no shared index, no ctx coupling). Opt-in
/// `codeintel` feature (heavy grammar compilation). See [`codeintel`].
#[cfg(feature = "codeintel")]
pub mod codeintel;

/// Skills capability: a markdown/frontmatter skill loader + `use_skill` / `list_skills`
/// tools (Claude-Code-compatible). Opt-in `skills` feature. See [`skills`].
#[cfg(feature = "skills")]
pub mod skills;
