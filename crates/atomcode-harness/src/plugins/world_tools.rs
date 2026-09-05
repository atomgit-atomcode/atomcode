//! Model-facing tools that reach the outside **only** through the execution
//! world seams.
//!
//! None of them names a path API or spawns a process. Point `fs` or
//! `subprocess` somewhere else and every one of these follows, which is what
//! makes "run this agent against a sandbox" a config change rather than a
//! parallel tool implementation.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
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

// ---- read_file ----------------------------------------------------------

#[derive(Deserialize)]
struct ReadArgs {
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

struct ReadFile {
    ctx: Context,
    max_bytes: usize,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a text file from the current execution world. Output is 1-based line numbered. \
         Use `offset`/`limit` to page through a large file."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path, relative to the world root unless absolute" },
                "offset": { "type": "integer", "description": "1-based first line to return" },
                "limit": { "type": "integer", "description": "How many lines to return" }
            },
            "required": ["file_path"]
        })
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    /// Self-capped and line-numbered: a generic truncator would destroy the
    /// numbering the model needs to cite an edit.
    fn self_bounds_output(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let args: ReadArgs = match parse_args(args) {
            Ok(args) => args,
            Err(result) => return result,
        };
        let Some(fs) = self.ctx.service::<FsSvc>() else {
            return err("no `fs` provider is mounted");
        };
        match fs.read_text(&PathBuf::from(&args.file_path)).await {
            Ok(text) => {
                let offset = args.offset.unwrap_or(1).max(1);
                let limit = args.limit.unwrap_or(usize::MAX);
                let mut out = String::new();
                let mut truncated = false;
                for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
                    if out.len() >= self.max_bytes {
                        truncated = true;
                        break;
                    }
                    out.push_str(&format!("{}\t{}\n", index + 1, line));
                }
                if out.is_empty() {
                    return ok(format!(
                        "{} is empty (or the range is past its end)",
                        args.file_path
                    ));
                }
                if truncated {
                    out.push_str("[... truncated; use offset/limit to read further ...]\n");
                }
                ok(out)
            }
            Err(e) => err(e.to_string()),
        }
    }
}

// ---- write_file ---------------------------------------------------------

#[derive(Deserialize)]
struct WriteArgs {
    file_path: String,
    content: String,
}

struct WriteFile {
    ctx: Context,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create or overwrite a text file in the current execution world. Prefer `edit_file` \
         for changes to an existing file."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["file_path", "content"]
        })
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    /// Approving "always" for a write means the tool, not this one path — the
    /// same grant scope the production write tool uses.
    fn always_grant_scope(&self, _args: &str) -> String {
        "write_file".into()
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let args: WriteArgs = match parse_args(args) {
            Ok(args) => args,
            Err(result) => return result,
        };
        let Some(fs) = self.ctx.service::<FsSvc>() else {
            return err("no `fs` provider is mounted");
        };
        let path = PathBuf::from(&args.file_path);
        let existed = fs.info(&path).await.map(|i| i.exists).unwrap_or(false);
        match fs.write_text(&path, &args.content).await {
            Ok(()) => ok(format!(
                "{} {} ({} bytes, {} lines)",
                if existed { "Updated" } else { "Created" },
                args.file_path,
                args.content.len(),
                args.content.lines().count()
            )),
            Err(e) => err(e.to_string()),
        }
    }
}

// ---- edit_file ----------------------------------------------------------

#[derive(Deserialize)]
struct EditArgs {
    file_path: String,
    old_string: String,
    new_string: String,
}

struct EditFile {
    ctx: Context,
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Replace an exact, unique string in a file. Include enough surrounding context that \
         the match is unambiguous."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "old_string": { "type": "string", "description": "Exact text to replace; must occur exactly once" },
                "new_string": { "type": "string" }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        "edit_file".into()
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let args: EditArgs = match parse_args(args) {
            Ok(args) => args,
            Err(result) => return result,
        };
        let Some(fs) = self.ctx.service::<FsSvc>() else {
            return err("no `fs` provider is mounted");
        };
        match fs
            .edit_text(
                &PathBuf::from(&args.file_path),
                &args.old_string,
                &args.new_string,
            )
            .await
        {
            Ok(()) => ok(format!("Edited {}", args.file_path)),
            Err(e) => err(e.to_string()),
        }
    }
}

// ---- list_directory -----------------------------------------------------

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    depth: Option<usize>,
}

struct ListDirectory {
    ctx: Context,
}

#[async_trait]
impl Tool for ListDirectory {
    fn name(&self) -> &str {
        "list_directory"
    }
    fn description(&self) -> &str {
        "List a directory tree in the current execution world (indented; directories end with '/')."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: the world root)" },
                "depth": { "type": "integer", "description": "Max recursion depth (default 2, max 5)" }
            }
        })
    }
    fn read_only_hint(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let args: ListArgs = match parse_args(args) {
            Ok(args) => args,
            Err(result) => return result,
        };
        let Some(fs) = self.ctx.service::<FsSvc>() else {
            return err("no `fs` provider is mounted");
        };
        let path = args
            .path
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let depth = args.depth.unwrap_or(2).clamp(1, 5);
        match fs.list(&path, depth).await {
            Ok(entries) => {
                if entries.is_empty() {
                    return ok("(empty)");
                }
                let mut out = String::new();
                for entry in entries {
                    let name = entry
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    out.push_str(&"  ".repeat(entry.depth));
                    out.push_str(&name);
                    if entry.is_dir {
                        out.push('/');
                    }
                    out.push('\n');
                }
                ok(out)
            }
            Err(e) => err(e.to_string()),
        }
    }
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
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ReadFile {
                ctx: ctx.clone(),
                max_bytes,
            }),
            Arc::new(WriteFile { ctx: ctx.clone() }),
            Arc::new(EditFile { ctx: ctx.clone() }),
            Arc::new(ListDirectory { ctx: ctx.clone() }),
        ];
        super::tools::mount(ctx, tools)?;
        let fs = ctx.require::<FsSvc>().map_err(|e| e.to_string())?;
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
