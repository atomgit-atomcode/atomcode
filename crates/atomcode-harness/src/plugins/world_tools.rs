//! Model-facing tools that reach the outside **only** through the execution
//! world seams.
//!
//! Point `fs` or `subprocess` somewhere else and every one of these follows,
//! which is what makes "run this agent against a sandbox" a config change
//! rather than a parallel tool implementation.
//!
//! # The filesystem tools used to be a second implementation. They are not.
//!
//! This module once carried its own `read_file` / `write_file` / `edit_file` /
//! `list_directory` — ~560 lines that existed only because the production tools
//! in `atomcode-capabilities` reached the disk directly and so could not be
//! pointed anywhere else. The default tree therefore ran those 560 lines while
//! the shipped agent ran the real 28k-line ones, and they agreed on nothing
//! finer than "it reads files": encoding detection, line endings, oversize
//! handling and diff application were all different, and the differential rig
//! could not see it because it compares event streams rather than tool output.
//!
//! The seam moved down instead: `atomcode_capabilities::world` now sits under
//! the real tools, so this row mounts *those*, handed a world. What is left
//! here is `bash`, which still needs its own implementation because the
//! production one owns its child process (piped output, job objects, idle-kill
//! vs hard-timeout) and the `Shell` seam cannot yet carry that.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool};
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::seams::{FsSvc, ShellSvc};
use crate::world::SpawnOptions;

fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: false,
        images: vec![],
    }
}

fn err(message: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: message.into(),
        is_error: true,
        images: vec![],
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(args: &str) -> Result<T, ToolResult> {
    serde_json::from_str(args).map_err(|e| err(format!("invalid arguments: {e}")))
}


pub struct FsWorldToolsPlugin;

#[async_trait]
impl Plugin for FsWorldToolsPlugin {
    fn name(&self) -> &'static str {
        "tool-fs-world"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "fs"]
    }
    fn description(&self) -> &'static str {
        "read/write/edit/list, routed through the `fs` seam"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default = "default_read_bytes")]
            max_read_bytes: usize,
        }
        fn default_read_bytes() -> usize {
            48 * 1024
        }
        let max_bytes = if config.is_null() {
            default_read_bytes()
        } else {
            serde_json::from_value::<Row>(config.clone())
                .map_err(|e| format!("bad config: {e}"))?
                .max_read_bytes
        };
        let _ = max_bytes;
        let fs = ctx.require::<FsSvc>().map_err(|e| e.to_string())?;
        // The SAME implementations `tool-fs` mounts, handed a world instead of
        // the local disk. There is no second copy of `read_file` any more: this
        // row and that one differ by one argument, which is the whole claim the
        // `fs` seam was built to make.
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ReadFileTool::with_world(false, fs.clone())),
            Arc::new(WriteFileTool::with_world(fs.clone())),
            Arc::new(EditFileTool::with_world(fs.clone())),
            Arc::new(ListDirTool::with_world(fs.clone())),
        ];
        super::tools::mount(ctx, tools)?;
        // The model is told which world it is in, because "the file is not
        // there" and "the file is not there *in this world*" are different
        // problems and only one of them is the model's to solve.
        super::tools::contribute_prompt(
            ctx,
            "tool-fs-world",
            50,
            &format!(
                "Files you read and write live in this execution world: {}. Read a file before \
                 editing it, and prefer `edit_file` over rewriting a whole file.",
                fs.describe()
            ),
        );
        Ok(())
    }
}

// ---- bash ---------------------------------------------------------------

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

