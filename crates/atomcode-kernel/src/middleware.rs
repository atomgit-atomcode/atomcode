use crate::request::RequestCtx;
use crate::tool::{Tool, ToolCall};
use async_trait::async_trait;
use std::sync::Arc;

/// Around-tool middleware. Runs before a tool executes; may inspect the call and
/// optionally round-trip to the driver (via `RequestCtx::request`) before deciding
/// whether execution proceeds. The kernel knows nothing of approval — risk-gating,
/// sandboxing, logging, rate-limiting are all just middlewares a specialization
/// installs.
#[async_trait]
pub trait ToolMiddleware: Send + Sync {
    /// `Ok(())` → proceed. `Err(reason)` → block (tool not run).
    async fn before(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> Result<(), String>;
}
