//! `search_replace` — bulk find-and-replace across many files (literal or regex),
//! optionally scoped by a glob. Mutates the filesystem ⇒ always `Risky`. Neutral port of
//! the production tool, minus the coding bookkeeping (file_history / file_store / LSP) —
//! the L1 `ToolContext` has none of that. Reads, writes and compensation all go
//! through the same execution world's filesystem.

use super::{coerce_eol, err, ok, resolve_path};
use crate::world::{FileSystem, LocalFs};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct SearchReplaceTool {
    /// The tree being rewritten. Walk, read and write all go through it, so a
    /// fenced or read-only world refuses the write it would have refused for
    /// `write_file` — which is the reason this tool waited for `walk`: routing
    /// only its writes would have found matches outside the fence and then
    /// failed to apply some of them, a worse outcome than not routing at all.
    world: Arc<dyn FileSystem>,
    /// Where the product keeps its data: our own per-project dir is skipped
    /// in walks (plugin clones, artifacts, team worktrees).
    dirs: crate::ProductDirs,
}

#[cfg(test)]
impl Default for SearchReplaceTool {
    fn default() -> Self {
        Self::new(crate::product_dirs::test_dirs())
    }
}

impl SearchReplaceTool {
    /// Work on this machine's disk.
    pub fn new(dirs: crate::ProductDirs) -> Self {
        Self::with_world(Arc::new(LocalFs::unfenced()), dirs)
    }

    /// Rewrite files in `world` instead of on this machine's disk.
    pub fn with_world(world: Arc<dyn FileSystem>, dirs: crate::ProductDirs) -> Self {
        Self { world, dirs }
    }
}

#[derive(Deserialize)]
struct Args {
    search: String,
    replace: String,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    preview_token: Option<String>,
    #[serde(default)]
    expected_files: Option<usize>,
    #[serde(default)]
    expected_replacements: Option<usize>,
}

