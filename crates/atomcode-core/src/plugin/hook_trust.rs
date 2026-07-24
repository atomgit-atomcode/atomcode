//! Per-plugin content-hash trust for plugin-shipped hooks. A plugin's hooks run
//! only after the user trusts the CURRENT hash of its hook set (`atomcode plugin
//! trust <name>`). Changing a hook command changes the hash → re-trust required,
//! which blocks a benign-at-install plugin from silently adding hooks in an update.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use super::loader::PluginCcHook;

pub type TrustMap = BTreeMap<String, String>;

/// Stable identity for an installed plugin: `<plugin>@<marketplace>`.
pub fn plugin_id(plugin: &str, marketplace: &str) -> String {
    format!("{plugin}@{marketplace}")
}

/// SHA-256 over the sorted `(event, matcher, command)` triples — sensitive to
/// WHAT CODE RUNS, not to timeout or install path. Each field is length-prefixed
/// so no field content (including control bytes) can shift a boundary and collide
/// two different hook sets. Empty set → SHA-256 of empty input.
pub fn plugin_hook_set_hash(hooks: &[PluginCcHook]) -> String {
    let mut triples: Vec<(&str, &str, &str)> = hooks
        .iter()
        .map(|h| {
            (
                h.event.as_str(),
                h.matcher.as_deref().unwrap_or(""),
                h.command.as_str(),
            )
        })
        .collect();
    triples.sort_unstable();
    let mut hasher = Sha256::new();
    for (event, matcher, command) in &triples {
        for field in [event, matcher, command] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
}

fn trust_store_path() -> Option<PathBuf> {
    Some(super::paths::plugins_root()?.join("hook_trust.json"))
}

/// Load the trust map. Missing/unreadable/malformed → empty (⇒ nothing trusted,
/// the safe default).
pub fn load_trust() -> TrustMap {
    let Some(path) = trust_store_path() else {
        return TrustMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return TrustMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn is_trusted(map: &TrustMap, plugin_id: &str, hash: &str) -> bool {
    map.get(plugin_id).map(|h| h == hash).unwrap_or(false)
}

fn save_trust(map: &TrustMap) -> Result<()> {
    let path = trust_store_path().context("plugin home not configured")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_vec_pretty(map).context("serialize trust map")?;
    crate::fs_atomic::atomic_write(&path, &json, 0o600).context("write trust store")
}

pub fn trust(plugin_id: &str, hash: &str) -> Result<()> {
    let mut map = load_trust();
    map.insert(plugin_id.to_string(), hash.to_string());
    save_trust(&map)
}

pub fn untrust(plugin_id: &str) -> Result<()> {
    let mut map = load_trust();
    map.remove(plugin_id);
    save_trust(&map)
}

fn migration_marker_path() -> Option<PathBuf> {
    Some(super::paths::plugins_root()?.join(".hook_trust_migrated"))
}

/// One-time upgrade migration. The FIRST time this runs in a given home, trust
/// the CURRENT hook-set hash of every already-installed plugin — so upgrading
/// users whose plugin hooks auto-ran before the trust gate existed keep working.
/// After the marker is written, new installs / changed hooks require explicit
/// `plugin trust`. Idempotent (marker-guarded). Best-effort: IO errors are
/// swallowed (worst case a plugin stays untrusted and the user re-trusts).
pub fn ensure_migrated() {
    let Some(marker) = migration_marker_path() else {
        return;
    };
    if marker.exists() {
        return;
    }
    for s in super::loader::installed_plugin_hook_trust_status() {
        if !s.trusted {
            let _ = trust(&s.plugin_id, &s.hash);
        }
    }
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::write(&marker, b"1");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::loader::PluginCcHook;
    use std::path::PathBuf;

    fn mk(event: &str, matcher: Option<&str>, command: &str) -> PluginCcHook {
        PluginCcHook {
            event: event.into(),
            matcher: matcher.map(|s| s.into()),
            command: command.into(),
            timeout_secs: None,
            plugin_root: PathBuf::from("/x"),
        }
    }

    #[test]
    fn hash_is_stable_and_order_independent() {
        let a = plugin_hook_set_hash(&[mk("SessionStart", None, "x"), mk("Stop", None, "y")]);
        let b = plugin_hook_set_hash(&[mk("Stop", None, "y"), mk("SessionStart", None, "x")]);
        assert_eq!(a, b);
    }

    #[test]
    fn hash_changes_on_command_change_not_on_timeout() {
        let base = plugin_hook_set_hash(&[mk("SessionStart", None, "x")]);
        assert_ne!(
            base,
            plugin_hook_set_hash(&[mk("SessionStart", None, "x2")])
        );
        let mut with_timeout = mk("SessionStart", None, "x");
        with_timeout.timeout_secs = Some(30);
        assert_eq!(base, plugin_hook_set_hash(&[with_timeout]));
    }

    #[test]
    fn hash_no_collision_across_field_boundaries() {
        // Old delimiter-join scheme collided these two DISTINCT hook sets.
        let a = plugin_hook_set_hash(&[mk("E", Some("b\u{1f}c"), "d")]);
        let b = plugin_hook_set_hash(&[mk("E", Some("b"), "c\u{1f}d")]);
        assert_ne!(a, b);
    }

    #[test]
    #[serial_test::serial]
    fn trust_roundtrip() {
        let _home = crate::plugin::test_support::isolated_home();
        let id = plugin_id("superpowers", "superpowers-dev");
        let map = load_trust();
        assert!(!is_trusted(&map, &id, "h1"));
        trust(&id, "h1").unwrap();
        assert!(is_trusted(&load_trust(), &id, "h1"));
        // wrong hash (e.g. plugin updated) → not trusted
        assert!(!is_trusted(&load_trust(), &id, "h2"));
        untrust(&id).unwrap();
        assert!(!is_trusted(&load_trust(), &id, "h1"));
    }
}
