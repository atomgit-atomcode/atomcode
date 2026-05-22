use std::path::PathBuf;

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
}
