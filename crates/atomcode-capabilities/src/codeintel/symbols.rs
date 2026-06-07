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

    #[test]
    fn extracts_symbols_for_every_language() {
        use Lang::*;
        // (lang, sample source, symbol names that MUST be found)
        let cases: &[(Lang, &str, &[&str])] = &[
            (Rust, "struct S;\nfn f() {}\n", &["S", "f"]),
            (Python, "class A:\n    def m(self):\n        pass\n\ndef g():\n    pass\n", &["A", "g"]),
            (JavaScript, "function fn() {}\nclass C {}\n", &["fn", "C"]),
            (TypeScript, "function fn(): void {}\ninterface I {}\nclass C {}\n", &["fn", "I", "C"]),
            (Tsx, "function App() { return null; }\n", &["App"]),
            (Go, "package p\nfunc F() {}\ntype T struct{}\n", &["F", "T"]),
            (Java, "class C {\n  void m() {}\n}\n", &["C", "m"]),
            (C, "struct S { int x; };\nint f(void) { return 0; }\n", &["S", "f"]),
            (Cpp, "namespace N {}\nclass C {};\nvoid f() {}\n", &["N", "C", "f"]),
            (CSharp, "class C {\n  void M() {}\n}\n", &["C", "M"]),
            (Php, "<?php\nfunction f() {}\nclass C {}\n", &["f", "C"]),
        ];
        for (lang, src, expected) in cases {
            let syms = extract_symbols(src, *lang).unwrap_or_default();
            let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
            for e in *expected {
                assert!(names.contains(e), "{lang:?}: expected symbol {e:?}, got {names:?}");
            }
        }
        // HTML "symbols" are element-ish; just assert the query runs without error.
        assert!(extract_symbols("<div id=\"app\"></div>\n", Lang::Html).is_some());
    }

    #[test]
    fn byte_slicing_is_utf8_safe_with_multibyte_names() {
        // tree-sitter byte offsets align to token (char) boundaries, so name slicing
        // must not panic on multibyte identifiers / bodies.
        let src = "// 注释\nfn 计算(参数: i32) -> i32 {\n    参数 + 1\n}\n";
        let syms = extract_symbols(src, Lang::Rust).expect("parse");
        assert!(syms.iter().any(|s| s.name == "计算"), "{:?}", syms.iter().map(|s| &s.name).collect::<Vec<_>>());
    }
}
