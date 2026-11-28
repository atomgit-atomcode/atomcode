//! Discover + index skills from a set of directories. Neutral: the caller supplies the
//! directories (the standard `~/.claude/skills` etc. precedence is a driver concern —
//! see [`standard_skill_dirs`], which takes our own dirs from the caller too). Ported from production `skill.rs` `SkillRegistry`.

use super::skill::{parse_skill_dir, parse_skill_file, Parsed, Skill};
use crate::ProductDirs;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Directories installed alongside skills by runtimes and development tools.
/// This applies to discovered children, never an explicitly configured root.
fn ignored_discovery_directory(path: &Path) -> bool {
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
        // Claude's skill manager moves deleted skills here; they are not installed.
        ".trash",
    ];
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            IGNORED
                .iter()
                .any(|ignored| name.eq_ignore_ascii_case(ignored))
        })
}

/// The failure an entry has, if it is there but cannot be used.
///
/// **The entry itself, not what it points at.** `std::fs::metadata` FOLLOWS
/// a symlink, so a link whose target is gone reads as "not there" — and the
/// asset would vanish with nothing to say about it. `symlink_metadata` does
/// not follow it, so the entry is still visible.
///
/// `None` when the entry is fine (or is a regular file / a directory — the
/// caller's own probes say that).
fn entry_failure(path: &Path) -> Option<LoadFailure> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        // The link is there; is the target? `NotFound` is the dangling case —
        // any other error (a permission we may not read through) is not.
        if matches!(
            std::fs::metadata(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ) {
            return Some(LoadFailure::BrokenLink);
        }
        return None;
    }
    if metadata.is_file() || metadata.is_dir() {
        return None;
    }
    // A FIFO, a socket, a device — not something the loader can read.
    Some(LoadFailure::NotARegularFile)
}

/// The reason an entry could not be used, in plain words.
///
/// **A fact about the ENTRY**, not the loader's verdict — the loader never
/// treated this entry as a skill at all.
fn failure_reason(failure: LoadFailure) -> String {
    match failure {
        LoadFailure::Rejected => "the loader refused it".into(),
        LoadFailure::BrokenLink => "its symlink target is gone".into(),
        LoadFailure::NotARegularFile => "it is not a regular file".into(),
    }
}

/// The name an asset would have been registered under.
///
/// The registry's own naming rule: a flat template is named after its file
/// STEM, a skill directory after its directory name (`skill.rs:615`,
/// `skill.rs:655`).
fn asset_name(asset: &Path, entry: EntryKind) -> String {
    match entry {
        EntryKind::FlatFile => asset
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default(),
        EntryKind::SkillDir => asset
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// The caller identifies the role of a directory, including custom plugin paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateKind {
    Skill,
    Command,
}

pub type TemplateDirectory = (PathBuf, String, TemplateKind);

/// A load failure or an intentionally overridden template. No file contents are retained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub reason: String,
    /// Overrides are informational; failures are surfaced as warnings.
    pub warning: bool,
}

/// How the walk found an entry — the FORM it was found in.
///
/// **Not the index.** A skill directory (`<dir>/SKILL.md`) and a flat `*.md`
/// file can BOTH go into the skill index (`registry.rs:341` and
/// `registry.rs:353`), so the index cannot tell them apart. A caller that
/// matched a flat-file candidate against a directory skill's record would be
/// borrowing a fact about a different asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A skill directory: found at its `SKILL.md`.
    SkillDir,
    /// A flat template file: found at the file itself.
    FlatFile,
}

/// One entry the registry's walk found.
///
/// **The walk's own record.** A caller that needs to know what the registry
/// actually saw — not what a second traversal would find — reads this. Two
/// traversals of the same disk can disagree (a symlink, an ignored
/// directory, a depth limit), so a caller that re-derives the answer is
/// answering a different question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredTemplate {
    /// The path the walk found it at: the `SKILL.md` for a skill directory,
    /// the file itself for a flat command template.
    pub path: PathBuf,
    /// The ASSET this entry is — the thing a person would call "this skill".
    ///
    /// **A skill directory is its BUNDLE directory, not its `SKILL.md`.** Two
    /// bundles may ship the same `SKILL.md` (a symlink to one file); they are
    /// two assets that happen to share a body. Matching the file alone would
    /// report both as one.
    ///
    /// For a flat template file this is the file itself.
    pub asset: PathBuf,
    /// The name it was registered under.
    pub name: String,
    /// Which index it went into.
    pub kind: TemplateKind,
    /// The FORM it was found in — a skill directory or a flat file. The
    /// index cannot say this: both forms go into the skill index.
    pub entry: EntryKind,
    /// The scanned root this entry was found under — the root EXACTLY as
    /// handed to the registry.
    ///
    /// A caller enumerates a tree by its root; this says which root found
    /// the entry, so "the assets under THIS root" is a question the registry
    /// answers.
    pub root: PathBuf,
    /// Whether a later same-name entry displaced it.
    ///
    /// **A same-name entry displaces the earlier one even when the two
    /// definitions are identical** — the registry keeps the later one and
    /// says nothing, because nothing is wrong. A caller reporting "which one
    /// is in effect" needs to know the earlier one lost, and only the
    /// registry knows.
    pub replaced: bool,
    /// The diagnostics the registry recorded for THIS asset: its reason and
    /// whether it is a warning.
    ///
    /// **On the asset, not on the file.** A flat candidate and a skill
    /// directory can sit at the same path; a diagnostic about one is not
    /// about the other.
    pub diagnostics: Vec<(String, bool)>,
}

/// Why an asset the walk found could not be loaded.
///
/// **Whose words the reason is.** A `Rejected` failure is the LOADER's own
/// words: it read the file and refused it. The other two are facts about the
/// ENTRY — the loader never treated it as a skill at all, so putting its
/// words in its mouth would send the person to fix the wrong thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadFailure {
    /// The loader read it and refused it.
    Rejected,
    /// The entry is a symlink whose target is gone.
    BrokenLink,
    /// The entry is there, but it is not a regular file (a FIFO, a socket).
    NotARegularFile,
}

/// An asset the walk found but could NOT load.
///
/// **The asset's identity, not the file's.** A skill directory that fails to
/// parse is still the asset that directory is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedAsset {
    /// The path the walk found it at: the `SKILL.md` for a skill directory,
    /// the file itself for a flat template.
    pub path: PathBuf,
    /// The ASSET this is — the bundle directory for a skill directory, the
    /// file itself for a flat template.
    pub asset: PathBuf,
    /// The name it would have been registered under.
    pub name: String,
    /// The FORM it was found in.
    pub entry: EntryKind,
    /// The scanned root it was found under — the root EXACTLY as handed to
    /// the registry.
    pub root: PathBuf,
    /// Why it could not be loaded.
    pub failure: LoadFailure,
    /// The reason, in the registry's own words. Meaningful for
    /// [`LoadFailure::Rejected`].
    pub reason: String,
}