#[async_trait]
impl Tool for SearchReplaceTool {
    fn name(&self) -> &str {
        "search_replace"
    }
    fn description(&self) -> &str {
        "Find and replace text across MANY files at once — replaces every occurrence in \
         every matching file. Use for project-wide renames (a CSS class, an import, a \
         config key, a string literal). For a single file, prefer `edit_file`. \
         `regex:true` enables regex (with `$1`/`$2` capture groups in `replace`; write \
         `${1}` when the number is followed by name characters, e.g. `${1}_DONE` — \
         `$1_DONE` is read as the group name `1_DONE` and is now rejected); the \
         default is literal matching. `glob` limits scope (e.g. \"*.rs\", \"src/**/*.ts\"); \
         `path` sets the search root (default: working directory). Cross-file syntax deletion, or \
         changes exceeding 20 files or 1000 matches, return a PREVIEW without writing. \
         Inspect its counts and before/after samples; narrow an overbroad pattern rather than \
         blindly applying it. To apply the reviewed plan, repeat identical arguments with its \
         `preview_token`, `expected_files` and `expected_replacements` from the preview. \
         Any changed target file or scope invalidates the token. `dry_run:true` \
         previews any replacement. A failed batch attempts to restore its writes and reports \
         uncertain files. Inspect them before retrying. This is text matching, NOT syntax-aware editing: a regex \
         deleting empty parentheses also deletes function calls and declarations."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "search": { "type": "string", "description": "Text or regex pattern to find" },
                "replace": { "type": "string", "description": "Replacement text (use $1, $2 for regex captures; use ${1} when a digit is followed by name characters — $${1}_DONE, not $1_DONE)" },
                "glob": { "type": "string", "description": "File pattern to limit scope, e.g. \"*.rs\", \"src/**/*.ts\" (default: all files)" },
                "path": { "type": "string", "description": "Directory to search in (default: working directory)" },
                "regex": { "type": "boolean", "description": "Use regex matching (default: false = literal)" },
                "dry_run": { "type": "boolean", "description": "Preview affected files, counts and samples without writing (default: false)" },
                "preview_token": { "type": "string", "description": "Token from a reviewed preview; apply with identical search/replace/path/glob/regex and dry_run:false" },
                "expected_files": { "type": "integer", "minimum": 0, "description": "Exact expected number of changed files; mismatch refuses all writes. Required when applying a guarded preview." },
                "expected_replacements": { "type": "integer", "minimum": 0, "description": "Exact expected replacement count; mismatch refuses all writes. Required when applying a guarded preview." }
            },
            "required": ["search", "replace"]
        })
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky // mutates the filesystem
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        // Tool-wide: "总是 / Always" approves every search-replace this session (v1 parity).
        String::new()
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "search_replace: invalid arguments: {e}. Expected {{\"search\":..., \"replace\":...}}."
                ))
            }
        };
        if a.search.is_empty() {
            return err(
                "search_replace: search is empty — an empty pattern would corrupt every file."
                    .to_string(),
            );
        }
        if ctx.cancel.is_cancelled() {
            return err("search_replace: cancelled. No files written.");
        }
        let root = resolve_path(a.path.as_deref().unwrap_or("."), &ctx.working_dir);
        match self.world.info(&root).await {
            Ok(m) if m.exists => {}
            // Denied is not missing — see the same note in `read`.
            Err(e) if e.is_denied() => return err(format!("search_replace: {e}")),
            _ => {
                return err(format!(
                    "search_replace: directory not found: {}",
                    crate::pathnorm::to_display(&root)
                ))
            }
        }

        // Regex mode compiles the pattern; literal mode matches the raw string verbatim
        // (no regex, so `$1` in the replacement stays literal and an EOL mismatch can be
        // tolerated per-file — see sr_scan).
        let re = if a.regex {
            match regex::Regex::new(&a.search) {
                Ok(r) => Some(r),
                Err(e) => return err(format!("search_replace: invalid regex '{}': {e}", a.search)),
            }
        } else {
            None
        };
        // Refuse an unexpandable `$ref` BEFORE the walk touches anything: the
        // regex crate would otherwise expand it to the empty string.
        if let Some(re) = re.as_ref() {
            if let Err(e) = validate_replacement(re, &a.replace) {
                return err(e);
            }
        }
        let glob_filter = match a.glob.as_deref() {
            Some(p) => match FileGlob::new(p) {
                Ok(g) => Some(g),
                Err(e) => return err(format!("search_replace: invalid glob '{p}': {e}")),
            },
            None => None,
        };

        // Phase 1: walk + read + compute replacements, all through the world.
        let (modified, scanned) = match sr_scan(
            self.world.as_ref(),
            crate::pathutil::skip_dir_for(&self.dirs),
            &root,
            re.as_ref(),
            &a,
            glob_filter.as_ref(),
            &ctx.cancel,
        )
        .await
        {
            Ok(scan) if !ctx.cancel.is_cancelled() => scan,
            Ok(_) => return err("search_replace: cancelled. No files written."),
            Err(e) => return err(format!("search_replace: {e}")),
        };

        let total: usize = modified.iter().map(|edit| edit.count).sum();
        if !a.dry_run
            && (a.expected_files.is_some_and(|n| n != modified.len())
                || a.expected_replacements.is_some_and(|n| n != total))
        {
            return err(format!("search_replace: expected scope does not match: {} files, {total} replacements actually planned. No files written. Narrow the search or review a fresh dry_run preview.", modified.len()));
        }
        // Conservative bulk-operation heuristics, not syntax validation. Small
        // regex edits retain the direct path; cross-file punctuation removal
        // needs review even below the ordinary bulk-size thresholds.
        let needs_preview =
            modified.len() > 20 || total > 1000 || generic_syntax_deletion(&modified);
        if a.dry_run || needs_preview || a.preview_token.is_some() {
            let token = preview_token(&a, &root, &modified);
            if let Some(supplied) = a.preview_token.as_deref().filter(|_| !a.dry_run) {
                if supplied != token {
                    return err("search_replace: preview is stale or arguments changed. No files written. Run dry_run:true again and review the new plan.");
                }
            }
            if !modified.is_empty() && (a.dry_run || a.preview_token.is_none()) {
                return ok(preview_report(&a, &root, &modified, total, &token));
            }
        }

        if needs_preview && (a.expected_files.is_none() || a.expected_replacements.is_none()) {
            return err("search_replace: applying this preview requires expected_files and expected_replacements from the reviewed plan. No files written.");
        }

        if modified.is_empty() {
            return ok(format!(
                "No matches for '{}' in {} ({scanned} files scanned).",
                a.search,
                crate::pathnorm::to_display(&root)
            ));
        }
        let world = Arc::clone(&self.world);
        let cancel = ctx.cancel.child_token();
        let _stop_commit = cancel.clone().drop_guard();
        // The standalone kernel may drop execute on cancellation. Once a write
        // starts, keep polling it and its compensation to completion.
        let report = match tokio::spawn(async move {
            commit_edits(world.as_ref(), &modified, &cancel).await?;
            Ok::<_, String>(modified)
        })
        .await
        {
            Ok(Ok(edits)) => edits
                .iter()
                .map(|edit| format!("  {} ({} replacements)", crate::pathnorm::to_display(&edit.path), edit.count))
                .collect::<Vec<_>>(),
            Ok(Err(error)) => return err(error),
            Err(error) => return err(format!("search_replace: commit task failed ({error}); file states are unknown. Inspect the workspace before retrying.")),
        };
        ok(format!(
            "Replaced '{}' → '{}': {total} replacements across {} files.\n{}",
            a.search,
            a.replace,
            report.len(),
            report.join("\n")
        ))
    }
}

struct PlannedEdit {
    path: PathBuf,
    before: String,
    after: String,
    count: usize,
}

fn check_cancelled(cancel: &CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    Ok(())
}

