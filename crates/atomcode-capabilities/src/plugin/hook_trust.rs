//! Per-plugin content-hash trust for plugin-shipped hooks. A plugin's hooks run
//! only after the user trusts the CURRENT hash of its hook set (`atomcode plugin
//! trust <name>`). Changing a hook command changes the hash → re-trust required,
//! which blocks a benign-at-install plugin from silently adding hooks in an update.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use super::loader::PluginCcHook;

pub type TrustMap = BTreeMap<String, String>;

/// Stable identity for an installed plugin: `<plugin>@<marketplace>`.
pub fn plugin_id(plugin: &str, marketplace: &str) -> String {
    format!("{plugin}@{marketplace}")
}

/// SHA-256 over the sorted hook configuration and plugin bundle contents.
/// Install paths and timeouts are excluded. Each field is length-prefixed
/// so no field content (including control bytes) can shift a boundary and collide
/// two different hook sets.
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
    let metadata = format!("{:x}", hasher.clone().finalize());
    let roots: std::collections::BTreeSet<_> = hooks.iter().map(|h| &h.plugin_root).collect();
    hasher.update(b"plugin-content-v1");
    for root in roots {
        let content = match plugin_content_hash(root) {
            Ok(content) => content,
            Err(error) => {
                return format!(
                    "invalid:{metadata}: {}",
                    format!("{error:#}").escape_debug()
                )
            }
        };
        hasher.update(content.as_bytes());
    }
    #[cfg(feature = "cc-hooks")]
    for hook in hooks {
        if let Err(error) = crate::cc_hooks::validate_plugin_hook(hook) {
            let reason = format!(
                "{}: {}: {error}; use a plugin-relative script with python/python3/node/sh/bash",
                hook.event, hook.command
            );
            return format!("invalid:{metadata}: {}", reason.escape_debug());
        }
    }
    format!("{:x}", hasher.finalize())
}