#[derive(Default)]
struct SkillIndex {
    directories: Vec<PathBuf>,
    diagnostics: Vec<SkillDiagnostic>,
    /// Every entry the walk found, in the order it found them.
    discovered: Vec<DiscoveredTemplate>,
    /// Every asset the walk found but could NOT load — a parse failure, a
    /// dangling symlink, an entry that is not a regular file.
    ///
    /// **Separate from `discovered`**, because a failed asset has no
    /// registration: it has no name in any index, and no "was it
    /// displaced" question. It is still an ASSET the walk found, so the
    /// report must be able to say so.
    failed: Vec<FailedAsset>,
    /// Which discovery record is the CURRENT winner of each skill name.
    ///
    /// **The slot, not the path.** The same file can be found more than once
    /// (the same root listed twice, one directory reached through two
    /// roots), and the same path can sit in both indexes — so looking a
    /// record up by path finds the wrong one. The slot says which record
    /// actually holds the name.
    skill_records: BTreeMap<String, usize>,
    /// The same, for the command index. A **separate** map: the two indexes
    /// never compete, so one index's loss says nothing about the other's.
    command_records: BTreeMap<String, usize>,
    skills: BTreeMap<String, Arc<Skill>>,
    commands: BTreeMap<String, Arc<Skill>>,
}

impl std::ops::Deref for SkillIndex {
    type Target = BTreeMap<String, Arc<Skill>>;
    fn deref(&self) -> &Self::Target {
        &self.skills
    }
}

impl std::ops::DerefMut for SkillIndex {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.skills
    }
}

