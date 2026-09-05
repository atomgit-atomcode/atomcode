//! Providers for the execution world, and the tools that reach the outside
//! only through it.
//!
//! `fs-local` and `subprocess-local` are the local world. `fs-readonly` is the
//! same world with writes refused. `bash-local` builds the shell on whatever
//! fills `subprocess`, so replacing the process provider relocates bash without
//! bash knowing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

use crate::seams::{FsSvc, ShellSvc, SubprocessSvc};
use crate::world::{
    DirEntry, FileInfo, FileSystem, FsError, FsErrorKind, Output, Shell, SpawnOptions, Subprocess,
};

// ---- fs-local -----------------------------------------------------------

/// The local disk, fenced to a root.
///
/// Containment lives in the provider, not in each tool: a consumer cannot forget
/// to call it, and a different world enforces its own boundary its own way.
struct LocalFs {
    root: PathBuf,
    read_only: bool,
}

impl LocalFs {
    /// Resolve a model-supplied path inside the root, refusing anything that
    /// escapes it. Symlinks are resolved where the path exists, so a link out
    /// of the tree is caught rather than followed.
    fn resolve(&self, path: &Path) -> Result<PathBuf, FsError> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        // Canonicalize the deepest existing ancestor, then re-attach the rest:
        // a write to a not-yet-existing file still gets a real-path check.
        let mut existing = joined.as_path();
        let mut trailing = PathBuf::new();
        loop {
            if existing.exists() {
                break;
            }
            let Some(parent) = existing.parent() else {
                return Err(FsError::denied("path has no resolvable ancestor"));
            };
            let name = existing
                .file_name()
                .ok_or_else(|| FsError::denied("path has no file name"))?;
            trailing = if trailing.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                PathBuf::from(name).join(&trailing)
            };
            existing = parent;
        }
        let real = existing.canonicalize().map_err(FsError::io)?;
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let full = if trailing.as_os_str().is_empty() {
            real
        } else {
            real.join(&trailing)
        };
        if !full.starts_with(&root) {
            return Err(FsError::denied(format!(
                "{} is outside the world's root {}",
                full.display(),
                root.display()
            )));
        }
        Ok(full)
    }

    fn deny_write(&self) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::denied("this filesystem world is read-only"));
        }
        Ok(())
    }
}

#[async_trait]
impl FileSystem for LocalFs {
    fn describe(&self) -> String {
        if self.read_only {
            format!("local (read-only) at {}", self.root.display())
        } else {
            format!("local at {}", self.root.display())
        }
    }

    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    async fn read_text(&self, path: &Path) -> Result<String, FsError> {
        let path = self.resolve(path)?;
        if !path.exists() {
            return Err(FsError::not_found(&path));
        }
        if path.is_dir() {
            return Err(FsError {
                kind: FsErrorKind::NotAFile,
                message: format!("{} is a directory", path.display()),
            });
        }
        let bytes = std::fs::read(&path).map_err(FsError::io)?;
        String::from_utf8(bytes).map_err(|_| FsError {
            kind: FsErrorKind::NotText,
            message: format!("{} is not UTF-8 text", path.display()),
        })
    }

    async fn write_text(&self, path: &Path, content: &str) -> Result<(), FsError> {
        self.deny_write()?;
        let path = self.resolve(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(FsError::io)?;
        }
        std::fs::write(&path, content).map_err(FsError::io)
    }

