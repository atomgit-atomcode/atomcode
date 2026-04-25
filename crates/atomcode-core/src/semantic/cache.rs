use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tree_sitter::{Parser, Tree};

use super::language::{Lang, LanguageRegistry};

/// Entry in the AST cache: parsed tree + file modification time.
struct CacheEntry {
    tree: Tree,
    modified: SystemTime,
    lang: Lang,
}

/// Cache of parsed ASTs keyed by file path.
/// Avoids re-parsing files that haven't changed.
pub struct ASTCache {
    entries: HashMap<PathBuf, CacheEntry>,
    parser: Parser,
}

impl ASTCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            parser: Parser::new(),
        }
    }

    /// Parse a file and return its AST tree + detected language.
    /// Uses cache if the file hasn't been modified since last parse.
    /// Parse a file and return its AST tree + detected language.
    /// Uses cache if the file hasn't been modified since last parse.
    pub fn get_tree(&mut self, path: &Path) -> Option<(Tree, Lang)> {
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        // Check cache validity
        if let Some(entry) = self.entries.get(&abs) {
            if entry.modified == modified {
                return Some((entry.tree.clone(), entry.lang));
            }
        }

        // Parse fresh
        let lang = LanguageRegistry::detect(path)?;
        self.parser.set_language(&lang.grammar()).ok()?;
        let source = std::fs::read_to_string(path).ok()?;
        let tree = self.parser.parse(&source, None)?;

        self.entries.insert(
            abs,
            CacheEntry {
                tree: tree.clone(),
                modified,
                lang,
            },
        );

        Some((tree, lang))
    }

    /// Parse source code directly (not from file). No caching.
    pub fn parse_source(&mut self, source: &str, lang: Lang) -> Option<Tree> {
        self.parser.set_language(&lang.grammar()).ok()?;
        self.parser.parse(source, None)
    }

    /// Invalidate cache for a specific file (e.g. after edit_file).
    pub fn invalidate(&mut self, path: &Path) {
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.entries.remove(&abs);
    }

    /// Clear entire cache.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}