async fn commit_edits(
    world: &dyn FileSystem,
    edits: &[PlannedEdit],
    cancel: &CancellationToken,
) -> Result<(), String> {
    // Verify every original before the first write and remember the world path
    // so a retargeted path cannot be used for compensation.
    let mut targets = Vec::with_capacity(edits.len());
    for edit in edits {
        let verified = async {
            check_cancelled(cancel)?;
            let target = world
                .canonicalize(&edit.path)
                .await
                .map_err(|e| e.to_string())?;
            verify_bytes(world, &edit.path, edit.before.as_bytes()).await?;
            Ok::<_, String>(target)
        }
        .await;
        targets.push(verified.map_err(|error| {
            format!(
                "search_replace: {}: {error}. No files written; preview again.",
                crate::pathnorm::to_display(&edit.path)
            )
        })?);
    }

    let mut attempted = 0;
    for (edit, target) in edits.iter().zip(&targets) {
        let result = async {
            let lock = super::edit::edit_path_lock(target);
            let _guard = tokio::select! {
                guard = lock.lock() => guard,
                _ = cancel.cancelled() => return Err("cancelled".into()),
            };
            verify_target(world, &edit.path, target).await?;
            verify_bytes(world, &edit.path, edit.before.as_bytes()).await?;
            check_cancelled(cancel)?;
            attempted += 1;
            world
                .write_text(&edit.path, &edit.after)
                .await
                .map_err(|e| format!("write failed: {e}"))?;
            verify_target(world, &edit.path, target).await?;
            verify_bytes(world, &edit.path, edit.after.as_bytes()).await?;
            check_cancelled(cancel)
        }
        .await;
        if let Err(error) = result {
            let mut restored_or_unchanged = Vec::new();
            let mut uncertain = Vec::new();
            for (written, target) in edits[..attempted].iter().zip(&targets).rev() {
                let lock = super::edit::edit_path_lock(target);
                let _guard = lock.lock().await;
                let path = crate::pathnorm::to_display(&written.path);
                if restore_original(world, written, target).await {
                    restored_or_unchanged.push(path);
                } else {
                    uncertain.push(path);
                }
            }
            return Err(format!(
                "search_replace: {}",
                json!({
                    "status": "failed",
                    "file": crate::pathnorm::to_display(&edit.path),
                    "error": error,
                    "restored_or_unchanged": restored_or_unchanged,
                    "uncertain": uncertain,
                    "not_attempted": edits[attempted..].iter().map(|e| crate::pathnorm::to_display(&e.path)).collect::<Vec<_>>(),
                    "retry": "Inspect uncertain files and review a fresh preview before retrying.",
                })
            ));
        }
    }
    Ok(())
}

async fn verify_target(world: &dyn FileSystem, path: &Path, target: &Path) -> Result<(), String> {
    let current = world.canonicalize(path).await.map_err(|e| e.to_string())?;
    if current != target {
        return Err("resolved target changed".into());
    }
    Ok(())
}

async fn verify_bytes(world: &dyn FileSystem, path: &Path, expected: &[u8]) -> Result<(), String> {
    let current = world.read_bytes(path).await.map_err(|e| e.to_string())?;
    if current != expected {
        return Err("file content differs from expected bytes".into());
    }
    Ok(())
}

async fn restore_original(world: &dyn FileSystem, edit: &PlannedEdit, target: &Path) -> bool {
    if verify_target(world, &edit.path, target).await.is_err() {
        return false;
    }
    match world.read_bytes(&edit.path).await {
        Ok(current) if current == edit.before.as_bytes() => true,
        Ok(current) if current == edit.after.as_bytes() => {
            world
                .write_bytes(&edit.path, edit.before.as_bytes())
                .await
                .is_ok()
                && verify_target(world, &edit.path, target).await.is_ok()
                && verify_bytes(world, &edit.path, edit.before.as_bytes())
                    .await
                    .is_ok()
        }
        _ => false,
    }
}

fn generic_syntax_deletion(edits: &[PlannedEdit]) -> bool {
    if edits.len() < 2 {
        return false;
    }
    let syntax = |ch: char| "()[]{};,:".contains(ch);
    edits.iter().any(|edit| {
        // Compare actual results, not the spelling of the regex or replacement:
        // `$1` can preserve a function name while deleting its parentheses too.
        let significant = |ch: &char| !ch.is_whitespace() && !syntax(*ch);
        edit.before
            .chars()
            .filter(significant)
            .eq(edit.after.chars().filter(significant))
            && edit.before.chars().filter(|ch| syntax(*ch)).count()
                > edit.after.chars().filter(|ch| syntax(*ch)).count()
    })
}

