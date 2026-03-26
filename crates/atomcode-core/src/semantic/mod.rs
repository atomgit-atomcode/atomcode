pub mod cache;
pub mod language;

use std::path::Path;

use tree_sitter::{Query, QueryCursor, StreamingIterator};

use cache::ASTCache;
use language::{Lang, LanguageRegistry};

/// A symbol extracted from source code.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// Symbol name (function name, class name, etc.)
    pub name: String,
    /// Start line (1-indexed)
    pub start_line: usize,
    /// End line (1-indexed)
    pub end_line: usize,
    /// Start byte offset in source
    pub start_byte: usize,
    /// End byte offset in source
    pub end_byte: usize,
    /// The node kind from tree-sitter (e.g. "function_item", "class_definition")
    pub kind: String,
}

/// Semantic code searcher: fuses Ripgrep speed with Tree-sitter precision.
pub struct SemanticSearcher {
    cache: ASTCache,
}

impl SemanticSearcher {
    pub fn new() -> Self {
        Self {
            cache: ASTCache::new(),
        }
    }

    /// List all top-level symbols in a file.
    /// Returns function/class/struct signatures with line ranges.
    pub fn list_symbols(&mut self, path: &Path) -> Option<Vec<Symbol>> {
        let source = std::fs::read_to_string(path).ok()?;

        let lang = LanguageRegistry::detect(path);

        if let Some(lang) = lang {
            self.list_symbols_treesitter(path, &source, lang)
        } else {
            Some(self.list_symbols_indent(&source))
        }
    }

    /// Extract a specific symbol (function/class) by name from a file.
    /// Returns the complete source text of that symbol.
    pub fn extract_symbol(&mut self, path: &Path, symbol_name: &str) -> Option<SymbolSlice> {
        let source = std::fs::read_to_string(path).ok()?;
        let lang = LanguageRegistry::detect(path)?;
        let symbols = self.list_symbols_treesitter(path, &source, lang)?;

        // Find the symbol with matching name
        let sym = symbols.iter().find(|s| s.name == symbol_name)?;
        let text = source[sym.start_byte..sym.end_byte].to_string();

        Some(SymbolSlice {
            name: sym.name.clone(),
            kind: sym.kind.clone(),
            start_line: sym.start_line,
            end_line: sym.end_line,
            start_byte: sym.start_byte,
            end_byte: sym.end_byte,
            text,
        })
    }

    /// Generate a skeleton of a file: signatures only, bodies replaced with { ... }.
    pub fn skeleton(&mut self, path: &Path) -> Option<String> {
        let source = std::fs::read_to_string(path).ok()?;
        let lang = LanguageRegistry::detect(path);

        if let Some(lang) = lang {
            self.skeleton_treesitter(path, &source, lang)
        } else {
            Some(self.skeleton_indent(&source))
        }
    }

    /// Invalidate cache for a file (call after edit_file).
    pub fn invalidate(&mut self, path: &Path) {
        self.cache.invalidate(path);
    }

    // ── Tree-sitter implementation ──

    fn list_symbols_treesitter(
        &mut self,
        path: &Path,
        source: &str,
        lang: Lang,
    ) -> Option<Vec<Symbol>> {
        // Vue/Svelte SFC: extract <script> section, parse as TypeScript, adjust offsets.
        if lang == Lang::Vue {
            return self.list_symbols_vue(path, source);
        }

        let tree = self.cache.parse_source(source, lang)?;
        let query_src = lang.symbols_query();
        let grammar = lang.grammar();
        let query = Query::new(&grammar, query_src).ok()?;

        let def_idx = query.capture_index_for_name("definition")?;
        let name_idx = query.capture_index_for_name("name")?;

        let mut cursor = QueryCursor::new();

        let mut symbols = Vec::new();
        let mut seen_ranges: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();

        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        loop {
            matches.advance();
            let m = match matches.get() {
                Some(m) => m,
                None => break,
            };

            let mut sym_name = None;
            let mut def_start = 0usize;
            let mut def_end = 0usize;
            let mut def_start_row = 0usize;
            let mut def_end_row = 0usize;
            let mut def_kind = "";
            let mut has_def = false;

            for capture in m.captures {
                if capture.index == name_idx {
                    sym_name = Some(
                        source[capture.node.start_byte()..capture.node.end_byte()].to_string(),
                    );
                }
                if capture.index == def_idx {
                    def_start = capture.node.start_byte();
                    def_end = capture.node.end_byte();
                    def_start_row = capture.node.start_position().row;
                    def_end_row = capture.node.end_position().row;
                    def_kind = capture.node.kind();
                    has_def = true;
                }
            }

            if let (Some(name), true) = (sym_name, has_def) {
                let range = (def_start, def_end);
                if seen_ranges.contains(&range) {
                    continue;
                }
                seen_ranges.insert(range);

                symbols.push(Symbol {
                    name,
                    start_line: def_start_row + 1,
                    end_line: def_end_row + 1,
                    start_byte: def_start,
                    end_byte: def_end,
                    kind: def_kind.to_string(),
                });
            }
        }

        Some(symbols)
    }

