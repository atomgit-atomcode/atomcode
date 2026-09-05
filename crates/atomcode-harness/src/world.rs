//! The execution world: filesystem, processes, and the shell built on them.
//!
//! These three seams share one property that makes them worth defining
//! together: they all answer "where does this actually happen?". Point them at a
//! container, a remote sandbox or a read-only view and every tool that goes
//! through them moves with no per-tool fork — which is the difference between a
//! sandbox feature and a sandbox *provider*.
//!
//! The shell deliberately sits **on top of** the process seam rather than beside
//! it: replacing `subprocess` relocates `bash` too, because bash is spawned
//! through the same interface as everything else.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsErrorKind {
    NotFound,
    NotAFile,
    NotADirectory,
    /// Outside the world's containment root, or the world is read-only.
    Denied,
    /// Not valid UTF-8, or otherwise not text.
    NotText,
    Io,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsError {
    pub kind: FsErrorKind,
    pub message: String,
}

impl FsError {
    pub fn denied(message: impl Into<String>) -> Self {
        Self {
            kind: FsErrorKind::Denied,
            message: message.into(),
        }
    }
    pub fn not_found(path: &Path) -> Self {
        Self {
            kind: FsErrorKind::NotFound,
            message: format!("{} does not exist", path.display()),
        }
    }
    pub fn io(error: impl std::fmt::Display) -> Self {
        Self {
            kind: FsErrorKind::Io,
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    /// Depth below the listed root, 0 for direct children.
    pub depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub exists: bool,
    pub is_dir: bool,
    pub len: u64,
}

/// One execution world's view of files.
///
/// Paths arriving here are model-supplied. A provider is responsible for its own
/// containment — the consumer must not have to know whether it is talking to a
/// local disk or a sandbox, which means it also must not be the one enforcing
/// the boundary.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Human-readable identity of this world, for diagnostics and for telling
    /// the model which world it is operating in.
    fn describe(&self) -> String;

    /// The root paths resolve against.
    fn root(&self) -> PathBuf;

    async fn read_text(&self, path: &Path) -> Result<String, FsError>;
    async fn write_text(&self, path: &Path, content: &str) -> Result<(), FsError>;
    async fn list(&self, path: &Path, depth: usize) -> Result<Vec<DirEntry>, FsError>;
    async fn info(&self, path: &Path) -> Result<FileInfo, FsError>;

    /// Replace the first occurrence of `old` with `new`.
    ///
    /// Default-implemented on top of read + write, but overridable: a provider
    /// with real atomicity should do the match and the rewrite in one critical
    /// section rather than leaving a read-modify-write race in the open.
    async fn edit_text(&self, path: &Path, old: &str, new: &str) -> Result<(), FsError> {
        let content = self.read_text(path).await?;
        let Some(index) = content.find(old) else {
            return Err(FsError {
                kind: FsErrorKind::NotFound,
                message: "the text to replace was not found".into(),
            });
        };
        if content[index + old.len()..].contains(old) {
            return Err(FsError {
                kind: FsErrorKind::Denied,
                message: "the text to replace is not unique; include more context".into(),
            });
        }
        let mut updated = String::with_capacity(content.len() - old.len() + new.len());
        updated.push_str(&content[..index]);
        updated.push_str(new);
        updated.push_str(&content[index + old.len()..]);
        self.write_text(path, &updated).await
    }
}

#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub timeout: Option<std::time::Duration>,
    /// Bytes of combined output to keep. `0` means unbounded.
    pub max_output_bytes: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// True when output was cut to `max_output_bytes`.
    pub truncated: bool,
}

/// Process execution for one world.
#[async_trait]
pub trait Subprocess: Send + Sync {
    fn describe(&self) -> String;
    /// Run `argv` to completion. `argv[0]` is the program.
    async fn run(&self, argv: &[String], options: &SpawnOptions) -> Result<Output, String>;
}

/// Shell execution. Built on [`Subprocess`], so a world swap carries it along.
#[async_trait]
pub trait Shell: Send + Sync {
    fn describe(&self) -> String;
    async fn run(&self, command: &str, options: &SpawnOptions) -> Result<Output, String>;
}
