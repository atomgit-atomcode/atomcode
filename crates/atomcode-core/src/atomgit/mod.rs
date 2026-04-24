//! AtomGit REST API client, built on top of the OAuth token stored by
//! `crate::auth`. Scope is intentionally narrow: only the endpoints the
//! `fixissue` workflow needs.

pub mod client;
pub mod fixissue;
pub mod models;
pub mod url;

pub use client::Client;
pub use url::IssueRef;
