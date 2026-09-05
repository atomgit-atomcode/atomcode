//! Tool scheduling: how a round's calls run against each other.
//!
//! The loop's terminal runs them in a line, which is always correct and
//! sometimes slow. This row overlaps the ones that declare themselves safe to
//! overlap. It is a listener rather than loop code because scheduling is a
//! policy — a deployment that does not trust its tools' concurrency claims
//! removes the row and gets serial execution back, with nothing left behind.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use futures::future::BoxFuture;
use futures::stream::FuturesOrdered;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::events::{ToolBatch, ToolExec, ToolsExecuteBatch};
use crate::exec::execute_one;
use crate::seams::ToolsSvc;

/// Guards `Semaphore::new`, which asserts on absurd permit counts.
const CEILING: usize = 256;

#[derive(Debug, Deserialize)]
struct Row {
    /// How many `parallel_safe` calls may overlap. `1` is serial.
    #[serde(default = "default_max")]
    max_parallel: usize,
}

impl Default for Row {
    fn default() -> Self {
        Self {
            max_parallel: default_max(),
        }
    }
}

fn default_max() -> usize {
    4
}

struct Parallel {
    ctx: Context,
    max_parallel: usize,
}

#[async_trait]
impl Waterfall<ToolsExecuteBatch> for Parallel {
    async fn handle(
        &self,
        batch: &mut ToolBatch,
        next: Next<'_, ToolsExecuteBatch>,
    ) -> Vec<ToolResult> {
        // One call has nothing to overlap with; delegate so the terminal (or a
        // listener below) keeps whatever behaviour it has.
        if batch.calls.len() <= 1 || self.max_parallel <= 1 {
            return next.run(batch).await;
        }

        // Two things are balanced here. Read-only calls have no reason to be
        // serialized — a round with four greps takes four times as long as it
        // needs to. But a side-effecting call must not run while anything else
        // is in flight, or a concurrent read observes a half-applied mutation.
        //
        // So an `RwLock` is used as a barrier: a tool that declares itself
        // `parallel_safe` takes the read side and overlaps, anything else takes
        // the write side and runs alone. A `Semaphore` bounds how many overlap.
        // `FuturesOrdered` yields in push order, so results come back in
        // emission order whatever finished first, and the transcript stays
        // reproducible.
        let toolbox = self.ctx.service::<ToolsSvc>();
        let gate = Arc::new(tokio::sync::RwLock::new(()));
        let permits = Arc::new(tokio::sync::Semaphore::new(
            self.max_parallel.clamp(1, CEILING),
        ));

        let mut ordered: FuturesOrdered<BoxFuture<'_, ToolResult>> = FuturesOrdered::new();
        for call in batch.calls.clone() {
            // Read the tool's claim about itself *before* the future starts, so
            // the classification cannot change under a concurrent unmount.
            let parallel_safe = toolbox
                .as_ref()
                .and_then(|t| t.get(&call.name))
                .map(|tool| tool.parallel_safe(&call.arguments))
                .unwrap_or(false);
            let gate = gate.clone();
            let permits = permits.clone();
            let cancel = batch.cancel.clone();
            let working_dir = batch.working_dir.clone();
            let ctx = self.ctx.clone();
            let id = call.id.clone();
            let exec = ToolExec {
                call,
                turn: batch.turn,
                round: batch.step,
                pre_approved: false,
            };
            ordered.push_back(Box::pin(async move {
                let _permit = permits.acquire().await.expect("semaphore open");
                let _barrier = if parallel_safe {
                    futures::future::Either::Left(gate.read().await)
                } else {
                    futures::future::Either::Right(gate.write().await)
                };
                // Checked after the barrier, not before: a call that had not
                // started when the turn was cancelled never starts, and never
                // appears in the log as having run.
                if cancel.is_cancelled() {
                    return crate::exec::error_result(&id, "(cancelled before it started)");
                }
                execute_one(&ctx, cancel, working_dir, exec).await
            }));
        }

        let mut out = Vec::with_capacity(batch.calls.len());
        while let Some(result) = ordered.next().await {
            out.push(result);
        }
        out
    }
}

pub struct ParallelToolsPlugin;

#[async_trait]
impl Plugin for ParallelToolsPlugin {
    fn name(&self) -> &'static str {
        "tool-exec-parallel"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "overlap the tool calls that declare themselves safe to overlap"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: Row = if config.is_null() {
            Row::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let _ = ctx.on_waterfall::<ToolsExecuteBatch>(
            Arc::new(Parallel {
                ctx: ctx.clone(),
                max_parallel: row.max_parallel,
            }),
            false,
        );
        Ok(())
    }
}
