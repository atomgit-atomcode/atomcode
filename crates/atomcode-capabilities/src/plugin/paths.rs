use std::path::{Component, Path, PathBuf};

use super::state::InstallScope;

/// Root directory: `${ATOMCODE_HOME:-$HOME/.atomcode}/plugins/`.
///
/// Always returns `Some(_)`. The underlying `Config::config_dir()` falls back
/// to `./.atomcode` when `$HOME` cannot be resolved, so callers no longer
/// need to handle a `None`.
pub fn plugins_root() -> Option<PathBuf> {
    Some(atomcode_config::config::Config::config_dir().join("plugins"))
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
pub fn project_plugins_root(
    working_dir: &std::path::Path,
    scope: &InstallScope,
) -> Option<PathBuf> {
    match scope {
        InstallScope::Project => Some(working_dir.join(".atomcode/plugins")),
        InstallScope::Local => Some(working_dir.join(".atomcode/plugins/local")),
        InstallScope::User => None,
    }
}

/// Project-level `installed_plugins.json` path for a given scope.
pub fn project_installed_plugins_file(
    working_dir: &std::path::Path,
    scope: &InstallScope,
) -> Option<PathBuf> {
    project_plugins_root(working_dir, scope).map(|root| root.join("installed_plugins.json"))
}

/// Project-level marketplaces directory for a given scope.
#[allow(dead_code)]
pub fn project_marketplaces_root(
    working_dir: &std::path::Path,
    scope: &InstallScope,
) -> Option<PathBuf> {
    project_plugins_root(working_dir, scope).map(|root| root.join("marketplaces"))
}

/// True when a project/local scope's `installed_plugins.json` resolves to the
/// same file as the user-scope one.
///
/// This happens when `working_dir` IS the plugin home (e.g. running from
/// `$HOME`), where `<working_dir>/.atomcode/plugins` is the same directory as
/// the global `plugins_root()` — the same state file would otherwise be read
/// once per scope and every plugin enumerated twice. Callers (asset/status
/// iteration, `plugin list`) should skip such scopes.
pub fn scope_state_file_aliases_user_scope(working_dir: &Path, scope: &InstallScope) -> bool {
    let Some(scope_file) = project_installed_plugins_file(working_dir, scope) else {
        return false;
    };
    let Some(user_file) = installed_plugins_file() else {
        return false;
    };
    let (scope_file, user_file) = match (
        std::fs::canonicalize(&scope_file),
        std::fs::canonicalize(&user_file),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        // State files need not exist yet (for example before the first install),
        // so compare normalized absolute paths rather than raw PathBuf values.
        _ => (
            normalize_for_compare(&scope_file),
            normalize_for_compare(&user_file),
        ),
    };
    paths_equal(&scope_file, &user_file)
}

fn normalize_for_compare(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
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
        assert_eq!(
            root,
            std::path::PathBuf::from("/tmp/myproject/.atomcode/plugins")
        );
    }

    #[test]
    fn project_plugins_root_local_scope() {
        let dir = std::path::Path::new("/tmp/myproject");
        let root = project_plugins_root(dir, &InstallScope::Local).unwrap();
        assert_eq!(
            root,
            std::path::PathBuf::from("/tmp/myproject/.atomcode/plugins/local")
        );
    }

    #[test]
    fn project_plugins_root_user_scope_returns_none() {
        let dir = std::path::Path::new("/tmp/myproject");
        assert!(project_plugins_root(dir, &InstallScope::User).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn scope_state_file_aliases_user_scope_when_cwd_is_home() {
        // ATOMCODE_HOME 指向 <home>/.atomcode；cwd == <home> 时 project scope
        // 的 installed_plugins.json 与 user scope 是同一个文件。
        let _home = crate::plugin::test_support::isolated_home();
        let home_dir = _home.path().join("home");
        std::env::set_var("ATOMCODE_HOME", home_dir.join(".atomcode"));
        assert!(scope_state_file_aliases_user_scope(
            &home_dir,
            &InstallScope::Project
        ));
        // Local scope 位于 <home>/.atomcode/plugins/local/，与 user 文件不同。
        assert!(!scope_state_file_aliases_user_scope(
            &home_dir,
            &InstallScope::Local
        ));
        // User scope 无 project 状态文件路径。
        assert!(!scope_state_file_aliases_user_scope(
            &home_dir,
            &InstallScope::User
        ));
    }

    #[test]
    #[serial_test::serial]
    fn scope_state_file_aliases_user_scope_false_for_other_dir() {
        let _home = crate::plugin::test_support::isolated_home();
        let home_dir = _home.path().join("home");
        std::env::set_var("ATOMCODE_HOME", home_dir.join(".atomcode"));
        // 其他工作目录的 project scope 与 user scope 不是同一个文件。
        let other = _home.path().join("projects/myproj");
        assert!(!scope_state_file_aliases_user_scope(
            &other,
            &InstallScope::Project
        ));
    }

    #[test]
    fn normalize_for_compare_resolves_relative_dot_and_parent_components() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            normalize_for_compare(Path::new("project/../.atomcode/plugins")),
            cwd.join(".atomcode/plugins")
        );
        assert_eq!(
            normalize_for_compare(Path::new("./.atomcode/plugins")),
            cwd.join(".atomcode/plugins")
        );
    }
}
