//! Build a minimal `AgentLoop` for replay tests.
//!
//! Uses `Conversation::new()`, an explicit `ToolContext` pointing at a
//! `TempDir`, a `Config` with datalog disabled (we don't want test runs
//! writing to real filesystem locations), and whatever provider / tool
//! registry the caller gives us.

use std::collections::HashMap;
use std::path::Path;

use atomcode_core::agent::{AgentHandle, AgentLoop};
use atomcode_core::config::{Config, DatalogConfig};
use atomcode_core::conversation::Conversation;
use atomcode_core::provider::LlmProvider;
use atomcode_core::tool::{ToolContext, ToolRegistry};

/// Build a replay-ready AgentLoop. The `working_dir` should be a TempDir
/// so any `.atomcode/` / `datalog/` side effects land somewhere harmless.
pub fn build(
    provider: Box<dyn LlmProvider>,
    tool_registry: ToolRegistry,
    working_dir: &Path,
) -> (AgentLoop, AgentHandle) {
    let config = Config {
        default_provider: "replay-fixture".to_string(),
        default_workdir: Some(working_dir.to_string_lossy().to_string()),
        providers: HashMap::new(), // provider is injected directly, no lookup needed
        datalog: DatalogConfig { enabled: false, dir: None },
        auto_update: false,
    };
    let tool_context = ToolContext::new(working_dir.to_path_buf());
    let conversation = Conversation::new();

    AgentLoop::new(config, provider, tool_registry, tool_context, conversation)
}
