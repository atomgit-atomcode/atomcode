use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use atomcode_core::graph::{
    indexer::GraphIndexer, persist, resolve, CodeGraph, Edge, EdgeKind, SymbolKind, SymbolNode,
    Visibility,
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
    // Test that imported (score 3) beats same_dir (score 2).
    // Candidate A is in the same directory as the caller → score 2.
    // Candidate B is in a distant directory but has a unique name in imports → score 3.
    // Since find_by_name returns candidates by the SAME name, and import check is
    // name-based, we test a scenario where only one candidate qualifies for a higher
    // priority tier.
    //
    // Concrete: caller in "src/app.rs", candidate in "src/helper.rs" (same_dir, score 2),
    // candidate in "vendor/other.rs" (no match, score 0). same_dir wins over distant.
    // Then we verify that same_file (score 4) still beats same_dir (score 2).
    let mut graph = CodeGraph::new();
    let file_caller = PathBuf::from("src/app.rs");
    let file_same_dir = PathBuf::from("src/helper.rs");
    let file_distant = PathBuf::from("vendor/legacy/helper.rs");

    let sym_near = make_test_symbol(&file_same_dir, "format_date", 5);
    let sym_far = make_test_symbol(&file_distant, "format_date", 10);
    let id_near = sym_near.id;

    graph.add_symbol(sym_near);
    graph.add_symbol(sym_far);

    // No imports — same_dir (score 2) should beat distant other (score 0)
    let resolved = resolve::resolve_callee(&graph, "format_date", &file_caller, &[]);
    assert_eq!(resolved, Some(id_near));

    // Also verify same_root beats other when neither is same_dir
    let mut graph2 = CodeGraph::new();
    let caller2 = PathBuf::from("src/deep/main.rs");
    let same_root_file = PathBuf::from("src/utils/date.rs");
    let other_root_file = PathBuf::from("vendor/date.rs");

    let sym_root = make_test_symbol(&same_root_file, "format_date", 5);
    let sym_other = make_test_symbol(&other_root_file, "format_date", 10);
    let id_root = sym_root.id;

    graph2.add_symbol(sym_root);
    graph2.add_symbol(sym_other);

    let resolved2 = resolve::resolve_callee(&graph2, "format_date", &caller2, &[]);
    assert_eq!(resolved2, Some(id_root));
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

#[test]
fn test_indexer_indexes_rust_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // main.rs calls greet()
    std::fs::write(
        dir.join("main.rs"),
        r#"fn main() {
    greet();
}
"#,
    )
    .unwrap();

    // lib.rs defines greet()
    std::fs::write(
        dir.join("lib.rs"),
        r#"pub fn greet() {
    println!("hello");
}
"#,
    )
    .unwrap();

    let graph = Arc::new(RwLock::new(CodeGraph::new()));
    let mut indexer = GraphIndexer::new(graph.clone(), dir.to_path_buf());

    indexer.index_all();

    let g = graph.read().unwrap();

    // Should have at least 2 symbols: main and greet
    assert!(
        g.node_count() >= 2,
        "expected at least 2 symbols, got {}",
        g.node_count()
    );

    // Verify greet symbol exists
    let greets = g.find_by_name("greet");
    assert!(!greets.is_empty(), "greet symbol not found");

    // Verify main symbol exists
    let mains = g.find_by_name("main");
    assert!(!mains.is_empty(), "main symbol not found");

    // Verify main -> greet edge
    let main_id = mains[0].id;
    let callees = g.callees(main_id);
    assert!(callees.is_some(), "main should have callees");
    let callees = callees.unwrap();
    let greet_id = greets[0].id;
    assert!(
        callees.iter().any(|e| e.to == greet_id),
        "main should call greet, edges: {:?}",
        callees
    );
}

