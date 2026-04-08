use std::path::PathBuf;

use atomcode_core::graph::{
    persist, resolve, CodeGraph, Edge, EdgeKind, SymbolKind, SymbolNode, Visibility,
};
use atomcode_core::semantic::language::Lang;
use tree_sitter::StreamingIterator;

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

#[test]
fn test_resolve_same_file_wins() {
    let mut graph = CodeGraph::new();
    let file_a = PathBuf::from("src/a.rs");
    let file_b = PathBuf::from("src/b.rs");

    let helper_a = make_test_symbol(&file_a, "helper", 10);
    let helper_b = make_test_symbol(&file_b, "helper", 10);
    let id_a = helper_a.id;

    graph.add_symbol(helper_a);
    graph.add_symbol(helper_b);

    // Caller is in file_a, so "helper" in file_a should win
    let resolved = resolve::resolve_callee(&graph, "helper", &file_a, &[]);
    assert_eq!(resolved, Some(id_a));
}

#[test]
fn test_resolve_import_wins_over_distant() {
    let mut graph = CodeGraph::new();
    let file_caller = PathBuf::from("src/app.rs");
    let file_local = PathBuf::from("lib/utils/date.rs");
    let file_distant = PathBuf::from("vendor/legacy/date.rs");

    let fmt_local = make_test_symbol(&file_local, "format_date", 5);
    let fmt_distant = make_test_symbol(&file_distant, "format_date", 5);
    let id_local = fmt_local.id;

    graph.add_symbol(fmt_local);
    graph.add_symbol(fmt_distant);

    // "format_date" from file_local is imported
    let imported = vec!["format_date".to_string()];
    let resolved = resolve::resolve_callee(&graph, "format_date", &file_caller, &imported);
    assert_eq!(resolved, Some(id_local));
}

#[test]
fn test_rust_call_query() {
    let source = b"fn main() { foo(); bar::baz(x); obj.method(42); }";
    let lang = Lang::Rust;

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang.grammar()).unwrap();
    let tree = parser.parse(&source[..], None).unwrap();

    let query_src = lang.calls_query().expect("Rust should have a calls query");
    let query = tree_sitter::Query::new(&lang.grammar(), query_src).unwrap();
    let mut cursor = tree_sitter::QueryCursor::new();

    let mut matches = cursor.matches(&query, tree.root_node(), &source[..]);
    let callee_idx = query
        .capture_index_for_name("callee")
        .unwrap();

    let mut callees: Vec<String> = Vec::new();
    while let Some(m) = matches.next() {
        for cap in m.captures {
            if cap.index == callee_idx {
                let name = cap.node.utf8_text(source).unwrap();
                callees.push(name.to_string());
            }
        }
    }

    assert!(callees.contains(&"foo".to_string()), "missing foo: {:?}", callees);
    assert!(callees.contains(&"baz".to_string()), "missing baz: {:?}", callees);
    assert!(callees.contains(&"method".to_string()), "missing method: {:?}", callees);
    assert_eq!(callees.len(), 3, "expected exactly 3 callees: {:?}", callees);
}
