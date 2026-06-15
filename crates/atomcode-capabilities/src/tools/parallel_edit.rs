//! `parallel_edit_files` — edit several INDEPENDENT files concurrently, each via its own
//! child agent (subagent-by-composition). The model supplies, per file, a `path` + a
//! natural-language `instruction`, plus a cross-file `contract` (shared invariants)
//! forwarded verbatim to every child. Each child is a fresh kernel [`Agent`] (its own
//! provider + mounted tools) that edits ONLY its assigned file, then stops; the children
//! run in parallel and their per-file statuses are collected into one result.
//!
//! L1 placement: a tool may hold an [`LlmProvider`](atomcode_kernel::provider::LlmProvider)
//! and spawn child agents — same construction-time-injection pattern as the stateful
//! `change_dir`/`todo` tools. The kernel ([`Agent`] + `run_to_completion`) is L0, so this
//! needs nothing above the kernel. Because it carries a provider + a tool factory it is
//! OPT-IN (constructed by the embedder, not part of `register_coding_tools`).
//!
//! Scope vs. the production tool: this ports the dispatch + contract + per-file status
//! surface. It does NOT run the post-edit build probe (cargo/npm/…) — that is a
//! product/L2 concern; the model repairs cross-file gaps from the returned statuses.

use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::event::StopReason;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{MountedTools, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// Default per-child system prompt: a focused single-file editor.
const DEFAULT_PERSONA: &str = "You are a focused file editor working on ONE file as part \
of a parallel batch. Read the file if needed, make exactly the change described in the \
instruction, and honor the cross-file contract. Do not edit any other file. When the \
edit is complete, stop with a one-line summary of what you changed.";

const DEFAULT_MAX_FILES: usize = 12;

/// Edit multiple files in parallel via child agents. Construct with a provider factory
/// (a fresh provider per child — a session consumes its provider) and a tools factory
/// (a fresh `MountedTools` per child — it is not `Clone`); typically mount the L1
/// `read_file`/`edit_file`/`write_file` tools for the children.
pub struct ParallelEditTool {
    make_provider: Box<dyn Fn() -> Arc<dyn LlmProvider> + Send + Sync>,
    make_tools: Box<dyn Fn() -> MountedTools + Send + Sync>,
    persona: String,
    max_files: usize,
}

impl ParallelEditTool {
    pub fn new(
        make_provider: impl Fn() -> Arc<dyn LlmProvider> + Send + Sync + 'static,
        make_tools: impl Fn() -> MountedTools + Send + Sync + 'static,
    ) -> Self {
        Self {
            make_provider: Box::new(make_provider),
            make_tools: Box::new(make_tools),
            persona: DEFAULT_PERSONA.to_string(),
            max_files: DEFAULT_MAX_FILES,
        }
    }
    /// Override the per-child system prompt.
    pub fn with_persona(mut self, persona: impl Into<String>) -> Self {
        self.persona = persona.into();
        self
    }
    /// Override the max number of files per call (default 12).
    pub fn with_max_files(mut self, max: usize) -> Self {
        self.max_files = max.max(2);
        self
    }
}

#[derive(Deserialize)]
struct FileEdit {
    path: String,
    instruction: String,
}

#[derive(Deserialize)]
struct Args {
    files: Vec<FileEdit>,
    #[serde(default)]
    contract: String,
}

#[async_trait]
impl Tool for ParallelEditTool {
    fn name(&self) -> &str {
        "parallel_edit_files"
    }
    fn description(&self) -> &str {
        "Edit multiple INDEPENDENT files in parallel, one child agent per file. Use ONLY \
         when you have 2+ concrete files to edit, each with a clear instruction, the edits \
         don't depend on each other, and any cross-file invariant (shared trait/type/\
         interface) is captured in `contract`. Do NOT use while still exploring, for \
         impl/decl splits that need coordinated edits (use edit_file sequentially), or \
         when you still need to read files first. Each child sees only its file + the \
         contract — changes not in `contract` will be missed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 12,
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "File path (absolute or relative to the working directory)" },
                            "instruction": { "type": "string", "description": "Concrete edit for THIS file: what to add/change/remove and why. The child sees only this + the file + the contract." }
                        },
                        "required": ["path", "instruction"]
                    }
                },
                "contract": { "type": "string", "description": "Cross-file invariants every child must honor (shared traits, signatures, naming). Empty if files are fully independent." }
            },
            "required": ["files"]
        })
    }
    // children call edit_file (itself Risky + gateable); the dispatch itself reads as
    // Risky since it mutates many files. Approval middleware can gate on the name.
    fn risk(&self, _args: &str) -> atomcode_kernel::tool::RiskLevel {
        atomcode_kernel::tool::RiskLevel::Risky
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "parallel_edit_files: invalid arguments: {e}. Expected {{\"files\":[{{\"path\":..,\"instruction\":..}}], \"contract\":\"\"}}."
                ))
            }
        };
        if a.files.len() < 2 {
            return err("parallel_edit_files: provide at least 2 files (use edit_file for a single file).");
        }
        if a.files.len() > self.max_files {
            return err(format!("parallel_edit_files: too many files ({}, max {}).", a.files.len(), self.max_files));
        }
        if let Some(bad) = a.files.iter().find(|f| f.path.trim().is_empty() || f.instruction.trim().is_empty()) {
            let _ = bad;
            return err("parallel_edit_files: every file needs a non-empty `path` and `instruction`.");
        }

        let contract_block = if a.contract.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nCross-file contract (honor exactly):\n{}", a.contract.trim())
        };

        // Spawn one detached child agent per file, concurrently. Detaching via
        // tokio::spawn (then awaiting the JoinHandle) keeps the child's cancel wired to
        // the parent token: if this tool future is dropped on cancel, the still-running
        // child is stopped only by `ctx.cancel.child_token()` cascading in.
        let mut handles = Vec::with_capacity(a.files.len());
        for f in &a.files {
            let task = format!(
                "File to edit: {}\n\nInstruction:\n{}{}\n\nEdit ONLY this file using your tools, then stop.",
                f.path, f.instruction, contract_block
            );
            let child = Agent::builder()
                .provider((self.make_provider)())
                .tools((self.make_tools)())
                .persona(self.persona.clone())
                .working_dir(ctx.working_dir.clone())
                .cancel_token(ctx.cancel.child_token())
                .build();
            let path = f.path.clone();
            handles.push(tokio::spawn(async move {
                let outcome = child.run_to_completion(task, AutoRespond::AllowAll).await;
                (path, outcome)
            }));
        }

        let mut rows = Vec::with_capacity(handles.len());
        let mut any_failed = false;
        for h in handles {
            let (path, outcome) = match h.await {
                Ok(pair) => pair,
                Err(_) => {
                    any_failed = true;
                    rows.push("✗ <unknown>: child task panicked/aborted".to_string());
                    continue;
                }
            };
            if outcome.stop == StopReason::Stopped {
                let summary = outcome.text.lines().next().unwrap_or("").trim();
                let summary = if summary.is_empty() { "(edited)" } else { summary };
                rows.push(format!("✓ {path}: {summary}"));
            } else {
                any_failed = true;
                let reason = outcome.error.unwrap_or_else(|| format!("{:?}", outcome.stop));
                rows.push(format!("✗ {path}: {reason}"));
            }
        }

        let header = format!(
            "parallel_edit_files: {} file(s), {} succeeded, {} failed.",
            rows.len(),
            rows.iter().filter(|r| r.starts_with('✓')).count(),
            rows.iter().filter(|r| r.starts_with('✗')).count(),
        );
        let body = format!("{header}\n{}", rows.join("\n"));
        if any_failed {
            err(body)
        } else {
            ok(body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    use atomcode_kernel::tool::{ProgressSink, ToolDef, ToolRegistry};
    use futures::stream::{self, BoxStream};
    use futures::StreamExt;
    use tokio_util::sync::CancellationToken;

    /// Stateless scripted provider: `Some(reply)` → one text turn then stop;
    /// `None` → a terminal open error (simulates a failed child).
    struct MockProvider {
        reply: Option<String>,
    }
    #[async_trait]
    impl LlmProvider for MockProvider {
        fn model_name(&self) -> &str {
            "mock"
        }
        async fn chat_stream(
            &self,
            _m: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            match &self.reply {
                Some(text) => {
                    let evs = vec![StreamEvent::TextDelta(text.clone()), StreamEvent::Done { truncated: false }];
                    Ok(stream::iter(evs).boxed())
                }
                None => Err(ProviderError { retryable: false, message: "mock open failure".into(), ..Default::default() }),
            }
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            progress: ProgressSink::noop(),
        }
    }

    fn tool(reply: Option<&'static str>) -> ParallelEditTool {
        let reply = reply.map(|s| s.to_string());
        ParallelEditTool::new(
            move || Arc::new(MockProvider { reply: reply.clone() }) as Arc<dyn LlmProvider>,
            || ToolRegistry::new().mount(&[]), // children need no tools for these tests
        )
    }

    #[tokio::test]
    async fn dispatches_one_child_per_file_and_collects_statuses() {
        let t = tool(Some("renamed the symbol"));
        let r = t
            .execute(
                r#"{"files":[{"path":"a.rs","instruction":"do x"},{"path":"b.rs","instruction":"do y"}],"contract":"keep trait T stable"}"#,
                &ctx(),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("2 file(s), 2 succeeded, 0 failed"), "{}", r.content);
        assert!(r.content.contains("✓ a.rs: renamed the symbol"), "{}", r.content);
        assert!(r.content.contains("✓ b.rs: renamed the symbol"), "{}", r.content);
    }

    #[tokio::test]
    async fn fewer_than_two_files_errors() {
        let r = tool(Some("x")).execute(r#"{"files":[{"path":"a.rs","instruction":"y"}]}"#, &ctx()).await;
        assert!(r.is_error);
        assert!(r.content.contains("at least 2 files"), "{}", r.content);
    }

    #[tokio::test]
    async fn too_many_files_errors() {
        let files: Vec<String> = (0..13).map(|i| format!("{{\"path\":\"f{i}.rs\",\"instruction\":\"x\"}}")).collect();
        let args = format!("{{\"files\":[{}]}}", files.join(","));
        let r = tool(Some("x")).execute(&args, &ctx()).await;
        assert!(r.is_error);
        assert!(r.content.contains("too many files"), "{}", r.content);
    }

    #[tokio::test]
    async fn empty_path_or_instruction_errors() {
        let r = tool(Some("x"))
            .execute(r#"{"files":[{"path":"a.rs","instruction":""},{"path":"b.rs","instruction":"y"}]}"#, &ctx())
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("non-empty"), "{}", r.content);
    }

    #[tokio::test]
    async fn child_failure_is_surfaced_and_marks_error() {
        // provider returns None → every child fails its open; the row shows ✗ and the
        // overall result is_error.
        let r = tool(None)
            .execute(r#"{"files":[{"path":"a.rs","instruction":"x"},{"path":"b.rs","instruction":"y"}]}"#, &ctx())
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("0 succeeded, 2 failed"), "{}", r.content);
        assert!(r.content.contains("✗ a.rs:"), "{}", r.content);
    }

    #[test]
    fn risk_is_risky() {
        assert_eq!(tool(Some("x")).risk("{}"), atomcode_kernel::tool::RiskLevel::Risky);
    }
}