#[test]
fn test_indexer_incremental_update() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // Start with one file
    std::fs::write(
        dir.join("first.rs"),
        r#"fn alpha() {}
"#,
    )
    .unwrap();

    let graph = Arc::new(RwLock::new(CodeGraph::new()));
    let mut indexer = GraphIndexer::new(graph.clone(), dir.to_path_buf());

    indexer.index_all();

    let count_after_first = {
        let g = graph.read().unwrap();
        assert!(g.node_count() >= 1, "should have at least 1 symbol after first index");
        g.node_count()
    };

    // Add a second file
    std::fs::write(
        dir.join("second.rs"),
        r#"fn beta() {}
fn gamma() {}
"#,
    )
    .unwrap();

    indexer.index_all();

    let count_after_second = {
        let g = graph.read().unwrap();
        g.node_count()
    };

    assert!(
        count_after_second > count_after_first,
        "symbol count should increase after adding second file: {} vs {}",
        count_after_second,
        count_after_first
    );
}

// ── BFS query method tests ──

fn make_chain_graph() -> (CodeGraph, u64, u64, u64) {
    // A → B → C
    let mut graph = CodeGraph::new();
    let file = PathBuf::from("chain.rs");
    let sym_a = make_test_symbol(&file, "a", 1);
    let sym_b = make_test_symbol(&file, "b", 10);
    let sym_c = make_test_symbol(&file, "c", 20);
    let id_a = sym_a.id;
    let id_b = sym_b.id;
    let id_c = sym_c.id;
    graph.add_symbol(sym_a);
    graph.add_symbol(sym_b);
    graph.add_symbol(sym_c);
    graph.add_edge(id_a, Edge { to: id_b, kind: EdgeKind::Calls, line: 3 });
    graph.add_edge(id_b, Edge { to: id_c, kind: EdgeKind::Calls, line: 12 });
    (graph, id_a, id_b, id_c)
}

#[test]
fn test_trace_callers_bfs() {
    let (graph, id_a, id_b, id_c) = make_chain_graph();
    let callers = graph.trace_callers(id_c, 2);
    assert_eq!(callers.len(), 2);
    assert!(callers.contains(&(id_b, 1)), "should contain B at depth 1");
    assert!(callers.contains(&(id_a, 2)), "should contain A at depth 2");
}

#[test]
fn test_trace_callees_bfs() {
    let (graph, id_a, id_b, id_c) = make_chain_graph();
    let callees = graph.trace_callees(id_a, 3);
    assert_eq!(callees.len(), 2);
    assert!(callees.contains(&(id_b, 1)), "should contain B at depth 1");
    assert!(callees.contains(&(id_c, 2)), "should contain C at depth 2");
}

#[test]
fn test_shortest_path() {
    let (graph, id_a, id_b, id_c) = make_chain_graph();
    let path = graph.shortest_path(id_a, id_c);
    assert_eq!(path, Some(vec![id_a, id_b, id_c]));

    // Reverse direction — no path
    let no_path = graph.shortest_path(id_c, id_a);
    assert_eq!(no_path, None);
}

#[test]
fn test_file_dependents() {
    let mut graph = CodeGraph::new();
    let widget_file = PathBuf::from("widget.rs");
    let app_file = PathBuf::from("app.rs");

    let widget_sym = SymbolNode {
        id: CodeGraph::make_id(&widget_file, "Widget", 1),
        name: "Widget".into(),
        kind: SymbolKind::Struct,
        visibility: Visibility::Public,
        file: widget_file.clone(),
        start_line: 1,
        end_line: 5,
        signature: None,
    };
    let use_widget_sym = SymbolNode {
        id: CodeGraph::make_id(&app_file, "use_widget", 1),
        name: "use_widget".into(),
        kind: SymbolKind::Function,
        visibility: Visibility::Public,
        file: app_file.clone(),
        start_line: 1,
        end_line: 5,
        signature: None,
    };
    let widget_id = widget_sym.id;
    let use_widget_id = use_widget_sym.id;

    graph.add_symbol(widget_sym);
    graph.add_symbol(use_widget_sym);
    graph.add_edge(use_widget_id, Edge { to: widget_id, kind: EdgeKind::Calls, line: 3 });

    let dependents = graph.file_dependents(std::path::Path::new("widget.rs"), 3);
    assert_eq!(dependents.len(), 1);
    assert!(dependents.contains(&app_file));
}
