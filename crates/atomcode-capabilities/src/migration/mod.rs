//! Scan the assets other AI coding agents left behind.
//!
//! A neutral, read-only capability: it is handed the paths to look at and
//! hands back a report. It resolves no environment and writes no file — the
//! host owns where the trees are (`AGENTS.md`: "解析环境只在宿主") and owns
//! every write.
//!
//! The shape follows Codex's `external-agent-migration`: detect, then report,
//! and never let an asset silently vanish — every asset found ends up in the
//! report with one of four statuses.
//!
//! Opt-in `migration` feature. See
//! `docs/plans/2026-11-17-external-agent-asset-migration-design.md`.

// The sources land one per task; each helper here is used by the source that
// comes next. The allow comes off when the last source is in.
#![allow(dead_code)]

mod claude;
mod codex;
mod opencode;
mod provider;
pub mod report;
mod text;

use std::path::{Path, PathBuf};

// The FORM the registry's walk found an entry in — a skill directory or a
// flat file. The INDEX cannot say this: both forms go into the skill index.
use crate::skills::EntryKind;

// `endpoint_in_use` is the endpoint an account will actually talk to. It is
// re-exported because `provider` is a **private** module: the host needs it
// to name the endpoint in its confirmation prompt, and the host cannot reach
// into a private module.
pub use provider::{endpoint_in_use, ApiKeyRef, ProviderAccountDraft, ProviderScan};
pub use report::{AssetKind, AssetReport, AssetScope, AssetStatus, MigrationReport, SourceReport};

/// Where to look. The host resolves all of it — this crate reads no environment.
///
/// **The two homes.** `os_home` is the person's home directory, where
/// `~/.claude` and `~/.codex` live. It is NOT the product's tree: the global
/// instruction tier reads `<product tree>/ATOMCODE.md` and the permission
/// loader reads `<product tree>` — see `instructions.rs:128` and
/// `permission_settings.rs:286`. The field is named `os_home` rather than
/// `home` because this crate already has another `home` meaning, and mixing
/// them up would make the report claim a file is read when it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanContext {
    /// The person's home directory (`~/.claude` lives under it).
    pub os_home: PathBuf,
    /// The current project root.
    pub project: PathBuf,
    /// Where OUR trees are, so the report can name a target path instead of
    /// saying "copy it to the user tree" and leaving the person to guess.
    ///
    /// `ProductDirs` lives in this crate (`product_dirs.rs`), so carrying it
    /// costs no new dependency.
    pub product: crate::ProductDirs,
    /// The skill directories the runtime ACTUALLY uses, in the same LOW→HIGH
    /// order.
    ///
    /// **The host resolves these**, because only the host knows the user's
    /// configured `extra_skill_dirs` (`coding/src/parts.rs:558`
    /// `resolve_skill_dirs_at` expands `~`, makes relative paths
    /// project-relative, and puts the extras FIRST — before the standard
    /// list).
    ///
    /// The capability cannot recompute them: `standard_skill_dirs` alone
    /// would miss the configured roots, and the registry could then pick a
    /// DIFFERENT winner for a same-named skill than the runtime does — while
    /// the report claimed to be saying "the way the runtime does".
    ///
    /// **`None` and `Some(vec![])` mean different things.**
    ///
    /// - `None` = the host gave no list → the standard list
    ///   (`standard_skill_dirs`) is used.
    /// - `Some(vec![])` = the host EXPLICITLY asked for **no** skill roots —
    ///   the runtime has its skill tools turned off. The report must not
    ///   fall back to the standard list and then claim `~/.claude/skills/foo`
    ///   is `Active`: nothing is loading it.
    /// - `Some(dirs)` = exactly these roots, in this order.
    pub skill_dirs: Option<Vec<PathBuf>>,
    /// Where OpenCode keeps its configuration
    /// (`$XDG_CONFIG_HOME/opencode`, default `~/.config/opencode`).
    ///
    /// **The host resolves it** — the capability reads no environment
    /// (`AGENTS.md`: 「解析环境只在宿主」).
    pub opencode_config_dir: PathBuf,
    /// Where OpenCode keeps its data
    /// (`$XDG_DATA_HOME/opencode`, default `~/.local/share/opencode`).
    ///
    /// The host resolves it, for the same reason.
    pub opencode_data_dir: PathBuf,
}

/// One external agent's assets.
pub trait MigrationSource: Send + Sync {
    /// A stable id, e.g. `"claude-code"`.
    fn id(&self) -> &'static str;
    /// The name a person knows this agent by.
    fn display_name(&self) -> &'static str;
    /// Look, and report. Must not write anything.
    ///
    /// `truth` is the registry load `scan()` did — **the ONE load for the
    /// whole report**. A source that loaded its own would see a different
    /// moment of the same disk than the source next to it, and the report
    /// would contradict itself.
    fn detect(&self, ctx: &ScanContext, truth: &SkillTruth) -> Vec<AssetReport>;
    /// Provider drafts and any source files whose configuration could not be read.
    fn scan_providers(&self, _ctx: &ScanContext) -> ProviderScan {
        ProviderScan::default()
    }

    #[cfg(test)]
    fn provider_drafts(&self, ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        self.scan_providers(ctx).drafts
    }
}

/// The agents we know how to read.
pub fn builtin_sources() -> Vec<Box<dyn MigrationSource>> {
    vec![
        Box::new(claude::ClaudeSource),
        Box::new(codex::CodexSource),
        Box::new(opencode::OpenCodeSource),
    ]
}

/// Scan every source. A source that finds nothing contributes no rows.
pub fn scan(sources: &[Box<dyn MigrationSource>], ctx: &ScanContext) -> MigrationReport {
    scan_with_provider_drafts(sources, ctx).0
}

/// Use the same provider drafts for the visible rows and the host's re-check.
pub fn scan_with_provider_drafts(
    sources: &[Box<dyn MigrationSource>],
    ctx: &ScanContext,
) -> (MigrationReport, Vec<ProviderAccountDraft>) {
    let mut report = MigrationReport::default();
    let mut drafts = Vec::new();
    let truth = SkillTruth::load(ctx);
    for source in sources {
        let mut assets = source.detect(ctx, &truth);
        let providers = source.scan_providers(ctx);
        assets.extend(providers.failures);
        assets.extend(providers.drafts.iter().map(|draft| {
            provider_row(
                draft,
                AssetStatus::Actionable,
                text::provider_copyable(
                    draft.base_url.as_deref(),
                    draft.api_key.as_ref().map(ApiKeyRef::kind),
                ),
            )
        }));
        drafts.extend(providers.drafts);
        if !assets.is_empty() {
            report.sources.push(SourceReport {
                source_id: source.id(),
                display_name: source.display_name(),
                assets,
            });
        }
    }
    (report, drafts)
}

