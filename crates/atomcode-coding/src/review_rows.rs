//! The review specialization as two rows of this product's tree:
//! `tool-code-review` (the `code_review` tool and `/review`) and
//! `persona-review` (the read-only reviewer prompt).
//!
//! They lived in the harness, which made the neutral mechanism depend on one
//! particular specialization (`atomcode-review`). The harness keeps what is
//! generic about them — `register_review_command` turns any reviewer tool into
//! `/review` — and the rows that know which reviewer this is live here, beside
//! the product that mounts them.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::plugins::capabilities::register_review_command;
use atomcode_harness::plugins::tools::{contribute_prompt, mount};
use atomcode_kernel::tool::Tool;
use atomcode_plexus::{Context, Plugin};
use atomcode_review::{ReviewTool, ReviewToolConfig, SharedReviewProvider};
use serde::Deserialize;
use serde_json::Value;

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}

#[derive(Debug, Deserialize, Default)]
struct ReviewRow {
    /// None keeps the historical automatic pre-handoff guidance.
    #[serde(default)]
    auto_review: Option<bool>,
    /// What the child reviewer is told it is running.
    ///
    /// Config rather than "just read the seam", and that is the whole design of
    /// this row: `App::patch` remounts only rows whose OWN entry changed, and
    /// `Fibers::unload` cascades to children rather than to consumers — so a
    /// tree that swaps `llm` would leave this row holding the provider it was
    /// given at mount. A tree that swaps models patches this row's `model` in
    /// the same layer (the coding tree does, beside its persona), and the
    /// remount picks up the new provider with the new name. Left unset it is
    /// whatever the `llm` seam answers at mount.
    #[serde(default)]
    model: Option<String>,
    /// Unset ⇒ the provider's own window, else the tool's default.
    #[serde(default)]
    context_window: Option<u32>,
    /// Per-language review rules. Unset ⇒ the built-in ones only.
    #[serde(default)]
    rules_dir: Option<String>,
}

/// The `code_review` tool: a read-only child reviewer over the current changes.
///
/// Not a second agent product — the reviewer is one tool inside THIS agent, the
/// way `task` is. It reuses the host's provider on purpose: a reviewer that
/// built its own would miss a signing gateway and fail where the conversation
/// around it works.
pub struct ReviewToolPlugin;

#[async_trait]
impl Plugin for ReviewToolPlugin {
    fn name(&self) -> &'static str {
        "tool-code-review"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["product-dirs", "tools", "llm"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // `commands` carries the `/review` a person runs; `fs` says which
        // directory the reviewer reads.
        &["commands", "fs"]
    }
    fn description(&self) -> &'static str {
        "the `code_review` tool: a read-only reviewer over the current changes, \
         running its own rounds on the host's provider"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ReviewRow = parse(config)?;
        let provider = ctx
            .service::<atomcode_harness::seams::LlmSvc>()
            .ok_or("the `llm` seam must be filled before `tool-code-review`")?;
        let defaults = ReviewToolConfig::default();
        let cfg = ReviewToolConfig {
            model: row
                .model
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| provider.model_name().to_string()),
            context_window: row
                .context_window
                .or_else(|| Some(provider.context_window()))
                .filter(|w| *w > 0)
                .unwrap_or(defaults.context_window),
            rules_dir: row.rules_dir.map(PathBuf::from),
            ..defaults
        };
        let slot: SharedReviewProvider = Arc::new(std::sync::RwLock::new(Some(provider)));
        let tool = Arc::new(ReviewTool::new(
            slot,
            cfg,
            (*atomcode_harness::product_dirs(ctx)?).clone(),
        ));
        mount(ctx, vec![tool.clone() as Arc<dyn Tool>])?;
        // And as a command a person runs, through the same tool
        // (`docs/adr/0021` §10,
        // `docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` B1):
        // "review what I changed" is a thing a person asks for directly, and
        // asking the model to call a tool on their behalf spends a turn to
        // reach the same reviewer.
        register_review_command(ctx, tool)?;
        // This row's guidance for this row's tool. It lived in the coding persona as
        // `## CODE REVIEW`, which described the tool on BOTH assemblies — and stayed describing
        // it after this row was patched out of the tree.
        let policy = if row.auto_review == Some(false) {
            MANUAL_REVIEW_POLICY
        } else {
            "Use it before handing work back — not instead of running the tests."
        };
        let guidance = format!(
            "`code_review` runs a reviewer over the current changes and reports what it \
             finds. It reads; it never edits. {policy} When the person asks to review code, a diff, staged \
             changes, a commit, or a branch range, call it before writing the review and pass \
             the requested scope and path filters straight to it rather than pre-reading the \
             diff with ordinary read/search tools, which is what the scoped reviewer is for. It \
             may report findings you did not find; weigh them. Do not claim it fixed files or \
             posted comments — it does neither."
        );
        contribute_prompt(ctx, "tool-code-review", 58, &guidance);
        Ok(())
    }
}

/// Shared policy for the standalone and production host-tool mount paths.
pub(crate) const MANUAL_REVIEW_POLICY: &str = "Automatic pre-handoff review is disabled. Call code_review only when the person explicitly asks for a review; do not call it just because a task is finished. Keep running the applicable tests.";

/// The reviewer prompt, taken from the shipped review specialization rather
/// than restated here. Two copies of a hard-won prompt drift, and the one that
/// drifts is always the copy.
pub struct ReviewPersonaPlugin;

#[derive(Debug, Deserialize, Default)]
struct ReviewPersonaRow {
    /// Named in the prompt, and used to decide whether the firmer wording is
    /// needed for models that under-execute without it.
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl Plugin for ReviewPersonaPlugin {
    fn name(&self) -> &'static str {
        "persona-review"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["system-prompt"]
    }
    fn description(&self) -> &'static str {
        "the read-only reviewer prompt from atomcode-review"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ReviewPersonaRow = if config.is_null() {
            ReviewPersonaRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        // No environment read: the model name is asked of whatever provider is
        // actually mounted, which is the same answer from any source — config
        // file, environment, or a scripted one in a test.
        let model = row
            .model
            .clone()
            .or_else(|| {
                ctx.service::<atomcode_harness::seams::LlmSvc>()
                    .map(|p| p.model_name().to_string())
            })
            .unwrap_or_default();
        contribute_prompt(
            ctx,
            "persona-review",
            0,
            &atomcode_review::review_persona(&model),
        );
        Ok(())
    }
}
