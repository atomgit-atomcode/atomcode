use crate::worktree_status::detail::{inspect, Failure};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde_json::{json, Value};

pub(super) struct GitStatusTool;

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }

    fn description(&self) -> &str {
        "Read the current session's Git repository status as JSON. Reports branch, HEAD, \
         upstream, ahead/behind, and staged, unstaged, untracked and conflicted paths. \
         Paths are repository-relative. Ahead/behind compare local refs without fetching. \
         Null counts mean the comparison is unavailable. A truncated result lists only some \
         changes. Uncommitted changes inside submodules are excluded."
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    fn read_only_hint(&self) -> bool {
        true
    }

    fn self_bounds_output(&self) -> bool {
        true
    }

    async fn execute(&self, _: &str, ctx: &ToolContext) -> ToolResult {
        let result = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return failure("cancelled", false),
            result = inspect(&ctx.working_dir) => result,
        };
        match result {
            Ok(content) => ToolResult {
                content,
                ..ToolResult::default()
            },
            Err(Failure::NotWorktree) => failure("not_a_git_worktree", false),
            Err(Failure::Timeout) => failure("timed_out", false),
            Err(Failure::OutputLimit) => failure("git_output_limit_exceeded", true),
            Err(Failure::GitFailed) => failure("git_query_failed", false),
            Err(Failure::InvalidOutput) => failure("invalid_git_output", false),
        }
    }
}

fn failure(error: &str, truncated: bool) -> ToolResult {
    ToolResult {
        content: json!({"error": error, "truncated": truncated}).to_string(),
        is_error: true,
        ..ToolResult::default()
    }
}