/// The report row for a provider account.
///
/// **The row's NAME is the name the source wrote**, and the normalized
/// account id the importer will write is in `draft.id` — that id, not the
/// row's name, is what the re-check matches. The name stays the source's own
/// because two definitions can normalize to the same id (`"foo bar"` and
/// `"foo-bar"` both become `foo-bar`): only the original name tells them
/// apart, and it is the one the person recognises. Every source builds its
/// provider rows through this one function, so the rule lives in one place.
///
/// The `detail` is passed in rather than built here: this module has no
/// sentence of its own, and the source that found the account knows which
/// wording applies to it.
pub fn provider_row(
    draft: &ProviderAccountDraft,
    status: AssetStatus,
    detail: String,
) -> AssetReport {
    // A draft that cannot be imported is reported as `Blocked`, with the
    // reason it carries — not as "can be brought over".
    let (status, detail) = match &draft.blocked {
        Some(reason) => (AssetStatus::Blocked, reason.clone()),
        None => (status, detail),
    };
    AssetReport {
        kind: AssetKind::ProviderConfig,
        // The name the SOURCE wrote, not the normalized id: it is what the
        // person recognises, and it is what tells two colliding definitions
        // apart.
        name: draft.source_name.clone(),
        path: draft.source_path.clone(),
        scope: draft.scope,
        status,
        detail,
    }
}

/// The last component of a path, for a report row's name.
fn last_component(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default();
    name.to_string_lossy().into_owned()
}

/// A tree of skills: do we scan it, or is it someone else's?
///
/// **The tree decides where the CANDIDATES come from.**
///
/// - [`SkillTree::Scanned`] — a tree the REGISTRY scans. The candidates are
///   the registry's own discoveries; the report does not walk the disk a
///   second time. An asset the registry never found is **not in the report
///   at all**: the registry does not think it is an asset, and neither does
///   the report.
/// - [`SkillTree::Foreign`] — a tree NOBODY scans. The report walks it
///   itself, with the report's own limits and the report's own sentences
///   about them.
///
/// `Clone`, not `Copy`: `Foreign` owns a `PathBuf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillTree {
    /// We scan this tree — the registry does. Its discoveries are the
    /// candidates, and its verdicts decide whether each one is really in
    /// effect — or merely shadowed, or rejected.
    Scanned,
    /// We do NOT scan this tree. Nothing in it is in effect — unless the
    /// registry happens to load this tree anyway (a host that put it in the
    /// skill roots), in which case the registry's own word wins. Otherwise
    /// every row says where to bring it over.
    ///
    /// `target` is the directory to copy into (`skills/` or `commands/`)
    /// itself — no further `skills`/`commands` component is added. Each row
    /// joins its own name onto it, shaped like the asset: `<name>` for a
    /// skill directory, `<name>.md` for a flat command file.
    Foreign { target: PathBuf },
}

impl SkillTree {
    /// The depth-limit sentence — **WHO** stopped the walk.
    ///
    /// The registry has a depth limit of its own, and it says so in its own
    /// words — but only for a tree it scans. In a tree it never scanned, the
    /// **report's** walk is what stopped, and saying "the skill loader
    /// rejected it" would put words in the loader's mouth about a file the
    /// loader never looked at.
    fn depth_limit_detail(&self) -> String {
        match self {
            SkillTree::Scanned => text::skill_rejected(&text::skill_depth_limit()),
            SkillTree::Foreign { .. } => text::report_depth_limit(),
        }
    }

    /// The symlink-cycle sentence — **WHO** stopped the walk. The same
    /// split as [`Self::depth_limit_detail`].
    fn symlink_cycle_detail(&self) -> String {
        match self {
            SkillTree::Scanned => text::skill_rejected(&text::skill_symlink_cycle()),
            SkillTree::Foreign { .. } => text::report_symlink_cycle(),
        }
    }
    /// The status and sentence for one skill found in a FOREIGN tree.
    ///
    /// `path` is the skill directory or the flat command file; `name` is the
    /// registry's name for it (for a command file, already without `.md`).
    /// `parses` is whether the registry's own parser accepts the file.
    ///
    /// **Only for a tree the registry does NOT scan** — a tree it scans gets
    /// its candidates from the registry itself, and the registry's own
    /// verdict (see [`registry_rows`]).
    fn judge(
        &self,
        registry: &SkillTruth,
        name: &str,
        path: &Path,
        parses: bool,
    ) -> (AssetStatus, String) {
        match self {
            SkillTree::Scanned => registry.verdict(name, path, parses),
            SkillTree::Foreign { target } => {
                // A tree the registry does NOT scan can still hold an asset
                // the registry DID load: a host that put this tree in its
                // skill roots loaded it, and it is in effect right now. Then
                // "copy it over" is nonsense — the registry says what is
                // true, and we say that.
                let is_skill_dir = probe_dir(path).unwrap_or(false);
                if let Some(verdict) = registry.registry_says(name, path, is_skill_dir) {
                    return verdict;
                }
                // The registry found this asset and could not load it — with
                // its own reason. Copying it over would only move the
                // failure.
                if let Some((reason, rejected)) = registry.registry_failure(path, is_skill_dir) {
                    return if rejected {
                        (AssetStatus::Blocked, text::skill_rejected(&reason))
                    } else {
                        (AssetStatus::Blocked, text::report_entry_unusable(&reason))
                    };
                }
                // A file the loader cannot parse will not load there either:
                // copying it over only moves the failure. (The registry never
                // looked at this file, so this is not the loader's word — it
                // is ours.)
                if !parses {
                    return (AssetStatus::Blocked, text::foreign_unparseable());
                }
                // The registry loads flat commands as top-level `*.md` files
                // (`registry.rs:340`): a copy named `<name>` without the
                // extension would never be scanned, and the advice would not
                // work. A skill directory's name is its directory name.
                let is_command_file =
                    path.extension().and_then(|e| e.to_str()) == Some("md") && !is_skill_dir;
                let destination = if is_command_file {
                    target.join(format!("{name}.md"))
                } else {
                    target.join(name)
                };
                // "Bring it over" — an ACTION, not a promise: whether it wins
                // against a same-named higher-priority definition is not ours
                // to promise.
                (AssetStatus::Actionable, text::skill_copyable(&destination))
            }
        }
    }
}

