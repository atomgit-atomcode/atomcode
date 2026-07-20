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
        "function_item" | "function_definition" | "function_declaration" | "func_literal" => {
            SymbolKind::Function
        }
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
    /// caller's start_line — lets the build reconstruct the caller's exact id via `make_id`
    /// instead of a name lookup (removes a scan and fixes wrong-caller attribution).
    caller_line: usize,
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
                .filter(|s| {
                    matches!(
                        classify_symbol_kind(&s.kind),
                        SymbolKind::Function | SymbolKind::Method
                    )
                })
                .filter(|s| s.start_line <= line && line <= s.end_line)
                .max_by_key(|s| s.start_line);
            if let Some(caller) = caller {
                if caller.name != callee_name {
                    calls.push(RawCall {
                        caller_name: caller.name.clone(),
                        caller_line: caller.start_line,
                        callee_name,
                        line,
                    });
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
const INDEXED_EXTS: &[&str] = &[
    "rs", "py", "js", "jsx", "mjs", "cjs", "ts", "mts", "tsx", "go", "java", "c", "h", "cc", "cpp",
    "cxx", "hpp", "hh",
];

/// A walked source file + the inputs to its staleness fingerprint.
struct Walked {
    path: PathBuf,
    /// mtime in NANOSECONDS — coarse whole seconds would miss a same-second edit and
    /// serve a stale graph.
    mtime_ns: u128,
    /// file length — defends against a same-instant edit whose mtime didn't move (content
    /// length almost always changes on a real edit).
    len: u64,
}

/// Walk `root` (assumed already canonical) for indexable source files + staleness inputs.
fn collect_files(root: &Path) -> Vec<Walked> {
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
        let ext_ok = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| INDEXED_EXTS.contains(&e))
            .unwrap_or(false);
        if !ext_ok {
            continue;
        }
        let md = entry.metadata().ok();
        let mtime_ns = md
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let len = md.as_ref().map(|m| m.len()).unwrap_or(0);
        out.push(Walked {
            path: p.to_path_buf(),
            mtime_ns,
            len,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn fingerprint(files: &[Walked]) -> u64 {
    let mut h = DefaultHasher::new();
    for w in files {
        w.path.hash(&mut h);
        w.mtime_ns.hash(&mut h);
        w.len.hash(&mut h);
    }
    h.finish()
}

fn top_component(p: &Path, root: &Path) -> Option<std::ffi::OsString> {
    p.strip_prefix(root)
        .ok()?
        .components()
        .next()
        .map(|c| c.as_os_str().to_os_string())
}

/// Resolve a callee name to a symbol id, preferring closer candidates (production
/// scoring): same file (4) > same dir (2) > same top-level component (1) > any (0).
/// (Import-based score 3 is omitted — like production, we do not parse imports yet.)
/// Ties are broken DETERMINISTICALLY by the smallest (file, start_line) — production's
/// tie-break depends on HashMap iteration order, which is not reproducible.
fn resolve_callee(
    g: &CodeGraph,
    callee: &str,
    caller_file: &Path,
    root: &Path,
) -> Option<SymbolId> {
    let score = |n: &SymbolNode| -> i32 {
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
    };
    let mut best: Option<&SymbolNode> = None;
    let mut best_score = i32::MIN;
    for n in g.find_by_name(callee) {
        let s = score(n);
        let better = match best {
            None => true,
            Some(b) => {
                s > best_score
                    || (s == best_score
                        && (n.file.as_path(), n.start_line) < (b.file.as_path(), b.start_line))
            }
        };
        if better {
            best = Some(n);
            best_score = s;
        }
    }
    best.map(|n| n.id)
}

fn build_from_files(root: &Path, files: Vec<Walked>) -> CodeGraph {
    let mut g = CodeGraph::new();
    let mut raw_calls: Vec<(PathBuf, RawCall)> = Vec::new();
    for w in &files {
        let Ok(source) = std::fs::read_to_string(&w.path) else {
            continue;
        };
        if let Some((nodes, calls)) = parse_file(&w.path, &source) {
            for n in nodes {
                g.add_symbol(n);
            }
            g.file_mtimes
                .insert(w.path.clone(), (w.mtime_ns / 1_000_000_000) as u64);
            for c in calls {
                raw_calls.push((w.path.clone(), c));
            }
        }
    }
    // Resolve after ALL symbols are inserted (a call may target a not-yet-seen file).
    for (caller_file, rc) in raw_calls {
        // Exact caller id: same make_id inputs as when the caller symbol was inserted.
        let caller = CodeGraph::make_id(&caller_file, &rc.caller_name, rc.caller_line);
        if g.node(caller).is_none() {
            continue;
        }
        if let Some(callee) = resolve_callee(&g, &rc.callee_name, &caller_file, root) {
            g.add_edge(
                caller,
                Edge {
                    to: callee,
                    kind: EdgeKind::Calls,
                    line: rc.line,
                },
            );
        }
    }
    g
}

/// Build a fresh code graph for `root` (walk → parse → resolve). O(repo), CPU-bound.
pub fn build_graph(root: &Path) -> CodeGraph {
    let root = super::canonical(root);
    build_from_files(&root, collect_files(&root))
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
        let root = super::canonical(root);
        let files = collect_files(&root);
        let fp = fingerprint(&files);
        if let Some((cfp, g)) = self.cache.lock().unwrap().as_ref() {
            if *cfp == fp {
                return g.clone();
            }
        }
        let g = Arc::new(build_from_files(&root, files));
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
        std::fs::write(
            d.path().join("a.rs"),
            "fn helper() {}\nfn main() {\n    helper();\n}\n",
        )
        .unwrap();
        let g = build_graph(d.path());
        let main = g.find_by_name("main").into_iter().next().expect("main");
        let helper = g.find_by_name("helper").into_iter().next().expect("helper");
        // main → helper edge exists
        let callees = g.callees(main.id).expect("callees");
        assert!(
            callees.iter().any(|e| e.to == helper.id),
            "main should call helper"
        );
        // reverse: helper has main as caller
        assert!(g
            .callers(helper.id)
            .unwrap()
            .iter()
            .any(|e| e.to == main.id));
    }

    #[test]
    fn resolves_calls_across_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("util.rs"), "pub fn compute() -> i32 { 42 }\n").unwrap();
        std::fs::write(
            d.path().join("main.rs"),
            "fn run() {\n    let _ = compute();\n}\n",
        )
        .unwrap();
        let g = build_graph(d.path());
        let run = g.find_by_name("run").into_iter().next().expect("run");
        let compute = g
            .find_by_name("compute")
            .into_iter()
            .next()
            .expect("compute");
        assert!(
            g.callees(run.id)
                .unwrap()
                .iter()
                .any(|e| e.to == compute.id),
            "run → compute across files"
        );
    }

    #[test]
    fn self_calls_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("r.rs"),
            "fn recur(n: i32) {\n    if n > 0 { recur(n - 1); }\n}\n",
        )
        .unwrap();
        let g = build_graph(d.path());
        let recur = g.find_by_name("recur").into_iter().next().expect("recur");
        assert!(
            g.callees(recur.id).map(|e| e.is_empty()).unwrap_or(true),
            "self-call must be skipped"
        );
    }

    #[test]
    fn same_second_edit_triggers_rebuild() {
        // Overwriting the SAME file (likely the same wall-clock second) must rebuild —
        // the fingerprint uses nanos + length, not coarse seconds.
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.rs");
        std::fs::write(&f, "fn one() {}\n").unwrap();
        let idx = CodeIndex::new();
        let g1 = idx.get(d.path());
        assert!(g1.find_by_name("two").is_empty());
        std::fs::write(&f, "fn one() {}\nfn two() {}\n").unwrap();
        let g2 = idx.get(d.path());
        assert!(
            !g2.find_by_name("two").is_empty(),
            "same-second edit must rebuild (nanos/len changed)"
        );
    }

    #[test]
    fn tie_break_resolution_is_deterministic() {
        // Two same-named fns in the same dir → equal score for a same-dir caller → tie,
        // resolved deterministically to the smallest (file, line) = a_util.rs.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a_util.rs"), "pub fn dup() {}\n").unwrap();
        std::fs::write(d.path().join("z_util.rs"), "pub fn dup() {}\n").unwrap();
        std::fs::write(d.path().join("main.rs"), "fn run() { dup(); }\n").unwrap();
        let g = build_graph(d.path());
        let run = g.find_by_name("run").into_iter().next().unwrap();
        let target = g
            .callees(run.id)
            .and_then(|e| e.first())
            .and_then(|e| g.node(e.to))
            .map(|n| n.file.clone());
        assert!(
            target
                .as_ref()
                .map(|f| f.ends_with("a_util.rs"))
                .unwrap_or(false),
            "tie → a_util.rs, got {target:?}"
        );
        // stable across a rebuild
        let g2 = build_graph(d.path());
        let run2 = g2.find_by_name("run").into_iter().next().unwrap();
        let t2 = g2
            .callees(run2.id)
            .and_then(|e| e.first())
            .and_then(|e| g2.node(e.to))
            .map(|n| n.file.clone());
        assert_eq!(target, t2);
    }

    #[test]
    fn index_caches_then_rebuilds_on_change() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn one() {}\n").unwrap();
        let idx = CodeIndex::new();
        let g1 = idx.get(d.path());
        let g2 = idx.get(d.path());
        assert!(
            Arc::ptr_eq(&g1, &g2),
            "unchanged repo → cached graph reused"
        );
        assert!(g1.find_by_name("two").is_empty());
        // change the repo (new mtime via a new file) → rebuild
        std::fs::write(d.path().join("b.rs"), "fn two() {}\n").unwrap();
        let g3 = idx.get(d.path());
        assert!(!Arc::ptr_eq(&g1, &g3), "changed repo → rebuilt");
        assert!(
            !g3.find_by_name("two").is_empty(),
            "rebuilt graph sees new symbol"
        );
    }

    #[test]
    fn caller_attribution_is_per_file() {
        // Two files each define a function named `handler`, each calling a DISTINCT callee.
        // The old resolver picked the first same-named symbol as caller, so both edges hung
        // off ONE handler. Caller id must be reconstructed exactly, per file.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.rs"),
            "fn handler() {\n    alpha();\n}\nfn alpha() {}\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("b.rs"),
            "fn handler() {\n    beta();\n}\nfn beta() {}\n",
        )
        .unwrap();
        let g = build_graph(d.path());

        let a_handler = g
            .find_by_name("handler")
            .into_iter()
            .find(|n| n.file.ends_with("a.rs"))
            .expect("a.rs handler");
        let b_handler = g
            .find_by_name("handler")
            .into_iter()
            .find(|n| n.file.ends_with("b.rs"))
            .expect("b.rs handler");
        let alpha = g.find_by_name("alpha").into_iter().next().expect("alpha");
        let beta = g.find_by_name("beta").into_iter().next().expect("beta");

        let a_callees = g.callees(a_handler.id).cloned().unwrap_or_default();
        let b_callees = g.callees(b_handler.id).cloned().unwrap_or_default();

        assert!(
            a_callees.iter().any(|e| e.to == alpha.id),
            "a.rs::handler → alpha"
        );
        assert!(
            !a_callees.iter().any(|e| e.to == beta.id),
            "a.rs::handler must NOT call beta"
        );
        assert!(
            b_callees.iter().any(|e| e.to == beta.id),
            "b.rs::handler → beta"
        );
        assert!(
            !b_callees.iter().any(|e| e.to == alpha.id),
            "b.rs::handler must NOT call alpha"
        );
    }
}
