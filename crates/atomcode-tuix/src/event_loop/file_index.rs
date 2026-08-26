// crates/atomcode-tuix/src/event_loop/file_index.rs
//
// `@`-mention infrastructure moved to `atomcode-capabilities` so the daemon
// `/fs/search` endpoint (webui `@`-mention picker) shares ONE walk engine with
// the TUI popup — same gitignore filtering, caps, and cross-level substring
// matching. This shim re-exports it so every `crate::event_loop::file_index::…`
// call site in the TUI keeps working unchanged.

pub use atomcode_capabilities::file_index::*;
