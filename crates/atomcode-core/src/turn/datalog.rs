//! Per-turn logging: writes each user request and agent response to a markdown
//! file under a configurable root, with one subdirectory per project so multiple
//! projects don't pile their logs into a single bucket.
//!
//! Default layout:
//!   $ATOMCODE_HOME/datalog/<project-slug>/YYYY-MM-DD_HH-MM-SS.md
//!   $ATOMCODE_HOME/datalog/<project-slug>/llm/YYYY-MM-DD_HH-MM-SS_sss.json
//!
//! Project slug = sanitized cwd basename + 8-char sha256 prefix of the canonical
//! cwd path. The hash suffix prevents collisions when two unrelated projects
//! share a basename (e.g. `~/work/foo` vs `~/personal/foo`).
//!
//! Content mirrors what the user sees on screen.
//! Every write operation flushes immediately so logs survive crashes.
//!
//! Configured via `[datalog]` in `$ATOMCODE_HOME/config.toml`:
//! - `enabled = false`     — disables logging entirely (writer becomes a no-op)
//! - `dir = "<path>"`      — root directory; project slug is always appended
//!   underneath. Accepts absolute, `~/…`, or relative paths. Default
//!   `"$ATOMCODE_HOME/datalog"`.

use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::config::DatalogConfig;

/// Accumulates log entries for a single turn, flushing to disk after each operation.
pub struct DatalogWriter {
    /// Current working directory — used to derive the per-project slug, and to
    /// resolve `configured_dir` when it's relative.
    base_dir: PathBuf,
    /// Raw user-configured root (pre-resolution). `None` → default to
    /// `$ATOMCODE_HOME/datalog`. Otherwise absolute, `~/…`, or relative. The
    /// project slug is always appended underneath whichever value resolves.
    configured_dir: Option<String>,
    /// When false, all methods are no-ops and no files are created.
    enabled: bool,
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
    /// Optional suffix inserted into the markdown filename before `.md`.
    filename_tag: Option<String>,
    /// Current conversation's session id — the same id that rides the
    /// `x-atomcode-session-id` header and telemetry. Recorded in each turn's
    /// header so logs are attributable to a session and track /session and
    /// /resume switches. Empty until `set_session_id` is called.
    session_id: Option<String>,
}

impl DatalogWriter {
    pub fn new(working_dir: &Path, config: &DatalogConfig) -> Self {
        Self::new_inner(working_dir, config, None)
    }

    pub fn new_with_filename_tag(
        working_dir: &Path,
        config: &DatalogConfig,
        filename_tag: &str,
    ) -> Self {
        Self::new_inner(
            working_dir,
            config,
            Some(sanitize_filename_tag(filename_tag)),
        )
    }

    fn new_inner(working_dir: &Path, config: &DatalogConfig, filename_tag: Option<String>) -> Self {
        Self {
            base_dir: working_dir.to_path_buf(),
            configured_dir: config.dir.clone(),
            enabled: config.enabled,
            buf: String::new(),
            active: false,
            start: None,
            llm_turn_start: None,
            step: 0,
            file_path: None,
            filename_tag: filename_tag.filter(|s| !s.is_empty()),
            session_id: None,
        }
    }

    /// Bind the conversation's session id so each turn's log records it.
    /// Called from the agent's SetSessionId handler, keeping the datalog,
    /// the request header, and telemetry on the same id.
    pub fn set_session_id(&mut self, session_id: &str) {
        self.session_id = if session_id.is_empty() {
            None
        } else {
            Some(session_id.to_string())
        };
    }

    /// Update the working directory (e.g. after `/cd`). Absolute / `~/`
    /// configured paths are unaffected — only the default and relative cases
    /// follow cwd changes.
    pub fn set_working_dir(&mut self, dir: &Path) {
        self.base_dir = dir.to_path_buf();
    }

