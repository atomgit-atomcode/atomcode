//! Background one-shot task (`/bg`): run a task to completion on a fresh, lean coding
//! agent (no session / mcp / memory / review), auto-approving every tool, and report what
//! it did. Intermediate events stay internal — the caller surfaces only the
//! [`BackgroundOutcome`].
//!
//! Relocated out of the bridge so any driver can spawn a background task once it goes
//! native (the bridge-elimination roadmap). The PROVIDER is injected (the caller builds
//! it — provider construction + signing lives above coding), and the legacy
//! `BackgroundComplete` EVENT emission stays with the caller; this owns only the
//! agent-running.

use std::sync::Arc;

use atomcode_capabilities::tools::{ApprovalResponse, APPROVAL_KIND};
use atomcode_kernel::event::{AgentCommand as KCmd, AgentEvent as KEv, StopReason};
use atomcode_kernel::provider::LlmProvider;

use crate::config::CodingAgentConfig;
use crate::parts::{assemble, prepare, PrepareOptions, SessionMode};

/// What a background run reports back: the assistant's text, the files it touched
/// (write/edit tool paths), the LLM round count, and whether it ended cleanly.
#[derive(Debug, Clone)]
pub struct BackgroundOutcome {
    pub summary: String,
    pub files_edited: Vec<String>,
    pub turns: usize,
    pub success: bool,
}

/// Run `task` to completion on a fresh one-shot agent over `provider`, auto-approving
/// tools, and return what it did. A setup failure (prepare/assemble) is reported as a
/// `success: false` outcome rather than a panic.
pub async fn run(
    cfg: &CodingAgentConfig,
    provider: Arc<dyn LlmProvider>,
    task: String,
) -> BackgroundOutcome {
    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        skill_dirs: None,
        mcp: false,
        memory: false,
        web: true,
        // A background one-shot stays lean — no nested review sub-agent.
        review: false,
        disabled_tools: Vec::new(),
    };
    let built = async {
        let mut parts = prepare(cfg, opts).await.map_err(|e| e.to_string())?;
        assemble(&mut parts, cfg, provider)
            .map(|a| a.spawn())
            .map_err(|e| e.to_string())
    }
    .await;
    let mut handle = match built {
        Ok(h) => h,
        Err(e) => {
            return BackgroundOutcome {
                summary: format!("background setup failed: {e}"),
                files_edited: vec![],
                turns: 0,
                success: false,
            }
        }
    };

    let _ = handle.commands.send(KCmd::SendMessage { text: task, images: vec![] });
    let mut summary = String::new();
    let mut files: Vec<String> = Vec::new();
    let mut turns = 0usize;
    let mut success = true;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            KEv::TextDelta(t) => summary.push_str(&t),
            KEv::Request { id, kind, .. } if kind == APPROVAL_KIND => {
                let value = serde_json::to_value(ApprovalResponse::allow())
                    .unwrap_or(serde_json::Value::Null);
                let _ = handle.commands.send(KCmd::Respond { id, value });
            }
            KEv::Request { id, .. } => {
                let _ = handle.commands.send(KCmd::Respond { id, value: serde_json::Value::Null });
            }
            KEv::ToolStarted { call } => {
                if matches!(call.name.as_str(), "write_file" | "edit_file" | "parallel_edit_files") {
                    for p in extract_paths(&call.name, &call.arguments) {
                        if !files.contains(&p) {
                            files.push(p);
                        }
                    }
                }
            }
            KEv::Usage(_) => turns += 1,
            KEv::Error { .. } => success = false,
            KEv::TurnComplete { reason } => {
                success = success && matches!(reason, StopReason::Stopped);
                break;
            }
            _ => {}
        }
    }
    let _ = handle.commands.send(KCmd::Shutdown);
    let _ = handle.task.await;
    let summary = if summary.trim().is_empty() {
        "(background task finished with no text output)".to_string()
    } else {
        summary
    };
    BackgroundOutcome { summary, files_edited: files, turns, success }
}

/// Pull the file path(s) a write/edit tool touched out of its JSON arguments.
/// `write_file`/`edit_file` carry a single top-level `file_path`; `parallel_edit_files`
/// carries a `files` array of `{ path, instruction }` entries.
fn extract_paths(tool: &str, args: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return Vec::new();
    };
    match tool {
        "parallel_edit_files" => v
            .get("files")
            .and_then(|f| f.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("path").and_then(|p| p.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        _ => v
            .get("file_path")
            .and_then(|p| p.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_paths_reads_file_path_for_single_file_tools() {
        // write_file / edit_file carry a top-level `file_path` (NOT `path`).
        assert_eq!(
            extract_paths("write_file", r#"{"file_path":"src/x.rs","content":"hi"}"#),
            vec!["src/x.rs".to_string()]
        );
        assert_eq!(
            extract_paths("edit_file", r#"{"file_path":"a.rs","old_string":"a","new_string":"b"}"#),
            vec!["a.rs".to_string()]
        );
    }

    #[test]
    fn extract_paths_reads_nested_files_for_parallel_edit() {
        // parallel_edit_files carries a `files` array of `{ path, instruction }`.
        assert_eq!(
            extract_paths(
                "parallel_edit_files",
                r#"{"files":[{"path":"a.rs","instruction":"x"},{"path":"b.rs","instruction":"y"}]}"#,
            ),
            vec!["a.rs".to_string(), "b.rs".to_string()]
        );
    }

    #[test]
    fn extract_paths_empty_on_missing_or_malformed() {
        assert!(extract_paths("write_file", r#"{"no_path":1}"#).is_empty());
        assert!(extract_paths("write_file", "not json").is_empty());
        assert!(extract_paths("parallel_edit_files", r#"{"files":[]}"#).is_empty());
    }
}
