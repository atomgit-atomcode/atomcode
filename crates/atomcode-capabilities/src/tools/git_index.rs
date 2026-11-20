//! Read-only Git-index review primitives. Policy/approval stays with the host.
//! This is an accident-prevention guard for literal shell invocations, not an
//! OS sandbox for arbitrary programs, aliases, or dynamically generated scripts.
use super::bash::{bash_invocations, parse_bash};
use crate::world::{Chunk, Shell, ShellDialect, SpawnOptions};
use sha2::{Digest, Sha256};
use std::path::Path;

const SPLIT: &str = "Git index review requires a standalone literal git command. Stage explicit files first, then run git commit separately (use git -C <directory> instead of cd && git).";
const LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Stage {
        paths: Vec<String>,
        broad: bool,
        force: bool,
    },
    Commit,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    git: Vec<String>,
    pub operation: Operation,
}

/// Decode a literal shell word without evaluating expansions or executing a shell.
fn literal(word: &str) -> Option<String> {
    let mut out = String::new();
    let mut quote = None;
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), c) => out.push(c),
            (Some('"'), '"') => quote = None,
            (_, '$' | '`') => return None,
            (None, '\'' | '"') => quote = Some(c),
            (_, '\\') => {
                let next = chars.next()?;
                if quote == Some('"') && !matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                    out.push('\\');
                }
                if next != '\n' {
                    out.push(next);
                }
            }
            (_, c) => out.push(c),
        }
    }
    quote.is_none().then_some(out)
}
fn base(s: &str) -> &str {
    s.rsplit(['/', '\\']).next().unwrap_or(s)
}
fn is_git(s: &str) -> bool {
    base(s).eq_ignore_ascii_case("git") || base(s).eq_ignore_ascii_case("git.exe")
}
fn broad_path(s: &str) -> bool {
    matches!(s, "." | "./" | ".." | "../" | "/" | ":" | ":/")
        || s.starts_with(":(")
        || s.contains(['*', '?', '['])
        || s.ends_with('/')
}

