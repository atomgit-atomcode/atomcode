use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Top-level marketplace manifest (`.atomcode-plugin/marketplace.json` or
/// `.claude-plugin/marketplace.json`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MarketplaceManifest {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginEntry {
    pub name: String,
    /// Either an inline path relative to the marketplace root (legacy / CC
    /// shorthand) or a tagged object describing an external source.
    #[serde(default)]
    pub source: PluginSource,
    #[serde(default)]
    pub description: Option<String>,
}

/// Plugin source spec. Mirrors Claude Code's marketplace schema.
///
/// Three wire forms are accepted via untagged deserialization:
///   1. A plain string → inline path inside the marketplace clone (e.g. "./",
///      "plugins/foo"). This is the historical AtomCode form.
///   2. A tagged object `{"source": "url"|"git"|"github"|"git-subdir"|"local",
///      ...}` describing an external location to clone/copy into the plugin's
///      own install directory.
///   3. Anything else (e.g. a future `{"source":"npm",...}`) → `Unknown`. This
///      keeps one unrecognised entry from failing the whole catalog parse
///      (see anthropics/claude-plugins-official#585, where a `git-subdir`
///      entry broke clients that only knew the older tags).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PluginSource {
    Inline(String),
    External(ExternalSource),
    /// Unrecognised source object. Consumers skip it (with a warning) instead
    /// of aborting the entire marketplace.
    Unknown(serde_json::Value),
}

impl Default for PluginSource {
    fn default() -> Self {
        PluginSource::Inline("./".into())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum ExternalSource {
    /// Arbitrary git URL. Cloned with `git clone`.
    Url {
        url: String,
        #[serde(flatten)]
        pin: GitPin,
    },
    /// Alias for `url` — Claude Code accepts both spellings.
    Git {
        url: String,
        #[serde(flatten)]
        pin: GitPin,
    },
    /// GitHub shorthand. Expanded to `https://github.com/<repo>.git`.
    Github {
        repo: String,
        #[serde(flatten)]
        pin: GitPin,
    },
    /// Plugin living in a SUBDIRECTORY of a git repo (Claude Code's
    /// `git-subdir`). `url` is a git URL or `owner/repo` shorthand; `path` is
    /// the subdirectory within the repo. The ref/commit pin rides in `GitPin`
    /// (`ref`). Realised by a sparse + partial clone of just that subtree.
    GitSubdir {
        url: String,
        path: String,
        #[serde(flatten)]
        pin: GitPin,
    },
    /// Local filesystem path. Contents copied into the install dir.
    Local { path: String },
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GitPin {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
}

/// Per-plugin manifest (`<plugin-dir>/plugin.json` or
/// `<plugin-dir>/.claude-plugin/plugin.json`). All fields optional.
///
/// Schema is the union of atomcode's original layout and Claude Code's
/// embedded format — both `skills: "skills"` (string path) and
/// `skills: ["./skills"]` (CC array form) parse, ditto for `hooks` which
/// can be either a path string (legacy) or an embedded CC hooks object.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PluginManifest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Path to the skills directory. Accepts a single path or a CC-style
    /// array (we use the first entry — multi-dir merging is a follow-up).
    #[serde(default)]
    pub skills: Option<PathOrList>,
    /// Path to commands directory. Same dual shape as `skills`.
    #[serde(default)]
    pub commands: Option<PathOrList>,
    /// Either a path to a legacy `hooks.json` (string) or an embedded
    /// Claude-Code-style hooks object keyed by event name.
    #[serde(default)]
    pub hooks: Option<HooksField>,
}

/// `"skills"` or `["./skills", "./more"]`. We keep all entries to leave the
/// door open for multi-dir merging; current callers pick the first.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PathOrList {
    One(String),
    Many(Vec<String>),
}

impl PathOrList {
    pub fn first(&self) -> &str {
        match self {
            PathOrList::One(s) => s.as_str(),
            PathOrList::Many(v) => v.first().map(String::as_str).unwrap_or(""),
        }
    }
}

/// Either a path to a hooks.json file, or an inline CC hooks map.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum HooksField {
    Path(String),
    Inline(CCHooksMap),
}

