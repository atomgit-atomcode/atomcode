// Redirect ATOMCODE_HOME to a throwaway temp dir before any test runs, so tests
// that persist sessions/config don't pollute the developer's real ~/.atomcode.
// The `#[ctor]` lives HERE (referencing the shared fn) so the linker keeps it.
#[cfg(test)]
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

// What remains of `atomcode-core`: the reqwest-applying proxy runtime policy
// (can't live in the leaf `atomcode-config`, which stays reqwest-free) and the
// `ctrace!` file-sink macro. Everything else (auth, config/i18n, conversation,
// provider, tool, skill, plugin, lsp, …) was retired into leaf/L1 crates. The
// `ATOMCODE_USER_AGENT` constant now lives in `atomcode-auth`.
pub mod proxy;
