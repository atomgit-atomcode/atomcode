//! `build_graph(root)` + the shared, lazily-built [`CodeIndex`] the graph tools hold.
//! Ported from production `graph/indexer.rs` (the build + call-resolution; the
//! background/incremental indexer + CPU throttling are replaced by a simpler
//! build-once-then-rebuild-on-mtime-change cache — correct first, optimize later).

use super::graph::{CodeGraph, Edge, EdgeKind, SymbolId, SymbolKind, SymbolNode, Visibility};
use super::lang::Lang;
use super::symbols::{extract_symbols, Symbol};
use ignore::WalkBuilder;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

/// Map a tree-sitter node-kind string to a [`SymbolKind`]. From production
/// `classify_symbol_kind`.
fn classify_symbol_kind(ts: &str) -> SymbolKind {
    match ts {
        "function_item" | "function_definition" | "function_declaration" | "func_literal" => SymbolKind::Function,
        "method_definition" | "method_declaration" => SymbolKind::Method,
        "struct_item" | "struct_specifier" | "struct_type" => SymbolKind::Struct,
        "class_definition" | "class_declaration" | "class_specifier" => SymbolKind::Class,
        "trait_item" => SymbolKind::Trait,
        "interface_declaration" | "interface_type" => SymbolKind::Interface,
        "enum_item" | "enum_declaration" | "enum_specifier" => SymbolKind::Enum,
        "const_item" | "const_declaration" => SymbolKind::Constant,
        "let_declaration" | "variable_declaration" | "static_item" => SymbolKind::Variable,
        "mod_item" | "module" => SymbolKind::Module,
        "use_declaration" | "import_statement" | "import_declaration" => SymbolKind::Import,
        "type_item" | "type_alias_declaration" => SymbolKind::TypeAlias,
        "impl_item" => SymbolKind::Other("impl".to_string()),
        other => SymbolKind::Other(other.to_string()),
    }
}

struct RawCall {
    caller_name: String,
    callee_name: String,
    line: usize,
}

/// Extract raw call edges via the language's calls query (`@callee`). Each call is
/// attributed to the innermost enclosing Function/Method symbol; self-calls are skipped.
fn extract_calls(source: &str, lang: Lang, syms: &[Symbol]) -> Vec<RawCall> {
    let Some(q_src) = lang.calls_query() else {
        return Vec::new();
    };
    let grammar = lang.grammar();
    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let Ok(query) = Query::new(&grammar, q_src) else {
        return Vec::new();
    };
    let Some(callee_idx) = query.capture_index_for_name("callee") else {
        return Vec::new();
    };

    let mut cursor = QueryCursor::new();
    let mut calls = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    loop {
        matches.advance();
        let m = match matches.get() {
            Some(m) => m,
            None => break,
        };
        for cap in m.captures {
            if cap.index != callee_idx {
                continue;
            }
            let callee_name = source[cap.node.start_byte()..cap.node.end_byte()].to_string();
            let line = cap.node.start_position().row + 1;
            // Innermost enclosing Function/Method (max start_line whose range covers the call).
            let caller = syms
                .iter()
                .filter(|s| matches!(classify_symbol_kind(&s.kind), SymbolKind::Function | SymbolKind::Method))
                .filter(|s| s.start_line <= line && line <= s.end_line)
                .max_by_key(|s| s.start_line);
            if let Some(caller) = caller {
                if caller.name != callee_name {
                    calls.push(RawCall { caller_name: caller.name.clone(), callee_name, line });
                }
            }
        }
    }
    calls
}

fn parse_file(path: &Path, source: &str) -> Option<(Vec<SymbolNode>, Vec<RawCall>)> {
    let lang = Lang::detect(path)?;
    if !lang.is_indexed() {
        return None;
    }
    let raw = extract_symbols(source, lang)?;
    let nodes = raw
        .iter()
        .map(|s| SymbolNode {
            id: CodeGraph::make_id(path, &s.name, s.start_line),
            name: s.name.clone(),
            kind: classify_symbol_kind(&s.kind),
            visibility: Visibility::Unknown,
            file: path.to_path_buf(),
            start_line: s.start_line,
            end_line: s.end_line,
            signature: None,
        })
        .collect();
    let calls = extract_calls(source, lang, &raw);
    Some((nodes, calls))
}

/// Extensions walked into the graph (matches production's INDEXED set + variants).
const INDEXED_EXTS: &[&str] =
    &["rs", "py", "js", "jsx", "mjs", "cjs", "ts", "mts", "tsx", "go", "java", "c", "h", "cc", "cpp", "cxx", "hpp", "hh"];