/// Skills indexed by name. `BTreeMap` for deterministic (sorted) order — the skill list
/// is injected into the system prompt, so a stable order keeps the prompt prefix
/// byte-identical (prompt-prefix caching), same rationale as the kernel ToolRegistry.
pub struct SkillRegistry {
    /// Behind a lock because skills live on disk and the disk changes under a
    /// running session: a person writes a skill, or installs a plugin, and then
    /// reloads. Every holder of this registry has an `Arc` to it — the
    /// `use_skill` and `list_skills` tools, the `skills` seam, the catalog
    /// fragment — so re-reading in place is what lets a reload reach all of
    /// them without rebuilding the tree they are mounted in (`docs/adr/0022`
    /// §2).
    skills: RwLock<SkillIndex>,
    /// Our own trees: a skill under one of them ranks first in the catalog
    /// ([`super::render::source_rank`]). Handed in by whoever knows where they
    /// are — [`Self::reload`] takes them from its [`SkillRoots`].
    native_roots: RwLock<Vec<PathBuf>>,
    notified: RwLock<BTreeSet<String>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(SkillIndex::default()),
            native_roots: RwLock::new(Vec::new()),
            notified: RwLock::new(BTreeSet::new()),
        }
    }

    /// Name our own trees, so skills under them rank first in the catalog.
    /// [`standard_skill_dirs`] callers pass [`SkillRoots::native`].
    pub fn set_native_roots(&self, roots: Vec<PathBuf>) {
        *self.native_roots.write().unwrap_or_else(|e| e.into_inner()) = roots;
    }

    /// The map, for a reader. A poisoned lock is taken over rather than
    /// propagated: a panic in a catalog render must not take every later skill
    /// lookup down with it.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, SkillIndex> {
        self.skills.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, SkillIndex> {
        self.skills.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Read `dirs` again, in place, replacing what is indexed now.
    ///
    /// The swap is one write: a reader either sees every skill from before or
    /// every skill from after, never a half-scanned catalog — which is what a
    /// model would otherwise be told about mid-reload.
    pub fn reload_dirs(&self, dirs: &[PathBuf], namespaced: &[TemplateDirectory]) {
        self.reload_dirs_with_explicit(dirs, namespaced, &[]);
    }

    pub fn reload_dirs_with_explicit(
        &self,
        dirs: &[PathBuf],
        namespaced: &[TemplateDirectory],
        explicit: &[PathBuf],
    ) {
        let fresh = Self::load_with_explicit(dirs, explicit);
        for (dir, namespace, kind) in namespaced {
            fresh.load_template_dir(dir, Some(namespace), *kind);
        }
        let next = std::mem::take(&mut *fresh.write());
        // Forget resolved failures, so a later recurrence is reported again.
        let current: BTreeSet<_> = next
            .diagnostics
            .iter()
            .filter(|d| d.warning)
            .map(|d| format!("[skills] {}: {}", d.path.display(), d.reason))
            .collect();
        self.notified
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|w| current.contains(w));
        *self.write() = next;
    }

    /// Load from `dirs` in LOW→HIGH priority order within each template kind.
    /// Skill sources support both single files and directories. Standard
    /// command roots are loaded into the separate command index.
    /// Each dir is scanned for flat `*.md` files AND `*/SKILL.md` subdirectories; parse
    /// failures are skipped.
    pub fn load(dirs: &[PathBuf]) -> Self {
        Self::load_with_explicit(dirs, &[])
    }

    /// Explicitly configured roots must report missing paths; implicit compatibility
    /// roots may legitimately not exist.
    pub fn load_with_explicit(dirs: &[PathBuf], explicit: &[PathBuf]) -> Self {
        let reg = Self::new();
        reg.write().directories = dirs.to_vec();
        for dir in dirs {
            if explicit.contains(dir) && matches!(dir.try_exists(), Ok(false)) {
                reg.diagnose(
                    dir,
                    "configured skill directory does not exist".into(),
                    true,
                );
            } else {
                reg.load_standard_dir(dir, None);
            }
        }
        reg
    }

    pub fn directories(&self) -> Vec<PathBuf> {
        self.read().directories.clone()
    }

    pub fn diagnostics(&self) -> Vec<SkillDiagnostic> {
        self.read().diagnostics.clone()
    }

    /// Every entry the walk found, in the order it found them.
    ///
    /// **The walk's own record.** A caller that needs to know what the registry
    /// actually saw reads this, rather than walking the disk a second time and
    /// hoping the two walks agree.
    pub fn discovered(&self) -> Vec<DiscoveredTemplate> {
        self.read().discovered.clone()
    }

    /// Every asset the walk found but could NOT load.
    ///
    /// **The walk's own record.** A file that failed to parse, a dangling
    /// symlink, an entry that is not a regular file — each is an asset the
    /// walk found and could not use, and a caller reporting on those trees
    /// needs to know about it.
    pub fn failed(&self) -> Vec<FailedAsset> {
        self.read().failed.clone()
    }

    pub fn take_new_warnings(&self) -> Vec<String> {
        let warnings: Vec<_> = self
            .read()
            .diagnostics
            .iter()
            .filter(|d| d.warning)
            .map(|d| format!("[skills] {}: {}", d.path.display(), d.reason))
            .collect();
        let mut notified = self.notified.write().unwrap_or_else(|e| e.into_inner());
        warnings
            .into_iter()
            .filter(|w| notified.insert(w.clone()))
            .collect()
    }

    fn diagnose(&self, path: &Path, reason: String, warning: bool) {
        self.write().diagnostics.push(SkillDiagnostic {
            path: path.to_path_buf(),
            reason,
            warning,
        });
    }

    /// Record one asset the walk found.
    ///
    /// `path` is where the walk found it (`<dir>/SKILL.md` for a skill
    /// directory, the file itself for a flat template); `asset` is what it
    /// IS — the bundle directory for a skill directory, the file itself for
    /// a flat template. `root` is the scanned root it was found under, the
    /// root EXACTLY as handed to the registry.
    fn insert_parsed(
        &self,
        parsed: Result<Parsed, String>,
        path: &Path,
        asset: &Path,
        root: &Path,
        kind: TemplateKind,
        entry: EntryKind,
    ) {
        match parsed {
            Ok(parsed) => {
                let mut index = self.write();
                let skill = parsed.skill;
                let name = skill.name.clone();
                let source = skill.source_path.clone();
                // A bad `args:` value is a warning, not a load failure: the
                // skill still loads with its default, so the typo is reported
                // without dropping the command from the menu.
                //
                // **The warning belongs to the ASSET**, so it is recorded
                // twice: once in the flat diagnostic list (the runtime's
                // view, unchanged) and once ON the discovery record (the
                // asset's own view — a flat candidate and a skill directory
                // can sit at the same path, and a diagnostic about one is
                // not about the other).
                let mut asset_diagnostics: Vec<(String, bool)> = Vec::new();
                for warning in parsed.warnings {
                    index.diagnostics.push(SkillDiagnostic {
                        path: path.to_path_buf(),
                        reason: warning.clone(),
                        warning: true,
                    });
                    asset_diagnostics.push((warning, true));
                }
                // Compare loaded definitions, not current disk contents: a file
                // may have changed since the earlier source was scanned.
                let same_definition = match kind {
                    TemplateKind::Skill => index.skills.get(&name),
                    TemplateKind::Command => index.commands.get(&name),
                }
                .is_some_and(|previous| {
                    previous.display_name == skill.display_name
                        && previous.description == skill.description
                        && previous.template == skill.template
                        && previous.allowed_tools == skill.allowed_tools
                        && previous.user_invocable == skill.user_invocable
                        && previous.args_requirement == skill.args_requirement
                });
                // The SLOT that holds this name now — and, before it, the one
                // that used to. The slot is what says which record actually
                // won: the same file can be found twice (the same root listed
                // twice, one directory reached through two roots), and the
                // same path can sit in BOTH indexes — so looking the loser up
                // by path would find the wrong record.
                //
                // Read BEFORE the slot is moved below.
                let displaced = match kind {
                    TemplateKind::Skill => index.skill_records.get(&name).copied(),
                    TemplateKind::Command => index.command_records.get(&name).copied(),
                };
                let previous = match kind {
                    TemplateKind::Skill => index.skills.insert(name.clone(), Arc::new(skill)),
                    TemplateKind::Command => index.commands.insert(name.clone(), Arc::new(skill)),
                };
                // The walk's own record of what it found. The report reads it
                // instead of walking the disk a second time.
                let record = index.discovered.len();
                index.discovered.push(DiscoveredTemplate {
                    path: path.to_path_buf(),
                    asset: asset.to_path_buf(),
                    name: name.clone(),
                    kind,
                    entry,
                    root: root.to_path_buf(),
                    replaced: false,
                    diagnostics: asset_diagnostics,
                });
                match kind {
                    TemplateKind::Skill => {
                        index.skill_records.insert(name.clone(), record);
                    }
                    TemplateKind::Command => {
                        index.command_records.insert(name.clone(), record);
                    }
                }
                // The slot MOVED, so the record that used to hold this name
                // lost it — even when the new record is the SAME file (the
                // same root listed twice) and even when the two definitions
                // are identical. Both are displacements: the earlier record
                // is not the one in effect any more.
                //
                // The registry keeps the later one and says nothing, because
                // nothing is wrong — but a caller reporting "which one is in
                // effect" needs to know the earlier one lost, and only the
                // registry knows.
                if let Some(previous) = previous {
                    // The OVERRIDE DIAGNOSTIC's condition is unchanged: an
                    // identical definition is not a problem, so it is not
                    // reported. The same path re-loaded is not an override
                    // either.
                    if previous.source_path != source && !same_definition {
                        let reason = format!("overridden by {}", source.display());
                        index.diagnostics.push(SkillDiagnostic {
                            path: previous.source_path.clone(),
                            reason: reason.clone(),
                            warning: false,
                        });
                        // The override belongs to the ASSET that LOST — the
                        // bundle directory, not the `SKILL.md` — so a flat
                        // candidate can never borrow it. It goes on the
                        // DISPLACED record, which is the one that lost.
                        if let Some(slot) = displaced {
                            if let Some(displaced_entry) = index.discovered.get_mut(slot) {
                                displaced_entry.diagnostics.push((reason, false));
                            }
                        }
                    }
                }
                if let Some(slot) = displaced {
                    if let Some(displaced_entry) = index.discovered.get_mut(slot) {
                        displaced_entry.replaced = true;
                    }
                }
            }
            Err(error) => {
                // A load FAILURE is still an asset the walk found: the file is
                // there, the walk reached it, and the loader refused it. The
                // report has to be able to say so — by ASSET identity, so a
                // flat candidate never borrows a directory skill's failure.
                //
                // The diagnostic itself is UNCHANGED — the runtime's view of
                // a failed load is exactly what it always was.
                self.diagnose(path, error.clone(), true);
                self.write().failed.push(FailedAsset {
                    path: path.to_path_buf(),
                    asset: asset.to_path_buf(),
                    name: asset_name(asset, entry),
                    entry,
                    root: root.to_path_buf(),
                    failure: LoadFailure::Rejected,
                    reason: error,
                });
            }
        }
    }

    /// Load one directory, optionally namespacing the skill names (`{ns}:{name}`).
    pub fn load_dir(&self, dir: &Path, namespace: Option<&str>) {
        self.load_template_dir(dir, namespace, TemplateKind::Skill);
    }

    fn load_standard_dir(&self, dir: &Path, namespace: Option<&str>) {
        let kind = if dir.file_name().is_some_and(|name| name == "commands") {
            TemplateKind::Command
        } else {
            TemplateKind::Skill
        };
        self.load_template_dir(dir, namespace, kind);
    }

    pub fn load_template_dir(&self, dir: &Path, namespace: Option<&str>, kind: TemplateKind) {
        self.scan_skill_dir(dir, dir, namespace, 0, kind, &mut BTreeSet::new());
    }

    /// Scan `dir` for skills, recursing into grouping subdirectories. A subdir holding a
    /// `SKILL.md` is a skill (NOT descended into — its own files are skill resources); a
    /// subdir without one is a grouping directory whose nested skills are discovered
    /// recursively, so `skills/GROUP/SUB/SKILL.md` is still found. Flat `*.md`
    /// slash-commands are TOP-LEVEL only (depth 0) — a skill's own `*.md` are resources,
    /// not separate commands. Dependency/cache trees are not groups. Ancestry
    /// detects real symlink cycles; depth independently bounds grouping traversal.
    ///
    /// `root` is the scanned root this walk started from — the root EXACTLY
    /// as handed to the registry. It is carried down through the recursion
    /// so every discovery record can say WHICH root found it: a caller
    /// enumerates a tree by its root, and "the assets under THIS root" is a
    /// question only the registry can answer.
    fn scan_skill_dir(
        &self,
        dir: &Path,
        root: &Path,
        namespace: Option<&str>,
        depth: usize,
        kind: TemplateKind,
        ancestors: &mut BTreeSet<PathBuf>,
    ) {
        const MAX_DEPTH: usize = 8;
        if depth > MAX_DEPTH {
            self.diagnose(
                dir,
                format!("skill scan depth limit exceeded (maximum grouping depth: {MAX_DEPTH})"),
                true,
            );
            return;
        }
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(error) => {
                if depth > 0 || error.kind() != std::io::ErrorKind::NotFound {
                    self.diagnose(dir, format!("cannot read skill directory: {error}"), true);
                }
                return;
            }
        };
        let identity = match std::fs::canonicalize(dir) {
            Ok(path) => path,
            Err(error) => {
                self.diagnose(
                    dir,
                    format!("cannot resolve skill directory: {error}"),
                    true,
                );
                return;
            }
        };
        if !ancestors.insert(identity.clone()) {
            self.diagnose(dir, "skill directory symlink cycle detected".into(), true);
            return;
        }
        let mut entries = Vec::new();
        for entry in rd {
            match entry {
                Ok(entry) => entries.push(entry.path()),
                Err(error) => {
                    self.diagnose(dir, format!("cannot read directory entry: {error}"), true)
                }
            }
        }
        entries.sort();
        for p in entries {
            if p.is_file() {
                if depth == 0 && p.extension().and_then(|e| e.to_str()) == Some("md") {
                    self.insert_parsed(
                        parse_skill_file(&p, namespace),
                        &p,
                        &p,
                        root,
                        kind,
                        EntryKind::FlatFile,
                    );
                }
            } else if p.is_dir() {
                // Explicit roots remain usable, including .system bundles and
                // symlinked installs. Only incidental dependency/cache children
                // are skipped; do not blanket-ignore hidden grouping directories.
                if ignored_discovery_directory(&p) {
                    continue;
                }
                let skill_md = p.join("SKILL.md");
                if skill_md.is_file() {
                    self.insert_parsed(
                        parse_skill_dir(&p, &skill_md, namespace),
                        &skill_md,
                        &p,
                        root,
                        TemplateKind::Skill,
                        EntryKind::SkillDir,
                    );
                } else {
                    // The `SKILL.md` entry is there, but it is not a regular
                    // file: a DANGLING symlink (the target is gone), a FIFO, a
                    // socket. `metadata` FOLLOWS the link, so a link whose
                    // target is gone reads as "not there" — and the asset
                    // would vanish, with nothing to say about it.
                    //
                    // **The loader never treated it as a skill**, so this is
                    // not the loader's story to tell: it is a fact about the
                    // ENTRY.
                    if let Some(failure) = entry_failure(&skill_md) {
                        self.record_failed(
                            &skill_md,
                            &p,
                            root,
                            EntryKind::SkillDir,
                            failure,
                            failure_reason(failure),
                        );
                        continue;
                    }
                    self.scan_skill_dir(&p, root, namespace, depth + 1, kind, ancestors);
                }
            } else if let Some(failure) = entry_failure(&p) {
                // The entry itself is a dangling symlink (or not a regular
                // file). At depth 0 a flat `*.md` would have been a command
                // template; deeper down it is nothing the loader loads. Either
                // way it is an asset the walk found and could not use.
                if depth == 0 && p.extension().and_then(|e| e.to_str()) == Some("md") {
                    self.record_failed(
                        &p,
                        &p,
                        root,
                        EntryKind::FlatFile,
                        failure,
                        failure_reason(failure),
                    );
                }
            }
        }
        ancestors.remove(&identity);
    }

    /// Record an asset the walk found but could not use.
    fn record_failed(
        &self,
        path: &Path,
        asset: &Path,
        root: &Path,
        entry: EntryKind,
        failure: LoadFailure,
        reason: String,
    ) {
        let mut index = self.write();
        index.failed.push(FailedAsset {
            path: path.to_path_buf(),
            asset: asset.to_path_buf(),
            name: asset_name(asset, entry),
            entry,
            root: root.to_path_buf(),
            failure,
            reason,
        });
    }

    /// Resolve a skill by its registry key, falling back to bare-name lookup.
    ///
    /// File skills are keyed by their namespaced identity
    /// (`skills:atomcode-smoke-test`), but the `$` trigger and the `/skills`
    /// sub-menu display and submit the *unqualified* name for skills whose
    /// bare name is unique (see `build_skill_menu_items`). Without this
    /// fallback, `get("atomcode-smoke-test")` misses the `skills:`-prefixed
    /// key — so every user invocation of a file skill either errors with a
    /// bogus "unknown skill" (menu path) or is silently sent to the model as
    /// plain `$name` text (typed path). The model path is unaffected because
    /// the injected catalog carries the fully-qualified name.
    ///
    /// A bare name that maps to more than one namespace is disambiguated the
    /// way the menu is: `build_skill_menu_items` lists only user-invocable
    /// skills and shows the bare name when it is unique among THEM, so a lone
    /// user-invocable match wins even if a hidden (`user-invocable: false`)
    /// skill shares the bare name. Only a genuinely ambiguous bare name — zero
    /// or several user-invocable matches — returns `None`; the menu shows the
    /// qualified form there, which resolves via the exact-match branch.
    pub fn get(&self, name: &str) -> Option<Arc<Skill>> {
        let skills = self.read();
        if let Some(skill) = skills.get(name) {
            return Some(skill.clone());
        }
        // A standard, unnamespaced skill gets a qualified menu alias when its
        // bare slash name belongs to a separate flat command.
        if let Some(bare) = name.strip_prefix("skills:") {
            if let Some(skill) = skills.get(bare) {
                return Some(skill.clone());
            }
        }
        if name.contains(':') {
            return None;
        }
        let suffix = format!(":{}", name.to_ascii_lowercase());
        let mut n_all = 0usize;
        let mut last_all: Option<&Arc<Skill>> = None;
        let mut n_invocable = 0usize;
        let mut last_invocable: Option<&Arc<Skill>> = None;
        for skill in skills.values() {
            if skill.name.ends_with(&suffix) {
                n_all += 1;
                last_all = Some(skill);
                if skill.user_invocable {
                    n_invocable += 1;
                    last_invocable = Some(skill);
                }
            }
        }
        if n_all == 1 {
            return last_all.cloned();
        }
        // Multiple namespaces share this bare name: resolve to the sole
        // user-invocable one (mirrors the menu). Zero or several ⇒ ambiguous.
        if n_invocable == 1 {
            return last_invocable.cloned();
        }
        None
    }
    pub fn len(&self) -> usize {
        self.read().len()
    }
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }
    /// `(name, description)` for every skill, sorted by name.
    pub fn list(&self) -> Vec<(String, String)> {
        self.read()
            .values()
            .map(|s| (s.name.clone(), s.summary()))
            .collect()
    }

    /// Clear and reload from the standard home+project skill dirs (the `/skills`
    /// slash-menu source), under the `skills:` namespace — matching the driver's
    /// slash-command convention. Mirrors core `SkillRegistry::reload`.
    ///
    /// Reload the standard roots and return newly observed failures.
    pub fn reload(&self, roots: SkillRoots<'_>) -> Vec<String> {
        *self.write() = SkillIndex::default();
        self.set_native_roots(roots.native());
        let dirs = standard_skill_dirs(roots);
        self.write().directories = dirs.clone();
        for dir in dirs {
            self.load_standard_dir(&dir, Some("skills"));
        }
        self.take_new_warnings()
    }

    /// Skills a user can invoke from the `/` menu (frontmatter `user-invocable` not
    /// `false`), sorted by name.
    pub fn user_invocable(&self) -> Vec<Arc<Skill>> {
        self.read()
            .values()
            .filter(|s| s.user_invocable)
            .cloned()
            .collect()
    }

    /// Direct slash-command templates, including hidden commands so their
    /// visibility cannot accidentally expose a same-named skill as /name.
    pub fn slash_commands(&self) -> Vec<Arc<Skill>> {
        self.read().commands.values().cloned().collect()
    }

    /// Insert a skill programmatically (same-name replaces). Mirrors core
    /// `SkillRegistry::register`; used by drivers injecting non-file skills and by tests.
    pub fn register(&self, skill: Skill) {
        self.write().insert(skill.name.clone(), Arc::new(skill));
    }

    /// Every loaded skill, sorted by name.
    pub fn all(&self) -> Vec<Arc<Skill>> {
        self.read().values().cloned().collect()
    }

    /// Render the `=== AVAILABLE SKILLS ===` system-prompt section (budget-gated,
    /// source-ranked), or `None` when no skills are installed. See
    /// [`super::render`].
    pub fn render_catalog(&self) -> Option<String> {
        self.render_catalog_prioritizing("")
    }

    /// Render the catalog while preserving installed skills whose exact names
    /// appear in the effective project instruction text.
    pub fn render_catalog_prioritizing(&self, instruction_text: &str) -> Option<String> {
        let native_roots = self
            .native_roots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let skills = self.read();
        let entries: Vec<super::render::CatalogEntry> = skills
            .values()
            .map(|s| super::render::CatalogEntry {
                name: s.name.clone(),
                hint: None, // capabilities skills carry no argument hint
                description: s.summary(),
                source_rank: super::render::source_rank(&s.source_path, &native_roots),
            })
            .collect();
        let priority_names = skills
            .keys()
            .filter(|name| text_mentions_exact_name(instruction_text, name))
            .cloned()
            .collect::<Vec<_>>();
        super::render::render_skill_catalog_prioritizing(&entries, &priority_names)
    }
}

