use std::path::PathBuf;

/// Root directory: `${ATOMCODE_HOME:-$HOME}/.atomcode/plugins/`.
pub fn plugins_root() -> Option<PathBuf> {
    let home: PathBuf = std::env::var("ATOMCODE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::tool::real_home_dir())?;
    Some(home.join(".atomcode").join("plugins"))
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
        let _home = crate::plugin::test_support::isolated_home();
        let root = plugins_root().unwrap();
        assert_eq!(root, _home.path().join(".atomcode").join("plugins"));
    }
}
