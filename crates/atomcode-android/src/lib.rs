//! Android host for the coding agent.
//!
//! # What this crate is
//!
//! The same thing `atomcode webui` does inside the TUI, except the "terminal" is
//! an Android `WebView`: bind a loopback listener, hand it to
//! [`atomcode_daemon::run_server`] together with a webui token store, and give
//! the URL back to Kotlin. The page the WebView loads is the embedded `webui`
//! bundle, so the front end is not ported — it is the existing one.
//!
//! # What this crate is not
//!
//! It is not a second runtime owner and it does not re-implement the agent. The
//! daemon already owns the `CodingRuntime`, the session store and the webui
//! routes; this crate only chooses *where* the product tree lives and *how long*
//! the server lives. See `docs/android-port.md`.
//!
//! # Why the host layer is separate from the JNI layer
//!
//! [`host::AgentServer`] is ordinary Rust with no `jni` in sight, so it can be
//! reasoned about (and, on a desktop checkout, compiled) without an Android
//! toolchain. `jni_api` is the thin translation surface: strings in, URL out.

pub mod host;
/// The bundled `git` and the shell shims that make it reachable.
pub mod shims;

#[cfg(target_os = "android")]
mod jni_api;
