//! AtomCode anonymous telemetry (v2: 6-event set).

#![forbid(unsafe_code)]

pub mod config;
pub mod event;
pub mod identity;
pub mod notice;
pub mod pending_invite;
pub mod queue;
pub mod runtime;
pub mod scrub;
pub mod sender;

pub use config::{CliOverride, ResolvedConfig, TelemetryConfig, TelemetryState};
pub use event::{
    CodingplanErrorKind, CodingplanResult, Envelope, Event, LlmErrorKind, McpErrorKind,
    McpTransport, Record, RepoHost, RepoOrigin, SessionMode, ToolErrorKind, UseCommandErrorKind,
};
pub use runtime::{
    resolve_provider_host, Counters, CountersSnapshot, CurrentContext, Telemetry, TelemetryError,
};

pub const SCHEMA_VERSION: u32 = 1;