// The skill helpers live HERE, not in one source: three sources report
// skills (Claude Code, OpenCode, and the shared `.agents` tree), and a
// helper that only one of them can reach is a helper the other two will
// re-implement — and then they will drift from the registry.
/// One row per skill directory (`<name>/SKILL.md`) or command file (`<name>.md`).
///
/// **Where the CANDIDATES come from depends on the tree.**
///
/// - [`SkillTree::Scanned`] — a tree the registry scans. The candidates are
///   **the registry's own discoveries**: it walked that tree, so it knows
///   what is under it. The report does **not** walk the disk a second time:
///   two walks of the same disk disagree (a symlink, an ignored directory, a
///   depth limit), and the report's answer must be the runtime's answer. So
///   an asset the registry never found is **not in the report at all** — it
///   is not ours, and listing it would mean explaining "why it is not in
///   effect", which is itself a guess.
/// - [`SkillTree::Foreign`] — a tree the registry does NOT scan. Nobody else
///   knows what is under it, so the report walks it itself. That walk is the
///   REPORT's own, with the report's own limits and the report's own
///   sentences about them.
///
/// **Whether the asset is really in effect is decided by the real registry**,
/// not by "the file exists". Two things make existence insufficient: a
/// `SKILL.md` that fails to parse is skipped, and a skill with the same name
/// in a **later, higher-priority** directory overrides this one — the
/// registry loads directories from low to high priority, so the last one
/// loaded wins. A report that says "already in the skill catalog" about an
/// overridden file is wrong.
///
/// **The row's name is the name the registry uses** — the `name:` in the
/// frontmatter, else the directory name or file stem, normalized — so the
/// report and the registry cannot disagree about what a skill is called.
fn skill_rows(
    kind: AssetKind,
    scope: AssetScope,
    root: &Path,
    registry: &SkillTruth,
    tree: SkillTree,
) -> Vec<AssetReport> {
    match tree {
        // A tree the registry scans: the registry's discoveries ARE the
        // candidates.
        SkillTree::Scanned => registry_rows(kind, scope, root, registry),
        // A tree nobody else scans: the report walks it itself.
        SkillTree::Foreign { .. } => foreign_rows(kind, scope, root, registry, &tree),
    }
}

/// The path the CALLER named, for an asset the registry found under a root.
///
/// The registry walks the LITERAL root it was handed, and the caller may
/// have named the same directory another way (the same path spelled two
/// ways — `/var` and `/private/var` on macOS). The report shows the path
/// the caller named, not the registry's spelling of it.
///
/// So: take the part of the asset's path that lies under the registry's
/// root, and re-attach it to the caller's root.
///
/// `None` when the asset is not under the registry's root at all — the
/// caller keeps its own path then.
fn re_spelled(root: &Path, registry_root: &Path, asset: &Path) -> Option<PathBuf> {
    let relative = asset.strip_prefix(registry_root).ok()?;
    Some(root.join(relative))
}

/// The rows for a tree the REGISTRY scans — its own discoveries.
///
/// **The candidates are the registry's**, filtered to the ones found under
/// THIS root: the root is what the caller named, and the registry says which
/// root found each entry.
///
/// **One row per DISCOVERY RECORD**, not per asset: the same directory
/// reached through two roots is two records, and the registry resolved them
/// against each other — the earlier one lost its name slot. Reporting the
/// loser as "in effect" would be the report's guess about which of two
/// records the runtime kept.
fn registry_rows(
    kind: AssetKind,
    scope: AssetScope,
    root: &Path,
    registry: &SkillTruth,
) -> Vec<AssetReport> {
    let mut out = Vec::new();
    let root_identity = identity(root);
    for found in &registry.discovered {
        // **Under THIS root?** The root is the one the caller named; the
        // registry may have spelled it another way (the same directory under
        // another name), so the identity settles it.
        if found.root != root_identity {
            continue;
        }
        // The row's PATH is the ASSET: the bundle directory for a skill
        // directory, the file for a flat template. **Spelled the way the
        // CALLER named it** — the registry may have spelled the same
        // directory another way, and the report shows the path the person
        // gave us.
        let path = re_spelled(root, &found.root_path, &found.asset_path)
            .unwrap_or_else(|| found.asset_path.clone());
        // **The registry's own verdict for THIS record**: it kept the record
        // that holds the name, and marked the others displaced. The flag was
        // set by the registry's slot resolution, not inferred here.
        let (status, detail) = if found.replaced {
            (AssetStatus::Blocked, text::skill_overridden(&found.name))
        } else {
            (AssetStatus::Active, text::skill_active())
        };
        out.push(AssetReport {
            kind,
            name: found.name.clone(),
            path,
            scope,
            status,
            detail,
        });
    }
    // **The assets the walk found but could NOT load.** They are assets too:
    // the file is there, the walk reached it, and the loader refused it.
    // Without a row they would vanish — and no asset may vanish.
    for failed in &registry.failed {
        if failed.root != root_identity {
            continue;
        }
        let (status, detail, path) =
            if matches!(failed.failure, crate::skills::LoadFailure::Rejected) {
                // The LOADER's own words: it read the file and refused it.
                // The row points at the ASSET — the bundle directory — the
                // same place a loaded skill's row points.
                let asset = re_spelled(root, &failed.root_path, &failed.asset_path)
                    .unwrap_or_else(|| failed.asset_path.clone());
                (
                    AssetStatus::Blocked,
                    text::skill_rejected(&failed.reason),
                    asset,
                )
            } else {
                // A fact about the ENTRY: the loader never treated it as a
                // skill at all, so this is not the loader's story to tell.
                // The row points at the ENTRY — that is the thing the person
                // has to look at.
                let entry = re_spelled(root, &failed.root_path, &failed.entry_path)
                    .unwrap_or_else(|| failed.entry_path.clone());
                (
                    AssetStatus::Blocked,
                    text::report_entry_unusable(&failed.reason),
                    entry,
                )
            };
        out.push(AssetReport {
            kind,
            name: failed.name.clone(),
            path,
            scope,
            status,
            detail,
        });
    }
    out
}

