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
    fn plugins_root_uses_atomcode_home_override() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", tmp.path());
        let root = plugins_root().unwrap();
        assert_eq!(root, tmp.path().join(".atomcode").join("plugins"));
        std::env::remove_var("ATOMCODE_HOME");
    }
}