fn git_plan(words: &[String]) -> Result<Option<Plan>, String> {
    let Some(exe) = words.first() else {
        return Ok(None);
    };
    if !is_git(exe) {
        return Ok(None);
    }
    let mut git = vec![exe.clone()];
    let mut at = 1;
    let mut unsupported = false;
    while let Some(arg) = words.get(at).filter(|a| a.starts_with('-')) {
        match arg.as_str() {
            "-C" => {
                git.extend_from_slice(words.get(at..at + 2).ok_or(SPLIT)?);
                at += 2;
            }
            "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                unsupported = true;
                at += 2;
            }
            "--no-pager" | "--literal-pathspecs" | "--no-optional-locks" => at += 1,
            a if a.starts_with("-C") => {
                git.push(a.to_string());
                at += 1;
            }
            _ => {
                unsupported = true;
                at += 1;
            }
        }
    }
    let Some(sub) = words.get(at) else {
        return Ok(None);
    };
    if !matches!(sub.as_str(), "add" | "commit") {
        return Ok(None);
    }
    let args = &words[at + 1..];
    if unsupported {
        return Err(SPLIT.into());
    }
    if sub == "add" {
        let mut paths = Vec::new();
        let mut broad = false;
        let mut force = false;
        let mut operands = false;
        let mut dry_run = false;
        for arg in args {
            if arg == "--" && !operands {
                operands = true;
                continue;
            }
            if !operands && arg.starts_with('-') {
                if arg.starts_with("--pathspec-from-file")
                    || matches!(arg.as_str(), "--patch" | "--interactive" | "--edit")
                    || (!arg.starts_with("--")
                        && arg.chars().skip(1).any(|c| matches!(c, 'p' | 'i' | 'e')))
                {
                    return Err(SPLIT.into());
                }
                // Git accepts abbreviated long options (e.g. --for = --force).
                // Only exact, understood spellings may affect this preview.
                if arg.starts_with("--") {
                    if !matches!(
                        arg.as_str(),
                        "--all"
                            | "--no-all"
                            | "--ignore-removal"
                            | "--no-ignore-removal"
                            | "--update"
                            | "--intent-to-add"
                            | "--force"
                            | "--verbose"
                            | "--refresh"
                            | "--ignore-errors"
                            | "--ignore-missing"
                            | "--sparse"
                            | "--renormalize"
                            | "--dry-run"
                            | "--chmod=+x"
                            | "--chmod=-x"
                            | "--no-warn-embedded-repo"
                    ) {
                        return Err("Git staging review requires full, supported option names and explicit file paths; abbreviated options are not supported.".into());
                    }
                } else if !arg
                    .chars()
                    .skip(1)
                    .all(|flag| matches!(flag, 'A' | 'u' | 'v' | 'N' | 'f' | 'n'))
                {
                    return Err(SPLIT.into());
                }
                if arg == "--dry-run" || (!arg.starts_with("--") && arg.contains('n')) {
                    dry_run = true;
                }
                force |= arg == "--force" || (!arg.starts_with("--") && arg.contains('f'));
            } else {
                paths.push(arg.clone());
            }
        }
        if dry_run {
            return Ok(None);
        }
        // -A/--all with explicit files is still a scoped add. No operands is broad.
        broad |= paths.is_empty() || paths.iter().any(|p| broad_path(p));
        Ok(Some(Plan {
            git,
            operation: Operation::Stage {
                paths,
                broad,
                force,
            },
        }))
    } else {
        let mut at = 0;
        let mut dry_run = false;
        while let Some(arg) = args.get(at) {
            if arg == "--dry-run" {
                dry_run = true;
            } else if arg == "--" {
                if at + 1 != args.len() {
                    return Err(SPLIT.into());
                }
            } else if !arg.starts_with('-') {
                return Err(SPLIT.into());
            } else if matches!(
                arg.split('=').next().unwrap_or(arg),
                "--all" | "--include" | "--only" | "--patch" | "--interactive"
            ) || arg.starts_with("--pathspec-")
            {
                return Err(SPLIT.into());
            } else if matches!(
                arg.as_str(),
                "--message"
                    | "--file"
                    | "--author"
                    | "--date"
                    | "--cleanup"
                    | "--trailer"
                    | "--fixup"
                    | "--squash"
                    | "--reuse-message"
                    | "--reedit-message"
                    | "--template"
            ) {
                at += 1;
                if at >= args.len() {
                    return Err(SPLIT.into());
                }
            } else if arg.starts_with("--") {
                let supported_flag = matches!(
                    arg.as_str(),
                    "--amend"
                        | "--no-edit"
                        | "--edit"
                        | "--reset-author"
                        | "--allow-empty"
                        | "--allow-empty-message"
                        | "--no-verify"
                        | "--verify"
                        | "--signoff"
                        | "--no-signoff"
                        | "--gpg-sign"
                        | "--no-gpg-sign"
                        | "--quiet"
                        | "--verbose"
                        | "--status"
                        | "--no-status"
                        | "--no-post-rewrite"
                        | "--post-rewrite"
                );
                let supported_value = arg.split_once('=').is_some_and(|(name, _)| {
                    matches!(
                        name,
                        "--message"
                            | "--file"
                            | "--author"
                            | "--date"
                            | "--cleanup"
                            | "--trailer"
                            | "--fixup"
                            | "--squash"
                            | "--reuse-message"
                            | "--reedit-message"
                            | "--template"
                            | "--gpg-sign"
                    )
                });
                if !supported_flag && !supported_value {
                    return Err("Git commit review requires full, supported option names; abbreviated or unknown options may change the index and are not supported.".into());
                }
            } else {
                for (index, flag) in arg.char_indices().skip(1) {
                    if matches!(flag, 'a' | 'i' | 'o' | 'p') {
                        return Err(SPLIT.into());
                    }
                    if matches!(flag, 'm' | 'F' | 'C' | 'c' | 't' | 'S') {
                        if index + 1 == arg.len() && flag != 'S' {
                            at += 1;
                            if at >= args.len() {
                                return Err(SPLIT.into());
                            }
                        }
                        break;
                    }
                    if !matches!(flag, 'q' | 'v' | 'n' | 's' | 'e') {
                        return Err(SPLIT.into());
                    }
                }
            }
            at += 1;
        }
        if dry_run {
            return Ok(None);
        }
        Ok(Some(Plan {
            git,
            operation: Operation::Commit,
        }))
    }
}

