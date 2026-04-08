use std::path::Path;

use super::{CodeGraph, SymbolId};

/// Resolve a callee name to the best-matching SymbolId.
/// Priority:
/// 1. Same file (private helper in same file) — score 4
/// 2. Imported name (appears in caller's import list) — score 3
/// 3. Same directory — score 2
/// 4. Same top-level crate/package — score 1
/// 5. Any match — score 0
pub fn resolve_callee(
    graph: &CodeGraph,
    callee_name: &str,
    caller_file: &Path,
    imported_names: &[String],
) -> Option<SymbolId> {
    let candidates = graph.find_by_name(callee_name);
    if candidates.is_empty() {
        return None;
    }

    let caller_dir = caller_file.parent();
    let caller_root = top_component(caller_file);

    let mut best_id: Option<SymbolId> = None;
    let mut best_score: i32 = -1;

    for node in &candidates {
        let score = if node.file == caller_file {
            4
        } else if imported_names.contains(&node.name) {
            3
        } else if caller_dir.is_some() && node.file.parent() == caller_dir {
            2
        } else if caller_root.is_some() && top_component(&node.file) == caller_root {
            1
        } else {
            0
        };

        if score > best_score {
            best_score = score;
            best_id = Some(node.id);
        }
    }

    best_id
}

fn top_component(path: &Path) -> Option<String> {
    path.components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
}
