//! KnowledgeBase for the atomcode-guide subagent.
//!
//! Provides lazy-loading, keyword-indexed retrieval from Markdown files
//! with YAML frontmatter. All knowledge files live in `knowledge/` next
//! to this module.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use crate::i18n::{t, Msg};
use crate::locale::Locale;

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
    /// The locale used when loading this data.
    locale: Locale,
}

/// Thread-safe knowledge base that loads Markdown files from `knowledge/`.
///
/// The knowledge base automatically reloads when the UI language changes,
/// ensuring content matches the user's locale.
#[derive(Debug)]
pub struct KnowledgeBase {
    inner: RwLock<Option<KnowledgeInner>>,
    base_dir: PathBuf,
    /// If set, this KB is pinned to a specific locale and will not
    /// reload when the global locale changes. Used by `embedded_en()`
    /// to create a stable English KB for cross-language fallback.
    target_locale: Option<Locale>,
}

impl Clone for KnowledgeBase {
    fn clone(&self) -> Self {
        let inner = match self.inner.read() {
            Ok(guard) => {
                let data = guard.clone();
                RwLock::new(data)
            }
            Err(_) => RwLock::new(None),
        };
        Self {
            inner,
            base_dir: self.base_dir.clone(),
            target_locale: self.target_locale,
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
/// Returns true for common English words that carry no topical signal.
fn is_stop_word(word: &str) -> bool {
    matches!(
        word,
        "a" | "an" | "the" | "and" | "or" | "but" | "nor" | "so" | "yet" | "for"
        | "in" | "on" | "at" | "to" | "of" | "with" | "by" | "from" | "up"
        | "about" | "into" | "through" | "during" | "before" | "after" | "above"
        | "below" | "between" | "under" | "over" | "off" | "out"
        | "is" | "are" | "was" | "were" | "be" | "been" | "being"
        | "have" | "has" | "had" | "do" | "does" | "did"
        | "will" | "would" | "shall" | "should" | "can" | "could" | "may" | "might" | "must"
        | "what" | "which" | "who" | "whom" | "when" | "where" | "why" | "how"
        | "i" | "you" | "he" | "she" | "it" | "we" | "they"
        | "me" | "him" | "her" | "us" | "them"
        | "my" | "your" | "his" | "its" | "our" | "their"
        | "this" | "that" | "these" | "those" | "some" | "any" | "no" | "every" | "each"
        | "all" | "both" | "few" | "more" | "most" | "other" | "such"
        | "not" | "only" | "just" | "very" | "too" | "also" | "here" | "there" | "then" | "now"
    )
}

/// Check whether a keyword matches a query word.
///
/// Matching order: exact match → CJK substring → prefix (≥3 chars).
/// Prefix matching handles plurals (plugin ↔ plugins) and word-form
/// variations (config ↔ configure) without full stemming.
fn keyword_matches(keyword: &str, query_word: &str) -> bool {
    if keyword == query_word {
        return true;
    }
    // CJK substring matching
    if contains_cjk(query_word) && query_word.contains(keyword) {
        return true;
    }
    if contains_cjk(keyword) && keyword.contains(query_word) {
        return true;
    }
    // Prefix matching (min 3 chars to avoid false positives)
    let shorter = if keyword.len() < query_word.len() { keyword } else { query_word };
    let longer = if keyword.len() < query_word.len() { query_word } else { keyword };
    if shorter.len() >= 3 && longer.starts_with(shorter) {
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
        let inner = RwLock::new(Some(Self::load_embedded()));
        Self {
            inner,
            base_dir: PathBuf::new(),
            target_locale: None,
        }
    }

    /// Create a `KnowledgeBase` with a custom base directory (used in tests).
    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        Self {
            inner: RwLock::new(None),
            base_dir,
            target_locale: None,
        }
    }

    // -----------------------------------------------------------------
    // Embedded loading
    // -----------------------------------------------------------------

    fn load_embedded() -> KnowledgeInner {
        Self::load_embedded_for_locale(crate::i18n::current_locale())
    }

    /// Create an embedded KnowledgeBase loaded with English files.
    /// Cached via OnceLock so the knowledge files are parsed only once.
    #[allow(dead_code)]
    fn embedded_en() -> &'static Self {
        use std::sync::OnceLock;
        static KB: OnceLock<KnowledgeBase> = OnceLock::new();
        KB.get_or_init(|| {
            let inner = RwLock::new(Some(Self::load_embedded_for_locale(crate::locale::Locale::En)));
            KnowledgeBase {
                inner,
                base_dir: PathBuf::new(),
                target_locale: Some(crate::locale::Locale::En),
            }
        })
    }

    /// Create an embedded KnowledgeBase loaded with Chinese files.
    /// Cached via OnceLock so the knowledge files are parsed only once.
    #[allow(dead_code)]
    fn embedded_zh() -> &'static Self {
        use std::sync::OnceLock;
        static KB: OnceLock<KnowledgeBase> = OnceLock::new();
        KB.get_or_init(|| {
            let inner = RwLock::new(Some(Self::load_embedded_for_locale(crate::locale::Locale::ZhCn)));
            KnowledgeBase {
                inner,
                base_dir: PathBuf::new(),
                target_locale: Some(crate::locale::Locale::ZhCn),
            }
        })
    }

