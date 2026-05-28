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

/// Estimate the number of LLM tokens for a string.
///
/// - CJK characters: ~1.5 tokens each
/// - ASCII alphanumeric: ~0.25 tokens each (4 chars ≈ 1 token)
/// - Whitespace/punctuation: ~0.25 tokens each
fn estimate_tokens(text: &str) -> usize {
    let mut tokens: f64 = 0.0;
    let mut ascii_word_len = 0usize;

    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            ascii_word_len += 1;
        } else {
            if ascii_word_len > 0 {
                tokens += (ascii_word_len as f64) / 4.0;
                ascii_word_len = 0;
            }
            if contains_cjk(&ch.to_string()) {
                tokens += 1.5;
            } else {
                tokens += 0.25;
            }
        }
    }
    if ascii_word_len > 0 {
        tokens += (ascii_word_len as f64) / 4.0;
    }
    tokens.ceil() as usize
}

/// Check if a string contains CJK (Chinese/Japanese/Korean) characters.
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c,
            '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
            | '\u{3400}'..='\u{4DBF}' // CJK Extension A
            | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        )
    })
}

/// Truncate text at a paragraph boundary, preserving code block integrity.
/// Returns empty string if no clean boundary is found within budget.
fn truncate_at_boundary(text: &str, estimated_token_budget: usize) -> String {
    // Rough conversion: mixed content ~2 chars per token
    let char_budget = estimated_token_budget * 2;
    if text.len() <= char_budget {
        return text.to_string();
    }

    let mut result = String::new();
    let mut in_code_block = false;
    let mut code_block_start = 0usize;

    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            if in_code_block {
                in_code_block = false;
            } else {
                in_code_block = true;
                code_block_start = result.len();
            }
            result.push_str(line);
            result.push('\n');
        } else {
            result.push_str(line);
            result.push('\n');
        }

        // Check budget at paragraph boundaries (blank lines)
        if !in_code_block && line.trim().is_empty() && result.len() >= char_budget {
            break;
        }
    }

    // If we ended inside a code block, roll back to before the block
    if in_code_block {
        result.truncate(code_block_start);
    }

    result
}

/// Check if a query word matches a keyword, supporting CJK substring matching.
///
/// For ASCII queries, exact word match is required.
/// For CJK-containing queries, substring matching is used since Chinese text
/// has no whitespace delimiters.
fn keyword_matches(keyword: &str, query_word: &str) -> bool {
    // Exact match (works for both ASCII and CJK)
    if keyword == query_word {
        return true;
    }
    // CJK substring matching: only when the query contains CJK characters
    if contains_cjk(query_word) && query_word.contains(keyword) {
        return true;
    }
    if contains_cjk(keyword) && keyword.contains(query_word) {
        return true;
    }
    false
}