/// Inspect literal native-shell invocations without applying Bash escaping to
/// Windows paths. This deliberately accepts only a standalone command, not a
/// general cmd/PowerShell program. Unsupported mutations must be split/rephrased.
pub fn classify_in(source: &str, dialect: ShellDialect) -> Result<Option<Plan>, String> {
    if dialect == ShellDialect::Bash {
        return classify(source);
    }
    let powershell = dialect == ShellDialect::PowerShell;
    let source = source.trim_matches([' ', '\t', '\r', '\n']);
    let source = if powershell {
        source.trim_start().strip_prefix('&').unwrap_or(source)
    } else {
        source.trim_start().strip_prefix('@').unwrap_or(source)
    };
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut unsafe_syntax = false;
    let mut chars = source.chars().peekable();
    let mut previous = None;
    while let Some(ch) = chars.next() {
        if matches!(ch, '\r' | '\n') {
            unsafe_syntax = true;
        }
        if powershell {
            if quote != Some('\'') && matches!(ch, '$' | '`') {
                unsafe_syntax = true;
            }
        } else if matches!(ch, '%' | '!' | '^') {
            unsafe_syntax = true;
        }
        if let Some(q) = quote {
            if ch == q {
                if powershell && chars.peek() == Some(&q) {
                    chars.next();
                    word.push(q);
                } else {
                    if !powershell && chars.peek() == Some(&q) {
                        unsafe_syntax = true;
                    }
                    // Backslash-before-quote has native argv rules which differ
                    // from both shell dialects. Do not guess that path's value.
                    if !powershell && previous == Some('\\') {
                        unsafe_syntax = true;
                    }
                    quote = None;
                }
            } else {
                word.push(ch);
            }
        } else if ch == '"' || (powershell && ch == '\'') {
            if !powershell && previous == Some('\\') {
                unsafe_syntax = true;
            }
            quote = Some(ch);
            started = true;
        } else if ch.is_ascii_whitespace() || (powershell && ch.is_whitespace()) {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else if matches!(ch, '&' | '|' | '<' | '>' | '(' | ')')
            || (powershell && matches!(ch, ';' | '{' | '}' | '@' | '#'))
        {
            unsafe_syntax = true;
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(ch);
            started = true;
        }
        previous = Some(ch);
    }
    if started {
        words.push(word);
    }
    unsafe_syntax |= quote.is_some() || words.iter().any(|word| word == "--%");
    // Plain echo/search arguments are data, not additional invocations. Only
    // scan later words when shell operators or a known invocation wrapper exist.
    let wrapper = words.first().is_some_and(|word| {
        matches!(
            base(word).to_ascii_lowercase().as_str(),
            "call" | "cmd" | "cmd.exe" | "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
        )
    });
    if !unsafe_syntax && !wrapper {
        return git_plan(&words);
    }
    let mut found = None;
    for (at, word) in words.iter().enumerate() {
        if !is_git(word) {
            continue;
        }
        if let Some(plan) = git_plan(&words[at..])? {
            if at != 0 || unsafe_syntax {
                return Err(SPLIT.into());
            }
            found = Some(plan);
        }
    }
    Ok(found)
}

/// Locate index-mutating Git calls. Compound/wrapped commands cannot be approved
/// against an index which another segment might change later in the same call.
pub fn classify(source: &str) -> Result<Option<Plan>, String> {
    classify_inner(source, 0)
}
fn classify_inner(source: &str, depth: usize) -> Result<Option<Plan>, String> {
    if depth > 8 {
        return Err(SPLIT.into());
    }
    let Some(calls) = bash_invocations(source) else {
        let words: Vec<_> = source.split_whitespace().collect();
        return if words
            .windows(2)
            .any(|w| matches!(base(w[0]), "git" | "git.exe") && matches!(w[1], "add" | "commit"))
        {
            Err(SPLIT.into())
        } else {
            Ok(None)
        };
    };
    let mut found = None;
    for call in &calls {
        let exe = literal(&call.command).unwrap_or_default();
        let raw: Vec<_> = std::iter::once(&call.command)
            .chain(&call.arguments)
            .collect();
        let words: Option<Vec<_>> = raw.iter().map(|w| literal(w)).collect();
        if matches!(base(&exe), "bash" | "sh" | "zsh") {
            if let Some(words) = &words {
                for pair in words.windows(2) {
                    if matches!(pair[0].as_str(), "-c" | "-lc")
                        && !matches!(classify_inner(&pair[1], depth + 1), Ok(None))
                    {
                        return Err(SPLIT.into());
                    }
                }
            }
        }
        if matches!(base(&exe), "env" | "command" | "exec" | "sudo" | "xargs") {
            if let Some(words) = &words {
                if let Some(i) = words
                    .iter()
                    .position(|w| matches!(base(w), "git" | "git.exe"))
                {
                    if !matches!(git_plan(&words[i..]), Ok(None)) {
                        return Err(SPLIT.into());
                    }
                }
            }
        }
        if !matches!(base(&exe), "git" | "git.exe") {
            continue;
        }
        let words = match words {
            Some(words) => words,
            None => {
                // Expansions in read-only Git commands are not our boundary.
                let skeleton: Vec<_> = raw
                    .iter()
                    .map(|w| literal(w).unwrap_or_else(|| "__dynamic_argument__".into()))
                    .collect();
                if !matches!(git_plan(&skeleton), Ok(None)) {
                    return Err(SPLIT.into());
                }
                continue;
            }
        };
        if let Some(plan) = git_plan(&words)? {
            found = Some(plan);
        }
    }
    if found.is_none() {
        return Ok(None);
    }
    let tree = parse_bash(source).ok_or(SPLIT)?;
    let root = tree.root_node();
    let nodes: Vec<_> = (0..root.named_child_count() as u32)
        .filter_map(|i| root.named_child(i))
        .filter(|n| n.kind() != "comment")
        .collect();
    if calls.len() != 1
        || nodes.len() != 1
        || nodes[0].kind() != "command"
        || (0..nodes[0].named_child_count() as u32)
            .filter_map(|i| nodes[0].named_child(i))
            .any(|n| {
                !matches!(
                    n.kind(),
                    "command_name" | "word" | "raw_string" | "string" | "concatenation" | "number"
                )
            })
    {
        return Err(SPLIT.into());
    }
    Ok(found)
}

/// Pure, argument-aware signal used by the ordinary shell approval path too.
pub fn risky(source: &str) -> bool {
    match classify(source) {
        Err(_)
        | Ok(Some(Plan {
            operation: Operation::Commit,
            ..
        })) => true,
        Ok(Some(Plan {
            operation: Operation::Stage { broad, .. },
            ..
        })) => broad,
        Ok(None) => false,
    }
}
/// Bounded read-only probe in the tool's execution world, never a local fallback.
async fn probe(
    shell: &dyn Shell,
    plan: &Plan,
    cwd: &Path,
    args: &[String],
    allow_missing: bool,
) -> Result<Vec<u8>, String> {
    let arguments: Vec<String> = plan
        .git
        .iter()
        .skip(1)
        .cloned()
        .chain([
            "--no-pager".into(),
            "-c".into(),
            "core.fsmonitor=false".into(),
            "-c".into(),
            "diff.relative=false".into(),
        ])
        .chain(args.iter().cloned())
        .collect();
    let options = SpawnOptions {
        cwd: Some(cwd.into()),
        env: vec![("GIT_OPTIONAL_LOCKS".into(), "0".into())],
    };
    let process = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        shell.spawn_program(&plan.git[0], &arguments, &options),
    )
    .await
    .map_err(|_| "Starting Git index inspection timed out")?
    .map_err(|error| format!("Could not start Git index inspection: {error}"))?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut stdout = Vec::new();
        let mut total = 0usize;
        while let Some(chunk) = process.next_chunk().await {
            total = total.saturating_add(chunk.bytes().len());
            if total > LIMIT {
                return Err("Git index inspection exceeds the bounded preview limit".to_string());
            }
            if let Chunk::Stdout(bytes) = chunk {
                stdout.extend(bytes);
            }
        }
        let exit = process.wait().await?;
        if !exit.success() && !(allow_missing && exit.code == Some(1)) {
            return Err("Git index inspection failed; no staging or commit was executed".into());
        }
        Ok(stdout)
    })
    .await;
    match result {
        Ok(Ok(bytes)) => Ok(bytes),
        other => {
            process.kill().await;
            other.map_err(|_| "Git index inspection timed out".to_string())?
        }
    }
}
fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| (*s).into()).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    pub fingerprint: String,
    pub summary: String,
}

