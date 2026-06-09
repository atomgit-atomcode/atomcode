//! # atomcode-review (L2)
//!
//! The REVIEW specialization. Assembles the neutral kernel ([`atomcode_kernel`]) +
//! capabilities ([`atomcode_capabilities`]) into a runnable, READ-ONLY code-review agent
//! that reports structured findings — with ZERO `atomcode-core` involvement.
//!
//! L2 owns:
//! 1. **Assembly** — [`build_review_agent`]: wires provider + the read-only review toolset
//!    (read/grep/glob/ast_grep/codeintel/web_search) + the `report_finding` sink + the
//!    reviewer persona into a kernel [`Agent`](atomcode_kernel::agent::Agent). It returns
//!    a [`ReportFindingTool`] HANDLE so the caller collects findings after the run.
//! 2. **Persona** — [`persona::review_persona`]: the reviewer system prompt.
//!
//! The DIFF to review is injected as the task by the caller (e.g. `atomcode-clix`), so the
//! agent needs no shell to obtain it and stays strictly read-only.
//!
//! ```no_run
//! # async fn demo() -> Result<(), String> {
//! use atomcode_review::{build_review_agent, ReviewAgentConfig};
//! use atomcode_kernel::agent::AutoRespond;
//!
//! let (agent, report) = build_review_agent(ReviewAgentConfig::new(
//!     "sk-...", "https://api.deepseek.com/v1", "deepseek-v4", ".",
//! ))?;
//! let _ = agent.run_to_completion("Review this diff:\n<diff>", AutoRespond::AllowAll).await;
//! for f in report.findings() {
//!     println!("[{} {:.2}] {}:{}-{}  {}", f.priority, f.confidence, f.file_path, f.line_start, f.line_end, f.title);
//! }
//! # Ok(()) }
//! ```

pub mod config;
pub mod persona;

mod assemble;

pub use assemble::{build_review_agent, build_review_agent_with};
pub use config::ReviewAgentConfig;
pub use persona::review_persona;

/// Re-exported so a driver (CLI) can read findings without depending on
/// `atomcode-capabilities` directly.
pub use atomcode_capabilities::tools::{Finding, ReportFindingTool};
