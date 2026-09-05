//! # atomcode-plexus — an everything-is-a-plugin runtime
//!
//! A Rust take on [cordis](https://github.com/cordiverse/cordis), the framework
//! under DeepSeek Harness. The claim it makes is narrow and load-bearing:
//!
//! > There is no privileged core to patch. Every part of the product — the model
//! > adapter, the tool registry, the approval policy, the agent loop itself — is a
//! > plugin mounted next to the others, and every registration it makes is a
//! > reversible effect.
//!
//! ## The five ideas
//!
//! | cordis | here |
//! |---|---|
//! | a plugin is an object with `inject` + `apply(ctx)` | [`Plugin`] |
//! | the context is a container of services (`ctx.tools`) | [`Context::provide`] / [`Context::service`], keyed by a marker type ([`ServiceKey`]) |
//! | dependencies are declared, not ordered | [`Plugin::inject`], resolved to a fixed point by [`App::start`] |
//! | typed events in five dispatch modes | [`Event`] + [`Mode`] |
//! | registration is a reversible effect | [`Disposable`], filed with the owning [fiber](fiber) |
//!
//! ## What differs, and why
//!
//! **Slots are keyed by type, not by string.** TypeScript recovers `ctx.fs`'s type
//! through declaration merging; Rust has no such thing, so a service is addressed
//! by a marker type that carries its face. Consumers get compile-time types
//! instead of a runtime cast, and the string name survives for config rows,
//! `inject`, and diagnostics — the places strings belong.
//!
//! **Plugins link at build time.** Rust cannot load a crate at runtime, so the
//! catalog of *available* plugins is fixed when you compile. What stays fully
//! dynamic is the part that carries the value: which plugins run, in what
//! configuration, with which providers filling which seams — all decided by a
//! config tree the user can patch, and all replaceable while the process runs
//! ([`App::patch`]). Out-of-process extension (MCP today, wasm later) is itself
//! just a plugin that bridges into the same seams.
//!
//! ## Shape of a system
//!
//! ```ignore
//! let mut registry = PluginRegistry::new();
//! registry.register(Arc::new(LlmSeam));            // defines ctx.llm
//! registry.register(Arc::new(OpenAiCompatAdapter)); // fills it
//! registry.register(Arc::new(AgentLoop));          // consumes it
//!
//! let tree = ConfigTree::from_layers([base_bundle()?, user_patch()?])?;
//! let mut app = App::new(registry, tree);
//! app.start().await?;
//! ```

pub mod app;
pub mod context;
pub mod error;
pub mod event;
pub mod fiber;
pub mod loader;
pub mod plugin;
pub mod realm;
pub mod service;

pub use app::{App, AuditFinding};
pub use context::Context;
pub use error::{PlexusError, Result};
pub use event::{Event, Listener, Mode, Next, Waterfall};
pub use fiber::{Disposable, FiberId, FiberStatus};
pub use loader::{ConfigTree, Entry, Layer, Op};
pub use plugin::{Plugin, PluginRegistry};
pub use realm::{RealmId, ROOT_REALM};
pub use service::{SeamMode, ServiceKey};

/// Declare a service slot: a marker type, the face consumers receive, and the
/// two facts a reader needs — is it replaceable, and what is it.
///
/// ```ignore
/// plexus_service!(Llm => dyn LlmProvider, "llm", Seam, "LLM adapter registry");
/// plexus_service!(Tools => ToolBox, "tools", Core, "The live tool catalog");
/// ```
///
/// `Seam` / `Core` / `Bundle` are [`SeamMode`] variants. Requiring them here is
/// deliberate: the classification belongs next to the interface, not in a
/// separate document that goes stale.
#[macro_export]
macro_rules! plexus_service {
    ($(#[$meta:meta])* $key:ident => $face:ty, $name:literal, $mode:ident, $title:literal) => {
        $(#[$meta])*
        pub struct $key;
        impl $crate::ServiceKey for $key {
            const NAME: &'static str = $name;
            const MODE: $crate::SeamMode = $crate::SeamMode::$mode;
            const TITLE: &'static str = $title;
            type Face = $face;
        }
    };
}

/// Declare a typed event.
///
/// ```ignore
/// plexus_event!(PreToolCall, "tools/pre-execute", Waterfall, ToolCall => Decision);
/// ```
#[macro_export]
macro_rules! plexus_event {
    ($(#[$meta:meta])* $ev:ident, $name:literal, $mode:ident, $args:ty => $out:ty) => {
        $(#[$meta])*
        pub struct $ev;
        impl $crate::Event for $ev {
            const NAME: &'static str = $name;
            const MODE: $crate::Mode = $crate::Mode::$mode;
            type Args = $args;
            type Output = $out;
        }
    };
    ($(#[$meta:meta])* $ev:ident, $name:literal, $mode:ident, $args:ty) => {
        $crate::plexus_event!($(#[$meta])* $ev, $name, $mode, $args => ());
    };
}
