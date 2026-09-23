//! What the checkout looks like right now, as git sees it.
//!
//! **A different question from `/diff`'s default, not a deeper version of it.**
//! The session's own diff answers "what has this agent done"; this answers "how
//! dirty is my tree" — including everything a person changed in their editor
//! before the session started, and everything they staged by hand. A coding
//! session asks the first more often, which is why it is the default; but the
//! second is the one you need before committing, and it was the only one the
//! classic front end had (`docs/plans/2026-09-23-tuix-parity-remaining.md`,
//! 决策 1: both, with the session's as the default).
//!
//! **Not the checkpoint repo.** [`crate::session::rewind`] runs git against a
//! shadow `--git-dir` holding this session's snapshots; the answer here comes
//! from the person's own repository, and the two must never be confused — a
//! file staged in the shadow repo means nothing to anybody.
//!
//! The parsing is separated from the running for the usual reason: `git
//! status`'s output format is the whole of what is worth judging here, and a
//! judgement that needed a repository on disk to reach it would be judging
//! `git` as much as this.

use std::path::Path;
use std::process::Command;

/// What happened to one file, on one side.
///
/// Git spells these as letters in `git status --porcelain`; they are named here
/// because a screen that drew `R` would be asking every reader to know git's
/// alphabet, and because the set is closed — an unknown letter is
/// [`Status::Other`] rather than a parse failure, so a future git cannot make
/// this refuse to answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    /// Not in the index at all: a new file nobody has told git about.
    Untracked,
    /// A merge left both sides in it.
    Conflicted,
    /// A letter this build does not know. Shown as changed rather than dropped.
    Other,
}

impl Status {
    fn from_letter(c: u8) -> Option<Self> {
        Some(match c {
            b' ' => return None,
            b'A' => Self::Added,
            b'M' => Self::Modified,
            b'D' => Self::Deleted,
            b'R' => Self::Renamed,
            b'C' => Self::Copied,
            b'?' => Self::Untracked,
            b'U' => Self::Conflicted,
            _ => Self::Other,
        })
    }
}

/// One file the checkout has something to say about.
///
/// **Both sides, separately.** Git tracks the index and the working tree
/// independently, and a file can be modified in both — staged one way and
/// edited again since. Collapsing them into one状态 is what makes a person
/// commit something other than what they read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeFile {
    pub path: String,
    /// What the index has, against `HEAD`.
    pub staged: Option<Status>,
    /// What the working tree has, against the index.
    pub unstaged: Option<Status>,
}

impl WorktreeFile {
    /// Whether any of this file is staged — what the two sections of a `/diff`
    /// listing are split by.
    pub fn is_staged(&self) -> bool {
        self.staged.is_some()
    }
}

/// Read `git status --porcelain -z` output into files.
///
/// **`-z`, not lines.** A path with a newline in it is legal on every platform
/// this runs on, and git's line-oriented output quotes and escapes such a path
/// — so a line parser sees one entry as two, and the second is nonsense. The
/// NUL-separated form is the only one that is unambiguous, which is why it is
/// worth the slightly fiddlier parse.
///
/// A rename entry carries two paths (`to\0from`); the new name is what a person
/// is looking at, so that is what is reported.
pub fn parse_porcelain_z(out: &[u8]) -> Vec<WorktreeFile> {
    let mut files = Vec::new();
    let mut records = out.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        // `XY <path>`: two status letters, a space, then the path.
        if record.len() < 4 {
            continue;
        }
        let (x, y) = (record[0], record[1]);
        let path = String::from_utf8_lossy(&record[3..]).into_owned();
        let (staged, unstaged) = if x == b'?' && y == b'?' {
            // Untracked is neither staged nor a working-tree modification; it
            // is a file git has never heard of, and saying that once is truer
            // than saying it twice.
            (None, Some(Status::Untracked))
        } else {
            (Status::from_letter(x), Status::from_letter(y))
        };
        if staged.is_none() && unstaged.is_none() {
            continue;
        }
        // A rename or copy is followed by its source path, in its own record.
        if matches!(staged, Some(Status::Renamed) | Some(Status::Copied)) {
            let _from = records.next();
        }
        files.push(WorktreeFile {
            path,
            staged,
            unstaged,
        });
    }
    files
}

