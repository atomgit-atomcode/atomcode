use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::render::diff::parse_unified_diff_files;
use crate::render::DiffEntry;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const METADATA_LIMIT: usize = 512 * 1024;
const PATCH_LIMIT: usize = 2 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const MAX_FILES: usize = 2_000;
// Rendered diff lines per file (added + removed + context + hunk separators).
// The viewer scrolls, so this is generous — it exists only to bound a
// pathologically huge single-file diff, not normal refactors. A low cap made
// the "snapshot truncated" notice fire on everyday file edits.
const MAX_ENTRIES_PER_FILE: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffBase {
    Head,
    Unborn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffScope {
    Combined,
    Staged,
    Unstaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffFileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    ModeChanged,
    Untracked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffContent {
    Text,
    Binary,
    Untracked,
    Truncated,
}

#[derive(Debug, Clone)]
pub(crate) struct DiffSection {
    pub scope: DiffScope,
    pub entries: Vec<DiffEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct DiffFile {
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub status: DiffFileStatus,
    pub additions: Option<usize>,
    pub deletions: Option<usize>,
    pub content: DiffContent,
    pub sections: Vec<DiffSection>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct DiffSnapshot {
    pub base: DiffBase,
    pub files_changed: usize,
    pub additions: usize,
    pub deletions: usize,
    pub files: Vec<DiffFile>,
    pub truncated: bool,
}

#[derive(Debug)]
struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
}

#[derive(Debug, Clone)]
struct NumstatRow {
    path: PathBuf,
    old_path: Option<PathBuf>,
    additions: Option<usize>,
    deletions: Option<usize>,
}

#[derive(Debug)]
struct PatchSection {
    scope: DiffScope,
    files: Vec<(PathBuf, Vec<DiffEntry>)>,
    truncated: bool,
    truncated_files: HashSet<PathBuf>,
}

pub(crate) fn capture_diff_snapshot(working_dir: &Path) -> Result<DiffSnapshot, String> {
    let repo_root = git_repo_root(working_dir)?;
    let base = if git_success(&repo_root, &["rev-parse", "--verify", "HEAD"])? {
        DiffBase::Head
    } else {
        DiffBase::Unborn
    };
    let comparison = match base {
        DiffBase::Head => "HEAD",
        DiffBase::Unborn => EMPTY_TREE,
    };

    let numstat = git_required(
        &repo_root,
        &[
            "diff",
            comparison,
            "--find-renames",
            "--numstat",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--",
        ],
        METADATA_LIMIT,
        false,
    )?;
    let rows = parse_numstat(&numstat)?;
    let additions = rows.iter().filter_map(|row| row.additions).sum();
    let deletions = rows.iter().filter_map(|row| row.deletions).sum();

    let (combined_patch, combined_truncated) = git_capture(
        &repo_root,
        &[
            "diff",
            comparison,
            "--find-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--",
        ],
        PATCH_LIMIT,
        true,
    )?;
    let combined_parsed =
        parse_unified_diff_files(&String::from_utf8_lossy(&combined_patch), usize::MAX);

    let patch_sections = match base {
        DiffBase::Head => vec![PatchSection {
            scope: DiffScope::Combined,
            files: combined_parsed
                .files
                .iter()
                .enumerate()
                .filter_map(|(index, file)| {
                    rows.get(index)
                        .map(|row| (row.path.clone(), file.entries.clone()))
                })
                .collect(),
            truncated: combined_truncated,
            truncated_files: possibly_truncated_files(
                &rows,
                combined_parsed.files.len(),
                combined_truncated,
            ),
        }],
        DiffBase::Unborn => vec![
            capture_patch_section(
                &repo_root,
                DiffScope::Staged,
                &[
                    "diff",
                    "--cached",
                    "--find-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--",
                ],
            )?,
            capture_patch_section(
                &repo_root,
                DiffScope::Unstaged,
                &[
                    "diff",
                    "--find-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--",
                ],
            )?,
        ],
    };

    let tracked_count = rows.len();
    let mut files = Vec::with_capacity(tracked_count.min(MAX_FILES));
    let mut truncated = combined_truncated
        || combined_parsed.truncated
        || patch_sections.iter().any(|section| section.truncated);
    for (index, row) in rows.iter().take(MAX_FILES).enumerate() {
        let parsed = combined_parsed.files.get(index);
        let summary = parsed.and_then(|file| file.summary.as_deref());
        let mut sections = Vec::new();
        let mut file_truncated = patch_sections
            .iter()
            .any(|section| section.truncated_files.contains(&row.path));
        for section in &patch_sections {
            if let Some((_, entries)) = section.files.iter().find(|(path, _)| path == &row.path) {
                if !entries.is_empty() {
                    file_truncated |= entries.len() > MAX_ENTRIES_PER_FILE;
                    truncated |= entries.len() > MAX_ENTRIES_PER_FILE;
                    sections.push(DiffSection {
                        scope: section.scope,
                        entries: entries.iter().take(MAX_ENTRIES_PER_FILE).cloned().collect(),
                    });
                }
            }
        }
        let content = if summary.is_some_and(is_binary_summary) {
            DiffContent::Binary
        } else if file_truncated {
            DiffContent::Truncated
        } else {
            DiffContent::Text
        };
        files.push(DiffFile {
            status: status_from_patch(summary, row.old_path.is_some()),
            path: row.path.clone(),
            old_path: row.old_path.clone(),
            additions: row.additions,
            deletions: row.deletions,
            content,
            sections,
            truncated: file_truncated,
        });
    }
    truncated |= tracked_count > MAX_FILES;

    let untracked = git_required(
        &repo_root,
        &["ls-files", "--others", "--exclude-standard", "-z", "--"],
        METADATA_LIMIT,
        false,
    )?;
    let untracked_paths: Vec<PathBuf> = untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(bytes_to_path)
        .collect();
    let files_changed = tracked_count.saturating_add(untracked_paths.len());
    for path in untracked_paths {
        if files.len() >= MAX_FILES {
            truncated = true;
            break;
        }
        files.push(DiffFile {
            path,
            old_path: None,
            status: DiffFileStatus::Untracked,
            additions: None,
            deletions: None,
            content: DiffContent::Untracked,
            sections: Vec::new(),
            truncated: false,
        });
    }

    Ok(DiffSnapshot {
        base,
        files_changed,
        additions,
        deletions,
        files,
        truncated,
    })
}

pub(crate) fn format_compact_snapshot(snapshot: &DiffSnapshot) -> String {
    let mut output = String::new();
    for file in &snapshot.files {
        output.push_str("  ");
        output.push_str(&display_path(&file.path));
        match (file.additions, file.deletions) {
            (Some(additions), Some(deletions)) => {
                output.push_str(&format!("  +{additions} -{deletions}"));
            }
            _ if file.status == DiffFileStatus::Untracked => output.push_str("  untracked"),
            _ => output.push_str("  binary"),
        }
        output.push('\n');
    }
    output.push_str(&format!(
        "{} files changed, +{} -{}",
        snapshot.files_changed, snapshot.additions, snapshot.deletions
    ));
    if snapshot.truncated {
        output.push('\n');
        output.push_str(crate::i18n::t(crate::i18n::Msg::CmdDiffTruncated).trim());
    }
    output
}

pub(crate) fn display_path(path: &Path) -> String {
    let mut output = String::new();
    for ch in path.to_string_lossy().chars() {
        match ch {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{{{:x}}}", ch as u32);
            }
            ch => output.push(ch),
        }
    }
    output
}

fn capture_patch_section(
    repo_root: &Path,
    scope: DiffScope,
    args: &[&str],
) -> Result<PatchSection, String> {
    let (patch, truncated) = git_capture(repo_root, args, PATCH_LIMIT, true)?;
    let parsed = parse_unified_diff_files(&String::from_utf8_lossy(&patch), usize::MAX);
    let mut numstat_args = args.to_vec();
    let insert_at = numstat_args
        .iter()
        .position(|arg| *arg == "--no-ext-diff")
        .unwrap_or(1);
    numstat_args.splice(insert_at..insert_at, ["--numstat", "-z"]);
    let rows = parse_numstat(&git_required(
        repo_root,
        &numstat_args,
        METADATA_LIMIT,
        false,
    )?)?;
    let truncated_files = possibly_truncated_files(&rows, parsed.files.len(), truncated);
    Ok(PatchSection {
        scope,
        files: rows
            .iter()
            .zip(parsed.files.into_iter().map(|file| file.entries))
            .map(|(row, entries)| (row.path.clone(), entries))
            .collect(),
        truncated,
        truncated_files,
    })
}

fn possibly_truncated_files(
    rows: &[NumstatRow],
    parsed_file_count: usize,
    truncated: bool,
) -> HashSet<PathBuf> {
    if !truncated || rows.is_empty() {
        return HashSet::new();
    }
    let first_possibly_truncated = parsed_file_count.saturating_sub(1).min(rows.len());
    rows[first_possibly_truncated..]
        .iter()
        .map(|row| row.path.clone())
        .collect()
}

fn git_repo_root(working_dir: &Path) -> Result<PathBuf, String> {
    let raw = git_required(
        working_dir,
        &["rev-parse", "--show-toplevel"],
        METADATA_LIMIT,
        false,
    )?;
    let root = raw.strip_suffix(b"\n").unwrap_or(&raw);
    if root.is_empty() {
        return Err("git 未返回仓库根目录".to_string());
    }
    Ok(bytes_to_path(root))
}

fn git_success(repo_root: &Path, args: &[&str]) -> Result<bool, String> {
    let output = run_git(repo_root, args, METADATA_LIMIT)?;
    Ok(output.status.success())
}

fn git_required(
    repo_root: &Path,
    args: &[&str],
    stdout_limit: usize,
    allow_truncated: bool,
) -> Result<Vec<u8>, String> {
    git_capture(repo_root, args, stdout_limit, allow_truncated).map(|(stdout, _)| stdout)
}

fn git_capture(
    repo_root: &Path,
    args: &[&str],
    stdout_limit: usize,
    allow_truncated: bool,
) -> Result<(Vec<u8>, bool), String> {
    let output = run_git(repo_root, args, stdout_limit)?;
    if !output.status.success() && !(allow_truncated && output.stdout_truncated) {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git {} 失败：{}",
            args.first().copied().unwrap_or("命令"),
            detail.trim()
        ));
    }
    if output.stdout_truncated && !allow_truncated {
        return Err(format!(
            "git {} 输出超过 {} KiB，无法可靠展示",
            args.first().copied().unwrap_or("命令"),
            stdout_limit / 1024
        ));
    }
    Ok((output.stdout, output.stdout_truncated))
}