struct BashTool {
    ctx: Context,
    default_timeout_secs: u64,
    max_output_bytes: usize,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command in the current execution world. Use it for builds, tests and git."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout_secs": { "type": "integer", "description": "Kill the command after this long" }
            },
            "required": ["command"]
        })
    }
    /// Arg-aware: a provably read-only command is Safe, so an approval policy
    /// does not have to gate `git status` to gate `rm -rf`.
    fn risk(&self, args: &str) -> RiskLevel {
        let Ok(parsed) = serde_json::from_str::<BashArgs>(args) else {
            return RiskLevel::Risky;
        };
        if is_read_only_command(&parsed.command) {
            RiskLevel::Safe
        } else {
            RiskLevel::Risky
        }
    }
    fn parallel_safe(&self, args: &str) -> bool {
        matches!(self.risk(args), RiskLevel::Safe)
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let args: BashArgs = match parse_args(args) {
            Ok(args) => args,
            Err(result) => return result,
        };
        let Some(shell) = self.ctx.service::<ShellSvc>() else {
            return err("no `shell` provider is mounted");
        };
        let options = SpawnOptions {
            cwd: self.ctx.service::<FsSvc>().map(|fs| fs.root()),
            env: Vec::new(),
            timeout: Some(std::time::Duration::from_secs(
                args.timeout_secs.unwrap_or(self.default_timeout_secs),
            )),
            max_output_bytes: self.max_output_bytes,
        };
        match shell.run(&args.command, &options).await {
            Ok(output) => {
                let mut content = String::new();
                if !output.stdout.is_empty() {
                    content.push_str(&output.stdout);
                }
                if !output.stderr.is_empty() {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&output.stderr);
                }
                if output.truncated {
                    content.push_str("\n[... output truncated ...]");
                }
                if output.timed_out {
                    return err(format!("command timed out\n{content}"));
                }
                if output.code != 0 {
                    return err(format!("exit {}\n{content}", output.code));
                }
                ok(if content.is_empty() {
                    "(no output)".to_string()
                } else {
                    content
                })
            }
            Err(e) => err(e),
        }
    }
}

/// A conservative allow-list. Anything not provably read-only is treated as a
/// mutation — the failure mode of guessing wrong the other way is a destroyed
/// working tree.
fn is_read_only_command(command: &str) -> bool {
    // Any shell metacharacter that could chain or redirect defeats the check.
    if command.contains(['|', '>', '<', ';', '&', '`', '$']) {
        return false;
    }
    const READ_ONLY: &[&str] = &[
        "ls", "cat", "head", "tail", "wc", "pwd", "echo", "date", "which", "file", "stat", "du",
        "df", "env", "uname", "whoami",
    ];
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if first == "git" {
        return matches!(
            words.next(),
            Some("status" | "log" | "diff" | "show" | "branch" | "remote")
        );
    }
    if first == "cargo" {
        return matches!(words.next(), Some("check" | "tree" | "metadata"));
    }
    READ_ONLY.contains(&first)
}

pub struct BashWorldToolPlugin;

#[async_trait]
impl Plugin for BashWorldToolPlugin {
    fn name(&self) -> &'static str {
        "tool-bash-world"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "shell"]
    }
    fn description(&self) -> &'static str {
        "shell execution, routed through the `shell` seam"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default = "default_timeout")]
            timeout_secs: u64,
            #[serde(default = "default_output")]
            max_output_bytes: usize,
        }
        fn default_timeout() -> u64 {
            120
        }
        fn default_output() -> usize {
            32 * 1024
        }
        let (timeout_secs, max_output_bytes) = if config.is_null() {
            (default_timeout(), default_output())
        } else {
            let row: Row =
                serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?;
            (row.timeout_secs, row.max_output_bytes)
        };
        super::tools::mount(
            ctx,
            vec![Arc::new(BashTool {
                ctx: ctx.clone(),
                default_timeout_secs: timeout_secs,
                max_output_bytes,
            })],
        )?;
        let shell = ctx.require::<ShellSvc>().map_err(|e| e.to_string())?;
        super::tools::contribute_prompt(
            ctx,
            "tool-bash-world",
            51,
            &format!(
                "Shell commands run through: {}. Quote paths that contain spaces.",
                shell.describe()
            ),
        );
        Ok(())
    }
}