/// `additions, deletions, path` from `git diff --numstat -z` output.
///
/// The counts git will not give — a binary file — arrive as `-`, and that is
/// reported as such rather than as zero: "changed by nothing" and "changed by
/// an amount nobody can count" are different things to read.
pub fn parse_numstat_z(out: &[u8]) -> Vec<(u64, u64, bool, String)> {
    let mut found = Vec::new();
    // In `-z` form the counts and the path are one record for ordinary files
    // (`add\tdel\tpath\0`) but a rename splits into three (`add\tdel\0to\0from`).
    let mut records = out.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        let text = String::from_utf8_lossy(record);
        let mut fields = text.splitn(3, '\t');
        let (Some(added), Some(removed)) = (fields.next(), fields.next()) else {
            continue;
        };
        let binary = added == "-" || removed == "-";
        let (added, removed) = (added.parse().unwrap_or(0), removed.parse().unwrap_or(0));
        match fields.next() {
            Some(path) if !path.is_empty() => {
                found.push((added, removed, binary, path.to_string()))
            }
            // A rename: the new name is the next record, the old one the one
            // after. The new name is what the listing shows.
            _ => {
                let Some(to) = records.next() else { break };
                let _from = records.next();
                found.push((
                    added,
                    removed,
                    binary,
                    String::from_utf8_lossy(to).into_owned(),
                ));
            }
        }
    }
    found
}

/// Run `git` in `at` and hand back its stdout, or why it could not.
fn git(at: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(at)
        // A pager would hang forever with no terminal to page into, and a
        // prompt for credentials would hang waiting for an answer nobody is
        // there to give.
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|why| format!("git: {why}"))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        return Err(said.trim().to_string());
    }
    Ok(out.stdout)
}

/// Every file the checkout at `at` has something to say about, with how much
/// each changed.
///
/// `Err` is "this is not a repository, or git could not be run" — which is an
/// answer a person needs, not a failure to hide: `/diff git` outside a repo has
/// to say so rather than show an empty list that reads as "nothing changed".
pub fn read(at: &Path) -> Result<Vec<(WorktreeFile, u64, u64, bool)>, String> {
    let files = parse_porcelain_z(&git(at, &["status", "--porcelain", "-z"])?);
    // Counts for both sides in one read: `HEAD` compares the working tree to
    // the last commit, which is the number a person means by "how much has
    // changed here" whether or not they have staged any of it.
    let counted = parse_numstat_z(&git(at, &["diff", "--numstat", "-z", "HEAD"])?);
    Ok(files
        .into_iter()
        .map(|file| {
            let found = counted.iter().find(|(_, _, _, path)| *path == file.path);
            match found {
                Some((added, removed, binary, _)) => (file, *added, *removed, *binary),
                // An untracked file is in no diff against HEAD — git has never
                // heard of it — so it has no counts. Shown as changed with
                // nothing counted rather than left out, which is what a person
                // looking for "what is in my tree" needs.
                None => (file, 0, 0, false),
            }
        })
        .collect())
}

