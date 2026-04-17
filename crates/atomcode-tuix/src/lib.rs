pub mod render;
pub mod input;

use anyhow::Result;
use atomcode_core::agent::AgentHandle;
use atomcode_core::config::Config;
use atomcode_core::tool::ToolContext;

pub async fn run(
    _config: Config,
    _model_name: String,
    _agent_handle: AgentHandle,
    _tool_context: ToolContext,
    _working_dir: std::path::PathBuf,
    _session_to_continue: Option<atomcode_core::session::Session>,
) -> Result<()> {
    println!("atomcode-tuix: starting...");
    Ok(())
}
