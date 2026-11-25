//! Path-confinement middleware for the REVIEW agent.
//!
//! 1. Pins every read-only tool's path arguments inside the repo root (blocks
//!    `grep /` whole-container scans → OOM, and out-of-repo reads).
//! 2. Optional **review-scope allowlist**: when the driver knows the changed /
//!    reviewable file set, tools may only touch those files (and their ancestor
//!    directories for list/grep). Stops the model from `read_file`ing ignored
//!    siblings like `notes.md` after DefaultIgnore already dropped them from the diff.
//!
//! Mounted ONLY on the review agent (see [`crate::assemble`]); other
//! specializations are untouched.

use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// JSON arg fields that carry a filesystem path across the review toolset
/// (read_file/grep/glob/list_directory/ast_grep/read_symbol/list_symbols/
/// find_references/blast_radius/file_dependencies/diagnostics). `paths` (ast_grep)
/// is the only array-valued one.
const PATH_KEYS: &[&str] = &["path", "file_path", "file", "dir", "paths"];

/// Tools that default to the working dir when no path is given. With an allowlist
/// we require an explicit in-scope path so they cannot re-scan the whole repo.
const PATH_SCOPED_TOOLS: &[&str] = &[
    "read_file",
    "grep",
    "glob",
    "list_directory",
    "ast_grep",
    "read_symbol",
    "list_symbols",
    "find_references",
    "blast_radius",
    "file_dependencies",
    "diagnostics",
];

/// Blocks review tool calls that escape the repo root, and optionally paths
/// outside the reviewable file set.
pub struct PathConfineMiddleware {
    root: PathBuf,
    /// Repo-relative paths (normalized, no `./` prefix). Empty `None` ⇒ root-only
    /// confinement (legacy). `Some(set)` ⇒ also require path ∈ set or ancestor of set.
    allow: Option<HashSet<PathBuf>>,
    /// Extra read-only roots the review tools may read, e.g. the `--skill-dir`
    /// packages. A skill's `SKILL.md` is only an index; its `references/` rules
    /// live in the skill package, which may sit outside the reviewed repo. Without
    /// this, `use_skill` returns the index and every `read_file` of the referenced
    /// rules is denied — the skill looks used but its rules never reach the model.
    ///
    /// Checked BEFORE the repo/allowlist logic: a skill package may also live
    /// INSIDE the repo (`.claude/skills`, `./skills`) and would otherwise be
    /// blocked by the review-scope allowlist. Canonicalized at construction.
    read_roots: Vec<PathBuf>,
}

impl PathConfineMiddleware {
    /// `root` is the review repo (the agent's working_dir). Callers pass an
    /// already-canonicalized path (clix canonicalizes `--repo`); we normalize it
    /// lexically too so containment checks compare like-for-like.
    pub fn new(root: PathBuf) -> Self {
        // Strip the Windows `\\?\` verbatim prefix that `canonicalize` adds (clix
        // canonicalizes `--repo`). Without this, `root` carries a `VerbatimDisk("C:")`
        // prefix while model-supplied absolute paths normalize to a plain
        // `Disk("C:")` prefix, so `starts_with(root)` is ALWAYS false and every
        // in-repo absolute path is wrongly rejected on Windows.
        let root = atomcode_capabilities::pathnorm::strip_verbatim_path(&root);
        Self {
            root: normalize_lexical(&root),
            allow: None,
            read_roots: Vec::new(),
        }
    }

