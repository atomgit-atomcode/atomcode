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

/// The `<system-reminder>` convention — one constructor ([`reminder::system_reminder`]) so
/// every runtime-context injector wraps consistently and the wrapper can't be forgotten.
/// Dependency-free, so it is always available regardless of capability features.
pub mod reminder;

/// Claude-Code-compatible EXTERNAL hooks ([`cc_hooks::CCExternalHooks`]) — runs the
/// user's `hooks.json` commands on the kernel's [`LifecycleHooks`]/[`ToolMiddleware`]
/// seams (the port of core's hook engine onto the v2 engine). Opt-in: spawns
/// subprocesses, so it pulls `tokio/process` + `dirs`.
#[cfg(feature = "cc-hooks")]
pub mod cc_hooks;

/// Cache-friendly history compaction strategy ([`compaction::StubCompaction`]) — a
/// [`atomcode_kernel::message::CompactionStrategy`] that stubs old tool results in place.
/// Kernel-only deps, so it is always available regardless of capability features.
pub mod compaction;

/// Shared `$ATOMCODE_HOME` path resolution for the persisting capabilities — one
/// home for the rule (and for documenting its single known `sudo` divergence from
/// production). Internal; compiled only when a feature that persists needs it.
/// `provider` also needs it: the byte-level wire dump lands under `config_dir()/wire-dump`.
#[cfg(any(feature = "mcp", feature = "session", feature = "memory", feature = "provider"))]
pub(crate) mod paths;

/// Kernel-only (L0) console-window suppressors — a local copy of
/// `core::process_utils` so spawn sites here can stop the Windows
/// console-window flash without `capabilities` depending on `core`.
pub(crate) mod process_utils;

/// Proxy policy for outbound HTTP clients — a self-contained mirror of
/// `core::proxy` (reads the process `ATOMCODE_PROXY_MODE` env) so v2 clients
/// honor `no_proxy` without `capabilities` depending on `core`. Compiled
/// whenever a reqwest-using capability is enabled.
#[cfg(any(feature = "provider", feature = "web", feature = "atomgit", feature = "mcp"))]
pub(crate) mod proxy;

/// Ungated path helpers (leading-`~` expansion, home dir) shared by the `tools` and
/// `codeintel` families so model-supplied paths resolve identically across both — see
/// [`pathutil`]. Free of any feature `cfg` because `codeintel` is independent of `tools`.
pub(crate) mod pathutil;

#[cfg(feature = "provider")]
pub mod provider;

/// Real, NEUTRAL coding [`Tool`](atomcode_kernel::tool::Tool)s — fs `read`/`write`/
/// `edit`/`list` + `bash` + `grep`/`glob` — plus a generic
/// [`ApprovalMiddleware`](tools::ApprovalMiddleware). Each runs against the kernel's
/// minimal `ToolContext` with NO coding enrichments; see [`tools`] for the trust model.
#[cfg(feature = "tools")]
pub mod tools;

/// AtomGit REST tools (repo / pull-request / issue). Opt-in `atomgit` feature.
#[cfg(feature = "atomgit")]
pub mod atomgit;

/// Code-intelligence capability: tree-sitter `list_symbols` / `read_symbol` over 12
/// languages. Single-file + stateless (no shared index, no ctx coupling). Opt-in
/// `codeintel` feature (heavy grammar compilation). See [`codeintel`].
#[cfg(feature = "codeintel")]
pub mod codeintel;

/// Skills capability: a markdown/frontmatter skill loader + `use_skill` / `list_skills`
/// tools (Claude-Code-compatible). Opt-in `skills` feature. See [`skills`].
#[cfg(feature = "skills")]
pub mod skills;

/// MCP (Model Context Protocol) capability: connect external MCP servers over
/// stdio / HTTP(SSE) (with OAuth), discover their tools, and surface them as kernel
/// [`Tool`](atomcode_kernel::tool::Tool)s (`mcp__{server}__{tool}`). Ported from
/// `atomcode-core::mcp` with zero core dependency. Opt-in `mcp` feature. See [`mcp`].
#[cfg(feature = "mcp")]
pub mod mcp;

/// Session persistence + cross-session recall: a two-tier on-disk store (a per-turn
/// compacted `<id>.snapshot` for RESUME + an append-only, never-compacted `<id>.jsonl`
/// transcript for RECALL), driven entirely by kernel seams ([`SnapshotHook`](session::SnapshotHook)
/// / [`TranscriptHook`](session::TranscriptHook) on the `turn_complete` terminal hook, a
/// `recall` tool, a current-date injection hook). Wall-clock lives only here (the kernel
/// is clock-free). Opt-in `session` feature. See [`session`].
#[cfg(feature = "session")]
pub mod session;

/// User-driven persistent memory: the production `memory.md` store (global
/// `$ATOMCODE_HOME/memory.md` + per-project `<root>/.atomcode/memory.md` — the SAME
/// files production reads/writes, so the two stacks share one memory) + a
/// [`MemoryHook`](memory::MemoryHook) that injects the merged entries as a system
/// message at `session_start` (fresh sessions only — a resumed snapshot already
/// carries it). v1 has NO model-facing remember/forget tools: the store is written by
/// the user via driver slash-commands. Opt-in `memory` feature. See [`memory`].
#[cfg(feature = "memory")]
pub mod memory;
