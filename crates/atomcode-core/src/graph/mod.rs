pub mod indexer;
pub mod persist;
pub mod resolve;

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Unique identifier for a symbol node in the graph.
pub type SymbolId = u64;

/// The kind of symbol (function, struct, trait, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Class,
    Trait,
    Interface,
    Enum,
    Constant,
    Variable,
    Module,
    Import,
    TypeAlias,
    Other(String),
}

/// Visibility of a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Public,
    Private,
    Protected,
    Internal,
    Unknown,
}

/// A node representing a code symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolNode {
    pub id: SymbolId,
    pub name: String,
    pub kind: SymbolKind,
    pub visibility: Visibility,
    pub file: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    /// Optional short signature or doc string.
    pub signature: Option<String>,
}

/// The kind of relationship between two symbols.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    Calls,
    Imports,
    Inherits,
    Implements,
    References,
}

/// A directed edge in the code graph.
///
/// In `edges_out`, `to` is the callee/target.
/// In `edges_in`, `to` stores the caller/source (i.e. the "from" SymbolId).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub to: SymbolId,
    pub kind: EdgeKind,
    pub line: usize,
}

/// Cross-file code knowledge graph indexing symbol relationships.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeGraph {
    pub nodes: HashMap<SymbolId, SymbolNode>,
    pub edges_out: HashMap<SymbolId, Vec<Edge>>,
    pub edges_in: HashMap<SymbolId, Vec<Edge>>,
    pub file_symbols: HashMap<PathBuf, Vec<SymbolId>>,
    pub file_mtimes: HashMap<PathBuf, u64>,
}

