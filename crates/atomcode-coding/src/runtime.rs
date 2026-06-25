//! A driver-agnostic, kernel-native coding-agent runtime: owns the assembled
//! [`CodingParts`] + the spawned [`AgentHandle`], and knows how to (re)spawn the
//! agent. Drivers (clix, tuix, daemon, the headless CLI) share this instead of
//! each re-implementing `prepare → assemble → spawn` — the seam the bridge proved
//! and `atomcode-clix` open-coded.
//!
//! Provider construction is INJECTED (a [`ProviderFactory`]) so this crate stays
//! free of the driver/core coupling real provider construction carries (e.g. the
//! AtomGit gateway request signing, which needs auth identity + the closed crate).

use std::io;
use std::sync::Arc;

use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::provider::LlmProvider;

use crate::cc_hooks::HookConfig;
use crate::config::CodingAgentConfig;
use crate::parts::{assemble, prepare_with_plugin_hooks, CodingParts, PrepareOptions, SessionMode};

/// Builds the provider from the current config. Stored so a respawn (model swap)
/// can rebuild it against a mutated config. Returns a `String` error the driver
/// maps from its own error type — keeps provider construction (and its core/auth
/// coupling) on the driver side of the seam.
pub type ProviderFactory =
    Box<dyn Fn(&CodingAgentConfig) -> Result<Arc<dyn LlmProvider>, String> + Send>;

/// The assembled coding agent + everything a respawn must reuse: the spawned
/// kernel [`AgentHandle`], the [`CodingParts`] the driver observes, and the config
/// / options / plugin hooks / provider factory needed to rebuild on respawn.
pub struct CodingRuntime {
    cfg: CodingAgentConfig,
    opts_template: PrepareOptions,
    plugin_cc_hooks: Vec<HookConfig>,
    provider_factory: ProviderFactory,
    parts: CodingParts,
    handle: AgentHandle,
}

impl CodingRuntime {
    /// Prepare → build provider (via the factory) → assemble → spawn. Returns once
    /// the kernel agent task is running; the command channel buffers anything sent
    /// before the first turn.
    pub async fn spawn(
        cfg: CodingAgentConfig,
        opts: PrepareOptions,
        plugin_cc_hooks: Vec<HookConfig>,
        provider_factory: ProviderFactory,
    ) -> io::Result<Self> {
        let mut parts =
            prepare_with_plugin_hooks(&cfg, opts.clone(), plugin_cc_hooks.clone()).await?;
        let provider =
            (provider_factory)(&cfg).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let handle = assemble(&mut parts, &cfg, provider)?.spawn();
        Ok(Self {
            cfg,
            opts_template: opts,
            plugin_cc_hooks,
            provider_factory,
            parts,
            handle,
        })
    }

    /// The live kernel agent handle: send `AgentCommand`s, receive `AgentEvent`s.
    pub fn handle(&mut self) -> &mut AgentHandle {
        &mut self.handle
    }

    /// The assembled parts (approval grant store, plan-mode flag, session binding,
    /// MCP registry, …) the driver observes.
    pub fn parts(&self) -> &CodingParts {
        &self.parts
    }

    /// Tear the kernel agent down and rebuild it for `session` (model swap / reload
    /// / resume / new session), REUSING the approval grant store and the plan-mode
    /// flag so allow-always grants and plan mode survive the respawn.
    pub async fn respawn(&mut self, session: SessionMode) -> io::Result<()> {
        // Tear down the old kernel agent and await its task.
        let _ = self
            .handle
            .commands
            .send(atomcode_kernel::event::AgentCommand::Shutdown);
        let old_task = std::mem::replace(&mut self.handle.task, tokio::spawn(async {}));
        let _ = old_task.await;

        let mut opts = self.opts_template.clone();
        opts.session = session;
        let mut parts =
            prepare_with_plugin_hooks(&self.cfg, opts, self.plugin_cc_hooks.clone()).await?;

        // Grants + plan mode survive a respawn (the bridge's C1 contract): carry the
        // SAME shared handles over so allow-always grants and plan mode persist.
        parts.approval = self.parts.approval.clone();
        parts.plan_mode = self.parts.plan_mode.clone();

        let provider = (self.provider_factory)(&self.cfg)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let handle = assemble(&mut parts, &self.cfg, provider)?.spawn();

        self.parts = parts;
        self.handle = handle;
        Ok(())
    }

