//! After a successful `git push` to a gitcode.com/atomgit.com repo, ensure the
//! repo carries the `atomcode` project label. Best-effort: every failure is a
//! `tracing::warn` and the turn proceeds. See design 2026-07-21.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_kernel::middleware::{AfterOutcome, BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall, ToolResult};

use super::remote::{detect_push_target, PushTarget};
use super::{AtomgitClient, AtomgitConfig, StaticTokenProvider};

/// True when the bash args' `command` is a `git push` invocation.
fn is_git_push(arguments: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return false;
    };
    let Some(cmd) = v.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    let mut it = cmd.split_whitespace();
    matches!((it.next(), it.next()), (Some("git"), Some("push")))
}

pub struct GitPushLabelMiddleware {
    working_dir: PathBuf,
    pending: Arc<Mutex<HashSet<String>>>, // call_ids of in-flight git push
    ensured: Arc<Mutex<HashSet<String>>>, // "owner/repo@base" already labelled
}

impl GitPushLabelMiddleware {
    pub fn new(working_dir: PathBuf) -> Self {
        Self {
            working_dir,
            pending: Arc::default(),
            ensured: Arc::default(),
        }
    }

    async fn ensure(&self, t: PushTarget) {
        let key = format!("{}/{}@{}", t.owner, t.repo, t.base_url);
        if self.ensured.lock().unwrap().contains(&key) {
            return;
        }
        // Fetch the token OFF the async runtime (blocking auth I/O).
        let token =
            match tokio::task::spawn_blocking(atomcode_auth::oauth::get_valid_token).await {
                Ok(Ok(tok)) => tok,
                Ok(Err(e)) => {
                    tracing::warn!("atomcode-label: no token: {e:#}");
                    return;
                }
                Err(e) => {
                    tracing::warn!("atomcode-label: token task failed: {e}");
                    return;
                }
            };
        let client = match AtomgitClient::new(AtomgitConfig {
            base_url: t.base_url.to_string(),
            user_agent: format!("atomcode/{}", env!("CARGO_PKG_VERSION")),
            token: Arc::new(StaticTokenProvider(token)),
        }) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("atomcode-label: client build failed: {e}");
                return;
            }
        };
        match client
            .repo_ensure_label(&t.owner, &t.repo, "atomcode")
            .await
        {
            Ok(added) => {
                tracing::info!(
                    "atomcode-label: {}/{} ({})",
                    t.owner,
                    t.repo,
                    if added { "added" } else { "already present" }
                );
                self.ensured.lock().unwrap().insert(key);
            }
            Err(e) => tracing::warn!(
                "atomcode-label: ensure failed for {}/{}: {e}",
                t.owner,
                t.repo
            ),
        }
    }
}

#[async_trait]
impl ToolMiddleware for GitPushLabelMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if tool.name() == "bash" && is_git_push(&call.arguments) {
            self.pending.lock().unwrap().insert(call.id.clone());
        }
        BeforeOutcome::Proceed
    }

    async fn after(&self, result: &mut ToolResult) -> AfterOutcome {
        let was_push = self.pending.lock().unwrap().remove(&result.call_id);
        if was_push && !result.is_error {
            if let Some(target) = detect_push_target(&self.working_dir) {
                self.ensure(target).await; // best-effort
            }
        }
        AfterOutcome::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_git_push_commands() {
        assert!(is_git_push(r#"{"command":"git push"}"#));
        assert!(is_git_push(r#"{"command":"git push origin main"}"#));
        assert!(is_git_push(r#"{"command":"  git   push --force"}"#));
        assert!(!is_git_push(r#"{"command":"git pull"}"#));
        assert!(!is_git_push(r#"{"command":"echo git push"}"#));
        assert!(!is_git_push(r#"{"command":"git status"}"#));
        assert!(!is_git_push(r#"not json"#));
    }
}
