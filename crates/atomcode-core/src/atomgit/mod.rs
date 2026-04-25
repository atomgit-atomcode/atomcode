//! AtomGit REST API client, built on top of the OAuth token stored by
//! `crate::auth`. Scope is intentionally narrow: only the endpoints the
//! `fixissue` workflow needs.

pub mod client;
pub mod fixissue;
pub mod models;
pub mod url;

pub use client::Client;
pub use url::IssueRef;

/// Owner of the atomcode upstream repo on atomgit.com. Used by `/issue`
/// to file bug reports / feature requests against atomcode **itself** —
/// not against the user's current working project. Keep in sync with
/// the `repository` field in the workspace `Cargo.toml`.
pub const UPSTREAM_OWNER: &str = "atomgit_atomcode";
/// Name of the atomcode upstream repo. Keep in sync with the
/// `repository` field in the workspace `Cargo.toml`.
pub const UPSTREAM_REPO: &str = "atomcode";