    /// Resolve the actual directory to write datalog files into, given the
    /// current `base_dir` and `configured_dir`. Pure function — used by
    /// `begin_turn`, by `runner.rs` to decide where `log_llm_request` writes
    /// the JSONL request dump (so both writers stay in sync), and covered by
    /// unit tests.
    ///
    /// The result is always `<root>/<project-slug>` so multiple projects don't
    /// collide. `<root>` is `$ATOMCODE_HOME/datalog` by default, or whatever the
    /// user configured (absolute / `~/…` / relative-to-cwd).
    pub fn resolve_log_dir(base_dir: &Path, configured: Option<&str>) -> PathBuf {
        let root = match configured {
            None => Self::default_root(),
            Some(s) if s.starts_with("~/") || s == "~" => {
                let rest = s.strip_prefix("~/").unwrap_or("");
                crate::tool::real_home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(rest)
            }
            Some(s) => {
                let p = PathBuf::from(s);
                if p.is_absolute() {
                    p
                } else {
                    base_dir.join(p)
                }
            }
        };
        root.join(project_slug(base_dir))
    }

    /// `$ATOMCODE_HOME/datalog` — the built-in root used when `[datalog].dir`
    /// is unset. Respects `ATOMCODE_HOME` environment variable if set.
    /// Falls back to a CWD-relative path on the (vanishingly rare)
    /// platforms where `$HOME` can't be resolved.
    fn default_root() -> PathBuf {
        crate::config::Config::config_dir().join("datalog")
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
        if !self.enabled {
            return;
        }
        self.buf.clear();
        self.step = 0;
        self.active = true;
        self.start = Some(Instant::now());

        let timestamp = format_timestamp();
        let filename_stem = timestamp.replace(' ', "_").replace(':', "-");
        let filename = match self.filename_tag.as_deref() {
            Some(tag) => format!("{filename_stem}_{tag}.md"),
            None => format!("{filename_stem}.md"),
        };
        let log_dir = Self::resolve_log_dir(&self.base_dir, self.configured_dir.as_deref());
        let _ = std::fs::create_dir_all(&log_dir);
        self.file_path = Some(log_dir.join(filename));

        let build_id = option_env!("ATOMCODE_BUILD_ID").unwrap_or("dev");
        let _ = writeln!(&mut self.buf, "# Turn {} [build:{}]", timestamp, build_id);
        let _ = writeln!(
            &mut self.buf,
            "**env:** model={}, ctx_window={}, session={}, cwd={}",
            model_name,
            context_window,
            self.session_id.as_deref().unwrap_or("-"),
            self.base_dir.display()
        );
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
        if !self.active {
            return;
        }
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
    pub fn log_context_stats(
        &mut self,
        system_tokens: usize,
        sent_tokens: usize,
        dropped_tokens: usize,
        _working_set_tokens: usize,
        total_messages: usize,
    ) {
        if !self.active {
            return;
        }
        let total = system_tokens + sent_tokens;
        let _ = writeln!(
            &mut self.buf,
            "  _[ctx: {}tok = sys:{}+sent:{}+dropped:{}, msgs:{}]_",
            total, system_tokens, sent_tokens, dropped_tokens, total_messages
        );
        self.flush();
    }

    /// Log prompt cache hit info (only when provider reports cached_tokens > 0).
    pub fn log_cache_hit(&mut self, prompt_tokens: usize, cached_tokens: usize) {
        if !self.active {
            return;
        }
        let pct = if prompt_tokens > 0 {
            cached_tokens * 100 / prompt_tokens
        } else {
            0
        };
        let _ = writeln!(
            &mut self.buf,
            "  _[cache: {}/{}tok = {}% hit]_",
            cached_tokens, prompt_tokens, pct
        );
        self.flush();
    }

    /// Log token usage for the current LLM round-trip.
    pub fn log_token_usage(
        &mut self,
        prompt_tokens: usize,
        completion_tokens: usize,
        cached_tokens: usize,
    ) {
        if !self.active {
            return;
        }
        let cache_str = if cached_tokens > 0 {
            format!(", cache={}tok", cached_tokens)
        } else {
            String::new()
        };
        let _ = writeln!(
            &mut self.buf,
            "  _[tokens: prompt={}+completion={}{}]_",
            prompt_tokens, completion_tokens, cache_str
        );
        self.flush();
    }

    /// Dump full LLM request as JSON into the datalog for debugging.
    /// Appends to a single JSONL file (one JSON object per line) colocated
    /// with the turn .md file: `<turn_timestamp>_requests.jsonl`.
    /// Each line has the step number so you can correlate with the md.
    pub fn log_llm_dump(
        &mut self,
        messages: &[crate::conversation::message::Message],
        tool_count: usize,
        model: &str,
        context_window: usize,
    ) {
        if !self.active {
            return;
        }

        // Derive JSONL path from the md file path: same name but .jsonl extension
        let jsonl_path = self.file_path.as_ref().map(|p| p.with_extension("jsonl"));

        if let Some(ref path) = jsonl_path {
            let msgs_json = serde_json::to_value(messages).unwrap_or(serde_json::json!([]));
            let total_tokens: usize = messages.iter().map(|m| m.estimate_tokens()).sum();
            let dump = serde_json::json!({
                "step": self.step,
                "session_id": self.session_id.as_deref().unwrap_or(""),
                "model": model,
                "context_window": context_window,
                "message_count": messages.len(),
                "estimated_tokens": total_tokens,
                "tool_count": tool_count,
                "messages": msgs_json,
            });
            // Append as single line (compact JSON) to JSONL
            if let Ok(json_line) = serde_json::to_string(&dump) {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(f, "{}", json_line);
                }
            }
            let _ = writeln!(
                &mut self.buf,
                "  _[request: {}msgs · {}tok · {}tools]_",
                messages.len(),
                total_tokens,
                tool_count
            );
            self.flush();
        }
    }

