//! The model's way into the layout.
//!
//! A tool like any other, so it goes through the same approval, tracing and
//! catalog as everything else. Marked `Safe`: it changes nothing durable and
//! `/undo-layout` puts it back, so stopping to ask would be friction with no
//! risk behind it.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde_json::{json, Value};

use crate::layout::{Layout, LayoutOp};
use crate::module::Modules;

fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: false,
        images: Vec::new(),
    }
}

fn err(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: true,
        images: Vec::new(),
    }
}

pub struct AdjustLayout {
    pub layout: Arc<Layout>,
    pub modules: Arc<Modules>,
}

impl AdjustLayout {
    fn known(&self) -> Vec<String> {
        self.modules
            .view_ids()
            .into_iter()
            .map(str::to_string)
            .chain(["mascot".to_string(), "findings".to_string()])
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[async_trait]
impl Tool for AdjustLayout {
    fn name(&self) -> &str {
        "adjust_layout"
    }

    fn description(&self) -> &str {
        "Rearrange the terminal UI: show or hide a panel, swap two areas, \
         resize one, or apply a named layout. Use it when the user asks for a \
         different screen. Reversible with the `undo` op."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["show", "hide", "swap", "resize", "preset", "undo"]
                },
                "module": { "type": "string", "description": "for show / hide" },
                "side": { "type": "string", "enum": ["top", "bottom", "left", "right"] },
                "size": { "type": "integer", "description": "rows or columns" },
                "name": { "type": "string", "description": "for preset" },
                "a": { "type": "string", "description": "for swap: a module id, or `stream`" },
                "b": { "type": "string" }
            },
            "required": ["op"]
        })
    }

    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }

    fn read_only_hint(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        // Accept both the enum's own shape and the flat one the schema
        // advertises: a model that reads the schema and a model that copied the
        // description's example must both work.
        let op: Result<LayoutOp, _> = serde_json::from_str(args).or_else(|_| {
            let v: Value = serde_json::from_str(args).unwrap_or(Value::Null);
            let swap_target = |k: &str| match v.get(k).and_then(Value::as_str) {
                Some("stream") => json!({ "stream": null }),
                Some(m) => json!({ "module": m }),
                None => Value::Null,
            };
            let rebuilt = match v.get("op").and_then(Value::as_str) {
                Some("swap") => json!({
                    "op": "swap", "a": swap_target("a"), "b": swap_target("b")
                }),
                Some("resize") => json!({
                    "op": "resize",
                    "target": swap_target("target"),
                    "size": v.get("size").cloned().unwrap_or(json!(1))
                }),
                _ => v.clone(),
            };
            serde_json::from_value::<LayoutOp>(rebuilt)
        });

        let op = match op {
            Ok(op) => op,
            Err(e) => {
                return err(format!(
                    "看不懂这个布局操作:{e}。例如 \
                     {{\"op\":\"hide\",\"module\":\"status\"}} 或 \
                     {{\"op\":\"preset\",\"name\":\"focus\"}}"
                ))
            }
        };

        match self.layout.apply(&op, &self.known()) {
            Ok(what) => ok(format!(
                "{what}\n\n{}",
                self.layout.describe_for_model(&self.known())
            )),
            // The reason is written for the model to act on, not just for a log.
            Err(e) => err(e.to_string()),
        }
    }
}
