//! Project-trust store for MCP: which project directories the user has
//! explicitly trusted to load project-level (`.mcp.json`) MCP servers.
//! Untrusted projects have their project-source servers withheld from the
//! connect loop so a committed `.mcp.json` cannot auto-spawn a subprocess.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::config::{McpConfigSource, McpServerConfig};

#[derive(Debug, Serialize, Deserialize, Default)]
struct TrustStore {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, TrustEntry>,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Serialize, Deserialize)]
struct TrustEntry {
    /// Absolute project dir — audit/display only; matching is by the map key hash.
    path: String,
}

/// Location of the trust store file. Honors `ATOMCODE_MCP_TRUST_STORE` (test seam).
pub fn trust_store_path() -> PathBuf {
    if let Ok(p) = std::env::var("ATOMCODE_MCP_TRUST_STORE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    atomcode_config::config::Config::config_dir().join("mcp_trust.json")
}

fn load_store() -> TrustStore {
    let path = trust_store_path();
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::debug!("mcp trust store unreadable ({}); treating as empty", e);
            TrustStore::default()
        }),
        Err(_) => TrustStore::default(),
    }
}

fn save_store(store: &TrustStore) -> anyhow::Result<()> {
    let path = trust_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let bytes = serde_json::to_vec_pretty(store)?;
    crate::fs_atomic::atomic_write(&path, &bytes, 0o600)?;
    Ok(())
}

fn key_for(project_dir: &Path) -> String {
    atomcode_config::util::stable_project_hash(project_dir)
}

/// True iff `project_dir` is recorded as trusted.
pub fn is_project_trusted(project_dir: &Path) -> bool {
    load_store().projects.contains_key(&key_for(project_dir))
}

/// Record `project_dir` as trusted (idempotent, atomic).
pub fn trust_project(project_dir: &Path) -> anyhow::Result<()> {
    let mut store = load_store();
    store.version = 1;
    store.projects.insert(
        key_for(project_dir),
        TrustEntry {
            path: project_dir.display().to_string(),
        },
    );
    save_store(&store)
}

/// Remove `project_dir` from the trust store.
///
/// Returns `Ok(true)` if the entry was present and has been removed (and saved).
/// Returns `Ok(false)` if the project was not trusted to begin with (no-op, no save).
pub fn untrust_project(project_dir: &Path) -> anyhow::Result<bool> {
    let mut store = load_store();
    if store.projects.remove(&key_for(project_dir)).is_some() {
        save_store(&store)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Result of splitting configs by trust.
pub struct TrustPartition {
    pub allowed: Vec<McpServerConfig>,
    /// Withheld project-source servers (untrusted project only).
    pub blocked: Vec<McpServerConfig>,
}

/// Split configs: when the project is untrusted, project-source servers are
/// `blocked`; everything else is `allowed`. When trusted, all are `allowed`.
pub fn partition_by_trust(configs: Vec<McpServerConfig>, project_dir: &Path) -> TrustPartition {
    if is_project_trusted(project_dir) {
        return TrustPartition {
            allowed: configs,
            blocked: Vec::new(),
        };
    }
    let (blocked, allowed): (Vec<_>, Vec<_>) = configs
        .into_iter()
        .partition(|c| matches!(c.source, McpConfigSource::Project));
    TrustPartition { allowed, blocked }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use super::super::config::McpTransportConfig;
    use serial_test::serial;

    /// Pin the on-disk project key so trust and session buckets cannot drift.
    #[cfg(unix)]
    #[test]
    fn golden_key_matches_capabilities_mirror() {
        assert_eq!(
            key_for(Path::new("/tmp/atomcode-trust-golden")),
            "8b6a67e0b2c06dae",
            "shared project hash changed and would orphan existing trust/session data"
        );
    }

    // Point the store at a unique temp file for this test process.
    fn with_temp_store(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: tests are single-threaded per module here; env override is the test seam.
        unsafe {
            std::env::set_var("ATOMCODE_MCP_TRUST_STORE", dir.path().join(name));
        }
        dir
    }

    fn cfg(name: &str, source: McpConfigSource) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            disabled: false,
            config: McpTransportConfig::Stdio {
                command: "true".to_string(),
                args: vec![],
                env: Default::default(),
                timeout_ms: None,
            },
            source,
            trust: false,
            auto_approve: vec![],
        }
    }

    #[test]
    #[serial]
    fn untrusted_by_default_then_trust_then_untrust() {
        let _g = with_temp_store("store1.json");
        let proj = Path::new("/tmp/some/project-a");

        assert!(!is_project_trusted(proj), "fresh store: nothing trusted");

        trust_project(proj).unwrap();
        assert!(is_project_trusted(proj), "after trust_project");

        let removed = untrust_project(proj).unwrap();
        assert!(removed, "untrust of trusted project should return true");
        assert!(!is_project_trusted(proj), "after untrust_project");
    }

    #[test]
    #[serial]
    fn corrupt_store_is_fail_closed() {
        let dir = with_temp_store("store2.json");
        std::fs::write(dir.path().join("store2.json"), b"{ not json").unwrap();
        assert!(
            !is_project_trusted(Path::new("/tmp/x")),
            "corrupt store => untrusted"
        );
    }

    #[test]
    #[serial]
    fn untrusted_blocks_project_keeps_user() {
        let _g = with_temp_store("store3.json");
        let proj = Path::new("/tmp/proj-part");
        let configs = vec![
            cfg("evil", McpConfigSource::Project),
            cfg("user-ok", McpConfigSource::User),
        ];
        let part = partition_by_trust(configs, proj);
        assert_eq!(
            part.blocked
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["evil"]
        );
        assert_eq!(
            part.allowed
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["user-ok"]
        );
    }

    #[test]
    #[serial]
    fn untrust_never_trusted_returns_false() {
        let _g = with_temp_store("store_un1.json");
        let proj = Path::new("/tmp/never-trusted");
        assert!(
            !untrust_project(proj).unwrap(),
            "untrust of untrusted project should be false"
        );
    }

    #[test]
    #[serial]
    fn trust_then_untrust_returns_true() {
        let _g = with_temp_store("store_un2.json");
        let proj = Path::new("/tmp/was-trusted");
        trust_project(proj).unwrap();
        assert!(
            untrust_project(proj).unwrap(),
            "untrust of trusted project should be true"
        );
    }

    #[test]
    #[serial]
    fn double_untrust_second_returns_false() {
        let _g = with_temp_store("store_un3.json");
        let proj = Path::new("/tmp/double-untrust");
        trust_project(proj).unwrap();
        assert!(
            untrust_project(proj).unwrap(),
            "first untrust should be true"
        );
        assert!(
            !untrust_project(proj).unwrap(),
            "second untrust should be false"
        );
    }

    #[test]
    #[serial]
    fn trusted_allows_all() {
        let _g = with_temp_store("store4.json");
        let proj = Path::new("/tmp/proj-trusted");
        trust_project(proj).unwrap();
        let configs = vec![
            cfg("p", McpConfigSource::Project),
            cfg("u", McpConfigSource::User),
        ];
        let part = partition_by_trust(configs, proj);
        assert!(part.blocked.is_empty());
        assert_eq!(part.allowed.len(), 2);
    }
}