    /// Log a tool call start (within the current LLM turn).
    pub fn log_tool_call(&mut self, name: &str, args: &str) {
        if !self.active {
            return;
        }

        let detail = format_tool_args(name, args);
        let _ = writeln!(&mut self.buf, "- {} {}", capitalize(name), detail);
        // Log raw args when JSON is invalid (for debugging model output)
        if serde_json::from_str::<serde_json::Value>(args).is_err() {
            let _ = writeln!(
                &mut self.buf,
                "  [RAW ARGS: {}]",
                args.chars().take(200).collect::<String>()
            );
        }
        self.flush();
    }

    /// Log a tool call result.
    pub fn log_tool_result(&mut self, output: &str, success: bool) {
        if !self.active {
            return;
        }
        let icon = if success { "+" } else { "x" };
        let first_line = output.lines().next().unwrap_or("");
        let summary = if first_line.len() > 100 {
            format!("{}...", first_line.chars().take(97).collect::<String>())
        } else {
            first_line.to_string()
        };
        let total_lines = output.lines().count();
        if total_lines > 1 {
            let _ = writeln!(
                &mut self.buf,
                "  {} {} ({} lines)",
                icon, summary, total_lines
            );
        } else {
            let _ = writeln!(&mut self.buf, "  {} {}", icon, summary);
        }
        let _ = writeln!(&mut self.buf);
        self.flush();
    }

