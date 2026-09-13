//! `build_graph(root)` + the shared, lazily-built [`CodeIndex`] the graph tools hold.
//! Ported from production `graph/indexer.rs` (the build + call-resolution; the
//! background/incremental indexer + CPU throttling are replaced by a simpler
//! build-once-then-rebuild-on-mtime-change cache — correct first, optimize later).

use super::graph::{CodeGraph, Edge, EdgeKind, SymbolId, SymbolKind, SymbolNode, Visibility};
use super::lang::Lang;
use super::symbols::{extract_symbols, Symbol};
use ignore::{DirEntry, WalkBuilder};
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
        // Kotlin: singletons/companions are type-bearing → Class; a property/enum
        // entry map to Variable/Constant. (`class_declaration` already covers
        // Kotlin class/interface/enum-class; `function_declaration` covers both
        // top-level funs and methods.)
        "object_declaration" | "companion_object" => SymbolKind::Class,
        "enum_entry" => SymbolKind::Constant,
        "property_declaration" => SymbolKind::Variable,
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
    "cxx", "hpp", "hh", "kt", "kts",
];

/// A walked source file + the inputs to its staleness fingerprint.
#[derive(Debug)]
struct Walked {
    path: PathBuf,
    /// mtime in NANOSECONDS — coarse whole seconds would miss a same-second edit and
    /// serve a stale graph.
    mtime_ns: u128,
    /// file length — defends against a same-instant edit whose mtime didn't move (content
    /// length almost always changes on a real edit).
    len: u64,
}

/// Bytes as MiB, for the human-facing oversize messages.
fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Bounds on the index walk. An unbounded walk of a huge non-hidden working directory
/// (e.g. `C:/Users/<name>` including `AppData`) makes the graph tools hang for tens of
/// minutes with no return (issue #1538), so the walk is capped: the first time the
/// source-file count or the accumulated source bytes exceed a cap, the walk aborts with
/// an actionable error instead of continuing (it NEVER returns a silent partial set).
/// The defaults are generous — a real repository fits far below them — but bound the
/// worst case to a walk, not a parse, of the whole tree.
#[derive(Debug, Clone)]
pub struct IndexLimits {
    pub max_files: usize,
    pub max_total_bytes: u64,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            // ~20k source files: far above any normal repo (a large monorepo is
            // typically a few thousand), small enough that a walk is still fast.
            max_files: 20_000,
            // 256 MiB of indexed source bytes: a full monorepo of code, and an
            // AppData-scale tree blows past it within seconds of walking.
            max_total_bytes: 256 * 1024 * 1024,
        }
    }
}

/// The index walk aborted early because the working directory is too large to index.
/// Carries the limits so the caller can render an actionable message.
#[derive(Debug, Clone)]
pub struct IndexError {
    pub reason: String,
}

impl IndexError {
    pub fn oversize(reason: String) -> Self {
        Self { reason }
    }
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for IndexError {}

/// Build the shared WalkBuilder options: gitignore-aware + the crate-wide directory
/// excludes ([`crate::pathutil::is_skip_dir`], the same list grep/glob/list use). Those
/// cover build output, dependency caches, VCS metadata, and temp/`AppData` dirs — the
/// classic offenders behind a huge non-repo workdir (issue #1538). Sharing the one
/// list keeps codeintel consistent with the other walkers instead of maintaining a
/// second copy that drifts. (The `ignore` builder methods return `&mut WalkBuilder`,
/// so options are set as statements and the owned builder returned at the end.)
fn build_walk(root: &Path) -> WalkBuilder {
    let mut walker = WalkBuilder::new(root);
    walker.hidden(true);
    walker.git_ignore(true);
    walker.git_global(true);
    walker.git_exclude(true);
    walker.filter_entry(|e: &DirEntry| {
        // Prune excluded DIRECTORIES (whole subtree); never prune files. The `ignore`
        // crate never passes the walk-root entry here (depth 0 is exempt), so a workdir
        // that is itself named like a skip-dir is still walked.
        if e.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            if let Some(name) = e.file_name().to_str() {
                return !crate::pathutil::is_skip_dir(name);
            }
        }
        true
    });
    walker
}