fn text_mentions_exact_name(text: &str, name: &str) -> bool {
    if text.is_empty() || name.is_empty() {
        return false;
    }
    let text = text.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    text.match_indices(&name).any(|(start, _)| {
        let end = start + name.len();
        let is_name_char = |ch: char| ch.is_alphanumeric() || matches!(ch, '_' | '-' | ':');
        let before_ok = text[..start]
            .chars()
            .next_back()
            .map_or(true, |ch| !is_name_char(ch));
        let after_ok = text[end..]
            .chars()
            .next()
            .map_or(true, |ch| !is_name_char(ch));
        before_ok && after_ok
    })
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The four roots skills are looked up under.
///
/// `home` and `project` hold OTHER products' dirs (`.claude`, `.agents`), which
/// this crate names because they are not ours. Our own two come from `dirs`:
/// what the product is called — and where a person moved its tree — is the
/// host's to say, not this crate's to guess.
#[derive(Clone, Copy, Debug)]
pub struct SkillRoots<'a> {
    /// The person's home directory.
    pub home: &'a Path,
    /// The project (workspace) root.
    pub project: &'a Path,
    /// Our user-level tree and per-project dir name.
    pub dirs: &'a ProductDirs,
}

impl SkillRoots<'_> {
    /// Our two trees — what [`SkillRegistry::set_native_roots`] wants.
    pub fn native(&self) -> Vec<PathBuf> {
        vec![
            self.dirs.user().to_path_buf(),
            self.dirs.project(self.project),
        ]
    }
}