    async fn list(&self, path: &Path, depth: usize) -> Result<Vec<DirEntry>, FsError> {
        let root = self.resolve(path)?;
        if !root.exists() {
            return Err(FsError::not_found(&root));
        }
        if !root.is_dir() {
            return Err(FsError {
                kind: FsErrorKind::NotADirectory,
                message: format!("{} is not a directory", root.display()),
            });
        }
        let mut out = Vec::new();
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, level)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut children: Vec<_> = entries.flatten().collect();
            children.sort_by_key(|e| e.file_name());
            for entry in children {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // Skip the directories that make a listing useless.
                if matches!(
                    name.as_ref(),
                    ".git" | "node_modules" | "target" | ".venv" | "__pycache__"
                ) {
                    continue;
                }
                let is_dir = path.is_dir();
                out.push(DirEntry {
                    path: path.clone(),
                    is_dir,
                    depth: level,
                });
                if is_dir && level + 1 < depth {
                    stack.push((path, level + 1));
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn info(&self, path: &Path) -> Result<FileInfo, FsError> {
        let path = self.resolve(path)?;
        Ok(match std::fs::metadata(&path) {
            Ok(meta) => FileInfo {
                exists: true,
                is_dir: meta.is_dir(),
                len: meta.len(),
            },
            Err(_) => FileInfo {
                exists: false,
                is_dir: false,
                len: 0,
            },
        })
    }
}

#[derive(Debug, Deserialize, Default)]
struct FsRow {
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    read_only: bool,
}

pub struct FsLocalPlugin;

#[async_trait]
impl Plugin for FsLocalPlugin {
    fn name(&self) -> &'static str {
        "fs-local"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["fs"]
    }
    fn description(&self) -> &'static str {
        "the local disk, fenced to a root"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: FsRow = parse(config)?;
        let root = row
            .root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let _ = ctx
            .provide::<FsSvc>(Arc::new(LocalFs {
                root,
                read_only: row.read_only,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The same world with writes refused. A separate row rather than a flag,
/// because "which world am I in" should be visible in `--dump-config`.
pub struct FsReadOnlyPlugin;

#[async_trait]
impl Plugin for FsReadOnlyPlugin {
    fn name(&self) -> &'static str {
        "fs-readonly"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["fs"]
    }
    fn description(&self) -> &'static str {
        "the local disk with every mutation refused"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: FsRow = parse(config)?;
        let root = row
            .root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let _ = ctx
            .provide::<FsSvc>(Arc::new(LocalFs {
                root,
                read_only: true,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- subprocess-local ---------------------------------------------------

struct LocalSubprocess;

#[async_trait]
impl Subprocess for LocalSubprocess {
    fn describe(&self) -> String {
        "local processes".into()
    }

    async fn run(&self, argv: &[String], options: &SpawnOptions) -> Result<Output, String> {
        let Some((program, args)) = argv.split_first() else {
            return Err("empty argv".into());
        };
        let mut command = tokio::process::Command::new(program);
        command.args(args);
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &options.env {
            command.env(key, value);
        }
        let child = command.output();
        let output = match options.timeout {
            Some(limit) => match tokio::time::timeout(limit, child).await {
                Ok(result) => result.map_err(|e| e.to_string())?,
                Err(_) => {
                    return Ok(Output {
                        code: -1,
                        stdout: String::new(),
                        stderr: format!("timed out after {:?}", limit),
                        timed_out: true,
                        truncated: false,
                    })
                }
            },
            None => child.await.map_err(|e| e.to_string())?,
        };
        let mut out = Output {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            timed_out: false,
            truncated: false,
        };
        if options.max_output_bytes > 0 {
            for field in [&mut out.stdout, &mut out.stderr] {
                if field.len() > options.max_output_bytes {
                    let keep: String = field.chars().take(options.max_output_bytes).collect();
                    *field = keep;
                    out.truncated = true;
                }
            }
        }
        Ok(out)
    }
}

pub struct SubprocessLocalPlugin;

#[async_trait]
impl Plugin for SubprocessLocalPlugin {
    fn name(&self) -> &'static str {
        "subprocess-local"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["subprocess"]
    }
    fn description(&self) -> &'static str {
        "spawn processes on this machine"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<SubprocessSvc>(Arc::new(LocalSubprocess))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- bash-local ---------------------------------------------------------

/// Bash, spawned through whatever fills `subprocess`.
///
/// It holds the context rather than the provider so it resolves per call: swap
/// the process provider and the very next command runs in the new world.
struct BashShell {
    ctx: Context,
    program: String,
}

#[async_trait]
impl Shell for BashShell {
    fn describe(&self) -> String {
        match self.ctx.service::<SubprocessSvc>() {
            Some(sub) => format!("{} via {}", self.program, sub.describe()),
            None => format!("{} (no process provider)", self.program),
        }
    }

    async fn run(&self, command: &str, options: &SpawnOptions) -> Result<Output, String> {
        let sub = self
            .ctx
            .service::<SubprocessSvc>()
            .ok_or("no `subprocess` provider is mounted")?;
        let argv = vec![self.program.clone(), "-c".to_string(), command.to_string()];
        sub.run(&argv, options).await
    }
}

#[derive(Debug, Deserialize)]
struct BashRow {
    #[serde(default = "default_shell")]
    program: String,
}

impl Default for BashRow {
    fn default() -> Self {
        Self {
            program: default_shell(),
        }
    }
}

fn default_shell() -> String {
    "bash".into()
}

pub struct BashLocalPlugin;

#[async_trait]
impl Plugin for BashLocalPlugin {
    fn name(&self) -> &'static str {
        "bash-local"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["subprocess"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["shell"]
    }
    fn description(&self) -> &'static str {
        "bash -c, spawned through the process seam"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: BashRow = parse(config)?;
        let _ = ctx
            .provide::<ShellSvc>(Arc::new(BashShell {
                ctx: ctx.clone(),
                program: row.program,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}
