//! What a host outside this workspace writes its own rows with, and which of
//! coding's rows it can count on.
//!
//! **Open by mechanism.** A host hands [`crate::CodingRuntime`] any [`Plugin`]
//! and any [`Layer`] through [`HostPlugins`] (`PrepareOptions.host_plugins`).
//! Its layers go last, so it may insert rows of its own and patch, swap or
//! disable any row of coding's — not only the ones listed here.
//!
//! **Stable by promise, for what is listed here.** The rows in
//! [`PUBLISHED_ROWS`], the config shapes documented on them, and the items in
//! this module are what this crate keeps: a published row is not renamed,
//! split or removed without first keeping its old id working for a release and
//! saying so at start. Everything else — any other row id, the harness's own
//! API reached through a direct dependency — is reachable, and is this crate's
//! to change. When it changes, the host is not left running something else:
//! the start fails naming the row or plugin that no longer fits.
//!
//! Criteria: `tests/host_plugins.rs`.

use std::sync::Arc;

pub use crate::parts::HostPlugins;
pub use atomcode_plexus::{Context, Entry, Layer, Plugin};

/// Rows of coding's a host may address and keep addressing.
///
/// | row | what a host does with it |
/// |---|---|
/// | `persona-atomcode` | swap in its own persona: `[[patch]] id = "persona-atomcode"` `name = "<its plugin>"`. The row's config is [`PersonaConfig`]. |
/// | `tools` | narrow the catalog: `config = { exclude = [..], include = [..] }`, patterns with `*`, `row:tool` to name one row's tool (`docs/tool-catalog-policy.md`). The runtime never writes this row's config, so a host's patch is the whole of it. |
/// | `codeintel` | disable: `list_symbols`, `read_symbol`, `find_references` and their prompt fragment |
/// | `code-graph` | disable: `trace_callers`, `trace_callees`, `trace_chain`, `blast_radius`, `file_dependencies` and their prompt fragment |
/// | `tool-ast-grep` | disable: `ast_grep` and its prompt fragment |
///
/// Disable rather than remove: the runtime patches some rows itself (the
/// persona on every `/model`), and a patch cannot reach a row a layer removed.
pub const PUBLISHED_ROWS: &[&str] = &[
    "persona-atomcode",
    "tools",
    "codeintel",
    "code-graph",
    "tool-ast-grep",
];

/// The seams a host row names in `Plugin::inject` (wait until it is filled)
/// or `Plugin::uses` (read it if it is there).
pub mod seams {
    /// The tool catalog. Inject it to add tools with [`super::mount_tools`].
    pub const TOOLS: &str = "tools";
    /// The system prompt. Inject it to add a fragment with
    /// [`super::contribute_prompt`].
    pub const SYSTEM_PROMPT: &str = "system-prompt";
}

/// The rank of the identity line: first in the system prompt. A persona that
/// replaces coding's contributes at this rank.
pub const PERSONA_RANK: i32 = 0;

/// Put `tools` in the catalog for as long as the calling row is mounted.
///
/// The row must inject [`seams::TOOLS`]. A name already in the catalog is
/// refused; to replace one of coding's tools, exclude it as `row:tool` first
/// (`docs/tool-catalog-policy.md`).
pub fn mount_tools(
    ctx: &Context,
    tools: Vec<Arc<dyn atomcode_kernel::tool::Tool>>,
) -> Result<(), String> {
    atomcode_harness::plugins::tools::mount(ctx, tools)
}

/// Add `text` to the system prompt under `id`, ordered by `rank` (lower goes
/// first; [`PERSONA_RANK`] is the identity line), for as long as the calling
/// row is mounted.
///
/// The row should inject [`seams::SYSTEM_PROMPT`]; without the seam this does
/// nothing.
pub fn contribute_prompt(ctx: &Context, id: &str, rank: i32, text: &str) {
    atomcode_harness::plugins::tools::contribute_prompt(ctx, id, rank, text)
}

/// The config the `persona-atomcode` row is mounted with — so also what a
/// host's persona swapped onto that row receives.
///
/// The runtime rewrites it when the model changes, which remounts the row: a
/// persona that names the model reads it from here and stays right.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[non_exhaustive]
pub struct PersonaConfig {
    /// The model the conversation runs on, as the person chose it. Empty means
    /// nobody said; coding's own persona then asks the running provider.
    #[serde(default)]
    pub model: String,
}

impl PersonaConfig {
    /// Read the row's config. Unknown keys are ignored, so a field added later
    /// does not break a host built against this one.
    pub fn from_config(config: &serde_json::Value) -> Result<Self, String> {
        if config.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(config.clone()).map_err(|e| format!("persona row config: {e}"))
    }
}