    /// Log model text output between tool calls (plan, thinking, explanation).
    pub fn log_model_text(&mut self, text: &str) {
        if !self.active {
            return;
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
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
        if !self.active {
            return;
        }
        if text.trim().is_empty() {
            return;
        }
        let _ = writeln!(&mut self.buf, "**Response:**");
        let _ = writeln!(&mut self.buf, "{}", text.trim());
        let _ = writeln!(&mut self.buf);
        self.flush();
    }

    /// Log an error.
    pub fn log_error(&mut self, error: &str) {
        if !self.active {
            return;
        }
        let _ = writeln!(&mut self.buf, "**Error:** {}", error);
        let _ = writeln!(&mut self.buf);
        self.flush();
    }

    /// Log a non-fatal advisory (e.g. provider truncation detector).
    /// Persisting it here makes post-hoc datalog inspection useful even
    /// when the live TUI line scrolls past — the user can grep the
    /// markdown for `**Warning:**` after the run.
    pub fn log_warning(&mut self, warning: &str) {
        if !self.active {
            return;
        }
        let _ = writeln!(&mut self.buf, "**Warning:** {}", warning);
        let _ = writeln!(&mut self.buf);
        self.flush();
    }

    /// Log one /goal evaluator round. Independent of `active` turn state
    /// because evaluator calls happen *between* main turns (not inside one),
    /// so they don't have a natural `begin_turn`/`end_turn` pair. Writes to
    /// `<log_dir>/goal-evaluator.md` (append-only) so the user can audit
    /// (a) what the evaluator was asked, (b) what it said, and (c) how many
    /// tokens it cost — currently invisible everywhere else.
    pub fn log_evaluator_round(
        &self,
        round: u32,
        verdict: &str,
        reason: &str,
        prompt_tokens: Option<usize>,
        completion_tokens: Option<usize>,
    ) {
        if !self.enabled {
            return;
        }
        let dir = Self::resolve_log_dir(&self.base_dir, self.configured_dir.as_deref());
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("goal-evaluator.md");
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let tokens = match (prompt_tokens, completion_tokens) {
            (Some(p), Some(c)) => format!(" · prompt={p} completion={c}"),
            (Some(p), None) => format!(" · prompt={p}"),
            (None, Some(c)) => format!(" · completion={c}"),
            (None, None) => String::new(),
        };
        let line = format!(
            "[{timestamp}] round={round} verdict={verdict}{tokens}\n    reason: {reason}\n"
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }

    /// End the turn: write duration and final flush.
    pub fn end_turn(&mut self, total_tokens: usize, tool_call_count: usize) {
        if !self.active {
            return;
        }
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
            let size = args
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            format!("{} ({} bytes)", short_path(path), size)
        }
        "edit_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            short_path(path)
        }
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.chars().count() > 80 {
                format!("`{}...`", cmd.chars().take(77).collect::<String>())
            } else {
                format!("`{}`", cmd)
            }
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
                            serde_json::Value::String(s) if s.chars().count() > 30 => {
                                format!("{}...", s.chars().take(27).collect::<String>())
                            }
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
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn sanitize_filename_tag(tag: &str) -> String {
    tag.chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                Some(c)
            } else if c.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

/// Per-project slug derived from `working_dir`. Shape: `<basename>-<hash8>`.
///
/// - `<basename>`: the last path component, with anything outside
///   `[A-Za-z0-9_-]` collapsed to `_` (filesystem-safe on every OS).
/// - `<hash8>`: first 8 hex chars of sha256 over the canonical absolute path.
///   Disambiguates same-name projects (`~/work/foo` vs `~/personal/foo`) and
///   keeps the slug stable across atomcode releases (sha2 is a fixed algorithm,
///   unlike `DefaultHasher` whose output the std reserves the right to change).
///
/// `canonicalize` may fail on freshly-deleted dirs / weird permissions; in that
/// case we hash the as-given path so the slug is still deterministic and
/// non-canonical paths just get their own bucket (acceptable — beats panicking).
fn project_slug(working_dir: &Path) -> String {
    let canonical = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let basename = canonical
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string());
    let safe: String = basename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    format!(
        "{}-{:02x}{:02x}{:02x}{:02x}",
        safe, digest[0], digest[1], digest[2], digest[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_log(dir: &Path) -> DatalogWriter {
        // Pin `dir` explicitly so tests write under the temp root, not the
        // real `$ATOMCODE_HOME/datalog/` (the new default). Slug subdir still
        // gets appended underneath — file_path lookups in the test go via
        // `log.file_path`, so the exact slugged path is opaque to callers.
        let cfg = DatalogConfig {
            enabled: true,
            dir: Some(dir.to_string_lossy().to_string()),
        };
        let mut log = DatalogWriter::new(dir, &cfg);
        log.begin_turn("test", "test-model", 16000);
        log.log_llm_call();
        log
    }

    #[test]
    fn resolve_log_dir_default_lands_under_home() {
        let base = PathBuf::from("/tmp/work");
        let p = DatalogWriter::resolve_log_dir(&base, None);
        // default_root() uses Config::config_dir().join("datalog"),
        // which resolves ATOMCODE_HOME when set, else $HOME/.atomcode.
        let expected_root = crate::config::Config::config_dir().join("datalog");
        assert!(
            p.starts_with(&expected_root),
            "{:?} should start with {:?}",
            p,
            expected_root
        );
        // Slug = basename + 8-hex hash. `/tmp/work` may not exist on the test
        // host (canonicalize fails → we hash the as-given path), so just
        // assert the shape: starts with "work-" and ends with 8 hex chars.
        let slug = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(slug.starts_with("work-"), "slug {:?}", slug);
        let hex_tail = slug.rsplit('-').next().unwrap();
        assert_eq!(hex_tail.len(), 8, "hash tail should be 8 hex chars");
        assert!(hex_tail.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resolve_log_dir_absolute_uses_configured_root_with_slug() {
        let base = PathBuf::from("/tmp/work");
        let p = DatalogWriter::resolve_log_dir(&base, Some("/var/logs/atomcode"));
        assert!(p.starts_with("/var/logs/atomcode"));
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("work-"));
    }

    #[test]
    fn resolve_log_dir_relative_joins_working_dir_then_slug() {
        let base = PathBuf::from("/tmp/work");
        let p = DatalogWriter::resolve_log_dir(&base, Some("logs/ac"));
        assert!(p.starts_with("/tmp/work/logs/ac"));
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("work-"));
    }

    #[test]
    fn resolve_log_dir_tilde_expands_home() {
        let base = PathBuf::from("/tmp/work");
        let p = DatalogWriter::resolve_log_dir(&base, Some("~/.atomcode/logs"));
        // `~/.atomcode/logs` expands via real_home_dir, which always resolves
        // to the system home (not ATOMCODE_HOME) — this is the same as the
        // `~/` expansion in the datalog dir config.
        let expected_root = crate::tool::real_home_dir().unwrap().join(".atomcode/logs");
        assert!(p.starts_with(&expected_root));
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("work-"));
    }

    #[test]
    fn project_slug_is_stable_for_same_path() {
        let p = PathBuf::from("/tmp/repeatable-path");
        let s1 = project_slug(&p);
        let s2 = project_slug(&p);
        assert_eq!(s1, s2, "slug must be deterministic");
    }

    #[test]
    fn project_slug_disambiguates_same_basename() {
        let a = PathBuf::from("/tmp/dup-test-a/foo");
        let b = PathBuf::from("/tmp/dup-test-b/foo");
        let sa = project_slug(&a);
        let sb = project_slug(&b);
        assert!(sa.starts_with("foo-"));
        assert!(sb.starts_with("foo-"));
        assert_ne!(sa, sb, "different parents must yield different slugs");
    }

    #[test]
    fn format_timestamp_produces_correct_format() {
        let ts = format_timestamp();
        // Should match format: YYYY-MM-DD HH:MM:SS
        let parts: Vec<&str> = ts.split(' ').collect();
        assert_eq!(parts.len(), 2, "Timestamp should have date and time parts");

        // Check date format: YYYY-MM-DD
        let date_parts: Vec<&str> = parts[0].split('-').collect();
        assert_eq!(date_parts.len(), 3, "Date should have 3 parts");
        assert_eq!(date_parts[0].len(), 4, "Year should be 4 digits");
        assert_eq!(date_parts[1].len(), 2, "Month should be 2 digits");
        assert_eq!(date_parts[2].len(), 2, "Day should be 2 digits");

        // Check time format: HH:MM:SS
        let time_parts: Vec<&str> = parts[1].split(':').collect();
        assert_eq!(time_parts.len(), 3, "Time should have 3 parts");
        assert_eq!(time_parts[0].len(), 2, "Hour should be 2 digits");
        assert_eq!(time_parts[1].len(), 2, "Minute should be 2 digits");
        assert_eq!(time_parts[2].len(), 2, "Second should be 2 digits");

        // Verify numeric values are in valid ranges
        let hour: u32 = time_parts[0].parse().expect("Hour should be numeric");
        let minute: u32 = time_parts[1].parse().expect("Minute should be numeric");
        let second: u32 = time_parts[2].parse().expect("Second should be numeric");
        assert!(hour < 24, "Hour should be 0-23");
        assert!(minute < 60, "Minute should be 0-59");
        assert!(second < 60, "Second should be 0-59");
    }

    #[test]
    fn disabled_writer_never_creates_files() {
        // Point `dir` at a temp subdir so this test doesn't depend on (or
        // pollute) the real `$ATOMCODE_HOME/datalog/`. With enabled=false the
        // writer should still create nothing under that root.
        let dir = std::env::temp_dir().join("atomcode_test_datalog_disabled");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = DatalogConfig {
            enabled: false,
            dir: Some(dir.to_string_lossy().to_string()),
        };
        let mut log = DatalogWriter::new(&dir, &cfg);
        log.begin_turn("hello", "m", 1000);
        log.log_llm_call();
        log.log_text("response");
        log.end_turn(0, 0);
        assert!(log.file_path.is_none());
        assert!(
            !dir.exists(),
            "disabled writer must not create the root dir"
        );
    }

    #[test]
    fn filename_tag_is_added_only_for_tagged_writer() {
        let dir = std::env::temp_dir().join("atomcode_test_datalog_filename_tag");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = DatalogConfig {
            enabled: true,
            dir: Some(dir.to_string_lossy().to_string()),
        };

        let mut default_log = DatalogWriter::new(&dir, &cfg);
        default_log.begin_turn("hello", "m", 1000);
        let default_name = default_log
            .file_path
            .as_ref()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(!default_name.contains("runtime-2"));

        let mut tagged_log = DatalogWriter::new_with_filename_tag(&dir, &cfg, "runtime-2");
        tagged_log.begin_turn("hello", "m", 1000);
        let tagged_name = tagged_log
            .file_path
            .as_ref()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(tagged_name.ends_with("_runtime-2.md"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_log_model_text_chinese_truncation() {
        let dir = std::env::temp_dir().join("atomcode_test_datalog_cn");
        let _ = std::fs::create_dir_all(&dir);
        let mut log = make_log(&dir);

        let long_chinese = "这是一段很长的中文文本用于测试截断逻辑".repeat(30);
        assert!(long_chinese.chars().count() > 500);

        log.log_model_text(&long_chinese);

        let content = std::fs::read_to_string(log.file_path.as_ref().unwrap()).unwrap();
        assert!(content.contains("..."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_log_model_text_short_no_truncation() {
        let dir = std::env::temp_dir().join("atomcode_test_datalog_short");
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
        let dir = std::env::temp_dir().join("atomcode_test_datalog_mixed");
        let _ = std::fs::create_dir_all(&dir);
        let mut log = make_log(&dir);

        let mixed = format!("Hello 你好 {} end", "🎉测试".repeat(200));
        assert!(mixed.chars().count() > 500);

        log.log_model_text(&mixed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_end_turn_stats_format() {
        let dir = std::env::temp_dir().join("atomcode_test_datalog_stats");
        let _ = std::fs::create_dir_all(&dir);
        let mut log = make_log(&dir);

        log.log_tool_call("bash", r#"{"command":"ls"}"#);
        log.log_tool_result("file.txt", true);
        log.end_turn(1000, 3);

        let content = std::fs::read_to_string(log.file_path.as_ref().unwrap()).unwrap();
        assert!(content.contains("1 turns"));
        assert!(content.contains("3 tool calls"));
        assert!(content.contains("1000 tokens"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