/// Bundle contents, including imported helpers. Git metadata is outside this
/// trust scope. Links to files are included only when their targets stay in
/// the root; linked directories are rejected to avoid cycles or omitted code.
pub(crate) fn plugin_content_hash(root: &Path) -> Result<String> {
    use std::io::Read;
    fn walk(root: &Path, dir: &Path, hash: &mut Sha256) -> Result<()> {
        let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if entry.file_name() == ".git" {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(root)?;
            let target = std::fs::canonicalize(&path)?;
            anyhow::ensure!(
                target.starts_with(root),
                "plugin bundle link escapes its directory: {}",
                relative.display()
            );
            let meta = std::fs::symlink_metadata(&path)?;
            let name = relative
                .to_str()
                .context("plugin paths must be UTF-8")?
                .replace('\\', "/");
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            if meta.is_dir() {
                hash.update(b"dir");
                walk(root, &path, hash)?;
            } else {
                anyhow::ensure!(
                    target.is_file(),
                    "plugin bundle entry is not a regular file: {}",
                    relative.display()
                );
                hash.update(b"file");
                let mut file = std::fs::File::open(&target)?;
                let mut content = Sha256::new();
                let mut buffer = [0u8; 16384];
                loop {
                    let n = file.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    content.update(&buffer[..n]);
                }
                hash.update(content.finalize());
            }
        }
        Ok(())
    }
    let root = std::fs::canonicalize(root).context("plugin directory cannot be read")?;
    let mut hash = Sha256::new();
    walk(&root, &root, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn trust_store_path(user_dir: &Path) -> Option<PathBuf> {
    Some(super::paths::plugins_root(user_dir)?.join("hook_trust.json"))
}

/// Load the trust map. Missing/unreadable/malformed → empty (⇒ nothing trusted,
/// the safe default).
pub fn load_trust(user_dir: &Path) -> TrustMap {
    let Some(path) = trust_store_path(user_dir) else {
        return TrustMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return TrustMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn is_trusted(map: &TrustMap, plugin_id: &str, hash: &str) -> bool {
    !hash.starts_with("invalid:") && map.get(plugin_id).map(|h| h == hash).unwrap_or(false)
}

fn save_trust(user_dir: &Path, map: &TrustMap) -> Result<()> {
    let path = trust_store_path(user_dir).context("plugin home not configured")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_vec_pretty(map).context("serialize trust map")?;
    crate::fs::atomic_write(&path, &json, 0o600).context("write trust store")
}

pub fn trust(user_dir: &Path, plugin_id: &str, hash: &str) -> Result<()> {
    anyhow::ensure!(
        !hash.starts_with("invalid:"),
        "plugin {plugin_id} hooks cannot be trusted: {hash}"
    );
    let mut map = load_trust(user_dir);
    map.insert(plugin_id.to_string(), hash.to_string());
    save_trust(user_dir, &map)
}

pub fn untrust(user_dir: &Path, plugin_id: &str) -> Result<()> {
    let mut map = load_trust(user_dir);
    map.remove(plugin_id);
    save_trust(user_dir, &map)
}

fn migration_marker_path(user_dir: &Path) -> Option<PathBuf> {
    Some(super::paths::plugins_root(user_dir)?.join(".hook_trust_migrated"))
}

/// Record the migration boundary without granting content authorization.
/// Existing command-only grants deliberately require explicit re-trust.
pub fn ensure_migrated(dirs: &crate::ProductDirs) {
    let user_dir = dirs.user();
    let Some(marker) = migration_marker_path(user_dir) else {
        return;
    };
    if marker.exists() {
        return;
    }
    // A content authorization must be explicit. Never turn an old command-only
    // grant (or a pre-trust installation) into authorization for new bytes.
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
    #[cfg(feature = "cc-hooks")]
    fn incompatible_commands_are_safe_to_display_in_terminal_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let mut hook = mk(
            "UserPromptSubmit",
            None,
            "node -e '\u{1b}]52;c;payload\u{7}'\n",
        );
        hook.plugin_root = root.path().into();
        let hash = plugin_hook_set_hash(&[hook]);
        assert!(hash.starts_with("invalid:"));
        assert!(!hash.chars().any(char::is_control), "{hash:?}");
        assert!(hash.contains("payload"));
    }

    #[test]
    fn incompatible_entry_points_cannot_be_authorized() {
        let root = tempfile::tempdir().unwrap();
        let mut hook = mk("UserPromptSubmit", None, "sh -c 'echo hello'");
        hook.plugin_root = root.path().into();
        let hash = plugin_hook_set_hash(&[hook]);
        #[cfg(feature = "cc-hooks")]
        {
            assert!(hash.starts_with("invalid:"));
            assert!(trust(root.path(), "bad@plugin", &hash).is_err());
            assert!(!is_trusted(
                &BTreeMap::from([("bad@plugin".into(), hash.clone())]),
                "bad@plugin",
                &hash
            ));
        }
    }

    #[test]
    fn script_and_imported_helper_changes_invalidate_authorization() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("entry.py"), "import helper").unwrap();
        std::fs::write(root.path().join("helper.py"), "safe = True").unwrap();
        // The interpreter must be one this host resolves (twin in `cc_hooks`): the
        // pre-check accepts a real program found on PATH, and Windows resolves `.exe`/
        // `.com` only, where a bare `python3` does not exist.
        let entry = if cfg!(windows) {
            "python entry.py"
        } else {
            "python3 entry.py"
        };
        let mut hook = mk("UserPromptSubmit", None, entry);
        hook.plugin_root = root.path().into();
        let original = plugin_hook_set_hash(&[hook.clone()]);
        assert!(!original.starts_with("invalid:"));
        std::fs::write(root.path().join("helper.py"), "safe = False").unwrap();
        let changed = plugin_hook_set_hash(&[hook.clone()]);
        assert_ne!(original, changed);
        std::fs::write(root.path().join("entry.py"), "print('changed')").unwrap();
        assert_ne!(changed, plugin_hook_set_hash(&[hook]));
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
        let home = crate::plugin::test_support::isolated_home();
        let tree = home.path();
        let id = plugin_id("superpowers", "superpowers-dev");
        let map = load_trust(tree);
        assert!(!is_trusted(&map, &id, "h1"));
        trust(tree, &id, "h1").unwrap();
        assert!(is_trusted(&load_trust(tree), &id, "h1"));
        // wrong hash (e.g. plugin updated) → not trusted
        assert!(!is_trusted(&load_trust(tree), &id, "h2"));
        untrust(tree, &id).unwrap();
        assert!(!is_trusted(&load_trust(tree), &id, "h1"));
    }
}