fn run_git(repo_root: &Path, args: &[&str], stdout_limit: usize) -> Result<CommandOutput, String> {
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动 git：{error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取 git 输出".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取 git 错误输出".to_string())?;
    let exceeded = Arc::new(AtomicBool::new(false));
    let stdout_exceeded = Arc::clone(&exceeded);
    let stdout_reader =
        std::thread::spawn(move || read_bounded(stdout, stdout_limit, stdout_exceeded));
    let stderr_reader =
        std::thread::spawn(move || read_prefix_while_draining(stderr, STDERR_LIMIT));

    let started = Instant::now();
    let status = loop {
        if exceeded.load(Ordering::Relaxed) {
            let _ = child.kill();
            break child
                .wait()
                .map_err(|error| format!("等待 git 退出失败：{error}"))?;
        }
        if started.elapsed() >= COMMAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("git 命令执行超过 {} 秒", COMMAND_TIMEOUT.as_secs()));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("查询 git 状态失败：{error}"))?
        {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "读取 git 输出的线程异常退出".to_string())?
        .map_err(|error| format!("读取 git 输出失败：{error}"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "读取 git 错误输出的线程异常退出".to_string())?
        .map_err(|error| format!("读取 git 错误输出失败：{error}"))?;
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
        stdout_truncated: exceeded.load(Ordering::Relaxed),
    })
}

