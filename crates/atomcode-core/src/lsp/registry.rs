use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
}

pub struct LspServerRegistry {
    servers: HashMap<String, LspServerConfig>,
}

impl LspServerRegistry {
    pub fn empty() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        let mut servers = HashMap::new();
        servers.insert(
            "rs".into(),
            LspServerConfig {
                command: "rust-analyzer".into(),
                args: vec![],
                root_markers: vec!["Cargo.toml".into()],
            },
        );
        servers.insert(
            "ts".into(),
            LspServerConfig {
                command: "typescript-language-server".into(),
                args: vec!["--stdio".into()],
                root_markers: vec!["tsconfig.json".into(), "package.json".into()],
            },
        );
        servers.insert(
            "tsx".into(),
            LspServerConfig {
                command: "typescript-language-server".into(),
                args: vec!["--stdio".into()],
                root_markers: vec!["tsconfig.json".into()],
            },
        );
        servers.insert(
            "js".into(),
            LspServerConfig {
                command: "typescript-language-server".into(),
                args: vec!["--stdio".into()],
                root_markers: vec!["package.json".into()],
            },
        );
        servers.insert(
            "py".into(),
            LspServerConfig {
                command: "pylsp".into(),
                args: vec![],
                root_markers: vec!["pyproject.toml".into(), "setup.py".into()],
            },
        );
        servers.insert(
            "go".into(),
            LspServerConfig {
                command: "gopls".into(),
                args: vec!["serve".into()],
                root_markers: vec!["go.mod".into()],
            },
        );
        servers.insert(
            "java".into(),
            LspServerConfig {
                command: "jdtls".into(),
                args: vec![],
                root_markers: vec!["pom.xml".into(), "build.gradle".into()],
            },
        );
        Self { servers }
    }

    pub fn get(&self, extension: &str) -> Option<&LspServerConfig> {
        self.servers.get(extension)
    }

    pub fn server_for_file(&self, file_path: &str) -> Option<&LspServerConfig> {
        let ext = file_path.rsplit('.').next()?;
        self.servers.get(ext)
    }

    pub fn merge_user_config(&mut self, user: HashMap<String, LspServerConfig>) {
        for (ext, config) in user {
            self.servers.insert(ext, config);
        }
    }

    pub fn available_languages(&self) -> Vec<&str> {
        let mut langs: Vec<_> = self.servers.keys().map(String::as_str).collect();
        langs.sort();
        langs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_common_langs() {
        let registry = LspServerRegistry::with_defaults();
        let langs = registry.available_languages();
        assert!(langs.contains(&"rs"));
        assert!(langs.contains(&"ts"));
        assert!(langs.contains(&"py"));
        assert!(langs.contains(&"go"));
        assert!(langs.contains(&"java"));
        assert!(langs.contains(&"js"));
        assert!(langs.contains(&"tsx"));
    }

    #[test]
    fn server_for_file_resolves() {
        let registry = LspServerRegistry::with_defaults();

        let rust_cfg = registry.server_for_file("src/main.rs").unwrap();
        assert_eq!(rust_cfg.command, "rust-analyzer");

        let ts_cfg = registry.server_for_file("app/index.ts").unwrap();
        assert_eq!(ts_cfg.command, "typescript-language-server");

        let py_cfg = registry.server_for_file("script.py").unwrap();
        assert_eq!(py_cfg.command, "pylsp");

        // Unknown extension returns None
        assert!(registry.server_for_file("data.csv").is_none());
    }

    #[test]
    fn user_config_overrides() {
        let mut registry = LspServerRegistry::with_defaults();

        // Override Rust to use a custom analyzer
        let mut user = HashMap::new();
        user.insert(
            "rs".into(),
            LspServerConfig {
                command: "custom-rust-analyzer".into(),
                args: vec!["--custom".into()],
                root_markers: vec!["Cargo.toml".into()],
            },
        );
        // Add a new language
        user.insert(
            "rb".into(),
            LspServerConfig {
                command: "solargraph".into(),
                args: vec!["stdio".into()],
                root_markers: vec!["Gemfile".into()],
            },
        );
        registry.merge_user_config(user);

        let rust_cfg = registry.get("rs").unwrap();
        assert_eq!(rust_cfg.command, "custom-rust-analyzer");

        let ruby_cfg = registry.get("rb").unwrap();
        assert_eq!(ruby_cfg.command, "solargraph");
    }
}
