//! Cross-file code graph: symbol nodes + call edges, with BFS traversal. Ported from
//! production `graph/mod.rs` (the model + traversal the 5 graph tools need; persistence,
//! incremental remove_file, and the summary helpers are omitted — not used here).
//!
//! EDGE CONVENTION (load-bearing, from production): `edges_out[from]` holds `Edge{to:
//! callee}` (forward); `edges_in[to]` holds `Edge{to: from}` — i.e. in the reverse map
//! the `to` field stores the SOURCE (caller). Do not "fix" this.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

pub type SymbolId = u64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Public,
    Private,
    Protected,
    Internal,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolNode {
    pub id: SymbolId,
    pub name: String,
    pub kind: SymbolKind,
    pub visibility: Visibility,
    pub file: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    Calls,
    Imports,
    Inherits,
    Implements,
    References,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub to: SymbolId,
    pub kind: EdgeKind,
    pub line: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodeGraph {
    pub nodes: HashMap<SymbolId, SymbolNode>,
    pub edges_out: HashMap<SymbolId, Vec<Edge>>,
    pub edges_in: HashMap<SymbolId, Vec<Edge>>,
    pub file_symbols: HashMap<PathBuf, Vec<SymbolId>>,
    pub file_mtimes: HashMap<PathBuf, u64>,
    /// name → symbol ids. Derivable from `nodes`; `#[serde(skip)]` keeps it out of any
    /// serialized form, so `rebuild_name_index` must be called after a deserialize.
    /// Lets `find_by_name` be O(candidates) instead of an O(nodes) scan.
    #[serde(skip)]
    pub by_name: HashMap<String, Vec<SymbolId>>,
}

impl CodeGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Deterministic id from (file, name, start_line) — stable across runs.
    pub fn make_id(file: &Path, name: &str, start_line: usize) -> SymbolId {
        let mut h = DefaultHasher::new();
        file.hash(&mut h);
        name.hash(&mut h);
        start_line.hash(&mut h);
        h.finish()
    }

    pub fn add_symbol(&mut self, node: SymbolNode) {
        let id = node.id;
        let file = node.file.clone();
        self.by_name.entry(node.name.clone()).or_default().push(id);
        self.nodes.insert(id, node);
        self.file_symbols.entry(file).or_default().push(id);
    }

    pub fn add_edge(&mut self, from: SymbolId, edge: Edge) {
        let to = edge.to;
        let kind = edge.kind.clone();
        let line = edge.line;
        self.edges_out.entry(from).or_default().push(edge);
        // reverse map stores the SOURCE in `to` (production convention).
        self.edges_in.entry(to).or_default().push(Edge { to: from, kind, line });
    }

    pub fn node(&self, id: SymbolId) -> Option<&SymbolNode> {
        self.nodes.get(&id)
    }
    pub fn symbols_in_file(&self, file: &Path) -> Option<&Vec<SymbolId>> {
        self.file_symbols.get(file)
    }
    pub fn callees(&self, id: SymbolId) -> Option<&Vec<Edge>> {
        self.edges_out.get(&id)
    }
    pub fn callers(&self, id: SymbolId) -> Option<&Vec<Edge>> {
        self.edges_in.get(&id)
    }
    pub fn find_by_name(&self, name: &str) -> Vec<&SymbolNode> {
        match self.by_name.get(name) {
            Some(ids) => ids.iter().filter_map(|id| self.nodes.get(id)).collect(),
            None => Vec::new(),
        }
    }

    /// Rebuild `by_name` from `nodes`. Call after constructing a graph by any path that
    /// bypasses `add_symbol` (e.g. a serde deserialize, which skips `by_name`).
    pub fn rebuild_name_index(&mut self) {
        self.by_name.clear();
        for (id, node) in &self.nodes {
            self.by_name.entry(node.name.clone()).or_default().push(*id);
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// BFS over incoming edges (who calls `id`), up to `max_depth`. Returns
    /// (caller_id, depth) pairs, deduped.
    pub fn trace_callers(&self, id: SymbolId, max_depth: usize) -> Vec<(SymbolId, usize)> {
        self.trace(id, max_depth, true)
    }
    /// BFS over outgoing edges (what `id` calls), up to `max_depth`.
    pub fn trace_callees(&self, id: SymbolId, max_depth: usize) -> Vec<(SymbolId, usize)> {
        self.trace(id, max_depth, false)
    }
    fn trace(&self, id: SymbolId, max_depth: usize, callers: bool) -> Vec<(SymbolId, usize)> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut result = Vec::new();
        visited.insert(id);
        queue.push_back((id, 0usize));
        while let Some((cur, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let edges = if callers { self.callers(cur) } else { self.callees(cur) };
            if let Some(edges) = edges {
                for e in edges {
                    if visited.insert(e.to) {
                        result.push((e.to, depth + 1));
                        queue.push_back((e.to, depth + 1));
                    }
                }
            }
        }
        result
    }

    /// Shortest forward call path `from → … → to` (BFS, ≤10 hops). Includes both ends.
    pub fn shortest_path(&self, from: SymbolId, to: SymbolId) -> Option<Vec<SymbolId>> {
        if from == to {
            return Some(vec![from]);
        }
        const MAX_HOPS: usize = 10;
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut parent: HashMap<SymbolId, SymbolId> = HashMap::new();
        visited.insert(from);
        queue.push_back((from, 0usize));
        while let Some((cur, depth)) = queue.pop_front() {
            if depth >= MAX_HOPS {
                continue;
            }
            if let Some(edges) = self.callees(cur) {
                for e in edges {
                    if visited.insert(e.to) {
                        parent.insert(e.to, cur);
                        if e.to == to {
                            let mut path = vec![to];
                            let mut c = to;
                            while let Some(&p) = parent.get(&c) {
                                path.push(p);
                                c = p;
                            }
                            path.reverse();
                            return Some(path);
                        }
                        queue.push_back((e.to, depth + 1));
                    }
                }
            }
        }
        None
    }

    /// Files (other than `file`) whose symbols transitively call into `file`'s symbols.
    pub fn file_dependents(&self, file: &Path, max_depth: usize) -> Vec<PathBuf> {
        let fb = file.to_path_buf();
        let ids = match self.file_symbols.get(&fb) {
            Some(i) => i.clone(),
            None => return Vec::new(),
        };
        let mut deps = HashSet::new();
        for sid in ids {
            for (cid, _) in self.trace_callers(sid, max_depth) {
                if let Some(n) = self.node(cid) {
                    if n.file != fb {
                        deps.insert(n.file.clone());
                    }
                }
            }
        }
        deps.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn node(id: SymbolId, name: &str, file: &str) -> SymbolNode {
        SymbolNode {
            id,
            name: name.into(),
            kind: SymbolKind::Function,
            visibility: Visibility::Unknown,
            file: PathBuf::from(file),
            start_line: 1,
            end_line: 2,
            signature: None,
        }
    }

    // a → b → c
    fn chain() -> CodeGraph {
        let mut g = CodeGraph::new();
        g.add_symbol(node(1, "a", "a.rs"));
        g.add_symbol(node(2, "b", "b.rs"));
        g.add_symbol(node(3, "c", "c.rs"));
        g.add_edge(1, Edge { to: 2, kind: EdgeKind::Calls, line: 1 });
        g.add_edge(2, Edge { to: 3, kind: EdgeKind::Calls, line: 1 });
        g
    }

    #[test]
    fn callees_and_callers() {
        let g = chain();
        assert_eq!(g.callees(1).unwrap()[0].to, 2);
        assert_eq!(g.callers(3).unwrap()[0].to, 2); // reverse map stores source
    }

    #[test]
    fn trace_callees_depth() {
        let g = chain();
        let r = g.trace_callees(1, 5);
        let ids: Vec<SymbolId> = r.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&2) && ids.contains(&3), "{ids:?}");
        // depth-limited: only direct callee
        let r1 = g.trace_callees(1, 1);
        assert_eq!(r1, vec![(2, 1)]);
    }

    #[test]
    fn trace_callers_reverse() {
        let g = chain();
        let r = g.trace_callers(3, 5);
        let ids: Vec<SymbolId> = r.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&2) && ids.contains(&1), "{ids:?}");
    }

    #[test]
    fn shortest_path_chain() {
        let g = chain();
        assert_eq!(g.shortest_path(1, 3), Some(vec![1, 2, 3]));
        assert_eq!(g.shortest_path(3, 1), None);
        assert_eq!(g.shortest_path(2, 2), Some(vec![2]));
    }

    #[test]
    fn file_dependents_excludes_self() {
        let g = chain();
        let deps = g.file_dependents(&PathBuf::from("c.rs"), 5);
        assert!(deps.contains(&PathBuf::from("a.rs")));
        assert!(deps.contains(&PathBuf::from("b.rs")));
        assert!(!deps.contains(&PathBuf::from("c.rs")));
    }

    #[test]
    fn name_index_finds_all_same_named() {
        let mut g = CodeGraph::new();
        g.add_symbol(node(1, "dup", "a.rs"));
        g.add_symbol(node(2, "dup", "b.rs"));
        g.add_symbol(node(3, "other", "c.rs"));
        assert_eq!(g.find_by_name("dup").len(), 2, "both dup symbols found");
        assert_eq!(g.find_by_name("other").len(), 1);
        assert!(g.find_by_name("missing").is_empty());
    }

    #[test]
    fn rebuild_name_index_restores_lookup() {
        let mut g = CodeGraph::new();
        g.add_symbol(node(1, "dup", "a.rs"));
        g.add_symbol(node(2, "dup", "b.rs"));
        // Simulate a graph produced by a path that bypasses add_symbol (serde skips by_name).
        g.by_name.clear();
        assert!(g.find_by_name("dup").is_empty(), "empty index → lookup misses");
        g.rebuild_name_index();
        assert_eq!(g.find_by_name("dup").len(), 2, "rebuild restores the index");
    }
}
