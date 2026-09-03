//! `BashWorkspaceGate` — workspace-aware approval for DESTRUCTIVE bash commands whose target
//! lands OUTSIDE the workspace, mirroring [`WriteApprovalGate`](super::write_approval) for the
//! file-mutation tools.
//!
//! ## Why
//!
//! `BashTool::risk()` classifies purely by COMMAND PATTERN: a `rm` is `Risky` only when
//! RECURSIVE (`rm -r`). A single-file `rm <path>` is `Safe`, and `Safe` tools bypass
//! [`ApprovalMiddleware`](super::approval::ApprovalMiddleware) in EVERY mode — so
//! `rm ~/Downloads/x.txt` (a delete OUTSIDE the workspace) runs with no prompt. Claude Code
//! prompts for out-of-workspace access even while auto-accepting edits, offering a
//! per-directory "always allow". This gate restores that: a destructive bash op targeting a
//! path outside the workspace prompts, with a directory-scoped "Always".
//!
//! ## Policy (mirror of `WriteApprovalGate`)
//!
//! | bash target | behavior |
//! |---|---|
//! | not a destructive command | `Proceed` (defer to the normal risk flow) |
//! | destructive, all targets IN workspace | `Proceed` (unchanged: single-file rm stays Safe→runs; recursive rm still Risky→ApprovalMiddleware) |
//! | destructive, target is SENSITIVE (any location) | prompt EVERY time, never remembered |
//! | destructive, target OUT of workspace | prompt; "Always" grants PER canonical directory |
//! | destructive but targets UNRESOLVABLE (`cd`-then-relative, `$()`, unexpanded `$var`, no clean target) | prompt EVERY time, never remembered (fail-closed) |
//!
//! Mode independence: this gate is about WORKSPACE SCOPE, not edit-accept, so it fires
//! regardless of the accept-edits flag. In full `Auto` (免审批) mode the driver auto-answers
//! the prompt, so `Auto` bypasses it with no special-casing.
//!
//! **Register it BEFORE [`ApprovalMiddleware`](super::approval::ApprovalMiddleware)** (after
//! `WriteApprovalGate`) so its `Allow` short-circuits the generic prompt, and its `Proceed`
//! lets in-workspace Risky commands (recursive rm) still reach `ApprovalMiddleware` unchanged.
//!
//! Anything this gate misclassifies as `NotDestructive` still faces `ApprovalMiddleware`'s
//! existing Risky rules (recursive rm, sudo, eval, pipe-to-shell, `xargs rm`, `find -delete`),
//! so the fail-open surface is bounded to un-wrapped single-target commands, which the scanner
//! parses precisely.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall};
use serde::Deserialize;

use super::approval::{
    ApprovalRequest, InMemoryPermissionStore, PermissionDecision, PermissionStore, APPROVAL_KIND,
};
use super::resolve_path;
use super::sensitive_path::path_is_sensitive;
use super::write_approval::{canonical_dir_key, path_in_temp_dir, path_in_workspace};

/// A token that contains an unexpanded shell expansion (`$var`, `$(...)`, backtick) whose value —
/// and thus whether it escapes the workspace — cannot be known statically.
fn has_dynamic_token(t: &str) -> bool {
    t.contains('$') || t.contains('`')
}

/// Filesystem-destructive commands that take a path operand. `dd` writes only its `of=` operand;
/// `cp` writes only its LAST (dest) operand — both handled specially in [`extract_targets`].
const DESTRUCTIVE_CMDS: &[&str] = &[
    "rm", "rmdir", "unlink", "mv", "cp", "shred", "truncate", "dd",
];

/// Leading wrapper commands to skip when finding a segment's effective command. Conservative:
/// unknown wrappers just mean we don't recognize the destructive command (→ `Proceed`, and
/// `ApprovalMiddleware` still catches the wrapped-Risky forms like `sudo`).
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "timeout", "nice", "ionice", "stdbuf", "time", "command", "builtin",
];

/// Redirect targets that are not real file writes (fd dups / the null device).
fn is_noop_redirect_target(t: &str) -> bool {
    t.starts_with('&') || matches!(t, "/dev/null" | "/dev/stdout" | "/dev/stderr")
}

/// A destructive/redirect TARGET whose location does NOT depend on the cwd: an absolute path
/// (`/…`) or a `~`-home path. A preceding `cd` cannot move such a target, so it stays statically
/// classifiable even when the same command also runs a `cd` — only RELATIVE targets are made
/// ambiguous by a `cd` (see [`scan_destructive_bash`]).
fn target_is_cwd_independent(t: &str) -> bool {
    let t = strip_quotes(t.trim());
    t.starts_with('/') || t == "~" || t.starts_with("~/")
}

/// Result of statically scanning a bash command line for filesystem-destructive operations.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BashScan {
    /// No destructive command / redirect found — not our concern.
    NotDestructive,
    /// Destructive op(s) with confidently-extracted, non-empty target path strings.
    Targets(Vec<String>),
    /// A destructive op is present but its target(s) cannot be resolved statically — fail closed.
    Unresolvable,
}

/// The `command` string from a bash tool call's args (`{"command": "...", ...}`).
fn bash_command(args: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct A {
        command: String,
    }
    serde_json::from_str::<A>(args).ok().map(|a| a.command)
}

fn base(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Strip a single layer of matching surrounding quotes.
fn strip_quotes(t: &str) -> &str {
    let b = t.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Quote-aware split of a command line into pipeline/list segments on unquoted `; && || | \n`.
/// A `|` immediately following an unquoted `>` is the noclobber-override redirect `>|`, NOT a
/// pipeline separator — it is dropped so the redirect target stays in the same segment.
fn split_segments(cmd: &str) -> Vec<String> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut prev_nonspace: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            cur.push(c);
            // A backslash can protect a double quote inside a double-quoted token. Preserve the
            // pair verbatim, but do not let the escaped quote terminate quote mode and expose a
            // following literal `>` as a redirect. Backslashes are literal inside single quotes.
            if q == '"' && c == '\\' && i + 1 < chars.len() {
                cur.push(chars[i + 1]);
                prev_nonspace = Some(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            prev_nonspace = Some(c);
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                cur.push(c);
                quote = Some(c);
                prev_nonspace = Some(c);
                i += 1;
            }
            '\\' if i + 1 < chars.len() => {
                // Preserve the pair for `tokenize`, while ensuring an escaped `;`, `&`, or `|`
                // remains ordinary word content rather than splitting the command.
                cur.push(c);
                cur.push(chars[i + 1]);
                prev_nonspace = Some(chars[i + 1]);
                i += 2;
            }
            ';' | '\n' => {
                segs.push(std::mem::take(&mut cur));
                prev_nonspace = None;
                i += 1;
            }
            '&' if i + 1 < chars.len() && chars[i + 1] == '&' => {
                segs.push(std::mem::take(&mut cur));
                prev_nonspace = None;
                i += 2;
            }
            '|' if i + 1 < chars.len() && chars[i + 1] == '|' => {
                segs.push(std::mem::take(&mut cur));
                prev_nonspace = None;
                i += 2;
            }
            '|' if prev_nonspace == Some('>') => {
                // `>|` — noclobber-override write; the `|` belongs to the redirect, not a pipe.
                prev_nonspace = Some('|');
                i += 1;
            }
            '|' => {
                segs.push(std::mem::take(&mut cur));
                prev_nonspace = None;
                i += 1;
            }
            c if c.is_whitespace() => {
                cur.push(c);
                i += 1; // whitespace does not change prev_nonspace
            }
            _ => {
                cur.push(c);
                prev_nonspace = Some(c);
                i += 1;
            }
        }
    }
    segs.push(cur);
    segs
}

