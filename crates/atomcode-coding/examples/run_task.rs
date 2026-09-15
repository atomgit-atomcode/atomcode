//! One-shot coding agent — the product running a single task end-to-end against
//! a REAL provider. This is the live smoke (no mock). It AUTO-APPROVES tool calls
//! (no human in the loop), so run it deliberately.
//!
//! ```bash
//! ATOMCODE_API_KEY=sk-... \
//! ATOMCODE_BASE_URL=https://api.deepseek.com/v1 \
//! ATOMCODE_MODEL=deepseek-chat \
//! cargo run -p atomcode-coding --example run_task -- "list the rust files and summarize the crate"
//! ```

use atomcode_coding::{
    prepare, CodingAgentConfig, DefaultCodingProviderFactory, PrepareOptions, SessionMode,
};
use atomcode_kernel::event::{AgentCommand, AgentEvent};

#[tokio::main]
async fn main() {
    let task = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let task = if task.trim().is_empty() {
        "List the files in the current directory and briefly describe the project.".to_string()
    } else {
        task
    };

    let Ok(api_key) = std::env::var("ATOMCODE_API_KEY") else {
        eprintln!("Set ATOMCODE_API_KEY (+ optional ATOMCODE_BASE_URL / ATOMCODE_MODEL) to run a live task.");
        std::process::exit(2);
    };
    let base_url = std::env::var("ATOMCODE_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com/v1".to_string());
    let model = std::env::var("ATOMCODE_MODEL").unwrap_or_else(|_| "deepseek-chat".to_string());
    let cwd = std::env::current_dir().expect("cwd");

    let cfg = CodingAgentConfig::new(api_key, base_url, model, cwd);
    // No session on disk, and no MCP: a smoke run should not adopt the machine's
    // state or connect anyone else's processes.
    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        mcp: false,
        ..Default::default()
    };
    let parts = match prepare(&cfg, opts.clone()).await {
        Ok(parts) => parts,
        Err(error) => {
            eprintln!("prepare failed: {error}");
            std::process::exit(1);
        }
    };
    let provider = match atomcode_coding::CodingProviderFactory::build(
        &DefaultCodingProviderFactory::new(concat!("atomcode/", env!("CARGO_PKG_VERSION"))),
        &cfg,
        None,
    ) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("provider failed: {error}");
            std::process::exit(1);
        }
    };
    let mounted = match atomcode_coding::runtime::mount(&parts, &cfg, &opts, provider).await {
        Ok(mounted) => mounted,
        Err(error) => {
            eprintln!("mount failed: {error}");
            std::process::exit(1);
        }
    };
    let mut handle = mounted.handle;

    println!("task: {task}\n--- running ---");
    handle
        .commands
        .send(AgentCommand::SendMessage {
            text: task,
            images: Vec::new(),
        })
        .expect("the agent must accept the task");

    let mut text = String::new();
    let mut tool_calls = 0usize;
    let mut failure = None;
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::TextDelta(delta) => {
                print!("{delta}");
                text.push_str(&delta);
            }
            AgentEvent::ToolResult { .. } => tool_calls += 1,
            // Nobody is watching, so anything it asks is allowed — that is what
            // this example says on the tin.
            AgentEvent::Request { id, .. } => {
                let _ = handle.commands.send(AgentCommand::Respond {
                    id,
                    value: serde_json::json!({ "decision": "allow" }),
                });
            }
            AgentEvent::Error { message, .. } => failure = Some(message),
            AgentEvent::TurnComplete { reason } => {
                println!("\n--- outcome ---\nstop: {reason:?}\ntool calls: {tool_calls}\n\n{text}");
                break;
            }
            _ => {}
        }
    }
    let _ = handle.commands.send(AgentCommand::Shutdown);
    let _ = handle.task.await;
    if let Some(error) = failure {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