/// Claude Code's hook layout: `{ "<EventName>": [ { matcher?, hooks: [...] } ] }`.
/// Event names are PascalCase (`PreToolUse`, `UserPromptSubmit`, ...). Each
/// matcher group contains an optional regex/glob to filter on tool name and
/// a list of command specs to run.
pub type CCHooksMap = std::collections::BTreeMap<String, Vec<CCHookGroup>>;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CCHookGroup {
    #[serde(default)]
    pub matcher: Option<String>,
    pub hooks: Vec<CCHookSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CCHookSpec {
    /// CC always sets this to "command"; reserved for future variants.
    #[serde(default = "default_hook_type", rename = "type")]
    pub kind: String,
    pub command: String,
    /// CC's timeout is in seconds; we convert to ms in the adapter.
    #[serde(default)]
    pub timeout: Option<u64>,
}

fn default_hook_type() -> String {
    "command".to_string()
}

impl PluginManifest {
    pub fn skills_path(&self) -> &str {
        self.skills.as_ref().map(|p| p.first()).filter(|s| !s.is_empty()).unwrap_or("skills")
    }
    /// All skills paths declared in the manifest.
    ///
    /// When `skills` is absent, returns `vec!["skills"]` (single default).
    /// When it is a CC-style array (`["./skills/foo", "./skills/bar"]`),
    /// each entry is included so that `SkillRegistry::reload` can load
    /// skills from every declared directory.
    pub fn skills_paths(&self) -> Vec<&str> {
        match &self.skills {
            None => vec!["skills"],
            Some(PathOrList::One(s)) if s.is_empty() => vec!["skills"],
            Some(PathOrList::One(s)) => vec![s.as_str()],
            Some(PathOrList::Many(v)) => {
                let paths: Vec<&str> = v.iter().map(String::as_str).filter(|s| !s.is_empty()).collect();
                if paths.is_empty() { vec!["skills"] } else { paths }
            }
        }
    }
    pub fn commands_path(&self) -> &str {
        self.commands.as_ref().map(|p| p.first()).filter(|s| !s.is_empty()).unwrap_or("commands")
    }
    /// Path to a legacy `hooks.json`. Returns the default "hooks.json" both
    /// when the field is absent AND when it is the inline CC form (which has
    /// no associated path on disk — its hooks come from the manifest itself).
    pub fn hooks_path(&self) -> &str {
        match &self.hooks {
            Some(HooksField::Path(p)) => p.as_str(),
            _ => "hooks.json",
        }
    }
    /// CC-style inline hooks, if present. Loader callers convert these into
    /// `HookConfig` entries via `cc_hooks_to_atomcode`.
    pub fn inline_cc_hooks(&self) -> Option<&CCHooksMap> {
        match &self.hooks {
            Some(HooksField::Inline(m)) => Some(m),
            _ => None,
        }
    }
}

/// Try to load a marketplace manifest from a marketplace clone root.
/// Order: `.atomcode-plugin/marketplace.json` → `.claude-plugin/marketplace.json`.
/// Returns `Ok(None)` when neither file exists (single-plugin fallback caller).
/// Returns `Err` when a file exists but cannot be parsed (fail closed).
pub fn load_marketplace_manifest(marketplace_root: &Path) -> Result<Option<MarketplaceManifest>> {
    for rel in [".atomcode-plugin/marketplace.json", ".claude-plugin/marketplace.json"] {
        let path = marketplace_root.join(rel);
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let manifest: MarketplaceManifest = serde_json::from_str(&raw)
                .with_context(|| format!("parse {}", path.display()))?;
            return Ok(Some(manifest));
        }
    }
    Ok(None)
}

