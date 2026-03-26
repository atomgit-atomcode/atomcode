//! Per-turn logging: writes each user request and agent response to a markdown
//! file in the `datalog/` directory under the working directory.
//!
//! File naming: `datalog/YYYY-MM-DD_HH-MM-SS.md`
//! Content mirrors what the user sees on screen.
//! Every write operation flushes immediately so logs survive crashes.

use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// Accumulates log entries for a single turn, flushing to disk after each operation.
pub struct TurnLog {
    /// Working directory (datalog/ created under this)
    base_dir: PathBuf,
    /// Content buffer
    buf: String,
    /// Whether we have an active turn
    active: bool,
    /// Turn start time (for duration)
    start: Option<Instant>,
    /// Step counter
    step: usize,
    /// File path for this turn
    file_path: Option<PathBuf>,
}

impl TurnLog {
    pub fn new(working_dir: &Path) -> Self {
        Self {
            base_dir: working_dir.to_path_buf(),
            buf: String::new(),
            active: false,
            start: None,
            step: 0,
            file_path: None,
        }
    }

    /// Flush current buffer to disk immediately.
    fn flush(&self) {
        if let Some(ref path) = self.file_path {
            let _ = std::fs::write(path, &self.buf);
        }
    }

    /// Start a new turn: create log file, write user message.
    pub fn begin_turn(&mut self, user_message: &str) {
        self.buf.clear();
        self.step = 0;
        self.active = true;
        self.start = Some(Instant::now());

        let timestamp = format_timestamp();
        let filename = format!("{}.md", timestamp.replace(' ', "_").replace(':', "-"));
        let log_dir = self.base_dir.join("datalog");
        let _ = std::fs::create_dir_all(&log_dir);
        self.file_path = Some(log_dir.join(filename));

        let _ = writeln!(&mut self.buf, "# Turn {}", timestamp);
        let _ = writeln!(&mut self.buf);
        let _ = writeln!(&mut self.buf, "## User");
        let _ = writeln!(&mut self.buf, "```");
        let _ = writeln!(&mut self.buf, "{}", user_message);
        let _ = writeln!(&mut self.buf, "```");
        let _ = writeln!(&mut self.buf);
        let _ = writeln!(&mut self.buf, "## Agent");
        let _ = writeln!(&mut self.buf);
        self.flush();
    }

    /// Log start of a new LLM round-trip (increments the turn counter).
    pub fn log_llm_call(&mut self) {
        if !self.active { return; }
        self.step += 1;
        let _ = writeln!(&mut self.buf, "### Turn {}", self.step);
        self.flush();
    }

    /// Log a tool call start (within the current LLM turn).
    pub fn log_tool_call(&mut self, name: &str, args: &str) {
        if !self.active { return; }

        let detail = format_tool_args(name, args);
        let _ = writeln!(&mut self.buf, "- {} {}", capitalize(name), detail);
        // Log raw args when JSON is invalid (for debugging model output)
        if serde_json::from_str::<serde_json::Value>(args).is_err() {
            let _ = writeln!(&mut self.buf, "  [RAW ARGS: {}]", args.chars().take(200).collect::<String>());
        }
        self.flush();
    }

    /// Log a tool call result.
    pub fn log_tool_result(&mut self, output: &str, success: bool) {
        if !self.active { return; }
        let icon = if success { "+" } else { "x" };
        let first_line = output.lines().next().unwrap_or("");
        let summary = if first_line.len() > 100 {
            format!("{}...", first_line.chars().take(97).collect::<String>())
        } else {
            first_line.to_string()
        };
        let total_lines = output.lines().count();
        if total_lines > 1 {
            let _ = writeln!(&mut self.buf, "  {} {} ({} lines)", icon, summary, total_lines);
        } else {
            let _ = writeln!(&mut self.buf, "  {} {}", icon, summary);
        }
        let _ = writeln!(&mut self.buf);
        self.flush();
    }

    /// Log assistant text output.
    pub fn log_text(&mut self, text: &str) {
        if !self.active { return; }
        if text.trim().is_empty() { return; }
        let _ = writeln!(&mut self.buf, "**Response:**");
        let _ = writeln!(&mut self.buf, "{}", text.trim());
        let _ = writeln!(&mut self.buf);
        self.flush();
    }

    /// Log an error.
    pub fn log_error(&mut self, error: &str) {
        if !self.active { return; }
        let _ = writeln!(&mut self.buf, "**Error:** {}", error);
        let _ = writeln!(&mut self.buf);
        self.flush();
    }

    /// End the turn: write duration and final flush.
    pub fn end_turn(&mut self, total_tokens: usize) {
        if !self.active { return; }
        self.active = false;

        let duration = self.start.map(|s| s.elapsed()).unwrap_or_default();
        let _ = writeln!(&mut self.buf);
        let _ = writeln!(&mut self.buf, "---");
        let _ = writeln!(
            &mut self.buf,
            "**Stats:** {} turns, {:.1}s, {} tokens",
            self.step,
            duration.as_secs_f64(),
            total_tokens,
        );
        self.flush();
    }
}

fn capitalize(name: &str) -> String {
    name.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(ch) => ch.to_uppercase().to_string() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_tool_args(tool_name: &str, args_json: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    match tool_name {
        "read_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let short = short_path(path);
            let mut s = short;
            if let Some(offset) = args.get("offset").and_then(|v| v.as_u64()) {
                if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                    s.push_str(&format!(" L{}-{}", offset, offset + limit));
                }
            }
            s
        }
        "write_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let size = args.get("content").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0);
            format!("{} ({} bytes)", short_path(path), size)
        }
        "edit_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            short_path(path)
        }
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.chars().count() > 80 { format!("`{}...`", cmd.chars().take(77).collect::<String>()) } else { format!("`{}`", cmd) }
        }
        "list_directory" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            short_path(path)
        }
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("\"{}\" in {}", pattern, short_path(path))
        }
        "glob" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("\"{}\"", pattern)
        }
        _ => {
            if let Some(obj) = args.as_object() {
                obj.iter()
                    .map(|(k, v)| {
                        let val = match v {
                            serde_json::Value::String(s) if s.chars().count() > 30 => format!("{}...", s.chars().take(27).collect::<String>()),
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        format!("{}={}", k, val)
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                String::new()
            }
        }
    }
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    match parts.len() {
        0 | 1 => path.to_string(),
        2 => format!("{}/{}", parts[1], parts[0]),
        _ => format!(".../{}/{}", parts[1], parts[0]),
    }
}

/// Format current local time as "YYYY-MM-DD HH:MM:SS".
fn format_timestamp() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            let secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format!("{}", secs)
        })
}