impl CodeGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges_out: HashMap::new(),
            edges_in: HashMap::new(),
            file_symbols: HashMap::new(),
            file_mtimes: HashMap::new(),
        }
    }

    /// Deterministic symbol ID from file path, name, and start line.
    pub fn make_id(file: &PathBuf, name: &str, start_line: usize) -> SymbolId {
        let mut hasher = DefaultHasher::new();
        file.hash(&mut hasher);
        name.hash(&mut hasher);
        start_line.hash(&mut hasher);
        hasher.finish()
    }

    /// Add a symbol node to the graph.
    pub fn add_symbol(&mut self, node: SymbolNode) {
        let id = node.id;
        let file = node.file.clone();
        self.nodes.insert(id, node);
        self.file_symbols.entry(file).or_default().push(id);
    }

    /// Add a directed edge from `from` to the target in `edge`.
    pub fn add_edge(&mut self, from: SymbolId, edge: Edge) {
        let to = edge.to;
        let kind = edge.kind.clone();
        let line = edge.line;

        self.edges_out.entry(from).or_default().push(edge);
        self.edges_in.entry(to).or_default().push(Edge {
            to: from,
            kind,
            line,
        });
    }

    /// Look up a node by ID.
    pub fn node(&self, id: SymbolId) -> Option<&SymbolNode> {
        self.nodes.get(&id)
    }

    /// Get all symbol IDs in a file.
    pub fn symbols_in_file(&self, file: &PathBuf) -> Option<&Vec<SymbolId>> {
        self.file_symbols.get(file)
    }

    /// Get outgoing edges (callees / targets) from a symbol.
    pub fn callees(&self, id: SymbolId) -> Option<&Vec<Edge>> {
        self.edges_out.get(&id)
    }

    /// Get incoming edges (callers / sources) to a symbol.
    pub fn callers(&self, id: SymbolId) -> Option<&Vec<Edge>> {
        self.edges_in.get(&id)
    }

    /// Remove all symbols belonging to a file and clean up associated edges.
    pub fn remove_file(&mut self, file: &PathBuf) {
        let symbol_ids = match self.file_symbols.remove(file) {
            Some(ids) => ids,
            None => return,
        };

        for &id in &symbol_ids {
            self.nodes.remove(&id);

            // Remove outgoing edges and clean corresponding incoming entries
            if let Some(out_edges) = self.edges_out.remove(&id) {
                for edge in &out_edges {
                    if let Some(in_list) = self.edges_in.get_mut(&edge.to) {
                        in_list.retain(|e| e.to != id);
                        if in_list.is_empty() {
                            self.edges_in.remove(&edge.to);
                        }
                    }
                }
            }

            // Remove incoming edges and clean corresponding outgoing entries
            if let Some(in_edges) = self.edges_in.remove(&id) {
                for edge in &in_edges {
                    if let Some(out_list) = self.edges_out.get_mut(&edge.to) {
                        out_list.retain(|e| e.to != id);
                        if out_list.is_empty() {
                            self.edges_out.remove(&edge.to);
                        }
                    }
                }
            }
        }

        self.file_mtimes.remove(file);
    }

    /// Find symbols by name (exact match).
    pub fn find_by_name(&self, name: &str) -> Vec<&SymbolNode> {
        self.nodes.values().filter(|n| n.name == name).collect()
    }

    /// Total number of symbol nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Total number of indexed files.
    pub fn file_count(&self) -> usize {
        self.file_symbols.len()
    }

    /// Whether the graph has any data.
    pub fn is_ready(&self) -> bool {
        !self.nodes.is_empty()
    }

    /// BFS reverse traversal — find all transitive callers up to `max_depth`.
    pub fn trace_callers(&self, id: SymbolId, max_depth: usize) -> Vec<(SymbolId, usize)> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut result = Vec::new();
        visited.insert(id);
        queue.push_back((id, 0usize));
        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            if let Some(edges) = self.callers(current) {
                for edge in edges {
                    if visited.insert(edge.to) {
                        result.push((edge.to, depth + 1));
                        queue.push_back((edge.to, depth + 1));
                    }
                }
            }
        }
        result
    }

    /// BFS forward traversal — find all transitive callees up to `max_depth`.
    pub fn trace_callees(&self, id: SymbolId, max_depth: usize) -> Vec<(SymbolId, usize)> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut result = Vec::new();
        visited.insert(id);
        queue.push_back((id, 0usize));
        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            if let Some(edges) = self.callees(current) {
                for edge in edges {
                    if visited.insert(edge.to) {
                        result.push((edge.to, depth + 1));
                        queue.push_back((edge.to, depth + 1));
                    }
                }
            }
        }
        result
    }

    /// BFS shortest path from `from` to `to`, max 10 hops.
    /// Returns the ordered path `[from, ..., to]` or `None`.
    pub fn shortest_path(&self, from: SymbolId, to: SymbolId) -> Option<Vec<SymbolId>> {
        if from == to {
            return Some(vec![from]);
        }
        let max_hops = 10;
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut parent: HashMap<SymbolId, SymbolId> = HashMap::new();
        visited.insert(from);
        queue.push_back((from, 0usize));
        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            if let Some(edges) = self.callees(current) {
                for edge in edges {
                    if visited.insert(edge.to) {
                        parent.insert(edge.to, current);
                        if edge.to == to {
                            // Reconstruct path
                            let mut path = vec![to];
                            let mut cur = to;
                            while let Some(&p) = parent.get(&cur) {
                                path.push(p);
                                cur = p;
                            }
                            path.reverse();
                            return Some(path);
                        }
                        queue.push_back((edge.to, depth + 1));
                    }
                }
            }
        }
        None
    }

    /// Find all files that depend on (call into) symbols in the given file.
    pub fn file_dependents(&self, file: &Path, max_depth: usize) -> Vec<PathBuf> {
        let file_buf = file.to_path_buf();
        let symbol_ids = match self.file_symbols.get(&file_buf) {
            Some(ids) => ids.clone(),
            None => return Vec::new(),
        };
        let mut dependent_files = HashSet::new();
        for &sym_id in &symbol_ids {
            for (caller_id, _depth) in self.trace_callers(sym_id, max_depth) {
                if let Some(node) = self.node(caller_id) {
                    if node.file != file_buf {
                        dependent_files.insert(node.file.clone());
                    }
                }
            }
        }
        dependent_files.into_iter().collect()
    }

    /// Generate a dependency summary for a file: what it uses, what uses it.
    pub fn file_dependency_summary(&self, filename: &str) -> Option<String> {
        let (path, sym_ids) = self.file_symbols.iter().find(|(p, _)| {
            p.file_name()
                .map(|f| f.to_string_lossy() == filename)
                .unwrap_or(false)
        })?;
        let path = path.clone();

        let mut uses: Vec<String> = Vec::new();
        for &sid in sym_ids.iter().take(10) {
            if let Some(edges) = self.callees(sid) {
                for edge in edges {
                    if let Some(node) = self.node(edge.to) {
                        if node.file != path {
                            let f = node
                                .file
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default();
                            if !uses.contains(&f) {
                                uses.push(f);
                            }
                        }
                    }
                }
            }
        }

        let deps = self.file_dependents(&path, 2);
        let dep_names: Vec<String> = deps
            .iter()
            .filter_map(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
            .collect();

        if uses.is_empty() && dep_names.is_empty() {
            return None;
        }

        let mut info = format!("[Graph: {}]", filename);
        if !uses.is_empty() {
            info.push_str(&format!(" uses: {}", uses.join(", ")));
        }
        if !dep_names.is_empty() {
            info.push_str(&format!(" | used by: {}", dep_names.join(", ")));
        }
        Some(info)
    }

    /// Generate a call chain summary for a function.
    /// Skips struct/enum/trait nodes (they don't have call edges).
    /// If multiple functions match, picks the one with the most callees.
    pub fn call_chain_summary(&self, fn_name: &str) -> Option<String> {
        let symbols = self.find_by_name(fn_name);
        // Filter to functions/methods only, pick the one with most callees
        let sym = symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
            .max_by_key(|s| self.callees(s.id).map(|e| e.len()).unwrap_or(0))?;

        let callees = self.trace_callees(sym.id, 3);
        if callees.is_empty() {
            return None;
        }

        let mut chain = format!("[Call chain: {}()", fn_name);
        for (callee_id, depth) in &callees {
            if let Some(node) = self.node(*callee_id) {
                let short_file = node
                    .file
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                let indent = " → ".repeat(*depth);
                chain.push_str(&format!(
                    "\n  {}{}() ({}:{})",
                    indent, node.name, short_file, node.start_line
                ));
            }
        }
        chain.push_str("]\n");
        chain.push_str(
            "[SCOPE: The issue is in ONE of these functions. \
            Read ONLY these files — do not explore outside this chain.]",
        );
        Some(chain)
    }
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new()
    }
}
