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

impl Symbol {
    /// Check if this symbol has a Chinese name.
    pub fn is_chinese(&self) -> bool {
        contains_chinese(&self.name)
    }

    /// Check if this symbol looks like a Pinyin variable name.
    pub fn is_pinyin(&self) -> bool {
        is_pinyin_identifier(&self.name)
    }

    /// Check if this symbol is likely Chinese-related (Chinese name or Pinyin).
    pub fn is_chinese_related(&self) -> bool {
        self.is_chinese() || self.is_pinyin()
    }
}

/// Check if a character is a Chinese character (CJK Unified Ideographs).
fn is_chinese(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}' |  // CJK Unified Ideographs
        '\u{3400}'..='\u{4DBF}' |  // CJK Unified Ideographs Extension A
        '\u{20000}'..='\u{2A6DF}' | // CJK Unified Ideographs Extension B
        '\u{F900}'..='\u{FAFF}' |  // CJK Compatibility Ideographs
        '\u{2F800}'..='\u{2FA1F}'  // CJK Compatibility Ideographs Supplement
    )
}

/// Check if a string contains Chinese characters.
fn contains_chinese(s: &str) -> bool {
    s.chars().any(is_chinese)
}

/// Check if a string looks like a Pinyin variable name (e.g., yonghuMing, dingdanList).
fn is_pinyin_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }

    // Must start with alphabetic character
    let first = s.chars().next().unwrap();
    if !first.is_ascii_alphabetic() {
        return false;
    }

    // Common Pinyin syllables found in Chinese variable names (deduplicated, sorted).
    // Derived from the original curated list — excludes syllables that overlap with
    // common English fragments (e.g., "ge", "tu", "se", "he", "de", "le") to avoid
    // false positives on English identifiers like "getUser".
    let pinyin_syllables = [
        "ba", "bai", "bei", "biao", "chang", "chu", "da", "dan", "di", "ding",
        "dong", "duan", "duo", "er", "fen", "gao", "guo", "hao", "hou", "hu",
        "huai", "ji", "jian", "jiu", "kuai", "kuan", "leng", "li", "lie", "lu",
        "man", "miao", "ming", "mu", "nan", "nei", "nian", "qi", "qian", "re",
        "ren", "ri", "san", "shang", "shao", "shen", "shi", "shu", "si", "tian",
        "wai", "wan", "wen", "wu", "xi", "xia", "xiao", "xin", "xing", "yi",
        "yong", "you", "yue", "zhai", "zhong", "zuo",
    ];

    let lower = s.to_lowercase();
    let remaining_str = lower.as_str();

    // Greedy longest-match: try longest syllables first to avoid short syllables
    // consuming characters that belong to longer ones (e.g., "xi" eating into "xiang").
    let mut pos = 0usize;
    let mut consumed_count = 0usize;
    let mut syllable_count = 0usize;

    while pos < remaining_str.len() {
        let mut matched_len = 0usize;
        for len in (1..=5.min(remaining_str.len() - pos)).rev() {
            let candidate = &remaining_str[pos..pos + len];
            if pinyin_syllables.binary_search(&candidate).is_ok() {
                matched_len = len;
                break;
            }
        }
        if matched_len > 0 {
            pos += matched_len;
            consumed_count += matched_len;
            syllable_count += 1;
        } else {
            break;
        }
    }

    // Require: (1) at least 2 syllable matches, AND (2) 80% coverage.
    // This prevents false positives on English words like "getUser" which
    // would match zero syllables from the restricted list.
    syllable_count >= 2 && consumed_count as f64 / lower.len() as f64 > 0.8
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
            let mut symbols = self.list_symbols_treesitter(path, &source, lang)?;

            // Vue SFC: also parse <template> section with HTML parser
            if lang.is_vue() {
                if let Some(html_symbols) = self.list_vue_template_symbols(&source) {
                    symbols.extend(html_symbols);
                }
            }

            Some(symbols)
        } else {
            Some(self.list_symbols_indent(&source, path))
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
            Some(self.skeleton_indent(&source, path))
        }
    }

    /// Invalidate cache for a file (call after edit_file).
    pub fn invalidate(&mut self, path: &Path) {
        self.cache.invalidate(path);
    }

    /// Count ERROR nodes in source code. Language-agnostic.
    /// Returns (error_count, first few error line numbers).
    pub fn count_syntax_errors(&mut self, source: &str, path: &Path) -> (usize, Vec<usize>) {
        let lang = match language::LanguageRegistry::detect(path) {
            Some(l) => l,
            None => return (0, vec![]),
        };
        let tree = match self.cache.parse_source(source, lang) {
            Some(t) => t,
            None => return (0, vec![]),
        };

        let mut errors = Vec::new();
        Self::collect_errors(tree.root_node(), &mut errors);
        let count = errors.len();
        errors.truncate(5); // Only report first 5
        (count, errors)
    }

    fn collect_errors(node: tree_sitter::Node, errors: &mut Vec<usize>) {
        if node.is_error() || node.is_missing() {
            errors.push(node.start_position().row + 1);
        }
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                Self::collect_errors(cursor.node(), errors);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    /// Find all call sites in a file that match a pattern (e.g., "tagRepository")
    /// and report their line numbers and enclosing function.
    ///
    /// Language-agnostic: works on any tree-sitter supported language by searching
    /// for method_invocation / call_expression nodes whose text contains the pattern.
    ///
    /// Used by auto_diagnose to give the model a complete list of similar call sites
    /// when a stack trace points to one — preventing the "fix one, miss nine" pattern.
    pub fn find_similar_calls(&mut self, path: &Path, pattern: &str) -> Option<String> {
        let source = std::fs::read_to_string(path).ok()?;
        let lang = LanguageRegistry::detect(path)?;
        let tree = self.cache.parse_source(&source, lang)?;

        let pattern_lower = pattern.to_lowercase();
        let mut results: Vec<(usize, String, String)> = Vec::new(); // (line, call_text, enclosing_fn)

        Self::walk_matching_calls(tree.root_node(), &source, &pattern_lower, &mut results, "");

        if results.is_empty() {
            return None;
        }

        let short_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());

        let mut out = format!(
            "{} calls matching '{}' in {}:\n",
            results.len(),
            pattern,
            short_name
        );
        for (line, call_text, func) in &results {
            if func.is_empty() {
                out.push_str(&format!("  L{}: {}\n", line, call_text));
            } else {
                out.push_str(&format!("  L{}: {} (in {})\n", line, call_text, func));
            }
        }
        Some(out)
    }

    /// Walk AST to find call expressions matching a pattern.
    fn walk_matching_calls(
        node: tree_sitter::Node,
        source: &str,
        pattern: &str,
        results: &mut Vec<(usize, String, String)>,
        enclosing_fn: &str,
    ) {
        // Track enclosing function name
        let mut current_fn = enclosing_fn.to_string();
        let kind = node.kind();
        if kind.contains("function") || kind.contains("method") || kind == "constructor_declaration"
        {
            if let Some(name_node) = node.child_by_field_name("name") {
                current_fn = source[name_node.start_byte()..name_node.end_byte()].to_string();
            }
        }

        // Match method_invocation (Java), call_expression (JS/TS/Python/Go/Rust)
        if kind == "method_invocation" || kind == "call_expression" {
            let call_text = &source[node.start_byte()..node.end_byte()];
            // Truncate long call texts (keep first 80 chars)
            let short = if call_text.len() > 80 {
                let mut end = 77;
                while !call_text.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}...", &call_text[..end])
            } else {
                call_text.to_string()
            };
            // Remove newlines for display
            let oneline = short.replace('\n', " ").replace("  ", " ");

            if call_text.to_lowercase().contains(pattern) {
                let line = node.start_position().row + 1;
                results.push((line, oneline, current_fn.clone()));
            }
        }

        // Recurse
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                Self::walk_matching_calls(cursor.node(), source, pattern, results, &current_fn);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    /// Extract symbols from Vue <template> section using tree-sitter-html.
    /// Returns key HTML elements as symbols so they appear in skeleton/file tree.
    fn list_vue_template_symbols(&mut self, source: &str) -> Option<Vec<Symbol>> {
        // Find <template> section
        let template_start = source.find("<template")?;
        let template_end = source.rfind("</template>")?;
        if template_start >= template_end {
            return None;
        }

        // Byte offset of <template> in the original file
        let template_content_start = source[template_start..].find('>')? + template_start + 1;
        let template_content = &source[template_content_start..template_end];

        // Line offset: count newlines before template start
        let line_offset = source[..template_content_start].lines().count();

        // Parse with HTML grammar
        let html_grammar = Lang::html_grammar();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&html_grammar).ok()?;
        let tree = parser.parse(template_content, None)?;

        let query_str = Lang::Html.symbols_query();
        let query = tree_sitter::Query::new(&html_grammar, query_str).ok()?;
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), template_content.as_bytes());

        let name_idx = query.capture_index_for_name("name")?;
        let def_idx = query.capture_index_for_name("definition")?;

        let mut symbols = Vec::new();
        let mut seen_lines = std::collections::HashSet::new();

        while let Some(m) = matches.next() {
            let name_cap = match m.captures.iter().find(|c| c.index == name_idx) {
                Some(c) => c,
                None => continue,
            };
            let def_cap = match m.captures.iter().find(|c| c.index == def_idx) {
                Some(c) => c,
                None => continue,
            };
            let name_node = name_cap.node;
            let def_node = def_cap.node;

            let tag_name = &template_content[name_node.start_byte()..name_node.end_byte()];
            let start_line = def_node.start_position().row + line_offset;

            // Skip common noise tags, keep structural/component elements
            if matches!(
                tag_name,
                "div"
                    | "span"
                    | "p"
                    | "a"
                    | "li"
                    | "ul"
                    | "ol"
                    | "br"
                    | "hr"
                    | "img"
                    | "i"
                    | "b"
                    | "strong"
                    | "em"
                    | "small"
                    | "label"
                    | "input"
                    | "option"
                    | "thead"
                    | "tbody"
                    | "tr"
                    | "td"
                    | "th"
            ) {
                // Only keep div/span if they have interesting attributes
                let line = template_content
                    .lines()
                    .nth(def_node.start_position().row)
                    .unwrap_or("");
                let has_vue_attr = line.contains("v-if")
                    || line.contains("v-for")
                    || line.contains("v-show")
                    || line.contains("@click")
                    || line.contains("v-model");
                if !has_vue_attr {
                    continue;
                }
            }

            // Dedup by line
            if !seen_lines.insert(start_line) {
                continue;
            }

            let end_line = def_node.end_position().row + line_offset;
            symbols.push(Symbol {
                name: format!("<{}>", tag_name),
                start_line,
                end_line,
                start_byte: def_node.start_byte() + template_content_start,
                end_byte: def_node.end_byte() + template_content_start,
                kind: "element".to_string(),
            });

            if symbols.len() >= 20 {
                break;
            } // Cap to avoid noise
        }

        if symbols.is_empty() {
            None
        } else {
            Some(symbols)
        }
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
        let mut seen_ranges: std::collections::HashSet<(usize, usize)> =
            std::collections::HashSet::new();

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

    fn skeleton_treesitter(&mut self, path: &Path, source: &str, lang: Lang) -> Option<String> {
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
                sym.start_line,
                sig_line.trim_end(),
                line_range,
                body_lines
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
        let mut seen_ranges: std::collections::HashSet<(usize, usize)> =
            std::collections::HashSet::new();

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
                if seen_ranges.contains(&range) {
                    continue;
                }
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

        // Add SFC section boundaries (<template>/<script>/<style>) as pseudo-symbols.
        // This lets the skeleton show where each section lives, so the model can
        // target-read the right section (e.g., template for HTML, script for logic).
        let lines: Vec<&str> = source.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("<template")
                || trimmed.starts_with("<script")
                || trimmed.starts_with("<style")
            {
                let tag = if trimmed.starts_with("<template") {
                    "template"
                } else if trimmed.starts_with("<script") {
                    "script"
                } else {
                    "style"
                };
                let close_tag = format!("</{}>", tag);
                let end_line = lines[i..]
                    .iter()
                    .position(|l| l.trim().starts_with(&close_tag))
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

    // ── File-type-aware fallback for languages without tree-sitter ──
    //
    // Single source of truth for skeleton generation of CSS/HTML/JSON/YAML/Markdown
    // and code files without tree-sitter support. read.rs has ZERO file-type logic.

    fn list_symbols_indent(&self, source: &str, path: &Path) -> Vec<Symbol> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let lines: Vec<&str> = source.lines().collect();

        match ext {
            "css" | "scss" | "less" | "sass" => self.list_symbols_css(&lines),
            "html" | "htm" => self.list_symbols_html(&lines),
            "json" => self.list_symbols_json(&lines),
            "yaml" | "yml" | "toml" => self.list_symbols_yaml(&lines),
            "md" | "mdx" => self.list_symbols_markdown(&lines),
            _ => self.list_symbols_code_indent(&lines),
        }
    }

    /// CSS/SCSS: :root, @rules, comment headers, top-level selectors
    fn list_symbols_css(&self, lines: &[&str]) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            let is_match = trimmed.starts_with(":root")
                || trimmed.starts_with("@keyframes")
                || trimmed.starts_with("@media")
                || trimmed.starts_with("@layer")
                || trimmed.starts_with("@import")
                || trimmed.starts_with("@font-face")
                || trimmed.starts_with("/* ===")
                || trimmed.starts_with("/* ---")
                || trimmed.starts_with("/* ***")
                || (indent == 0 && trimmed.starts_with('.') && trimmed.contains('{'))
                || (indent == 0 && trimmed.starts_with('#') && trimmed.contains('{'));

            if is_match {
                // Find the block end (matching closing brace)
                let end = find_block_end(lines, i);
                let name = trimmed
                    .split('{')
                    .next()
                    .unwrap_or(trimmed)
                    .trim()
                    .to_string();
                symbols.push(make_symbol(name, "css_rule", i, end, lines));
            }
        }
        symbols
    }

    /// HTML: structural tags
    fn list_symbols_html(&self, lines: &[&str]) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        let tags = [
            "<head",
            "<body",
            "<header",
            "<main",
            "<footer",
            "<nav",
            "<section",
            "<article",
            "<!DOCTYPE",
        ];
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if tags.iter().any(|t| trimmed.starts_with(t)) {
                let name = trimmed
                    .split(|c: char| c == '>' || c == ' ')
                    .next()
                    .unwrap_or(trimmed)
                    .to_string();
                symbols.push(make_symbol(name, "html_tag", i, i + 1, lines));
            }
        }
        symbols
    }

    /// JSON: top-level keys
    fn list_symbols_json(&self, lines: &[&str]) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let indent = line.len() - line.trim_start().len();
            // Top-level keys: indent ≤ 2, starts with "
            if indent <= 2 && trimmed.starts_with('"') && trimmed.contains(':') {
                let name = trimmed
                    .split(':')
                    .next()
                    .unwrap_or(trimmed)
                    .trim_matches('"')
                    .trim()
                    .to_string();
                symbols.push(make_symbol(name, "json_key", i, i + 1, lines));
            }
        }
        symbols
    }

    /// YAML/TOML: top-level keys
    fn list_symbols_yaml(&self, lines: &[&str]) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let indent = line.len() - line.trim_start().len();
            if indent == 0
                && !trimmed.is_empty()
                && !trimmed.starts_with('#')
                && !trimmed.starts_with("---")
            {
                let name = trimmed
                    .split(':')
                    .next()
                    .unwrap_or(trimmed)
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    symbols.push(make_symbol(name, "yaml_key", i, i + 1, lines));
                }
            }
        }
        symbols
    }

    /// Markdown: headings
    fn list_symbols_markdown(&self, lines: &[&str]) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                let name = trimmed.trim_start_matches('#').trim().to_string();
                // Find next heading or end
                let end = lines[i + 1..]
                    .iter()
                    .position(|l| l.trim().starts_with('#'))
                    .map(|p| i + 1 + p)
                    .unwrap_or(lines.len());
                symbols.push(make_symbol(name, "heading", i, end, lines));
            }
        }
        symbols
    }

    /// Code files: indent-level-0 definitions (fn/class/def/etc.)
    fn list_symbols_code_indent(&self, lines: &[&str]) -> Vec<Symbol> {
        let mut symbols = Vec::new();

        // Pass 1: Extract Chinese variable assignments at any indent level.
        // This runs independently of the definition block detection below,
        // ensuring variables inside function bodies are also captured.
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            if indent <= 8 && contains_chinese(trimmed) {
                if let Some(eq_pos) = trimmed.find('=') {
                    let var_name = trimmed[..eq_pos].trim();
                    if contains_chinese(var_name) && !var_name.contains(' ') {
                        symbols.push(make_symbol(
                            var_name.to_string(),
                            "chinese_variable",
                            i,
                            i + 1,
                            lines,
                        ));
                    }
                }
            }
        }

        // Pass 2: Extract indent-level-0 definition blocks (fn/class/def/etc.)
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let trimmed = line.trim();

            if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
                i += 1;
                continue;
            }

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
                    if end < lines.len() && lines[end].trim() == "}" {
                        end += 1;
                    }

                    let name = extract_indent_name(trimmed);
                    symbols.push(make_symbol(name, "indent_block", start, end, lines));

                    i = end;
                    continue;
                }
            }

            i += 1;
        }

        symbols
    }

    fn skeleton_indent(&self, source: &str, path: &Path) -> String {
        let symbols = self.list_symbols_indent(source, path);
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

/// Create a Symbol from line indices.
fn make_symbol(name: String, kind: &str, start: usize, end: usize, lines: &[&str]) -> Symbol {
    let start_byte = lines[..start].iter().map(|l| l.len() + 1).sum::<usize>();
    let end_byte = lines[..end].iter().map(|l| l.len() + 1).sum::<usize>();
    Symbol {
        name,
        start_line: start + 1,
        end_line: end,
        start_byte,
        end_byte,
        kind: kind.to_string(),
    }
}

/// Find the end of a CSS block starting at `start` (matching closing brace).
fn find_block_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    for i in start..lines.len() {
        for ch in lines[i].chars() {
            if ch == '{' {
                depth += 1;
            }
            if ch == '}' {
                depth -= 1;
            }
        }
        if depth <= 0 && i > start {
            return i + 1;
        }
    }
    (start + 1).min(lines.len())
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
        let clean = tok
            .trim_start_matches('*')
            .trim_end_matches(|c: char| "({:<".contains(c));
        if !clean.is_empty()
            && clean
                .chars()
                .next()
                .map_or(false, |c| c.is_alphabetic() || c == '_')
        {
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
        assert_eq!(
            LanguageRegistry::detect(Path::new("foo.rs")),
            Some(Lang::Rust)
        );
        assert_eq!(
            LanguageRegistry::detect(Path::new("bar.py")),
            Some(Lang::Python)
        );
        assert_eq!(
            LanguageRegistry::detect(Path::new("baz.js")),
            Some(Lang::JavaScript)
        );
        assert_eq!(
            LanguageRegistry::detect(Path::new("qux.ts")),
            Some(Lang::TypeScript)
        );
        assert_eq!(
            LanguageRegistry::detect(Path::new("main.go")),
            Some(Lang::Go)
        );
        assert_eq!(
            LanguageRegistry::detect(Path::new("App.java")),
            Some(Lang::Java)
        );
        assert_eq!(LanguageRegistry::detect(Path::new("main.c")), Some(Lang::C));
        assert_eq!(
            LanguageRegistry::detect(Path::new("main.cpp")),
            Some(Lang::Cpp)
        );
        assert_eq!(
            LanguageRegistry::detect(Path::new("Program.cs")),
            Some(Lang::CSharp)
        );
        assert_eq!(
            LanguageRegistry::detect(Path::new("index.php")),
            Some(Lang::Php)
        );
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
    fn test_list_symbols_csharp() {
        let mut searcher = SemanticSearcher::new();
        let source = r#"
class Program {
    Program() {}

    public static void Main(string[] args) {
    }
}

interface IGreeter {
    void Greet();
}
"#;
        let mut tmp = tempfile::NamedTempFile::with_suffix(".cs").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let symbols = searcher.list_symbols(tmp.path()).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Program"), "symbols: {:?}", names);
        assert!(names.contains(&"Main"), "symbols: {:?}", names);
        assert!(names.contains(&"IGreeter"), "symbols: {:?}", names);
    }

    #[test]
    fn test_list_symbols_php() {
        let mut searcher = SemanticSearcher::new();
        let source = r#"
<?php

class Calculator {
    public function add($a, $b) {
        return $a + $b;
    }
}

function greet($name) {
    return "Hello, $name";
}

interface Printable {
    public function print();
}
"#;
        let mut tmp = tempfile::NamedTempFile::with_suffix(".php").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let symbols = searcher.list_symbols(tmp.path()).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Calculator"), "php: {:?}", names);
        assert!(names.contains(&"add"), "php: {:?}", names);
        assert!(names.contains(&"greet"), "php: {:?}", names);
        assert!(names.contains(&"Printable"), "php: {:?}", names);
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
        assert!(
            names.contains(&"hello()"),
            "indent fallback symbols: {:?}",
            names
        );
    }

    #[test]
    fn test_chinese_character_detection() {
        assert!(is_chinese('中'));
        assert!(is_chinese('文'));
        assert!(!is_chinese('a'));
        assert!(!is_chinese('1'));
        assert!(!is_chinese('_'));
    }

    #[test]
    fn test_contains_chinese() {
        assert!(contains_chinese("用户名"));
        assert!(contains_chinese("hello世界"));
        assert!(!contains_chinese("hello"));
        assert!(!contains_chinese("123"));
    }

    #[test]
    fn test_pinyin_identifier_detection() {
        // Valid Pinyin identifiers
        assert!(is_pinyin_identifier("yonghuMing"));
        assert!(is_pinyin_identifier("dingdanList"));
        assert!(is_pinyin_identifier("zhongguoRen"));
        assert!(is_pinyin_identifier("wenjianMuLu"));

        // Invalid Pinyin identifiers
        assert!(!is_pinyin_identifier("hello"));
        assert!(!is_pinyin_identifier("getUser"));
        assert!(!is_pinyin_identifier(""));
        assert!(!is_pinyin_identifier("123"));
    }

    #[test]
    fn test_symbol_chinese_detection() {
        let sym = Symbol {
            name: "用户名".to_string(),
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: 9,
            kind: "variable".to_string(),
        };
        assert!(sym.is_chinese());
        assert!(!sym.is_pinyin());
        assert!(sym.is_chinese_related());

        let sym_pinyin = Symbol {
            name: "yonghuMing".to_string(),
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: 10,
            kind: "variable".to_string(),
        };
        assert!(!sym_pinyin.is_chinese());
        assert!(sym_pinyin.is_pinyin());
        assert!(sym_pinyin.is_chinese_related());

        let sym_english = Symbol {
            name: "getUser".to_string(),
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: 7,
            kind: "function".to_string(),
        };
        assert!(!sym_english.is_chinese());
        assert!(!sym_english.is_pinyin());
        assert!(!sym_english.is_chinese_related());
    }

    #[test]
    fn test_chinese_variable_extraction() {
        let mut searcher = SemanticSearcher::new();
        let source = r#"用户名 = "张三"
年龄 = 25
def get_user():
    return 用户名
"#;
        let mut tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let symbols = searcher.list_symbols(tmp.path()).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"用户名"), "symbols: {:?}", names);
    }

    #[test]
    fn test_mixed_chinese_english_detection() {
        // Mixed identifiers: English prefix + Chinese suffix
        assert!(contains_chinese("getUser用户名"));
        assert!(contains_chinese("query_订单列表"));
        assert!(contains_chinese("test数据"));
        assert!(contains_chinese("order详情"));

        // Mixed identifiers should be detected as Chinese-related
        let sym_mixed1 = Symbol {
            name: "getUser用户名".to_string(),
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: 0,
            kind: "variable".to_string(),
        };
        assert!(sym_mixed1.is_chinese_related());

        let sym_mixed2 = Symbol {
            name: "query_订单列表".to_string(),
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: 0,
            kind: "variable".to_string(),
        };
        assert!(sym_mixed2.is_chinese_related());

        // Pure English should NOT be detected
        assert!(!contains_chinese("getUser"));
        assert!(!contains_chinese("queryOrderList"));
    }

    #[test]
    fn test_mixed_content_extraction() {
        let mut searcher = SemanticSearcher::new();
        let source = r#"getUser用户名 = "张三"
query_订单列表 = []
test数据 = 42
"#;
        let mut tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let symbols = searcher.list_symbols(tmp.path()).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"getUser用户名"), "symbols: {:?}", names);
        assert!(names.contains(&"query_订单列表"), "symbols: {:?}", names);
        assert!(names.contains(&"test数据"), "symbols: {:?}", names);
    }

    #[test]
    fn test_chinese_variable_nested_indent() {
        // Chinese variables inside nested blocks (indent > 0) should be extracted
        let mut searcher = SemanticSearcher::new();
        let source = r#"def process():
    用户名 = "张三"
    订单列表 = []
    if True:
        配置项 = "value"
"#;
        let mut tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        tmp.write_all(source.as_bytes()).unwrap();

        let symbols = searcher.list_symbols(tmp.path()).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"用户名"), "nested symbols: {:?}", names);
        assert!(names.contains(&"订单列表"), "nested symbols: {:?}", names);
        assert!(names.contains(&"配置项"), "nested symbols: {:?}", names);
    }
}