/// Load a plugin manifest. Search order mirrors the marketplace manifest:
///   1. `<plugin-dir>/.atomcode-plugin/plugin.json`  atomcode native
///   2. `<plugin-dir>/.claude-plugin/plugin.json`    Claude Code compat
///   3. `<plugin-dir>/plugin.json`                   legacy flat layout
/// First file that exists wins. Returns the default manifest when none
/// exist; returns `Err` when a file exists but cannot be parsed (fail closed).
pub fn load_plugin_manifest(plugin_dir: &Path) -> Result<PluginManifest> {
    for rel in [
        ".atomcode-plugin/plugin.json",
        ".claude-plugin/plugin.json",
        "plugin.json",
    ] {
        let path = plugin_dir.join(rel);
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let manifest: PluginManifest = serde_json::from_str(&raw)
                .with_context(|| format!("parse {}", path.display()))?;
            return Ok(manifest);
        }
    }
    Ok(PluginManifest::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_atomcode_manifest_with_priority_over_claude() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".atomcode-plugin")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude-plugin")).unwrap();
        std::fs::write(
            tmp.path().join(".atomcode-plugin/marketplace.json"),
            r#"{"name":"atom","plugins":[{"name":"a"}]}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".claude-plugin/marketplace.json"),
            r#"{"name":"claude","plugins":[{"name":"c"}]}"#,
        )
        .unwrap();
        let m = load_marketplace_manifest(tmp.path()).unwrap().unwrap();
        assert_eq!(m.name, "atom");
        assert_eq!(m.plugins[0].name, "a");
    }

    #[test]
    fn missing_manifest_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_marketplace_manifest(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn malformed_manifest_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".atomcode-plugin")).unwrap();
        std::fs::write(
            tmp.path().join(".atomcode-plugin/marketplace.json"),
            "{ not json",
        )
        .unwrap();
        assert!(load_marketplace_manifest(tmp.path()).is_err());
    }

    #[test]
    fn plugin_manifest_defaults() {
        let m = PluginManifest::default();
        assert_eq!(m.skills_path(), "skills");
        assert_eq!(m.commands_path(), "commands");
        assert_eq!(m.hooks_path(), "hooks.json");
    }

    #[test]
    fn plugin_manifest_loads_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("plugin.json"),
            r#"{"name":"p","skills":"my_skills"}"#,
        )
        .unwrap();
        let m = load_plugin_manifest(tmp.path()).unwrap();
        assert_eq!(m.skills_path(), "my_skills");
        assert_eq!(m.commands_path(), "commands");
    }

    #[test]
    fn parses_inline_source_string() {
        let raw = r#"{"name":"mp","plugins":[{"name":"p","source":"./sub"}]}"#;
        let m: MarketplaceManifest = serde_json::from_str(raw).unwrap();
        match &m.plugins[0].source {
            PluginSource::Inline(s) => assert_eq!(s, "./sub"),
            _ => panic!("expected Inline"),
        }
    }

    #[test]
    fn parses_object_source_url_with_extra_fields() {
        // Real Claude-Code-style marketplace.json: source is an object, plus
        // unknown sibling fields like version/author. Both must round-trip.
        let raw = r#"{
            "name": "ascend",
            "owner": {"name": "x"},
            "plugins": [{
                "name": "ascend",
                "source": {"source": "url", "url": "https://example.com/r.git"},
                "description": "d",
                "version": "1.0.0",
                "author": {"name": "a"}
            }]
        }"#;
        let m: MarketplaceManifest = serde_json::from_str(raw).unwrap();
        match &m.plugins[0].source {
            PluginSource::External(ExternalSource::Url { url, .. }) => {
                assert_eq!(url, "https://example.com/r.git");
            }
            _ => panic!("expected External::Url"),
        }
    }

    #[test]
    fn parses_object_source_github_with_branch_pin() {
        let raw = r#"{"name":"mp","plugins":[{"name":"p","source":{"source":"github","repo":"o/r","branch":"dev"}}]}"#;
        let m: MarketplaceManifest = serde_json::from_str(raw).unwrap();
        match &m.plugins[0].source {
            PluginSource::External(ExternalSource::Github { repo, pin }) => {
                assert_eq!(repo, "o/r");
                assert_eq!(pin.branch.as_deref(), Some("dev"));
            }
            _ => panic!("expected External::Github"),
        }
    }

    #[test]
    fn parses_object_source_local() {
        let raw = r#"{"name":"mp","plugins":[{"name":"p","source":{"source":"local","path":"/tmp/x"}}]}"#;
        let m: MarketplaceManifest = serde_json::from_str(raw).unwrap();
        match &m.plugins[0].source {
            PluginSource::External(ExternalSource::Local { path }) => assert_eq!(path, "/tmp/x"),
            _ => panic!("expected External::Local"),
        }
    }

    #[test]
    fn parses_object_source_git_alias() {
        let raw = r#"{"name":"mp","plugins":[{"name":"p","source":{"source":"git","url":"u"}}]}"#;
        let m: MarketplaceManifest = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            m.plugins[0].source,
            PluginSource::External(ExternalSource::Git { .. })
        ));
    }

    #[test]
    fn parses_git_subdir_shorthand_with_ref() {
        // The real official-catalog shape: owner/repo shorthand + path + ref.
        let raw = r#"{"name":"mp","plugins":[{"name":"p","source":{
            "source":"git-subdir","url":"openclaw/openclaw",
            "path":".agents/skills/autoreview","ref":"main"}}]}"#;
        let m: MarketplaceManifest = serde_json::from_str(raw).unwrap();
        match &m.plugins[0].source {
            PluginSource::External(ExternalSource::GitSubdir { url, path, pin }) => {
                assert_eq!(url, "openclaw/openclaw");
                assert_eq!(path, ".agents/skills/autoreview");
                assert_eq!(pin.git_ref.as_deref(), Some("main"));
            }
            other => panic!("expected External::GitSubdir, got {other:?}"),
        }
    }

    #[test]
    fn parses_git_subdir_full_url() {
        let raw = r#"{"name":"mp","plugins":[{"name":"p","source":{
            "source":"git-subdir","url":"https://example.com/r.git",
            "path":"plugins/foo"}}]}"#;
        let m: MarketplaceManifest = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            m.plugins[0].source,
            PluginSource::External(ExternalSource::GitSubdir { .. })
        ));
    }

    #[test]
    fn unknown_source_type_becomes_unknown_variant() {
        // A future/unsupported tag (e.g. `npm`) must not error — it lands in
        // PluginSource::Unknown so the catalog still parses.
        let raw = r#"{"name":"p","source":{"source":"npm","package":"@x/y"}}"#;
        let e: PluginEntry = serde_json::from_str(raw).unwrap();
        assert!(matches!(e.source, PluginSource::Unknown(_)));
    }

    /// Regression for anthropics/claude-plugins-official#585: one unrecognised
    /// entry must NOT fail the whole catalog. A mix of inline + git-subdir +
    /// an unknown `npm` entry parses, yielding all three (the npm one Unknown).
    #[test]
    fn catalog_with_one_unknown_entry_still_parses() {
        let raw = r#"{"name":"mp","plugins":[
            {"name":"a","source":"plugins/a"},
            {"name":"b","source":{"source":"npm","package":"@x/y"}},
            {"name":"c","source":{"source":"git-subdir","url":"o/r","path":"sub","ref":"main"}}
        ]}"#;
        let m: MarketplaceManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(m.plugins.len(), 3);
        assert!(matches!(m.plugins[0].source, PluginSource::Inline(_)));
        assert!(matches!(m.plugins[1].source, PluginSource::Unknown(_)));
        assert!(matches!(
            m.plugins[2].source,
            PluginSource::External(ExternalSource::GitSubdir { .. })
        ));
    }

    #[test]
    fn plugin_manifest_missing_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let m = load_plugin_manifest(tmp.path()).unwrap();
        assert_eq!(m.skills_path(), "skills");
    }

    #[test]
    fn skills_paths_default_returns_single_skills() {
        let m = PluginManifest::default();
        assert_eq!(m.skills_paths(), vec!["skills"]);
    }

    #[test]
    fn skills_paths_single_string() {
        let m: PluginManifest = serde_json::from_str(r#"{"skills":"my_skills"}"#).unwrap();
        assert_eq!(m.skills_paths(), vec!["my_skills"]);
    }

    #[test]
    fn skills_paths_cc_array() {
        let m: PluginManifest =
            serde_json::from_str(r#"{"skills":["./skills/foo","./skills/bar"]}"#).unwrap();
        assert_eq!(m.skills_paths(), vec!["./skills/foo", "./skills/bar"]);
    }

    #[test]
    fn skills_paths_empty_array_falls_back() {
        let m: PluginManifest = serde_json::from_str(r#"{"skills":[]}"#).unwrap();
        assert_eq!(m.skills_paths(), vec!["skills"]);
    }
}