impl KnowledgeBase {
    /// Create a new `KnowledgeBase` with embedded knowledge files.
    ///
    /// Files are embedded at compile time via `include_str!` so the
    /// knowledge base works regardless of the runtime working directory.
    pub fn embedded() -> Self {
        let inner = OnceLock::new();
        let _ = inner.set(Self::load_embedded());
        Self {
            inner,
            base_dir: PathBuf::new(),
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
    // Embedded loading
    // -----------------------------------------------------------------

    fn load_embedded() -> KnowledgeInner {
        let files: &[(&str, &str)] = &[
            ("overview.md", include_str!("knowledge/overview.md")),
            ("features.md", include_str!("knowledge/features.md")),
            ("slash-commands.md", include_str!("knowledge/slash-commands.md")),
            ("mcp.md", include_str!("knowledge/mcp.md")),
            ("skills.md", include_str!("knowledge/skills.md")),
            ("config.md", include_str!("knowledge/config.md")),
            ("bg.md", include_str!("knowledge/bg.md")),
            ("context.md", include_str!("knowledge/context.md")),
            ("getting-started.md", include_str!("knowledge/getting-started.md")),
            ("memory.md", include_str!("knowledge/memory.md")),
            ("modes.md", include_str!("knowledge/modes.md")),
            ("sessions.md", include_str!("knowledge/sessions.md")),
            ("worktree.md", include_str!("knowledge/worktree.md")),
            ("troubleshooting.md", include_str!("knowledge/troubleshooting.md")),
            ("doc-urls.md", include_str!("knowledge/doc-urls.md")),
            ("keybindings.md", include_str!("knowledge/keybindings.md")),
        ];

        let mut entries = Vec::new();
        let mut keyword_index: HashMap<String, Vec<usize>> = HashMap::new();

        for (name, raw) in files {
            match Self::parse_raw(name, raw) {
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
                    tracing::warn!("KnowledgeBase: skipping embedded {}: {}", name, e);
                }
            }
        }

        tracing::debug!("KnowledgeBase: loaded {} entries (embedded)", entries.len());
        KnowledgeInner { entries, keyword_index }
    }

    // -----------------------------------------------------------------
    // Disk-based loading (for tests and custom knowledge)
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

    /// Parse a single Markdown file from disk (used in tests).
    fn parse_md(&self, path: &std::path::Path) -> Result<KnowledgeEntry, String> {
        let raw =
            std::fs::read_to_string(path).map_err(|e| format!("read error: {}", e))?;
        Self::parse_raw(&path.to_string_lossy(), &raw)
    }

    /// Parse Markdown content with optional YAML frontmatter.
    ///
    /// `source_name` is used for diagnostics and the entry's path field.
    fn parse_raw(source_name: &str, raw: &str) -> Result<KnowledgeEntry, String> {
        // Detect and strip YAML frontmatter
        let raw_trimmed = raw.trim();
        let (frontmatter, content) = if raw_trimmed.starts_with("---\n") || raw_trimmed.starts_with("---\r\n") {
            let rest = &raw_trimmed[3..].trim_start_matches(|c| c == '\n' || c == '\r');
            if let Some(end) = rest.find("\n---") {
                let fm = &rest[..end];
                let body_section = &rest[end + 4..];
                let body = body_section
                    .strip_prefix('\n')
                    .unwrap_or(body_section)
                    .trim();
                (fm.to_string(), body.to_string())
            } else {
                return Err("missing closing ---".to_string());
            }
        } else {
            ("".to_string(), raw_trimmed.to_string())
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
            path: PathBuf::from(source_name),
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
                .filter(|(k, _)| {
                    k.split_whitespace()
                        .any(|kw| keyword_matches(kw, word.as_str()))
                })
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
    /// OR fallback: when AND search returns nothing, try matching entries
    /// by any single query word. Returns entries sorted by match count.
    fn search_or(&self, query: &str) -> Vec<usize> {
        let inner = self.get_or_load();
        let words: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        if words.is_empty() {
            return (0..inner.entries.len()).collect();
        }
        let mut scores: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for word in &words {
            for (kw, indices) in &inner.keyword_index {
                if kw.split_whitespace()
                    .any(|w| keyword_matches(w, word.as_str()))
                {
                    for &idx in indices {
                        *scores.entry(idx).or_default() += 1;
                    }
                }
            }
        }
        let mut pairs: Vec<(usize, usize)> = scores.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        pairs.into_iter().map(|(i, _)| i).collect()
    }

    pub fn render_for_query(&self, query: &str, max_tokens: usize) -> String {
        let inner = self.get_or_load();
        let hits = self.search(query);
        // AND may be too strict for CJK queries — fall back to OR.
        let hits = if hits.is_empty() { self.search_or(query) } else { hits };
        tracing::debug!(query, hits = hits.len(), "knowledge search");
        if hits.is_empty() && !query.is_empty() {
            tracing::debug!(query, "AND search empty, falling back to OR");
        }

        if hits.is_empty() {
            return format!(
                "本地知识库中未找到与「{}」直接相关的条目。\n\n\
                 你可以试试这些常见问题：\n\
                 - /guide 怎么切换模型\n\
                 - /guide MCP 怎么配置\n\
                 - /guide 怎么用记忆功能\n\
                 - /guide 快捷键有哪些\n\
                 - /guide 怎么用后台任务\n\n\
                 也可以访问文档站：https://atomcode.atomgit.com/docs/zh/",
                query,
            );
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
            let combined = format!("{}{}", out, chunk);
            if estimate_tokens(&combined) > max_tokens {
                // Try to fit a partial entry at paragraph boundary
                let remaining = max_tokens.saturating_sub(estimate_tokens(&out));
                if remaining > 50 {
                    let partial = truncate_at_boundary(&chunk, remaining);
                    if !partial.is_empty() {
                        out.push_str(&partial);
                        out.push_str("\n... (知识库内容已截断)\n");
                    }
                } else {
                    out.push_str("... (知识库内容已截断)\n");
                }
                break;
            }
            out = combined;
        }
        out
    }
}

impl crate::agent::sub_agent::types::KnowledgeProvider for KnowledgeBase {
    fn render_for_query(&self, query: &str, max_tokens: usize) -> String {
        self.render_for_query(query, max_tokens)
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
            rendered.contains("本地知识库中未找到"),
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
        assert!(rendered.contains("本地知识库中未找到"));
        assert!(rendered.contains("atomcode.atomgit.com"));
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

    #[test]
    fn test_search_substring_no_false_positive() {
        // "at" should NOT match "configuration" with word-boundary matching.
        // This tests the Fix 4 change from k.contains(word) to word-boundary matching.
        let (tmp, kb) = create_test_kb(&[(
            "config.md",
            r#"---
title: "Configuration"
category: "config"
keywords: [configuration, setup]
---
Config docs.
"#,
        )]);
        let _tmp = tmp;

        let hits = kb.search("at");
        assert!(hits.is_empty(), "'at' should not match keyword 'configuration'");

        // Sanity: actual keyword still matches
        let hits = kb.search("configuration");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_search_exact_word_only() {
        let (tmp, kb) = create_test_kb(&[(
            "test.md",
            r#"---
title: "Test"
category: "test"
keywords: [con, test]
---
Content.
"#,
        )]);
        let _tmp = tmp;

        // "con" should match "con" but NOT "configuration" or "content"
        let hits = kb.search("con");
        assert_eq!(hits.len(), 1, "'con' should match keyword 'con'");
        assert_eq!(kb.get_or_load().entries[hits[0]].title, "Test");

        // "test" should match "test"
        let hits = kb.search("test");
        assert_eq!(hits.len(), 1);

        // "testing" should NOT match "test" (exact word match)
        let hits = kb.search("testing");
        assert_eq!(hits.len(), 0, "'testing' should not match keyword 'test' (exact word required)");
    }

    #[test]
    fn test_chinese_concatenated_query() {
        // Chinese input without spaces should still match individual keywords
        let (tmp, kb) = create_test_kb(&[(
            "mcp.md",
            r#"---
title: "MCP 集成"
category: "扩展"
keywords: [mcp, 配置, 怎么, server]
---
MCP 配置说明。
"#,
        )]);
        let _tmp = tmp;

        // "怎么配置MCP" is a concatenated Chinese query — should match keywords "怎么", "配置", "mcp"
        let hits = kb.search("怎么配置mcp");
        assert_eq!(hits.len(), 1, "concatenated Chinese query should match via substring");

        // Partial match: "怎么用mcp" contains "怎么" and "mcp"
        let hits = kb.search("怎么用mcp");
        assert_eq!(hits.len(), 1, "partial Chinese query should match");
    }

    #[test]
    fn test_chinese_substring_match_not_for_english() {
        // Ensure ASCII queries still use exact word matching
        let (tmp, kb) = create_test_kb(&[(
            "test.md",
            r#"---
title: "Test"
category: "test"
keywords: [command, config]
---
Content.
"#,
        )]);
        let _tmp = tmp;

        // "com" should NOT match "command" (ASCII exact-match preserved)
        let hits = kb.search("com");
        assert!(hits.is_empty(), "'com' should not match keyword 'command'");

        // "command" should match
        let hits = kb.search("command");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_estimate_tokens_chinese() {
        // Pure Chinese: each char ~1.5 tokens
        let cn = "这是一段中文文本用于测试估算函数的准确性";
        let est = estimate_tokens(cn);
        // 17 Chinese chars * 1.5 ≈ 26 tokens
        assert!(est >= 20, "Chinese estimation should be >= 20, got {}", est);
        assert!(est <= 40, "Chinese estimation should be <= 40, got {}", est);
    }

    #[test]
    fn test_estimate_tokens_english() {
        // Pure ASCII: "hello world" = 11 chars, ~3 tokens
        let en = "hello world";
        let est = estimate_tokens(en);
        assert!(est >= 2 && est <= 5, "English estimation should be 2-5, got {}", est);
    }

    #[test]
    fn test_estimate_tokens_mixed() {
        // Mixed Chinese + English
        let mixed = "配置MCP服务器需要修改config文件";
        let est = estimate_tokens(mixed);
        // Should be between pure-Chinese and pure-English estimates
        assert!(est > 5, "Mixed estimation should be > 5, got {}", est);
        assert!(est < 50, "Mixed estimation should be < 50, got {}", est);
    }

    #[test]
    fn test_truncation_preserves_code_blocks() {
        let (tmp, kb) = create_test_kb(&[(
            "code.md",
            r#"---
title: "Code Example"
category: "test"
keywords: [code, example]
---
Some text before.

```rust
fn main() {
    println!("hello");
}
```

Some text after.
"#,
        )]);
        let _tmp = tmp;

        // Very tight budget should either show the full code block or skip it entirely
        let rendered = kb.render_for_query("code", 15);
        // If code block is included, it should be complete (have both ``` markers)
        let backtick_count = rendered.matches("```").count();
        assert!(
            backtick_count % 2 == 0,
            "Code blocks should be complete (even number of ``` markers), got {}",
            backtick_count
        );
    }
}