/// The unified diff of one file in the checkout at `at`, against `HEAD`.
pub fn file_diff(at: &Path, path: &str) -> Result<String, String> {
    // `--` so a path that looks like a revision is still read as a path.
    let out = git(at, &["diff", "HEAD", "--", path])?;
    let text = String::from_utf8_lossy(&out).into_owned();
    if !text.is_empty() {
        return Ok(text);
    }
    // Empty against HEAD means untracked: git has nothing to compare. Ask it to
    // diff the file against nothing, which is how `git diff` shows a new file.
    let out = git(at, &["diff", "--no-index", "--", "/dev/null", path]).unwrap_or_default();
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(records: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for r in records {
            out.extend_from_slice(r.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn both_sides_of_a_file_are_kept_apart() {
        let got = parse_porcelain_z(&z(&["MM src/a.rs"]));
        assert_eq!(
            got,
            vec![WorktreeFile {
                path: "src/a.rs".into(),
                staged: Some(Status::Modified),
                unstaged: Some(Status::Modified),
            }],
            "staged one way and edited again since is two facts, not one"
        );

        let got = parse_porcelain_z(&z(&["M  staged.rs", " M dirty.rs"]));
        assert_eq!(got[0].staged, Some(Status::Modified));
        assert_eq!(got[0].unstaged, None);
        assert_eq!(got[1].staged, None);
        assert_eq!(got[1].unstaged, Some(Status::Modified));
        assert!(got[0].is_staged() && !got[1].is_staged());
    }

    #[test]
    fn the_letters_git_uses_are_named() {
        let got = parse_porcelain_z(&z(&[
            "A  new.rs",
            "D  gone.rs",
            "?? untracked.rs",
            "UU conflict.rs",
        ]));
        assert_eq!(got[0].staged, Some(Status::Added));
        assert_eq!(got[1].staged, Some(Status::Deleted));
        assert_eq!(
            (got[2].staged, got[2].unstaged),
            (None, Some(Status::Untracked)),
            "untracked is one fact: git has never heard of it"
        );
        assert_eq!(got[3].staged, Some(Status::Conflicted));
    }

    /// A letter this build does not know must not make it refuse to answer.
    #[test]
    fn an_unknown_letter_is_still_a_changed_file() {
        let got = parse_porcelain_z(&z(&["Z  strange.rs"]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].staged, Some(Status::Other));
    }

    /// **The reason this reads `-z` and not lines.** A path with a newline is
    /// legal, and git's line form quotes and escapes it — one entry read as
    /// two, the second nonsense.
    #[test]
    fn a_path_with_a_newline_in_it_is_one_file() {
        let got = parse_porcelain_z(&z(&[" M src/od\nd.rs", " M src/b.rs"]));
        assert_eq!(got.len(), 2, "two files, not three: {got:?}");
        assert_eq!(got[0].path, "src/od\nd.rs");
        assert_eq!(got[1].path, "src/b.rs");
    }

    /// A rename carries its old name in a record of its own, and that record is
    /// not a second file.
    #[test]
    fn a_rename_reports_the_new_name_once() {
        let got = parse_porcelain_z(&z(&["R  new.rs", "old.rs", " M other.rs"]));
        assert_eq!(got.len(), 2, "the source path is not a file: {got:?}");
        assert_eq!(got[0].path, "new.rs");
        assert_eq!(got[0].staged, Some(Status::Renamed));
        assert_eq!(got[1].path, "other.rs");
    }

    #[test]
    fn numstat_reads_counts_and_says_which_are_uncountable() {
        let got = parse_numstat_z(&z(&["3\t1\tsrc/a.rs", "-\t-\tlogo.png"]));
        assert_eq!(got[0], (3, 1, false, "src/a.rs".to_string()));
        assert_eq!(
            got[1],
            (0, 0, true, "logo.png".to_string()),
            "a binary file is uncountable, which is not the same as zero"
        );
    }

    #[test]
    fn numstat_reads_a_rename_that_split_across_records() {
        let got = parse_numstat_z(&z(&["2\t2", "new.rs", "old.rs", "1\t0\tplain.rs"]));
        assert_eq!(got[0], (2, 2, false, "new.rs".to_string()));
        assert_eq!(
            got[1],
            (1, 0, false, "plain.rs".to_string()),
            "and the record after a rename is read as its own file"
        );
    }
}
