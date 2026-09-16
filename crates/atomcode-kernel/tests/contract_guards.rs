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

/// Where a type is defined, by the line that defines it. A needle that ends in
/// a name must end where the name does: `pub struct Question` is not `pub struct
/// QuestionsHandlePlugin`.
fn definitions_of(needle: &str) -> Vec<String> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let ends_in_name = needle.chars().last().is_some_and(ident);
    let defines = |line: &str| {
        line.trim_start()
            .strip_prefix(needle)
            .is_some_and(|rest| !ends_in_name || !rest.chars().next().is_some_and(ident))
    };
    workspace_rust_files()
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            text.lines()
                .any(defines)
                .then(|| path.display().to_string())
        })
        .collect()
}

/// The session vocabulary is the kernel's (`docs/adr/0024` §6): a store that
/// only knows the kernel and a front end that must not know the harness read the
/// same facts. The harness keeps the in-memory log and re-exports the words; it
/// does not define a second copy of them.
#[test]
fn the_session_vocabulary_is_defined_once_in_the_kernel() {
    for needle in [
        "pub enum SessionEvent",
        "pub struct SessionHeader",
        "pub struct LoggedEvent",
        "pub struct Committed",
        "pub enum InjectionOrigin",
        "pub struct Question",
        "pub struct Answer",
        "pub struct RateLimitPause",
        "pub fn derive_messages(events",
        "pub fn build_compact_stub(",
    ] {
        let found = definitions_of(needle);
        assert_eq!(found.len(), 1, "`{needle}` defined exactly once: {found:?}");
        assert!(
            found[0].ends_with("atomcode-kernel/src/session.rs"),
            "`{needle}` lives in kernel::session: {found:?}"
        );
    }
}

/// What a front end is told about an agent — its command catalog included — is
/// the kernel's, and the harness's agent reports its status in those words
/// rather than a copy of them (`docs/adr/0022` §5, `docs/adr/0021` §10).
#[test]
fn the_agent_description_is_defined_once_in_the_kernel() {
    for (needle, file) in [
        ("pub enum AgentStatus", "description.rs"),
        ("pub struct AgentDescription", "description.rs"),
        ("pub struct MemberIdentity", "description.rs"),
        ("pub struct CommandDescription", "catalog.rs"),
        ("pub enum CommandTarget", "catalog.rs"),
    ] {
        let found = definitions_of(needle);
        assert_eq!(found.len(), 1, "`{needle}` defined exactly once: {found:?}");
        assert!(
            found[0].ends_with(&format!("atomcode-kernel/src/agent/{file}")),
            "`{needle}` lives in kernel::agent: {found:?}"
        );
    }
}

/// Host control carries intents, never the host's implementation: no
/// generation, no whole conversation, no agent configuration (`docs/adr/0021`
/// §2 and its failure conditions). Comments may say what is kept out; code may
/// not carry it.
#[test]
fn host_control_names_no_implementation() {
    let host = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/host.rs");
    let text = std::fs::read_to_string(&host).expect("kernel::host");
    let offenders: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| {
            let lower = line.to_lowercase();
            ["generation", "sessionsnapshot", "codingagentconfig"]
                .iter()
                .any(|word| lower.contains(word))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "host control must not carry the host's implementation: {offenders:#?}"
    );
}