/// The rows for a tree NOBODY scans — the report walks it itself.
///
/// **The report's own walk**, with the report's own limits: the depth
/// bound, the ignored cache directories, the symlink-cycle guard. These are
/// not guesses about what the registry found — the registry never looked at
/// this tree — they are the boundaries of what the report can reach.
fn foreign_rows(
    kind: AssetKind,
    scope: AssetScope,
    root: &Path,
    registry: &SkillTruth,
    tree: &SkillTree,
) -> Vec<AssetReport> {
    let mut out = Vec::new();
    // Not `is_dir()`: it reports a directory we may not read as absent.
    match probe_dir(root) {
        Ok(true) => {}
        Ok(false) => {
            // "Not a directory" is not the same as "not there". A root that
            // IS there but is not a directory we can walk — a dangling
            // symlink, a regular file — is a row, not silence: the person
            // has to see that their tree is not what they think it is.
            if entry_exists(root) {
                out.push(AssetReport {
                    kind,
                    name: last_component(root),
                    path: root.to_path_buf(),
                    scope,
                    status: AssetStatus::Blocked,
                    detail: if is_broken_link(root) {
                        text::broken_link()
                    } else {
                        text::not_a_directory()
                    },
                });
            }
            return out;
        }
        Err(reason) => {
            out.push(AssetReport {
                kind,
                name: last_component(root),
                path: root.to_path_buf(),
                scope,
                status: AssetStatus::Blocked,
                detail: text::unreadable_file(&reason),
            });
            return out;
        }
    }
    // **Grouping directories are walked**: `skills/group/deploy/SKILL.md` is a
    // real skill, and a report that only looked at the immediate children
    // would drop it — a valid, active skill that vanishes from the report.
    const MAX_DEPTH: usize = 8;
    let mut ancestors: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    collect_skill_rows(
        kind,
        scope,
        root,
        0,
        MAX_DEPTH,
        &mut ancestors,
        registry,
        tree,
        &mut out,
    );
    out
}