fn read_bounded(
    mut reader: impl Read,
    limit: usize,
    exceeded: Arc<AtomicBool>,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            exceeded.store(true, Ordering::Relaxed);
            break;
        }
    }
    Ok(output)
}

fn read_prefix_while_draining(mut reader: impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if output.len() < limit {
            let remaining = limit - output.len();
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
    }
    Ok(output)
}

fn parse_numstat(raw: &[u8]) -> Result<Vec<NumstatRow>, String> {
    let records: Vec<&[u8]> = raw.split(|byte| *byte == 0).collect();
    let mut rows = Vec::new();
    let mut index = 0usize;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(3, |byte| *byte == b'\t');
        let additions = parse_count(fields.next())?;
        let deletions = parse_count(fields.next())?;
        let path_field = fields
            .next()
            .ok_or_else(|| "git numstat 缺少文件路径".to_string())?;
        let (old_path, path) = if path_field.is_empty() {
            let old = records
                .get(index)
                .ok_or_else(|| "git numstat 缺少重命名前路径".to_string())?;
            let new = records
                .get(index + 1)
                .ok_or_else(|| "git numstat 缺少重命名后路径".to_string())?;
            index += 2;
            (Some(bytes_to_path(old)), bytes_to_path(new))
        } else {
            (None, bytes_to_path(path_field))
        };
        rows.push(NumstatRow {
            path,
            old_path,
            additions,
            deletions,
        });
    }
    Ok(rows)
}

