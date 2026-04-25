//! AtomCode anonymous telemetry (v2: 6-event set).

#![forbid(unsafe_code)]

pub mod config;
pub mod event;
pub mod identity;
pub mod notice;
pub mod queue;
pub mod runtime;
pub mod scrub;
pub mod sender;

pub use config::{CliOverride, ResolvedConfig, TelemetryConfig, TelemetryState};
pub use event::{CodingplanResult, Envelope, Event, Record, RepoHost, RepoOrigin, SessionMode};
pub use runtime::{Counters, CountersSnapshot, CurrentContext, Telemetry};

pub const SCHEMA_VERSION: u32 = 1;
