//! Stateless tree-sitter symbol extraction. Ported from production
//! `semantic/mod.rs::list_symbols_treesitter`, minus the parse cache and the
//! SemanticSearcher state — we parse fresh per call (single-file parsing is cheap, and
//! a neutral tool holds no shared index; the cross-file graph layer comes later).

use crate::codeintel::lang::Lang;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

/// A symbol definition found in a source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// The tree-sitter node kind of the definition (e.g. `function_item`, `struct_item`).
    pub kind: String,
    /// 1-based inclusive line range of the definition.
    pub start_line: usize,
    pub end_line: usize,
    /// Byte range of the definition in the source.
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Parse `source` as `lang` and extract symbol definitions (functions, types, classes,
/// methods, …) via the language's tree-sitter query. `None` only if parsing or query
/// compilation fails; an empty `Vec` means a clean parse with no symbols.
pub fn extract_symbols(source: &str, lang: Lang) -> Option<Vec<Symbol>> {
    let grammar = lang.grammar();
    let mut parser = Parser::new();
    parser.set_language(&grammar).ok()?;
    let tree = parser.parse(source, None)?;

    let query = Query::new(&grammar, lang.symbols_query()).ok()?;
    let def_idx = query.capture_index_for_name("definition")?;
    let name_idx = query.capture_index_for_name("name")?;

    let mut cursor = QueryCursor::new();
    let mut symbols = Vec::new();
    // Dedup by byte range — a query may match the same definition via multiple patterns.
    let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();

    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    loop {
        matches.advance();
        let m = match matches.get() {
            Some(m) => m,
            None => break,
        };

        let mut name = None;
        let (mut ds, mut de, mut dsr, mut der) = (0usize, 0usize, 0usize, 0usize);
        let mut kind = "";
        let mut has_def = false;
        for cap in m.captures {
            if cap.index == name_idx {
                name = Some(source[cap.node.start_byte()..cap.node.end_byte()].to_string());
            }
            if cap.index == def_idx {
                ds = cap.node.start_byte();
                de = cap.node.end_byte();
                dsr = cap.node.start_position().row;
                der = cap.node.end_position().row;
                kind = cap.node.kind();
                has_def = true;
            }
        }
        if let (Some(name), true) = (name, has_def) {
            if seen.insert((ds, de)) {
                symbols.push(Symbol {
                    name,
                    kind: kind.to_string(),
                    start_line: dsr + 1,
                    end_line: der + 1,
                    start_byte: ds,
                    end_byte: de,
                });
            }
        }
    }
    Some(symbols)
}

/// Extract the first symbol named `name` from `source`.
pub fn extract_symbol(source: &str, lang: Lang, name: &str) -> Option<Symbol> {
    extract_symbols(source, lang)?.into_iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_symbols() {
        let src = "struct Foo {\n    x: i32,\n}\n\nfn bar(a: i32) -> i32 {\n    a + 1\n}\n";
        let syms = extract_symbols(src, Lang::Rust).expect("parse");
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"), "{names:?}");
        assert!(names.contains(&"bar"), "{names:?}");
        let bar = syms.iter().find(|s| s.name == "bar").unwrap();
        assert_eq!(bar.start_line, 5);
        assert_eq!(bar.end_line, 7);
    }

    #[test]
    fn extracts_python_symbols() {
        let src = "class A:\n    def m(self):\n        pass\n\ndef top():\n    return 1\n";
        let syms = extract_symbols(src, Lang::Python).expect("parse");
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"A"), "{names:?}");
        assert!(names.contains(&"top"), "{names:?}");
    }

    #[test]
    fn extract_symbol_finds_by_name() {
        let src = "fn alpha() {}\nfn beta() {}\n";
        let s = extract_symbol(src, Lang::Rust, "beta").expect("found");
        assert_eq!(s.name, "beta");
        assert_eq!(s.start_line, 2);
        assert!(extract_symbol(src, Lang::Rust, "missing").is_none());
    }

    #[test]
    fn empty_source_is_some_empty() {
        let syms = extract_symbols("", Lang::Rust).expect("parse");
        assert!(syms.is_empty());
    }
}