fn parse_count(field: Option<&[u8]>) -> Result<Option<usize>, String> {
    let field = field.ok_or_else(|| "git numstat 缺少行数".to_string())?;
    if field == b"-" {
        return Ok(None);
    }
    let text = std::str::from_utf8(field).map_err(|_| "git numstat 行数不是 UTF-8".to_string())?;
    text.parse::<usize>()
        .map(Some)
        .map_err(|_| format!("git numstat 行数无效：{text}"))
}

fn status_from_patch(summary: Option<&str>, renamed: bool) -> DiffFileStatus {
    if renamed {
        DiffFileStatus::Renamed
    } else if summary.is_some_and(|text| text.contains("new file mode ")) {
        DiffFileStatus::Added
    } else if summary.is_some_and(|text| text.contains("deleted file mode ")) {
        DiffFileStatus::Deleted
    } else if summary.is_some_and(|text| text.contains("old mode ") && text.contains("new mode ")) {
        DiffFileStatus::ModeChanged
    } else {
        DiffFileStatus::Modified
    }
}

fn is_binary_summary(summary: &str) -> bool {
    summary.contains("Binary files ") || summary.contains("GIT binary patch")
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn committed_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("temp git repo");
        git(repo.path(), &["init"]);
        std::fs::write(repo.path().join("tracked.txt"), "base\n").expect("write tracked fixture");
        git(repo.path(), &["add", "tracked.txt"]);
        git(
            repo.path(),
            &[
                "-c",
                "user.email=tuix@local",
                "-c",
                "user.name=tuix",
                "commit",
                "-m",
                "init",
            ],
        );
        repo
    }

    #[test]
    fn staged_only_changes_are_part_of_the_head_snapshot() {
        let repo = committed_repo();
        std::fs::write(repo.path().join("tracked.txt"), "staged\n").expect("change fixture");
        git(repo.path(), &["add", "tracked.txt"]);

        let snapshot = capture_diff_snapshot(repo.path()).expect("capture staged diff");

        assert_eq!(snapshot.base, DiffBase::Head);
        assert_eq!(snapshot.files_changed, 1);
        assert_eq!(snapshot.additions, 1);
        assert_eq!(snapshot.deletions, 1);
        assert_eq!(snapshot.files[0].path, PathBuf::from("tracked.txt"));
        assert_eq!(snapshot.files[0].sections[0].scope, DiffScope::Combined);
        assert!(snapshot.files[0].sections[0]
            .entries
            .iter()
            .any(|entry| entry.text == "staged"));
    }

    #[test]
    fn untracked_files_are_listed_without_fake_line_counts() {
        let repo = committed_repo();
        std::fs::write(repo.path().join("new file.txt"), "not indexed\n")
            .expect("write untracked fixture");

        let snapshot = capture_diff_snapshot(repo.path()).expect("capture untracked diff");

        assert_eq!(snapshot.files_changed, 1);
        assert_eq!(snapshot.additions, 0);
        assert_eq!(snapshot.deletions, 0);
        assert_eq!(snapshot.files[0].path, PathBuf::from("new file.txt"));
        assert_eq!(snapshot.files[0].status, DiffFileStatus::Untracked);
        assert_eq!(snapshot.files[0].content, DiffContent::Untracked);
        assert_eq!(snapshot.files[0].additions, None);
        assert_eq!(snapshot.files[0].deletions, None);
    }

    #[test]
    fn unborn_repository_keeps_staged_and_unstaged_sections_explicit() {
        let repo = tempfile::tempdir().expect("temp git repo");
        git(repo.path(), &["init"]);
        std::fs::write(repo.path().join("new.txt"), "staged\n").expect("write staged fixture");
        git(repo.path(), &["add", "new.txt"]);
        std::fs::write(repo.path().join("new.txt"), "staged\nworking\n")
            .expect("write unstaged fixture");

        let snapshot = capture_diff_snapshot(repo.path()).expect("capture unborn diff");

        assert_eq!(snapshot.base, DiffBase::Unborn);
        assert_eq!(snapshot.files_changed, 1);
        assert_eq!(snapshot.files[0].sections.len(), 2);
        assert_eq!(snapshot.files[0].sections[0].scope, DiffScope::Staged);
        assert_eq!(snapshot.files[0].sections[1].scope, DiffScope::Unstaged);
    }

    #[test]
    fn unborn_sections_are_matched_by_path_not_patch_position() {
        let repo = tempfile::tempdir().expect("temp git repo");
        git(repo.path(), &["init"]);
        std::fs::write(repo.path().join("a.txt"), "a staged\n").expect("write a fixture");
        std::fs::write(repo.path().join("b.txt"), "b staged\n").expect("write b fixture");
        git(repo.path(), &["add", "a.txt", "b.txt"]);
        std::fs::write(repo.path().join("b.txt"), "b staged\nb working\n")
            .expect("change b fixture");

        let snapshot = capture_diff_snapshot(repo.path()).expect("capture unborn diff");

        let a = snapshot
            .files
            .iter()
            .find(|file| file.path == Path::new("a.txt"))
            .expect("a file");
        let b = snapshot
            .files
            .iter()
            .find(|file| file.path == Path::new("b.txt"))
            .expect("b file");
        assert_eq!(a.sections.len(), 1);
        assert_eq!(a.sections[0].scope, DiffScope::Staged);
        assert_eq!(b.sections.len(), 2);
        assert_eq!(b.sections[0].scope, DiffScope::Staged);
        assert_eq!(b.sections[1].scope, DiffScope::Unstaged);
    }

    #[test]
    fn rename_keeps_old_and_new_paths() {
        let repo = committed_repo();
        git(repo.path(), &["mv", "tracked.txt", "renamed.txt"]);

        let snapshot = capture_diff_snapshot(repo.path()).expect("capture rename");

        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].status, DiffFileStatus::Renamed);
        assert_eq!(
            snapshot.files[0].old_path,
            Some(PathBuf::from("tracked.txt"))
        );
        assert_eq!(snapshot.files[0].path, PathBuf::from("renamed.txt"));
    }

    #[test]
    fn binary_changes_have_unknown_line_counts_and_no_fake_hunks() {
        let repo = committed_repo();
        std::fs::write(repo.path().join("binary.bin"), [0, 1, 2, 3]).expect("write binary fixture");
        git(repo.path(), &["add", "binary.bin"]);
        git(
            repo.path(),
            &[
                "-c",
                "user.email=tuix@local",
                "-c",
                "user.name=tuix",
                "commit",
                "-m",
                "binary",
            ],
        );
        std::fs::write(repo.path().join("binary.bin"), [0, 4, 5, 6])
            .expect("change binary fixture");

        let snapshot = capture_diff_snapshot(repo.path()).expect("capture binary diff");

        let binary = snapshot
            .files
            .iter()
            .find(|file| file.path == Path::new("binary.bin"))
            .expect("binary file");
        assert_eq!(binary.content, DiffContent::Binary);
        assert_eq!(binary.additions, None);
        assert_eq!(binary.deletions, None);
        assert!(binary.sections.is_empty());
    }

    #[test]
    fn non_git_directory_is_an_error_not_a_clean_snapshot() {
        let dir = tempfile::tempdir().expect("plain tempdir");
        let error = capture_diff_snapshot(dir.path()).expect_err("non-git must fail");
        assert!(error.contains("git"), "unexpected error: {error}");
    }

    #[test]
    fn display_path_escapes_line_breaks_tabs_and_terminal_controls() {
        let path = Path::new("line\nnext\t\u{1b}[2J.rs");
        assert_eq!(display_path(path), "line\\nnext\\t\\u{1b}[2J.rs");
    }

    #[test]
    fn files_after_the_patch_limit_are_marked_truncated() {
        let repo = committed_repo();
        std::fs::write(repo.path().join("a-large.txt"), "before\n").expect("write large base");
        std::fs::write(repo.path().join("z-after.txt"), "before\n").expect("write trailing base");
        git(repo.path(), &["add", "a-large.txt", "z-after.txt"]);
        git(
            repo.path(),
            &[
                "-c",
                "user.email=tuix@local",
                "-c",
                "user.name=tuix",
                "commit",
                "-m",
                "add truncation fixtures",
            ],
        );

        let mut large = vec![b'x'; PATCH_LIMIT + 64 * 1024];
        large.push(b'\n');
        std::fs::write(repo.path().join("a-large.txt"), large).expect("write oversized diff");
        std::fs::write(repo.path().join("z-after.txt"), "after\n").expect("change trailing file");

        let snapshot = capture_diff_snapshot(repo.path()).expect("capture bounded diff");
        let trailing = snapshot
            .files
            .iter()
            .find(|file| file.path == Path::new("z-after.txt"))
            .expect("trailing file");

        assert!(snapshot.truncated);
        assert_eq!(trailing.content, DiffContent::Truncated);
        assert!(trailing.truncated);
    }

    #[test]
    fn pipe_read_errors_are_not_treated_as_eof() {
        struct FailingReader {
            emitted: bool,
        }

        impl std::io::Read for FailingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.emitted {
                    return Err(std::io::Error::other("injected read failure"));
                }
                self.emitted = true;
                buffer[..7].copy_from_slice(b"partial");
                Ok(7)
            }
        }

        let exceeded = Arc::new(AtomicBool::new(false));
        let stdout_error = read_bounded(
            FailingReader { emitted: false },
            1024,
            Arc::clone(&exceeded),
        )
        .expect_err("stdout read failure must propagate");
        let stderr_error = read_prefix_while_draining(FailingReader { emitted: false }, 1024)
            .expect_err("stderr read failure must propagate");

        assert_eq!(stdout_error.kind(), std::io::ErrorKind::Other);
        assert_eq!(stderr_error.kind(), std::io::ErrorKind::Other);
        assert!(!exceeded.load(Ordering::Relaxed));
    }
}
