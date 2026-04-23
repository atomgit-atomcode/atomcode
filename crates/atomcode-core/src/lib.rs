pub mod agent;
pub mod atomgit;
pub mod auth;
pub mod coding_plan;
pub mod config;
pub mod conversation;
pub mod ctx;
pub mod graph;
pub mod input_history;
pub mod notify;
pub mod project_context;
pub mod provider;
pub mod semantic;
pub mod session;
pub mod skill;
pub mod stream;
pub mod tool;
pub mod turn;
pub mod self_update;
pub mod version_check;

/// User-Agent identifier for every outbound HTTP request the app makes.
///
/// AtomGit's API gateway filters requests by User-Agent; missing the
/// `AtomCode` token gets the request rejected at the edge (LLM calls,
/// OAuth token exchange, issue REST, self-update downloads — every
/// atomgit.com endpoint). Routing all HTTP clients through this single
/// constant means adding the token (or future changes like appending an
/// install-id) happens in one place, not five.
pub const ATOMCODE_USER_AGENT: &str = concat!("AtomCode/", env!("CARGO_PKG_VERSION"));
