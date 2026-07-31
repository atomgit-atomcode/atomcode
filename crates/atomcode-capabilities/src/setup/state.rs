//! setup-state.json — per-project sentinel + accepted/declined log.
//! Follows sync_marker.rs philosophy: failed parse / unknown future version → None, no error.

use crate::setup::fs_atomic::atomic_write;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const STATE_FILENAME: &str = "setup-state.json";
pub const STATE_DIR: &str = ".atomcode";
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecIdRef {
    pub kind: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupState {
    pub schema_version: u32,
    pub signals_hash: String,
    pub completed_at: DateTime<Utc>,
    pub atomcode_version: String,
    pub accepted: Vec<RecIdRef>,
}

pub fn state_path(project_root: &Path) -> PathBuf {
    project_root.join(STATE_DIR).join(STATE_FILENAME)
}

pub fn load_setup_state(project_root: &Path) -> Option<SetupState> {
    let path = state_path(project_root);
    let raw = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    match v.get("schema_version").and_then(|x| x.as_u64()) {
        Some(n) if n as u32 == CURRENT_SCHEMA_VERSION => serde_json::from_value(v).ok(),
        // Future or unknown — treat as absent.
        _ => None,
    }
}

pub fn save_setup_state(project_root: &Path, state: &SetupState) -> anyhow::Result<()> {
    let path = state_path(project_root);
    let json = serde_json::to_vec_pretty(state)?;
    atomic_write(&path, &json, 0o644)
}

/// SHA256 of canonical-normalized content of marker files. CRLF→LF, strip BOM.
/// Order: sort marker paths lexicographically before hashing.
pub fn compute_signals_hash(marker_paths: &[PathBuf]) -> String {
    let mut sorted: Vec<&PathBuf> = marker_paths.iter().collect();
    sorted.sort();
    let mut h = Sha256::new();
    for path in sorted {
        let bytes = std::fs::read(path).unwrap_or_default();
        let normalized = normalize_text(&bytes);
        h.update(path.to_string_lossy().as_bytes());
        h.update(b"\0");
        h.update(&normalized);
        h.update(b"\0");
    }
    format!("sha256:{:x}", h.finalize())
}

/// Canonicalize text bytes for hashing: strip UTF-8 BOM, CRLF→LF.
fn normalize_text(bytes: &[u8]) -> Vec<u8> {
    let trimmed = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    };
    let mut out = Vec::with_capacity(trimmed.len());
    let mut i = 0;
    while i < trimmed.len() {
        if i + 1 < trimmed.len() && trimmed[i] == b'\r' && trimmed[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(trimmed[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_none_when_state_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_setup_state(dir.path()).is_none());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let state = SetupState {
            schema_version: CURRENT_SCHEMA_VERSION,
            signals_hash: "sha256:abc".into(),
            completed_at: Utc::now(),
            atomcode_version: "test".into(),
            accepted: vec![RecIdRef {
                kind: "skill".into(),
                slug: "rust-best-practices".into(),
            }],
        };
        save_setup_state(dir.path(), &state).unwrap();
        let loaded = load_setup_state(dir.path()).expect("loaded");
        assert_eq!(loaded.signals_hash, "sha256:abc");
        assert_eq!(loaded.accepted.len(), 1);
    }

    #[test]
    fn load_returns_none_for_future_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomcode")).unwrap();
        let path = state_path(dir.path());
        std::fs::write(&path, r#"{"schema_version": 999, "junk": "future"}"#).unwrap();
        // Per sync_marker philosophy: future version → None, no error.
        assert!(load_setup_state(dir.path()).is_none());
    }

    #[test]
    fn load_returns_none_for_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomcode")).unwrap();
        std::fs::write(&state_path(dir.path()), "{not json").unwrap();
        assert!(load_setup_state(dir.path()).is_none());
    }

    #[test]
    fn signals_hash_normalizes_crlf_to_lf() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("marker.txt");
        std::fs::write(&f, b"line1\nline2\n").unwrap();
        let lf_hash = compute_signals_hash(&[f.clone()]);
        std::fs::write(&f, b"line1\r\nline2\r\n").unwrap();
        let crlf_hash = compute_signals_hash(&[f]);
        assert_eq!(lf_hash, crlf_hash);
    }

    #[test]
    fn signals_hash_strips_bom() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("marker.txt");
        std::fs::write(&f, b"hello").unwrap();
        let nobom_hash = compute_signals_hash(&[f.clone()]);
        std::fs::write(&f, b"\xEF\xBB\xBFhello").unwrap();
        let bom_hash = compute_signals_hash(&[f]);
        assert_eq!(nobom_hash, bom_hash);
    }
}
