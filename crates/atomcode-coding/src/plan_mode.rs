//! Plan mode — read-only exploration, no edits.
//!
//! v1 exposed `/plan` (and `SetPlanMode`): the agent explores and presents a plan
//! WITHOUT mutating anything. This re-implements the ENFORCEMENT on the new stack as
//! a [`ToolMiddleware`] that, while active, blocks every `Risky` tool (the kernel's
//! own risk metadata already marks the mutating ones: write/edit/bash). Read-only
//! tools (read_file, grep, list_*, symbols, web, …) stay available.
//!
//! The flag is an `Arc<AtomicBool>` so the driver can toggle it live (the bridge maps
//! `SetPlanMode` onto it) without a respawn — like the shared cwd handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::middleware::ToolMiddleware;
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall};

/// Blocks mutating (`Risky`) tools while plan mode is active. Share the same
/// `Arc<AtomicBool>` with the driver to toggle it live.
pub struct PlanModeGate {
    active: Arc<AtomicBool>,
}

impl PlanModeGate {
    pub fn new(active: Arc<AtomicBool>) -> Self {
        Self { active }
    }
}

#[async_trait]
impl ToolMiddleware for PlanModeGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> Result<(), String> {
        if self.active.load(Ordering::Relaxed) && tool.risk(&call.arguments) == RiskLevel::Risky {
            return Err(format!(
                "plan mode is active — `{}` would modify the workspace and is blocked. Only \
                 read-only tools are allowed: explore and present a plan for the user to approve \
                 before making changes.",
                call.name
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::testkit::{EchoTool, RiskyWriteTool};

    fn rt() -> RequestCtx {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        RequestCtx::new(tx, None)
    }

    #[tokio::test]
    async fn blocks_risky_only_when_active() {
        let flag = Arc::new(AtomicBool::new(false));
        let gate = PlanModeGate::new(flag.clone());
        let risky: Arc<dyn Tool> = Arc::new(RiskyWriteTool); // always Risky
        let safe: Arc<dyn Tool> = Arc::new(EchoTool); // Safe
        let mut call =
            ToolCall { id: "c".into(), name: "risky_write".into(), arguments: "{}".into() };

        // Inactive: nothing blocked.
        assert!(gate.before(&mut call, &risky, &rt()).await.is_ok());

        // Active: Risky blocked, Safe allowed.
        flag.store(true, Ordering::Relaxed);
        assert!(gate.before(&mut call, &risky, &rt()).await.is_err());
        let mut safe_call = ToolCall { id: "c".into(), name: "echo".into(), arguments: "{}".into() };
        assert!(gate.before(&mut safe_call, &safe, &rt()).await.is_ok());
    }
}
