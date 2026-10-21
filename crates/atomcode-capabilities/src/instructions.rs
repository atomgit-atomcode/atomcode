//! Three-tier project-instructions loader (global / project / user) — the v2 port of
//! v1 `config/instructions.rs`. Each tier is a markdown file injected into the session
//! context block under a header carrying its source path. Caller-supplied paths keep
//! it testable without touching the real home dir; [`SessionContextHook`] supplies the
//! config-root and project paths.
//!
//! [`SessionContextHook`]: crate::session::SessionContextHook

use std::path::{Path, PathBuf};

/// Per-tier size cap (v1 parity): a runaway instructions file can't blow the prompt.
const MAX_INSTRUCTIONS_BYTES: usize = 1024 * 1024; // 1 MiB

// Advisory only: changing which instructions are loaded requires an explicit user choice.
const LARGE_INSTRUCTIONS_BYTES: usize = 16 * 1024;

/// Project-root filenames checked IN ORDER for the project tier; the FIRST existing wins.
/// Includes the ecosystem names (`AGENTS.md`, `CLAUDE.md`) so a repo's existing agent
/// instructions are honored (matches v1).
const PROJECT_NAMES: [&str; 5] = [
    ".atomcode.md",
    "ATOMCODE.md",
    "AGENTS.md",
    "CLAUDE.md",
    "claude.md",
];

