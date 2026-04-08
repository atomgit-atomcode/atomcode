use std::path::PathBuf;

use atomcode_core::graph::{
    persist, CodeGraph, Edge, EdgeKind, SymbolKind, SymbolNode, Visibility,
};

fn make_test_symbol(file: &PathBuf, name: &str, line: usize) -> SymbolNode {
    SymbolNode {
        id: CodeGraph::make_id(file, name, line),
        name: name.to_string(),
        kind: SymbolKind::Function,
        visibility: Visibility::Public,
        file: file.clone(),
        start_line: line,
        end_line: line + 10,
        signature: None,
    }
}

#[test]
fn test_add_symbol_and_edge() {
    let mut graph = CodeGraph::new();
    let file = PathBuf::from("src/main.rs");

    let sym_a = make_test_symbol(&file, "foo", 1);
    let sym_b = make_test_symbol(&file, "bar", 20);
    let id_a = sym_a.id;
    let id_b = sym_b.id;

    graph.add_symbol(sym_a);
    graph.add_symbol(sym_b);

    graph.add_edge(
        id_a,
        Edge {
            to: id_b,
            kind: EdgeKind::Calls,
            line: 5,
        },
    );

    // foo calls bar
    let callees = graph.callees(id_a).unwrap();
    assert_eq!(callees.len(), 1);
    assert_eq!(callees[0].to, id_b);
    assert_eq!(callees[0].kind, EdgeKind::Calls);

    // bar is called by foo
    let callers = graph.callers(id_b).unwrap();
    assert_eq!(callers.len(), 1);
    assert_eq!(callers[0].to, id_a);

    assert_eq!(graph.node_count(), 2);
    assert!(graph.is_ready());
}

#[test]
fn test_file_symbols() {
    let mut graph = CodeGraph::new();
    let file = PathBuf::from("src/lib.rs");

    let sym = make_test_symbol(&file, "helper", 10);
    let id = sym.id;
    graph.add_symbol(sym);

    let ids = graph.symbols_in_file(&file).unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], id);

    assert_eq!(graph.file_count(), 1);
}

#[test]
fn test_remove_file() {
    let mut graph = CodeGraph::new();
    let file_a = PathBuf::from("src/a.rs");
    let file_b = PathBuf::from("src/b.rs");

    let sym_a = make_test_symbol(&file_a, "alpha", 1);
    let sym_b = make_test_symbol(&file_b, "beta", 1);
    let id_a = sym_a.id;
    let id_b = sym_b.id;

    graph.add_symbol(sym_a);
    graph.add_symbol(sym_b);

    graph.add_edge(
        id_a,
        Edge {
            to: id_b,
            kind: EdgeKind::Calls,
            line: 3,
        },
    );

    // Remove file_a — should remove sym_a and clean edges
    graph.remove_file(&file_a);

    assert!(graph.node(id_a).is_none());
    assert!(graph.node(id_b).is_some());
    assert_eq!(graph.node_count(), 1);
    assert!(graph.symbols_in_file(&file_a).is_none());

    // The incoming edge on sym_b from sym_a should be cleaned
    assert!(graph.callers(id_b).is_none());
    // No outgoing edges remain for the removed symbol
    assert!(graph.callees(id_a).is_none());
}

#[test]
fn test_serialize_roundtrip() {
    let mut graph = CodeGraph::new();
    let file = PathBuf::from("src/roundtrip.rs");

    let sym = make_test_symbol(&file, "round", 5);
    let id = sym.id;
    graph.add_symbol(sym);

    let bytes = persist::serialize(&graph).expect("serialize failed");
    let restored = persist::deserialize(&bytes).expect("deserialize failed");

    assert_eq!(restored.node_count(), 1);
    let node = restored.node(id).unwrap();
    assert_eq!(node.name, "round");
    assert_eq!(node.start_line, 5);
    assert!(restored.is_ready());
}