/// Quote-aware tokenize of a single segment. Redirect operators become standalone tokens
/// (`">"` / `">>"`) so [`redirect_targets`] and arg extraction can find them; a leading fd digit
/// (`2>`) is absorbed into the operator (stderr-to-file is still a file write).
fn tokenize(seg: &str) -> Vec<String> {
    let chars: Vec<char> = seg.chars().collect();
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            cur.push(c);
            if q == '"' && c == '\\' && i + 1 < chars.len() {
                cur.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                cur.push(c);
                quote = Some(c);
                i += 1;
            }
            '\\' if i + 1 < chars.len() => {
                // Outside quotes, bash uses `\` to make the next character literal. Append the
                // escaped character to the current word so `\>` is not tokenized as a redirect;
                // this also keeps escaped spaces within the same path token.
                cur.push(chars[i + 1]);
                i += 2;
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
                i += 1;
            }
            '>' => {
                // A pure-digit accumulator is an fd prefix (`2>`); fold it into the operator.
                if !cur.is_empty() {
                    if cur.chars().all(|d| d.is_ascii_digit()) {
                        cur.clear();
                    } else {
                        toks.push(std::mem::take(&mut cur));
                    }
                }
                if i + 1 < chars.len() && chars[i + 1] == '>' {
                    toks.push(">>".to_string());
                    i += 2;
                } else {
                    toks.push(">".to_string());
                    i += 1;
                }
            }
            '<' => {
                if !cur.is_empty() {
                    if cur.chars().all(|d| d.is_ascii_digit()) {
                        cur.clear();
                    } else {
                        toks.push(std::mem::take(&mut cur));
                    }
                }
                toks.push("<".to_string());
                i += 1;
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
}

/// File targets written by `>`/`>>` redirects in a token stream (fd dups / `/dev/null` skipped).
/// Returns `Err(())` if a redirect target is an unexpanded `$var` (→ unresolvable).
fn redirect_targets(toks: &[String]) -> Result<Vec<String>, ()> {
    let mut out = Vec::new();
    // `>` is an operator inside `[[ ... ]]` and `(( ... ))`, but a redirect after the closing
    // token is ordinary shell syntax (`[[ ... ]] >/path`). Start scanning immediately after the
    // expression rather than exempting the whole command segment.
    let mut i = match effective_command_index(toks) {
        Some(ci) => {
            let closing = match command_word(&toks[ci]) {
                "[[" => Some("]]"),
                "((" => Some("))"),
                _ => None,
            };
            match closing {
                Some(closing) => {
                    let Some(relative_end) = toks[ci + 1..]
                        .iter()
                        .position(|t| strip_quotes(t) == closing)
                    else {
                        // An unclosed expression is a shell syntax error and executes no write.
                        return Ok(out);
                    };
                    ci + relative_end + 2
                }
                None => 0,
            }
        }
        None => 0,
    };
    while i < toks.len() {
        if toks[i] == ">" || toks[i] == ">>" {
            if let Some(raw) = toks.get(i + 1) {
                let t = strip_quotes(raw);
                if has_dynamic_token(t) {
                    return Err(());
                }
                if !t.is_empty() && !is_noop_redirect_target(t) {
                    out.push(t.to_string());
                }
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    Ok(out)
}

/// Parse every file target written by shell redirection in `command`.
///
/// This is shared with `BashTool::risk()` so the advisory risk classifier and this enforcing
/// workspace gate cannot disagree about whitespace-free redirects (`>/etc/passwd`), fd-prefixed
/// redirects (`2>/etc/passwd`), quoted targets, or redirects embedded in compound commands.
/// `Err(())` means a redirect target is dynamic and therefore cannot be classified statically.
pub(crate) fn scan_redirect_writes(command: &str) -> Result<Vec<String>, ()> {
    let joined = command.replace("\\\r\n", "").replace("\\\n", "");
    let mut targets = Vec::new();
    for seg in split_segments(joined.trim()) {
        let toks = tokenize(seg.trim());
        if toks.is_empty() {
            continue;
        }
        targets.extend(redirect_targets(&toks)?);
    }
    Ok(targets)
}

/// The index of a segment's effective command token, skipping leading wrappers, flags, and
/// `VAR=val` assignments. `None` if the segment is only wrappers/flags (no command).
fn effective_command_index(toks: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        let b = base(strip_quotes(t));
        if t.starts_with('-') || t.contains('=') {
            i += 1;
            continue;
        }
        if WRAPPERS.contains(&b) {
            i += 1;
            // `timeout`/`stdbuf` take a mandatory operand (the duration / buffer spec) that is
            // not the command — skip one non-flag token so it isn't mistaken for the command.
            if matches!(b, "timeout" | "stdbuf" | "ionice") {
                while i < toks.len() && (toks[i].starts_with('-') || toks[i].contains('=')) {
                    i += 1;
                }
                i += 1; // the operand
            }
            continue;
        }
        return Some(i);
    }
    None
}

/// Parse a destructive command's args into `(positional path operands, explicit -t dest)`,
/// stopping at a redirect operator. Recognizes GNU `-t DEST` / `-tDEST` / `--target-directory DEST`
/// / `--target-directory=DEST` (a written destination that would otherwise be dropped as a flag,
/// smuggling an out-of-workspace path past the gate). `None` if any collected path is an
/// unresolvable expansion (`$`/backtick).
fn parse_path_operands(args: &[String]) -> Option<(Vec<String>, Option<String>)> {
    let mut positionals = Vec::new();
    let mut target_dir: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let raw = &args[i];
        if raw == ">" || raw == ">>" || raw == "<" {
            break;
        }
        let a = strip_quotes(raw);
        if a == "-t" || a == "--target-directory" {
            if let Some(v) = args.get(i + 1) {
                let v = strip_quotes(v);
                if has_dynamic_token(v) {
                    return None;
                }
                if !v.is_empty() {
                    target_dir = Some(v.to_string());
                }
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(v) = a.strip_prefix("--target-directory=") {
            if has_dynamic_token(v) {
                return None;
            }
            if !v.is_empty() {
                target_dir = Some(v.to_string());
            }
            i += 1;
            continue;
        }
        if !a.starts_with("--") && a.starts_with("-t") && a.len() > 2 {
            let v = &a[2..];
            if has_dynamic_token(v) {
                return None;
            }
            target_dir = Some(v.to_string());
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        if has_dynamic_token(a) {
            return None;
        }
        if !a.is_empty() {
            positionals.push(a.to_string());
        }
        i += 1;
    }
    Some((positionals, target_dir))
}

/// Extract the destructive target paths from a recognized command's args (the tokens AFTER the
/// command). `Some(vec![])` = recognized but writes no file path (e.g. `dd` without `of=`,
/// `rm` with only flags) → treat as non-destructive. `None` = destructive but a target is
/// unresolvable (unexpanded `$var`/`$(...)`/backtick).
fn extract_targets(cmd: &str, args: &[String]) -> Option<Vec<String>> {
    if cmd == "dd" {
        // Only `of=<path>` is written; `if=` is a read.
        for a in args {
            let a = strip_quotes(a);
            if let Some(of) = a.strip_prefix("of=") {
                if has_dynamic_token(of) {
                    return None;
                }
                return Some(if of.is_empty() {
                    vec![]
                } else {
                    vec![of.to_string()]
                });
            }
        }
        return Some(vec![]); // no of= → writes stdout, not a file
    }

    let (positionals, target_dir) = parse_path_operands(args)?;
    match cmd {
        "cp" => {
            // Only the destination is written (sources are reads): the explicit `-t` dir if
            // present, else the LAST positional operand.
            if let Some(d) = target_dir {
                Some(vec![d])
            } else {
                Some(positionals.last().cloned().into_iter().collect())
            }
        }
        // rm/rmdir/unlink/mv/shred/truncate: every operand is mutated; an explicit `-t` dest
        // (mv) is also written.
        _ => {
            let mut targets = positionals;
            if let Some(d) = target_dir {
                targets.push(d);
            }
            Some(targets)
        }
    }
}

/// The COMMAND word, normalized for classification: path basename, quotes stripped, and a leading
/// `\` removed. Bash's `\cmd` bypasses aliases/functions but still runs the REAL command, so
/// `\rm` / `\mv` must be gated exactly like `rm` / `mv` — without this strip they'd be classified
/// as an unknown command and slip through as non-destructive.
fn command_word(tok: &str) -> &str {
    base(strip_quotes(tok)).trim_start_matches('\\')
}

/// True when any pipeline/list segment of `command` invokes `git <subcommand>` — i.e. the
/// segment's effective command (after leading env-assignments and wrappers like
/// `env`/`sudo`/`timeout`) is `git` and its subcommand equals `subcommand`.
///
/// Reuses the same quote- and compound-command-aware parser as the destructive-fs scan, so a
/// chained `cd ~/r && git add -A && GIT_SSH_COMMAND=... git push origin main` is recognized while
/// `echo git push` and a quoted `"... git push ..."` argument (e.g. a commit message) are not.
/// Git's own global options that take a separate value (`-c key=val`, `-C dir`, `--git-dir dir`,
/// `--work-tree dir`, `--namespace name`, `--exec-path path`) are skipped so they don't shadow the
/// subcommand. Best-effort: an exotic `--global-opt value` form not in that set may mis-skip, which
/// only means the label ensure is not triggered for that rare shape.
///
/// Gated on `atomgit`: the sole consumer is the post-push project-label middleware.
#[cfg(feature = "atomgit")]
pub(crate) fn command_invokes_git_subcommand(command: &str, subcommand: &str) -> bool {
    // Bash removes `\<newline>` line continuations entirely; mirror that before splitting.
    let joined = command.replace("\\\r\n", "").replace("\\\n", "");
    for seg in split_segments(joined.trim()) {
        let toks = tokenize(seg.trim());
        let Some(ci) = effective_command_index(&toks) else {
            continue;
        };
        if command_word(&toks[ci]) != "git" {
            continue;
        }
        if git_subcommand(&toks[ci + 1..]) == Some(subcommand) {
            return true;
        }
    }
    false
}

/// The git subcommand from the tokens AFTER the `git` word, skipping global options. Short options
/// that consume a following value (`-c`/`-C`) and the long `--x value` forms git accepts are
/// skipped; every other `-`/`--` token is treated as a single self-contained flag.
#[cfg(feature = "atomgit")]
fn git_subcommand(args: &[String]) -> Option<&str> {
    const VALUE_OPTS: &[&str] = &[
        "-c",
        "-C",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--exec-path",
    ];
    let mut i = 0;
    while i < args.len() {
        let t = strip_quotes(&args[i]);
        if VALUE_OPTS.contains(&t) {
            i += 2; // option + its value
            continue;
        }
        if t.starts_with('-') {
            i += 1; // `--flag` / `--flag=val` / bundled short flag: single token
            continue;
        }
        return Some(t);
    }
    None
}

/// For each `mv` in the line, the `(source, dest)` pairs it performs — `mv A B C DEST`
/// (or `mv -t DEST A B C`) → `[(A,DEST), (B,DEST), (C,DEST)]`.
///
/// Used to catch a MOVE that removes an in-workspace file by relocating it OUT of the workspace
/// (temp included). The target-location check alone misses this: `mv ws_file /tmp/x` has a
/// temp/absolute dest that counts as an acceptable WRITE location, yet the SOURCE has left the
/// workspace — an equivalent delete. `cp` is excluded: it copies, so the source stays put.
/// Sources/dests with a dynamic token are already fail-closed by `scan_destructive_bash`.
pub(crate) fn mv_moves(command: &str) -> Vec<(String, String)> {
    let joined = command.replace("\\\r\n", "").replace("\\\n", "");
    let mut out = Vec::new();
    for seg in split_segments(joined.trim()) {
        let toks = tokenize(seg.trim());
        let Some(ci) = effective_command_index(&toks) else {
            continue;
        };
        if command_word(&toks[ci]) != "mv" {
            continue;
        }
        let Some((positionals, target_dir)) = parse_path_operands(&toks[ci + 1..]) else {
            continue;
        };
        let (sources, dest) = match target_dir {
            // `mv -t DEST A B` — every positional is a source; DEST is the destination dir.
            Some(d) => (positionals, d),
            // `mv A B ... DEST` — last positional is the destination, the rest are sources.
            None if positionals.len() >= 2 => {
                let dest = positionals[positionals.len() - 1].clone();
                (positionals[..positionals.len() - 1].to_vec(), dest)
            }
            None => continue, // `mv` with < 2 operands and no `-t` does nothing well-formed
        };
        for src in sources {
            out.push((src, dest.clone()));
        }
    }
    out
}

/// Statically scan a bash command line for filesystem-destructive operations. See [`BashScan`].
pub(crate) fn scan_destructive_bash(command: &str) -> BashScan {
    // Bash removes `\<newline>` line continuations entirely (joining the tokens). Do the same so a
    // wrapped `rm \⏎/outside/x` isn't split into a commandless segment with an orphaned target.
    let joined = command.replace("\\\r\n", "").replace("\\\n", "");
    let cmd = joined.trim();
    if cmd.is_empty() {
        return BashScan::NotDestructive;
    }

    let (mut targets, mut any_destructive, mut unresolvable) = match scan_redirect_writes(cmd) {
        Ok(targets) => {
            let any = !targets.is_empty();
            (targets, any, false)
        }
        Err(()) => (Vec::new(), true, true),
    };
    let mut saw_cd = false;

    for seg in split_segments(cmd) {
        let toks = tokenize(seg.trim());
        if toks.is_empty() {
            continue;
        }
        let ci = effective_command_index(&toks);
        let effcmd: Option<&str> = ci.map(|i| command_word(&toks[i]));

        if let Some(ci) = ci {
            let effcmd = effcmd.unwrap();
            // A `cd` moves the resolution base for later RELATIVE paths. Record it and fail
            // closed only if a cwd-DEPENDENT (relative) target is actually present (checked after
            // the scan) — an absolute / `~` target lands the same place regardless of the cd, so
            // it stays classifiable (e.g. `cd ws && cargo build > /tmp/log` is not ambiguous).
            if effcmd == "cd" {
                saw_cd = true;
            } else if DESTRUCTIVE_CMDS.contains(&effcmd) {
                match extract_targets(effcmd, &toks[ci + 1..]) {
                    Some(ts) if ts.is_empty() => {} // recognized but writes no path → not destructive
                    Some(mut ts) => {
                        any_destructive = true;
                        targets.append(&mut ts);
                    }
                    None => {
                        any_destructive = true;
                        unresolvable = true;
                    }
                }
            }
        }
    }

    // A `cd` in the command makes only RELATIVE targets unresolvable (their base moved);
    // absolute / `~` targets remain statically classifiable.
    if saw_cd && targets.iter().any(|t| !target_is_cwd_independent(t)) {
        unresolvable = true;
    }

    if !any_destructive {
        return BashScan::NotDestructive;
    }
    if unresolvable || targets.is_empty() {
        return BashScan::Unresolvable;
    }
    BashScan::Targets(targets)
}

/// Workspace-aware approval gate for destructive bash commands. Clone-cheap (Arc-backed store +
/// cwd handle).
pub struct BashWorkspaceGate {
    store: Arc<dyn PermissionStore>,
    /// LIVE working dir — the same handle the kernel reads per call, so a `/cd` moves the
    /// in-workspace boundary with it. Grant keys are canonicalized to ABSOLUTE paths, so a
    /// remembered out-of-workspace grant survives a `/cd`.
    cwd: Arc<RwLock<PathBuf>>,
    kind: String,
}

impl BashWorkspaceGate {
    /// Gate over the LIVE (mutable) working dir handle.
    pub fn new(cwd: Arc<RwLock<PathBuf>>) -> Self {
        Self {
            store: Arc::new(InMemoryPermissionStore::new()),
            cwd,
            kind: APPROVAL_KIND.to_string(),
        }
    }

    /// Gate over a FIXED workspace root (tests / assemblies that pin an immutable working dir).
    pub fn pinned(root: PathBuf) -> Self {
        Self::new(Arc::new(RwLock::new(root)))
    }

    /// Use a caller-supplied grant store (tests).
    pub fn with_store(cwd: Arc<RwLock<PathBuf>>, store: Arc<dyn PermissionStore>) -> Self {
        Self {
            store,
            cwd,
            kind: APPROVAL_KIND.to_string(),
        }
    }

    /// Round-trip the driver for an approval decision (same wire shape as `ApprovalMiddleware`,
    /// so the existing prompt UI renders it unchanged).
    async fn prompt(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> PermissionDecision {
        let payload = serde_json::to_value(ApprovalRequest {
            call_id: call.id.clone(),
            tool: tool.name().to_string(),
            args: call.arguments.clone(),
        })
        .unwrap_or(serde_json::Value::Null);
        PermissionDecision::from_value(&rt.request(&self.kind, payload).await)
    }

    /// Prompt and NEVER remember it — every call re-prompts (used for sensitive targets and for
    /// unresolvable destructive commands). Both allow-once and allow-always proceed without storing.
    async fn prompt_unremembered(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> BeforeOutcome {
        match self.prompt(call, tool, rt).await {
            PermissionDecision::AllowOnce | PermissionDecision::AllowAlways => {
                BeforeOutcome::Allow {
                    reason: Some("destructive bash approved (not remembered)".into()),
                }
            }
            PermissionDecision::Deny => BeforeOutcome::deny(
                "a destructive bash command needs approval and was denied".to_string(),
            ),
        }
    }
}

#[async_trait]
impl ToolMiddleware for BashWorkspaceGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> BeforeOutcome {
        if !super::is_command_shell_tool(tool.name()) {
            return BeforeOutcome::Proceed; // not a command-running shell tool (bash / bash_start)
        }
        let Some(command) = bash_command(&call.arguments) else {
            return BeforeOutcome::Proceed; // unparseable args — bash tool / risk flow handles it
        };

        let targets = match scan_destructive_bash(&command) {
            BashScan::NotDestructive => return BeforeOutcome::Proceed,
            BashScan::Unresolvable => return self.prompt_unremembered(call, tool, rt).await,
            BashScan::Targets(t) => t,
        };

        // Snapshot the live cwd (clone in one statement so the guard isn't held across an await —
        // the future must stay Send). A poisoned lock means we can't resolve relative targets:
        // still block an ABSOLUTE sensitive target; otherwise defer.
        let cwd = match self.cwd.read().ok().map(|g| g.clone()) {
            Some(c) => c,
            None => {
                return if targets.iter().any(|t| path_is_sensitive(Path::new(t))) {
                    self.prompt_unremembered(call, tool, rt).await
                } else {
                    BeforeOutcome::Proceed
                };
            }
        };

        // Sensitive TARGET → prompt EVERY time, never remembered (mirror WriteApprovalGate).
        // Classified on the resolved target (not a substring of the whole command) so a benign
        // command that merely MENTIONS a secret name (`echo id_rsa >> ./notes.txt`) isn't blocked.
        if targets
            .iter()
            .any(|t| path_is_sensitive(&resolve_path(t, &cwd)))
        {
            return self.prompt_unremembered(call, tool, rt).await;
        }

        // Classification canonicalizes paths (filesystem I/O) — run OFF the async worker, bounded,
        // so a hung mount can't freeze the kernel's turn loop (Esc/Ctrl-C stay live). On timeout we
        // degrade to "not in workspace" → a normal prompt (safe, never hangs).
        // `mv` (source, dest) pairs — for the "moved OUT of the workspace = deleted" check below.
        let mv_pairs = mv_moves(&command);
        let (all_in_workspace, out_keys) = {
            let targets = targets.clone();
            let mv_pairs = mv_pairs.clone();
            let cwd = cwd.clone();
            let fallback = (false, Vec::<String>::new());
            super::run_bounded(super::GATE_FS_TIMEOUT, fallback, move || {
                // Written/deleted TARGETS that land OUTSIDE the workspace (path canonicalizes —
                // filesystem I/O). Temp-dir targets (codex parity: /tmp + $TMPDIR are
                // default-writable) count as in-workspace: `cargo build > /tmp/x.json` is benign
                // scratch, not a write to a user-owned directory outside the workspace.
                let mut out: Vec<String> = targets
                    .iter()
                    .filter(|t| !(path_in_workspace(t, &cwd) || path_in_temp_dir(t, &cwd)))
                    .cloned()
                    .collect();
                // A `mv` carrying an in-workspace file to a dest NOT in the workspace PROPER removes
                // it from the workspace — an equivalent delete. Temp does NOT rescue the dest here
                // (moving a workspace file to /tmp still deletes it from the workspace), so a
                // rejected `rm` can't be laundered through `mv <ws_file> /tmp/backup`. Treat that
                // dest as out-of-workspace so the move prompts. `!out.contains` avoids a duplicate
                // when the same dest is already a scan target (`mv ws /outside/x`).
                for (src, dst) in &mv_pairs {
                    if path_in_workspace(src, &cwd)
                        && !path_in_workspace(dst, &cwd)
                        && !out.contains(dst)
                    {
                        out.push(dst.clone());
                    }
                }
                let all_in = out.is_empty();
                // Per-out-target directory grant keys, deduped (multiple targets can share a dir).
                let mut keys: Vec<String> = out
                    .iter()
                    .map(|t| format!("bashdir::{}", canonical_dir_key(t, &cwd)))
                    .collect();
                keys.sort();
                keys.dedup();
                (all_in, keys)
            })
            .await
        };

        // All targets in-workspace → unchanged behavior (single-file rm stays Safe→runs; a
        // recursive rm is Risky and still reaches ApprovalMiddleware after this Proceed).
        if all_in_workspace {
            return BeforeOutcome::Proceed;
        }

        // Out-of-workspace → prompt; "Always" remembers per canonical directory. Auto-allow ONLY
        // when EVERY out-of-workspace target's directory is already granted — a prior grant for
        // ONE directory must not let an ADDITIONAL out-of-workspace / mv-escape target in the same
        // command ride along (`rm /granted/x && mv ws_file /tmp/stolen`). Empty keys (FS-timeout
        // fallback) → never auto-allow → prompt (fail-closed).
        if !out_keys.is_empty() && out_keys.iter().all(|k| self.store.is_granted(k)) {
            return BeforeOutcome::Allow {
                reason: Some("previously granted this session".into()),
            };
        }
        match self.prompt(call, tool, rt).await {
            PermissionDecision::AllowOnce => BeforeOutcome::Allow {
                reason: Some("approved once".into()),
            },
            PermissionDecision::AllowAlways => {
                for k in &out_keys {
                    self.store.grant(k);
                }
                BeforeOutcome::Allow {
                    reason: Some("approved always (this folder)".into()),
                }
            }
            PermissionDecision::Deny => {
                BeforeOutcome::deny("destructive bash denied by approval policy".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::event::AgentEvent;
    use std::time::Duration;
    use tokio::sync::mpsc::unbounded_channel;

    // ---- git-subcommand detection --------------------------------------------------------------

    #[cfg(feature = "atomgit")]
    fn runs_push(cmd: &str) -> bool {
        command_invokes_git_subcommand(cmd, "push")
    }

    #[cfg(feature = "atomgit")]
    #[test]
    fn git_subcommand_detects_push_across_shapes() {
        assert!(runs_push("git push"));
        assert!(runs_push("git push origin main"));
        assert!(runs_push(
            "cd ~/r && git add -A && git commit -m x && git push origin main"
        ));
        assert!(runs_push("env GIT_SSH_COMMAND=\"ssh -i k\" git push"));
        assert!(runs_push(
            "git -c http.sslVerify=false -C /repo push origin HEAD"
        ));
        assert!(runs_push("git push origin main 2>&1 | tail -4"));
        // Line-continuation join: a wrapped push is still one command.
        assert!(runs_push("cd ~/r \\\n && git push"));
        // The exact multi-step chain the GLM session emitted.
        assert!(runs_push(
            "cd ~/Desktop/menu && git add -A 2>&1 && git -c user.name=\"saulcy\" commit -m msg 2>&1 | tail -4 && GIT_SSH_COMMAND=\"ssh -i ~/.ssh/id_rsa -o StrictHostKeyChecking=accept-new\" git push origin main 2>&1 | tail -4"
        ));
    }

    #[cfg(feature = "atomgit")]
    #[test]
    fn git_subcommand_rejects_non_push() {
        assert!(!runs_push("git pull"));
        assert!(!runs_push("git status"));
        assert!(!runs_push("git commit -m x"));
        assert!(!runs_push("echo git push"));
        assert!(!runs_push("cd ~/r && echo git push"));
        // `git push` living inside a quoted argument is not an invocation.
        assert!(!runs_push("git commit -m \"remember to git push later\""));
        assert!(!runs_push(""));
    }

    // ---- scanner unit tests ----------------------------------------------------------------

    fn scan(cmd: &str) -> BashScan {
        scan_destructive_bash(cmd)
    }

    #[test]
    fn non_destructive_commands_pass() {
        assert_eq!(scan("ls -la"), BashScan::NotDestructive);
        assert_eq!(scan("cat file.txt | grep foo"), BashScan::NotDestructive);
        assert_eq!(
            scan("git commit -m 'cd into dir and rm stuff'"),
            BashScan::NotDestructive
        );
    }

    #[test]
    fn single_file_rm_extracts_target() {
        assert_eq!(
            scan("rm /outside/x.txt"),
            BashScan::Targets(vec!["/outside/x.txt".into()])
        );
        assert_eq!(
            scan("rm -f ./a.txt"),
            BashScan::Targets(vec!["./a.txt".into()])
        );
        assert_eq!(
            scan("rm -rf src/gen"),
            BashScan::Targets(vec!["src/gen".into()])
        );
    }

    #[test]
    fn mv_targets_both_operands() {
        assert_eq!(
            scan("mv a.txt /outside/b.txt"),
            BashScan::Targets(vec!["a.txt".into(), "/outside/b.txt".into()])
        );
    }

    #[test]
    fn mv_moves_extracts_source_dest_pairs() {
        assert_eq!(
            mv_moves("mv a.txt /tmp/b"),
            vec![("a.txt".to_string(), "/tmp/b".to_string())]
        );
        // multi-source → one pair per source, shared dest dir
        assert_eq!(
            mv_moves("mv a b c /tmp/dest"),
            vec![
                ("a".to_string(), "/tmp/dest".to_string()),
                ("b".to_string(), "/tmp/dest".to_string()),
                ("c".to_string(), "/tmp/dest".to_string()),
            ]
        );
        // `-t DEST` form: every positional is a source
        assert_eq!(
            mv_moves("mv -t /tmp/dest a b"),
            vec![
                ("a".to_string(), "/tmp/dest".to_string()),
                ("b".to_string(), "/tmp/dest".to_string())
            ]
        );
        // cp is a copy (source stays) → not a move
        assert!(mv_moves("cp a.txt /tmp/b").is_empty());
        // rm / single-operand mv → no pair
        assert!(mv_moves("rm a.txt").is_empty());
        assert!(mv_moves("mv onlyone").is_empty());
    }

    #[test]
    fn escaped_command_is_still_recognized() {
        // `\rm` / `\mv` (leading backslash bypasses aliases but runs the real command) must not
        // slip through as non-destructive.
        assert_eq!(
            scan("\\rm /outside/x.txt"),
            BashScan::Targets(vec!["/outside/x.txt".into()])
        );
        assert_eq!(
            scan("\\mv a.txt /outside/b"),
            BashScan::Targets(vec!["a.txt".into(), "/outside/b".into()])
        );
        assert_eq!(
            mv_moves("\\mv a.txt /tmp/b"),
            vec![("a.txt".to_string(), "/tmp/b".to_string())]
        );
    }

    #[test]
    fn cp_targets_only_dest() {
        // src is a read; only the dest (last operand) is written.
        assert_eq!(
            scan("cp /outside/src ./dst"),
            BashScan::Targets(vec!["./dst".into()])
        );
        assert_eq!(
            scan("cp -r a b /outside/dest"),
            BashScan::Targets(vec!["/outside/dest".into()])
        );
    }

    #[test]
    fn dd_targets_only_of() {
        assert_eq!(
            scan("dd if=/dev/zero of=/outside/big bs=1M"),
            BashScan::Targets(vec!["/outside/big".into()])
        );
        assert_eq!(scan("dd if=/dev/sda"), BashScan::NotDestructive); // no of= → writes stdout
    }

    #[test]
    fn redirect_targets_gated_fd_dups_skipped() {
        assert_eq!(
            scan("echo hi > /outside/out.txt"),
            BashScan::Targets(vec!["/outside/out.txt".into()])
        );
        assert_eq!(
            scan("echo hi >> ./log.txt"),
            BashScan::Targets(vec!["./log.txt".into()])
        );
        // fd dup + /dev/null are not file writes.
        assert_eq!(scan("run 2>&1"), BashScan::NotDestructive);
        assert_eq!(scan("run 2>/dev/null"), BashScan::NotDestructive);
        // stderr-to-file IS a file write.
        assert_eq!(
            scan("run 2> /outside/err.log"),
            BashScan::Targets(vec!["/outside/err.log".into()])
        );
    }

    #[test]
    fn redirect_scanner_handles_spacing_fds_quotes_and_compounds() {
        assert_eq!(
            scan_redirect_writes("echo x >/etc/passwd").unwrap(),
            vec!["/etc/passwd"]
        );
        assert_eq!(
            scan_redirect_writes("echo x >>/etc/shadow").unwrap(),
            vec!["/etc/shadow"]
        );
        assert_eq!(
            scan_redirect_writes("echo x 2>\"/etc/hosts\"").unwrap(),
            vec!["/etc/hosts"]
        );
        assert_eq!(
            scan_redirect_writes("echo ok && echo x >/etc/sudoers").unwrap(),
            vec!["/etc/sudoers"]
        );
        assert!(scan_redirect_writes("echo x >$TARGET").is_err());
        assert_eq!(scan_redirect_writes("test z > a").unwrap(), vec!["a"]);
        assert_eq!(scan_redirect_writes("[ z ] > b").unwrap(), vec!["b"]);
        assert!(scan_redirect_writes("[[ z > a ]]").unwrap().is_empty());
        assert!(scan_redirect_writes("(( 2 > 1 ))").unwrap().is_empty());
        assert_eq!(
            scan_redirect_writes("[[ z > a ]] >/etc/passwd").unwrap(),
            vec!["/etc/passwd"]
        );
        assert_eq!(
            scan_redirect_writes("(( 2 > 1 )) >/etc/shadow").unwrap(),
            vec!["/etc/shadow"]
        );
        assert!(scan_redirect_writes(r"echo \>/etc/passwd")
            .unwrap()
            .is_empty());
        assert!(scan_redirect_writes(r#"echo "literal \" | >/etc/passwd""#)
            .unwrap()
            .is_empty());
        assert!(scan_redirect_writes("echo '>/etc/passwd'")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn cd_then_destructive_is_unresolvable() {
        assert_eq!(scan("cd /outside && rm x.txt"), BashScan::Unresolvable);
        assert_eq!(scan("cd sub; rm y"), BashScan::Unresolvable);
    }

    #[test]
    fn cd_then_absolute_target_is_resolvable() {
        // A `cd` only makes RELATIVE targets ambiguous; an ABSOLUTE target lands the same place
        // regardless of cwd, so it must classify normally (→ temp/workspace check), not fail
        // closed. This is the `cd ws && cargo build > /tmp/log` false-positive fix.
        assert_eq!(
            scan("cd /ws && cargo build > /tmp/out.txt"),
            BashScan::Targets(vec!["/tmp/out.txt".into()])
        );
        assert_eq!(
            scan("cd /ws && rm /outside/a.txt"),
            BashScan::Targets(vec!["/outside/a.txt".into()])
        );
        // `~`-home targets are also cwd-independent (a preceding cd cannot move them).
        assert_eq!(
            scan("cd /ws && echo x > ~/out.txt"),
            BashScan::Targets(vec!["~/out.txt".into()])
        );
    }

    #[test]
    fn cd_then_relative_target_still_unresolvable() {
        // With a cd in the mix, a RELATIVE destructive target genuinely can't be resolved.
        assert_eq!(scan("cd /ws && echo x > rel.txt"), BashScan::Unresolvable);
        // A mix: any relative target taints the whole command (fail closed).
        assert_eq!(
            scan("cd /ws && rm /abs/a.txt rel.txt"),
            BashScan::Unresolvable
        );
    }

    #[test]
    fn command_substitution_is_unresolvable() {
        assert_eq!(scan("rm $(cat list.txt)"), BashScan::Unresolvable);
        assert_eq!(scan("rm `cat list.txt`"), BashScan::Unresolvable);
        assert_eq!(scan("rm $TARGET"), BashScan::Unresolvable);
    }

    #[test]
    fn compound_all_targets_collected() {
        assert_eq!(
            scan("rm a.txt && rm /outside/b.txt"),
            BashScan::Targets(vec!["a.txt".into(), "/outside/b.txt".into()])
        );
    }

    #[test]
    fn quoted_path_with_spaces() {
        assert_eq!(
            scan(r#"rm "/outside/my file.txt""#),
            BashScan::Targets(vec!["/outside/my file.txt".into()])
        );
    }

    #[test]
    fn rm_only_flags_writes_nothing() {
        // No path operand → nothing to delete → not our concern.
        assert_eq!(scan("rm -f"), BashScan::NotDestructive);
    }

    #[test]
    fn cp_target_directory_flag_is_a_target() {
        // The destination smuggled via -t / --target-directory must be classified, not dropped.
        assert_eq!(
            scan("cp -t /outside a.txt b.txt"),
            BashScan::Targets(vec!["/outside".into()])
        );
        assert_eq!(
            scan("cp --target-directory=/outside a.txt"),
            BashScan::Targets(vec!["/outside".into()])
        );
        assert_eq!(
            scan("cp -t/outside a.txt"),
            BashScan::Targets(vec!["/outside".into()])
        );
    }

    #[test]
    fn mv_target_directory_long_form_is_a_target() {
        // `mv --target-directory=/outside a.txt` must include /outside (the =-glued long form was
        // previously dropped as a flag → silent move outside the workspace).
        assert_eq!(
            scan("mv --target-directory=/outside a.txt"),
            BashScan::Targets(vec!["a.txt".into(), "/outside".into()])
        );
    }

    #[test]
    fn noclobber_override_redirect_is_gated() {
        // `>|` (noclobber override) must not be split on the `|` and lose its target.
        assert_eq!(
            scan("cat secrets >| /outside/out.txt"),
            BashScan::Targets(vec!["/outside/out.txt".into()])
        );
    }

    #[test]
    fn backslash_newline_continuation_is_joined() {
        // `rm \⏎/outside/x` — the line continuation is removed so the target isn't orphaned.
        assert_eq!(
            scan("rm \\\n/outside/x.txt"),
            BashScan::Targets(vec!["/outside/x.txt".into()])
        );
    }

    #[test]
    fn dynamic_in_unrelated_arg_is_not_unresolvable() {
        // `$(date)` in an echo ARG must not force the whole (in-workspace) redirect to prompt.
        assert_eq!(
            scan(r#"echo "x=$(date)" > log"#),
            BashScan::Targets(vec!["log".into()])
        );
        // ...but a $()/backtick in the TARGET itself is still unresolvable.
        assert_eq!(scan("echo x > `mktemp`"), BashScan::Unresolvable);
    }

    #[test]
    fn test_construct_gt_is_not_a_redirect() {
        // `>` inside a conditional / arithmetic construct is a comparison, not a file write.
        assert_eq!(scan("[[ $a > $b ]]"), BashScan::NotDestructive);
        assert_eq!(scan("(( a > b ))"), BashScan::NotDestructive);
    }

    // ---- gate integration tests ------------------------------------------------------------

    fn bash_tool() -> Arc<dyn Tool> {
        Arc::new(crate::tools::bash::BashTool)
    }

    /// A driver that never answers → the bounded round-trip times out → Null → Deny. Any path
    /// that REACHES the prompt fails closed; an auto-approve/proceed returns without the driver.
    fn silent_rt() -> RequestCtx {
        let (tx, _rx) = unbounded_channel::<AgentEvent>();
        RequestCtx::new(tx, Some(Duration::from_millis(20)))
    }

    fn bash_call(command: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": command }).to_string(),
        }
    }

    #[tokio::test]
    async fn non_bash_tool_is_not_ours() {
        let gate = BashWorkspaceGate::pinned(std::env::temp_dir());
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::read::ReadFileTool::default());
        let mut call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: r#"{"file_path":"a"}"#.into(),
        };
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn in_workspace_single_file_rm_proceeds() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), "x").unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("rm a.txt");
        // Proceed → the silent driver is never consulted (would Deny if reached).
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn out_of_workspace_rm_prompts() {
        let ws = tempfile::tempdir().unwrap();
        // Use a fabricated non-temp, non-workspace absolute path (need not exist — gate is
        // path-based, canonicalizes ancestors up to `/`).
        let target = std::path::PathBuf::from("/atomcode-test-outside-rm/x.txt");
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call(&format!("rm {}", target.to_str().unwrap()));
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "out-of-workspace rm must prompt (fail closed when silent), got {out:?}"
        );
    }

    /// Regression: `bash_start` runs arbitrary commands too, so it must face THIS gate exactly
    /// like `bash` — an out-of-workspace delete backgrounded via `bash_start` prompts, it does
    /// not slip through as "not ours" (the bug this gate's `is_command_shell_tool` check fixes).
    #[tokio::test]
    async fn out_of_workspace_rm_via_bash_start_prompts() {
        let ws = tempfile::tempdir().unwrap();
        let target = std::path::PathBuf::from("/atomcode-test-outside-rm/x.txt");
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::bash::BashStartTool);
        let mut call = ToolCall {
            id: "1".into(),
            name: "bash_start".into(),
            arguments: serde_json::json!({ "command": format!("rm {}", target.to_str().unwrap()) })
                .to_string(),
        };
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "out-of-workspace rm via bash_start must prompt (fail closed when silent), got {out:?}"
        );
    }

    #[tokio::test]
    async fn sensitive_target_prompts_unremembered() {
        // A pre-granted key must NOT let a sensitive delete through.
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "k").unwrap();
        let store: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new());
        store.grant(&format!(
            "bashdir::{}",
            canonical_dir_key(secret.to_str().unwrap(), ws.path())
        ));
        let gate =
            BashWorkspaceGate::with_store(Arc::new(RwLock::new(ws.path().to_path_buf())), store);
        let tool = bash_tool();
        let mut call = bash_call(&format!("rm {}", secret.to_str().unwrap()));
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "sensitive delete must prompt (ignore cache), got {out:?}"
        );
    }

    #[tokio::test]
    async fn unresolvable_prompts() {
        let ws = tempfile::tempdir().unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("cd /somewhere/else && rm data.bin");
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "unresolvable destructive must prompt (fail closed), got {out:?}"
        );
    }

    #[tokio::test]
    async fn out_of_workspace_grant_is_per_directory() {
        // Grant ONE out-of-workspace directory; a sibling delete there auto-approves, a delete in
        // a different folder still prompts.
        // Use fabricated non-temp absolute paths (need not exist — gate is path-based).
        let ws = tempfile::tempdir().unwrap();
        let dir_a = std::path::PathBuf::from("/atomcode-test-outside-grant/a");
        let dir_b = std::path::PathBuf::from("/atomcode-test-outside-grant/b");
        let granted = dir_a.join("granted.txt");
        let sibling = dir_a.join("sibling.txt");
        let other = dir_b.join("other.txt");
        let store: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new());
        store.grant(&format!(
            "bashdir::{}",
            canonical_dir_key(granted.to_str().unwrap(), ws.path())
        ));
        let gate =
            BashWorkspaceGate::with_store(Arc::new(RwLock::new(ws.path().to_path_buf())), store);
        let tool = bash_tool();

        let mut s = bash_call(&format!("rm {}", sibling.to_str().unwrap()));
        assert!(
            matches!(
                gate.before(&mut s, &tool, &silent_rt()).await,
                BeforeOutcome::Allow { .. }
            ),
            "a sibling delete in the granted folder must auto-approve"
        );
        let mut o = bash_call(&format!("rm {}", other.to_str().unwrap()));
        assert!(
            gate.before(&mut o, &tool, &silent_rt()).await.is_deny(),
            "a delete in a different folder must still prompt"
        );
    }

    #[tokio::test]
    async fn mv_workspace_file_to_temp_prompts() {
        // The equivalent-delete bypass: a rejected `rm` laundered through `mv <ws_file> /tmp/x`.
        // Moving an in-workspace file to /tmp removes it from the workspace, so it must prompt —
        // the temp carve-out must NOT rescue an mv DEST when the SOURCE is in the workspace.
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("data.bin"), "x").unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("mv data.bin /tmp/atomcode-mv-test-backup");
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "mv of a workspace file out to /tmp must prompt (fail closed), got {out:?}"
        );
    }

    #[tokio::test]
    async fn escaped_mv_out_of_workspace_prompts() {
        // `\mv` must be gated like `mv` — the backslash-escape can't launder a workspace delete.
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("data.bin"), "x").unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("\\mv data.bin /tmp/atomcode-esc-backup");
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "escaped mv of a workspace file out must prompt, got {out:?}"
        );
    }

    #[tokio::test]
    async fn grant_does_not_cover_an_additional_out_target() {
        // A prior grant for ONE out-of-workspace dir must NOT auto-approve a compound command that
        // ALSO moves a workspace file out (the equivalent-delete must still prompt).
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("data.bin"), "x").unwrap();
        let granted_dir = std::path::PathBuf::from("/atomcode-test-grant-cover/a");
        let store: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new());
        store.grant(&format!(
            "bashdir::{}",
            canonical_dir_key(granted_dir.join("x").to_str().unwrap(), ws.path())
        ));
        let gate =
            BashWorkspaceGate::with_store(Arc::new(RwLock::new(ws.path().to_path_buf())), store);
        let tool = bash_tool();
        // Op in the granted dir + an mv-escape of a workspace file to /tmp.
        let mut call = bash_call(&format!(
            "rm {}/x && mv data.bin /tmp/atomcode-stolen",
            granted_dir.to_str().unwrap()
        ));
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "the grant must not cover the additional mv-escape target, got {out:?}"
        );
    }

    #[tokio::test]
    async fn mv_within_workspace_proceeds() {
        // A rename inside the workspace keeps the file in the workspace → no prompt.
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), "x").unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("mv a.txt b.txt");
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn mv_temp_to_temp_proceeds() {
        // No in-workspace source involved → the move doesn't touch the workspace → no prompt.
        let ws = tempfile::tempdir().unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("mv /tmp/atomcode-a /tmp/atomcode-b");
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn in_workspace_redirect_mentioning_secret_name_proceeds() {
        // A benign in-workspace log write that merely MENTIONS a secret name must NOT prompt
        // (the sensitivity check is on the resolved target, not a substring of the command).
        let ws = tempfile::tempdir().unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("echo id_rsa >> ./notes.txt");
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn in_workspace_redirect_proceeds() {
        let ws = tempfile::tempdir().unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("echo hi > out.txt");
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    // ---- temp-dir whitelist tests (codex parity) -------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn redirect_to_slash_tmp_proceeds() {
        let ws = tempfile::tempdir().unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("cargo build > /tmp/build_all.json");
        // /tmp is a writable temp root (codex parity) → no prompt.
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cd_then_redirect_to_slash_tmp_proceeds() {
        // The reported false positive: `cd <ws> && cargo build > /tmp/x; wc -l /tmp/x` must NOT
        // prompt — the absolute /tmp target is a writable temp root, and the leading `cd` cannot
        // move it, so it stays classifiable instead of failing closed on the cd.
        let ws = tempfile::tempdir().unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let cmd = format!(
            "cd {} && cargo build --workspace 2>&1 > /tmp/atomcode_dead_full.txt; wc -l /tmp/atomcode_dead_full.txt",
            ws.path().to_str().unwrap()
        );
        let mut call = bash_call(&cmd);
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cd_then_relative_redirect_still_prompts() {
        // Counterpart: with a cd, a RELATIVE redirect target can't be resolved → still fail closed.
        let ws = tempfile::tempdir().unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let mut call = bash_call("cd /somewhere && echo x > out.txt");
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "cd + relative redirect target must still prompt, got {out:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn redirect_to_system_tmpdir_proceeds() {
        let ws = tempfile::tempdir().unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        let target = std::env::temp_dir().join("atomcode_gate_probe.json");
        let mut call = bash_call(&format!("echo x > {}", target.to_str().unwrap()));
        assert_eq!(
            gate.before(&mut call, &tool, &silent_rt()).await,
            BeforeOutcome::Proceed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn traversal_out_of_tmp_still_prompts() {
        let ws = tempfile::tempdir().unwrap();
        let gate = BashWorkspaceGate::pinned(ws.path().to_path_buf());
        let tool = bash_tool();
        // /tmp/../atomcode_gate_escape.txt canonicalizes OUT of temp → still out-of-workspace → prompts (fail-closed under silent_rt).
        let mut call = bash_call("echo x > /tmp/../atomcode_gate_escape.txt");
        let out = gate.before(&mut call, &tool, &silent_rt()).await;
        assert!(
            out.is_deny(),
            "a target that escapes /tmp via .. must still prompt, got {out:?}"
        );
    }
}