/// Walk `root` (assumed already canonical) for indexable source files + staleness
/// inputs. `limits` caps the walk (see [`IndexLimits`]); the first cap breach aborts
/// with `IndexError::oversize` — never a silent partial file set.
fn collect_files_limited(root: &Path, limits: &IndexLimits) -> Result<Vec<Walked>, IndexError> {
    let mut out = Vec::new();
    let mut total_bytes: u64 = 0;
    for entry in build_walk(root).build().flatten() {
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
        // Cap checks run BEFORE the file joins the set, so `out` never holds more than
        // the cap and the reported counts are exact (no off-by-one fudging).
        if out.len() >= limits.max_files {
            return Err(IndexError::oversize(format!(
                "working directory too large to index ({} source files so far, limit {}) - start \
                 AtomCode from a project subdirectory such as the repository root",
                out.len(),
                limits.max_files
            )));
        }
        let bytes_after = total_bytes.saturating_add(len);
        if bytes_after > limits.max_total_bytes {
            return Err(IndexError::oversize(format!(
                "working directory too large to index (over {:.1} MiB of source in {} files, \
                 limit {:.1} MiB) - start AtomCode from a project subdirectory such as the \
                 repository root",
                mib(bytes_after),
                out.len(),
                mib(limits.max_total_bytes)
            )));
        }
        out.push(Walked {
            path: p.to_path_buf(),
            mtime_ns,
            len,
        });
        total_bytes = bytes_after;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Walk `root` (assumed already canonical) for indexable source files + staleness
/// inputs, with [`IndexLimits::default`] caps. An oversize walk yields an EMPTY file
/// set (the caller then builds an empty graph — never a hang, never a partial set).
/// [`CodeIndex::get_limited`] is the fallible entry point that surfaces the error.
fn collect_files(root: &Path) -> Vec<Walked> {
    collect_files_limited(root, &IndexLimits::default()).unwrap_or_default()
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
/// Applies the default directory excludes and the [`IndexLimits::default`] caps; if the
/// walk is oversize it returns an EMPTY graph (never a hang, never a partial graph). The
/// graph TOOLS surface a proper error instead, via [`CodeIndex::get_limited`].
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
    /// Cached graph for `root`, built with [`IndexLimits::default`]. An oversize walk
    /// (issue #1538) yields an EMPTY graph instead of hanging; call
    /// [`Self::get_limited`] when the caller must surface the reason rather than a
    /// silently empty result.
    pub fn get(&self, root: &Path) -> Arc<CodeGraph> {
        self.get_limited(root, &IndexLimits::default())
            .unwrap_or_else(|_| Arc::new(CodeGraph::new()))
    }

    /// Like [`get`](Self::get) but propagates an oversize walk failure (issue #1538)
    /// instead of hiding it behind an empty graph, so a tool can surface an actionable
    /// message. Cache key is the file-set fingerprint (excludes limits); `limits` only
    /// bound the walk itself.
    pub fn get_limited(
        &self,
        root: &Path,
        limits: &IndexLimits,
    ) -> Result<Arc<CodeGraph>, IndexError> {
        let root = super::canonical(root);
        let files = collect_files_limited(&root, limits)?;
        let fp = fingerprint(&files);
        if let Some((cfp, g)) = self.cache.lock().unwrap().as_ref() {
            if *cfp == fp {
                return Ok(g.clone());
            }
        }
        let g = Arc::new(build_from_files(&root, files));
        *self.cache.lock().unwrap() = Some((fp, g.clone()));
        Ok(g)
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

    #[test]
    fn oversize_file_limit_aborts_walk() {
        // 7 source files > cap of 4 → the walk must abort with the oversize error,
        // never return a silent partial set of the first few files, and never walk on.
        let d = tempfile::tempdir().unwrap();
        for i in 0..7 {
            std::fs::write(d.path().join(format!("f{i}.rs")), "fn f() {}\n").unwrap();
        }
        let limits = IndexLimits {
            max_files: 4,
            ..IndexLimits::default()
        };
        let r = collect_files_limited(d.path(), &limits);
        assert!(
            r.is_err(),
            "oversize tree must error, not walk on: {:?}",
            r.as_ref().map(|v| v.len())
        );
        let reason = r.unwrap_err().reason;
        assert!(reason.contains("too large to index"), "{}", reason);
        assert!(reason.contains("source files"), "{}", reason);
    }

    #[test]
    fn oversize_byte_limit_aborts_walk() {
        // 40 files (far under the file cap) but ~148 KiB of source > the 128 KiB cap.
        let chunk = "fn f() { let x = 1234567890; }\n".repeat(120);
        let d = tempfile::tempdir().unwrap();
        for i in 0..40 {
            std::fs::write(d.path().join(format!("f{i}.rs")), &chunk).unwrap();
        }
        let limits = IndexLimits {
            max_total_bytes: 128 * 1024,
            ..IndexLimits::default()
        };
        let r = collect_files_limited(d.path(), &limits);
        assert!(
            r.is_err(),
            "oversize tree must error on the byte cap: {:?}",
            r.as_ref().map(|v| v.len())
        );
        let reason = r.unwrap_err().reason;
        assert!(reason.contains("too large to index"), "{}", reason);
        assert!(reason.contains("MiB of source"), "{}", reason);
    }

    #[test]
    fn default_excludes_skip_temp_and_build_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("main.rs"), "fn main() {}\n").unwrap();
        for sub in ["node_modules", "target", ".venv", "tmp"] {
            let dir = d.path().join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("lib.rs"), "fn dep() {}\n").unwrap();
        }
        // The Windows layout behind issue #1538, exercised cross-platform:
        // `AppData\Local\Temp` source must never be walked (AppData is name-excluded).
        let appdata_temp = d.path().join("AppData/Local/Temp");
        std::fs::create_dir_all(&appdata_temp).unwrap();
        std::fs::write(appdata_temp.join("junk.rs"), "fn j() {}\n").unwrap();

        let files = collect_files_limited(d.path(), &IndexLimits::default()).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|w| w.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["main.rs".to_string()],
            "only the normal source file may be walked; walked: {names:?}"
        );
    }
}