    /// Shut the kernel agent down and await its task.
    pub async fn shutdown(self) {
        let _ = self
            .handle
            .commands
            .send(atomcode_kernel::event::AgentCommand::Shutdown);
        let _ = self.handle.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parts::SessionMode;
    use atomcode_kernel::event::{AgentCommand, AgentEvent};
    use atomcode_kernel::stream::StreamEvent;
    use atomcode_kernel::testkit::MockProvider;

    /// The runtime assembles a real coding agent over the injected provider and
    /// drives a turn end-to-end: a `SendMessage` streams the provider's `TextDelta`
    /// back out and the turn completes. This is the exact seam clix/tuix/daemon share.
    #[tokio::test]
    async fn spawn_assembles_and_drives_a_turn() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![vec![
            StreamEvent::TextDelta("hello".into()),
            StreamEvent::Done { truncated: false },
        ]]));

        let tmp = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("", "http://mock", "mock-model", tmp.path());
        let opts = PrepareOptions {
            session: SessionMode::Disabled,
            skill_dirs: Some(vec![]), // hermetic: no real skill-dir scan
            mcp: false,
            memory: false,
            web: false,
            review: false,
            disabled_tools: Vec::new(),
        };

        let factory: ProviderFactory = Box::new(move |_cfg| Ok(provider.clone()));
        let mut rt = CodingRuntime::spawn(cfg, opts, Vec::new(), factory)
            .await
            .expect("spawn");

        rt.handle()
            .commands
            .send(AgentCommand::SendMessage { text: "hi".into(), images: vec![] })
            .expect("send");

        let mut streamed = String::new();
        let mut completed = false;
        while let Some(ev) = rt.handle().events.recv().await {
            match ev {
                AgentEvent::TextDelta(t) => streamed.push_str(&t),
                AgentEvent::TurnComplete { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }

        assert_eq!(streamed, "hello", "provider text streamed through the runtime");
        assert!(completed, "the turn completed");

        rt.shutdown().await;
    }

    /// A respawn tears down and rebuilds the kernel agent, but the approval grant
    /// store and the plan-mode flag are carried over as the SAME shared handles —
    /// so allow-always grants and plan mode survive (the bridge's respawn contract).
    #[tokio::test]
    async fn respawn_preserves_approval_and_plan_mode() {
        use std::sync::atomic::Ordering;

        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![]));
        let tmp = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("", "http://mock", "mock-model", tmp.path());
        let opts = PrepareOptions {
            session: SessionMode::Disabled,
            skill_dirs: Some(vec![]),
            mcp: false,
            memory: false,
            web: false,
            review: false,
            disabled_tools: Vec::new(),
        };
        let factory: ProviderFactory = Box::new(move |_cfg| Ok(provider.clone()));
        let mut rt = CodingRuntime::spawn(cfg, opts, Vec::new(), factory).await.expect("spawn");

        let approval_before = Arc::as_ptr(&rt.parts().approval);
        let plan_before = Arc::as_ptr(&rt.parts().plan_mode);
        rt.parts().plan_mode.store(true, Ordering::SeqCst);

        rt.respawn(SessionMode::Disabled).await.expect("respawn");

        assert_eq!(
            Arc::as_ptr(&rt.parts().approval),
            approval_before,
            "approval grant store survives respawn"
        );
        assert_eq!(
            Arc::as_ptr(&rt.parts().plan_mode),
            plan_before,
            "plan-mode flag handle survives respawn"
        );
        assert!(
            rt.parts().plan_mode.load(Ordering::SeqCst),
            "plan-mode value survives respawn"
        );

        rt.shutdown().await;
    }
}
