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

/// The screen is an App apart from the agent (`docs/adr/0022` §3): what it
/// knows about the agent is what came back over its connection. A read of one
/// of the agent's own services is the screen reaching into another App — and
/// works only by accident, when both happen to share a process and a tree.
#[test]
fn the_screen_reads_no_service_of_the_agents() {
    let found = code_lines_naming(&[
        "AgentsSvc",
        "LlmSvc",
        "CompactionSvc",
        "ControlSvc",
        "ToolsSvc",
        "SystemPromptSvc",
    ]);
    assert!(found.is_empty(), "{found:#?}");
}

/// The screen speaks the two contracts (`docs/adr/0021` §5): the handle
/// protocol and host control. The runtime's driver protocol is the host's
/// transitional business, so neither its names appear here nor the crate that
/// defines them among this crate's dependencies.
#[test]
fn the_screen_does_not_speak_the_runtime_driver_protocol() {
    let found = code_lines_naming(&["CodingRuntimeHandle", "DriverCommand", "CodingRuntimeEvent"]);
    assert!(found.is_empty(), "{found:#?}");

    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("the manifest");
    let mut table = String::new();
    let mut offenders = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            table = trimmed.to_string();
            continue;
        }
        if table == "[dependencies]" && trimmed.starts_with("atomcode-coding") {
            offenders.push(trimmed.to_string());
        }
    }
    assert!(offenders.is_empty(), "{offenders:?}");
}

/// Nothing is printed once the screen has been given back.
///
/// Full screen is given back and the session is over, so whatever is written
/// next lands in the person's own buffer — next to the shell prompt, with no
/// repaint diff to bound it. A transcript re-typed down here is also a second
/// copy of what the session log already owns (`docs/adr/0024`), and the bytes
/// in it came from tools: `ESC[2J` cleared the scrollback the copy was meant
/// to hand back. The exit path therefore ends at `restore()`; see
/// `docs/tui-composability.md` §四 义务 7 and §九.
#[test]
fn nothing_is_printed_after_the_screen_is_given_back() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/plugin.rs");
    let text = std::fs::read_to_string(&src).expect("plugin.rs");
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let at = lines
        .iter()
        .position(|line| *line == "self.surface.restore();")
        .expect("the exit path gives the screen back");
    let next = lines[at + 1..]
        .iter()
        .find(|line| !line.is_empty() && !line.starts_with("//"))
        .copied();
    assert_eq!(
        next,
        Some("Ok(())"),
        "the exit stops at the screen: no transcript is printed into the shell's buffer"
    );
}
