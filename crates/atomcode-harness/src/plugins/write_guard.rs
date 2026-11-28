//! `write-guard`: `write_file` does not replace a file this conversation has
//! never looked at.
//!
//! `write_file` overwrites whole files. A model that believes a path is free —
//! because a teammate said so, or because it never checked — destroys whatever
//! is there, and an untracked file has nothing to restore it from. Seen in the
//! wild: a team member told "create the file if it does not exist" replaced a
//! 48-line notes file it had never opened.
//!
//! Deliberately narrow, after what others shipped. A read-before-every-mutation
//! policy (`edit` and patches too, plus a changed-since-read check) costs weak
//! models rounds; deepseek-harness benchmarked theirs and ships it off, and
//! opencode removed its own. `edit_file` already has to match text it has seen,
//! so only the whole-file replacement is held, and only for a file this
//! conversation has not touched at all: one successful `read_file`, `edit_file`
//! or `write_file` of the path — a partial read included — lets the write
//! through. What it asks for is knowing the file exists, not having seen all of it.
//!
//! The evidence is the session log, the one authority on what this
//! conversation did: compaction and resume keep it, and each agent — a lead,
//! its members, a `task` child — answers from its own.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde_json::Value;

use crate::events::{ToolExec, ToolsExecute};
use crate::seams::{FsSvc, SessionSvc};
use crate::session::SessionEvent;

/// Tools whose successful call on a path means this conversation knows the file.
const TOUCHES: [&str; 3] = ["read_file", "edit_file", "write_file"];

struct WriteGuard {
    ctx: Context,
}

/// `path` with `.` and `..` folded away, so `./a.md` and `a.md` are one file.
fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn target(arguments: &str, working_dir: &Path) -> Option<PathBuf> {
    let args: Value = serde_json::from_str(arguments).ok()?;
    let raw = args.get("file_path")?.as_str()?;
    Some(normalized(&atomcode_capabilities::tools::resolve_path(
        raw,
        working_dir,
    )))
}

/// Whether this conversation has successfully read, edited or written `path`.
fn touched(events: &[crate::session::LoggedEvent], path: &Path, working_dir: &Path) -> bool {
    let failed: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|logged| match &logged.event {
            SessionEvent::ToolResultLogged {
                call_id,
                is_error: true,
                ..
            } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    events.iter().any(|logged| match &logged.event {
        SessionEvent::ToolStarted { call, .. } => {
            TOUCHES.contains(&call.name.as_str())
                && !failed.contains(call.id.as_str())
                && target(&call.arguments, working_dir).as_deref() == Some(path)
        }
        _ => false,
    })
}

async fn exists(ctx: &Context, path: &Path) -> bool {
    match crate::agent::scoped(ctx).service::<FsSvc>() {
        Some(fs) => fs.info(path).await.is_ok_and(|info| info.exists),
        None => tokio::fs::metadata(path).await.is_ok(),
    }
}

#[async_trait]
impl Waterfall<ToolsExecute> for WriteGuard {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        if exec.call.name != "write_file" {
            return next.run(exec).await;
        }
        let Some(path) = target(&exec.call.arguments, &exec.working_dir) else {
            return next.run(exec).await;
        };
        let Some(session) = crate::agent::scoped(&self.ctx).service::<SessionSvc>() else {
            return next.run(exec).await;
        };
        if !exists(&self.ctx, &path).await
            || session.read_events(|events| touched(events, &path, &exec.working_dir))
        {
            return next.run(exec).await;
        }
        ToolResult {
            call_id: exec.call.id.clone(),
            content: format!(
                "write_file refused: {} already exists and this conversation has not read it, \
                 so writing would replace content you have not seen. Read it with read_file \
                 first (then write it whole if you still mean to), or change part of it with \
                 edit_file. Nothing was written.",
                atomcode_capabilities::pathnorm::to_display(&path)
            ),
            is_error: true,
            images: vec![],
        }
    }
}

pub struct WriteGuardPlugin;

#[async_trait]
impl Plugin for WriteGuardPlugin {
    fn name(&self) -> &'static str {
        "write-guard"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "refuse a write_file over an existing file this conversation has not read, edited or written"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx.on_waterfall::<ToolsExecute>(Arc::new(WriteGuard { ctx: ctx.clone() }), false);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::normalized;
    use std::path::Path;

    #[test]
    fn dot_segments_name_one_file() {
        assert_eq!(normalized(Path::new("/w/./a.md")), Path::new("/w/a.md"));
        assert_eq!(
            normalized(Path::new("/w/sub/../a.md")),
            Path::new("/w/a.md")
        );
    }
}
