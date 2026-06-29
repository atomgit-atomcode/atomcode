pub mod agent;
pub mod atomgit;
pub mod auth;
pub mod process_utils;
pub mod coding_plan;
pub mod config;
pub mod conversation;
pub mod ctx;
pub mod graph;
pub mod hook;
pub mod i18n;
pub mod live;
pub mod locale;
pub mod lsp;
pub mod mcp;
pub mod notify;
pub mod proxy;
pub mod provider;
pub mod self_update;
pub mod semantic;
pub mod session;
pub mod plugin;
pub mod setup;
pub mod skill;
pub mod stream;
pub mod telemetry_bootstrap;
pub mod tool;
pub mod trace;
pub mod turn;
pub mod vision_preprocessor;

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
