use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use ignore::WalkBuilder;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

use crate::semantic::language::{Lang, LanguageRegistry};

use super::resolve::resolve_callee;
use super::{CodeGraph, Edge, EdgeKind, SymbolKind, SymbolNode, Visibility};

/// Result of parsing a single file: extracted symbols and raw call edges.
struct FileParseResult {
    symbols: Vec<SymbolNode>,
    raw_calls: Vec<RawCall>,
}

/// A raw (unresolved) call extracted from source.
struct RawCall {
    caller_name: String,
    callee_name: String,
    line: usize,
}

/// Supported extensions for indexing.
const INDEXED_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "go", "java", "c", "cpp", "vue",
];

/// Background indexer that walks a project directory, parses source files
/// with tree-sitter, extracts symbols and call edges, and populates a
/// shared `CodeGraph`.
pub struct GraphIndexer {
    graph: Arc<RwLock<CodeGraph>>,
    project_dir: PathBuf,
    parser: Parser,
}

impl GraphIndexer {
    /// Create a new indexer for the given project directory.
    pub fn new(graph: Arc<RwLock<CodeGraph>>, project_dir: PathBuf) -> Self {
        Self {
            graph,
            project_dir,
            parser: Parser::new(),
        }
    }

    /// Full/incremental index pass.
    ///
    /// 1. Collect files via `ignore::WalkBuilder` (respects .gitignore)
    /// 2. Compare mtimes — only parse dirty/new files
    /// 3. Detect deleted files — remove from graph
    /// 4. Parse dirty files, extract symbols and calls
    /// 5. Resolve calls to edges
    ///
    /// `cancel` is checked at every point where abandoning is cheap: top
    /// of the fn, between parse-file iterations (the expensive step),
    /// and before the final write-lock mutation block. Cancellation is
    /// cooperative — the sync `WalkBuilder` inside `spawn_blocking`
    /// cannot itself be interrupted, so best we can do is skip the
    /// parsing and mutation phases if cancel fires mid-walk. Rapid `/cd`
    /// chains depend on this: without it, each cd spawns a fresh
    /// indexer and all of them parse in parallel.
    pub async fn index_all(&mut self, cancel: CancellationToken) {
        // Refuse to index obvious non-projects. See `should_index`:
        // guards against $HOME / `/` and "umbrella" directories whose
        // children are themselves projects (e.g. `~/project` that
        // contains dozens of repos). Without the umbrella guard a
        // single `/cd ~/project` spawns a full tree-sitter parse of
        // every indexed file across every child repo — pegs CPU and
        // starves the TUI event loop.
        if !should_index(&self.project_dir) {
            return;
        }

        if cancel.is_cancelled() {
            return;
        }

        // Walk + stat the tree on the blocking-thread pool rather than on
        // an async worker. `WalkBuilder` is pure sync I/O; leaving it on
        // an async task blocks that worker for the full walk duration,
        // which (a) starves the select! / timer machinery and (b) defeats
        // tokio's work-stealing (other tasks can't migrate *into* the
        // stuck worker's queue). `spawn_blocking` is exactly what the
        // docs prescribe for this.
        let project_dir = self.project_dir.clone();
        let files = tokio::task::spawn_blocking(move || collect_files_sync(&project_dir))
            .await
            .unwrap_or_default();
        let current_paths: HashSet<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();

        // Snapshot mtimes under a short read lock to determine dirty files.
        let (deleted, dirty_files) = {
            let graph = self.graph.read().await;
            let deleted: Vec<PathBuf> = graph
                .file_mtimes
                .keys()
                .filter(|p| !current_paths.contains(*p))
                .cloned()
                .collect();
            let dirty: Vec<(PathBuf, u64)> = files
                .into_iter()
                .filter(|(path, mtime)| graph.file_mtimes.get(path) != Some(mtime))
                .collect();
            (deleted, dirty)
        };
        // Read lock released here.

        // Parse dirty files OUTSIDE the lock (CPU-intensive, no graph access needed).
        //
        // Two concerns stack on this loop and both fixes apply:
        // 1. **CPU throttle** — tree-sitter parse per file is sync CPU work.
        //    Running it in a tight loop inside an async task pegs one core
        //    at ~99% for the whole initial index, which reads as "atomcode
        //    hogs CPU at startup" on the user's Activity Monitor. Yield
        //    after each file so the runtime can service UI renders / agent
        //    events between parses; sleep briefly every CHUNK files so
        //    cumulative CPU use stays moderate. Total added wall-clock
        //    is tiny (~5 ms × N/CHUNK).
        // 2. **Cancellation** — a stale indexer spawned by a previous
        //    working-dir can burn minutes of CPU after the user has
        //    already `/cd`'d elsewhere. The rapid-cd case spawns a fresh
        //    indexer per cd and without this check they'd all parse in
        //    parallel. Bail at the top of every iteration.
        const CPU_BREATHE_CHUNK: usize = 16;
        const CPU_BREATHE_MS: u64 = 5;
        let mut all_results: Vec<(PathBuf, u64, FileParseResult)> = Vec::new();
        for (i, (path, mtime)) in dirty_files.into_iter().enumerate() {
            if cancel.is_cancelled() {
                return;
            }
            if let Some(result) = self.parse_file(&path) {
                all_results.push((path, mtime, result));
            }
            tokio::task::yield_now().await;
            if i > 0 && i % CPU_BREATHE_CHUNK == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(CPU_BREATHE_MS)).await;
            }
        }

        if deleted.is_empty() && all_results.is_empty() {
            return; // Nothing to update
        }

        // Final cancel check: skip the write-lock critical section if
        // a newer indexer has been spawned. Contention on graph.write()
        // would otherwise delay the new indexer's own write.
        if cancel.is_cancelled() {
            return;
        }

        // Single write lock for ALL mutations — atomic from readers' perspective.
        // Grep/trace_callees will block briefly here but never see partial state.
        let mut graph = self.graph.write().await;

        // Remove deleted files
        for path in &deleted {
            graph.remove_file(path);
        }

        // Remove + re-insert dirty files
        for (path, mtime, result) in &all_results {
            graph.remove_file(path);
            for sym in &result.symbols {
                graph.add_symbol(sym.clone());
            }
            graph.file_mtimes.insert(path.clone(), *mtime);
        }

        // Resolve calls to edges (all symbols inserted, safe to resolve)
        for (_path, _mtime, result) in &all_results {
            for raw_call in &result.raw_calls {
                let caller_candidates = graph.find_by_name(&raw_call.caller_name);
                let caller_id = caller_candidates.first().map(|s| s.id);
                if let Some(caller_id) = caller_id {
                    let caller_file = graph.node(caller_id).unwrap().file.clone();
                    if let Some(callee_id) =
                        resolve_callee(&graph, &raw_call.callee_name, &caller_file, &[])
                    {
                        graph.add_edge(
                            caller_id,
                            Edge {
                                to: callee_id,
                                kind: EdgeKind::Calls,
                                line: raw_call.line,
                            },
                        );
                    }
                }
            }
        }
        // Write lock released here — graph is fully consistent.
    }

    /// Re-index a single file (for live updates after edit).
    pub async fn reindex_file(&mut self, path: &Path) {
        let mtime = match std::fs::metadata(path) {
            Ok(meta) => {
                use std::time::UNIX_EPOCH;
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }
            Err(_) => {
                // File was deleted
                let mut graph = self.graph.write().await;
                graph.remove_file(&path.to_path_buf());
                return;
            }
        };

        let result = match self.parse_file(path) {
            Some(r) => r,
            None => return,
        };

        let mut graph = self.graph.write().await;
        let path_buf = path.to_path_buf();

        // Remove old data
        graph.remove_file(&path_buf);

        // Insert symbols
        for sym in &result.symbols {
            graph.add_symbol(sym.clone());
        }
        graph.file_mtimes.insert(path_buf.clone(), mtime);

        // Resolve calls
        for raw_call in &result.raw_calls {
            let caller_candidates = graph.find_by_name(&raw_call.caller_name);
            let caller_id = caller_candidates.first().map(|s| s.id);

            if let Some(caller_id) = caller_id {
                let caller_file = graph.node(caller_id).unwrap().file.clone();
                if let Some(callee_id) =
                    resolve_callee(&graph, &raw_call.callee_name, &caller_file, &[])
                {
                    graph.add_edge(
                        caller_id,
                        Edge {
                            to: callee_id,
                            kind: EdgeKind::Calls,
                            line: raw_call.line,
                        },
                    );
                }
            }
        }
    }

    /// Parse a single file: extract symbols and raw calls.
    fn parse_file(&mut self, path: &Path) -> Option<FileParseResult> {
        let source = std::fs::read_to_string(path).ok()?;
        let lang = LanguageRegistry::detect(path)?;

        self.parser.set_language(&lang.grammar()).ok()?;
        let tree = self.parser.parse(source.as_bytes(), None)?;

        let symbols = self.extract_symbols(path, &source, lang, &tree);
        let raw_calls = self.extract_calls(path, &source, lang, &tree, &symbols);

        Some(FileParseResult { symbols, raw_calls })
    }

    /// Extract symbol definitions from a parsed tree using the language's symbols_query.
    fn extract_symbols(
        &self,
        path: &Path,
        source: &str,
        lang: Lang,
        tree: &tree_sitter::Tree,
    ) -> Vec<SymbolNode> {
        let query_src = lang.symbols_query();
        let query = match Query::new(&lang.grammar(), query_src) {
            Ok(q) => q,
            Err(_) => return Vec::new(),
        };

        let def_idx = match query.capture_index_for_name("definition") {
            Some(i) => i,
            None => return Vec::new(),
        };
        let name_idx = match query.capture_index_for_name("name") {
            Some(i) => i,
            None => return Vec::new(),
        };

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

        let mut symbols = Vec::new();
        let mut seen_ranges: HashSet<(usize, usize)> = HashSet::new();
        let path_buf = path.to_path_buf();

        loop {
            matches.advance();
            let m = match matches.get() {
                Some(m) => m,
                None => break,
            };

            let mut sym_name = None;
            let mut def_start_line = 0usize;
            let mut def_end_line = 0usize;
            let mut def_start_byte = 0usize;
            let mut def_end_byte = 0usize;
            let mut ts_kind = "";
            let mut has_def = false;

            for capture in m.captures {
                if capture.index == name_idx {
                    sym_name = Some(
                        source[capture.node.start_byte()..capture.node.end_byte()].to_string(),
                    );
                }
                if capture.index == def_idx {
                    def_start_byte = capture.node.start_byte();
                    def_end_byte = capture.node.end_byte();
                    def_start_line = capture.node.start_position().row + 1; // 1-indexed
                    def_end_line = capture.node.end_position().row + 1;
                    ts_kind = capture.node.kind();
                    has_def = true;
                }
            }

            if let (Some(name), true) = (sym_name, has_def) {
                let range = (def_start_byte, def_end_byte);
                if seen_ranges.contains(&range) {
                    continue;
                }
                seen_ranges.insert(range);

                let id = CodeGraph::make_id(&path_buf, &name, def_start_line);
                let kind = classify_symbol_kind(ts_kind);

                symbols.push(SymbolNode {
                    id,
                    name,
                    kind,
                    visibility: Visibility::Unknown,
                    file: path_buf.clone(),
                    start_line: def_start_line,
                    end_line: def_end_line,
                    signature: None,
                });
            }
        }

        symbols
    }

    /// Extract raw call edges from a parsed tree using the language's calls_query.
    fn extract_calls(
        &self,
        _path: &Path,
        source: &str,
        lang: Lang,
        tree: &tree_sitter::Tree,
        symbols: &[SymbolNode],
    ) -> Vec<RawCall> {
        let query_src = match lang.calls_query() {
            Some(q) => q,
            None => return Vec::new(),
        };

        let query = match Query::new(&lang.grammar(), query_src) {
            Ok(q) => q,
            Err(_) => return Vec::new(),
        };

        let callee_idx = match query.capture_index_for_name("callee") {
            Some(i) => i,
            None => return Vec::new(),
        };

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

        let mut raw_calls = Vec::new();

        loop {
            matches.advance();
            let m = match matches.get() {
                Some(m) => m,
                None => break,
            };

            for capture in m.captures {
                if capture.index == callee_idx {
                    let callee_name =
                        source[capture.node.start_byte()..capture.node.end_byte()].to_string();
                    let call_line = capture.node.start_position().row + 1; // 1-indexed

                    // Find enclosing function
                    let caller_name = symbols
                        .iter()
                        .filter(|s| {
                            matches!(s.kind, SymbolKind::Function | SymbolKind::Method)
                                && s.start_line <= call_line
                                && call_line <= s.end_line
                        })
                        .last()
                        .map(|s| s.name.clone());

                    if let Some(caller_name) = caller_name {
                        // Skip self-calls
                        if caller_name == callee_name {
                            continue;
                        }

                        raw_calls.push(RawCall {
                            caller_name,
                            callee_name,
                            line: call_line,
                        });
                    }
                }
            }
        }

        raw_calls
    }
}

