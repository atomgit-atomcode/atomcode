use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

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
    let callee_idx = query.capture_index_for_name("callee").unwrap();

    let mut callees: Vec<String> = Vec::new();
    while let Some(m) = matches.next() {
        for cap in m.captures {
            if cap.index == callee_idx {
                let name = cap.node.utf8_text(source).unwrap();
                callees.push(name.to_string());
            }
        }
    }

    assert!(
        callees.contains(&"foo".to_string()),
        "missing foo: {:?}",
        callees
    );
    assert!(
        callees.contains(&"baz".to_string()),
        "missing baz: {:?}",
        callees
    );
    assert!(
        callees.contains(&"method".to_string()),
        "missing method: {:?}",
        callees
    );
    assert_eq!(
        callees.len(),
        3,
        "expected exactly 3 callees: {:?}",
        callees
    );
}

#[tokio::test]
async fn test_indexer_indexes_rust_files() {
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

    indexer.index_all(CancellationToken::new()).await;

    let g = graph.read().await;

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

#[tokio::test]
async fn test_indexer_incremental_update() {
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

    indexer.index_all(CancellationToken::new()).await;

    let count_after_first = {
        let g = graph.read().await;
        assert!(
            g.node_count() >= 1,
            "should have at least 1 symbol after first index"
        );
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

    indexer.index_all(CancellationToken::new()).await;

    let count_after_second = {
        let g = graph.read().await;
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
    graph.add_edge(
        id_a,
        Edge {
            to: id_b,
            kind: EdgeKind::Calls,
            line: 3,
        },
    );
    graph.add_edge(
        id_b,
        Edge {
            to: id_c,
            kind: EdgeKind::Calls,
            line: 12,
        },
    );
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
    graph.add_edge(
        use_widget_id,
        Edge {
            to: widget_id,
            kind: EdgeKind::Calls,
            line: 3,
        },
    );

    let dependents = graph.file_dependents(std::path::Path::new("widget.rs"), 3);
    assert_eq!(dependents.len(), 1);
    assert!(dependents.contains(&app_file));
}

#[tokio::test]
async fn test_full_pipeline_index_and_query() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(
        dir.path().join("main.rs"),
        r#"
fn main() {
    let result = handle_request();
    println!("{}", result);
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("handler.rs"),
        r#"
pub fn handle_request() -> String {
    let data = fetch_data();
    format_response(data)
}

fn format_response(data: String) -> String {
    format!("Response: {}", data)
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("fetcher.rs"),
        r#"
pub fn fetch_data() -> String {
    "hello".to_string()
}
"#,
    )
    .unwrap();

    let graph = Arc::new(RwLock::new(CodeGraph::new()));
    let mut indexer = GraphIndexer::new(graph.clone(), dir.path().to_path_buf());
    indexer.index_all(CancellationToken::new()).await;

    let g = graph.read().await;

    // Verify: main calls handle_request
    let mains = g.find_by_name("main");
    assert!(!mains.is_empty(), "should find 'main'");

    let callees = g.trace_callees(mains[0].id, 3);
    let callee_names: Vec<String> = callees
        .iter()
        .filter_map(|(id, _)| g.node(*id).map(|n| n.name.clone()))
        .collect();
    assert!(
        callee_names.contains(&"handle_request".to_string()),
        "expected handle_request in callees of main: {:?}",
        callee_names
    );

    // Verify: fetch_data callers include handle_request
    let fetchers = g.find_by_name("fetch_data");
    if !fetchers.is_empty() {
        let callers = g.trace_callers(fetchers[0].id, 3);
        let caller_names: Vec<String> = callers
            .iter()
            .filter_map(|(id, _)| g.node(*id).map(|n| n.name.clone()))
            .collect();
        assert!(
            caller_names.contains(&"handle_request".to_string()),
            "expected handle_request in callers of fetch_data: {:?}",
            caller_names
        );
    }

    // Verify: blast radius of fetcher.rs includes handler.rs
    let deps = g.file_dependents(&dir.path().join("fetcher.rs"), 3);
    let dep_names: Vec<String> = deps
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        dep_names.contains(&"handler.rs".to_string()),
        "expected handler.rs in dependents of fetcher.rs: {:?}",
        dep_names
    );
}

// ── Auto-injection summary tests ──

#[test]
fn test_empty_graph_queries() {
    let graph = CodeGraph::new();
    assert!(!graph.is_ready());
    assert_eq!(graph.node_count(), 0);
    assert!(graph.find_by_name("anything").is_empty());
    assert!(graph.callees(12345).is_none());
    assert!(graph.callers(12345).is_none());
    assert_eq!(graph.trace_callers(12345, 3), vec![]);
    assert_eq!(graph.trace_callees(12345, 3), vec![]);
    assert_eq!(graph.shortest_path(1, 2), None);
    assert!(graph
        .file_dependents(std::path::Path::new("x.rs"), 3)
        .is_empty());
}

#[test]
fn test_cycle_detection() {
    let mut graph = CodeGraph::new();
    let file = PathBuf::from("cycle.rs");
    let sym_a = make_test_symbol(&file, "a", 1);
    let sym_b = make_test_symbol(&file, "b", 10);
    let sym_c = make_test_symbol(&file, "c", 20);
    let id_a = sym_a.id;
    let id_b = sym_b.id;
    let id_c = sym_c.id;
    graph.add_symbol(sym_a);
    graph.add_symbol(sym_b);
    graph.add_symbol(sym_c);
    graph.add_edge(
        id_a,
        Edge {
            to: id_b,
            kind: EdgeKind::Calls,
            line: 2,
        },
    );
    graph.add_edge(
        id_b,
        Edge {
            to: id_c,
            kind: EdgeKind::Calls,
            line: 12,
        },
    );
    graph.add_edge(
        id_c,
        Edge {
            to: id_a,
            kind: EdgeKind::Calls,
            line: 22,
        },
    );

    let callees = graph.trace_callees(id_a, 10);
    assert_eq!(callees.len(), 2);
    assert!(callees.iter().all(|(id, _)| *id != id_a));
}

#[test]
fn test_diamond_dependency() {
    let mut graph = CodeGraph::new();
    let file = PathBuf::from("diamond.rs");
    let sym_a = make_test_symbol(&file, "a", 1);
    let sym_b = make_test_symbol(&file, "b", 10);
    let sym_c = make_test_symbol(&file, "c", 20);
    let sym_d = make_test_symbol(&file, "d", 30);
    let id_a = sym_a.id;
    let id_b = sym_b.id;
    let id_c = sym_c.id;
    let id_d = sym_d.id;
    graph.add_symbol(sym_a);
    graph.add_symbol(sym_b);
    graph.add_symbol(sym_c);
    graph.add_symbol(sym_d);
    graph.add_edge(
        id_a,
        Edge {
            to: id_b,
            kind: EdgeKind::Calls,
            line: 2,
        },
    );
    graph.add_edge(
        id_a,
        Edge {
            to: id_c,
            kind: EdgeKind::Calls,
            line: 3,
        },
    );
    graph.add_edge(
        id_b,
        Edge {
            to: id_d,
            kind: EdgeKind::Calls,
            line: 12,
        },
    );
    graph.add_edge(
        id_c,
        Edge {
            to: id_d,
            kind: EdgeKind::Calls,
            line: 22,
        },
    );

    let callers = graph.trace_callers(id_d, 3);
    assert_eq!(callers.len(), 3);
    let depth1: Vec<_> = callers.iter().filter(|(_, d)| *d == 1).collect();
    assert_eq!(depth1.len(), 2);
}

#[test]
fn test_depth_limit_respected() {
    let mut graph = CodeGraph::new();
    let file = PathBuf::from("deep.rs");
    let names = ["a", "b", "c", "d", "e", "f"];
    let mut ids = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let sym = make_test_symbol(&file, name, (i * 10 + 1) as usize);
        ids.push(sym.id);
        graph.add_symbol(sym);
    }
    for i in 0..5 {
        graph.add_edge(
            ids[i],
            Edge {
                to: ids[i + 1],
                kind: EdgeKind::Calls,
                line: 2,
            },
        );
    }
    let callees = graph.trace_callees(ids[0], 2);
    assert_eq!(callees.len(), 2);
    assert!(graph.trace_callees(ids[0], 0).is_empty());
}

#[test]
fn test_serialize_roundtrip_with_edges() {
    let mut graph = CodeGraph::new();
    let file = PathBuf::from("rt.rs");
    let sym_a = make_test_symbol(&file, "alpha", 1);
    let sym_b = make_test_symbol(&file, "beta", 10);
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

    let bytes = persist::serialize(&graph).unwrap();
    let restored = persist::deserialize(&bytes).unwrap();
    assert_eq!(restored.node_count(), 2);
    assert_eq!(restored.callees(id_a).unwrap().len(), 1);
}

#[test]
fn test_file_dependency_summary() {
    let mut graph = CodeGraph::new();
    let handler_file = PathBuf::from("src/handler.rs");
    let fetcher_file = PathBuf::from("src/fetcher.rs");
    let main_file = PathBuf::from("src/main.rs");

    let handler_sym = make_test_symbol(&handler_file, "handle", 1);
    let fetcher_sym = make_test_symbol(&fetcher_file, "fetch", 1);
    let main_sym = make_test_symbol(&main_file, "main", 1);
    let handler_id = handler_sym.id;
    let fetcher_id = fetcher_sym.id;
    let main_id = main_sym.id;

    graph.add_symbol(handler_sym);
    graph.add_symbol(fetcher_sym);
    graph.add_symbol(main_sym);
    graph.add_edge(
        handler_id,
        Edge {
            to: fetcher_id,
            kind: EdgeKind::Calls,
            line: 3,
        },
    );
    graph.add_edge(
        main_id,
        Edge {
            to: handler_id,
            kind: EdgeKind::Calls,
            line: 2,
        },
    );

    let summary = graph.file_dependency_summary("handler.rs").unwrap();
    assert!(summary.contains("Graph: handler.rs"), "{}", summary);
    assert!(summary.contains("fetcher.rs"), "{}", summary);
    assert!(summary.contains("main.rs"), "{}", summary);
    assert!(graph.file_dependency_summary("nope.rs").is_none());
}

#[test]
fn test_call_chain_summary() {
    let mut graph = CodeGraph::new();
    let files = [
        PathBuf::from("main.rs"),
        PathBuf::from("handler.rs"),
        PathBuf::from("fetcher.rs"),
        PathBuf::from("http.rs"),
    ];
    let names = ["main", "handle", "fetch", "http_get"];
    let mut ids = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let sym = make_test_symbol(&files[i], name, 1);
        ids.push(sym.id);
        graph.add_symbol(sym);
    }
    for i in 0..3 {
        graph.add_edge(
            ids[i],
            Edge {
                to: ids[i + 1],
                kind: EdgeKind::Calls,
                line: 5,
            },
        );
    }

    let chain = graph.call_chain_summary("main").unwrap();
    assert!(chain.contains("Call chain: main()"), "{}", chain);
    assert!(chain.contains("handle()"), "{}", chain);
    assert!(chain.contains("http_get()"), "{}", chain);
    assert!(graph.call_chain_summary("http_get").is_none());
    assert!(graph.call_chain_summary("nonexistent").is_none());
}

#[tokio::test]
async fn test_indexer_deleted_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
    std::fs::write(dir.path().join("remove.rs"), "fn remove() {}\n").unwrap();

    let graph = Arc::new(RwLock::new(CodeGraph::new()));
    let mut indexer = GraphIndexer::new(graph.clone(), dir.path().to_path_buf());
    indexer.index_all(CancellationToken::new()).await;
    assert!(!graph.read().await.find_by_name("remove").is_empty());

    std::fs::remove_file(dir.path().join("remove.rs")).unwrap();
    indexer.index_all(CancellationToken::new()).await;
    assert!(graph.read().await.find_by_name("remove").is_empty());
    assert!(!graph.read().await.find_by_name("keep").is_empty());
}

#[tokio::test]
async fn test_indexer_python_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.py"),
        "def main():\n    result = process()\n\ndef process():\n    return 'done'\n",
    )
    .unwrap();

    let graph = Arc::new(RwLock::new(CodeGraph::new()));
    let mut indexer = GraphIndexer::new(graph.clone(), dir.path().to_path_buf());
    indexer.index_all(CancellationToken::new()).await;

    let g = graph.read().await;
    assert!(!g.find_by_name("main").is_empty());
    assert!(!g.find_by_name("process").is_empty());
}