    fn skeleton_treesitter(
        &mut self,
        path: &Path,
        source: &str,
        lang: Lang,
    ) -> Option<String> {
        let symbols = self.list_symbols_treesitter(path, source, lang)?;
        let lines: Vec<&str> = source.lines().collect();
        let mut out = String::new();

        // Collect import/use lines at the top
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("use ")
                || trimmed.starts_with("import ")
                || trimmed.starts_with("from ")
                || trimmed.starts_with("#include")
                || trimmed.starts_with("package ")
                || trimmed.starts_with("require")
            {
                out.push_str(&format!("{:4}| {}\n", i + 1, line));
            }
        }

        if !out.is_empty() {
            out.push('\n');
        }

        for sym in &symbols {
            // Get the first line (signature) of the symbol
            let sig_line = if sym.start_line <= lines.len() {
                lines[sym.start_line - 1]
            } else {
                &sym.name
            };

            let line_range = format!("L{}-{}", sym.start_line, sym.end_line);
            let body_lines = sym.end_line - sym.start_line + 1;

            out.push_str(&format!(
                "{:4}| {}  {{ ... }}  // {} ({} lines)\n",
                sym.start_line, sig_line.trim_end(), line_range, body_lines
            ));
        }

        Some(out)
    }

    // ── Vue/Svelte SFC support ──

    /// Extract <script> section from a Vue/Svelte SFC, parse as TypeScript.
    fn extract_script_section(source: &str) -> Option<(String, usize, usize)> {
        // Find <script...> opening tag
        let script_start = source.find("<script")?;
        let tag_end = source[script_start..].find('>')? + script_start + 1;
        // Find </script> closing tag
        let script_end = source[tag_end..].find("</script>")? + tag_end;
        let script_content = &source[tag_end..script_end];

        // Calculate line offset: how many lines before the script content
        let line_offset = source[..tag_end].lines().count();
        let byte_offset = tag_end;

        Some((script_content.to_string(), line_offset, byte_offset))
    }

    fn list_symbols_vue(&mut self, _path: &Path, source: &str) -> Option<Vec<Symbol>> {
        let (script, line_offset, byte_offset) = Self::extract_script_section(source)?;
        let tree = self.cache.parse_source(&script, Lang::Vue)?;
        let query_src = Lang::Vue.symbols_query();
        let grammar = Lang::Vue.grammar();
        let query = Query::new(&grammar, query_src).ok()?;

        let def_idx = query.capture_index_for_name("definition")?;
        let name_idx = query.capture_index_for_name("name")?;

        let mut cursor = QueryCursor::new();
        let mut symbols = Vec::new();
        let mut seen_ranges: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();

        let mut matches = cursor.matches(&query, tree.root_node(), script.as_bytes());
        loop {
            matches.advance();
            let m = match matches.get() {
                Some(m) => m,
                None => break,
            };

            let mut sym_name = None;
            let mut def_start = 0usize;
            let mut def_end = 0usize;
            let mut def_start_row = 0usize;
            let mut def_end_row = 0usize;
            let mut def_kind = "";
            let mut has_def = false;

            for capture in m.captures {
                if capture.index == name_idx {
                    sym_name = Some(
                        script[capture.node.start_byte()..capture.node.end_byte()].to_string(),
                    );
                }
                if capture.index == def_idx {
                    def_start = capture.node.start_byte();
                    def_end = capture.node.end_byte();
                    def_start_row = capture.node.start_position().row;
                    def_end_row = capture.node.end_position().row;
                    def_kind = capture.node.kind();
                    has_def = true;
                }
            }

            if let (Some(name), true) = (sym_name, has_def) {
                let range = (def_start, def_end);
                if seen_ranges.contains(&range) { continue; }
                seen_ranges.insert(range);

                symbols.push(Symbol {
                    name,
                    // Adjust line/byte offsets to be relative to the full .vue file
                    start_line: def_start_row + line_offset,
                    end_line: def_end_row + line_offset,
                    start_byte: def_start + byte_offset,
                    end_byte: def_end + byte_offset,
                    kind: def_kind.to_string(),
                });
            }
        }

        // Also add <template> and <style> as pseudo-symbols for skeleton
        let lines: Vec<&str> = source.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("<template") || trimmed.starts_with("<style") {
                let tag = if trimmed.starts_with("<template") { "template" } else { "style" };
                let close_tag = format!("</{}>", tag);
                let end_line = lines[i..].iter().position(|l| l.trim().starts_with(&close_tag))
                    .map(|p| i + p + 1)
                    .unwrap_or(lines.len());
                let start_byte = lines[..i].iter().map(|l| l.len() + 1).sum::<usize>();
                let end_byte = lines[..end_line].iter().map(|l| l.len() + 1).sum::<usize>();
                symbols.push(Symbol {
                    name: format!("<{}>", tag),
                    start_line: i + 1,
                    end_line,
                    start_byte,
                    end_byte,
                    kind: "sfc_section".to_string(),
                });
            }
        }

        symbols.sort_by_key(|s| s.start_line);
        Some(symbols)
    }

    // ── Indent-based fallback for unsupported languages ──

    fn list_symbols_indent(&self, source: &str) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        let lines: Vec<&str> = source.lines().collect();
        let mut i = 0;

        while i < lines.len() {
            let line = lines[i];
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
                i += 1;
                continue;
            }

            // A line at indent level 0 that looks like a definition
            let indent = line.len() - line.trim_start().len();
            if indent == 0 && !trimmed.starts_with('}') && !trimmed.starts_with(')') {
                let is_def = trimmed.starts_with("fn ")
                    || trimmed.starts_with("pub ")
                    || trimmed.starts_with("def ")
                    || trimmed.starts_with("class ")
                    || trimmed.starts_with("function ")
                    || trimmed.starts_with("func ")
                    || trimmed.starts_with("type ")
                    || trimmed.starts_with("struct ")
                    || trimmed.starts_with("enum ")
                    || trimmed.starts_with("interface ")
                    || trimmed.starts_with("impl ")
                    || trimmed.starts_with("trait ")
                    || trimmed.starts_with("const ")
                    || trimmed.starts_with("export ")
                    || trimmed.starts_with("async ")
                    || trimmed.starts_with("public ")
                    || trimmed.starts_with("private ")
                    || trimmed.starts_with("protected ");

                if is_def {
                    // Find the end: next line at indent 0
                    let start = i;
                    let mut end = i + 1;
                    while end < lines.len() {
                        let next = lines[end];
                        let next_trimmed = next.trim();
                        if next_trimmed.is_empty() {
                            end += 1;
                            continue;
                        }
                        let next_indent = next.len() - next.trim_start().len();
                        if next_indent == 0 && !next_trimmed.starts_with('}') {
                            break;
                        }
                        end += 1;
                    }
                    // Include closing brace
                    if end < lines.len() && lines[end].trim() == "}" {
                        end += 1;
                    }

                    // Extract name: first word after keyword
                    let name = extract_indent_name(trimmed);

                    let start_byte = lines[..start].iter().map(|l| l.len() + 1).sum::<usize>();
                    let end_byte = lines[..end].iter().map(|l| l.len() + 1).sum::<usize>();

                    symbols.push(Symbol {
                        name,
                        start_line: start + 1,
                        end_line: end,
                        start_byte,
                        end_byte,
                        kind: "indent_block".to_string(),
                    });

                    i = end;
                    continue;
                }
            }
            i += 1;
        }

        symbols
    }

    fn skeleton_indent(&self, source: &str) -> String {
        let symbols = self.list_symbols_indent(source);
        let lines: Vec<&str> = source.lines().collect();
        let mut out = String::new();

        for sym in &symbols {
            if sym.start_line <= lines.len() {
                let sig = lines[sym.start_line - 1];
                let body_lines = sym.end_line - sym.start_line + 1;
                out.push_str(&format!(
                    "{:4}| {}  // L{}-{} ({} lines)\n",
                    sym.start_line,
                    sig.trim_end(),
                    sym.start_line,
                    sym.end_line,
                    body_lines
                ));
            }
        }

        out
    }
}

