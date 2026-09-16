//! Guards on what the screen must not contain.
//!
//! They read the source, or the shipped keymap, rather than trust a
//! convention, so each goes red the day someone brings back what it forbids.

use std::path::{Path, PathBuf};

use atomcode_tui::keymap::{Default_, Keymap};
use atomcode_tui::surface::KeyPress;

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

/// Every non-comment line under `src/` that contains one of `needles`, as
/// `path:line: text`.
fn code_lines_naming(needles: &[&str]) -> Vec<String> {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut found = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if needles.iter().any(|needle| line.contains(needle)) {
                found.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    found
}

/// The adjustable layout is gone until it is thought through (`docs/adr/0022`
/// §8): nothing gives the model a tool or a description of the layout, no row
/// offers layout commands, and there is no layout history to undo. Panels still
/// put themselves on screen when they mount.
#[test]
fn the_adjustable_layout_is_gone() {
    let found = code_lines_naming(&[
        "adjust_layout",
        "LayoutOp::Undo",
        "LayoutOp::Preset",
        "tui-commands-layout",
        "describe_for_model",
    ]);
    assert!(found.is_empty(), "{found:#?}");
}

/// No key rearranges the screen: `ctrl-f` (a preset), `ctrl-z` (undo a layout
/// change) and `ctrl-n` (toggle the mascot) are unbound.
#[test]
fn no_key_rearranges_the_screen() {
    let bound: Vec<KeyPress> = Default_.bindings().into_iter().map(|(k, _)| k).collect();
    for key in ['f', 'z', 'n'] {
        assert!(
            !bound.contains(&KeyPress::ctrl(key)),
            "ctrl-{key} is bound again"
        );
    }
}
