//! Render the installed-skills catalog into a system-prompt section.
//!
//! Two problems this solves (see also the verbatim-aligned twin in
//! `atomcode-capabilities/src/skills/render.rs`):
//!
//! 1. **Signal dilution** — a machine with 60+ community skills installed would
//!    otherwise dump every full description into the prompt, drowning the few
//!    high-value process skills (brainstorming, systematic-debugging, …). The
//!    render is *budget-gated*: under budget everything is emitted verbatim (few
//!    skills → zero overhead, no reordering visible); only when the catalog
//!    exceeds [`CATALOG_BYTE_BUDGET`] does source-priority ranking decide who
//!    survives and the rest are summarised as an omitted count.
//! 2. **Weak model nudge** — the guidance paragraph tells the model to load a
//!    skill when the task *matches its description*, not only when the user names
//!    it, and points at the create-a-feature / design-work case explicitly.
//!
//! Kept verbatim-identical to the capabilities twin on purpose: the two
//! `SkillRegistry` implementations don't share code (capabilities does not depend
//! on core), same rationale as `model_suggests_vision`. If you edit one, edit both.

use std::path::Path;

/// Total byte budget for the skill list body (excludes the header + guidance
/// paragraph, which are always emitted). Bytes, not chars: for ASCII skill
/// names/descriptions the two coincide; CJK bites the budget sooner (a closer
/// token proxy anyway). Mirrors codex's ~8 KB default.
pub const CATALOG_BYTE_BUDGET: usize = 8000;

/// Per-skill description cap. A single pathological description cannot eat the
/// whole budget; anything longer is truncated with an ellipsis.
pub const PER_SKILL_DESC_CAP: usize = 1024;

/// First line of the rendered block; the injection hook matches this prefix to
/// reconcile the block in place across `--resume`.
pub const CATALOG_HEADER: &str = "=== AVAILABLE SKILLS ===";

const GUIDANCE: &str = "Skills are reusable instruction templates for specific tasks. If a task clearly matches a skill's description — not only when the user names the skill — you MUST load it with the `use_skill` tool and follow it BEFORE doing the work, INCLUDING before asking the user clarifying questions, exploring, or planning (the skill guides those steps). Announce in one line which skill you're using; if you skip an obviously matching skill, say why. For example, before designing or building a feature, a component, or a plan, load the matching skill first. If several skills match, use the minimal set that covers the request; if none match, proceed normally.";

/// One catalog row, already reduced from a crate-specific `Skill`. `source_rank`
/// is computed via [`source_rank`]; lower = higher priority when budget forces
/// cuts.
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub name: String,
    /// Argument autocomplete hint, e.g. `[issue-number]`. Capabilities skills
    /// don't carry one (always `None`); the core twin passes it through.
    pub hint: Option<String>,
    pub description: String,
    pub source_rank: u8,
}

/// Source-priority tier from a skill's `source_path`. Curated, product-native
/// dirs survive a budget squeeze over community bulk. Kept as approved:
/// `.atomcode` > `.claude` > `.agents` > everything else.
pub fn source_rank(path: &Path) -> u8 {
    let s = path.to_string_lossy();
    if s.contains(".atomcode") {
        0
    } else if s.contains(".claude") {
        1
    } else if s.contains(".agents") {
        2
    } else {
        3
    }
}

/// Truncate a description to [`PER_SKILL_DESC_CAP`] chars on a char boundary,
/// appending `…` when cut.
fn truncate_desc(desc: &str) -> String {
    if desc.chars().count() <= PER_SKILL_DESC_CAP {
        return desc.to_string();
    }
    let cut: String = desc.chars().take(PER_SKILL_DESC_CAP).collect();
    format!("{cut}…")
}

