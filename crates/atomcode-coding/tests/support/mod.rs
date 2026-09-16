//! Mounting the product for a test that drives an agent handle directly.
//!
//! The runtime mounts the tree through [`atomcode_coding::runtime::mount`]; so
//! does this, so a test cannot pass against an assembly that does not ship. What
//! it adds is the driving: send one message, answer whatever is asked, and
//! collect the turn — the shape the kernel's `run_to_completion` had, which a
//! handle-driven tree does not come with.

#![allow(dead_code)]

use std::sync::Arc;

use atomcode_coding::{CodingAgentConfig, PrepareOptions};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::ToolResult;

/// A mounted product tree and the handle that drives it.
///
/// The `App` rides along because the tree must outlive the handle: dropping it
/// unloads every row, and the next command reaches a conversation whose services
/// are gone.
pub struct Mounted {
    pub handle: AgentHandle,
    app: atomcode_plexus::App,
}

impl Mounted {
    /// Stop the tree and wait for it to let go.
    ///
    /// Awaiting the driver's task matters: the session lease lives in the rows
    /// and in whatever the pump still holds, so a phase that resumes the same
    /// session right after a `Shutdown` finds it in use unless this has returned.
    pub async fn shutdown(mut self) {
        let _ = self.handle.commands.send(AgentCommand::Shutdown);
        let _ = (&mut self.handle.task).await;
        self.app.stop();
    }

    /// The mounted tree, row by row, in the order it will run — what
    /// `--dump-config` shows. For a criterion about ordering, which is a
    /// product decision the row list is supposed to state out loud.
    pub fn rows(&self) -> Vec<String> {
        self.app
            .tree()
            .dump()
            .lines()
            .filter_map(|l| l.strip_prefix("- "))
            .map(|l| l.trim().to_string())
            .collect()
    }

    /// Every mounted row's id and the config it ended up with.
    ///
    /// The merged result, which is what the rows actually run on — and the only
    /// place a field silently lost to a wholesale `[[patch]]` is visible.
    pub fn row_configs(&self) -> Vec<(String, serde_json::Value)> {
        self.app
            .tree()
            .active()
            .map(|e| (e.id.clone(), e.config.clone()))
            .collect()
    }

    /// Composition findings for the mounted product, told what this host reads
    /// and fills itself.
    pub fn audit(&self) -> Vec<String> {
        self.app
            .audit_with(
                atomcode_harness::seam_map::HOST_CONSUMED,
                atomcode_harness::seam_map::HOST_PROVIDED,
            )
            .iter()
            .filter(|f| f.is_defect())
            .map(|f| f.to_string())
            .collect()
    }

    /// Stop the tree without waiting, for a test that is done with it.
    pub fn stop(self) {
        let mut app = self.app;
        app.stop();
    }
}

/// Prepare the capability graph and mount it, as the runtime does.
pub async fn mount(
    cfg: &CodingAgentConfig,
    opts: PrepareOptions,
    provider: Arc<dyn LlmProvider>,
) -> Mounted {
    let parts = atomcode_coding::prepare(cfg, opts.clone())
        .await
        .expect("prepare");
    mount_parts(&parts, cfg, &opts, provider).await
}

/// Mount an already-prepared graph — for a test that reassembles the same parts,
/// which is what a `/model` swap or a provider reload does.
pub async fn mount_parts(
    parts: &atomcode_coding::CodingParts,
    cfg: &CodingAgentConfig,
    opts: &PrepareOptions,
    provider: Arc<dyn LlmProvider>,
) -> Mounted {
    let mounted = atomcode_coding::runtime::mount(parts, cfg, opts, provider)
        .await
        .expect("the product must mount");
    Mounted {
        handle: mounted.handle,
        app: mounted.app,
    }
}

/// What one turn produced, in the shape the kernel's `Outcome` had.
#[derive(Default, Debug)]
pub struct Turn {
    pub text: String,
    pub tool_results: Vec<ToolResult>,
    pub stop: Option<StopReason>,
    pub error: Option<String>,
    /// Every question put to the person, as `(kind, payload)`.
    pub asked: Vec<(String, serde_json::Value)>,
}

/// Send one message and drive the turn to its end, answering every question with
/// `answer`. `None` refuses, which is what a driver that cannot ask does.
pub async fn turn(handle: &mut AgentHandle, text: &str, answer: Option<serde_json::Value>) -> Turn {
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: text.into(),
            images: Vec::new(),
        })
        .expect("the handle must accept a message");
    let mut out = Turn::default();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "`{text}`: the turn never ended: {out:?}");
        match tokio::time::timeout(left, handle.events.recv()).await {
            Ok(Some(AgentEvent::TextDelta(delta))) => out.text.push_str(&delta),
            Ok(Some(AgentEvent::ToolResult { result })) => out.tool_results.push(result),
            Ok(Some(AgentEvent::Error { message, .. })) => out.error = Some(message),
            Ok(Some(AgentEvent::Request { id, kind, payload })) => {
                out.asked.push((kind, payload));
                let value = answer
                    .clone()
                    .unwrap_or(serde_json::json!({ "decision": "deny" }));
                let _ = handle.commands.send(AgentCommand::Respond { id, value });
            }
            Ok(Some(AgentEvent::TurnComplete { reason })) => {
                out.stop = Some(reason);
                return out;
            }
            Ok(Some(_)) => continue,
            other => panic!("`{text}`: the event stream ended early: {other:?}"),
        }
    }
}

/// Answer every question with "allow", the way an auto-approving driver does.
pub fn allow() -> Option<serde_json::Value> {
    Some(serde_json::json!({ "decision": "allow" }))
}

/// Options that keep `prepare` free of network and home-directory I/O.
pub fn quiet_options() -> PrepareOptions {
    PrepareOptions {
        session: atomcode_coding::SessionMode::Disabled,
        tools: true,
        skill_dirs: Some(Vec::new()),
        plugin_skill_dirs: Vec::new(),
        mcp: false,
        extra_mcp_servers: Vec::new(),
        external_subagents: Vec::new(),
        memory: false,
        web: false,
        review: false,
        subagents: atomcode_coding::SubagentPolicy::Disabled,
        request_user_input: true,
        rate_limit_source: None,
    }
}