/// Walk one FOREIGN skill directory — the REPORT's own walk.
///
/// **Only for a tree the registry does NOT scan.** A tree it scans gets its
/// candidates from the registry's own discoveries, so this walk would be a
/// second, disagreeing answer to the same question.
///
/// The limits here (the depth bound, the ignored cache directories, the
/// symlink-cycle guard) are **the report's own reach** — not a guess about
/// what the registry found, because the registry never looked at this tree.
#[allow(clippy::too_many_arguments)]
fn collect_skill_rows(
    kind: AssetKind,
    scope: AssetScope,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    ancestors: &mut std::collections::BTreeSet<PathBuf>,
    registry: &SkillTruth,
    tree: &SkillTree,
    out: &mut Vec<AssetReport>,
) {
    // A row about `dir` itself. The name matches the PATH this row reports
    // (`dir`), not the skills root it was found under.
    let blocked = |detail: String| AssetReport {
        kind,
        name: last_component(dir),
        path: dir.to_path_buf(),
        scope,
        status: AssetStatus::Blocked,
        detail,
    };
    if depth > max_depth {
        out.push(blocked(tree.depth_limit_detail()));
        return;
    }
    // The cycle guard: a symlink back to an ancestor would otherwise be
    // walked forever. The registry does the same (`registry.rs:320`).
    let identity = match std::fs::canonicalize(dir) {
        Ok(path) => path,
        Err(error) => {
            out.push(blocked(text::unreadable_file(&error.to_string())));
            return;
        }
    };
    if !ancestors.insert(identity.clone()) {
        out.push(blocked(tree.symlink_cycle_detail()));
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            out.push(blocked(text::unreadable_file(&error.to_string())));
            ancestors.remove(&identity);
            return;
        }
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    // An entry that cannot be read is a row, not a silent skip: a skill we
    // could not look at is one we could not promise works.
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(error) => out.push(blocked(text::unreadable_file(&error.to_string()))),
        }
    }
    paths.sort();
    // A row about one entry we could not tell the type of. The scan goes on
    // with the rest.
    let blocked_entry = |path: &Path, reason: String| AssetReport {
        kind,
        name: last_component(path),
        path: path.to_path_buf(),
        scope,
        status: AssetStatus::Blocked,
        detail: text::unreadable_file(&reason),
    };
    for path in paths {
        // Not `is_file()` / `is_dir()`: they report an entry we may not stat
        // as absent, and the entry would vanish from the report.
        let is_file = match probe_file(&path) {
            Ok(is_file) => is_file,
            Err(reason) => {
                out.push(blocked_entry(&path, reason));
                continue;
            }
        };
        let is_dir = if is_file {
            false
        } else {
            match probe_dir(&path) {
                Ok(is_dir) => is_dir,
                Err(reason) => {
                    out.push(blocked_entry(&path, reason));
                    continue;
                }
            }
        };
        if !is_file && !is_dir {
            // The entry is there, but it is neither a regular file nor a
            // directory. It is NOT absent — and an absent entry would be
            // skipped, but a present one that cannot be used must be a row:
            // a dangling symlink (the target is gone), a FIFO, a socket.
            //
            // `metadata` FOLLOWS the link, so a link whose target is gone
            // reads as "not there" — and the asset would vanish from the
            // report. `symlink_metadata` does not follow it, so the entry
            // itself is still visible.
            if !entry_exists(&path) {
                continue;
            }
            let name = if path.extension().and_then(|e| e.to_str()) == Some("md") {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            } else {
                last_component(&path)
            };
            // Computed BEFORE `path` moves into the row: the link has to be
            // asked about while the path is still ours.
            let detail = if is_broken_link(&path) {
                text::broken_link()
            } else {
                text::not_a_regular_file()
            };
            out.push(AssetReport {
                kind,
                name,
                path,
                scope,
                status: AssetStatus::Blocked,
                detail,
            });
            continue;
        }
        if is_file {
            // Flat command templates are TOP-LEVEL only (`registry.rs:340`):
            // a skill's own `*.md` files are its resources, not commands.
            if depth == 0 && path.extension().and_then(|e| e.to_str()) == Some("md") {
                // The name the registry gives it (`registry.rs:342` parses
                // flat command files the same way). If it does not parse,
                // fall back to the stem; the registry's diagnostic for this
                // path then says why it is not in effect.
                let (name, parses) = match crate::skills::skill::parse_skill_file(&path, None) {
                    Ok(parsed) => (parsed.skill.name, true),
                    Err(_) => {
                        let stem = path.file_stem().unwrap_or_default();
                        (stem.to_string_lossy().into_owned(), false)
                    }
                };
                let (status, detail) = tree.judge(registry, &name, &path, parses);
                out.push(AssetReport {
                    kind,
                    name,
                    path,
                    scope,
                    status,
                    detail,
                });
            }
        } else if is_dir {
            // The registry skips dependency/cache directories
            // (`registry.rs:13`) — the report must skip them too, or it
            // would report thousands of vendored files as skills.
            if is_ignored_skill_directory(&path) {
                continue;
            }
            // Not `is_file()`: a `SKILL.md` we may not stat would look like no
            // `SKILL.md` at all, the directory would be walked as a grouping
            // directory, and the report would blame the depth limit for what
            // is really an unreadable file.
            let skill_md = path.join("SKILL.md");
            // "No `SKILL.md`" and "a `SKILL.md` that is not a regular file"
            // are different: only the first is a grouping directory. A
            // `SKILL.md` that is itself a directory would otherwise be walked
            // as a group, find nothing, and the asset would vanish from the
            // report without a word.
            match probe_dir(&skill_md) {
                Ok(false) => {}
                Ok(true) => {
                    // **The report's own finding** — the loader never
                    // treated this as a skill at all.
                    out.push(AssetReport {
                        kind,
                        name: last_component(&path),
                        path: skill_md,
                        scope,
                        status: AssetStatus::Blocked,
                        detail: text::report_entry_unusable("SKILL.md is a directory, not a file"),
                    });
                    continue;
                }
                Err(reason) => {
                    out.push(blocked_entry(&path, reason));
                    continue;
                }
            }
            let is_skill = match probe_file(&skill_md) {
                Ok(is_skill) => is_skill,
                Err(reason) => {
                    out.push(blocked_entry(&path, reason));
                    continue;
                }
            };
            if !is_skill && entry_exists(&skill_md) {
                // The `SKILL.md` ENTRY is there, but it is not a regular file:
                // a dangling symlink (the target is gone), a FIFO, a socket.
                // `metadata` FOLLOWS the link, so a link whose target is gone
                // reads as "not there" — and the whole skill would vanish
                // from the report, walked as a grouping directory that finds
                // nothing.
                //
                // Computed BEFORE `skill_md` moves into the row: the link has
                // to be asked about while the path is still ours.
                let detail = if is_broken_link(&skill_md) {
                    text::broken_link()
                } else {
                    text::report_entry_unusable("SKILL.md is not a regular file")
                };
                out.push(AssetReport {
                    kind,
                    name: last_component(&path),
                    path: skill_md,
                    scope,
                    status: AssetStatus::Blocked,
                    detail,
                });
                continue;
            }
            if is_skill {
                // The name the registry gives it (`registry.rs:354` parses
                // skill directories the same way, with no namespace for a
                // standard directory). If it does not parse, fall back to
                // the directory name; the registry's diagnostic for this
                // path then says why it is not in effect.
                let (name, parses) =
                    match crate::skills::skill::parse_skill_dir(&path, &skill_md, None) {
                        Ok(parsed) => (parsed.skill.name, true),
                        Err(_) => (last_component(&path), false),
                    };
                let (status, detail) = tree.judge(registry, &name, &path, parses);
                out.push(AssetReport {
                    kind,
                    name,
                    path,
                    scope,
                    status,
                    detail,
                });
            } else {
                // A grouping directory: keep walking.
                collect_skill_rows(
                    kind,
                    scope,
                    &path,
                    depth + 1,
                    max_depth,
                    ancestors,
                    registry,
                    tree,
                    out,
                );
            }
        }
    }
    ancestors.remove(&identity);
}

/// The dependency/cache directories the registry skips.
///
/// The same list as `registry.rs:13` — the report must not walk into
/// `node_modules` and report thousands of vendored files as skills.
fn is_ignored_skill_directory(path: &Path) -> bool {
    const IGNORED: &[&str] = &[
        ".venv",
        "venv",
        "node_modules",
        ".git",
        ".hg",
        ".svn",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        ".tox",
        ".nox",
    ];
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    IGNORED
        .iter()
        .any(|ignored| name.eq_ignore_ascii_case(ignored))
}

/// What the real skill registry makes of the skills we found.
///
/// The registry is loaded ONCE per scan, from the same directory list the
/// runtime uses (the host's list, or `standard_skill_dirs` when it gave
/// none), so it sees the same collisions the runtime will see.
///
/// **Every path here is an [`identity`]**, not the string the registry
/// stored: the registry keeps paths literally (`skill.rs:759`,
/// `registry.rs:225`), and the same file can be spelled two ways (`/var` and
/// `/private/var` on macOS). Comparisons use identities; the report still
/// shows the path WE scanned, never one of these.
///
/// **Public because it appears in a public trait's method.** Every source
/// gets the ONE registry load `scan()` did — two sources in one report must
/// not see two different moments of the same disk.
pub struct SkillTruth {
    /// Every entry the registry's walk found.
    ///
    /// **The walk's own record.** The report asks the registry what it saw
    /// rather than walking the disk a second time: two walks of the same
    /// disk can disagree (a symlink, an ignored directory, a depth limit),
    /// and a report that re-derives the answer is answering a different
    /// question than the runtime will.
    discovered: Vec<DiscoveredAsset>,
    /// Every asset the walk found but could NOT load.
    ///
    /// **Separate from `discovered`**: a failed asset has no registration,
    /// so it has no name in any index and no "was it displaced" question —
    /// but it IS an asset the walk found, and the report has to say so.
    failed: Vec<FailedTruthAsset>,
    /// The skills map (`registry.all()`): the name each skill was registered
    /// under, and the identity of the `SKILL.md` it came from.
    skills: Vec<(String, PathBuf)>,
    /// The flat command templates (`registry.slash_commands()`), the same
    /// way. A **separate index**, as in the registry (`registry.rs:56`): a
    /// skill and a command with the same name do not collide there, so they
    /// must not collide here either.
    commands: Vec<(String, PathBuf)>,
}