fn collect_files(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
        .flatten()
    {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let ext_ok = p.extension().and_then(|e| e.to_str()).map(|e| INDEXED_EXTS.contains(&e)).unwrap_or(false);
        if !ext_ok {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push((p.to_path_buf(), mtime));
    }
    out.sort();
    out
}

fn fingerprint(files: &[(PathBuf, u64)]) -> u64 {
    let mut h = DefaultHasher::new();
    for (p, m) in files {
        p.hash(&mut h);
        m.hash(&mut h);
    }
    h.finish()
}

fn top_component(p: &Path, root: &Path) -> Option<std::ffi::OsString> {
    p.strip_prefix(root).ok()?.components().next().map(|c| c.as_os_str().to_os_string())
}

/// Resolve a callee name to a symbol id, preferring closer candidates (production
/// scoring): same file (4) > same dir (2) > same top-level component (1) > any (0).
/// (Import-based score 3 is omitted — like production, we do not parse imports yet.)
fn resolve_callee(g: &CodeGraph, callee: &str, caller_file: &Path, root: &Path) -> Option<SymbolId> {
    let candidates = g.find_by_name(callee);
    candidates
        .iter()
        .max_by_key(|n| {
            if n.file == caller_file {
                4
            } else if n.file.parent().is_some() && n.file.parent() == caller_file.parent() {
                2
            } else {
                let a = top_component(&n.file, root);
                if a.is_some() && a == top_component(caller_file, root) {
                    1
                } else {
                    0
                }
            }
        })
        .map(|n| n.id)
}

fn build_from_files(root: &Path, files: Vec<(PathBuf, u64)>) -> CodeGraph {
    let mut g = CodeGraph::new();
    let mut raw_calls: Vec<(PathBuf, RawCall)> = Vec::new();
    for (path, mtime) in &files {
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Some((nodes, calls)) = parse_file(path, &source) {
            for n in nodes {
                g.add_symbol(n);
            }
            g.file_mtimes.insert(path.clone(), *mtime);
            for c in calls {
                raw_calls.push((path.clone(), c));
            }
        }
    }
    // Resolve after ALL symbols are inserted (a call may target a not-yet-seen file).
    for (caller_file, rc) in raw_calls {
        let Some(caller) = g.find_by_name(&rc.caller_name).first().map(|n| n.id) else {
            continue;
        };
        if let Some(callee) = resolve_callee(&g, &rc.callee_name, &caller_file, root) {
            g.add_edge(caller, Edge { to: callee, kind: EdgeKind::Calls, line: rc.line });
        }
    }
    g
}

/// Build a fresh code graph for `root` (walk → parse → resolve). O(repo), CPU-bound.
pub fn build_graph(root: &Path) -> CodeGraph {
    build_from_files(root, collect_files(root))
}

/// Shared, lazily-built code index the graph tools hold. `get` returns a cached graph
/// when the indexed files' (path, mtime) fingerprint is unchanged, else rebuilds. O(repo)
/// and CPU-bound — call from a blocking context (the tools use `spawn_blocking`).
#[derive(Default)]
pub struct CodeIndex {
    cache: Mutex<Option<(u64, Arc<CodeGraph>)>>,
}

impl CodeIndex {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self, root: &Path) -> Arc<CodeGraph> {
        let files = collect_files(root);
        let fp = fingerprint(&files);
        if let Some((cfp, g)) = self.cache.lock().unwrap().as_ref() {
            if *cfp == fp {
                return g.clone();
            }
        }
        let g = Arc::new(build_from_files(root, files));
        *self.cache.lock().unwrap() = Some((fp, g.clone()));
        g
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_cross_file_call_edges() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn helper() {}\nfn main() {\n    helper();\n}\n").unwrap();
        let g = build_graph(d.path());
        let main = g.find_by_name("main").into_iter().next().expect("main");
        let helper = g.find_by_name("helper").into_iter().next().expect("helper");
        // main → helper edge exists
        let callees = g.callees(main.id).expect("callees");
        assert!(callees.iter().any(|e| e.to == helper.id), "main should call helper");
        // reverse: helper has main as caller
        assert!(g.callers(helper.id).unwrap().iter().any(|e| e.to == main.id));
    }

    #[test]
    fn resolves_calls_across_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("util.rs"), "pub fn compute() -> i32 { 42 }\n").unwrap();
        std::fs::write(d.path().join("main.rs"), "fn run() {\n    let _ = compute();\n}\n").unwrap();
        let g = build_graph(d.path());
        let run = g.find_by_name("run").into_iter().next().expect("run");
        let compute = g.find_by_name("compute").into_iter().next().expect("compute");
        assert!(g.callees(run.id).unwrap().iter().any(|e| e.to == compute.id), "run → compute across files");
    }

    #[test]
    fn self_calls_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("r.rs"), "fn recur(n: i32) {\n    if n > 0 { recur(n - 1); }\n}\n").unwrap();
        let g = build_graph(d.path());
        let recur = g.find_by_name("recur").into_iter().next().expect("recur");
        assert!(g.callees(recur.id).map(|e| e.is_empty()).unwrap_or(true), "self-call must be skipped");
    }

    #[test]
    fn index_caches_then_rebuilds_on_change() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn one() {}\n").unwrap();
        let idx = CodeIndex::new();
        let g1 = idx.get(d.path());
        let g2 = idx.get(d.path());
        assert!(Arc::ptr_eq(&g1, &g2), "unchanged repo → cached graph reused");
        assert!(g1.find_by_name("two").is_empty());
        // change the repo (new mtime via a new file) → rebuild
        std::fs::write(d.path().join("b.rs"), "fn two() {}\n").unwrap();
        let g3 = idx.get(d.path());
        assert!(!Arc::ptr_eq(&g1, &g3), "changed repo → rebuilt");
        assert!(!g3.find_by_name("two").is_empty(), "rebuilt graph sees new symbol");
    }
}
