// crates/atomcode-core/src/coding_plan/mod.rs
//
// CodingPlan integration: claim the free plan, fetch the eligible model
// list, set up matching provider entries, and pull plan status. Drives
// both `atomcode codingplan` (CLI subcommand) and `/codingplan` (TUI
// slash command); they share the orchestrator in `setup` and differ only
// in how the returned `SetupReport` is rendered.
//
// All three REST endpoints live on `api.gitcode.com` (distinct host from
// `atomgit.com` where the OAuth flow runs; same backend — the token
// obtained from atomgit OAuth authenticates both). The base URL is
// configurable via `ATOMCODE_CODINGPLAN_API_BASE`. The shared UA
// (`ATOMCODE_USER_AGENT`) is honoured by every request so AtomGit's
// API gateway sees a consistent `atomcode/<ver>` identifier.

pub mod client;
pub mod setup;
pub mod sync_marker;
pub mod types;
pub mod usage;

// Gateway request-signing (is_atomgit_gateway / RequestSigner / SignInput / …) lives in
// `atomcode_auth::gateway_crypto` now — this crate is the CodingPlan REST/usage/setup business
// layer only. Consumers that need signing import from `atomcode_auth::gateway_crypto` directly.

pub use client::{api_base_url, is_auth_expired, AuthExpired, Client};
pub use setup::{merge_successful_config, run, SetupReport, StepResult};
pub use sync_marker::{read_last_sync, write_last_sync_now};
pub use types::{ClaimResponse, ModelEntry, PlanInfo, PlanType, StatusResponse, UsageInfo};