/// One asset the registry's walk found.
///
/// **The ASSET identity is the key.** A skill directory is its BUNDLE
/// directory, not its `SKILL.md`: two bundles may ship the same `SKILL.md`
/// (a symlink to one file), and they are two assets that happen to share a
/// body. Matching the file alone would report both as one.
struct DiscoveredAsset {
    /// The ASSET's identity — the bundle directory for a skill directory, the
    /// file itself for a flat template.
    asset: PathBuf,
    /// The ASSET's path as the REGISTRY spelled it. The report shows the
    /// path the CALLER named — the two can differ (the same directory under
    /// two names), and only the registry knows where the asset really is.
    asset_path: PathBuf,
    /// The name it was registered under.
    name: String,
    /// The FORM it was found in.
    entry: EntryKind,
    /// The scanned root that found it — the root EXACTLY as handed to the
    /// registry.
    root: PathBuf,
    /// The same root as the registry spelled it.
    root_path: PathBuf,
    /// Whether a later same-name entry displaced it.
    replaced: bool,
    /// The diagnostics the registry recorded for THIS asset: its reason and
    /// whether it is a warning.
    ///
    /// **On the asset, not on the file.** A flat candidate and a skill
    /// directory can sit at the same path; a diagnostic about one is not
    /// about the other.
    diagnostics: Vec<(String, bool)>,
}

/// One asset the registry's walk found but could NOT load.
///
/// **The ASSET's identity, not the file's** — a skill directory that fails
/// to parse is still the asset that directory is.
struct FailedTruthAsset {
    /// The ASSET's identity.
    asset: PathBuf,
    /// The ASSET's path as the registry spelled it.
    asset_path: PathBuf,
    /// The ENTRY's path as the registry spelled it — the `SKILL.md` for a
    /// skill directory, the file itself for a flat template.
    ///
    /// **The entry is where the problem is** when the entry itself is
    /// unusable (a dangling link, a FIFO): the row shows it, because that
    /// is the thing the person has to look at.
    entry_path: PathBuf,
    /// The name it would have been registered under.
    name: String,
    /// The FORM it was found in.
    entry: EntryKind,
    /// The scanned root that found it.
    root: PathBuf,
    /// The same root as the registry spelled it.
    root_path: PathBuf,
    /// Why it could not be loaded — and WHOSE words the reason is.
    failure: crate::skills::LoadFailure,
    /// The reason, in the registry's own words.
    reason: String,
}

