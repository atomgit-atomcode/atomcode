use std::path::Path;

use anyhow::Result;

use super::CodeGraph;

/// Serialize a CodeGraph to bytes using bincode.
pub fn serialize(graph: &CodeGraph) -> Result<Vec<u8>> {
    let bytes = bincode::serialize(graph)?;
    Ok(bytes)
}

/// Deserialize a CodeGraph from bytes.
pub fn deserialize(bytes: &[u8]) -> Result<CodeGraph> {
    let graph = bincode::deserialize(bytes)?;
    Ok(graph)
}

/// Load a CodeGraph from a file path. Returns an empty graph if the file
/// does not exist or cannot be parsed.
pub fn load(path: &Path) -> CodeGraph {
    match std::fs::read(path) {
        Ok(bytes) => deserialize(&bytes).unwrap_or_else(|_| CodeGraph::new()),
        Err(_) => CodeGraph::new(),
    }
}

/// Save a CodeGraph to a file, creating parent directories if needed.
pub fn save(graph: &CodeGraph, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serialize(graph)?;
    std::fs::write(path, bytes)?;
    Ok(())
}
