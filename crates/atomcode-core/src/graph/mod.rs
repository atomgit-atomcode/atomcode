pub mod persist;
pub mod resolve;

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

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
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new()
    }
}
