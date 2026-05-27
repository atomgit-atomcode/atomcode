//! KnowledgeBase for the atomcode-guide subagent.
//!
//! Provides lazy-loading, keyword-indexed retrieval from Markdown files
//! with YAML frontmatter. All knowledge files live in `knowledge/` next
//! to this module.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// A single entry in the knowledge base.
#[derive(Debug, Clone)]
pub struct KnowledgeEntry {
    /// Display title of the entry.
    pub title: String,
    /// Category grouping (e.g. "command", "config", "mcp", "skill").
    pub category: String,
    /// Keywords for index-based retrieval.
    pub keywords: Vec<String>,
    /// Full Markdown body (without frontmatter).
    pub content: String,
    /// Filesystem path of the source file.
    pub path: PathBuf,
}

/// Lazily-loaded inner state with the full entry list and keyword index.
#[derive(Debug, Clone)]
struct KnowledgeInner {
    entries: Vec<KnowledgeEntry>,
    /// Maps lowercase keyword -> list of entry indices that contain it.
    keyword_index: HashMap<String, Vec<usize>>,
}

/// Thread-safe knowledge base that loads Markdown files from `knowledge/`.
///
/// The actual loading is deferred until the first search or render call,
/// making it cheap to construct even when the knowledge base may not be used.
///
/// # Thread safety
///
/// `KnowledgeBase` uses `OnceLock` internally so it is safe to share
/// across threads. The `Clone` implementation creates a new `OnceLock`;
/// the underlying data is only loaded once in any clone, which is
/// acceptable given the expected memory footprint of the knowledge files.
#[derive(Debug)]
pub struct KnowledgeBase {
    inner: OnceLock<KnowledgeInner>,
    base_dir: PathBuf,
}

impl Clone for KnowledgeBase {
    fn clone(&self) -> Self {
        // We intentionally create a fresh OnceLock so that each clone
        // can independently detect its first access. The underlying data
        // may be loaded more than once across clones, but the expected
        // number of entries is tiny (< 100 files, < 1 MiB total).
        let inner = OnceLock::new();
        // If the original is already loaded, seed the clone's OnceLock
        // so it doesn't re-read from disk.
        if let Some(data) = self.inner.get() {
            let _ = inner.set(data.clone());
        }
        Self {
            inner,
            base_dir: self.base_dir.clone(),
        }
    }
}

impl KnowledgeBase {
    /// Create a new `KnowledgeBase` rooted at `knowledge/` next to this source file.
    ///
    /// The base directory is resolved at compile time via `CARGO_MANIFEST_DIR`
    /// so that knowledge files are found regardless of the runtime working directory.
    pub fn new() -> Self {
        // env!("CARGO_MANIFEST_DIR") is resolved at compile time to
        // atomcode-core's Cargo.toml directory.
        let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/agent/guide/knowledge");
        Self {
            inner: OnceLock::new(),
            base_dir,
        }
    }

