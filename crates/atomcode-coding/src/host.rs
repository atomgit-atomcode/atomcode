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
//! Who the agent says it is — the product's name and provider in coding's own
//! persona — is not a row to replace but data to pass: [`ProductIdentity`] on
//! `PrepareOptions.identity`. A host's own persona reads the same identity off
//! [`PersonaConfig`].
//!
//! Criteria: `tests/host_plugins.rs`, `tests/product_identity.rs`.

use std::sync::Arc;

pub use crate::parts::HostPlugins;
pub use crate::persona::ProductIdentity;
pub use atomcode_plexus::{Context, Entry, Layer, Plugin};

/// Rows of coding's a host may address and keep addressing.
///
/// | row | what a host does with it |
/// |---|---|
/// | `persona-atomcode` | swap in its own persona: `[[patch]] id = "persona-atomcode"` `name = "<its plugin>"`. The row's config is [`PersonaConfig`]. To change only who the agent says it is, keep the row and set `PrepareOptions.identity` ([`ProductIdentity`]) instead. |
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
/// The runtime rewrites `model` when the model changes, which remounts the row,
/// and keeps the rest: a persona that names the model or the product reads them
/// from here and stays right.
///
/// `product` and `provider` are the product identity the runtime was started
/// with (`PrepareOptions.identity`): coding's own persona introduces the agent
/// by them, and a host's persona that should say the same reads them here. A
/// row config that does not carry them means this product's own — AtomCode, by
/// AtomGit.
#[derive(Clone, Debug, serde::Deserialize)]
#[non_exhaustive]
pub struct PersonaConfig {
    /// The model the conversation runs on, as the person chose it. Empty means
    /// nobody said; coding's own persona then asks the running provider.
    #[serde(default)]
    pub model: String,
    /// The product's name ([`ProductIdentity::name`]).
    #[serde(default = "default_product")]
    pub product: String,
    /// Who provides it ([`ProductIdentity::provider`]).
    #[serde(default = "default_provider")]
    pub provider: String,
}

fn default_product() -> String {
    ProductIdentity::default().name().to_string()
}

fn default_provider() -> String {
    ProductIdentity::default().provider().to_string()
}

impl Default for PersonaConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            product: default_product(),
            provider: default_provider(),
        }
    }
}

impl PersonaConfig {
    /// The identity this row was configured with.
    pub fn identity(&self) -> ProductIdentity {
        ProductIdentity::new(&self.product, &self.provider)
    }

    /// Read the row's config. Unknown keys are ignored, so a field added later
    /// does not break a host built against this one.
    pub fn from_config(config: &serde_json::Value) -> Result<Self, String> {
        if config.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(config.clone()).map_err(|e| format!("persona row config: {e}"))
    }
}