    /// Allow reading files under these roots regardless of whether they sit inside
    /// or outside the repo. Used for `--skill-dir`: a skill package's `references/`
    /// must be readable or `use_skill` only delivers the `SKILL.md` index and the
    /// detailed rules never load.
    ///
    /// Each root is resolved to the actual skill package(s) it names:
    /// - the dir itself has `SKILL.md` → that dir;
    /// - otherwise its direct child dirs with `SKILL.md` → those dirs (a parent
    ///   directory holding several skills; the parent itself is NOT opened, so a
    ///   parent that happens to contain the reviewed repo cannot bypass the
    ///   review-scope allowlist);
    /// - otherwise flat `*.md` skill files (the `SkillRegistry` single-file
    ///   layout) → that dir;
    /// - anything else (`~`, `/tmp`) is skipped with a hint.
    ///
    /// The review allowlist still governs every non-skill repo path: these roots
    /// are read-only resources, not review targets.
    pub fn with_read_roots(mut self, roots: &[PathBuf]) -> Self {
        let mut list: Vec<PathBuf> = Vec::with_capacity(roots.len());
        for r in roots {
            let Ok(c) = std::fs::canonicalize(r) else {
                continue;
            };
            collect_read_roots(&normalize_lexical(&c), &mut list);
        }
        self.read_roots = list;
        self
    }

    /// 构造后的只读根（测试断言接线用）。
    pub(crate) fn read_roots(&self) -> &[PathBuf] {
        &self.read_roots
    }

    /// Restrict tools to the given repo-relative reviewable paths (changed files
    /// after ignore/fold). Empty slice leaves root-only confinement.
    pub fn with_allowlist(mut self, paths: &[String]) -> Self {
        if paths.is_empty() {
            self.allow = None;
            return self;
        }
        let mut set = HashSet::with_capacity(paths.len());
        for p in paths {
            let p = p.trim();
            if p.is_empty() {
                continue;
            }
            let rel = normalize_lexical(Path::new(p));
            // Drop absolute / escape attempts in the allowlist itself.
            if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
                continue;
            }
            if rel.as_os_str().is_empty() {
                continue;
            }
            set.insert(rel);
        }
        self.allow = if set.is_empty() { None } else { Some(set) };
        self
    }
}

#[async_trait]
impl ToolMiddleware for PathConfineMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        match check_arguments(
            &self.root,
            self.allow.as_ref(),
            &self.read_roots,
            &call.name,
            &call.arguments,
        ) {
            Ok(()) => BeforeOutcome::Proceed,
            Err(reason) => BeforeOutcome::deny(reason),
        }
    }
}

/// Reject if any path field escapes `root`, or (when `allow` is set) falls outside
/// the reviewable set. Paths outside `root` are accepted only when they fall under
/// one of `read_roots` (skill packages). Unparseable args pass through — the tool
/// will reject them.
fn check_arguments(
    root: &Path,
    allow: Option<&HashSet<PathBuf>>,
    read_roots: &[PathBuf],
    tool_name: &str,
    args: &str,
) -> Result<(), String> {
    let v: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    let mut saw_path = false;
    for key in PATH_KEYS {
        match v.get(key) {
            Some(serde_json::Value::String(s)) => {
                saw_path = true;
                check_one(root, allow, read_roots, s)?;
            }
            Some(serde_json::Value::Array(items)) => {
                for it in items {
                    if let Some(s) = it.as_str() {
                        saw_path = true;
                        check_one(root, allow, read_roots, s)?;
                    }
                }
            }
            _ => {}
        }
    }

    // Allowlist + path-scoped tool + no path ⇒ would default to whole working_dir.
    if allow.is_some() && !saw_path && PATH_SCOPED_TOOLS.contains(&tool_name) {
        return Err(
            "path is required under review scope confinement; pass a path within the changed/reviewable files"
                .into(),
        );
    }
    Ok(())
}

