//! Embedded seeds: rust-best-practices.md, /review command, etc.
//! Packed at build time into setup-seeds.tar.zst, extracted at first run to
//! ~/.atomcode/seeds-cache/<binary-version>/. Subsequent runs hit the cache.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const SEEDS_TARZST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/setup-seeds.tar.zst"));

/// Returns the path to the extracted seeds directory. Extracts on first call.
///
/// Cache key uses a hash of the embedded tar.zst content (not just CARGO_PKG_VERSION)
/// so that rebuilding with updated seeds (same version number) correctly invalidates
/// the cache. This prevents stale seed files from being served after a rebuild.
pub fn ensure_seeds_extracted(cache_root: &Path) -> Result<PathBuf> {
    // Use content hash so same-version rebuilds with new seeds still invalidate.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(SEEDS_TARZST);
    let content_hash = format!("{:x}", h.finalize());
    let short_hash = &content_hash[..12]; // 12 hex chars = 48 bits, plenty for cache key

    let cache_dir = cache_root.join("seeds-cache").join(format!(
        "{}-{}",
        env!("CARGO_PKG_VERSION"),
        short_hash
    ));
    let sentinel = cache_dir.join(".extracted");
    if sentinel.exists() {
        return Ok(cache_dir);
    }
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("create_dir_all({})", cache_dir.display()))?;

    let decoder = zstd::Decoder::new(SEEDS_TARZST).context("zstd decoder for embedded seeds")?;
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(&cache_dir)
        .with_context(|| format!("unpack to {}", cache_dir.display()))?;

    #[cfg(target_os = "macos")]
    remove_quarantine_recursive(&cache_dir);

    std::fs::write(&sentinel, b"").context("write sentinel")?;
    Ok(cache_dir)
}

/// Remove com.apple.quarantine xattr from all extracted files.
/// Some IDEs refuse to read quarantined files.
#[cfg(target_os = "macos")]
fn remove_quarantine_recursive(dir: &std::path::Path) {
    use std::process::Command;
    let _ = Command::new("xattr")
        .args(["-rd", "com.apple.quarantine"])
        .arg(dir)
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_extracts_and_creates_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let extracted = ensure_seeds_extracted(dir.path()).unwrap();
        assert!(extracted.join(".extracted").exists());
        // Placeholder README from build.rs should be present.
        assert!(
            extracted.join("skills").join("README.md").exists()
                || extracted.join("README.md").exists()
                || extracted.read_dir().unwrap().count() > 0
        );
    }

    #[test]
    fn second_call_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure_seeds_extracted(dir.path()).unwrap();
        let second = ensure_seeds_extracted(dir.path()).unwrap();
        assert_eq!(first, second);
    }
}
