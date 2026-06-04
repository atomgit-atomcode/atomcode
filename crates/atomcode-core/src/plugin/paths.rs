use std::path::PathBuf;

use super::state::InstallScope;

/// Root directory: `${ATOMCODE_HOME:-$HOME/.atomcode}/plugins/`.
///
/// Always returns `Some(_)`. The underlying `Config::config_dir()` falls back
/// to `./.atomcode` when `$HOME` cannot be resolved, so callers no longer
/// need to handle a `None`.
pub fn plugins_root() -> Option<PathBuf> {
    Some(crate::config::Config::config_dir().join("plugins"))
}

pub fn marketplaces_root() -> Option<PathBuf> {
    Some(plugins_root()?.join("marketplaces"))
}

pub fn marketplaces_file() -> Option<PathBuf> {
    Some(plugins_root()?.join("marketplaces.json"))
}

pub fn installed_plugins_file() -> Option<PathBuf> {
    Some(plugins_root()?.join("installed_plugins.json"))
}

/// Project-level plugins directory for a given working directory and scope.
///
/// - `Project` scope: `<working_dir>/.atomcode/plugins/`
/// - `Local` scope: `<working_dir>/.atomcode/plugins/local/`
///
/// Returns `None` for `User` scope (user scope uses the global `plugins_root()`).
pub fn project_plugins_root(working_dir: &std::path::Path, scope: &InstallScope) -> Option<PathBuf> {
    match scope {
        InstallScope::Project => Some(working_dir.join(".atomcode/plugins")),
        InstallScope::Local => Some(working_dir.join(".atomcode/plugins/local")),
        InstallScope::User => None,
    }
}

/// Project-level `installed_plugins.json` path for a given scope.
pub fn project_installed_plugins_file(working_dir: &std::path::Path, scope: &InstallScope) -> Option<PathBuf> {
    project_plugins_root(working_dir, scope).map(|root| root.join("installed_plugins.json"))
}

/// Project-level marketplaces directory for a given scope.
#[allow(dead_code)]
pub fn project_marketplaces_root(working_dir: &std::path::Path, scope: &InstallScope) -> Option<PathBuf> {
    project_plugins_root(working_dir, scope).map(|root| root.join("marketplaces"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn plugins_root_uses_atomcode_home_override() {
        // Under the unified semantics, ATOMCODE_HOME IS the config root
        // (equivalent to ~/.atomcode), so plugins land directly under it.
        let _home = crate::plugin::test_support::isolated_home();
        let root = plugins_root().unwrap();
        assert_eq!(root, _home.path().join("plugins"));
    }

    #[test]
    fn project_plugins_root_project_scope() {
        let dir = std::path::Path::new("/tmp/myproject");
        let root = project_plugins_root(dir, &InstallScope::Project).unwrap();
        assert_eq!(root, std::path::PathBuf::from("/tmp/myproject/.atomcode/plugins"));
    }

    #[test]
    fn project_plugins_root_local_scope() {
        let dir = std::path::Path::new("/tmp/myproject");
        let root = project_plugins_root(dir, &InstallScope::Local).unwrap();
        assert_eq!(root, std::path::PathBuf::from("/tmp/myproject/.atomcode/plugins/local"));
    }

    #[test]
    fn project_plugins_root_user_scope_returns_none() {
        let dir = std::path::Path::new("/tmp/myproject");
        assert!(project_plugins_root(dir, &InstallScope::User).is_none());
    }
}
