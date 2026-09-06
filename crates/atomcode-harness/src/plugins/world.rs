//! Providers for the execution world, and the tools that reach the outside
//! only through it.
//!
//! `fs-local` and `subprocess-local` are the local world. `fs-readonly` is the
//! same world with writes refused. `bash-local` builds the shell on whatever
//! fills `subprocess`, so replacing the process provider relocates bash without
//! bash knowing.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

use crate::seams::{FsSvc, ShellSvc, SubprocessSvc};
use atomcode_capabilities::world::LocalFs;

use crate::seams::FileSystem;
use crate::world::{Output, Shell, SpawnOptions, Subprocess};

// ---- fs-local -----------------------------------------------------------
//
// The implementation lives in `atomcode_capabilities::world`, next to the tools
// that go through it. These two rows only choose which one to mount.

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
            .provide::<FsSvc>(if row.read_only {
                Arc::new(LocalFs::read_only(root)) as Arc<dyn FileSystem>
            } else {
                Arc::new(LocalFs::new(root))
            })
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
            .provide::<FsSvc>(Arc::new(LocalFs::read_only(root)))
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
