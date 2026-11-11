//! Project-trust store for MCP: which project directories the user has
//! explicitly trusted to load project-level (`.mcp.json`) MCP servers.
//! Untrusted projects have their project-source servers withheld from the
//! connect loop so a committed `.mcp.json` cannot auto-spawn a subprocess.
//!
//! Ported from `atomcode-core`'s `mcp::trust` (L1 cannot depend on core). Reads
//! and writes the SAME `mcp_trust.json` file, using the SAME project key
//! ([`super::registry::project_trust_key`], the store-consistent mirror of core's
//! `session::hash_path`), so core and capabilities agree on trust state at runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::config::{McpConfigSource, McpServerConfig};
use super::registry::project_trust_key;

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

/// Location of the trust store file: `<user tree>/mcp_trust.json`.
pub fn trust_store_path(user_dir: &Path) -> PathBuf {
    user_dir.join("mcp_trust.json")
}

fn load_store(user_dir: &Path) -> TrustStore {
    let path = trust_store_path(user_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::debug!("mcp trust store unreadable ({}); treating as empty", e);
            TrustStore::default()
        }),
        Err(_) => TrustStore::default(),
    }
}

fn save_store(user_dir: &Path, store: &TrustStore) -> anyhow::Result<()> {
    let path = trust_store_path(user_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let bytes = serde_json::to_vec_pretty(store)?;
    crate::fs::atomic_write(&path, &bytes, 0o600)?;
    Ok(())
}

/// What untrusting a project that was never trusted answers with. A constant
/// so a front end can recognise it and say it in its own words.
pub const PROJECT_NOT_TRUSTED: &str = "this project is not trusted";

/// True iff `project_dir` is recorded as trusted in `user_dir`'s store.
pub fn is_project_trusted(project_dir: &Path, user_dir: &Path) -> bool {
    load_store(user_dir)
        .projects
        .contains_key(&project_trust_key(project_dir))
}

/// Record `project_dir` as trusted (idempotent, atomic).
pub fn trust_project(project_dir: &Path, user_dir: &Path) -> anyhow::Result<()> {
    let mut store = load_store(user_dir);
    store.version = 1;
    store.projects.insert(
        project_trust_key(project_dir),
        TrustEntry {
            path: project_dir.display().to_string(),
        },
    );
    save_store(user_dir, &store)
}

/// Remove `project_dir` from the trust store.
///
/// Returns `Ok(true)` if the entry was present and has been removed (and saved).
/// Returns `Ok(false)` if the project was not trusted to begin with (no-op, no save).
pub fn untrust_project(project_dir: &Path, user_dir: &Path) -> anyhow::Result<bool> {
    let mut store = load_store(user_dir);
    if store
        .projects
        .remove(&project_trust_key(project_dir))
        .is_some()
    {
        save_store(user_dir, &store)?;
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
pub fn partition_by_trust(
    configs: Vec<McpServerConfig>,
    project_dir: &Path,
    user_dir: &Path,
) -> TrustPartition {
    if is_project_trusted(project_dir, user_dir) {
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
    use super::super::config::McpTransportConfig;
    use super::*;
    use std::path::Path;

    // A fresh user tree per test: its `mcp_trust.json` is the store.
    fn with_temp_store(_name: &str) -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
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
            allowed_tools: None,
        }
    }

    #[test]
    fn untrusted_by_default_then_trust_then_untrust() {
        let g = with_temp_store("store1.json");
        let proj = Path::new("/tmp/some/project-a");
        assert!(
            !is_project_trusted(proj, g.path()),
            "fresh store: nothing trusted"
        );
        trust_project(proj, g.path()).unwrap();
        assert!(is_project_trusted(proj, g.path()), "after trust_project");
        let removed = untrust_project(proj, g.path()).unwrap();
        assert!(removed, "untrust of trusted project should return true");
        assert!(!is_project_trusted(proj, g.path()), "after untrust_project");
    }

    #[test]
    fn corrupt_store_is_fail_closed() {
        let g = with_temp_store("store2.json");
        std::fs::write(trust_store_path(g.path()), b"{ not json").unwrap();
        assert!(
            !is_project_trusted(Path::new("/tmp/x"), g.path()),
            "corrupt store => untrusted"
        );
    }

    #[test]
    fn untrusted_blocks_project_keeps_user() {
        let g = with_temp_store("store3.json");
        let proj = Path::new("/tmp/proj-part");
        let configs = vec![
            cfg("evil", McpConfigSource::Project),
            cfg("user-ok", McpConfigSource::User),
        ];
        let part = partition_by_trust(configs, proj, g.path());
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
    fn double_untrust_second_returns_false() {
        let g = with_temp_store("store_un3.json");
        let proj = Path::new("/tmp/double-untrust");
        trust_project(proj, g.path()).unwrap();
        assert!(
            untrust_project(proj, g.path()).unwrap(),
            "first untrust should be true"
        );
        assert!(
            !untrust_project(proj, g.path()).unwrap(),
            "second untrust should be false"
        );
    }

    #[test]
    fn trusted_allows_all() {
        let g = with_temp_store("store4.json");
        let proj = Path::new("/tmp/proj-trusted");
        trust_project(proj, g.path()).unwrap();
        let configs = vec![
            cfg("p", McpConfigSource::Project),
            cfg("u", McpConfigSource::User),
        ];
        let part = partition_by_trust(configs, proj, g.path());
        assert!(part.blocked.is_empty());
        assert_eq!(part.allowed.len(), 2);
    }
}
