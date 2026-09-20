//! What a row puts into a shared registry, it takes back when it goes.
//!
//! A tool mount is two halves — put it in the catalog, and file the removal
//! that takes it out when the row leaves — so there is one door that does both
//! (`plugins::tools::mount`, and `mount_optional` for a row whose catalog may
//! be absent).
//!
//! The door only holds if callers can reach it. It was `pub(super)` until
//! 2026-09-20, so rows outside this crate could not call it at all and copied
//! the two halves by hand instead; `docs/adr/0019` counted four such copies,
//! and by the time anyone looked again there were six. Each copy is a chance to
//! write the first half and forget the second, and the result of forgetting is
//! a tool the model can still call after the row that owns it is gone.
//!
//! This guard reads the source tree. It finds the names bound from the shared
//! `ToolsSvc` and flags a `register` on any of them, so a new hand-rolled mount
//! goes red the day it is written.
//!
//! The prompt registry is the same story with sharper teeth: it keys fragments
//! by id and has no idea who contributed one, so the removal half is the only
//! thing that takes a row's text out of the prompt. Two product rows had
//! written the first half and stopped, and a live patch that disabled them left
//! their text in front of the model — see
//! `atomcode-coding/tests/prompt_fragments.rs`, which is the behavioural half of
//! this guard.
//!
//! **What these do not catch**: a row that binds a registry and hands it to a
//! helper that writes. That shape is `publish_mcp`'s, and it is named below
//! rather than guessed at — a guard that pretends to catch everything is worse
//! than one that says where it stops.

use std::path::{Path, PathBuf};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the harness crate sits under crates/")
        .to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every workspace crate's `src/` tree. Tests build their own catalogs on
/// purpose and are not rows, so they are out of scope.
fn workspace_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(crates) = std::fs::read_dir(crates_dir()) else {
        return out;
    };
    for krate in crates.flatten() {
        rust_files(&krate.path().join("src"), &mut out);
    }
    out.sort();
    out
}

/// Names bound from the tree's shared tool catalog in this file — `let toolbox
/// = ctx.require::<ToolsSvc>()`, `if let Some(toolbox) = ctx.service::<…>()`,
/// and a `ToolBox` taken as a parameter.
///
/// A child's own restricted box (`ToolBox::new()`) is not one of these: filling
/// it is building a catalog, not contributing to the tree's.
fn catalog_bindings(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (idx, _) in text.match_indices("ToolsSvc>()") {
        let head = &text[..idx];
        let Some(let_at) = head.rfind("let ") else {
            continue;
        };
        let rest = head[let_at + 4..].trim_start();
        let rest = rest.strip_prefix("Some(").unwrap_or(rest);
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    for (idx, _) in text.match_indices(": &ToolBox") {
        let head = &text[..idx];
        let start = head
            .rfind(|c: char| c == '(' || c == ',')
            .map(|at| at + 1)
            .unwrap_or(0);
        let name = head[start..].trim().to_string();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The function a line sits in, by the nearest `fn` above it.
fn enclosing_fn(lines: &[&str], at: usize) -> String {
    for line in lines[..=at].iter().rev() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("pub fn ")
            .or_else(|| trimmed.strip_prefix("fn "))
            .or_else(|| trimmed.strip_prefix("pub(crate) fn "))
            .or_else(|| trimmed.strip_prefix("pub(super) fn "))
            .or_else(|| trimmed.strip_prefix("async fn "))
            .or_else(|| trimmed.strip_prefix("pub async fn "))
        else {
            continue;
        };
        return rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
    }
    "<top level>".to_string()
}

/// Where a tool may be put into the tree's catalog: the door, and the one
/// republisher that files a single removal for a whole server's tools.
const TOOL_DOORS: &[(&str, &str)] = &[
    ("atomcode-harness/src/plugins/tools.rs", "mount_into"),
    ("atomcode-coding/src/host_rows.rs", "publish_mcp"),
];

/// Where a fragment may be put into the prompt: the door, and the live reload
/// that re-contributes under a mounted row's id (it mounts nothing, so it has
/// no row to leave with).
const PROMPT_DOORS: &[(&str, &str)] = &[
    ("atomcode-harness/src/plugins/tools.rs", "contribute_prompt"),
    ("atomcode-coding/src/runtime.rs", "reload_skills_live"),
];

#[test]
fn every_tool_mount_goes_through_the_door() {
    let root = crates_dir();
    let mut strays: Vec<String> = Vec::new();
    for path in workspace_sources() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let names = catalog_bindings(&text);
        if names.is_empty() {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        for (no, line) in lines.iter().enumerate() {
            if !names
                .iter()
                .any(|n| line.contains(&format!("{n}.register(")))
            {
                continue;
            }
            let owner = enclosing_fn(&lines, no);
            if TOOL_DOORS
                .iter()
                .any(|(file, func)| relative == *file && owner == *func)
            {
                continue;
            }
            strays.push(format!("{relative}:{}  in `{owner}`", no + 1));
        }
    }
    assert!(
        strays.is_empty(),
        "these register a tool without the door's second half — call \
         `plugins::tools::mount` (or `mount_optional`) instead:\n{}",
        strays.join("\n")
    );
}

/// Names bound from the tree's shared prompt registry in this file. A child's
/// own registry (`PromptRegistry::new()`) is not one — composing a prompt for a
/// delegated agent is building one, not contributing to the tree's.
fn prompt_bindings(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (idx, _) in text.match_indices("SystemPromptSvc>()") {
        let head = &text[..idx];
        let Some(let_at) = head.rfind("let ") else {
            continue;
        };
        let rest = head[let_at + 4..].trim_start();
        let rest = rest.strip_prefix("Some(").unwrap_or(rest);
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[test]
fn every_prompt_fragment_goes_through_the_door() {
    let root = crates_dir();
    let mut strays: Vec<String> = Vec::new();
    for path in workspace_sources() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let names = prompt_bindings(&text);
        if names.is_empty() {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        for (no, line) in lines.iter().enumerate() {
            if !names
                .iter()
                .any(|n| line.contains(&format!("{n}.contribute(")))
            {
                continue;
            }
            let owner = enclosing_fn(&lines, no);
            if PROMPT_DOORS
                .iter()
                .any(|(file, func)| relative == *file && owner == *func)
            {
                continue;
            }
            strays.push(format!("{relative}:{}  in `{owner}`", no + 1));
        }
    }
    assert!(
        strays.is_empty(),
        "these put a fragment in the prompt without the door's second half — \
         call `plugins::tools::contribute_prompt` instead:\n{}",
        strays.join("\n")
    );
}