    fn load_embedded_for_locale(locale: crate::locale::Locale) -> KnowledgeInner {
        let is_english = matches!(locale, crate::locale::Locale::En);

        // Chinese knowledge files (default)
        let zh_files: &[(&str, &str)] = &[
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
            ("guide-usage.md", include_str!("knowledge/guide-usage.md")),
        ];

        // English knowledge files
        let en_files: &[(&str, &str)] = &[
            ("overview.en.md", include_str!("knowledge/overview.en.md")),
            ("features.en.md", include_str!("knowledge/features.en.md")),
            ("slash-commands.en.md", include_str!("knowledge/slash-commands.en.md")),
            ("mcp.en.md", include_str!("knowledge/mcp.en.md")),
            ("skills.en.md", include_str!("knowledge/skills.en.md")),
            ("config.en.md", include_str!("knowledge/config.en.md")),
            ("bg.en.md", include_str!("knowledge/bg.en.md")),
            ("context.en.md", include_str!("knowledge/context.en.md")),
            ("getting-started.en.md", include_str!("knowledge/getting-started.en.md")),
            ("memory.en.md", include_str!("knowledge/memory.en.md")),
            ("modes.en.md", include_str!("knowledge/modes.en.md")),
            ("sessions.en.md", include_str!("knowledge/sessions.en.md")),
            ("worktree.en.md", include_str!("knowledge/worktree.en.md")),
            ("troubleshooting.en.md", include_str!("knowledge/troubleshooting.en.md")),
            ("keybindings.en.md", include_str!("knowledge/keybindings.en.md")),
            ("doc-urls.en.md", include_str!("knowledge/doc-urls.en.md")),
            ("guide-usage.en.md", include_str!("knowledge/guide-usage.en.md")),
        ];

        // Select files based on locale
        let files = if is_english {
            en_files.to_vec()
        } else {
            zh_files.to_vec()
        };

        let mut entries = Vec::new();
        let mut keyword_index: HashMap<String, Vec<usize>> = HashMap::new();

        for (name, raw) in &files {
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

        tracing::debug!("KnowledgeBase: loaded {} entries (embedded, locale={:?})", entries.len(), locale);
        KnowledgeInner { entries, keyword_index, locale }
    }

    // -----------------------------------------------------------------
    // Disk-based loading (for tests and custom knowledge)
    // -----------------------------------------------------------------

    /// Load (or reload) all knowledge entries from disk.
    fn load(&self) -> KnowledgeInner {
        let locale = crate::i18n::current_locale();
        let mut entries = Vec::new();
        let mut keyword_index: HashMap<String, Vec<usize>> = HashMap::new();

        let dir = match std::fs::read_dir(&self.base_dir) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    "KnowledgeBase: cannot read directory {}: {}",
                    self.base_dir.display(),
                    e
                );
                return KnowledgeInner {
                    entries,
                    keyword_index,
                    locale,
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
            "KnowledgeBase: loaded {} entries from {} (locale={:?})",
            entries.len(),
            self.base_dir.display(),
            locale
        );
        KnowledgeInner { entries, keyword_index, locale }
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

    /// Get-or-load the inner data structure, reloading if locale changed.
    ///
    /// If `target_locale` is set, this KB is pinned to that locale and
    /// will never reload due to global locale changes.
    fn get_or_load(&self) -> KnowledgeInner {
        // If pinned to a specific locale, never reload
        if let Some(pinned) = self.target_locale {
            let guard = self.inner.read().unwrap();
            if let Some(ref inner) = *guard {
                if inner.locale == pinned {
                    return inner.clone();
                }
            }
            // Pinned but not loaded yet — load with pinned locale (shouldn't
            // happen for embedded KBs, but handle it gracefully)
            drop(guard);
            let new_inner = Self::load_embedded_for_locale(pinned);
            let mut guard = self.inner.write().unwrap();
            *guard = Some(new_inner.clone());
            return new_inner;
        }

        let current_locale = crate::i18n::current_locale();

        // Check if we need to reload due to locale change
        let needs_reload = {
            match self.inner.read() {
                Ok(guard) => match &*guard {
                    Some(inner) => inner.locale != current_locale,
                    None => true,
                },
                Err(_) => true,
            }
        };

        if needs_reload {
            let new_inner = if self.base_dir.as_os_str().is_empty() {
                Self::load_embedded()
            } else {
                self.load()
            };
            let mut guard = self.inner.write().unwrap();
            *guard = Some(new_inner.clone());
            new_inner
        } else {
            self.inner.read().unwrap().clone().unwrap()
        }
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
            .filter(|s| !is_stop_word(s))
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

        // Sort by relevance: entries with fewer total keywords (more
        // specific) rank higher. This prevents the overview entry
        // (50+ keywords) from always appearing before specialized
        // entries that better match the query.
        let _word_count = words.len();
        indices.sort_by(|a, b| {
            let a_kw = inner.entries.get(*a).map(|e| e.keywords.len()).unwrap_or(0);
            let b_kw = inner.entries.get(*b).map(|e| e.keywords.len()).unwrap_or(0);
            // Fewer keywords = more specific = higher rank
            a_kw.cmp(&b_kw).then(a.cmp(b))
        });
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
            .filter(|s| !is_stop_word(s))
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
        pairs.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| {
                    let a_kw = inner.entries.get(a.0).map(|e| e.keywords.len()).unwrap_or(0);
                    let b_kw = inner.entries.get(b.0).map(|e| e.keywords.len()).unwrap_or(0);
                    a_kw.cmp(&b_kw)
                })
                .then(a.0.cmp(&b.0))
        });
        pairs.into_iter().map(|(i, _)| i).collect()
    }

    pub fn render_for_query(&self, query: &str, max_tokens: usize) -> String {
        // Always use self (locale-based KB), no character-based detection.
        let inner = self.get_or_load();
        let hits = self.search(query);
        let hits = if hits.is_empty() { self.search_or(query) } else { hits };

        // Cross-locale fallback: when the locale KB returns nothing, try
        // the other locale's KB. Handles cases like a Chinese-locale user
        // typing English "skill" which only exists as a keyword in the
        // English KB.
        let (hits, entries) = if hits.is_empty() && self.base_dir.as_os_str().is_empty() {
            let other = Self::other_locale_kb();
            let other_inner = other.get_or_load();
            let other_hits = other.search(query);
            let other_hits = if other_hits.is_empty() { other.search_or(query) } else { other_hits };
            tracing::debug!(query, hits = other_hits.len(), "locale KB miss → cross-locale fallback");
            (other_hits, other_inner.entries)
        } else {
            (hits, inner.entries)
        };

        Self::render_hits(query, &hits, &entries, max_tokens)
    }

    fn other_locale_kb() -> &'static Self {
        match crate::i18n::current_locale() {
            crate::locale::Locale::En => Self::embedded_zh(),
            _ => Self::embedded_en(),
        }
    }

    fn render_hits(query: &str, hits: &[usize], entries: &[KnowledgeEntry], max_tokens: usize) -> String {
        tracing::debug!(query, hits = hits.len(), "knowledge search");

        if hits.is_empty() {
            return t(Msg::GuideKbNoResults { query }).into_owned();
        }

        let mut out = t(Msg::GuideKbRelatedHeader).into_owned();
        for idx in hits {
            if *idx >= entries.len() {
                continue;
            }
            let entry = &entries[*idx];
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
                        out.push_str(&t(Msg::GuideKbTruncated));
                    }
                } else {
                    out.push_str(&t(Msg::GuideKbTruncated));
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
        // Should contain documentation URL (language-independent)
        assert!(
            rendered.contains("atomcode.atomgit.com/docs/"),
            "should render entry overview on miss with doc URL"
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
        // Should contain documentation URL (language-independent)
        assert!(rendered.contains("atomcode.atomgit.com/docs/"));
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
        // Use language-independent check: truncation marker contains "..." in both locales
        assert!(
            !(has_a && has_b) || rendered.contains("..."),
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
        // "ion" should NOT match "configuration" with prefix matching
        // (it's a suffix, not a prefix at all).
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

        let hits = kb.search("ion");
        assert!(hits.is_empty(), "'ion' should not match keyword 'configuration' (suffix, not prefix)");

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

        // "con" should match "con" exactly
        let hits = kb.search("con");
        assert_eq!(hits.len(), 1, "'con' should match keyword 'con'");
        assert_eq!(kb.get_or_load().entries[hits[0]].title, "Test");

        // "test" should match "test"
        let hits = kb.search("test");
        assert_eq!(hits.len(), 1);

        // "testing" now matches "test" via prefix matching (intentional:
        // handles plurals and word-form variations like plugin/plugins).
        let hits = kb.search("testing");
        assert_eq!(hits.len(), 1, "'testing' should match keyword 'test' via prefix");
    }

    #[test]
    fn test_search_prefix_match_singular_plural() {
        // Prefix matching should handle plural/singular forms
        let (tmp, kb) = create_test_kb(&[(
            "skills.md",
            r#"---
title: "Skills"
category: "extensions"
keywords: [skill, plugin]
---
Skills and plugins.
"#,
        )]);
        let _tmp = tmp;

        // "skills" should match "skill" via prefix
        let hits = kb.search("skills");
        assert_eq!(hits.len(), 1, "'skills' should prefix-match keyword 'skill'");
        // "plugins" should match "plugin" via prefix
        let hits = kb.search("plugins");
        assert_eq!(hits.len(), 1, "'plugins' should prefix-match keyword 'plugin'");
        // "skill" still matches "skill" exactly
        let hits = kb.search("skill");
        assert_eq!(hits.len(), 1);
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
        // Prefix matching applies to ASCII too (handles plural/singular etc.).
        // But non-prefix substrings like "mmand" should NOT match "command".
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

        // "mmand" is NOT a prefix of "command" → no match
        let hits = kb.search("mmand");
        assert!(hits.is_empty(), "'mmand' should not match keyword 'command' (not a prefix)");

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
    fn test_embedded_ascii_query_selects_english_kb() {
        // No CJK in query → use self (locale). Default locale=En → English KB.
        let _guard = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::locale::Locale::En);
        let kb = KnowledgeBase::embedded();
        let rendered = kb.render_for_query("Getting started", 2000);

        // Should contain English knowledge content
        assert!(
            rendered.contains("Getting Started") || rendered.contains("Installation"),
            "No-CJK query with En locale should return English KB content, got: {}",
            &rendered[..rendered.len().min(200)]
        );
        // Should NOT contain Chinese-only knowledge content
        assert!(
            !rendered.contains("安装完成后"),
            "No-CJK query with En locale should NOT return Chinese KB content"
        );
    }

    #[test]
    fn test_zh_locale_hits_chinese_kb() {
        // ZhCn locale + query that matches Chinese KB keywords → Chinese content.
        let _guard = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::locale::Locale::ZhCn);
        assert_eq!(crate::i18n::current_locale(), crate::locale::Locale::ZhCn);

        let kb = KnowledgeBase::embedded();
        let rendered = kb.render_for_query("安装", 2000);

        // Should contain Chinese knowledge content
        assert!(
            rendered.contains("安装") || rendered.contains("启动") || rendered.contains("配置"),
            "ZhCn locale with matching Chinese query should return Chinese KB content, got: {}",
            &rendered[..rendered.len().min(300)]
        );
        // i18n header should be Chinese
        assert!(
            rendered.starts_with("## 相关知识"),
            "Header should be Chinese, got: {}",
            &rendered[..rendered.len().min(50)]
        );
    }

    #[test]
    fn test_cross_locale_fallback_zh_locale_english_query() {
        // ZhCn locale + English-only query → Chinese KB misses → English KB fallback.
        let _guard = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::locale::Locale::ZhCn);
        assert_eq!(crate::i18n::current_locale(), crate::locale::Locale::ZhCn);

        let kb = KnowledgeBase::embedded();
        let rendered = kb.render_for_query("Getting started", 2000);

        // Cross-locale fallback → English KB content
        assert!(
            rendered.contains("Getting Started") || rendered.contains("Installation"),
            "ZhCn locale + English query should fall back to English KB, got: {}",
            &rendered[..rendered.len().min(300)]
        );
    }

    #[test]
    fn test_zh_locale_returns_chinese_kb() {
        // ZhCn locale → Chinese KB, regardless of query characters.
        let _guard = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::locale::Locale::ZhCn);
        let kb = KnowledgeBase::embedded();
        let rendered = kb.render_for_query("入门指南", 2000);

        // Should contain Chinese knowledge content
        assert!(
            rendered.contains("安装") || rendered.contains("启动") || rendered.contains("配置"),
            "ZhCn locale should return Chinese KB content, got: {}",
            &rendered[..rendered.len().min(200)]
        );
    }

    #[test]
    fn test_embedded_en_direct() {
        // Directly test embedded_en returns English content
        let kb = KnowledgeBase::embedded_en();
        let inner = kb.get_or_load();

        // Should have English entries
        let titles: Vec<&str> = inner.entries.iter().map(|e| e.title.as_str()).collect();
        assert!(
            titles.iter().any(|t| t.contains("Getting Started")),
            "embedded_en should contain 'Getting Started' entry, got: {:?}",
            titles
        );

        // English content should NOT have Chinese body text
        for entry in &inner.entries {
            assert!(
                !entry.content.contains("安装完成后"),
                "English KB entry '{}' should not contain Chinese body",
                entry.title
            );
        }
    }

    #[test]
    fn test_embedded_zh_direct() {
        // Directly test embedded_zh returns Chinese content
        let kb = KnowledgeBase::embedded_zh();
        let inner = kb.get_or_load();

        // Should have Chinese entries
        let titles: Vec<&str> = inner.entries.iter().map(|e| e.title.as_str()).collect();
        assert!(
            titles.iter().any(|t| t.contains("入门")),
            "embedded_zh should contain Chinese '入门' entry, got: {:?}",
            titles
        );
    }

    /// End-to-end test: simulate the full /guide Getting started flow
    /// with locale=En. Verifies that:
    /// 1. System prompt is the English version
    /// 2. Knowledge content is English (not Chinese)
    /// 3. The i18n header matches the locale
    #[test]
    fn test_e2e_guide_getting_started_with_en_locale() {
        use crate::agent::sub_agent::registry::SubAgentRegistry;

        let _guard = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::locale::Locale::En);
        assert_eq!(crate::i18n::current_locale(), crate::locale::Locale::En);

        // Step 1: Register the guide subagent (same as mod.rs::register)
        let registry = SubAgentRegistry::new();
        crate::agent::guide::register(&registry).unwrap();

        // Step 2: Find the guide definition (same as mod.rs:1558-1573)
        let def = registry.find("atomcode-guide").expect("guide should be registered");

        // Step 3: Update system prompt based on current locale (same as mod.rs:1589-1592)
        let mut def = def;
        def.system_prompt = crate::agent::guide::get_guide_system_prompt();

        // Verify system prompt is English
        assert!(
            def.system_prompt.contains("Answer in the same language as the user's question"),
            "System prompt should be English with 'Answer in the same language', got: {}",
            &def.system_prompt[..def.system_prompt.len().min(200)]
        );
        assert!(
            !def.system_prompt.contains("使用中文回答"),
            "System prompt should NOT contain old '使用中文回答'"
        );

        // Step 4: Render knowledge for ASCII query (same as runner.rs:126-128)
        let kb = def.knowledge.as_ref().expect("guide should have knowledge");
        let kb_text = kb.render_for_query("Getting started", def.max_knowledge_tokens);

        // Verify knowledge content is English
        assert!(
            kb_text.contains("Getting Started") || kb_text.contains("Installation"),
            "Knowledge should contain English content for ASCII query, got: {}",
            &kb_text[..kb_text.len().min(300)]
        );
        assert!(
            !kb_text.contains("安装完成后"),
            "Knowledge should NOT contain Chinese body for ASCII query"
        );

        // Verify i18n header is English (locale=En)
        assert!(
            kb_text.starts_with("## Related Knowledge"),
            "KB header should be English '## Related Knowledge' with En locale, got: {}",
            &kb_text[..kb_text.len().min(50)]
        );
    }

    /// Same end-to-end test but with locale=ZhCn.
    /// Verifies that with Chinese locale, no-CJK query returns Chinese knowledge.
    #[test]
    fn test_e2e_guide_getting_started_with_zh_locale() {
        use crate::agent::sub_agent::registry::SubAgentRegistry;

        let _guard = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::locale::Locale::ZhCn);
        assert_eq!(crate::i18n::current_locale(), crate::locale::Locale::ZhCn);

        // Register and find guide
        let registry = SubAgentRegistry::new();
        crate::agent::guide::register(&registry).unwrap();
        let mut def = registry.find("atomcode-guide").expect("guide should be registered");

        // Update system prompt (simulates mod.rs:1589-1592)
        def.system_prompt = crate::agent::guide::get_guide_system_prompt();

        // System prompt should be Chinese version with "same language" instruction
        assert!(
            def.system_prompt.contains("使用与用户提问相同的语言回答"),
            "ZhCn system prompt should say '使用与用户提问相同的语言回答', got: {}",
            &def.system_prompt[..def.system_prompt.len().min(200)]
        );

        // Render knowledge for English query with ZhCn locale
        // Chinese KB misses → cross-locale fallback → English KB content
        let kb = def.knowledge.as_ref().expect("guide should have knowledge");
        let kb_text = kb.render_for_query("Getting started", def.max_knowledge_tokens);

        // Content is English (from cross-locale fallback)
        assert!(
            kb_text.contains("Getting Started") || kb_text.contains("Installation"),
            "ZhCn locale + English query should fall back to English KB, got: {}",
            &kb_text[..kb_text.len().min(300)]
        );

        // i18n header follows global locale → Chinese
        assert!(
            kb_text.starts_with("## 相关知识"),
            "KB header should be Chinese with ZhCn locale, got: {}",
            &kb_text[..kb_text.len().min(50)]
        );
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
