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
    /// Turn start time (for total duration)
    start: Option<Instant>,
    /// Per-LLM-turn timer (reset on each log_llm_call)
    llm_turn_start: Option<Instant>,
    /// Step counter (LLM turn count)
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
            llm_turn_start: None,
            step: 0,
            file_path: None,
        }
    }

    /// Update the base directory (e.g. after `/cd`).
    pub fn set_working_dir(&mut self, dir: &Path) {
        self.base_dir = dir.to_path_buf();
    }

    /// Clear the current turn log state and delete the log file if it exists.
    /// Called when user runs /clear command.
    pub fn clear(&mut self) {
        // Delete the current log file if it exists
        if let Some(ref path) = self.file_path {
            let _ = std::fs::remove_file(path);
        }
        self.buf.clear();
        self.active = false;
        self.start = None;
        self.llm_turn_start = None;
        self.step = 0;
        self.file_path = None;
    }

    /// Flush current buffer to disk immediately.
    fn flush(&self) {
        if let Some(ref path) = self.file_path {
            let _ = std::fs::write(path, &self.buf);
        }
    }

    /// Start a new turn: create log file, write env info + user message.
    pub fn begin_turn(&mut self, user_message: &str, model_name: &str, context_window: usize) {
        self.buf.clear();
        self.step = 0;
        self.active = true;
        self.start = Some(Instant::now());

        let timestamp = format_timestamp();
        let filename = format!("{}.md", timestamp.replace(' ', "_").replace(':', "-"));
        let log_dir = self.base_dir.join("datalog");
        let _ = std::fs::create_dir_all(&log_dir);
        self.file_path = Some(log_dir.join(filename));

        let _ = writeln!(&mut self.buf, "# Turn {} [build:{}]", timestamp, env!("ATOMCODE_BUILD_ID"));
        let _ = writeln!(&mut self.buf, "**env:** model={}, ctx_window={}, cwd={}",
            model_name, context_window, self.base_dir.display());
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
    /// Records the duration of the previous LLM turn.
    pub fn log_llm_call(&mut self) {
        if !self.active { return; }
        // Log previous turn's duration
        if let Some(prev_start) = self.llm_turn_start {
            let dur = prev_start.elapsed();
            if dur.as_millis() >= 1000 {
                let _ = writeln!(&mut self.buf, "  _({:.1}s)_\n", dur.as_secs_f64());
            }
        }
        self.step += 1;
        let _ = writeln!(&mut self.buf, "### Turn {}", self.step);
        self.llm_turn_start = Some(Instant::now());
        self.flush();
    }

    /// Log context statistics for debugging.
    /// Called from AgentLoop before each LLM call.
    pub fn log_context_stats(&mut self, system_tokens: usize, sent_tokens: usize,
                              dropped_tokens: usize, _working_set_tokens: usize,
                              total_messages: usize) {
        if !self.active { return; }
        let total = system_tokens + sent_tokens;
        let _ = writeln!(&mut self.buf,
            "  _[ctx: {}tok = sys:{}+sent:{}+dropped:{}, msgs:{}]_",
            total, system_tokens, sent_tokens, dropped_tokens, total_messages
        );
        self.flush();
    }

    /// Log prompt cache hit info (only when provider reports cached_tokens > 0).
    pub fn log_cache_hit(&mut self, prompt_tokens: usize, cached_tokens: usize) {
        if !self.active { return; }
        let pct = if prompt_tokens > 0 { cached_tokens * 100 / prompt_tokens } else { 0 };
        let _ = writeln!(&mut self.buf,
            "  _[cache: {}/{}tok = {}% hit]_",
            cached_tokens, prompt_tokens, pct
        );
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

    /// Log model text output between tool calls (plan, thinking, explanation).
    pub fn log_model_text(&mut self, text: &str) {
        if !self.active { return; }
        let trimmed = text.trim();
        if trimmed.is_empty() { return; }
        // Cap at 500 chars to avoid bloating datalog
        let display = if trimmed.chars().count() > 500 {
            format!("{}...", trimmed.chars().take(497).collect::<String>())
        } else {
            trimmed.to_string()
        };
        let _ = writeln!(&mut self.buf, "  > {}", display.replace('\n', "\n  > "));
        let _ = writeln!(&mut self.buf);
        self.flush();
    }

    /// Log final assistant text output (response/summary).
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
    pub fn end_turn(&mut self, total_tokens: usize, tool_call_count: usize) {
        if !self.active { return; }
        self.active = false;

        // Log last LLM turn duration
        if let Some(prev_start) = self.llm_turn_start.take() {
            let dur = prev_start.elapsed();
            if dur.as_millis() >= 1000 {
                let _ = writeln!(&mut self.buf, "  _({:.1}s)_", dur.as_secs_f64());
            }
        }

        let duration = self.start.map(|s| s.elapsed()).unwrap_or_default();
        let _ = writeln!(&mut self.buf);
        let _ = writeln!(&mut self.buf, "---");
        let _ = writeln!(
            &mut self.buf,
            "**Stats:** {} turns, {} tool calls, {:.1}s, {} tokens",
            self.step,
            tool_call_count,
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
        "create_file" => {
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
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_log(dir: &Path) -> TurnLog {
        let mut log = TurnLog::new(dir);
        log.begin_turn("test", "test-model", 16000);
        log.log_llm_call();
        log
    }

    #[test]
    fn test_log_model_text_chinese_truncation() {
        let dir = std::env::temp_dir().join("atomcode_test_turnlog_cn");
        let _ = std::fs::create_dir_all(&dir);
        let mut log = make_log(&dir);

        // 500+ Chinese characters — each is 3 bytes in UTF-8.
        // Old code used byte slicing [..497] which would panic on char boundary.
        let long_chinese = "这是一段很长的中文文本用于测试截断逻辑".repeat(30);
        assert!(long_chinese.chars().count() > 500);

        // This must not panic
        log.log_model_text(&long_chinese);

        // Verify it was truncated
        let content = std::fs::read_to_string(log.file_path.as_ref().unwrap()).unwrap();
        assert!(content.contains("..."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_log_model_text_short_no_truncation() {
        let dir = std::env::temp_dir().join("atomcode_test_turnlog_short");
        let _ = std::fs::create_dir_all(&dir);
        let mut log = make_log(&dir);

        log.log_model_text("短文本");

        let content = std::fs::read_to_string(log.file_path.as_ref().unwrap()).unwrap();
        assert!(content.contains("短文本"));
        assert!(!content.contains("..."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_log_model_text_mixed_unicode() {
        let dir = std::env::temp_dir().join("atomcode_test_turnlog_mixed");
        let _ = std::fs::create_dir_all(&dir);
        let mut log = make_log(&dir);

        // Mix of ASCII, Chinese, emoji — all multi-byte
        let mixed = format!("Hello 你好 {} end", "🎉测试".repeat(200));
        assert!(mixed.chars().count() > 500);

        // Must not panic
        log.log_model_text(&mixed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_end_turn_stats_format() {
        let dir = std::env::temp_dir().join("atomcode_test_turnlog_stats");
        let _ = std::fs::create_dir_all(&dir);
        let mut log = make_log(&dir);

        log.log_tool_call("bash", r#"{"command":"ls"}"#);
        log.log_tool_result("file.txt", true);
        log.end_turn(1000, 3);

        let content = std::fs::read_to_string(log.file_path.as_ref().unwrap()).unwrap();
        // Verify the new dual-metric stats format
        assert!(content.contains("1 turns"));
        assert!(content.contains("3 tool calls"));
        assert!(content.contains("1000 tokens"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

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
