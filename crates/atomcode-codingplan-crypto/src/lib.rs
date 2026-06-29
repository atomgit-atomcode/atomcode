//! Open-source placeholder. The real signing crate is overlaid by the
//! official build (scripts/build-official.sh); this stub exists only so the
//! public workspace compiles and carries no signing logic.

#![deny(unsafe_code)]

/// Sentinel only. `0` = "unavailable" (real algorithms start at 1). Never
/// read in an open-source build — the `codingplan-crypto` feature is off,
/// so this crate is not linked.
pub const ALGORITHM_VERSION: u8 = 0;

/// Placeholder matching the real crate's signature so `atomcode-core`
/// type-checks. Unreachable in open-source builds; the official overlay
/// replaces this file wholesale.
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
    unreachable!("request signing requires the official build")
}
