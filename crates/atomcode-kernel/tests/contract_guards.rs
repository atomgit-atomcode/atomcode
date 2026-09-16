//! Guards on where the front-end contract lives (`docs/adr/0021` §6).
//!
//! They read the source tree rather than trust a convention, so each one goes
//! red the day someone adds the thing it forbids.

use std::path::{Path, PathBuf};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the kernel crate sits under crates/")
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

/// Every `src/` and `tests/` tree of every workspace crate.
fn workspace_rust_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(crates) = std::fs::read_dir(crates_dir()) else {
        return out;
    };
    for krate in crates.flatten() {
        for tree in ["src", "tests"] {
            rust_files(&krate.path().join(tree), &mut out);
        }
    }
    out
}

/// One reason a turn ended, for the log and for the handle alike. The harness
/// used to keep a second copy for `TurnEnd`, and its pump folded three causes
/// away translating between them — so a front end read one reason in the log
/// and another on the handle.
#[test]
fn there_is_one_stop_reason() {
    let definitions: Vec<String> = workspace_rust_files()
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            text.lines()
                .any(|line| line.trim_start().starts_with("pub enum StopReason"))
                .then(|| path.display().to_string())
        })
        .collect();
    assert_eq!(
        definitions.len(),
        1,
        "exactly one `pub enum StopReason`, in the kernel; found: {definitions:?}"
    );
    assert!(
        definitions[0].ends_with("atomcode-kernel/src/event.rs"),
        "the one `StopReason` lives in the kernel: {definitions:?}"
    );
}

/// The contract's types and traits are the kernel's, and the kernel depends on
/// no workspace crate — not plexus (service keys are declared by whoever
/// consumes them), not anything else.
#[test]
fn the_kernel_depends_on_no_workspace_crate() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("the kernel's manifest");
    let mut table = String::new();
    let mut offenders = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            table = trimmed.to_string();
            continue;
        }
        let is_production_table =
            table.ends_with("dependencies]") && !table.contains("dev-dependencies");
        if is_production_table && trimmed.starts_with("atomcode-") {
            offenders.push(format!("{table} {trimmed}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "the kernel must not depend on a workspace crate: {offenders:?}"
    );
}
