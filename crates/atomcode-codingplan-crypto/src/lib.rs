//! Open-source stub for the closed-source AtomCode request-signing
//! crate. The official build pipeline replaces this entire directory
//! with the private crate before invoking
//! `cargo build --features atomcode-core/codingplan-crypto`.
//!
//! In the open-source build the `codingplan-crypto` feature stays off,
//! so this stub is compiled but never linked into `atomcode-core` —
//! `atomcode-core::coding_plan::crypto::UnavailableSigner` handles
//! every signing attempt with a localised "official build required"
//! hint.
//!
//! `sign_v1` and `ALGORITHM_VERSION` are the public API contract: any
//! change here must be mirrored in the private crate (and vice versa)
//! so the stub-swap continues to type-check.

#![deny(unsafe_code)]

/// Algorithm version. Real crate uses `1`; the stub keeps `0` so a
/// stub-on-by-accident build emits an obviously-wrong `X-AtomCode-Alg`
/// value (matching `UnavailableSigner::algorithm_version`).
pub const ALGORITHM_VERSION: u8 = 0;

/// Sign a request and return the HTTP headers the caller must attach
/// to the outbound request. Open-source stub panics — if this runs,
/// somebody enabled the `codingplan-crypto` feature without swapping
/// the directory for the closed-source crate. That is a build-time
/// configuration bug, not a runtime condition.
pub fn sign_v1(
    _method: &str,
    _path: &str,
    _body: &[u8],
    _oauth_token: &str,
    _user_id: &str,
    _timestamp_unix: u64,
    _nonce: &[u8; 16],
    _client_version: &str,
) -> Vec<(&'static str, String)> {
    panic!(
        "atomcode-codingplan-crypto stub invoked: feature \
         `codingplan-crypto` is enabled but the open-source stub is \
         still in place. Replace crates/atomcode-codingplan-crypto \
         with the private crate before building (see private-repo \
         BUILD.md)."
    );
}