/// State of a source when this context was loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionState {
    Loaded,
    Missing,
    Empty,
    Unreadable,
    Shadowed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionSource {
    pub scope: &'static str,
    pub path: PathBuf,
    pub state: InstructionState,
    /// The disk no longer matches this loaded snapshot. This does not reload it.
    pub disk_changed: bool,
    body: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct LoadedInstructions {
    pub sources: Vec<InstructionSource>,
    text: String,
}

impl LoadedInstructions {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn warnings(&self) -> Vec<String> {
        let project = self.sources.iter().find(|s| {
            s.scope == "PROJECT"
                && s.state != InstructionState::Shadowed
                && s.state != InstructionState::Missing
        });
        let mut warnings: Vec<_> = self
            .sources
            .iter()
            .filter_map(|source| {
                let reason = match source.state {
                    InstructionState::Empty => {
                        "selected instruction file is empty; no instructions loaded".to_string()
                    }
                    InstructionState::Unreadable => {
                        "selected instruction file cannot be read as UTF-8; no instructions loaded"
                            .to_string()
                    }
                    InstructionState::Shadowed => {
                        format!("not loaded: shadowed by {}", project?.path.display())
                    }
                    _ => return None,
                };
                Some(format!(
                    "[instructions] {}: {reason}",
                    source.path.display()
                ))
            })
            .collect();
        let loaded: Vec<_> = self
            .sources
            .iter()
            .filter(|source| source.state == InstructionState::Loaded)
            .collect();
        let bytes: usize = loaded
            .iter()
            .filter_map(|source| source.body.as_deref())
            .map(str::len)
            .sum();
        if bytes > LARGE_INSTRUCTIONS_BYTES {
            let chars: usize = loaded
                .iter()
                .filter_map(|source| source.body.as_deref())
                .map(|body| body.chars().count())
                .sum();
            let sources = loaded
                .iter()
                .map(|source| {
                    let body = source.body.as_deref().unwrap_or_default();
                    format!(
                        "{} {}: {} UTF-8 bytes, {} characters",
                        source.scope,
                        source.path.display(),
                        body.len(),
                        body.chars().count()
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            warnings.push(format!("[instructions] Large loaded instruction bodies: {bytes} UTF-8 bytes, {chars} characters (advisory threshold: {LARGE_INSTRUCTIONS_BYTES} bytes). Sources: {sources}. Review long instructions for history or repetition; move details to linked documents only with your approval. This notice does not shorten or rewrite instructions. Sizes are not token counts."));
        }
        warnings
    }

    /// Compare against disk without changing the text the runtime actually uses.
    pub fn sources_on_disk(&self) -> Vec<InstructionSource> {
        self.sources
            .iter()
            .cloned()
            .map(|mut source| {
                let (state, body) = read_source(&source.path);
                source.disk_changed = if source.state == InstructionState::Shadowed {
                    state == InstructionState::Missing
                } else {
                    state != source.state || body != source.body
                };
                source.body = None; // A driver's status result never carries instruction contents.
                source
            })
            .collect()
    }
}

pub fn load_instructions(home: &Path, project: &Path) -> LoadedInstructions {
    let mut sources = Vec::new();
    let mut out = Vec::new();
    let mut add = |path: PathBuf, scope: &'static str, shadowed: bool| {
        let (mut state, body) = read_source(&path);
        if shadowed && state != InstructionState::Missing {
            state = InstructionState::Shadowed;
        }
        if state == InstructionState::Loaded {
            out.push(format!(
                "=== {scope} INSTRUCTIONS ({}) ===\n{}",
                path.display(),
                body.as_deref().unwrap_or_default()
            ));
        }
        sources.push(InstructionSource {
            scope,
            path,
            state,
            disk_changed: false,
            body,
        });
    };
    add(home.join("ATOMCODE.md"), "GLOBAL", false);
    let mut selected = false;
    let mut seen = std::collections::BTreeSet::new();
    for name in PROJECT_NAMES {
        let path = project.join(name);
        // CLAUDE.md and claude.md can name the same file on Windows/macOS.
        if path.is_file() && !seen.insert(path.canonicalize().unwrap_or_else(|_| path.clone())) {
            continue;
        }
        add(path.clone(), "PROJECT", selected);
        selected |= path.is_file();
    }
    add(project.join(".atomcode.user.md"), "USER", false);
    LoadedInstructions {
        sources,
        text: render_blocks(out),
    }
}

pub fn render_instructions(home: &Path, project: &Path) -> String {
    load_instructions(home, project).text
}

fn render_blocks(out: Vec<String>) -> String {
    if out.is_empty() {
        return String::new();
    }
    // Precedence preamble injected right next to the user's own instructions so it is
    // hard to overlook: these GLOBAL/PROJECT/USER blocks OVERRIDE the assistant's default
    // working rules on conflict, but describe how to work on the project — never the host
    // product or active model. Reinforces the same scoped precedence stated in the persona.
    const PREAMBLE: &str = "The following GLOBAL / PROJECT / USER instructions take \
PRECEDENCE over the assistant's default system-prompt rules — when they conflict with a \
default working rule, follow these. These instructions govern work on the project only; \
they do not describe or override the host application or active configured model. \
(Safety, approval, and destructive-action gates are not overridable here.)";
    format!("{PREAMBLE}\n\n{}", out.join("\n\n"))
}

fn read_source(path: &Path) -> (InstructionState, Option<String>) {
    if !path.is_file() {
        return (InstructionState::Missing, None);
    }
    match std::fs::read_to_string(path) {
        Err(_) => (InstructionState::Unreadable, None),
        Ok(body) if body.trim().is_empty() => (InstructionState::Empty, Some(body)),
        Ok(body) => {
            let body = body.trim();
            let body = if body.len() > MAX_INSTRUCTIONS_BYTES {
                let cut: String = body.chars().take(MAX_INSTRUCTIONS_BYTES).collect();
                format!("{cut}\n[truncated: instructions file exceeds 1 MiB]")
            } else {
                body.to_string()
            };
            (InstructionState::Loaded, Some(body))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn large_loaded_bodies_are_reported_without_changing_instructions() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let project = d.path().join("project");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&project).unwrap();
        let body = "规".repeat(LARGE_INSTRUCTIONS_BYTES / 6 + 1);
        assert!(body.len() < LARGE_INSTRUCTIONS_BYTES);
        fs::write(home.join("ATOMCODE.md"), &body).unwrap();
        fs::write(project.join("AGENTS.md"), &body).unwrap();
        let loaded = load_instructions(&home, &project);
        let before = loaded.text().to_string();
        let warnings = loaded.warnings();
        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert!(warning.contains(&format!(
            "{} UTF-8 bytes, {} characters",
            body.len() * 2,
            body.chars().count() * 2
        )));
        assert!(warning.contains("GLOBAL") && warning.contains("PROJECT"));
        assert!(warning.contains("ATOMCODE.md") && warning.contains("AGENTS.md"));
        assert!(!warning.contains(&body));
        assert_eq!(loaded.text(), before);
        assert_eq!(
            before.matches(&body).count(),
            2,
            "even duplicate rules remain intact"
        );
        assert_eq!(fs::read_to_string(project.join("AGENTS.md")).unwrap(), body);
    }

    #[test]
    fn size_notice_ignores_shadowed_sources_and_respects_the_advisory_boundary() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("missing");
        fs::write(
            d.path().join(".atomcode.md"),
            "x".repeat(LARGE_INSTRUCTIONS_BYTES),
        )
        .unwrap();
        fs::write(
            d.path().join("AGENTS.md"),
            "y".repeat(LARGE_INSTRUCTIONS_BYTES + 1),
        )
        .unwrap();
        let loaded = load_instructions(&home, d.path());
        assert_eq!(
            loaded.warnings().len(),
            1,
            "only the existing shadow warning"
        );
        assert!(!loaded.warnings()[0].contains("Large loaded"));
        fs::write(d.path().join(".atomcode.user.md"), "z").unwrap();
        let loaded = load_instructions(&home, d.path());
        assert_eq!(loaded.warnings().len(), 2);
        assert!(
            loaded.warnings()[1].contains(&format!("{} UTF-8 bytes", LARGE_INSTRUCTIONS_BYTES + 1))
        );
        assert!(!loaded.text().contains("yyyy"));
    }

    #[test]
    fn selected_empty_or_unreadable_file_still_shadows_the_other_candidates() {
        let d = tempfile::tempdir().unwrap();
        let preferred = d.path().join(".atomcode.md");
        let agents = d.path().join("AGENTS.md");
        fs::write(&agents, "must not be injected").unwrap();
        for (bytes, state) in [
            (b"   ".as_slice(), InstructionState::Empty),
            (b"\xff".as_slice(), InstructionState::Unreadable),
        ] {
            fs::write(&preferred, bytes).unwrap();
            let loaded = load_instructions(&d.path().join("nohome"), d.path());
            assert!(loaded.text().is_empty());
            assert_eq!(
                loaded
                    .sources
                    .iter()
                    .find(|s| s.path == preferred)
                    .unwrap()
                    .state,
                state
            );
            assert_eq!(
                loaded
                    .sources
                    .iter()
                    .find(|s| s.path == agents)
                    .unwrap()
                    .state,
                InstructionState::Shadowed
            );
            let warnings = loaded.warnings().join("\n");
            assert!(warnings.contains(".atomcode.md") && warnings.contains("AGENTS.md"));
            assert!(!warnings.contains("must not be injected"));
        }
    }

    #[test]
    fn source_status_keeps_loaded_bytes_after_edit_or_rename() {
        let d = tempfile::tempdir().unwrap();
        let preferred = d.path().join(".atomcode.md");
        fs::write(&preferred, "old loaded rule").unwrap();
        let loaded = load_instructions(&d.path().join("nohome"), d.path());
        fs::rename(&preferred, d.path().join("AGENTS.md")).unwrap();
        let status = loaded.sources_on_disk();
        let old = status.iter().find(|s| s.path == preferred).unwrap();
        assert_eq!(old.state, InstructionState::Loaded);
        assert!(old.disk_changed);
        let new = status
            .iter()
            .find(|s| s.path.ends_with("AGENTS.md"))
            .unwrap();
        assert_eq!(new.state, InstructionState::Missing);
        assert!(new.disk_changed, "new file has not been injected");
        assert!(loaded.text().contains("old loaded rule"));
        let refreshed = load_instructions(&d.path().join("nohome"), d.path());
        assert!(refreshed.text().contains("AGENTS.md"));
        assert!(!refreshed.sources_on_disk().iter().any(|s| s.disk_changed));
    }

    #[test]
    fn project_tier_picks_first_existing_name() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path();
        // AGENTS.md and CLAUDE.md both exist → AGENTS.md (earlier in precedence) wins.
        fs::write(proj.join("AGENTS.md"), "agents rules").unwrap();
        fs::write(proj.join("CLAUDE.md"), "claude rules").unwrap();
        let out = render_instructions(&d.path().join("nohome"), proj);
        assert!(out.contains("PROJECT INSTRUCTIONS"));
        assert!(out.contains("agents rules"));
        assert!(!out.contains("claude rules"), "first-precedence file wins");
        assert!(out.contains("AGENTS.md"), "header carries the source path");
    }

    #[test]
    fn global_and_user_tiers_included() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("ATOMCODE.md"), "global rules").unwrap();
        let proj = d.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join(".atomcode.user.md"), "user rules").unwrap();
        let out = render_instructions(&home, &proj);
        assert!(out.contains("GLOBAL INSTRUCTIONS") && out.contains("global rules"));
        assert!(out.contains("USER INSTRUCTIONS") && out.contains("user rules"));
        // Precedence preamble is present and sits BEFORE the instruction blocks (so the
        // model reads "these override defaults" next to the user's own rules).
        let prec = out.find("PRECEDENCE").expect("preamble present");
        let global_idx = out.find("GLOBAL INSTRUCTIONS").unwrap();
        assert!(prec < global_idx, "preamble precedes the blocks: {out}");
        assert!(
            out.contains("OVERRIDE") || out.contains("take PRECEDENCE"),
            "states override: {out}"
        );
        assert!(
            out.contains("govern work on the project only"),
            "scopes project instructions to project work: {out}"
        );
        assert!(
            out.contains(
                "do not describe or override the host application or active configured model"
            ),
            "protects runtime identity at the instruction boundary: {out}"
        );
    }

    #[test]
    fn no_precedence_preamble_when_no_instructions() {
        // Empty output (no tiers) must stay empty — the preamble must NOT leak when there
        // are no user instructions to elevate (else the caller emits a dangling section).
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let out = render_instructions(&d.path().join("nohome"), &proj);
        assert!(
            out.is_empty(),
            "no tiers → fully empty, no preamble: {out:?}"
        );
    }

    #[test]
    fn missing_and_empty_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("AGENTS.md"), "   \n  ").unwrap(); // whitespace-only → empty
        let out = render_instructions(&d.path().join("nohome"), &proj);
        assert!(out.is_empty(), "no non-empty tiers → empty: {out:?}");
    }

    #[test]
    fn oversized_file_is_truncated() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let big = "x".repeat(MAX_INSTRUCTIONS_BYTES + 100);
        fs::write(proj.join("AGENTS.md"), &big).unwrap();
        let out = render_instructions(&d.path().join("nohome"), &proj);
        assert!(out.contains("[truncated:"), "oversized tier truncated");
    }
}