/// The standard skill directories (LOW→HIGH priority), Claude-Code-compatible. A driver
/// may pass this to [`SkillRegistry::load`] or supply its own.
pub fn standard_skill_dirs(roots: SkillRoots<'_>) -> Vec<PathBuf> {
    let SkillRoots {
        home,
        project,
        dirs,
    } = roots;
    let user_dir = dirs.user();
    let project_dir = dirs.project(project);
    vec![
        home.join(".claude/commands"),
        user_dir.join("commands"),
        home.join(".claude/skills"),
        // `.agents/skills` — cross-agent shared convention (opencode et al.). Between
        // `.claude` and ours so our native skills win a same-name collision.
        home.join(".agents/skills"),
        user_dir.join("skills"),
        project.join(".claude/commands"),
        project_dir.join("commands"),
        project.join(".claude/skills"),
        project.join(".agents/skills"),
        project_dir.join("skills"),
    ]
}

/// Where a new skill should be written so [`standard_skill_dirs`] finds it and it
/// wins a name clash at its level: `(every project, this project)`.
///
/// Next to the list it indexes into, so the two cannot drift: the test
/// `install_dirs_are_the_winning_dirs_of_the_standard_list` pins it.
pub fn skill_install_dirs(roots: SkillRoots<'_>) -> (PathBuf, PathBuf) {
    (
        roots.dirs.user().join("skills"),
        roots.dirs.project(roots.project).join("skills"),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn versioned_installs_and_display_titles_have_safe_invocation_names() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = [
            ("atomgit-1.0.0", "# AtomGit", "atomgit"),
            ("debug-pro-1.0.0", "# debug-pro", "debug-pro"),
            ("test-runner-1.0.0", "# test-runner", "test-runner"),
            (
                "ffmpeg-video-editor-1.0.0",
                "---\nname: FFmpeg Video Editor\n---\nbody",
                "ffmpeg-video-editor",
            ),
            (
                "market-research-1.0.0",
                "---\nname: Market Research\nslug: market-research\n---\nbody",
                "market-research",
            ),
            (
                "self-improving-1.1.3",
                "---\nname: Self-Improving Agent (With Self-Reflection)\n---\nbody",
                "self-improving",
            ),
            (
                "seo-1.0.3",
                "---\nname: SEO (Site Audit + Content Writer + Competitor Analysis)\n---\nbody",
                "seo",
            ),
            (
                "social-media-scheduler-1.0.0",
                "---\nname: Social Media Scheduler\n---\nbody",
                "social-media-scheduler",
            ),
            (
                "renamed-1.0.0",
                "---\nname: explicit-name\n---\nbody",
                "explicit-name",
            ),
        ];
        for (dir, body, _) in entries {
            std::fs::create_dir(tmp.path().join(dir)).unwrap();
            std::fs::write(tmp.path().join(dir).join("SKILL.md"), body).unwrap();
        }
        let reg = SkillRegistry::load(&[tmp.path().to_path_buf()]);
        assert!(reg.take_new_warnings().is_empty());
        for (dir, _, name) in entries {
            let skill = reg.get(name).expect(name);
            assert_eq!(skill.skill_dir, tmp.path().join(dir));
            assert_eq!(skill.source_path, tmp.path().join(dir).join("SKILL.md"));
            assert!(skill.user_invocable);
        }
    }

    #[test]
    fn title_compatibility_does_not_relax_flat_command_validation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("command.md"),
            "---\nname: Invalid Command Title\n---\nbody",
        )
        .unwrap();
        let reg = SkillRegistry::new();
        reg.load_template_dir(tmp.path(), None, TemplateKind::Command);
        assert!(reg.get("command").is_none());
        assert_eq!(reg.take_new_warnings().len(), 1);
    }

    #[test]
    fn identical_definitions_are_quiet_but_keep_the_higher_priority_source() {
        let tmp = tempfile::tempdir().unwrap();
        let low = tmp.path().join("low");
        let high = tmp.path().join("high");
        for root in [&low, &high] {
            std::fs::create_dir_all(root.join("sample")).unwrap();
            std::fs::write(
                root.join("sample/SKILL.md"),
                "---\nname: sample\ndescription: same\n---\nbody",
            )
            .unwrap();
        }
        let reg = SkillRegistry::load(&[low.clone(), high.clone()]);
        assert!(reg.diagnostics().is_empty());
        assert_eq!(
            reg.get("sample").unwrap().source_path,
            high.join("sample/SKILL.md")
        );
        std::fs::write(
            high.join("sample/SKILL.md"),
            "---\nname: sample\ndescription: same\nuser-invocable: false\n---\nbody",
        )
        .unwrap();
        reg.reload_dirs(&[low, high], &[]);
        assert_eq!(
            reg.diagnostics().len(),
            1,
            "metadata changes remain visible"
        );
        assert!(!reg.diagnostics()[0].warning);
        assert!(!reg.get("sample").unwrap().user_invocable);
    }

    #[test]
    fn versioned_name_collisions_keep_existing_precedence_and_diagnostics() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["sample-1.0.0", "sample-2.0.0"] {
            std::fs::create_dir(tmp.path().join(name)).unwrap();
            std::fs::write(tmp.path().join(name).join("SKILL.md"), name).unwrap();
        }
        let reg = SkillRegistry::load(&[tmp.path().to_path_buf()]);
        assert_eq!(
            reg.get("sample").unwrap().skill_dir,
            tmp.path().join("sample-2.0.0")
        );
        let diagnostics = reg.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].reason.contains("overridden by"));
        assert!(!diagnostics[0].warning);
    }

    #[test]
    fn failures_are_reported_once_and_can_recover_on_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        std::fs::create_dir_all(root.join("broken")).unwrap();
        let broken = root.join("broken/SKILL.md");
        std::fs::write(&broken, "---\nname: bad.name\n---\nPRIVATE BODY").unwrap();
        let absent = tmp.path().join("missing");
        let implicit = tmp.path().join("implicit");
        let dirs = vec![root.clone(), absent.clone(), implicit];
        let reg = SkillRegistry::load_with_explicit(&dirs, std::slice::from_ref(&absent));
        assert_eq!(reg.diagnostics().len(), 2);
        let warnings = reg.take_new_warnings();
        assert_eq!(warnings.len(), 2);
        assert!(warnings
            .iter()
            .any(|s| s.contains("SKILL.md") && s.contains("invalid characters")));
        assert!(!warnings.join("\n").contains("PRIVATE BODY"));
        reg.reload_dirs_with_explicit(&dirs, &[], std::slice::from_ref(&absent));
        assert!(reg.take_new_warnings().is_empty());
        std::fs::write(&broken, "---\nname: fixed\n---\nworks").unwrap();
        std::fs::create_dir(&absent).unwrap();
        reg.reload_dirs_with_explicit(&dirs, &[], std::slice::from_ref(&absent));
        assert!(reg.diagnostics().is_empty());
        assert!(reg.get("fixed").is_some());
        std::fs::write(&broken, "---\nname: invalid.name\n---\nagain").unwrap();
        reg.reload_dirs_with_explicit(&dirs, &[], std::slice::from_ref(&absent));
        assert_eq!(reg.take_new_warnings().len(), 1, "recurrence is reported");
    }

    #[test]
    fn overridden_sources_are_visible_without_becoming_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let roots: Vec<_> = ["low", "high"]
            .iter()
            .map(|name| tmp.path().join(name))
            .collect();
        for root in &roots {
            std::fs::create_dir_all(root.join("same")).unwrap();
            std::fs::write(
                root.join("same/SKILL.md"),
                format!("skill from {}", root.display()),
            )
            .unwrap();
        }
        let reg = SkillRegistry::load(&roots);
        assert_eq!(
            reg.get("same").unwrap().source_path,
            roots[1].join("same/SKILL.md")
        );
        let diagnostics = reg.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, roots[0].join("same/SKILL.md"));
        assert!(diagnostics[0].reason.contains("high"));
        assert!(!diagnostics[0].warning);
        assert!(reg.take_new_warnings().is_empty());
    }

    #[test]
    fn install_dirs_are_the_winning_dirs_of_the_standard_list() {
        let dirs = ProductDirs::new("/custom/tree", ".ours");
        let roots = SkillRoots {
            home: Path::new("/home/u"),
            project: Path::new("/proj"),
            dirs: &dirs,
        };
        let dirs = standard_skill_dirs(roots);
        let (user, this_project) = skill_install_dirs(roots);
        let at = |p: &PathBuf| dirs.iter().position(|d| d == p);
        let user_at = at(&user).expect("the user install dir is scanned");
        let project_at = at(&this_project).expect("the project install dir is scanned");
        assert_eq!(
            project_at,
            dirs.len() - 1,
            "the project dir wins every clash"
        );
        assert!(
            dirs[user_at + 1..]
                .iter()
                .all(|d| d.starts_with(roots.project)),
            "the user dir wins every clash at the user level: {dirs:?}"
        );
    }

    use super::*;

    /// Our dirs are exactly the ones handed in — nothing under the person's
    /// home is spelled with our name — while other products' dirs stay where
    /// those products keep them.
    #[test]
    fn our_dirs_are_the_ones_handed_in_and_nothing_else_is_ours() {
        let home = Path::new("/home/u");
        let project = Path::new("/proj");
        let product = ProductDirs::new("/elsewhere/fork-tree", ".fork");
        let user_dir = product.user();
        let project_dir = product.project(project);
        let dirs = standard_skill_dirs(SkillRoots {
            home,
            project,
            dirs: &product,
        });

        for ours in ["skills", "commands"] {
            assert!(dirs.contains(&user_dir.join(ours)));
            assert!(dirs.contains(&project_dir.join(ours)));
        }
        for dir in &dirs {
            assert!(
                dir.starts_with(user_dir)
                    || dir.starts_with(&project_dir)
                    || dir.starts_with(home.join(".claude"))
                    || dir.starts_with(home.join(".agents"))
                    || dir.starts_with(project.join(".claude"))
                    || dir.starts_with(project.join(".agents")),
                "{dir:?} is neither ours (handed in) nor another product's"
            );
        }
    }

    #[test]
    fn a_flat_command_never_replaces_a_same_named_directory_skill() {
        for command_first in [false, true] {
            for hidden in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let skills = root.path().join("skills");
                let commands = root.path().join("commands");
                std::fs::create_dir_all(skills.join("changelog")).unwrap();
                std::fs::create_dir_all(&commands).unwrap();
                std::fs::write(
                    skills.join("changelog/SKILL.md"),
                    "---\ndescription: skill description\n---\nSKILL BODY",
                )
                .unwrap();
                // Identity is the frontmatter name, not the file's basename.
                std::fs::write(commands.join("other.md"), format!(
                    "---\nname: changelog\ndescription: command description\nuser-invocable: {}\n---\nCOMMAND BODY", !hidden)).unwrap();
                let registry = SkillRegistry::new();
                let paths = if command_first {
                    [&commands, &skills]
                } else {
                    [&skills, &commands]
                };
                for path in paths {
                    registry.load_template_dir(
                        path,
                        Some("plugin"),
                        if path == &commands {
                            TemplateKind::Command
                        } else {
                            TemplateKind::Skill
                        },
                    );
                }
                for name in ["changelog", "plugin:changelog"] {
                    let skill = registry.get(name).unwrap();
                    assert_eq!(skill.template, "SKILL BODY");
                    assert_eq!(skill.description, "skill description");
                    assert!(skill.user_invocable);
                }
                assert_eq!(registry.len(), 1);
                assert_eq!(registry.user_invocable().len(), 1);
                let command = registry.slash_commands().pop().unwrap();
                assert_eq!(command.template, "COMMAND BODY");
                assert_eq!(command.user_invocable, !hidden);
                let catalog = registry.render_catalog().unwrap();
                assert!(catalog.contains("skill description"));
                assert!(!catalog.contains("command description"));

                // Both maps are replaced together; removing a skill leaves the
                // command-only behavior intact, and removing that leaves nothing.
                let dirs = vec![
                    (skills.clone(), "plugin".into(), TemplateKind::Skill),
                    (commands.clone(), "plugin".into(), TemplateKind::Command),
                ];
                registry.reload_dirs(&[], &dirs);
                assert_eq!(registry.get("changelog").unwrap().template, "SKILL BODY");
                std::fs::remove_file(skills.join("changelog/SKILL.md")).unwrap();
                registry.reload_dirs(&[], &dirs);
                assert!(registry.get("changelog").is_none());
                assert_eq!(registry.slash_commands()[0].template, "COMMAND BODY");
                std::fs::remove_file(commands.join("other.md")).unwrap();
                registry.reload_dirs(&[], &dirs);
                assert!(registry.is_empty());
                assert!(registry.slash_commands().is_empty());
            }
        }
    }

    #[test]
    fn loads_flat_and_dir_skills_with_precedence() {
        let d = tempfile::tempdir().unwrap();
        // flat command
        std::fs::write(d.path().join("greet.md"), "Hello $ARGUMENTS\n").unwrap();
        // dir-style skill
        std::fs::create_dir_all(d.path().join("review")).unwrap();
        std::fs::write(
            d.path().join("review/SKILL.md"),
            "---\ndescription: review code\n---\nReview the diff.\n",
        )
        .unwrap();

        let reg = SkillRegistry::load(&[d.path().to_path_buf()]);
        assert_eq!(reg.len(), 2);
        assert!(reg.get("greet").is_some());
        let review = reg.get("review").unwrap();
        assert_eq!(review.description, "review code");
    }

    /// A namespaced file skill (`skills:atomcode-smoke-test`) must resolve by
    /// its bare name — the exact spelling the `$` trigger and `/skills` menu
    /// submit. Regression for the reported "first `$atomcode-smoke-test` errors
    /// with 未知技能, second runs it as plain text" bug.
    #[test]
    fn get_resolves_namespaced_skill_by_bare_name() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("atomcode-smoke-test")).unwrap();
        std::fs::write(
            d.path().join("atomcode-smoke-test/SKILL.md"),
            "---\nname: atomcode-smoke-test\ndescription: probe\n---\nbody\n",
        )
        .unwrap();
        let reg = SkillRegistry::new();
        reg.load_dir(d.path(), Some("skills"));

        // Stored under the namespaced key…
        assert!(reg.get("skills:atomcode-smoke-test").is_some());
        // …but the bare name the menu submits must resolve too.
        assert_eq!(
            reg.get("atomcode-smoke-test").map(|s| s.name.clone()),
            Some("skills:atomcode-smoke-test".to_string())
        );
        // Case-insensitive on the bare fallback (menu names are lowercased,
        // but a typed capitalisation should still land).
        assert!(reg.get("Atomcode-Smoke-Test").is_some());
    }

    /// When two namespaces own the same bare name the reference is ambiguous:
    /// the bare fallback must decline (the menu shows the qualified form, which
    /// still resolves via exact match).
    #[test]
    fn get_bare_name_is_ambiguous_across_namespaces() {
        let reg = SkillRegistry::new();
        reg.register(Skill {
            name: "skills:dup".into(),
            display_name: None,
            description: "a".into(),
            template: "A".into(),
            allowed_tools: Vec::new(),
            user_invocable: true,
            auto_load: Default::default(),
            args_requirement: atomcode_kernel::agent::ArgsRequirement::Unspecified,
            skill_dir: PathBuf::from("a"),
            source_path: PathBuf::from("a"),
        });
        reg.register(Skill {
            name: "plugin:dup".into(),
            display_name: None,
            description: "b".into(),
            template: "B".into(),
            allowed_tools: Vec::new(),
            user_invocable: true,
            auto_load: Default::default(),
            args_requirement: atomcode_kernel::agent::ArgsRequirement::Unspecified,
            skill_dir: PathBuf::from("b"),
            source_path: PathBuf::from("b"),
        });
        assert!(reg.get("dup").is_none(), "ambiguous bare name must decline");
        // Qualified names still resolve exactly.
        assert!(reg.get("skills:dup").is_some());
        assert!(reg.get("plugin:dup").is_some());
    }

    /// The `$`/`/skills` menu lists only user-invocable skills and shows a bare
    /// name when unique AMONG THEM. So a hidden (`user-invocable: false`) skill
    /// sharing the bare name must NOT make the menu-shown bare name ambiguous —
    /// otherwise the "menu shows it but resolve fails → 未知技能" bug returns.
    #[test]
    fn get_bare_name_prefers_sole_user_invocable_over_hidden() {
        let reg = SkillRegistry::new();
        reg.register(Skill {
            name: "skills:review".into(),
            display_name: None,
            description: "visible".into(),
            template: "V".into(),
            allowed_tools: Vec::new(),
            user_invocable: true,
            auto_load: Default::default(),
            args_requirement: atomcode_kernel::agent::ArgsRequirement::Unspecified,
            skill_dir: PathBuf::from("v"),
            source_path: PathBuf::from("v"),
        });
        reg.register(Skill {
            name: "plugin:review".into(),
            display_name: None,
            description: "hidden".into(),
            template: "H".into(),
            allowed_tools: Vec::new(),
            user_invocable: false,
            auto_load: Default::default(),
            args_requirement: atomcode_kernel::agent::ArgsRequirement::Unspecified,
            skill_dir: PathBuf::from("h"),
            source_path: PathBuf::from("h"),
        });
        assert_eq!(
            reg.get("review").map(|s| s.name.clone()),
            Some("skills:review".to_string()),
            "bare name must resolve to the sole user-invocable skill"
        );
    }

    #[test]
    fn later_dir_overrides_same_name() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("x.md"), "from A\n").unwrap();
        std::fs::write(b.path().join("x.md"), "from B\n").unwrap();
        let reg = SkillRegistry::load(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        assert!(
            reg.get("x").unwrap().template.contains("from B"),
            "later dir wins"
        );
    }

    #[test]
    fn missing_dir_is_skipped() {
        let reg = SkillRegistry::load(&[PathBuf::from("/no/such/skills/dir")]);
        assert!(reg.is_empty());
    }

    #[test]
    fn standard_dirs_include_agents_skills_between_claude_and_ours() {
        let home = Path::new("/home/u");
        let project = Path::new("/proj");
        let product = ProductDirs::new("/home/u/.ours", ".ours");
        let user_dir = product.user();
        let project_dir = product.project(project);
        let dirs = standard_skill_dirs(SkillRoots {
            home,
            project,
            dirs: &product,
        });
        // `.agents/skills` is the cross-agent shared convention (opencode et al.) —
        // scanned at BOTH user and project level so shared skills load directly.
        assert!(
            dirs.contains(&home.join(".agents/skills")),
            "user-level ~/.agents/skills"
        );
        assert!(
            dirs.contains(&project.join(".agents/skills")),
            "project-level .agents/skills"
        );
        // Precedence (last-wins): .claude < .agents < ours at each level, so a
        // user's native skill still overrides a same-named shared one.
        let pos = |p: PathBuf| dirs.iter().position(|d| *d == p).expect("dir present");
        assert!(pos(home.join(".claude/skills")) < pos(home.join(".agents/skills")));
        assert!(pos(home.join(".agents/skills")) < pos(user_dir.join("skills")));
        assert!(pos(project.join(".claude/skills")) < pos(project.join(".agents/skills")));
        assert!(pos(project.join(".agents/skills")) < pos(project_dir.join("skills")));
    }

    #[test]
    fn duplicate_directory_skills_in_our_user_dir_collapse_to_one() {
        let home = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let product = ProductDirs::new(tree.path(), ".ours");

        let skill_root = tree.path().join("skills");
        for (dir, body) in [("dedup-a", "from A"), ("dedup-b", "from B")] {
            let skill_dir = skill_root.join(dir);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: dedup-skill\ndescription: duplicate\n---\n{body}\n"),
            )
            .unwrap();
        }

        let dirs = standard_skill_dirs(SkillRoots {
            home: home.path(),
            project: project.path(),
            dirs: &product,
        });
        let user_skills = dirs
            .iter()
            .find(|dir| *dir == &skill_root)
            .cloned()
            .expect("the handed-in user tree's skills directory should be scanned");
        let reg = SkillRegistry::load(&[user_skills]);
        assert_eq!(reg.len(), 1, "same-name skills must collapse to one entry");
        assert!(reg.get("dedup-skill").is_some());
    }

    #[test]
    fn dependency_and_cache_trees_are_not_skill_groups() {
        let root = tempfile::tempdir().unwrap();
        for ignored in [
            ".venv",
            "venv",
            "node_modules",
            ".git",
            "__pycache__",
            ".pytest_cache",
            ".tox",
            ".trash",
        ] {
            let directory = root.path().join(ignored).join("dependency");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("SKILL.md"),
                "---\ndescription: dependency example\n---\nNot an installed skill.",
            )
            .unwrap();
            // Reproduce NumPy's deep environment tree from the report.
            std::fs::create_dir_all(
                root.path()
                    .join(ignored)
                    .join("lib/python/site-packages/numpy/core/tests/examples/cython/deep"),
            )
            .unwrap();
        }
        let builtin = root.path().join(".system/real-skill");
        std::fs::create_dir_all(&builtin).unwrap();
        std::fs::write(
            builtin.join("SKILL.md"),
            "---\ndescription: built-in skill\n---\nReal skill.",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[root.path().to_path_buf()]);
        assert_eq!(
            registry.len(),
            1,
            "dependency examples must not become skills"
        );
        assert!(
            registry.get("real-skill").is_some(),
            "do not skip all dot-directories"
        );
        assert!(
            registry.diagnostics().is_empty(),
            "{:?}",
            registry.diagnostics()
        );
        // A caller can still explicitly select a custom root under an environment.
        let explicit = SkillRegistry::load(&[root.path().join(".venv")]);
        assert!(explicit.get("dependency").is_some());
    }

    #[test]
    fn a_deep_group_is_not_reported_as_a_symlink_cycle() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b/c/d/e/f/g/h/i/j")).unwrap();
        let registry = SkillRegistry::load(&[root.path().to_path_buf()]);
        let warnings = registry.take_new_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("depth limit"));
        assert!(!warnings[0].contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_is_detected_before_the_depth_limit_without_hiding_valid_links() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(external.path().join("linked-skill")).unwrap();
        std::fs::write(
            external.path().join("linked-skill/SKILL.md"),
            "---\ndescription: linked\n---\nLinked skill.",
        )
        .unwrap();
        symlink(external.path(), root.path().join("group-link")).unwrap();
        symlink(root.path(), root.path().join("loop")).unwrap();
        let registry = SkillRegistry::load(&[root.path().to_path_buf()]);
        assert!(registry.get("linked-skill").is_some());
        let warnings = registry.take_new_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("symlink cycle detected"),
            "{warnings:?}"
        );
        assert!(
            warnings[0].contains("loop:"),
            "diagnose the first cycle, not depth-nine descendants: {warnings:?}"
        );
    }

    #[test]
    fn discovers_nested_skills_under_grouping_dirs() {
        let d = tempfile::tempdir().unwrap();
        // A grouping dir (no SKILL.md of its own) holding a nested skill two levels deep.
        std::fs::create_dir_all(d.path().join("GROUP/sub")).unwrap();
        std::fs::write(
            d.path().join("GROUP/sub/SKILL.md"),
            "---\ndescription: nested skill\n---\nDo the nested thing.\n",
        )
        .unwrap();
        // A skill dir's OWN nested *.md must NOT be mis-loaded as a separate skill.
        std::fs::create_dir_all(d.path().join("top")).unwrap();
        std::fs::write(
            d.path().join("top/SKILL.md"),
            "---\ndescription: top\n---\nTop.\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("top/resource.md"),
            "internal resource, not a command\n",
        )
        .unwrap();

        let reg = SkillRegistry::load(&[d.path().to_path_buf()]);
        assert!(
            reg.get("sub").is_some(),
            "nested GROUP/sub/SKILL.md must be discovered"
        );
        assert_eq!(reg.get("sub").unwrap().description, "nested skill");
        assert!(reg.get("top").is_some(), "top-level skill dir loaded");
        assert!(
            reg.get("resource").is_none(),
            "a skill's own resource .md is not a command"
        );
    }

    #[test]
    fn display_titles_are_discoverable_but_never_resolve_as_aliases() {
        let root = tempfile::tempdir().unwrap();
        for id in ["packager", "other"] {
            std::fs::create_dir(root.path().join(id)).unwrap();
            std::fs::write(root.path().join(id).join("SKILL.md"),
                format!("---\nname: {id}\ndisplay-name: 玲珑打包技能\ndescription: Build packages\n---\nbody")).unwrap();
        }
        let reg = SkillRegistry::load(&[root.path().to_path_buf()]);
        assert_eq!(reg.list().len(), 2);
        assert!(reg.get("packager").is_some());
        assert!(reg.get("other").is_some());
        assert!(reg.get("玲珑打包技能").is_none());
        assert!(reg
            .list()
            .iter()
            .all(|(_, summary)| summary.contains("玲珑打包技能")));
        let catalog = reg.render_catalog().unwrap();
        assert!(catalog.contains("packager"));
        assert!(catalog.contains("玲珑打包技能"));
    }

    #[test]
    fn chinese_names_load_from_frontmatter_directories_and_flat_files() {
        let tmp = tempfile::tempdir().unwrap();
        for (dir, content) in [
            (
                "linyaps-packager-skill",
                "---\nname: 玲珑打包技能\ndescription: 打包\n---\n打包 $ARGUMENTS",
            ),
            ("中文目录-1.0.0", "目录技能"),
        ] {
            std::fs::create_dir(tmp.path().join(dir)).unwrap();
            std::fs::write(tmp.path().join(dir).join("SKILL.md"), content).unwrap();
        }
        std::fs::write(tmp.path().join("中文指令.md"), "平铺技能").unwrap();
        let reg = super::SkillRegistry::load(&[tmp.path().to_path_buf()]);
        assert!(reg.diagnostics().is_empty());
        assert_eq!(
            reg.get("玲珑打包技能").unwrap().expand("应用", ""),
            "打包 应用"
        );
        assert!(reg.get("中文目录").is_some());
        assert!(reg.get("中文指令").is_some());
        assert!(reg.get("skills:玲珑打包技能").is_some());
        let namespaced = super::SkillRegistry::new();
        namespaced.load_dir(tmp.path(), Some("plugin"));
        assert!(namespaced.get("plugin:玲珑打包技能").is_some());
        assert!(namespaced.get("玲珑打包技能").is_some());
        assert!(text_mentions_exact_name(
            "使用 `玲珑打包技能`",
            "玲珑打包技能"
        ));
        assert!(!text_mentions_exact_name(
            "其他玲珑打包技能",
            "玲珑打包技能"
        ));
        assert!(!text_mentions_exact_name(
            "玲珑打包技能扩展",
            "玲珑打包技能"
        ));
    }

    #[test]
    fn instruction_skill_reference_requires_exact_token_boundaries() {
        assert!(text_mentions_exact_name(
            "Use `superpowers:writing-plans` before editing.",
            "superpowers:writing-plans"
        ));
        assert!(text_mentions_exact_name(
            "Use BRAINSTORMING first.",
            "brainstorming"
        ));
        assert!(!text_mentions_exact_name(
            "Use brainstorming-extra first.",
            "brainstorming"
        ));
        assert!(!text_mentions_exact_name(
            "Use mybrainstorming first.",
            "brainstorming"
        ));
    }
}
