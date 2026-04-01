//! Cross-session project knowledge persistence.
//!
//! Automatically extracts valuable information from tool results (database credentials,
//! server ports, startup commands, etc.) and persists them to `.atomcode/knowledge.md`.
//! This knowledge is injected into the system prompt so future sessions don't waste
//! steps rediscovering the same information.

use std::path::Path;

/// Known patterns to extract from tool results.
struct Pattern {
    /// What to look for in tool output
    trigger: &'static str,
    /// Category for dedup
    category: &'static str,
    /// How to extract the knowledge line
    extractor: fn(&str) -> Option<String>,
}

const PATTERNS: &[Pattern] = &[
    // Database credentials from application.properties/yml
    Pattern {
        trigger: "spring.datasource",
        category: "db_credentials",
        extractor: extract_db_credentials,
    },
    // Server port from config
    Pattern {
        trigger: "server.port",
        category: "server_port",
        extractor: extract_server_port,
    },
    // MySQL access denied → next grep finds password
    Pattern {
        trigger: "Access denied for user",
        category: "db_credentials",
        extractor: |_| Some("MySQL requires password. Check application.properties for spring.datasource.password".to_string()),
    },
];

/// Try to extract knowledge from a tool result output.
/// Returns a list of (category, knowledge_line) pairs.
pub fn extract_knowledge(output: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    for pat in PATTERNS {
        if output.contains(pat.trigger) {
            if let Some(knowledge) = (pat.extractor)(output) {
                results.push((pat.category.to_string(), knowledge));
            }
        }
    }
    results
}

/// Persist extracted knowledge to .atomcode/knowledge.md.
/// Deduplicates by category — newer knowledge replaces older.
pub fn save_knowledge(working_dir: &Path, entries: &[(String, String)]) {
    if entries.is_empty() {
        return;
    }

    let dir = working_dir.join(".atomcode");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("knowledge.md");

    // Read existing knowledge
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<(String, String)> = existing
        .lines()
        .filter(|l| l.contains(": "))
        .filter_map(|l| {
            let mut parts = l.splitn(2, ": ");
            let cat = parts.next()?.trim().trim_start_matches("- ").to_string();
            let val = parts.next()?.trim().to_string();
            Some((cat, val))
        })
        .collect();

    // Merge: new entries replace old ones with same category
    for (cat, val) in entries {
        lines.retain(|(c, _)| c != cat);
        lines.push((cat.clone(), val.clone()));
    }

    // Write back
    let content = lines
        .iter()
        .map(|(cat, val)| format!("- {}: {}", cat, val))
        .collect::<Vec<_>>()
        .join("\n");

    let _ = std::fs::write(&path, format!("# Project Knowledge (auto-generated)\n\n{}\n", content));
}

/// Save a user-triggered knowledge entry ("记住这个" / "remember this").
/// Extracts the last assistant response as the knowledge content.
pub fn save_user_knowledge(working_dir: &Path, category: &str, content: &str) {
    let entry = vec![(category.to_string(), content.to_string())];
    save_knowledge(working_dir, &entry);
}

/// Load knowledge from .atomcode/knowledge.md for injection into system prompt.
pub fn load_knowledge(working_dir: &Path) -> String {
    let path = working_dir.join(".atomcode/knowledge.md");
    match std::fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => {
            let lines: Vec<&str> = content.lines()
                .filter(|l| l.starts_with("- "))
                .collect();
            if lines.is_empty() {
                return String::new();
            }
            format!("=== PROJECT KNOWLEDGE (from previous sessions) ===\n{}\n", lines.join("\n"))
        }
        _ => String::new(),
    }
}

// ── Extractors ──

fn extract_db_credentials(output: &str) -> Option<String> {
    let mut url = String::new();
    let mut user = String::new();
    let mut pass = String::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("datasource.url") || trimmed.contains("url:") {
            if let Some(val) = extract_value(trimmed) {
                url = val;
            }
        }
        if trimmed.contains("datasource.username") || (trimmed.contains("username:") && !trimmed.contains("git")) {
            if let Some(val) = extract_value(trimmed) {
                user = val;
            }
        }
        if trimmed.contains("datasource.password") || trimmed.contains("password:") {
            if let Some(val) = extract_value(trimmed) {
                pass = val;
            }
        }
    }

    if !pass.is_empty() || !user.is_empty() {
        Some(format!("MySQL: user={}, password={}, url={}",
            if user.is_empty() { "root" } else { &user },
            if pass.is_empty() { "(unknown)" } else { &pass },
            if url.is_empty() { "(check config)" } else { &url },
        ))
    } else {
        None
    }
}

fn extract_server_port(output: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("server.port") {
            if let Some(val) = extract_value(trimmed) {
                return Some(format!("Server port: {}", val));
            }
        }
    }
    None
}

fn extract_value(line: &str) -> Option<String> {
    // Handle both "key=value" and "key: value" formats
    let val = if let Some(pos) = line.find('=') {
        line[pos + 1..].trim().to_string()
    } else if let Some(pos) = line.rfind(':') {
        line[pos + 1..].trim().to_string()
    } else {
        return None;
    };
    if val.is_empty() { None } else { Some(val) }
}