/// Render the full `=== AVAILABLE SKILLS ===` section, or `None` when there are
/// no skills. The returned string has no surrounding blank lines — the caller
/// wraps it with newlines to taste.
pub fn render_skill_catalog(entries: &[CatalogEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    // Rank first (source tier), then name for determinism / prompt-cache
    // stability within a tier.
    let mut sorted: Vec<&CatalogEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        a.source_rank
            .cmp(&b.source_rank)
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut lines: Vec<String> = Vec::new();
    let mut body_bytes = 0usize; // skill-list body only, NOT header+guidance
    let mut omitted = 0usize;
    for e in &sorted {
        let hint = e
            .hint
            .as_deref()
            .map(|h| format!(" {h}"))
            .unwrap_or_default();
        let line = format!("- {}{}: {}", e.name, hint, truncate_desc(&e.description));
        let cost = line.len() + 1; // + newline
                                   // Always emit at least the top-ranked skill even if it alone is huge.
        if lines.is_empty() || body_bytes + cost <= CATALOG_BYTE_BUDGET {
            body_bytes += cost;
            lines.push(line);
        } else {
            omitted += 1;
        }
    }

    let mut out = String::new();
    out.push_str(CATALOG_HEADER);
    out.push('\n');
    out.push_str(GUIDANCE);
    out.push('\n');
    out.push_str(&lines.join("\n"));
    if omitted > 0 {
        out.push('\n');
        out.push_str(&format!(
            "... and {omitted} more lower-priority skills not shown."
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str, desc: &str, rank: u8) -> CatalogEntry {
        CatalogEntry {
            name: name.into(),
            hint: None,
            description: desc.into(),
            source_rank: rank,
        }
    }

    #[test]
    fn empty_yields_none() {
        assert!(render_skill_catalog(&[]).is_none());
    }

    #[test]
    fn source_rank_tiers() {
        assert_eq!(
            source_rank(&PathBuf::from("/home/u/.atomcode/skills/x/SKILL.md")),
            0
        );
        assert_eq!(
            source_rank(&PathBuf::from("/home/u/.claude/skills/x/SKILL.md")),
            1
        );
        assert_eq!(
            source_rank(&PathBuf::from("/home/u/.agents/skills/x/SKILL.md")),
            2
        );
        assert_eq!(source_rank(&PathBuf::from("/opt/plugins/foo/SKILL.md")), 3);
    }

    #[test]
    fn small_catalog_emits_all_with_guidance_and_no_omitted_note() {
        let out = render_skill_catalog(&[
            entry("brainstorming", "before creative work", 0),
            entry("seo", "search stuff", 2),
        ])
        .unwrap();
        assert!(out.starts_with("=== AVAILABLE SKILLS ==="));
        assert!(out.contains("use_skill"));
        // codex-style anti-bypass framing: mandatory-if-matches + justify a skip.
        assert!(out.contains("MUST"), "mandatory-if-matches framing");
        assert!(
            out.contains("say why"),
            "accountability: justify skipping an obvious match"
        );
        assert!(out.contains("- brainstorming: before creative work"));
        assert!(out.contains("- seo: search stuff"));
        assert!(
            !out.contains("not shown"),
            "no omission under budget: {out}"
        );
    }

    #[test]
    fn ranks_curated_before_community() {
        // Names chosen so name-sort would REVERSE the desired order; rank must win.
        let out = render_skill_catalog(&[
            entry("zzz-native", "d", 0),    // .atomcode
            entry("aaa-community", "d", 2), // .agents
        ])
        .unwrap();
        let native = out.find("zzz-native").unwrap();
        let community = out.find("aaa-community").unwrap();
        assert!(native < community, "curated must precede community:\n{out}");
    }

    #[test]
    fn over_budget_omits_lowest_rank_and_counts() {
        // Each description ~500 chars; ~30 skills ⇒ well over 8000-char budget.
        let big = "x".repeat(500);
        let mut entries: Vec<CatalogEntry> = Vec::new();
        entries.push(entry("keep-me", "critical process skill", 0)); // curated, must survive
        for i in 0..40 {
            entries.push(entry(&format!("community-{i:02}"), &big, 2));
        }
        let out = render_skill_catalog(&entries).unwrap();
        assert!(
            out.contains("- keep-me: critical process skill"),
            "curated survived:\n{out}"
        );
        assert!(
            out.contains("more lower-priority skills not shown"),
            "omission note present"
        );
        // Body must respect the budget (allow header+guidance+note overhead).
        assert!(
            out.len() < CATALOG_BYTE_BUDGET + GUIDANCE.len() + 400,
            "budget respected: {}",
            out.len()
        );
    }

    #[test]
    fn long_description_is_truncated() {
        let long = "d".repeat(PER_SKILL_DESC_CAP + 500);
        let out = render_skill_catalog(&[entry("x", &long, 0)]).unwrap();
        assert!(out.contains('…'), "truncation ellipsis present");
        assert!(
            !out.contains(&long),
            "full over-cap description must not appear verbatim"
        );
    }

    #[test]
    fn always_emits_top_ranked_even_if_alone_over_budget() {
        let huge = "d".repeat(PER_SKILL_DESC_CAP); // capped, but line still large
        let out = render_skill_catalog(&[entry("solo", &huge, 0)]).unwrap();
        assert!(
            out.contains("- solo: "),
            "top-ranked always emitted:\n{}",
            &out[..80.min(out.len())]
        );
    }
}