/// Map tree-sitter node kind strings to `SymbolKind`.
fn classify_symbol_kind(ts_kind: &str) -> SymbolKind {
    match ts_kind {
        "function_item" | "function_definition" | "function_declaration" | "func_literal" => {
            SymbolKind::Function
        }
        "method_definition" | "method_declaration" => SymbolKind::Method,
        "struct_item" | "struct_specifier" => SymbolKind::Struct,
        "class_definition" | "class_declaration" | "class_specifier" => SymbolKind::Class,
        "trait_item" => SymbolKind::Trait,
        "interface_declaration" => SymbolKind::Interface,
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

/// True when `path` is the user's HOME directory or the filesystem root.
/// Either one hosts a massive tree the indexer should never walk in full.
/// Policy: should the indexer walk this directory?
///
/// Used both internally (`index_all` top guard) and externally by
/// `agent/services.rs` to decide whether to even spawn the indexer
/// task after a `/cd`. Returns `false` for:
///
/// - `$HOME` or `/` — walking pulls in Library / Downloads / Documents
///   trees with hundreds of thousands of paths.
/// - "Umbrella" dirs — the target itself has no project marker but
///   three or more of its immediate child dirs are projects (e.g.
///   `~/project` with 30+ repos inside). Indexing walks every child.
///
/// The escape hatch is [`looks_like_project`]: drop a `.atomcode`
/// (or any other marker) at the root and indexing kicks in.
pub fn should_index(project_dir: &Path) -> bool {
    if looks_like_project(project_dir) {
        return true;
    }
    if is_home_or_root(project_dir) {
        return false;
    }
    if is_umbrella_dir(project_dir) {
        return false;
    }
    true
}

fn is_home_or_root(path: &Path) -> bool {
    if path == Path::new("/") {
        return true;
    }
    if let Some(home) = crate::tool::real_home_dir() {
        if path == home.as_path() {
            return true;
        }
    }
    false
}

/// Detect an "umbrella" directory: the target itself has no project
/// marker, but 3+ of its immediate child directories are projects.
/// Covers the common `~/project/` or `~/code/` layout where the user
/// keeps many repos side-by-side. Indexing such a dir walks every
/// child — full tree-sitter parse across dozens of repos, minutes of
/// CPU, starved TUI event loop.
///
/// Scan is capped at 200 immediate entries so the guard itself stays
/// cheap. Returns on the third match.
fn is_umbrella_dir(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut project_children = 0;
    for entry in entries.flatten().take(200) {
        let p = entry.path();
        if p.is_dir() && looks_like_project(&p) {
            project_children += 1;
            if project_children >= 3 {
                return true;
            }
        }
    }
    false
}

/// Cheap "is this a project?" heuristic — checks for a user-maintained
/// project-marker file or directory at the root.
///
/// `.atomcode` is intentionally NOT in this list even though atomcode
/// writes `.atomcode/graph.bin` there: using atomcode's own storage
/// dir as a "user opt-in" marker is self-fulfilling — the very first
/// `/cd` to any directory creates `.atomcode/` and pins that dir as
/// "project" forever, defeating the umbrella / $HOME guards. Only
/// user-placed markers count.
fn looks_like_project(dir: &Path) -> bool {
    const MARKERS: &[&str] = &[
        ".git",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
    ];
    MARKERS.iter().any(|m| dir.join(m).exists())
}

/// Free-function form of the file walk so `tokio::task::spawn_blocking`
/// can own it cleanly (taking only `&Path`, not `&self`).
fn collect_files_sync(project_dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut files = Vec::new();

    let walker = WalkBuilder::new(project_dir)
        .hidden(true)
        .git_ignore(true)
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };

        if !INDEXED_EXTENSIONS.contains(&ext) {
            continue;
        }

        let mtime = match entry.metadata() {
            Ok(meta) => {
                use std::time::UNIX_EPOCH;
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }
            Err(_) => 0,
        };

        files.push((path.to_path_buf(), mtime));
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(parent: &Path, name: &str, markers: &[&str]) {
        let p = parent.join(name);
        std::fs::create_dir_all(&p).unwrap();
        for m in markers {
            std::fs::write(p.join(m), "").unwrap();
        }
    }

    #[test]
    fn should_index_accepts_marked_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        assert!(should_index(tmp.path()));
    }

    #[test]
    fn should_index_refuses_umbrella_dir_with_many_child_projects() {
        // Layout: umbrella/{a,b,c,d}/ each with .git — no marker at umbrella root.
        let tmp = tempfile::TempDir::new().unwrap();
        mk(tmp.path(), "a", &[".git"]);
        mk(tmp.path(), "b", &[".git"]);
        mk(tmp.path(), "c", &["package.json"]);
        mk(tmp.path(), "d", &["Cargo.toml"]);
        assert!(
            !should_index(tmp.path()),
            "umbrella of 4 projects without own marker must be skipped"
        );
    }

    #[test]
    fn should_index_accepts_umbrella_with_real_marker() {
        // Umbrella layout, but the umbrella itself also has a real
        // user-placed marker (e.g. a root Cargo workspace). Under those
        // circumstances indexing is intentional.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]").unwrap();
        mk(tmp.path(), "a", &[".git"]);
        mk(tmp.path(), "b", &[".git"]);
        mk(tmp.path(), "c", &[".git"]);
        assert!(
            should_index(tmp.path()),
            "user-placed marker must override umbrella detection"
        );
    }

    /// Regression: `.atomcode` is atomcode's own storage dir (it gets
    /// created by `graph::persist::save` on every successful index).
    /// Using it as a project-marker is self-fulfilling — the very first
    /// /cd to ~/project writes ~/project/.atomcode/graph.bin, and from
    /// then on ~/project looks "marked" and the umbrella guard
    /// never fires again. User-reported: `/cd ~/project` still spiked
    /// CPU after the umbrella guard landed, because an earlier run
    /// had already planted .atomcode there.
    #[test]
    fn should_index_refuses_umbrella_with_only_atomcode_storage_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Simulate the state left by a prior indexer run.
        std::fs::create_dir_all(tmp.path().join(".atomcode")).unwrap();
        std::fs::write(tmp.path().join(".atomcode").join("graph.bin"), b"x").unwrap();
        // And the umbrella shape.
        mk(tmp.path(), "a", &[".git"]);
        mk(tmp.path(), "b", &[".git"]);
        mk(tmp.path(), "c", &[".git"]);
        assert!(
            !should_index(tmp.path()),
            ".atomcode dir must not rescue an umbrella from the guard"
        );
    }

    #[test]
    fn should_index_accepts_dir_with_fewer_than_3_child_projects() {
        // Two child projects doesn't qualify as umbrella — could be a
        // monorepo with a couple of sub-packages.
        let tmp = tempfile::TempDir::new().unwrap();
        mk(tmp.path(), "a", &[".git"]);
        mk(tmp.path(), "b", &[".git"]);
        mk(tmp.path(), "other", &[]); // not a project
        assert!(
            should_index(tmp.path()),
            "2 child projects < umbrella threshold"
        );
    }

    /// Regression: a pre-cancelled token must cause index_all to bail
    /// before doing any parse / write-lock work. This is the guarantee
    /// that makes rapid `/cd` chains cheap — each new /cd cancels the
    /// prior indexer, which then cooperatively exits at its next
    /// cancel check (top of fn / between parses / before write-lock).
    #[tokio::test]
    async fn index_all_bails_on_cancelled_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Marker so should_index returns true (we want the cancel
        // check to be what stops us, not the umbrella guard).
        std::fs::write(tmp.path().join(".atomcode"), "").unwrap();
        // Drop a Rust source so there'd be real work if we proceeded.
        std::fs::write(
            tmp.path().join("lib.rs"),
            "pub fn foo() {}\npub fn bar() {}\n",
        )
        .unwrap();

        let graph = Arc::new(RwLock::new(super::super::CodeGraph::default()));
        let mut indexer = GraphIndexer::new(graph.clone(), tmp.path().to_path_buf());

        let cancel = CancellationToken::new();
        cancel.cancel();
        indexer.index_all(cancel).await;

        // Graph must be untouched — cancel fired before any symbol
        // insertion.
        let g = graph.read().await;
        assert!(
            g.file_mtimes.is_empty(),
            "cancelled indexer must not mutate graph"
        );
    }
}
