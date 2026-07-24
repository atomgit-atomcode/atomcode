//! One-shot coding agent — the L2 stack running a single task end-to-end against a REAL
//! provider. This is the live smoke (no mock). It AUTO-APPROVES tool calls (no human in
//! the loop), so run it deliberately.
//!
//! ```bash
//! ATOMCODE_API_KEY=sk-... \
//! ATOMCODE_BASE_URL=https://api.deepseek.com/v1 \
//! ATOMCODE_MODEL=deepseek-chat \
//! cargo run -p atomcode-coding --example run_task -- "list the rust files and summarize the crate"
//! ```

use atomcode_coding::{build_coding_agent, CodingAgentConfig};
use atomcode_kernel::agent::AutoRespond;

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

    let agent = match build_coding_agent(CodingAgentConfig::new(api_key, base_url, model, cwd)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("build failed: {e}");
            std::process::exit(1);
        }
    };

    println!("task: {task}\n--- running ---");
    let outcome = agent.run_to_completion(task, AutoRespond::AllowAll).await;
    println!(
        "\n--- outcome ---\nstop: {:?}\ntool calls: {}\n\n{}",
        outcome.stop,
        outcome.tool_results.len(),
        outcome.text
    );
    if let Some(err) = outcome.error {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