/// A precise slice of source code for a single symbol.
#[derive(Debug, Clone)]
pub struct SymbolSlice {
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
}

/// Extract a plausible name from an indent-level-0 definition line.
fn extract_indent_name(line: &str) -> String {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    // Skip keywords, take the first identifier-like token
    for (i, tok) in tokens.iter().enumerate() {
        if i == 0 {
            continue; // skip the keyword itself
        }
        // Strip common suffixes: (, {, :, <
        let clean = tok.trim_start_matches('*')
            .trim_end_matches(|c: char| "({:<".contains(c));
        if !clean.is_empty() && clean.chars().next().map_or(false, |c| c.is_alphabetic() || c == '_') {
            return clean.to_string();
        }
    }
    tokens.first().unwrap_or(&"unknown").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_language_detection() {
        assert_eq!(LanguageRegistry::detect(Path::new("foo.rs")), Some(Lang::Rust));
        assert_eq!(LanguageRegistry::detect(Path::new("bar.py")), Some(Lang::Python));
        assert_eq!(LanguageRegistry::detect(Path::new("baz.js")), Some(Lang::JavaScript));
        assert_eq!(LanguageRegistry::detect(Path::new("qux.ts")), Some(Lang::TypeScript));
        assert_eq!(LanguageRegistry::detect(Path::new("main.go")), Some(Lang::Go));
        assert_eq!(LanguageRegistry::detect(Path::new("App.java")), Some(Lang::Java));
        assert_eq!(LanguageRegistry::detect(Path::new("main.c")), Some(Lang::C));
        assert_eq!(LanguageRegistry::detect(Path::new("main.cpp")), Some(Lang::Cpp));
        assert_eq!(LanguageRegistry::detect(Path::new("readme.md")), None);
    }

    #[test]
    fn test_list_symbols_rust() {
        let mut searcher = SemanticSearcher::new();
        let source = r#"
pub fn hello() {
    println!("hello");
}

pub struct Point {
    x: f64,
    y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}
"#;
        let mut tmp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let symbols = searcher.list_symbols(tmp.path()).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello"), "symbols: {:?}", names);
        assert!(names.contains(&"Point"), "symbols: {:?}", names);
    }

    #[test]
    fn test_extract_symbol_rust() {
        let mut searcher = SemanticSearcher::new();
        let source = r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn sub(a: i32, b: i32) -> i32 {
    a - b
}
"#;
        let mut tmp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let slice = searcher.extract_symbol(tmp.path(), "add").unwrap();
        assert!(slice.text.contains("a + b"), "text: {}", slice.text);
        assert!(!slice.text.contains("a - b"), "should not contain sub");
    }

    #[test]
    fn test_skeleton_rust() {
        let mut searcher = SemanticSearcher::new();
        let source = r#"use std::io;

pub fn hello() {
    println!("hello");
}

pub fn world() {
    println!("world");
}
"#;
        let mut tmp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let skel = searcher.skeleton(tmp.path()).unwrap();
        assert!(skel.contains("hello"), "skeleton: {}", skel);
        assert!(skel.contains("world"), "skeleton: {}", skel);
        assert!(skel.contains("use std::io"), "skeleton: {}", skel);
    }

    #[test]
    fn test_list_symbols_python() {
        let mut searcher = SemanticSearcher::new();
        let source = r#"
def greet(name):
    print(f"hello {name}")

class Calculator:
    def add(self, a, b):
        return a + b
"#;
        let mut tmp = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let symbols = searcher.list_symbols(tmp.path()).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"greet"), "symbols: {:?}", names);
        assert!(names.contains(&"Calculator"), "symbols: {:?}", names);
    }

    #[test]
    fn test_indent_fallback() {
        let mut searcher = SemanticSearcher::new();
        let source = r#"
def hello():
    print("hello")

def world():
    print("world")
"#;
        // Use .txt extension so no grammar is detected
        let mut tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let symbols = searcher.list_symbols(tmp.path()).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"hello()"), "indent fallback symbols: {:?}", names);
    }
}
