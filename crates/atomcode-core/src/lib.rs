// Redirect ATOMCODE_HOME to a throwaway temp dir before any test runs, so tests
// that persist sessions/config don't pollute the developer's real ~/.atomcode.
// The `#[ctor]` lives HERE (referencing the shared fn) so the linker keeps it.
#[cfg(test)]
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

// `auth` (OAuth login + secure chmod-600 token file) fully lives in the leaf `atomcode-auth`
// crate now; re-export it so core's own `crate::auth::…` uses and external
// `atomcode_core::auth` consumers keep working during the transition.
pub use atomcode_auth as auth;
pub mod process_utils;
// `config` fully lives in the leaf `atomcode-config` crate now; core code (and its
// tests) use `atomcode_config::config` directly, so the transitional re-export shim
// is gone.
// `i18n` fully lives in the leaf `atomcode-config` crate now (it only needs
// `locale::Locale`); re-export it so core's own `crate::i18n::…` uses and any
// external `atomcode_core::i18n` consumers keep working during the transition.
pub use atomcode_config::i18n;
// `locale` fully lives in `atomcode-config` now (re-export shim removed).
mod fs_atomic;
pub mod lsp;
pub mod plugin;
pub mod proxy;
pub mod skill;
pub mod skill_render;
pub mod trace;

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
