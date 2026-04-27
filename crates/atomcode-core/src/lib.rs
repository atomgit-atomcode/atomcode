pub mod agent;
pub mod atomgit;
pub mod auth;
pub mod coding_plan;
pub mod config;
pub mod conversation;
pub mod ctx;
pub mod git;
pub mod graph;
pub mod init;
pub mod input_history;
pub mod mcp;
pub mod notify;
pub mod provider;
pub mod self_update;
pub mod semantic;
pub mod session;
pub mod skill;
pub mod stream;
pub mod telemetry_bootstrap;
pub mod tool;
pub mod turn;
pub mod version_check;

/// User-Agent identifier for every outbound HTTP request the app makes.
///
/// Lowercase `atomcode/<version>` is deliberate. The LLM gateway at
/// `api-ai.gitcode.com` has a UA filter that silently hijacks any
/// request whose UA starts with capital-A `AtomCode` and replies
/// with a 200 + single SSE chunk containing the literal string
/// "参数错误", no `[DONE]` frame — which surfaces in the TUI as a
/// 4-token assistant reply rather than an error. The other
/// atomgit.com / gitcode.com endpoints (CodingPlan, user REST,
/// self-update) accept either case, so normalising to lowercase
/// avoids the LLM-path hijack without breaking the rest. Revisit
/// once the gateway filter is removed upstream.
pub const ATOMCODE_USER_AGENT: &str = concat!("atomcode/", env!("CARGO_PKG_VERSION"));
