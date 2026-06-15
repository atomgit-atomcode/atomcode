//! Which language server to run for which file extension. Ported from production
//! `lsp/registry.rs` (defaults; the user-config merge is dropped — policy/config is an
//! L2 concern, injected via `LspServerRegistry::insert`).

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct LspServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub root_markers: Vec<String>,
}

pub struct LspServerRegistry {
    servers: HashMap<String, LspServerConfig>,
}

impl LspServerRegistry {
    pub fn empty() -> Self {
        Self { servers: HashMap::new() }
    }

    /// Common defaults (server binaries must be on PATH; absence degrades gracefully).
    pub fn with_defaults() -> Self {
        let mut servers = HashMap::new();
        let mut add = |ext: &str, command: &str, args: &[&str], markers: &[&str]| {
            servers.insert(
                ext.to_string(),
                LspServerConfig {
                    command: command.to_string(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                    root_markers: markers.iter().map(|s| s.to_string()).collect(),
                },
            );
        };
        add("rs", "rust-analyzer", &[], &["Cargo.toml"]);
        add("ts", "typescript-language-server", &["--stdio"], &["tsconfig.json", "package.json"]);
        add("tsx", "typescript-language-server", &["--stdio"], &["tsconfig.json"]);
        add("js", "typescript-language-server", &["--stdio"], &["package.json"]);
        add("py", "pylsp", &[], &["pyproject.toml", "setup.py"]);
        add("go", "gopls", &["serve"], &["go.mod"]);
        add("java", "jdtls", &[], &["pom.xml", "build.gradle"]);
        Self { servers }
    }

    pub fn get(&self, extension: &str) -> Option<&LspServerConfig> {
        self.servers.get(extension)
    }
    /// Inject / override a server config (L2 policy hook).
    pub fn insert(&mut self, ext: impl Into<String>, config: LspServerConfig) {
        self.servers.insert(ext.into(), config);
    }
    pub fn available_extensions(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.servers.keys().map(String::as_str).collect();
        v.sort();
        v
    }
}

impl Default for LspServerRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// LSP `languageId` for a file extension (sent in `textDocument/didOpen`). An unknown
/// extension maps to itself (production parity) so a server can still recognize a custom
/// language id rather than always seeing "plaintext".
pub fn extension_to_language_id(ext: &str) -> String {
    let id = match ext {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        other => other,
    };
    id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_common_langs() {
        let r = LspServerRegistry::with_defaults();
        assert_eq!(r.get("rs").unwrap().command, "rust-analyzer");
        assert_eq!(r.get("py").unwrap().command, "pylsp");
        assert_eq!(r.get("go").unwrap().command, "gopls");
        assert_eq!(r.get("ts").unwrap().args, vec!["--stdio".to_string()]);
        assert!(r.get("csv").is_none());
    }

    #[test]
    fn language_ids() {
        assert_eq!(extension_to_language_id("rs"), "rust");
        assert_eq!(extension_to_language_id("tsx"), "typescriptreact");
        assert_eq!(extension_to_language_id("cpp"), "cpp");
        assert_eq!(extension_to_language_id("zzz"), "zzz"); // unknown → itself
    }

    #[test]
    fn insert_overrides() {
        let mut r = LspServerRegistry::with_defaults();
        r.insert("rs", LspServerConfig { command: "custom-ra".into(), args: vec![], root_markers: vec![] });
        assert_eq!(r.get("rs").unwrap().command, "custom-ra");
    }
}