    /// Create a `KnowledgeBase` with a custom base directory (used in tests).
    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        Self {
            inner: OnceLock::new(),
            base_dir,
        }
    }

    // -----------------------------------------------------------------
    // Internal loading & parsing
    // -----------------------------------------------------------------

    /// Load (or reload) all knowledge entries from disk.
    ///
    /// This is called at most once per `KnowledgeBase` instance because
    /// `get_or_load` uses `OnceLock`. In the common case the data stays
    /// resident for the lifetime of the agent.
    fn load(&self) -> KnowledgeInner {
        let mut entries = Vec::new();
        let mut keyword_index: HashMap<String, Vec<usize>> = HashMap::new();

        let dir = match std::fs::read_dir(&self.base_dir) {
            Ok(d) => d,
            Err(e) => {
                // Directory may not exist yet if no knowledge files have been shipped.
                tracing::warn!(
                    "KnowledgeBase: cannot read directory {}: {}",
                    self.base_dir.display(),
                    e
                );
                return KnowledgeInner {
                    entries,
                    keyword_index,
                };
            }
        };

        for entry in dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            if path.extension().map_or(true, |e| e != "md") {
                continue;
            }
            match self.parse_md(&path) {
                Ok(ke) => {
                    let idx = entries.len();
                    for kw in &ke.keywords {
                        keyword_index
                            .entry(kw.to_lowercase())
                            .or_default()
                            .push(idx);
                    }
                    entries.push(ke);
                }
                Err(e) => {
                    tracing::warn!(
                        "KnowledgeBase: skipping corrupted file {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }

        tracing::debug!(
            "KnowledgeBase: loaded {} entries from {}",
            entries.len(),
            self.base_dir.display()
        );
        KnowledgeInner {
            entries,
            keyword_index,
        }
    }

    /// Parse a single Markdown file with optional YAML frontmatter.
    ///
    /// Expected format:
    /// ```markdown
    /// ---
    /// title: "My Title"
    /// category: "config"
    /// keywords: [keyword1, keyword2]
    /// ---
    /// Content body...
    /// ```
    fn parse_md(&self, path: &std::path::Path) -> Result<KnowledgeEntry, String> {
        let raw =
            std::fs::read_to_string(path).map_err(|e| format!("read error: {}", e))?;

        // Detect and strip YAML frontmatter
        let (frontmatter, content) = if raw.starts_with("---\n") || raw.starts_with("---\r\n") {
            // The three dashes and an optional trailing newline
            let rest = &raw[3..].trim_start_matches(|c| c == '\n' || c == '\r');
            if let Some(end) = rest.find("\n---") {
                let fm = &rest[..end];
                let body_section = &rest[end + 4..];
                // Strip a single leading newline from the body if present
                let body = body_section
                    .strip_prefix('\n')
                    .unwrap_or(body_section)
                    .trim();
                (fm.to_string(), body.to_string())
            } else {
                return Err("missing closing ---".to_string());
            }
        } else {
            ("".to_string(), raw.trim().to_string())
        };

        let mut title = String::new();
        let mut category = String::new();
        let mut keywords = Vec::new();

        for line in frontmatter.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim();
                match key {
                    "title" => {
                        title = value.trim_matches('"').to_string();
                    }
                    "category" => {
                        category = value.trim_matches('"').to_string();
                    }
                    "keywords" => {
                        let inner_list = value
                            .trim_start_matches('[')
                            .trim_end_matches(']');
                        keywords = inner_list
                            .split(',')
                            .map(|k| k.trim().trim_matches('"').to_lowercase())
                            .filter(|k| !k.is_empty())
                            .collect();
                    }
                    _ => {}
                }
            }
        }

        Ok(KnowledgeEntry {
            title,
            category,
            keywords,
            content,
            path: path.to_path_buf(),
        })
    }

    // -----------------------------------------------------------------
    // Retrieval
    // -----------------------------------------------------------------

    /// Get-or-load the inner data structure.
    fn get_or_load(&self) -> &KnowledgeInner {
        self.inner.get_or_init(|| self.load())
    }

    /// Perform a keyword-based search against the loaded knowledge entries.
    ///
    /// The query is lowercased, split on whitespace, and matched with AND
    /// semantics — every word must match at least one keyword prefix in the
    /// entry. Returns indices into the `entries` vector, sorted and dedup'd.
    ///
    /// When the query is empty, all entry indices are returned.
    pub fn search(&self, query: &str) -> Vec<usize> {
        let inner = self.get_or_load();
        let words: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        if words.is_empty() {
            return (0..inner.entries.len()).collect();
        }

        // For each word, collect all entry indices whose keyword_index keys
        // *contain* that word as a substring. Then intersect across words (AND).
        let mut result: Option<Vec<usize>> = None;
        for word in &words {
            let matched: Vec<usize> = inner
                .keyword_index
                .iter()
                .filter(|(k, _)| k.contains(word))
                .flat_map(|(_, indices)| indices.iter().copied())
                .collect();

            result = match result {
                None => Some(matched),
                Some(existing) => {
                    let set: std::collections::HashSet<usize> =
                        matched.into_iter().collect();
                    Some(
                        existing
                            .into_iter()
                            .filter(|i| set.contains(i))
                            .collect(),
                    )
                }
            };
        }

        let mut indices = result.unwrap_or_default();
        indices.sort();
        indices.dedup();
        indices
    }

    /// Render a knowledge response for a given query, respecting a token budget.
    ///
    /// When there are hits, renders full entry content (capped at `max_tokens`).
    /// When there are no hits, renders an overview of all available entries so
    /// the caller can refine their query.
    ///
    /// The `max_tokens` parameter is an approximate limit: we multiply by 4 to
    /// get a character budget, then stop adding entries once we would exceed it.
    pub fn render_for_query(&self, query: &str, max_tokens: usize) -> String {
        let inner = self.get_or_load();
        let hits = self.search(query);
        let max_chars = max_tokens * 4;

        if hits.is_empty() {
            let mut out = String::from("## 可用知识条目\n\n");
            for entry in &inner.entries {
                let line = format!("- **{}** ({})\n", entry.title, entry.category);
                if out.len() + line.len() > max_chars {
                    break;
                }
                out.push_str(&line);
            }
            out.push_str("\n请指定你想了解哪个功能的更多信息。");
            return out;
        }

        let mut out = String::from("## 相关知识\n\n");
        for idx in &hits {
            if *idx >= inner.entries.len() {
                continue;
            }
            let entry = &inner.entries[*idx];
            let chunk = format!(
                "### {} ({})\n\n{}\n\n",
                entry.title, entry.category, entry.content
            );
            if out.len() + chunk.len() > max_chars {
                out.push_str("... (知识库内容已截断)\n");
                break;
            }
            out.push_str(&chunk);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_kb(files: &[(&str, &str)]) -> (TempDir, KnowledgeBase) {
        let tmp = TempDir::new().unwrap();
        let kb_dir = tmp.path().join("knowledge");
        fs::create_dir(&kb_dir).unwrap();

        for (name, content) in files {
            fs::write(kb_dir.join(name), content).unwrap();
        }

        let kb = KnowledgeBase::with_base_dir(kb_dir);
        (tmp, kb)
    }

    #[test]
    fn test_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let kb = KnowledgeBase::with_base_dir(tmp.path().join("nonexistent"));

        let hits = kb.search("anything");
        assert!(hits.is_empty(), "no hits from empty directory");

        let rendered = kb.render_for_query("anything", 100);
        assert!(
            rendered.contains("可用知识条目"),
            "should render entry overview on miss"
        );
    }

    #[test]
    fn test_parse_frontmatter() {
        let (tmp, kb) = create_test_kb(&[(
            "hello.md",
            r#"---
title: "Hello World"
category: "test"
keywords: [greeting, hello, world]
---
This is the content body.
"#,
        )]);
        let _tmp = tmp; // keep alive

        let inner = kb.get_or_load();
        assert_eq!(inner.entries.len(), 1);

        let entry = &inner.entries[0];
        assert_eq!(entry.title, "Hello World");
        assert_eq!(entry.category, "test");
        assert_eq!(entry.keywords, vec!["greeting", "hello", "world"]);
        assert!(entry.content.contains("This is the content body"));
    }

    #[test]
    fn test_parse_no_frontmatter() {
        let (tmp, kb) = create_test_kb(&[("plain.md", "Just some plain markdown content.")]);
        let _tmp = tmp;

        let inner = kb.get_or_load();
        assert_eq!(inner.entries.len(), 1);

        let entry = &inner.entries[0];
        assert_eq!(entry.title, "");
        assert_eq!(entry.category, "");
        assert!(entry.keywords.is_empty());
        assert_eq!(entry.content, "Just some plain markdown content.");
    }

    #[test]
    fn test_search_by_keyword() {
        let (tmp, kb) = create_test_kb(&[
            (
                "cmd.md",
                r#"---
title: "Commands"
category: "reference"
keywords: [command, cli, usage]
---
List of commands.
"#,
            ),
            (
                "config.md",
                r#"---
title: "Configuration"
category: "guide"
keywords: [config, setup, install]
---
How to configure.
"#,
            ),
        ]);
        let _tmp = tmp;

        // Match "command" keyword
        let hits = kb.search("command");
        assert_eq!(hits.len(), 1, "only 'Commands' should match 'command'");
        assert_eq!(kb.get_or_load().entries[hits[0]].title, "Commands");

        // Match "config" keyword
        let hits = kb.search("config");
        assert_eq!(hits.len(), 1);
        assert_eq!(kb.get_or_load().entries[hits[0]].title, "Configuration");

        // AND match: no entry matches both "command" and "setup"
        let hits = kb.search("command setup");
        assert_eq!(hits.len(), 0, "no entry has both 'command' and 'setup'");
    }

    #[test]
    fn test_render_for_query_hit() {
        let (tmp, kb) = create_test_kb(&[(
            "cmd.md",
            r#"---
title: "Commands"
category: "reference"
keywords: [command, cli]
---
Full content about commands here.
"#,
        )]);
        let _tmp = tmp;

        let rendered = kb.render_for_query("cli", 100);
        assert!(rendered.contains("Commands (reference)"));
        assert!(rendered.contains("Full content about commands here."));
    }

    #[test]
    fn test_render_for_query_miss() {
        let (tmp, kb) = create_test_kb(&[(
            "cmd.md",
            r#"---
title: "Commands"
category: "reference"
keywords: [command, cli]
---
Full content.
"#,
        )]);
        let _tmp = tmp;

        let rendered = kb.render_for_query("nonexistent", 100);
        assert!(rendered.contains("可用知识条目"));
        assert!(rendered.contains("Commands"));
    }

    #[test]
    fn test_render_with_token_budget() {
        let (tmp, kb) = create_test_kb(&[
            (
                "a.md",
                r#"---
title: "Entry A"
category: "cat-a"
keywords: [alpha]
---
A content with enough text to fill up budget.
"#,
            ),
            (
                "b.md",
                r#"---
title: "Entry B"
category: "cat-b"
keywords: [alpha]
---
B content.
"#,
            ),
        ]);
        let _tmp = tmp;

        // Small budget means only one entry fits (header + one chunk)
        let rendered = kb.render_for_query("alpha", 20);
        let has_a = rendered.contains("Entry A");
        let has_b = rendered.contains("Entry B");
        // At least one entry should be shown
        assert!(has_a || has_b, "at least one entry should render");
        // Truncation marker should appear if only one entry fits
        assert!(
            !(has_a && has_b) || rendered.contains("截断"),
            "with tight budget both entries should not fit without truncation"
        );
    }

    #[test]
    fn test_chinese_query() {
        let (tmp, kb) = create_test_kb(&[(
            "zh.md",
            r#"---
title: "中文标题"
category: "测试"
keywords: [中文, 配置, atomcode]
---
简体中文内容。
"#,
        )]);
        let _tmp = tmp;

        let hits = kb.search("中文");
        assert_eq!(hits.len(), 1);

        let rendered = kb.render_for_query("atomcode", 100);
        assert!(rendered.contains("中文标题"));
    }
}