fn check_one(
    root: &Path,
    allow: Option<&HashSet<PathBuf>>,
    read_roots: &[PathBuf],
    raw: &str,
) -> Result<(), String> {
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let normalized = normalize_lexical(&joined);
    // Skill roots win regardless of where the package sits: outside the repo
    // (server-side cache) or inside it (`.claude/skills`, `./skills`). The review
    // allowlist governs review targets, not skill resources.
    if !read_roots.is_empty() && path_in_read_root(read_roots, &joined) {
        return Ok(());
    }
    if !normalized.starts_with(root) {
        return Err(format!(
            "path '{raw}' is outside the review repository; review tools may only access files within the repo"
        ));
    }
    if let Some(existing) = existing_prefix(&joined) {
        if let (Ok(canon_root), Ok(canon_existing)) =
            (std::fs::canonicalize(root), std::fs::canonicalize(existing))
        {
            if !canon_existing.starts_with(canon_root) {
                return Err(format!(
                    "path '{raw}' is outside the review repository; review tools may only access files within the repo"
                ));
            }
        }
    }

    if let Some(allowed) = allow {
        let rel = match normalized.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => {
                return Err(format!(
                    "path '{raw}' is outside the review repository; review tools may only access files within the repo"
                ));
            }
        };
        // Empty rel = repo root (".", "", absolute root). Not a useful review target
        // when we have a tighter allowlist — force an explicit scoped path.
        if rel.as_os_str().is_empty() {
            return Err(format!(
                "path '{raw}' is outside the review scope; tools may only access the changed/reviewable files (and their parent directories)"
            ));
        }
        if !path_in_review_scope(&rel, allowed) {
            return Err(format!(
                "path '{raw}' is outside the review scope; tools may only access the changed/reviewable files (and their parent directories)"
            ));
        }
    }
    Ok(())
}

/// `rel` is allowed if it is an allowlisted file, or an ancestor directory of one
/// (so `list_directory` / scoped `grep` on a parent still work).
fn path_in_review_scope(rel: &Path, allowed: &HashSet<PathBuf>) -> bool {
    if allowed.contains(rel) {
        return true;
    }
    for f in allowed {
        if f.starts_with(rel) {
            return true;
        }
    }
    false
}

/// Whether `joined` resolves under an explicitly declared read-only root (skill
/// package). Judgment uses the canonicalized existing prefix, so a symlink planted
/// inside a skill directory cannot escape it.
fn path_in_read_root(read_roots: &[PathBuf], joined: &Path) -> bool {
    let Some(existing) = existing_prefix(joined) else {
        return false;
    };
    let Ok(canon) = std::fs::canonicalize(existing) else {
        return false;
    };
    read_roots.iter().any(|r| canon.starts_with(r))
}

