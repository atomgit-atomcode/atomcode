//! Structured findings: the output shape a review or audit produces.
//!
//! A coding agent's product is a changed working tree. A reviewer's is a list —
//! with priorities, confidence, file and line, and a suggested fix — and prose
//! in a transcript is not that. `report_finding` gives the model somewhere to
//! put them, and the `findings` seam gives whoever launched the run somewhere to
//! read them from.
//!
//! It is a seam rather than a return value because the consumer differs by
//! deployment: a CLI prints them, CI posts them, a web front end renders them,
//! and an eval counts them.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::{Finding, ReportFindingTool};
use atomcode_kernel::tool::Tool;
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

use crate::seams::{Findings, FindingsSvc};

use super::tools::{contribute_prompt, mount};

/// The shipped sink: the report tool's own accumulator.
pub struct ToolBackedFindings {
    tool: ReportFindingTool,
}

#[async_trait]
impl Findings for ToolBackedFindings {
    fn describe(&self) -> String {
        "in-memory, collected from `report_finding`".into()
    }

    fn all(&self) -> Vec<Finding> {
        self.tool.findings()
    }

    fn take(&self) -> Vec<Finding> {
        self.tool.take_findings()
    }
}

pub struct FindingsPlugin;

#[async_trait]
impl Plugin for FindingsPlugin {
    fn name(&self) -> &'static str {
        "tool-report-finding"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["findings"]
    }
    fn description(&self) -> &'static str {
        "the `report_finding` tool and the sink that collects what it reports"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        // One instance, shared: the tool the model calls and the sink the host
        // reads must be the same accumulator, or findings land nowhere.
        let tool = ReportFindingTool::new();
        mount(ctx, vec![Arc::new(tool.clone()) as Arc<dyn Tool>])?;
        let _ = ctx
            .provide::<FindingsSvc>(Arc::new(ToolBackedFindings { tool }))
            .map_err(|e| e.to_string())?;
        contribute_prompt(
            ctx,
            "findings",
            58,
            "Report every issue through `report_finding` — one call per issue, with the file \
             and line range, a priority, and your confidence. Prose in your reply is not a \
             finding and will not be collected.",
        );
        Ok(())
    }
}