/// The path's identity — the single source of truth for "is this the same file".
///
/// `pathnorm::canonicalize` is the crate's one definition of path identity
/// (`pathnorm.rs:74`): on macOS `/var` and `/private/var` are the same
/// directory, and a caller may hand us either spelling. Comparing the
/// strings would say "different files"; the disk says otherwise.
///
/// A path that cannot be resolved falls back to itself: two identical
/// strings are the same file.
fn identity(path: &Path) -> PathBuf {
    crate::pathnorm::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether two paths name the same file.
///
/// **The disk is the only authority on this.** The same file can be spelled
/// two ways — `/var/folders/...` and `/private/var/folders/...` are the same
/// directory on macOS — and two *different* paths can be the same file too
/// (a hard link). Comparing the strings settles neither.
///
/// Three tiers, cheapest first:
/// 1. the literal paths — no syscall, and it settles the common case where
///    the caller handed us the same path twice;
/// 2. the [identities](identity) — the same path spelled two ways;
/// 3. the file itself — two different paths that are one inode.
///
/// A path we cannot resolve stays unproven: `None` is not "different", but
/// it is not "the same" either, and the caller must not treat it as proof.
fn same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    if identity(left) == identity(right) {
        return true;
    }
    match (file_id(left), file_id(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// The file's identity on disk: its device and inode.
///
/// `None` when the platform has no such notion, or the path cannot be
/// stat'd — a path we could not read is not proven to be anything.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).ok()?;
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

impl SkillTruth {
    /// Load the registry the way the runtime does.
    fn load(ctx: &ScanContext) -> Self {
        // **The host's list**, not `standard_skill_dirs` — the runtime may
        // add the user's configured `extra_skill_dirs`, and the report has
        // to see the SAME directories the runtime sees, or it picks a
        // different winner than the runtime does.
        //
        // **The order is the runtime's order.** `resolve_skill_dirs_at`
        // (`coding/src/parts.rs`) puts the extras FIRST and the standard
        // roots after them; the registry loads a list front to back and the
        // LAST one wins a same-name collision — so the standard roots
        // outrank the extras. Passing the host's list verbatim keeps that
        // order; recomputing the standard list here would drop the extras
        // and change which skill wins.
        //
        // An empty list means "the standard list" — the host passes the real
        // list in production, and the fixtures use the default.
        //
        // **`None` and `Some(vec![])` are different.** `None` = the host gave
        // no list → the standard list. `Some(vec![])` = the host EXPLICITLY
        // asked for no skill roots (the runtime's skill tools are off) →
        // scan nothing. Falling back to the standard list there would make
        // the report claim `~/.claude/skills/foo` is `Active` while nothing
        // is loading it.
        let dirs = match ctx.skill_dirs.clone() {
            Some(dirs) => dirs,
            None => crate::skills::standard_skill_dirs(crate::skills::SkillRoots {
                home: &ctx.os_home,
                project: &ctx.project,
                dirs: &ctx.product,
            }),
        };
        let registry = crate::skills::SkillRegistry::load_with_explicit(&dirs, &[]);
        // **Both** maps, kept APART: `all()` is the SKILLS map, and the flat
        // command templates live in a separate one (`slash_commands()`,
        // `registry.rs:467`). Looking only at `all()` would make every
        // command file look overridden; merging them would make a skill
        // look overridden by a command of the same name — a collision the
        // registry never resolved, because the two never compete.
        let index = |templates: &[std::sync::Arc<crate::skills::Skill>]| -> Vec<(String, PathBuf)> {
            templates
                .iter()
                .map(|skill| (skill.name.clone(), identity(&skill.source_path)))
                .collect()
        };
        let skills = index(&registry.all());
        let commands = index(&registry.slash_commands());
        // **The walk's own record.** The registry says what it found: its
        // ASSET identity, under which name, in which FORM, under which root,
        // and which of those a later same-name entry displaced. The report
        // reads this instead of walking the disk again.
        let discovered = registry
            .discovered()
            .into_iter()
            .map(|entry| DiscoveredAsset {
                asset: identity(&entry.asset),
                asset_path: entry.asset,
                name: entry.name,
                entry: entry.entry,
                root: identity(&entry.root),
                root_path: entry.root,
                replaced: entry.replaced,
                diagnostics: entry.diagnostics,
            })
            .collect();
        // **The assets the walk found but could not load.** Their reason, and
        // WHOSE words it is: the loader's (a rejection) or a fact about the
        // ENTRY (a dangling link, an entry that is not a regular file).
        let failed = registry
            .failed()
            .into_iter()
            .map(|asset| FailedTruthAsset {
                asset: identity(&asset.asset),
                asset_path: asset.asset,
                entry_path: asset.path,
                name: asset.name,
                entry: asset.entry,
                root: identity(&asset.root),
                root_path: asset.root,
                failure: asset.failure,
                reason: asset.reason,
            })
            .collect();
        Self {
            discovered,
            failed,
            skills,
            commands,
        }
    }

    /// What the registry says about this asset, if it says anything.
    ///
    /// `Some` means the registry has a FACT about this path — it found it (or
    /// displaced it). `None` means the registry never walked it, so it has
    /// nothing to say.
    ///
    /// Matching is by **ASSET identity** (the bundle directory for a skill
    /// directory, the file itself for a flat template) **and by name** (the
    /// same asset registered under another name is not this asset) **and by
    /// FORM** (a flat `*.md` candidate is not the skill directory that
    /// happens to sit at the same path — both go into the skill index, so the
    /// index cannot tell them apart).
    fn registry_says(
        &self,
        name: &str,
        path: &Path,
        is_skill_dir: bool,
    ) -> Option<(AssetStatus, String)> {
        // The ASSET a candidate stands for: the bundle directory for a skill
        // directory, the file itself for a flat template. Two bundles may
        // ship the same `SKILL.md`; they are two assets that share a body,
        // and matching the file would report both as one.
        let asset = identity(path);
        let entry = if is_skill_dir {
            EntryKind::SkillDir
        } else {
            EntryKind::FlatFile
        };
        // Everything the registry found AS this asset, in this form, under
        // any name.
        let at_this_asset: Vec<&DiscoveredAsset> = self
            .discovered
            .iter()
            .filter(|found| found.asset == asset && found.entry == entry)
            .collect();
        if at_this_asset.is_empty() {
            // The registry never walked this asset. It has no fact about it,
            // and a second traversal is a different question.
            return None;
        }
        // The entries registered under OUR name.
        let ours: Vec<&DiscoveredAsset> = at_this_asset
            .into_iter()
            .filter(|found| found.name == name)
            .collect();
        if ours.is_empty() {
            // The registry found this asset, but under ANOTHER name: it is in
            // effect, but it is not in effect AS OUR ASSET. Saying "active"
            // here would be a claim about someone else's row.
            return None;
        }
        // One of our entries survived → it is the one in effect.
        if ours.iter().any(|found| !found.replaced) {
            return Some((AssetStatus::Active, text::skill_active()));
        }
        // All of them were displaced by a later same-name entry.
        Some((AssetStatus::Blocked, text::skill_overridden(name)))
    }

    /// The failure the registry recorded for this asset, if it recorded one.
    ///
    /// **By ASSET identity** — a flat candidate and a skill directory can
    /// sit at the same path, and a failure about one is not about the other.
    fn registry_failure(&self, path: &Path, is_skill_dir: bool) -> Option<(String, bool)> {
        let asset = identity(path);
        let entry = if is_skill_dir {
            EntryKind::SkillDir
        } else {
            EntryKind::FlatFile
        };
        self.failed
            .iter()
            .find(|failed| failed.asset == asset && failed.entry == entry)
            .map(|failed| {
                (
                    failed.reason.clone(),
                    matches!(failed.failure, crate::skills::LoadFailure::Rejected),
                )
            })
    }

    /// Whether this file is the one that is actually in effect.
    ///
    /// `name` is the name the registry gives this skill (see
    /// [`skill_rows`]). Paths are compared by [`identity`]. **The order
    /// matters**: what the registry SAID about our asset comes before what we
    /// infer from names, and every answer but the last rests on something the
    /// registry actually did:
    ///
    /// 1. The registry's own record of this asset says it is the one in
    ///    effect, or that a later same-name entry displaced it. **A fact, not
    ///    an inference** — the registry walked these paths; we did not walk
    ///    them a second time to find out. This covers the case the registry
    ///    records no diagnostic for: two IDENTICAL definitions, where it
    ///    keeps the later one and says nothing, because nothing is wrong.
    /// 2. The registry rejected it → the loader's own words, with the
    ///    loader's own reason. A definite cause; a same-name winner
    ///    elsewhere is only an inference and must not hide it — the fix is
    ///    the file, not the priority order.
    /// 3. Nothing says why → say exactly that. Claiming "overridden" here
    ///    would be a guess: the registry never found this asset, so it never
    ///    weighed it against a winner.
    fn verdict(&self, name: &str, path: &Path, parses: bool) -> (AssetStatus, String) {
        // If we cannot tell whether the path is a directory, treat it as the
        // file itself: the lookups below then simply find nothing for it, and
        // the verdict falls through to "not in effect" rather than inventing a
        // reason.
        let is_skill_dir = probe_dir(path).unwrap_or(false);
        // 1. What the registry ACTUALLY found. It walked these paths; we did
        //    not walk them a second time to find out.
        if let Some(verdict) = self.registry_says(name, path, is_skill_dir) {
            return verdict;
        }
        // 2. The registry said the loader could not use it — with the
        //    loader's own reason. Believed only when the file really does
        //    not parse: for a file that parses, a warning is a non-fatal
        //    one, not a rejection.
        if !parses {
            if let Some((reason, rejected)) = self.registry_failure(path, is_skill_dir) {
                if rejected {
                    return (AssetStatus::Blocked, text::skill_rejected(&reason));
                }
                // Not the loader's story: a fact about the ENTRY (a dangling
                // link, an entry that is not a regular file). The loader
                // never treated it as a skill at all.
                return (AssetStatus::Blocked, text::report_entry_unusable(&reason));
            }
        }
        // 3. Nothing says why → say exactly that. The registry never found
        //    this asset, so it never weighed it against a winner: claiming
        //    "overridden" here would be a guess.
        (AssetStatus::Blocked, text::skill_not_in_effect())
    }
}

/// Whether a protocol id names a wire format we do not speak.
///
/// `provider::unsupported_protocol` builds `unsupported:<wire_api>` for one, and
/// such an account must never be imported: `ProviderBook::create_account`
/// resolves an unknown preset to `openai-compatible`
/// (`config/provider_preset.rs:381`), so the import would silently succeed
/// with the WRONG wire protocol.
pub fn is_unsupported_protocol(protocol: &str) -> bool {
    protocol.starts_with("unsupported:")
}

/// Whether this path is a file we can use — and if not, WHY.
///
/// `Path::is_file()` collapses every error into `false`: a path we cannot
/// stat (a parent directory we may not traverse) looks exactly like a path
/// that is not there. The report would then say "not found" about something
/// that IS there but unreadable — and the person never learns to look.
///
/// `Ok(true)` = a regular file. `Ok(false)` = not there (or not a regular
/// file). `Err(reason)` = we could not tell.
fn probe_file(path: &Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// Whether this path's ENTRY is there, whatever it is.
///
/// **`std::fs::metadata` follows the link**, so a dangling symlink reads as
/// "not there" — and the asset would vanish from the report, and no asset
/// may vanish. `symlink_metadata` does NOT follow it, so the entry itself is
/// still visible.
///
/// The mirror image of [`probe_file`]: that one says whether the path is
/// USABLE, this one says whether there is anything there at all.
fn entry_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Whether this path is a DANGLING symlink — the entry is there, but
/// following it does not reach anything.
///
/// A symlink whose target IS there is not broken — it resolves to a regular
/// file or a directory, and the probes already say so.
fn is_broken_link(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_symlink() {
        return false;
    }
    // The link is there; is the target? `NotFound` is the dangling case —
    // any other error (a permission we may not read through) is not.
    matches!(
        std::fs::metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Whether this path is a directory we can use — and if not, WHY.
///
/// The directory counterpart of [`probe_file`].
fn probe_dir(path: &Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// What the real instruction loader makes of ONE instruction file.
///
/// The loader is the only thing that knows the order, what "shadowed" means,
/// and that an empty file is `Empty` rather than `Loaded` (`instructions.rs:173`).
/// So the report asks it, rather than re-deriving those rules — a
/// hand-rolled check is exactly how a report ends up saying "already in
/// effect" about a file that is not.
///
/// `None` when the loader does not know this file at all — it is not an
/// instruction file, or it is not in the project tier.
fn instruction_verdict(ctx: &ScanContext, path: &Path) -> Option<InstructionVerdict> {
    // The loader wants the product tree as the "home" (`instructions.rs:105`).
    let loaded = crate::instructions::load_instructions(ctx.product.user(), &ctx.project);
    // Project tier only (`instructions.rs:137`). The global file
    // (`<product tree>/ATOMCODE.md`, scope `"GLOBAL"`) would otherwise match
    // too and come back labelled `AssetScope::Project`.
    let source = loaded
        .sources
        .iter()
        .find(|source| source.scope == "PROJECT" && source.path == path)?;
    // The file that WINS in the project tier — the one the loader did not
    // shadow. A shadowed file's own name is NOT the explanation ("CLAUDE.md
    // takes precedence" about a shadowed CLAUDE.md says nothing); the
    // winner's name is.
    //
    // The loader marks the FIRST existing project file as the winner and
    // shadows the rest (`instructions.rs:131-139`: `selected |= path.is_file()`),
    // so the winner is the first PROJECT source that is neither shadowed nor
    // missing — even if it is empty, because an empty file still wins the
    // slot.
    let winner = loaded.sources.iter().find(|candidate| {
        candidate.scope == "PROJECT"
            && candidate.state != crate::instructions::InstructionState::Shadowed
            && candidate.state != crate::instructions::InstructionState::Missing
    });
    Some(InstructionVerdict {
        name: last_component(&source.path),
        path: source.path.clone(),
        scope: AssetScope::Project,
        state: source.state,
        shadowed_by: winner.map(|winner| last_component(&winner.path)),
    })
}

/// One instruction file, and what the loader said about it.
struct InstructionVerdict {
    name: String,
    path: PathBuf,
    scope: AssetScope,
    state: crate::instructions::InstructionState,
    /// The project file that wins over this one, when this one is shadowed.
    shadowed_by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_with_no_assets_is_empty_not_an_error() {
        let report = MigrationReport::default();

        assert!(report.sources.is_empty());
        assert_eq!(report.total_assets(), 0);
    }

    #[test]
    fn the_report_counts_assets_across_sources() {
        let report = MigrationReport {
            sources: vec![
                SourceReport {
                    source_id: "codex",
                    display_name: "Codex",
                    assets: vec![],
                },
                SourceReport {
                    source_id: "claude-code",
                    display_name: "Claude Code",
                    assets: vec![AssetReport {
                        kind: AssetKind::Skills,
                        name: "web-access".into(),
                        path: "/home/u/.claude/skills/web-access".into(),
                        scope: AssetScope::User,
                        status: AssetStatus::Active,
                        detail: "directly read".into(),
                    }],
                },
            ],
        };

        assert_eq!(report.total_assets(), 1);
        assert_eq!(report.assets_with_status(AssetStatus::Active), 1);
        assert_eq!(report.assets_with_status(AssetStatus::Blocked), 0);
    }

    #[test]
    fn scanning_with_no_sources_produces_an_empty_report() {
        let ctx = ScanContext {
            os_home: PathBuf::from("/home/u"),
            project: PathBuf::from("/work"),
            product: crate::ProductDirs::new("/home/u/.ours", ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let report = scan(&[], &ctx);

        assert!(report.sources.is_empty());
        assert_eq!(report.total_assets(), 0);
    }
}
