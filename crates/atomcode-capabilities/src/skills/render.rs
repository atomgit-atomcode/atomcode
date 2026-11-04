//! Render the installed-skills catalog into a system-prompt section.
//!
//! Budgeted discovery for the native skill registry:
//!
//! 1. **Signal dilution** — a machine with 60+ community skills installed would
//!    otherwise dump every full description into the prompt, drowning the few
//!    high-value process skills (brainstorming, systematic-debugging, …). The
//!    render is *budget-gated*: under budget everything is emitted verbatim (few
//!    skills → zero overhead); only when the catalog
//!    exceeds [`CATALOG_BYTE_BUDGET`] does source-priority ranking decide who
//!    survives when even identity-only entries cannot fit. Descriptions share
//!    the remaining space before any identity is omitted.
//! 2. **Weak model nudge** — the guidance paragraph tells the model to load a
//!    skill when the task *matches its description*, not only when the user names
//!    it, and points at the create-a-feature / design-work case explicitly.
//!
//! This is the single native skill-catalog renderer.

use std::path::{Path, PathBuf};

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

const GUIDANCE: &str = "Before taking task actions, match the request against the skill descriptions below. If the user names a skill or a description clearly matches, you MUST load that exact skill with `use_skill` BEFORE searching files, running commands, planning, proposing fixes, or asking clarifying questions; then follow its instructions. A bug report, failed test, or a fix that still fails is a task to match, even when no skill is named. Use the minimal matching set. Never invent or guess a skill name: use only exact names from this catalog or `list_skills`. If skills were omitted, call `list_skills` to discover them before choosing an unlisted name. If none match, proceed normally. If loading fails, report it briefly and continue with the best fallback. Announce the skill you load; if you skip an obviously matching skill, say why.";

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
/// product-native > `.claude` > `.agents` > everything else.
///
/// "Product-native" is whatever lies under `native_roots` — our own trees, as
/// the host resolved them ([`SkillRegistry::set_native_roots`](super::SkillRegistry::set_native_roots)).
/// Prefix, not substring: a substring test on a short directory name would rank
/// unrelated paths that merely contain it.
///
/// It used to be the literal `.atomcode` (later `$ATOMCODE_HOME` with that
/// literal as the fallback), which is correct only while the tree is named
/// that. A build that relocated its tree ranked its OWN installed plugins at 3
/// — below `~/.claude` and `~/.agents` — so a budget squeeze cut the
/// deployment's curated plugins and kept third-party bulk. The symptom is
/// invisible from the outside: the plugin installs fine and `/plugin list`
/// shows it, but its name never reaches the model, so the model answers as if
/// the skill did not exist.
pub fn source_rank(path: &Path, native_roots: &[PathBuf]) -> u8 {
    if native_roots
        .iter()
        .any(|root| !root.as_os_str().is_empty() && path.starts_with(root))
    {
        return 0;
    }
    let s = path.to_string_lossy();
    if s.contains(".claude") {
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
    render_skill_catalog_prioritizing(entries, &[])
}

/// Render while keeping installed skills explicitly named in project/user
/// instructions ahead of source-rank ordering. This only recognizes exact skill
/// names at token boundaries; it never guesses a skill from prose.
pub fn render_skill_catalog_prioritizing(
    entries: &[CatalogEntry],
    priority_names: &[String],
) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    // Explicit project references survive the budget first, then source tier and
    // name preserve the existing deterministic/cache-stable order.
    let mut sorted: Vec<&CatalogEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        let a_priority = priority_names.iter().any(|name| name == &a.name);
        let b_priority = priority_names.iter().any(|name| name == &b.name);
        b_priority.cmp(&a_priority).then_with(|| {
            a.source_rank
                .cmp(&b.source_rank)
                .then_with(|| a.name.cmp(&b.name))
        })
    });

    let bases: Vec<String> = sorted
        .iter()
        .map(|e| {
            let hint = e
                .hint
                .as_deref()
                .map(|h| format!(" {h}"))
                .unwrap_or_default();
            format!("- {}{}", e.name, hint)
        })
        .collect();
    let descriptions: Vec<String> = sorted
        .iter()
        .map(|e| truncate_desc(&e.description))
        .collect();
    let full: Vec<String> = bases
        .iter()
        .zip(&descriptions)
        .map(|(base, desc)| format!("{base}: {desc}"))
        .collect();
    let full_cost = full.iter().map(|line| line.len() + 1).sum::<usize>();
    let minimum_cost = bases.iter().map(|line| line.len() + 1).sum::<usize>();
    let mut omitted = 0;
    let mut shortened = false;
    let lines = if full_cost <= CATALOG_BYTE_BUDGET {
        full
    } else if minimum_cost <= CATALOG_BYTE_BUDGET {
        // Reserve every identity first, then distribute description prefixes
        // round-robin. Account in UTF-8 bytes, including punctuation and ellipses.
        shortened = true;
        let chars: Vec<Vec<char>> = descriptions.iter().map(|d| d.chars().collect()).collect();
        let mut counts = vec![0; bases.len()];
        let mut costs = vec![0; bases.len()];
        let mut prefix_costs = vec![0; bases.len()];
        let mut remaining = CATALOG_BYTE_BUDGET - minimum_cost;
        loop {
            let mut changed = false;
            for (i, description) in chars.iter().enumerate() {
                if counts[i] == description.len() {
                    continue;
                }
                let next = counts[i] + 1;
                let prefix_bytes = prefix_costs[i] + description[counts[i]].len_utf8();
                let cost = 2
                    + prefix_bytes
                    + if next < description.len() {
                        "…".len()
                    } else {
                        0
                    };
                if cost <= costs[i] + remaining {
                    remaining = remaining + costs[i] - cost;
                    costs[i] = cost;
                    counts[i] = next;
                    prefix_costs[i] = prefix_bytes;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        bases
            .iter()
            .enumerate()
            .map(|(i, base)| {
                if counts[i] == 0 {
                    return base.clone();
                }
                let prefix: String = chars[i][..counts[i]].iter().collect();
                let suffix = if counts[i] < chars[i].len() {
                    "…"
                } else {
                    ""
                };
                format!("{base}: {prefix}{suffix}")
            })
            .collect::<Vec<_>>()
    } else {
        // Even identities do not fit: preserve source/project-reference order,
        // without letting one oversized name break the hard byte budget.
        shortened = true;
        let mut used = 0;
        bases
            .into_iter()
            .filter(|line| {
                let cost = line.len() + 1;
                if cost <= CATALOG_BYTE_BUDGET - used {
                    used += cost;
                    true
                } else {
                    omitted += 1;
                    false
                }
            })
            .collect::<Vec<_>>()
    };

    let mut out = String::new();
    out.push_str(CATALOG_HEADER);
    out.push('\n');
    out.push_str(GUIDANCE);
    out.push('\n');
    out.push_str(&lines.join("\n"));
    if shortened {
        out.push_str("\nSkill descriptions were shortened or removed to fit the catalog budget. Search `list_skills` by task keywords against full descriptions when these summaries are insufficient.");
    }
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
        let native = [PathBuf::from("/home/u/.ours")];
        assert_eq!(
            source_rank(&PathBuf::from("/home/u/.ours/skills/x/SKILL.md"), &native),
            0
        );
        assert_eq!(
            source_rank(&PathBuf::from("/home/u/.claude/skills/x/SKILL.md"), &native),
            1
        );
        assert_eq!(
            source_rank(&PathBuf::from("/home/u/.agents/skills/x/SKILL.md"), &native),
            2
        );
        assert_eq!(
            source_rank(&PathBuf::from("/opt/plugins/foo/SKILL.md"), &native),
            3
        );
    }

    /// The regression this ranking was silently getting wrong on any build
    /// whose config dir is not literally `.atomcode`: its OWN installed
    /// plugins ranked below `~/.claude` and `~/.agents`, so a catalog over
    /// budget dropped the curated ones and kept third-party bulk.
    #[test]
    fn a_relocated_tree_still_outranks_third_party_dirs() {
        let native = [PathBuf::from("/home/u/.longcode")];
        let own =
            PathBuf::from("/home/u/.longcode/plugins/marketplaces/mp/plugins/p/skills/s/SKILL.md");
        let claude = PathBuf::from("/home/u/.claude/skills/x/SKILL.md");
        let agents = PathBuf::from("/home/u/.agents/skills/x/SKILL.md");

        assert_eq!(
            source_rank(&own, &native),
            0,
            "this build's own tree must rank top"
        );
        assert!(source_rank(&own, &native) < source_rank(&claude, &native));
        assert!(source_rank(&own, &native) < source_rank(&agents, &native));
    }

    /// Nothing is ours by name: a path that merely spells the upstream
    /// directory, under a build whose trees are elsewhere, is not native.
    #[test]
    fn the_upstream_name_confers_nothing() {
        let native = [PathBuf::from("/home/u/.longcode")];
        assert_eq!(
            source_rank(
                &PathBuf::from("/home/u/.atomcode/skills/x/SKILL.md"),
                &native
            ),
            3
        );
    }

    #[test]
    fn an_empty_root_does_not_make_everything_native() {
        // An empty path is a bare relative path; left unchecked, every relative
        // source path would "start with" it.
        let native = [PathBuf::new()];
        assert_eq!(
            source_rank(&PathBuf::from("rel/.agents/skills/x/SKILL.md"), &native),
            2
        );
        assert_eq!(source_rank(&PathBuf::from("rel/foo/SKILL.md"), &native), 3);
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
        assert!(
            out.contains("use only exact names from this catalog or `list_skills`"),
            "catalog names form a closed set: {out}"
        );
        assert!(
            out.contains("Never invent or guess a skill name"),
            "must prohibit hallucinated skill names: {out}"
        );
        assert!(
            out.contains("If none match, proceed normally"),
            "no-match fallback: {out}"
        );
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
    fn explicit_instruction_reference_outranks_source_tier() {
        let out = render_skill_catalog_prioritizing(
            &[
                entry("native-general", "d", 0),
                entry("superpowers:writing-plans", "d", 3),
            ],
            &["superpowers:writing-plans".into()],
        )
        .unwrap();
        let referenced = out.find("superpowers:writing-plans").unwrap();
        let native = out.find("native-general").unwrap();
        assert!(
            referenced < native,
            "explicit project reference must win:\n{out}"
        );
    }

    #[test]
    fn over_budget_keeps_all_identities_and_shares_description_space() {
        // Full descriptions exceed the byte budget, but every identity fits.
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
            !out.contains("more lower-priority skills not shown"),
            "identities fit, so none should be omitted"
        );
        assert!(
            out.contains("call `list_skills`"),
            "omitted names require discovery before invocation"
        );
        for i in 0..40 {
            assert!(out.contains(&format!("- community-{i:02}:")));
        }
        // Body must respect the budget (allow header+guidance+note overhead).
        assert!(
            out.len() < CATALOG_BYTE_BUDGET + GUIDANCE.len() + 600,
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
    fn a_single_capped_description_fits_without_omission() {
        let huge = "d".repeat(PER_SKILL_DESC_CAP); // capped, but line still large
        let out = render_skill_catalog(&[entry("solo", &huge, 0)]).unwrap();
        assert!(
            out.contains("- solo: "),
            "single skill emitted:\n{}",
            &out[..80.min(out.len())]
        );
    }
    fn catalog_body(out: &str) -> Vec<&str> {
        out.lines().filter(|line| line.starts_with("- ")).collect()
    }

    #[test]
    fn hundreds_of_skills_share_budget_without_losing_names() {
        let entries: Vec<_> = (0..250)
            .map(|i| entry(&format!("s{i:03}"), &"中文用途说明".repeat(100), 1))
            .collect();
        let out = render_skill_catalog(&entries).unwrap();
        let lines = catalog_body(&out);
        assert_eq!(lines.len(), 250);
        assert!(lines.iter().map(|line| line.len() + 1).sum::<usize>() <= CATALOG_BYTE_BUDGET);
        assert!(lines.iter().all(|line| line.contains(": 中")));
        assert_eq!(
            out,
            render_skill_catalog(&entries.into_iter().rev().collect::<Vec<_>>()).unwrap()
        );
    }

    #[test]
    fn identity_only_overflow_keeps_priority_and_discovery_notice() {
        let entries: Vec<_> = (0..1000)
            .map(|i| entry(&format!("skill-{i:04}"), "description", 1))
            .collect();
        let out = render_skill_catalog_prioritizing(&entries, &["skill-0999".into()]).unwrap();
        let lines = catalog_body(&out);
        assert_eq!(lines[0], "- skill-0999");
        assert!(lines.len() < entries.len());
        assert!(lines.iter().all(|line| !line.contains("description")));
        assert!(lines.iter().map(|line| line.len() + 1).sum::<usize>() <= CATALOG_BYTE_BUDGET);
        assert!(out.contains("more lower-priority skills not shown"));
        assert!(out.contains("list_skills"));
    }

    #[test]
    fn oversized_identity_cannot_break_budget_or_hide_shorter_entries() {
        let out = render_skill_catalog(&[
            entry(&"a".repeat(CATALOG_BYTE_BUDGET), "x", 0),
            entry("small", "y", 1),
        ])
        .unwrap();
        assert_eq!(catalog_body(&out), vec!["- small"]);
        assert!(out.contains("1 more lower-priority skills not shown"));
    }
    #[test]
    fn identities_at_exact_budget_remain_visible_without_descriptions() {
        let long_name = "a".repeat(CATALOG_BYTE_BUDGET - 7);
        let out =
            render_skill_catalog(&[entry(&long_name, "description", 0), entry("z", "中文", 1)])
                .unwrap();
        let lines = catalog_body(&out);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines.iter().map(|line| line.len() + 1).sum::<usize>(),
            CATALOG_BYTE_BUDGET
        );
        assert!(lines.iter().all(|line| !line.contains(": ")));
        assert!(!out.contains("more lower-priority skills not shown"));
        assert!(out.contains("against full descriptions"));
    }

    #[test]
    fn equal_descriptions_receive_round_robin_prefixes() {
        let entries: Vec<_> = (0..100)
            .map(|i| entry(&format!("s{i:03}"), &"x".repeat(1000), 1))
            .collect();
        let out = render_skill_catalog(&entries).unwrap();
        let lines = catalog_body(&out);
        let lengths: Vec<_> = lines
            .iter()
            .map(|line| line.split_once(": ").unwrap().1.len())
            .collect();
        assert!(lengths.iter().max().unwrap() - lengths.iter().min().unwrap() <= 1);
        assert!(lines.iter().map(|line| line.len() + 1).sum::<usize>() <= CATALOG_BYTE_BUDGET);
    }
}
