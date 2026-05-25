// atomcode-codingplan-crypto — closed-source request signing for the
// AtomGit LLM gateway.
//
// This crate is intentionally a LEAF: no dependency on atomcode-core,
// no imports of trait types from the public repo. It exports a single
// primitive-typed entry point that returns the HTTP headers the caller
// must attach to the outbound request. All wire-format details — body
// hashing, canonical message construction, hex encoding, header names,
// "v1:" prefix — live entirely inside this crate. The public stub at
// atomcode/crates/atomcode-codingplan-crypto/src/lib.rs exposes only
// the function signature, not the wire format.
//
// Design notes are in
// docs/superpowers/specs/2026-05-18-atomgit-llm-gateway-signing-design.md
// (in the atomcode public repo, but private to maintainers via docs/superpowers/).
//
// P2 work — actual implementations to be filled in:
//   - master.rs: obfuscated master secret bytes (Level 2 protection)
//   - kdf.rs:    HKDF-SHA256 derivation (per-user + per-hour bucketing)
//   - versions/v1.rs: HMAC-SHA256(signing_key, canonical_msg)

#![deny(unsafe_code)]

use sha2::{Digest, Sha256};

pub mod master;
pub mod kdf;
pub mod versions;

/// Algorithm version number this crate considers "current". Embedded
/// in the `X-AtomCode-Alg` header so the server picks the matching
/// verification routine. See spec §7 for rotation.
pub const ALGORITHM_VERSION: u8 = 1;

/// Sign a request and return the five `X-AtomCode-*` HTTP headers
/// ready for attachment to the outbound request.
///
/// Inputs are primitive types so this crate has zero dependency on
/// atomcode-core. The public stub at
/// `atomcode/crates/atomcode-codingplan-crypto/src/lib.rs` exposes the
/// same signature and panics on call.
///
/// Parameter contract:
///
/// - `method`         — HTTP method, uppercase ASCII (e.g. "POST")
/// - `path`           — URL path, e.g. "/v1/chat/completions"
/// - `body`           — the exact bytes that will go on the wire
/// - `oauth_token`    — bearer token from auth.toml (digested into HKDF salt)
/// - `user_id`        — auth.toml user.id; pinned in HKDF salt
/// - `timestamp_unix` — request timestamp, seconds since epoch; bucket = ts / 3600
/// - `nonce`          — 16 bytes of crypto-rng per request
/// - `client_version` — the AtomCode binary semver (env!("CARGO_PKG_VERSION") at
///                      the call site); bound into HKDF salt + emitted as
///                      `X-AtomCode-Ver` in the returned headers
///
/// Returns a `Vec<(&'static str, String)>` of (header-name, value)
/// pairs. The caller passes each pair to its HTTP client unmodified.
pub fn sign_v1(
    method: &str,
    path: &str,
    body: &[u8],
    oauth_token: &str,
    user_id: &str,
    timestamp_unix: u64,
    nonce: &[u8; 16],
    client_version: &str,
) -> Vec<(&'static str, String)> {
    let body_hash: [u8; 32] = Sha256::digest(body).into();
    let sig = versions::v1::sign(
        method,
        path,
        &body_hash,
        oauth_token,
        user_id,
        timestamp_unix,
        nonce,
        client_version,
    );

    vec![
        ("X-AtomCode-Sig", format!("v1:{}", hex_encode(&sig))),
        ("X-AtomCode-Ts", timestamp_unix.to_string()),
        ("X-AtomCode-Nonce", hex_encode(nonce)),
        ("X-AtomCode-Alg", ALGORITHM_VERSION.to_string()),
        ("X-AtomCode-Ver", client_version.to_string()),
    ]
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same inputs produce identical headers — including a stable sig.
    /// Pins the deterministic-HMAC property end-to-end through the
    /// public entry point.
    #[test]
    fn sign_v1_is_deterministic() {
        let body = b"{\"hello\":\"world\"}";
        let nonce = [0x11u8; 16];
        let h1 = sign_v1("POST", "/v1/chat/completions", body, "tok", "user-1", 1_700_000_000, &nonce, "4.23.1");
        let h2 = sign_v1("POST", "/v1/chat/completions", body, "tok", "user-1", 1_700_000_000, &nonce, "4.23.1");
        assert_eq!(h1, h2);
    }

    /// All five expected header names are emitted, in a stable order.
    /// The order itself isn't load-bearing for the server, but a
    /// regression here would surprise downstream code that relies on
    /// positional access.
    #[test]
    fn sign_v1_emits_five_named_headers() {
        let body = b"{}";
        let nonce = [0u8; 16];
        let h = sign_v1("POST", "/v1/chat/completions", body, "tok", "user-1", 1, &nonce, "4.23.1");
        let names: Vec<&str> = h.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "X-AtomCode-Sig",
                "X-AtomCode-Ts",
                "X-AtomCode-Nonce",
                "X-AtomCode-Alg",
                "X-AtomCode-Ver",
            ]
        );
    }

    /// The `X-AtomCode-Sig` value uses the `v1:<hex>` shape the server
    /// verification middleware splits on. A regression on the prefix
    /// would make every request 401 the moment it goes live.
    #[test]
    fn sign_v1_sig_header_has_v1_hex_format() {
        let body = b"{}";
        let nonce = [0u8; 16];
        let h = sign_v1("POST", "/v1/chat/completions", body, "tok", "user-1", 1, &nonce, "4.23.1");
        let sig = h.iter().find(|(n, _)| *n == "X-AtomCode-Sig").unwrap().1.as_str();
        assert!(sig.starts_with("v1:"), "expected v1: prefix, got {sig}");
        let hex = &sig[3..];
        assert_eq!(hex.len(), 64, "expected 64 hex chars, got {}", hex.len());
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit() && (b.is_ascii_digit() || b.is_ascii_lowercase())),
                "expected lowercase hex, got {hex}");
    }

    /// Changing the body changes the signature — the body_sha256 path
    /// is reached and matters. (Detailed primitive-level coverage lives
    /// in versions::v1::tests.)
    #[test]
    fn sign_v1_changes_with_body() {
        let nonce = [0u8; 16];
        let h_a = sign_v1("POST", "/v1/chat/completions", b"{\"a\":1}", "tok", "user-1", 1, &nonce, "4.23.1");
        let h_b = sign_v1("POST", "/v1/chat/completions", b"{\"b\":2}", "tok", "user-1", 1, &nonce, "4.23.1");
        let sig_a = h_a.iter().find(|(n, _)| *n == "X-AtomCode-Sig").unwrap().1.as_str();
        let sig_b = h_b.iter().find(|(n, _)| *n == "X-AtomCode-Sig").unwrap().1.as_str();
        assert_ne!(sig_a, sig_b);
    }

    #[test]
    fn hex_encode_lowercase_pairs() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0xab]), "00ffab");
    }
}
