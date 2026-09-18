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

/// True when the bash args' `command` runs a `git push` — including a `git push` buried in a
/// compound `cd … && git add … && git push` chain or prefixed with a `GIT_SSH_COMMAND=…` env var,
/// which weak models emit constantly. Delegates the quote-/compound-aware parsing to the shared
/// bash command scanner so this stays in lockstep with the destructive-fs gate.
fn is_git_push(arguments: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return false;
    };
    let Some(cmd) = v.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    crate::tools::bash_workspace_gate::command_invokes_git_subcommand(cmd, "push")
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
        if self
            .ensured
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&key)
        {
            return;
        }
        // Fetch the token OFF the async runtime (blocking auth I/O).
        let token = match tokio::task::spawn_blocking(atomcode_auth::oauth::get_valid_token).await {
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
                self.ensured
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(key);
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
        if crate::tools::is_command_shell_tool(tool.name()) {
            let matched = is_git_push(&call.arguments);
            if matched {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(call.id.clone());
            }
            // DEBUG (not default `info`): shows whether the git-push detector fired for each
            // bash call, so a "label never set" report can be traced to detection vs. push
            // failure vs. non-atomgit remote with `RUST_LOG=debug`.
            tracing::debug!("atomcode-label: bash call is_git_push={matched}");
        }
        BeforeOutcome::Proceed
    }

    async fn after(
        &self,
        result: &mut ToolResult,
        _tool: Option<&std::sync::Arc<dyn atomcode_kernel::tool::Tool>>,
    ) -> AfterOutcome {
        let was_push = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&result.call_id);
        if !was_push {
            return AfterOutcome::Proceed; // not a git push we're tracking
        }
        if result.is_error {
            tracing::debug!("atomcode-label: git push tool reported error; not labelling");
            return AfterOutcome::Proceed;
        }
        // Log the decision at INFO so a successful push always leaves a trace, whether or not it
        // ends up labelling — otherwise a non-atomgit remote (the common no-op) is invisible.
        match detect_push_target(&self.working_dir) {
            Some(target) => {
                tracing::info!(
                    "atomcode-label: git push succeeded for {}/{}; ensuring label",
                    target.owner,
                    target.repo
                );
                self.ensure(target).await; // best-effort
            }
            None => {
                tracing::info!(
                    "atomcode-label: git push succeeded but origin at {} is not gitcode/atomgit; skipping",
                    self.working_dir.display()
                );
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

    #[test]
    fn detects_git_push_inside_compound_commands() {
        // Weak models routinely chain `cd && add && commit && push` into ONE bash call, and
        // prefix the push with a `GIT_SSH_COMMAND=...` env var. Both must still be recognized.
        assert!(is_git_push(
            r#"{"command":"cd ~/repo && git add -A && git commit -m x && git push origin main"}"#
        ));
        assert!(is_git_push(
            r#"{"command":"GIT_SSH_COMMAND=\"ssh -i ~/.ssh/id_rsa -o StrictHostKeyChecking=accept-new\" git push origin main"}"#
        ));
        assert!(is_git_push(
            r#"{"command":"cd ~/repo && GIT_SSH_COMMAND=\"ssh -i k\" git push origin main 2>&1 | tail -4"}"#
        ));
        // git global option (`-c key=val`) before the subcommand.
        assert!(is_git_push(
            r#"{"command":"git -c http.sslVerify=false push"}"#
        ));
    }

    #[test]
    fn rejects_non_push_compound_and_quoted_push() {
        // Compound with add + commit but NO push must not fire.
        assert!(!is_git_push(
            r#"{"command":"cd ~/repo && git add -A && git commit -m msg"}"#
        ));
        // `git push` living inside a quoted argument (e.g. a commit message) is not a push.
        assert!(!is_git_push(
            r#"{"command":"git commit -m \"remember to git push later\""}"#
        ));
        // `echo` inside a compound is still not a push.
        assert!(!is_git_push(r#"{"command":"cd ~/repo && echo git push"}"#));
    }
}