/// Resolve one `--skill-dir` into the actual read-only roots. See
/// [`PathConfineMiddleware::with_read_roots`] for the accepted layouts. A parent
/// directory holding multiple skills contributes its skill CHILD dirs, never itself.
fn collect_read_roots(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.join("SKILL.md").is_file() {
        push_unique(out, dir.to_path_buf());
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found_child = false;
    let mut has_flat_md = false;
    for e in entries.flatten() {
        let child = e.path();
        if child.is_dir() && child.join("SKILL.md").is_file() {
            // canonicalize follows a symlinked skill dir to its real target.
            if let Ok(c) = std::fs::canonicalize(&child) {
                push_unique(out, normalize_lexical(&c));
            }
            found_child = true;
            continue;
        }
        if e.file_type().map(|t| t.is_file()).unwrap_or(false)
            && child
                .extension()
                .map(|x| x.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        {
            has_flat_md = true;
        }
    }
    if found_child {
        return;
    }
    if has_flat_md {
        // Single-file skill layout (`<name>.md`), same contract as SkillRegistry.
        push_unique(out, dir.to_path_buf());
        return;
    }
    eprintln!(
        "[confine] ignoring read root without SKILL.md or skill files: {}",
        dir.display()
    );
}

fn push_unique(list: &mut Vec<PathBuf>, p: PathBuf) {
    if !list.contains(&p) {
        list.push(p);
    }
}

/// Longest existing prefix of `p`, used to catch symlink escapes without requiring
/// the final path (or glob tail) to exist.
fn existing_prefix(p: &Path) -> Option<&Path> {
    for candidate in p.ancestors() {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Lexical normalization: drop `.` and resolve `..` against prior components.
/// This catches `../` escapes before the later existing-prefix symlink check.
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/work/repo")
    }

    fn allow(files: &[&str]) -> HashSet<PathBuf> {
        files.iter().map(|f| PathBuf::from(f)).collect()
    }

    #[test]
    fn rejects_grep_root() {
        // The actual production failure: `grep /` scanned the whole container → OOM.
        assert!(check_arguments(
            &root(),
            None,
            &[],
            "grep",
            r#"{"pattern":"foo","path":"/"}"#
        )
        .is_err());
    }

    #[test]
    fn allows_relative_inside_repo() {
        assert!(check_arguments(
            &root(),
            None,
            &[],
            "read_file",
            r#"{"file_path":"src/a.rs"}"#
        )
        .is_ok());
    }

    #[test]
    fn allows_absolute_inside_repo() {
        assert!(check_arguments(
            &root(),
            None,
            &[],
            "read_file",
            r#"{"file_path":"/work/repo/src/a.rs"}"#
        )
        .is_ok());
    }

    #[test]
    fn rejects_parent_escape() {
        assert!(check_arguments(
            &root(),
            None,
            &[],
            "read_file",
            r#"{"file_path":"../../etc/passwd"}"#
        )
        .is_err());
    }

    #[test]
    fn rejects_absolute_outside_repo() {
        assert!(check_arguments(
            &root(),
            None,
            &[],
            "read_file",
            r#"{"file_path":"/etc/passwd"}"#
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("link")).unwrap();

        let root = std::fs::canonicalize(&repo).unwrap();
        assert!(check_arguments(
            &root,
            None,
            &[],
            "read_file",
            r#"{"file_path":"link/secret.txt"}"#
        )
        .is_err());
    }

    #[test]
    fn pattern_with_slash_not_treated_as_path() {
        // grep with no `path` field: a regex pattern containing '/' must NOT be
        // mistaken for a path argument. Without allowlist this still passes.
        assert!(check_arguments(&root(), None, &[], "grep", r#"{"pattern":"a/b/c"}"#).is_ok());
    }

    #[test]
    fn array_paths_any_escape_blocks() {
        // ast_grep `paths`: one inside, one escaping → blocked; all inside → ok.
        assert!(check_arguments(
            &root(),
            None,
            &[],
            "ast_grep",
            r#"{"paths":["src","/etc"]}"#
        )
        .is_err());
        assert!(
            check_arguments(&root(), None, &[], "ast_grep", r#"{"paths":["src","lib"]}"#).is_ok()
        );
    }

    #[test]
    fn missing_path_field_passes_without_allowlist() {
        // grep default path "." (field absent) → tool resolves to working_dir, safe
        // when we only confine to repo root.
        assert!(check_arguments(&root(), None, &[], "grep", r#"{"pattern":"foo"}"#).is_ok());
    }

    #[test]
    fn unparseable_args_pass_through() {
        assert!(check_arguments(&root(), None, &[], "read_file", "not json").is_ok());
    }

    #[test]
    fn allowlist_allows_changed_file() {
        let a = allow(&["pkg/conanfile.py", "pkg/patches/a.patch"]);
        assert!(check_arguments(
            &root(),
            Some(&a),
            &[],
            "read_file",
            r#"{"file_path":"pkg/conanfile.py"}"#
        )
        .is_ok());
        assert!(check_arguments(
            &root(),
            Some(&a),
            &[],
            "read_file",
            r#"{"file_path":"/work/repo/pkg/patches/a.patch"}"#
        )
        .is_ok());
    }

    #[test]
    fn allowlist_allows_ancestor_dir() {
        let a = allow(&["pkg/conanfile.py"]);
        assert!(check_arguments(
            &root(),
            Some(&a),
            &[],
            "list_directory",
            r#"{"path":"pkg"}"#
        )
        .is_ok());
    }

    #[test]
    fn allowlist_rejects_sibling_not_in_scope() {
        // The production anomaly: notes.md ignored from diff but still read_file'd.
        let a = allow(&["pkg/conanfile.py", "pkg/patches/a.patch"]);
        let err = check_arguments(
            &root(),
            Some(&a),
            &[],
            "read_file",
            r#"{"file_path":"pkg/notes.md"}"#,
        );
        assert!(err.is_err(), "notes.md must be denied");
        let msg = err.unwrap_err();
        assert!(
            msg.contains("review scope"),
            "message should mention scope: {msg}"
        );
    }

    #[test]
    fn allowlist_rejects_unrelated_path() {
        let a = allow(&["pkg/conanfile.py"]);
        assert!(check_arguments(
            &root(),
            Some(&a),
            &[],
            "read_file",
            r#"{"file_path":"other/main.go"}"#
        )
        .is_err());
    }

    #[test]
    fn allowlist_requires_path_on_scoped_tools() {
        let a = allow(&["pkg/a.go"]);
        assert!(check_arguments(&root(), Some(&a), &[], "grep", r#"{"pattern":"foo"}"#).is_err());
        assert!(check_arguments(&root(), Some(&a), &[], "list_directory", r#"{}"#).is_err());
    }

    #[test]
    fn allowlist_rejects_repo_root_dot() {
        let a = allow(&["pkg/a.go"]);
        assert!(check_arguments(
            &root(),
            Some(&a),
            &[],
            "grep",
            r#"{"pattern":"x","path":"."}"#
        )
        .is_err());
    }

    #[test]
    fn with_allowlist_builder() {
        let mw = PathConfineMiddleware::new(root()).with_allowlist(&[
            "pkg/a.go".into(),
            "".into(),
            "  ".into(),
        ]);
        assert!(mw.allow.as_ref().unwrap().contains(Path::new("pkg/a.go")));
        assert_eq!(mw.allow.as_ref().unwrap().len(), 1);
    }

    /// A skill package lives OUTSIDE the reviewed repo (`--skill-dir`). Its
    /// `SKILL.md` is only an index: the detailed rules sit in `references/`, so
    /// `read_file` on the skill dir must be allowed or `use_skill` delivers an
    /// index whose rules can never be loaded.
    #[test]
    fn read_roots_allow_skill_files_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let skill = tmp.path().join("skills").join("demo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(skill.join("references")).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: demo\n---\nbody").unwrap();
        std::fs::write(skill.join("references/rules-api.md"), "rule").unwrap();
        let root = std::fs::canonicalize(&repo).unwrap();
        let skill_root = std::fs::canonicalize(&skill).unwrap();
        let target = skill_root.join("references/rules-api.md");
        let args = serde_json::json!({"file_path": target.to_string_lossy()}).to_string();
        let a = allow(&["docs/a.md"]);

        assert!(
            check_arguments(
                &root,
                Some(&a),
                std::slice::from_ref(&skill_root),
                "read_file",
                &args,
            )
            .is_ok(),
            "skill references outside the repo must be readable"
        );
    }

    /// `--skill-dir` can also point INSIDE the repo (`.claude/skills`, `./skills`).
    /// Those files are not in the review allowlist, so the read-root exemption must
    /// be evaluated before the repo/allowlist logic or the same silent degradation
    /// returns for in-repo skill packages.
    #[test]
    fn read_roots_allow_skill_files_inside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let skill = repo.join(".claude/skills/demo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(skill.join("references")).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: demo\n---\nbody").unwrap();
        std::fs::write(skill.join("references/rules.md"), "rule").unwrap();
        let root = std::fs::canonicalize(&repo).unwrap();
        let skill_root = std::fs::canonicalize(&skill).unwrap();
        let a = allow(&["docs/a.md"]);
        let target = skill_root.join("references/rules.md");
        let args = serde_json::json!({"file_path": target.to_string_lossy()}).to_string();

        assert!(
            check_arguments(
                &root,
                Some(&a),
                std::slice::from_ref(&skill_root),
                "read_file",
                &args,
            )
            .is_ok(),
            "in-repo skill references must be readable"
        );
        // Other repo files are still governed by the review allowlist.
        assert!(check_arguments(
            &root,
            Some(&a),
            std::slice::from_ref(&skill_root),
            "read_file",
            r#"{"file_path":"src/other.rs"}"#
        )
        .is_err());
    }

    #[test]
    fn read_roots_do_not_open_other_outside_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let skill = tmp.path().join("skills").join("demo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&skill).unwrap();
        let root = std::fs::canonicalize(&repo).unwrap();
        let skill_root = std::fs::canonicalize(&skill).unwrap();
        assert!(check_arguments(
            &root,
            None,
            std::slice::from_ref(&skill_root),
            "read_file",
            r#"{"file_path":"/etc/passwd"}"#
        )
        .is_err());
    }

    #[test]
    fn read_roots_do_not_bypass_repo_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let skill = tmp.path().join("skills").join("demo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&skill).unwrap();
        let root = std::fs::canonicalize(&repo).unwrap();
        let skill_root = std::fs::canonicalize(&skill).unwrap();
        let a = allow(&["docs/a.md"]);
        // Skill roots only widen access OUTSIDE the repo; the review-scope
        // allowlist still governs repo-internal reads.
        assert!(check_arguments(
            &root,
            Some(&a),
            std::slice::from_ref(&skill_root),
            "read_file",
            r#"{"file_path":"src/other.rs"}"#
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_roots_reject_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let skill = tmp.path().join("skills").join("demo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, skill.join("link")).unwrap();
        let root = std::fs::canonicalize(&repo).unwrap();
        let skill_root = std::fs::canonicalize(&skill).unwrap();
        let target = skill_root.join("link/secret.txt");
        let args = serde_json::json!({"file_path": target.to_string_lossy()}).to_string();
        assert!(check_arguments(
            &root,
            None,
            std::slice::from_ref(&skill_root),
            "read_file",
            &args,
        )
        .is_err());
    }

    #[test]
    fn with_read_roots_skips_nonexistent_dirs() {
        let mw = PathConfineMiddleware::new(root())
            .with_read_roots(&[PathBuf::from("/nonexistent/skill-dir")]);
        assert!(mw.read_roots.is_empty());
    }

    /// A misconfigured wide `--skill-dir` (`~`, `/tmp`, a repo root with no skills
    /// and no markdown) must not widen the read sandbox to an arbitrary subtree.
    #[test]
    fn with_read_roots_skips_dirs_without_skill_content() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("wide");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let mw = PathConfineMiddleware::new(root()).with_read_roots(&[dir]);
        assert!(mw.read_roots().is_empty());
    }

    /// `--skill-dir` may be the parent of several skill packages: each CHILD skill
    /// dir becomes a read root, but the parent itself does not (it may contain the
    /// reviewed repo, and opening it would bypass the review-scope allowlist).
    #[test]
    fn with_read_roots_parent_dir_opens_only_skill_children() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("skills");
        let skill = parent.join("demo");
        let nested_repo = parent.join("repo");
        let other_repo = tmp.path().join("other-repo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::create_dir_all(&nested_repo).unwrap();
        std::fs::create_dir_all(&other_repo).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: demo\n---\nbody").unwrap();
        std::fs::write(nested_repo.join("secret.txt"), "secret").unwrap();
        let mw = PathConfineMiddleware::new(root()).with_read_roots(&[parent.clone()]);
        assert_eq!(mw.read_roots(), &[std::fs::canonicalize(&skill).unwrap()]);

        // A sibling repo under the same parent is NOT readable through the skill
        // root: only the skill child dir was opened, not the parent subtree.
        let root = std::fs::canonicalize(&other_repo).unwrap();
        let target = std::fs::canonicalize(nested_repo.join("secret.txt")).unwrap();
        let args = serde_json::json!({"file_path": target.to_string_lossy()}).to_string();
        assert!(check_arguments(&root, None, mw.read_roots(), "read_file", &args).is_err());
    }

    /// Single-file skill layout: `<name>.md` directly in the `--skill-dir` (same
    /// contract as `SkillRegistry`), so the dir itself is the skill package.
    #[test]
    fn with_read_roots_accepts_flat_markdown_skill_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("flat-skills");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo.md"), "# demo skill").unwrap();
        let mw = PathConfineMiddleware::new(root()).with_read_roots(&[dir.clone()]);
        assert_eq!(mw.read_roots(), &[std::fs::canonicalize(&dir).unwrap()]);
    }
}