fn preview_path(path: &str) -> String {
    let mut chars = path.chars();
    let mut short: String = chars.by_ref().take(180).collect();
    if chars.next().is_some() {
        short.push('…');
    }
    serde_json::to_string(&short).unwrap_or_default()
}

/// Snapshot the exact staged blob IDs/modes, HEAD, and (for bulk staging) the
/// candidate path list. No index mutation, refresh, diff contents or Git hooks.
pub async fn inspect(shell: &dyn Shell, plan: &Plan, cwd: &Path) -> Result<Option<Review>, String> {
    let mut broad = matches!(plan.operation, Operation::Commit);
    if let Operation::Stage {
        paths,
        broad: known,
        force,
    } = &plan.operation
    {
        broad = *known;
        if !broad {
            let prefix = probe(
                shell,
                plan,
                cwd,
                &args(&["rev-parse", "--show-prefix"]),
                false,
            )
            .await?;
            let prefix = String::from_utf8(prefix).map_err(|_| "Unsupported Git path encoding")?;
            let mut query = args(&[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "--full-name",
                "-z",
                "--",
            ]);
            if *force {
                query.retain(|arg| arg != "--exclude-standard");
            }
            query.extend(paths.iter().cloned());
            let files = probe(shell, plan, cwd, &query, false).await?;
            let expected: Vec<_> = paths
                .iter()
                .flat_map(|p| {
                    let forward = p.replace('\\', "/");
                    [
                        format!("{}{}", prefix.trim_end(), p.strip_prefix("./").unwrap_or(p)),
                        format!(
                            "{}{}",
                            prefix.trim_end(),
                            forward.strip_prefix("./").unwrap_or(&forward)
                        ),
                    ]
                })
                .collect();
            broad = files
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .any(|p| !expected.iter().any(|e| e.as_bytes() == p));
        }
        if !broad {
            return Ok(None);
        }
        if *force {
            return Err("Bulk forced staging cannot be previewed safely. Use git add --force -- <explicit-file> instead.".into());
        }
    }
    // A commit uses the repository-wide index, even when invoked from a child
    // directory. Normalize only the inspection location, never the user's call.
    let expected_git_dir = probe(
        shell,
        plan,
        cwd,
        &args(&["rev-parse", "--absolute-git-dir"]),
        false,
    )
    .await?;
    let root = probe(
        shell,
        plan,
        cwd,
        &args(&["rev-parse", "--show-toplevel"]),
        false,
    )
    .await?;
    let root = String::from_utf8(root).map_err(|_| "Unsupported Git path encoding")?;
    let mut repository_plan = plan.clone();
    repository_plan
        .git
        .extend(["-C".into(), root.trim_end_matches(['\r', '\n']).into()]);
    let plan = &repository_plan;
    let inspected_git_dir = probe(
        shell,
        plan,
        cwd,
        &args(&["rev-parse", "--absolute-git-dir"]),
        false,
    )
    .await?;
    if inspected_git_dir != expected_git_dir {
        return Err("Git inspection repository changed or has an ambiguous root path".into());
    }
    let head = probe(
        shell,
        plan,
        cwd,
        &args(&["rev-parse", "--verify", "--quiet", "HEAD"]),
        true,
    )
    .await?;
    let index = probe(
        shell,
        plan,
        cwd,
        &args(&[
            "diff",
            "--cached",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-color",
            "--no-renames",
            "--ignore-submodules=none",
            "--no-ext-diff",
            "--no-textconv",
        ]),
        false,
    )
    .await?;
    let mut snapshot = inspected_git_dir;
    snapshot.extend(head);
    snapshot.extend(
        probe(
            shell,
            plan,
            cwd,
            &args(&["ls-files", "--stage", "-z"]),
            false,
        )
        .await?,
    );
    snapshot.push(0);
    snapshot.extend(&index);
    let fields: Vec<_> = index.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
    if fields.len() % 2 != 0 {
        return Err("Unsupported Git index listing".into());
    }
    let mut lines = Vec::new();
    let mut count = 0usize;
    for pair in fields.chunks_exact(2) {
        count += 1;
        if lines.len() < 30 {
            let path = String::from_utf8_lossy(pair[1]);
            let status = String::from_utf8_lossy(pair[0])
                .split_whitespace()
                .last()
                .unwrap_or("?")
                .to_string();
            lines.push(format!("{status} {}", preview_path(&path)));
        }
    }
    let summary = if matches!(plan.operation, Operation::Stage { .. }) {
        let status = probe(
            shell,
            plan,
            cwd,
            &args(&[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=dirty",
            ]),
            false,
        )
        .await?;
        snapshot.extend(&status);
        let paths: Vec<_> = status
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .collect();
        let preview = paths
            .iter()
            .take(30)
            .map(|p| preview_path(&String::from_utf8_lossy(p)))
            .collect::<Vec<_>>()
            .join("; ");
        format!("Bulk staging may include unrelated changes, secrets, build outputs or deletions. Workspace candidate preview (first 30 status fields; not an ownership claim): {preview}. Prefer git add -- <explicit-files>.")
    } else {
        format!("Git commit includes {count} staged entries, including anything staged before this task. Review scope and any sensitive paths/deletions: {}{}", lines.join("; "), if count>30 { "; additional entries omitted" } else { "" })
    };
    Ok(Some(Review {
        fingerprint: format!("{:x}", Sha256::digest(&snapshot)),
        summary,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classification_is_argument_aware() {
        for command in [
            "git add -A",
            "git add --all",
            "git add .",
            "git add -u",
            "git add -- :",
            "git add '*.rs'",
            "'/usr/bin/git' -C '/tmp/a b' add --all",
            "git commit -m 'fix bug'",
            "git commit -m --dry-run",
            "git commit --message=fix --amend --no-edit --no-gpg-sign",
        ] {
            assert!(risky(command), "{command}");
        }
        for command in [
            "git add .gitignore",
            "git add -A -- src/main.rs",
            "git add --dry-run .",
            "git commit --dry-run",
            "git commit -m fix --dry-run",
            "echo 'git add -A'",
            "git status && git diff",
            "git show \"$revision\"",
            "git show \"$revision\" commit",
            "git -C \"$repo\" status",
        ] {
            assert!(!risky(command), "{command}");
        }
        for command in [
            "git add . && git commit -m fix",
            "cd repo && git commit -m fix",
            "bash -c 'git add -A'",
            "env git add .",
            "git commit -am fix",
            "git commit --only src/a",
            "git commit --include=src/a",
            "git commit -p",
            "git add -p",
            "git add -np",
            "git add -n -p",
            "git add --for ignored",
            "git add --inter",
            "git add -e file",
            "git commit --incl=file -m fix",
            "git commit --on=file -m fix",
            "git commit --pat",
            "git commit --future-index-changing-option",
            "git commit -Z",
            "git commit src/a",
            "git -c core.hooksPath=/tmp commit -m fix",
            "git add --pathspec-from-file=paths",
            "git add \"$file\"",
        ] {
            assert!(classify(command).is_err(), "{command}");
        }
    }

    #[test]
    fn windows_literals_keep_paths_and_reject_expansions_or_compounds() {
        for (dialect, command, directory) in [
            (
                ShellDialect::Cmd,
                r#""C:\Program Files\Git\cmd\git.exe" -C "C:\用户\repo & data" add -A"#,
                r"C:\用户\repo & data",
            ),
            (
                ShellDialect::PowerShell,
                r#"& 'C:\Program Files\Git\cmd\git.exe' -C 'C:\用户\repo''s $data' commit -m 'fix'"#,
                r"C:\用户\repo's $data",
            ),
        ] {
            let plan = classify_in(command, dialect).unwrap().unwrap();
            assert_eq!(plan.git[2], directory);
        }
        for (dialect, command) in [
            (ShellDialect::Cmd, r"git -C %REPO% add -A"),
            (ShellDialect::Cmd, "git add . & git commit -m fix"),
            (ShellDialect::Cmd, "git add . ^& echo hidden"),
            (ShellDialect::PowerShell, "git -C $repo add -A"),
            (ShellDialect::PowerShell, "git add .; git commit -m fix"),
            (ShellDialect::PowerShell, "git commit -am fix"),
            (ShellDialect::PowerShell, "git add --% -A"),
        ] {
            assert!(classify_in(command, dialect).is_err(), "{command}");
        }
        for dialect in [ShellDialect::Cmd, ShellDialect::PowerShell] {
            assert!(classify_in("git status", dialect).unwrap().is_none());
            assert!(classify_in("  git add -A\r\n", dialect).unwrap().is_some());
            assert!(classify_in("echo hello", dialect).unwrap().is_none());
            assert!(classify_in("echo git add -A", dialect).unwrap().is_none());
            assert!(classify_in("git log --grep git add", dialect)
                .unwrap()
                .is_none());
        }
    }

    #[tokio::test]
    async fn native_probes_preserve_spaces_unicode_and_shell_metacharacters() {
        let root = tempfile::Builder::new()
            .prefix("git argv ' & $ 中文 ")
            .tempdir()
            .unwrap();
        git(root.path(), &["init", "-q"]);
        std::fs::write(root.path().join("用户 & 文件.txt"), "user work").unwrap();
        git(root.path(), &["add", "--", "用户 & 文件.txt"]);
        let path = root.path().to_str().unwrap();
        let commands = [
            (
                ShellDialect::Cmd,
                format!("git -C \"{path}\" commit -m fix"),
            ),
            (
                ShellDialect::PowerShell,
                format!("& git -C '{}' commit -m fix", path.replace('\'', "''")),
            ),
        ];
        for (dialect, command) in commands {
            let plan = classify_in(&command, dialect).unwrap().unwrap();
            let review = inspect(&crate::world::LocalShell, &plan, root.path())
                .await
                .unwrap()
                .unwrap();
            assert!(review.summary.contains("用户 & 文件.txt"));
        }
    }

    fn git(root: &Path, argv: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(argv)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn an_ambiguous_root_path_cannot_review_a_different_repository() {
        let temp = tempfile::tempdir().unwrap();
        let ordinary = temp.path().join("repo");
        let unusual = temp.path().join("repo\n");
        for root in [&ordinary, &unusual] {
            std::fs::create_dir(root).unwrap();
            git(root, &["init", "-q"]);
        }
        std::fs::write(unusual.join("user.txt"), "existing work").unwrap();
        git(&unusual, &["add", "user.txt"]);
        let plan = classify("git commit -m fix").unwrap().unwrap();
        let error = inspect(&crate::world::LocalShell, &plan, &unusual)
            .await
            .unwrap_err();
        assert!(error.contains("ambiguous root"), "{error}");
    }

    #[tokio::test]
    async fn bulk_preview_includes_changed_submodule_heads() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        let child = root.path().join("child");
        std::fs::create_dir(&child).unwrap();
        git(&child, &["init", "-q"]);
        let commit = [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-verify",
            "-qm",
            "baseline",
        ];
        std::fs::write(child.join("file"), "one").unwrap();
        git(&child, &["add", "file"]);
        git(&child, &commit);
        git(root.path(), &["add", "child"]);
        git(root.path(), &commit);
        let plan = classify("git add -A").unwrap().unwrap();
        let before = inspect(&crate::world::LocalShell, &plan, root.path())
            .await
            .unwrap()
            .unwrap();
        std::fs::write(child.join("file"), "two").unwrap();
        git(&child, &["add", "file"]);
        git(&child, &commit);
        let after = inspect(&crate::world::LocalShell, &plan, root.path())
            .await
            .unwrap()
            .unwrap();
        assert!(after.summary.contains("child"), "{}", after.summary);
        assert_ne!(before.fingerprint, after.fingerprint);
    }

    #[tokio::test]
    async fn actual_index_is_read_only_and_blob_changes_invalidate_review() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        std::fs::write(root.path().join("user.txt"), "user work").unwrap();
        git(root.path(), &["add", "--", "user.txt"]);
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/a.rs"), "new code").unwrap();
        let index = root.path().join(".git/index");
        let before = std::fs::read(&index).unwrap();
        let shell = crate::world::LocalShell;
        let plan = classify("git commit -m fix").unwrap().unwrap();
        let first = inspect(&shell, &plan, root.path()).await.unwrap().unwrap();
        assert!(first.summary.contains("user.txt"));
        let from_child = inspect(&shell, &plan, &root.path().join("src"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first, from_child,
            "child cwd must still review the entire index"
        );
        let child_plan = classify("git -C src commit -m fix").unwrap().unwrap();
        assert_eq!(
            Some(first.clone()),
            inspect(&shell, &child_plan, root.path()).await.unwrap()
        );
        assert!(!first.summary.contains("src/a.rs"));
        assert_eq!(before, std::fs::read(&index).unwrap());
        assert_eq!(
            Some(first.clone()),
            inspect(&shell, &plan, root.path()).await.unwrap()
        );
        let file = classify("git add -- src/a.rs").unwrap().unwrap();
        assert!(inspect(&shell, &file, root.path()).await.unwrap().is_none());
        let directory = classify("git add -- src").unwrap().unwrap();
        assert!(inspect(&shell, &directory, root.path())
            .await
            .unwrap()
            .is_some());
        assert_eq!(before, std::fs::read(&index).unwrap());
        std::fs::write(root.path().join("user.txt"), "changed user work").unwrap();
        git(root.path(), &["add", "--", "user.txt"]);
        assert_ne!(
            first.fingerprint,
            inspect(&shell, &plan, root.path())
                .await
                .unwrap()
                .unwrap()
                .fingerprint
        );
        std::fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(root.path().join("ignored")).unwrap();
        std::fs::write(root.path().join("ignored/secret.txt"), "local only").unwrap();
        let forced_directory = classify("git add -f ignored").unwrap().unwrap();
        assert!(inspect(&shell, &forced_directory, root.path())
            .await
            .is_err());
        let forced_file = classify("git add -f -- ignored/secret.txt")
            .unwrap()
            .unwrap();
        assert!(inspect(&shell, &forced_file, root.path())
            .await
            .unwrap()
            .is_none());
        let forced = classify("git add -f .").unwrap().unwrap();
        assert!(inspect(&shell, &forced, root.path()).await.is_err());
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::world::{Exit, Process, SpawnError};
    use async_trait::async_trait;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    struct FakeProcess {
        oversized: bool,
        killed: AtomicBool,
    }
    #[async_trait]
    impl Process for FakeProcess {
        async fn next_chunk(&self) -> Option<Chunk> {
            if self.oversized {
                Some(Chunk::Stdout(vec![b'x'; LIMIT + 1]))
            } else {
                std::future::pending().await
            }
        }
        async fn wait(&self) -> Result<Exit, String> {
            std::future::pending().await
        }
        async fn kill(&self) {
            self.killed.store(true, Ordering::SeqCst);
        }
    }
    struct NoDirectShell;
    #[async_trait]
    impl Shell for NoDirectShell {
        fn describe(&self) -> String {
            "unsupported remote world".into()
        }
        async fn spawn(&self, _: &str, _: &SpawnOptions) -> Result<Arc<dyn Process>, SpawnError> {
            panic!("no shell/local fallback")
        }
    }
    #[tokio::test]
    async fn unsupported_world_fails_closed_without_shell_fallback() {
        let plan = classify("git commit -m fix").unwrap().unwrap();
        let error = inspect(&NoDirectShell, &plan, Path::new("/remote/repo"))
            .await
            .unwrap_err();
        assert!(
            error.contains("does not support direct program execution"),
            "{error}"
        );
    }
    struct FakeShell(Arc<FakeProcess>);
    #[async_trait]
    impl Shell for FakeShell {
        fn describe(&self) -> String {
            "remote test world".into()
        }
        async fn spawn(&self, _: &str, _: &SpawnOptions) -> Result<Arc<dyn Process>, SpawnError> {
            panic!("inspection must never use shell source")
        }
        async fn spawn_program(
            &self,
            program: &str,
            arguments: &[String],
            options: &SpawnOptions,
        ) -> Result<Arc<dyn Process>, SpawnError> {
            assert_eq!(program, "git");
            assert!(arguments.iter().any(|arg| arg == "core.fsmonitor=false"));
            assert_eq!(options.cwd.as_deref(), Some(Path::new("/remote/repo")));
            assert!(options
                .env
                .contains(&("GIT_OPTIONAL_LOCKS".into(), "0".into())));
            Ok(self.0.clone())
        }
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn remote_inspection_times_out_and_kills_without_local_fallback() {
        for oversized in [false, true] {
            let process = Arc::new(FakeProcess {
                oversized,
                killed: AtomicBool::new(false),
            });
            let plan = classify("git commit -m fix").unwrap().unwrap();
            let error = inspect(
                &FakeShell(process.clone()),
                &plan,
                Path::new("/remote/repo"),
            )
            .await
            .unwrap_err();
            assert!(
                error.contains(if oversized { "limit" } else { "timed out" }),
                "{error}"
            );
            assert!(process.killed.load(Ordering::SeqCst));
        }
    }
}
