//! How hard the model should think, as a row of its own.
//!
//! The level a person picks belongs to the *session*, not to the model route.
//! Switching models must not silently reset it, so it cannot live on the `llm`
//! row: that row is replaced wholesale by every `--model` / `/model` patch, and
//! anything sharing it would be wiped along with it.
//!
//! Only the STRENGTH lives here. Whether to reason at all is a different value
//! (`thinking.type` on the wire), and it stays a property of the model route —
//! `thinking_type` on the `llm` row. That is the split this tree has: a level is
//! something a person carries across models, a route's ability to reason is not.
//!
//! It reaches the request through the `agent/request` waterfall, which is where
//! [`ChatOptions`] is still mutable — the same seam compaction uses to shrink
//! the messages (see `loop_policy.rs`). Every request the agent makes passes
//! through here, so the choice survives a provider swap without anything having
//! to be re-applied: this row never names a provider, and it does not care which
//! one is mounted.
//!
//! What it deliberately does NOT do is decide whether the endpoint can take the
//! field. If a route cannot, the adapter drops it (`supports_reasoning_effort`),
//! and if the gateway rejects it the adapter remembers that for the session. The
//! person's answer is not the place to encode a route's capabilities.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::provider::ReasoningEffort;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};

/// The row's settings.
///
/// One value, `Option`, so that leaving it out is "no opinion": the endpoint's
/// own default stands, which for DeepSeek is `high` with thinking on.
#[derive(Debug, Deserialize, Default)]
struct EffortRow {
    /// The strength ladder (`low`/`medium`/`high`/`xhigh`/`max`). Unset = no
    /// opinion.
    #[serde(default)]
    level: Option<String>,
}

pub struct ReasoningEffortPlugin;

#[async_trait]
impl Plugin for ReasoningEffortPlugin {
    fn name(&self) -> &'static str {
        "reasoning-effort"
    }
    fn description(&self) -> &'static str {
        "how hard the model should think, independent of which model is mounted"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: EffortRow = if config.is_null() {
            EffortRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        // Parsed with the same lenient parser the rest of the stack uses, so an
        // unknown level is "no opinion" rather than a failure to start. A
        // mis-typed level must not be able to keep a session from opening.
        let Some(level) = ReasoningEffort::from_config(row.level.as_deref()) else {
            return Ok(());
        };
        // No level, no listener: a row that was configured but says nothing
        // should cost nothing, and "no opinion" has to look the same on the wire
        // whether the row is absent or empty.
        let _ = ctx.on_waterfall::<AgentRequest>(Arc::new(SetEffort { level }), false);
        Ok(())
    }
}

/// Writes the row's level onto every request that has not stated one.
struct SetEffort {
    level: ReasoningEffort,
}

#[async_trait]
impl Waterfall<AgentRequest> for SetEffort {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        // Only where the request states nothing. A request with its own level is
        // more specific than a session-wide default and stays untouched — today
        // nothing sets one, but the ordering should be decided here rather than
        // discovered later.
        if req.options.reasoning_effort.is_none() {
            req.options.reasoning_effort = Some(self.level);
        }
        next.run(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_row_has_no_opinion() {
        let row: EffortRow = serde_json::from_value(json!({})).expect("parse");
        assert_eq!(row.level, None);
    }

    #[test]
    fn the_row_carries_a_level_and_nothing_about_the_model() {
        let row: EffortRow = serde_json::from_value(json!({ "level": "high" })).expect("parse");
        assert_eq!(row.level.as_deref(), Some("high"));
    }

    /// The level arrives through the same parser the rest of the stack uses, so
    /// a typo degrades to "no opinion" — it can never keep a session from
    /// opening, and it never becomes a value the gateway is stuck with.
    #[test]
    fn an_unknown_level_is_no_opinion_not_a_failure() {
        for typo in ["hihg", "", "none", "off"] {
            let row: EffortRow = serde_json::from_value(json!({ "level": typo })).expect("parse");
            assert_eq!(
                ReasoningEffort::from_config(row.level.as_deref()),
                None,
                "`{typo}` is not a level this stack can put on the wire"
            );
        }
    }
}
