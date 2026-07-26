//! Fail-closed guard for raw AtomGit API calls issued through the generic bash tool.

use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall};
use serde::Deserialize;
use std::sync::Arc;

const ATOMGIT_API_HOST: &str = "api.atomgit.com";
const TOKEN_MARKERS: &[&str] = &[
    "access_token=",
    "authorization: bearer",
    "$atomgit_token",
    "${atomgit_token",
    "$atomgit_access_token",
    "${atomgit_access_token",
];

#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

/// Reject raw AtomGit API access through `bash`.
///
/// Typed AtomGit tools keep credentials outside model-visible arguments and attach
/// action-aware risk. The bash path has neither property, so it is not a supported fallback.
#[derive(Default)]
pub struct AtomgitBashGate;

impl AtomgitBashGate {
    pub fn new() -> Self {
        Self
    }
}

fn raw_atomgit_api_reason(command: &str) -> Option<&'static str> {
    let normalized = command.to_ascii_lowercase();
    if TOKEN_MARKERS
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        return Some("credentials must not be passed through shell arguments; use an AtomGit tool");
    }
    if normalized.contains(ATOMGIT_API_HOST) {
        return Some(
            "raw AtomGit API calls through bash are disabled; use atomgit_repo, atomgit_pr, or atomgit_issue",
        );
    }
    None
}

#[async_trait]
impl ToolMiddleware for AtomgitBashGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if tool.name() != "bash" {
            return BeforeOutcome::Proceed;
        }
        let Ok(args) = serde_json::from_str::<BashArgs>(&call.arguments) else {
            return BeforeOutcome::Proceed;
        };
        match raw_atomgit_api_reason(&args.command) {
            Some(reason) => BeforeOutcome::deny(reason),
            None => BeforeOutcome::Proceed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::BashTool;
    use atomcode_kernel::event::AgentEvent;
    use tokio::sync::mpsc::unbounded_channel;

    async fn outcome(command: &str) -> BeforeOutcome {
        let gate = AtomgitBashGate::new();
        let (events, _rx) = unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(events, None);
        let tool: Arc<dyn Tool> = Arc::new(BashTool);
        let mut call = ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": command }).to_string(),
        };
        gate.before(&mut call, &tool, &rt).await
    }

    #[tokio::test]
    async fn rejects_raw_atomgit_api_calls() {
        assert!(
            outcome("curl -X POST https://api.atomgit.com/api/v5/user/repos")
                .await
                .is_deny()
        );
        assert!(outcome("curl https://API.ATOMGIT.COM/api/v5/user")
            .await
            .is_deny());
    }

    #[tokio::test]
    async fn rejects_credentials_in_shell_arguments() {
        assert!(outcome("curl https://example.test?access_token=secret")
            .await
            .is_deny());
        assert!(
            outcome("curl -H 'Authorization: Bearer secret' https://example.test")
                .await
                .is_deny()
        );
        assert!(outcome("echo $ATOMGIT_TOKEN").await.is_deny());
        assert_eq!(
            outcome("rg ATOMGIT_TOKEN crates/").await,
            BeforeOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn leaves_unrelated_shell_commands_unchanged() {
        assert_eq!(
            outcome("curl -X POST https://example.test/api").await,
            BeforeOutcome::Proceed
        );
        assert_eq!(outcome("git status").await, BeforeOutcome::Proceed);
    }
}
