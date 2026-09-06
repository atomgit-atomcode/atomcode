//! The personas — one prompt fragment among several, not a privileged system
//! message. Removing the row leaves a working agent with no opinion, which is
//! what makes the runtime a harness rather than a product.
//!
//! # A persona row owns the identity sentence, and nothing else may
//!
//! "You are X" is this module's to say. [`super::self_knowledge`] states what
//! the agent is *assembled from* and never who it is, because both fragments
//! reach the model in the same request: two rows opening with "you are" is two
//! answers to one question, and the one that loses is whichever the model reads
//! second. Keeping the sentence in exactly one row is also what lets an
//! assembly change the answer — a product with its own name swaps this row, and
//! nothing else in the tree has to know.

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

const CODING_PERSONA: &str = "\
You are a coding agent working in a real repository. Investigate before you act: \
read the code you are about to change, and prefer the smallest change that solves \
the problem. After an edit, verify it — run the build or the tests — and report what \
you actually observed rather than what you expect. If something is blocked, say so \
plainly and continue with the rest.";

#[derive(Debug, Deserialize, Default)]
struct PersonaRow {
    /// Appended after the built-in persona. This is how a deployment adds house
    /// rules without forking the plugin.
    #[serde(default)]
    extra: Option<String>,
}

pub struct CodingPersonaPlugin;

#[async_trait]
impl Plugin for CodingPersonaPlugin {
    fn name(&self) -> &'static str {
        "persona-coding"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["system-prompt"]
    }
    fn description(&self) -> &'static str {
        "the coding specialization's system prompt"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: PersonaRow = if config.is_null() {
            PersonaRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let mut text = CODING_PERSONA.to_string();
        if let Some(extra) = row.extra.filter(|e| !e.trim().is_empty()) {
            text.push_str("\n\n");
            text.push_str(extra.trim());
        }
        // Rank 0: the persona leads, tool guidance follows.
        super::tools::contribute_prompt(ctx, "persona-coding", 0, &text);
        Ok(())
    }
}

// ---- the audit personas -------------------------------------------------

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
        let model = row
            .model
            .or_else(|| std::env::var("ATOMCODE_MODEL").ok())
            .unwrap_or_default();
        super::tools::contribute_prompt(
            ctx,
            "persona-review",
            0,
            &atomcode_review::review_persona(&model),
        );
        Ok(())
    }
}

const SECURITY_PERSONA: &str = "\
You are a security reviewer with read-only access to a codebase. Your job is to find \
real, reachable vulnerabilities — not to enumerate theoretical weaknesses.

For each candidate issue, establish three things before reporting it:
1. **The sink** — the operation that is actually dangerous (a query, a command, a \
   deserialization, a path join, a comparison used for authorization).
2. **The source** — where attacker-influenced data enters, and the concrete path from \
   there to the sink. Use `find_references` and `trace_callers` to establish reachability \
   rather than assuming it.
3. **What defeats the existing defence** — if there is validation, escaping, or a check \
   in between, say precisely why it does not hold. If you cannot, it is not a finding.

Priorities: P0 = remotely exploitable with real impact; P1 = exploitable with \
preconditions; P2 = a defence-in-depth gap with no established path; P3 = hygiene. \
Set your confidence honestly — a P0 you are 40% sure of is worth reporting *as* 40%.

Report through `report_finding`, one call per issue, with the file and the exact line \
range of the sink. Do not report: dependency versions without a known exploit path in \
this code, missing hardening that the threat model does not call for, or style. If you \
find nothing that meets this bar, say so — an empty result is a valid outcome and is \
far more useful than a list of maybes.";

pub struct SecurityPersonaPlugin;

#[async_trait]
impl Plugin for SecurityPersonaPlugin {
    fn name(&self) -> &'static str {
        "persona-security"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["system-prompt"]
    }
    fn description(&self) -> &'static str {
        "a read-only security reviewer that must establish reachability"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        super::tools::contribute_prompt(ctx, "persona-security", 0, SECURITY_PERSONA);
        Ok(())
    }
}
