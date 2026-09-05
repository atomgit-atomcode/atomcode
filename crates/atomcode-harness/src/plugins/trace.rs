//! Observability as a row.
//!
//! It listens; nothing produces for it. Remove the row and the harness is silent
//! — no flag threaded through the loop, no `if verbose` anywhere.

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{
    AssistantChunk, AssistantMessage, Chunk, ToolResultEvent, TurnEnd, TurnStart, TurnStarted,
};
use crate::seams::TurnOutcome;

#[derive(Debug, Deserialize, Default)]
struct TraceRow {
    /// Stream assistant text to stdout as it arrives.
    #[serde(default)]
    stream: bool,
    /// Print one line per tool result.
    #[serde(default)]
    tools: bool,
    /// Print the turn summary.
    #[serde(default)]
    summary: bool,
}

pub struct TracePlugin;

#[async_trait]
impl Plugin for TracePlugin {
    fn name(&self) -> &'static str {
        "trace"
    }
    fn description(&self) -> &'static str {
        "print the turn as it happens, by listening to the loop's events"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: TraceRow = if config.is_null() {
            TraceRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };

        let _ = ctx.on_emit::<TurnStart>(|started: &TurnStarted| {
            eprintln!("\x1b[2m› {}\x1b[0m", started.prompt);
        });

        if row.stream {
            let _ = ctx.on_emit::<AssistantChunk>(|chunk: &Chunk| {
                use std::io::Write;
                if chunk.reasoning {
                    eprint!("\x1b[2m{}\x1b[0m", chunk.text);
                } else {
                    print!("{}", chunk.text);
                    let _ = std::io::stdout().flush();
                }
            });
        }

        if row.tools {
            let _ = ctx.on_emit::<AssistantMessage>(|message: &Message| {
                for call in &message.tool_calls {
                    let args = call.arguments.replace('\n', " ");
                    let shown = args.chars().take(120).collect::<String>();
                    eprintln!("\x1b[36m⚒ {}\x1b[0m {}", call.name, shown);
                }
            });
            let _ = ctx.on_emit::<ToolResultEvent>(|result: &ToolResult| {
                let mark = if result.is_error { "✗" } else { "✓" };
                let first = result.content.lines().next().unwrap_or("");
                let shown = first.chars().take(120).collect::<String>();
                eprintln!("  {mark} {shown}");
            });
        }

        if row.summary {
            let _ = ctx.on_emit::<TurnEnd>(|outcome: &TurnOutcome| {
                eprintln!(
                    "\n\x1b[2m— {:?} after {} round(s), {} tool call(s){}\x1b[0m",
                    outcome.stop,
                    outcome.rounds,
                    outcome.tool_calls,
                    outcome
                        .error
                        .as_ref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                );
            });
        }
        Ok(())
    }
}