fn preview_token(args: &Args, root: &Path, edits: &[PlannedEdit]) -> String {
    let mut hash = Sha256::new();
    // Length-delimit fields so neither text nor path boundaries can alias.
    let mut field = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    field(root.as_os_str().as_encoded_bytes());
    field(args.search.as_bytes());
    field(args.replace.as_bytes());
    field(&[u8::from(args.regex)]);
    field(args.glob.as_deref().unwrap_or_default().as_bytes());
    for edit in edits {
        field(edit.path.as_os_str().as_encoded_bytes());
        field(edit.before.as_bytes());
        field(edit.after.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn preview_report(
    args: &Args,
    root: &Path,
    edits: &[PlannedEdit],
    total: usize,
    token: &str,
) -> String {
    let mut report = format!("PREVIEW ONLY — no files written. {total} replacements across {} files.\nReview the scope and samples; narrow the pattern if it touches unrelated code.\nApply the reviewed plan with identical arguments, dry_run:false and preview_token:\n{token}\nSamples (first change in up to 5 files; not the full diff):\n", edits.len());
    report.push_str(&format!(
        "Root: {}\nGlob: {}\nApply counts: expected_files={}, expected_replacements={total}\n",
        crate::pathnorm::to_display(root),
        args.glob.as_deref().unwrap_or("(all eligible files)"),
        edits.len()
    ));
    for edit in edits.iter().take(5) {
        let prefix = edit
            .before
            .chars()
            .zip(edit.after.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let sample = |text: &str| {
            text.chars()
                .skip(prefix.saturating_sub(60))
                .take(180)
                .collect::<String>()
        };
        report.push_str(&format!(
            "{} ({} replacements)\n  before: {:?}\n  after:  {:?}\n",
            crate::pathnorm::to_display(&edit.path),
            edit.count,
            sample(&edit.before),
            sample(&edit.after)
        ));
    }
    report
}

/// Walk + read + replace computation, through the world. Does NOT write — returns
/// the original, replacement and match count per changed file plus the count of files
/// scanned. `re.is_some()` ⇒ regex mode (capture-group `replace`); else literal mode
/// (verbatim `search`/`replace`, with per-file CRLF/LF tolerance).
async fn sr_scan(
    world: &dyn FileSystem,
    skip: crate::world::SkipDir,
    root: &Path,
    re: Option<&regex::Regex>,
    args: &Args,
    glob_filter: Option<&FileGlob>,
    cancel: &CancellationToken,
) -> Result<(Vec<PlannedEdit>, usize), crate::world::FsError> {
    let mut modified = Vec::new();
    let mut scanned = 0usize;
    // The caller discards the plan on cancellation, including a partial walk.
    for path in world.walk(root, &skip, cancel).await? {
        if cancel.is_cancelled() {
            break;
        }
        if let Some(g) = glob_filter {
            if !g.is_match(&path, root) {
                continue;
            }
        }
        let content = match world.read_text(&path).await {
            Ok(c) => c,
            Err(_) => continue, // skip binary / unreadable
        };
        scanned += 1;
        if let Some((new_content, count)) = replace_in(&content, re, &args.search, &args.replace) {
            modified.push(PlannedEdit {
                path,
                before: content,
                after: new_content,
                count,
            });
        }
    }
    modified.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((modified, scanned))
}

/// Capture-group references a replacement string makes, following the `regex`
/// crate's expansion rules: `$$` is a literal `$`, `${name}` is a braced
/// reference, and `$name` takes the longest run of `[0-9A-Za-z_]`.
///
/// Why this exists: `replace_all` treats `$1_DONE` as the group name `1_DONE`,
/// which does not exist, and expands the whole match to the EMPTY string —
/// silently emptying the file. Naming an unknown group is a caller mistake we
/// can detect before touching the disk.
fn dollar_refs(rep: &str) -> Vec<String> {
    let chars: Vec<char> = rep.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            i += 1;
            continue;
        }
        // `$$` → a literal dollar sign, not a reference.
        if i + 1 < chars.len() && chars[i + 1] == '$' {
            i += 2;
            continue;
        }
        // `${name}` — everything up to the closing brace.
        if i + 1 < chars.len() && chars[i + 1] == '{' {
            match chars[i + 2..].iter().position(|c| *c == '}') {
                Some(off) => {
                    out.push(chars[i + 2..i + 2 + off].iter().collect());
                    i = i + 3 + off;
                }
                None => i += 1,
            }
            continue;
        }
        // `$name` — longest run of name characters.
        let start = i + 1;
        let mut end = start;
        while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        if end > start {
            out.push(chars[start..end].iter().collect());
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Reject a replacement that references a capture group the pattern does not
/// have. Called once, before any file is read or written.
fn validate_replacement(re: &regex::Regex, replace: &str) -> Result<(), String> {
    let names: Vec<&str> = re.capture_names().flatten().collect();
    for group in dollar_refs(replace) {
        let known = match group.parse::<usize>() {
            Ok(idx) => idx < re.captures_len(),
            Err(_) => names.iter().any(|n| *n == group),
        };
        if !known {
            return Err(format!(
                "search_replace: replace refers to capture group '${group}', which this pattern does not have. \
                 Use ${{N}} when a digit is followed by name characters (e.g. ${{1}}_DONE, not $1_DONE)."
            ));
        }
    }
    Ok(())
}

/// The replacement for one file's content, or `None` when nothing changes. Pure, so
/// the world only ever sees a read and a write.
fn replace_in(
    content: &str,
    re: Option<&regex::Regex>,
    search: &str,
    replace: &str,
) -> Option<(String, usize)> {
    let (new_content, count) = match re {
        Some(re) => {
            if !re.is_match(content) {
                return None;
            }
            (
                re.replace_all(content, replace).to_string(),
                re.find_iter(content).count(),
            )
        }
        None => {
            // Literal mode. Match verbatim first; on a literal hit the search already
            // agrees with the file's bytes, so search/replace are used as-is. Only if
            // that fails do we coerce BOTH to THIS file's EOL — rescuing an LF-copied
            // multi-line search against a CRLF file without injecting mixed endings.
            // Plain string replace keeps `$1` etc. verbatim (no capture-group expansion).
            let literal = content.matches(search).count();
            let (needle, repl, count) = if literal > 0 {
                (search.to_string(), replace.to_string(), literal)
            } else {
                let file_eol = if content.contains("\r\n") {
                    "\r\n"
                } else {
                    "\n"
                };
                let n = coerce_eol(search, file_eol);
                let c = content.matches(&n).count();
                (n, coerce_eol(replace, file_eol), c)
            };
            if count == 0 {
                return None;
            }
            (content.replace(&needle, &repl), count)
        }
    };
    (new_content != content).then_some((new_content, count))
}

/// A file-scope glob. A pattern with a `/` matches against the path RELATIVE to the
/// search root (e.g. `src/**/*.ts`); a bare pattern matches the FILE NAME only (e.g.
/// `*.rs`).
struct FileGlob {
    has_path: bool,
    matcher: GlobMatcher,
}

impl FileGlob {
    fn new(pattern: &str) -> Result<Self, globset::Error> {
        let normalized = pattern.replace('\\', "/");
        Ok(Self {
            has_path: normalized.contains('/'),
            matcher: Glob::new(&normalized)?.compile_matcher(),
        })
    }

    fn is_match(&self, file_path: &Path, root: &Path) -> bool {
        if self.has_path {
            let rel = file_path.strip_prefix(root).unwrap_or(file_path);
            self.matcher
                .is_match(rel.to_string_lossy().replace('\\', "/"))
        } else {
            match file_path.file_name().and_then(|n| n.to_str()) {
                Some(name) => self.matcher.is_match(name),
                None => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{DirEntry, FileInfo, FsError};

    #[derive(Clone, Copy)]
    enum Fault {
        RefuseWrite,
        PartialWrite,
        EditEarlierFile,
        EditNextFile,
        CancelDuringWrite,
        PauseDuringWrite,
    }

    struct FaultWorld {
        inner: LocalFs,
        fault: Fault,
        cancel: CancellationToken,
        write_started: tokio::sync::Notify,
        resume_write: tokio::sync::Notify,
        restored: tokio::sync::Notify,
    }

    impl FaultWorld {
        fn new(root: &Path, fault: Fault, cancel: CancellationToken) -> Self {
            Self {
                inner: LocalFs::new(root),
                fault,
                cancel,
                write_started: tokio::sync::Notify::new(),
                resume_write: tokio::sync::Notify::new(),
                restored: tokio::sync::Notify::new(),
            }
        }
    }

    #[async_trait]
    impl FileSystem for FaultWorld {
        fn root(&self) -> PathBuf {
            self.inner.root()
        }
        fn describe(&self) -> String {
            "search_replace fault fixture".into()
        }
        async fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, FsError> {
            self.inner.read_bytes(path).await
        }
        async fn write_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
            if matches!(self.fault, Fault::PauseDuringWrite) && path.ends_with("a.txt") {
                if bytes == b"new" {
                    self.write_started.notify_one();
                    self.resume_write.notified().await;
                } else {
                    self.inner.write_bytes(path, bytes).await?;
                    self.restored.notify_one();
                    return Ok(());
                }
            }
            if bytes == b"new" {
                if path.ends_with("a.txt") && matches!(self.fault, Fault::EditNextFile) {
                    self.inner
                        .write_bytes(&path.with_file_name("b.txt"), b"external")
                        .await?;
                }
                if path.ends_with("b.txt") {
                    match self.fault {
                        Fault::RefuseWrite => return Err(FsError::io("second write failed")),
                        Fault::PartialWrite => {
                            self.inner.write_bytes(path, b"partial").await?;
                            return Err(FsError::io("second write failed"));
                        }
                        Fault::EditEarlierFile => {
                            self.inner
                                .write_bytes(&path.with_file_name("a.txt"), b"external")
                                .await?;
                            return Err(FsError::io("second write failed"));
                        }
                        Fault::CancelDuringWrite => self.cancel.cancel(),
                        Fault::EditNextFile | Fault::PauseDuringWrite => {}
                    }
                }
            }
            self.inner.write_bytes(path, bytes).await
        }
        async fn create_dir_all(&self, path: &Path) -> Result<(), FsError> {
            self.inner.create_dir_all(path).await
        }
        async fn list(&self, path: &Path) -> Result<Vec<DirEntry>, FsError> {
            self.inner.list(path).await
        }
        async fn info(&self, path: &Path) -> Result<FileInfo, FsError> {
            self.inner.info(path).await
        }
        async fn canonicalize(&self, path: &Path) -> Result<PathBuf, FsError> {
            self.inner.canonicalize(path).await
        }
    }

    async fn batch_with_fault(fault: Fault, files: &[&str]) -> (serde_json::Value, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            std::fs::write(dir.path().join(file), "old").unwrap();
        }
        let ctx = ctx(dir.path());
        let tool = SearchReplaceTool::with_world(
            Arc::new(FaultWorld::new(dir.path(), fault, ctx.cancel.clone())),
            crate::product_dirs::test_dirs(),
        );
        let result = tool
            .execute(r#"{"search":"old","replace":"new"}"#, &ctx)
            .await;
        assert!(result.is_error, "{}", result.content);
        let report =
            serde_json::from_str(result.content.strip_prefix("search_replace: ").unwrap()).unwrap();
        let contents = files
            .iter()
            .map(|file| std::fs::read_to_string(dir.path().join(file)).unwrap())
            .collect();
        (report, contents)
    }

    fn reported_files(report: &serde_json::Value, field: &str) -> Vec<String> {
        report[field]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| {
                path.as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_failed_write_restores_verified_files_and_reports_unattempted_targets() {
        let (report, contents) =
            batch_with_fault(Fault::RefuseWrite, &["a.txt", "b.txt", "c.txt"]).await;
        assert_eq!(contents, ["old", "old", "old"]);
        assert_eq!(
            reported_files(&report, "restored_or_unchanged"),
            ["b.txt", "a.txt"]
        );
        assert!(reported_files(&report, "uncertain").is_empty());
        assert_eq!(reported_files(&report, "not_attempted"), ["c.txt"]);
    }

    #[tokio::test]
    async fn a_partial_write_and_an_external_edit_are_left_for_review() {
        let files = ["a.txt", "b.txt", "c.txt"];
        let (report, contents) = batch_with_fault(Fault::PartialWrite, &files).await;
        assert_eq!(contents, ["old", "partial", "old"]);
        assert_eq!(reported_files(&report, "uncertain"), ["b.txt"]);
        assert_eq!(reported_files(&report, "restored_or_unchanged"), ["a.txt"]);

        let (report, contents) = batch_with_fault(Fault::EditEarlierFile, &files).await;
        assert_eq!(contents, ["external", "old", "old"]);
        assert_eq!(reported_files(&report, "uncertain"), ["a.txt"]);
        assert_eq!(reported_files(&report, "restored_or_unchanged"), ["b.txt"]);
    }

    #[tokio::test]
    async fn a_target_changed_after_preflight_is_not_written() {
        let (report, contents) =
            batch_with_fault(Fault::EditNextFile, &["a.txt", "b.txt", "c.txt"]).await;
        assert_eq!(contents, ["old", "external", "old"]);
        assert_eq!(reported_files(&report, "not_attempted"), ["b.txt", "c.txt"]);
        assert_eq!(reported_files(&report, "restored_or_unchanged"), ["a.txt"]);
    }

    #[tokio::test]
    async fn cancellation_during_the_last_write_restores_the_batch() {
        let (report, contents) =
            batch_with_fault(Fault::CancelDuringWrite, &["a.txt", "b.txt"]).await;
        assert_eq!(contents, ["old", "old"]);
        assert_eq!(report["error"], "cancelled");
        assert_eq!(
            reported_files(&report, "restored_or_unchanged"),
            ["b.txt", "a.txt"]
        );
        assert!(reported_files(&report, "not_attempted").is_empty());
    }

    #[tokio::test]
    async fn dropping_execute_during_a_write_still_restores_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "old").unwrap();
        std::fs::write(&b, "old").unwrap();
        let ctx = ctx(dir.path());
        let world = Arc::new(FaultWorld::new(
            dir.path(),
            Fault::PauseDuringWrite,
            ctx.cancel.clone(),
        ));
        let tool = SearchReplaceTool::with_world(world.clone(), crate::product_dirs::test_dirs());
        {
            let execution = tool.execute(r#"{"search":"old","replace":"new"}"#, &ctx);
            tokio::pin!(execution);
            tokio::select! {
                _ = world.write_started.notified() => {}
                result = &mut execution => panic!("finished before the write was resumed: {}", result.content),
            }
        }
        world.resume_write.notify_one();
        world.restored.notified().await;
        // Wait for the worker to finish verification before removing the fixture.
        let lock = super::super::edit::edit_path_lock(&a);
        let _guard = lock.lock().await;
        assert_eq!(std::fs::read_to_string(a).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(b).unwrap(), "old");
        assert!(!ctx.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn a_read_only_world_finds_the_matches_and_refuses_the_rewrite() {
        // The reason this tool waited for `walk`: with only its writes routed it
        // would find matches the world was never asked about and then fail to
        // apply them. Now the walk, the read and the refusal are all the world's.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "foo bar").unwrap();
        let tool = SearchReplaceTool::with_world(
            Arc::new(LocalFs::read_only(d.path())),
            crate::product_dirs::test_dirs(),
        );
        let r = tool
            .execute(r#"{"search":"foo","replace":"baz"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("read-only"), "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "foo bar",
            "the world refused; the file is untouched"
        );
    }

    #[tokio::test]
    async fn a_fenced_world_refuses_a_root_outside_it_before_walking() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("inside")).unwrap();
        std::fs::write(d.path().join("outside.txt"), "foo").unwrap();
        let tool = SearchReplaceTool::with_world(
            Arc::new(LocalFs::new(d.path().join("inside"))),
            crate::product_dirs::test_dirs(),
        );
        let r = tool
            .execute(
                r#"{"search":"foo","replace":"baz","path":".."}"#,
                &ctx(&d.path().join("inside")),
            )
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(
            r.content.contains("outside the world's root"),
            "{}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("outside.txt")).unwrap(),
            "foo"
        );
    }

    /// `$1_DONE` is read by the `regex` crate as the group name "1_DONE", which does
    /// not exist → the whole match expands to the empty string and the file is silently
    /// emptied. The tool must refuse instead of writing that.
    #[tokio::test]
    async fn regex_replacement_rejects_unknown_group_from_trailing_identifier() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("sr.txt");
        std::fs::write(&file, "SMOKE_OK\n").unwrap();
        let tool = SearchReplaceTool::with_world(
            Arc::new(LocalFs::new(d.path())),
            crate::product_dirs::test_dirs(),
        );
        let r = tool
            .execute(
                r#"{"search":"(SMOKE)_OK","replace":"$1_DONE","path":".","glob":"sr.txt","regex":true}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            r.is_error,
            "unknown group must be rejected, got: {}",
            r.content
        );
        assert!(
            r.content.contains("1_DONE"),
            "the error must name the bad reference, got: {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "SMOKE_OK\n",
            "the file must be untouched when the replacement is refused"
        );
    }

    /// The validator must ACCEPT the braced form — it is the documented way to put a
    /// group next to name characters, and a bug in the reference scanner would
    /// otherwise reject it.
    #[tokio::test]
    async fn regex_replacement_accepts_braced_group() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("sr.txt");
        std::fs::write(&file, "SMOKE_OK\n").unwrap();
        let tool = SearchReplaceTool::with_world(
            Arc::new(LocalFs::new(d.path())),
            crate::product_dirs::test_dirs(),
        );
        let r = apply_reviewed(&tool,
            r#"{"search":"(SMOKE)_OK","replace":"${1}_DONE","path":".","glob":"sr.txt","regex":true}"#,
            &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "SMOKE_DONE\n",
            "braced group expands to the captured text"
        );
    }

    async fn apply_reviewed(tool: &SearchReplaceTool, args: &str, ctx: &ToolContext) -> ToolResult {
        let mut args: serde_json::Value = serde_json::from_str(args).unwrap();
        args["dry_run"] = json!(true);
        let preview = tool.execute(&args.to_string(), ctx).await;
        assert!(!preview.is_error, "{}", preview.content);
        args["dry_run"] = json!(false);
        args["preview_token"] = json!(token_from(&preview));
        tool.execute(&args.to_string(), ctx).await
    }

    fn token_from(result: &ToolResult) -> &str {
        result
            .content
            .lines()
            .find(|line| line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit()))
            .expect("preview token")
    }

    #[tokio::test]
    async fn deleting_parentheses_across_a_project_previews_instead_of_corrupting_code() {
        let d = tempfile::tempdir().unwrap();
        let original = "fn new() { Vec::new() }\n";
        for n in 0..151 {
            std::fs::write(d.path().join(format!("{n}.rs")), original).unwrap();
        }
        let result = SearchReplaceTool::default()
            .execute(
                &json!({"search":r"\(\s*\)|\(\s*/[^()]*\)","replace":"","regex":true}).to_string(),
                &ctx(d.path()),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("PREVIEW ONLY"));
        assert!(
            result.content.contains("302 replacements across 151 files"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("fn new()"),
            "show the code being damaged: {}",
            result.content
        );
        for n in 0..151 {
            assert_eq!(
                std::fs::read_to_string(d.path().join(format!("{n}.rs"))).unwrap(),
                original
            );
        }
    }

    #[tokio::test]
    async fn cross_file_syntax_deletion_requires_review_and_exact_counts() {
        for (search, replace, regex) in [
            ("()", "", false),
            (r"\(\s*\)", "", true),
            (r"(\w+)\(\)", "$1", true),
        ] {
            let d = tempfile::tempdir().unwrap();
            for file in ["a.rs", "b.rs"] {
                std::fs::write(d.path().join(file), "fn new() { Vec::new() }").unwrap();
            }
            let tool = SearchReplaceTool::default();
            let mut args = json!({"search": search, "replace":replace, "regex":regex});
            let preview = tool.execute(&args.to_string(), &ctx(d.path())).await;
            assert!(
                preview.content.contains("PREVIEW ONLY"),
                "{}",
                preview.content
            );
            args["preview_token"] = json!(token_from(&preview));
            assert!(
                tool.execute(&args.to_string(), &ctx(d.path()))
                    .await
                    .is_error,
                "counts must be explicit"
            );
            args["expected_files"] = json!(2);
            args["expected_replacements"] = json!(3);
            let refused = tool.execute(&args.to_string(), &ctx(d.path())).await;
            assert!(refused.is_error, "incorrect match count must be refused");
            for file in ["a.rs", "b.rs"] {
                assert_eq!(
                    std::fs::read_to_string(d.path().join(file)).unwrap(),
                    "fn new() { Vec::new() }"
                );
            }
            args["expected_replacements"] = json!(4);
            let applied = tool.execute(&args.to_string(), &ctx(d.path())).await;
            assert!(!applied.is_error, "{}", applied.content);
            assert!(applied.content.contains("4 replacements across 2 files"));
        }
    }

    #[tokio::test]
    async fn small_regex_cleanup_does_not_require_a_preview() {
        let d = tempfile::tempdir().unwrap();
        for file in ["a.rs", "b.rs"] {
            std::fs::write(d.path().join(file), "fn new() {}  \n").unwrap();
        }
        let result = SearchReplaceTool::default().execute(
            &json!({"search":r"(?m) +$", "replace":"", "regex":true, "glob":"*.rs", "expected_files":2, "expected_replacements":2}).to_string(), &ctx(d.path())
        ).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.starts_with("Replaced"), "{}", result.content);
        for file in ["a.rs", "b.rs"] {
            assert_eq!(
                std::fs::read_to_string(d.path().join(file)).unwrap(),
                "fn new() {}\n"
            );
        }
    }

    #[tokio::test]
    async fn broad_literal_replacements_require_a_preview_too() {
        for (files, content) in [(21, "foo".to_owned()), (1, "foo ".repeat(1001))] {
            let d = tempfile::tempdir().unwrap();
            for n in 0..files {
                std::fs::write(d.path().join(format!("{n}.txt")), &content).unwrap();
            }
            let result = SearchReplaceTool::default()
                .execute(r#"{"search":"foo","replace":"bar"}"#, &ctx(d.path()))
                .await;
            assert!(
                result.content.contains("PREVIEW ONLY"),
                "{}",
                result.content
            );
            for n in 0..files {
                assert_eq!(
                    std::fs::read_to_string(d.path().join(format!("{n}.txt"))).unwrap(),
                    content
                );
            }
        }
    }

    #[tokio::test]
    async fn stale_previews_refuse_every_write_including_newly_matching_files() {
        for change in ["content", "new_file", "arguments", "all_matches_removed"] {
            let d = tempfile::tempdir().unwrap();
            let file = d.path().join("a.txt");
            std::fs::write(&file, "foo").unwrap();
            let tool = SearchReplaceTool::default();
            let mut args = json!({"search":"foo","replace":"bar","dry_run":true});
            let preview = tool.execute(&args.to_string(), &ctx(d.path())).await;
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "foo");
            args["preview_token"] = json!(token_from(&preview));
            args["dry_run"] = json!(false);
            match change {
                "content" => std::fs::write(&file, "foo changed").unwrap(),
                "new_file" => std::fs::write(d.path().join("b.txt"), "foo").unwrap(),
                "all_matches_removed" => std::fs::write(&file, "unrelated content").unwrap(),
                _ => args["replace"] = json!("baz"),
            }
            let before = std::fs::read_to_string(&file).unwrap();
            let result = tool.execute(&args.to_string(), &ctx(d.path())).await;
            assert!(result.is_error, "{change}: {}", result.content);
            assert!(
                result.content.contains("No files written"),
                "{}",
                result.content
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
            // Refresh must work with stale apply parameters still attached.
            args["dry_run"] = json!(true);
            args["expected_files"] = json!(999);
            args["expected_replacements"] = json!(999);
            let refreshed = tool.execute(&args.to_string(), &ctx(d.path())).await;
            assert!(!refreshed.is_error, "{change}: {}", refreshed.content);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
            if change == "new_file" {
                assert_eq!(
                    std::fs::read_to_string(d.path().join("b.txt")).unwrap(),
                    "foo"
                );
            }
        }
    }

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn literal_replace_across_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "foo bar foo").unwrap();
        std::fs::write(d.path().join("b.txt"), "no match here").unwrap();
        std::fs::write(d.path().join("c.txt"), "foo").unwrap();
        let r = SearchReplaceTool::default()
            .execute(r#"{"search":"foo","replace":"baz"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("3 replacements across 2 files"),
            "{}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "baz bar baz"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("c.txt")).unwrap(),
            "baz"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("b.txt")).unwrap(),
            "no match here"
        );
    }

    #[tokio::test]
    async fn glob_scopes_by_extension() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("keep.md"), "color").unwrap();
        std::fs::write(d.path().join("x.css"), "color").unwrap();
        let r = SearchReplaceTool::default()
            .execute(
                r#"{"search":"color","replace":"colour","glob":"*.css"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("x.css")).unwrap(),
            "colour"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("keep.md")).unwrap(),
            "color",
            "md untouched"
        );
    }

    #[tokio::test]
    async fn literal_multiline_matches_crlf_file_and_preserves_crlf() {
        // A literal multi-line search whose break is `\n` (what read_file shows the
        // model) must still match a CRLF file, and the file must stay CRLF.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "alpha\r\nbeta\r\n").unwrap();
        let r = SearchReplaceTool::default()
            .execute(
                r#"{"search":"alpha\nbeta","replace":"ALPHA\nbeta"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "ALPHA\r\nbeta\r\n"
        );
    }

    #[tokio::test]
    async fn literal_match_writes_verbatim_no_crlf_injection() {
        // Mostly-LF file with one stray CRLF line. A literal multi-line replace of an LF
        // region must match it verbatim and NOT force the result to CRLF.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("m.txt"), "head\r\nfoo\nbar\n").unwrap();
        let r = SearchReplaceTool::default()
            .execute(
                r#"{"search":"foo\nbar","replace":"foo\nBAR"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("m.txt")).unwrap(),
            "head\r\nfoo\nBAR\n"
        );
    }

    #[tokio::test]
    async fn empty_search_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "abc").unwrap();
        let r = SearchReplaceTool::default()
            .execute(r#"{"search":"","replace":"X"}"#, &ctx(d.path()))
            .await;
        assert!(
            r.is_error,
            "empty search must be refused (would corrupt every file): {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "abc",
            "unchanged"
        );
    }

    #[tokio::test]
    async fn literal_replace_does_not_expand_dollar_groups() {
        // Literal mode must treat `$1` in the replacement verbatim (not a capture ref).
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "key=val").unwrap();
        let r = SearchReplaceTool::default()
            .execute(r#"{"search":"val","replace":"$1x"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "key=$1x"
        );
    }

    #[tokio::test]
    async fn regex_with_capture_groups() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("v.rs"), "let v1 = 1; let v2 = 2;").unwrap();
        let r = apply_reviewed(
            &SearchReplaceTool::default(),
            r#"{"search":"v(\\d)","replace":"w$1","regex":true}"#,
            &ctx(d.path()),
        )
        .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("v.rs")).unwrap(),
            "let w1 = 1; let w2 = 2;"
        );
    }

    #[tokio::test]
    async fn literal_does_not_treat_search_as_regex() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "a.b a_b axb").unwrap();
        // "a.b" literal must match only "a.b", not "axb" (which `.` would match in regex).
        let r = SearchReplaceTool::default()
            .execute(r#"{"search":"a.b","replace":"Z"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "Z a_b axb"
        );
    }

    #[tokio::test]
    async fn no_matches_reports_and_is_not_error() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "nothing").unwrap();
        let r = SearchReplaceTool::default()
            .execute(r#"{"search":"zzz","replace":"x"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("No matches"), "{}", r.content);
    }

    #[tokio::test]
    async fn invalid_regex_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = SearchReplaceTool::default()
            .execute(
                r#"{"search":"(unclosed","replace":"x","regex":true}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("invalid regex"), "{}", r.content);
    }

    #[tokio::test]
    async fn skips_skip_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/gen.rs"), "foo").unwrap();
        std::fs::write(d.path().join("src.rs"), "foo").unwrap();
        let r = SearchReplaceTool::default()
            .execute(r#"{"search":"foo","replace":"bar"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("src.rs")).unwrap(),
            "bar"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("target/gen.rs")).unwrap(),
            "foo",
            "target/ skipped"
        );
    }

    #[test]
    fn risk_is_risky() {
        assert_eq!(SearchReplaceTool::default().risk("{}"), RiskLevel::Risky);
    }
}
