//! `recall` tool — lets the agent retrieve ANY past turn of THIS project, including
//! from OTHER sessions, by topic and/or time ("昨天我们讨论过的那个 OAuth 的事").
//!
//! Reads the never-compacted `<id>.jsonl` transcripts (the recall ground truth) under
//! the project's `<project_hash>` bucket — derived from `ToolContext.working_dir`, so the
//! tool needs no session wiring. Matching is keyword/full-text v1 behind a swappable
//! [`RecallIndex`] so a semantic/embedding backend can drop in later without touching the
//! tool. Read-only ⇒ `risk = Safe`. The model resolves relative dates ("yesterday") into
//! concrete `after`/`before` using the current date carried by the persona's frozen date anchor.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use chrono::{Local, TimeZone};
use serde::Deserialize;

use super::manager::{for_each_jsonl_line, regular_file_len, MAX_JSONL_BYTES, MAX_JSONL_LINES};
use super::{SessionManager, SessionResult, SessionStoreError, TurnRecord};

/// A parsed recall query: lowercased keyword terms + a result cap.
pub struct RecallQuery {
    pub terms: Vec<String>,
    pub limit: usize,
}

/// Swappable ranking backend over (already time-filtered) turns. v1 ships
/// [`KeywordIndex`]; a later `EmbeddingIndex` implements the same trait so the tool is
/// unchanged.
pub trait RecallIndex: Send + Sync {
    /// Rank `records` against `q`; return the best matches first, at most `q.limit`.
    fn search<'a>(&self, records: &'a [TurnRecord], q: &RecallQuery) -> Vec<&'a TurnRecord>;
}

/// Keyword/full-text ranking: coverage-first — a turn ranks by how many DISTINCT
/// query terms it matches (`matched_terms`), then by total term occurrences across
/// the turn's user + assistant + tool (name/args/result) text, then by hit density,
/// then by recency (`ts` desc). Query terms are CJK-bigram-expanded by
/// [`tokenize_query`] so space-less Chinese phrases still hit.
pub struct KeywordIndex;

/// Per-record score: how many distinct query terms matched, their total occurrences,
/// and the hay length (density tiebreak). Private; `RecallIndex`/`RecallQuery` unchanged.
#[derive(Default, Clone, Copy)]
struct Scored {
    matched_terms: usize,
    occurrences: usize,
    hay_len: usize,
}

fn density(s: Scored) -> f64 {
    s.occurrences as f64 / s.hay_len.max(1) as f64
}

impl RecallIndex for KeywordIndex {
    fn search<'a>(&self, records: &'a [TurnRecord], q: &RecallQuery) -> Vec<&'a TurnRecord> {
        let mut scored: Vec<(Scored, &'a TurnRecord)> = records
            .iter()
            .filter_map(|r| {
                let s = score_record(r, &q.terms);
                (s.matched_terms > 0).then_some((s, r))
            })
            .collect();
        // coverage desc → occurrences desc → density desc → ts desc (recency).
        scored.sort_by(|a, b| {
            b.0.matched_terms
                .cmp(&a.0.matched_terms)
                .then(b.0.occurrences.cmp(&a.0.occurrences))
                .then(
                    density(b.0)
                        .partial_cmp(&density(a.0))
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(b.1.ts.cmp(&a.1.ts))
        });
        scored.into_iter().take(q.limit).map(|(_, r)| r).collect()
    }
}

fn score_record(r: &TurnRecord, terms: &[String]) -> Scored {
    let mut hay = format!("{} {} {}", r.user, r.assistant, r.reasoning).to_lowercase();
    for t in &r.tools {
        hay.push(' ');
        hay.push_str(&t.name.to_lowercase());
        hay.push(' ');
        hay.push_str(&t.args.to_lowercase());
        hay.push(' ');
        hay.push_str(&t.result.to_lowercase());
    }
    let hay_len = hay.chars().count();
    let mut matched_terms = 0;
    let mut occurrences = 0;
    for term in terms {
        let n = hay.matches(term.as_str()).count();
        if n > 0 {
            matched_terms += 1;
        }
        occurrences += n;
    }
    Scored {
        matched_terms,
        occurrences,
        hay_len,
    }
}

/// CJK-run expansion: 1 char → the char itself; n≥2 → all consecutive char bigrams.
fn expand_cjk_run(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    match chars.len() {
        0 => Vec::new(),
        1 => vec![chars[0].to_string()],
        n => (0..n - 1)
            .map(|i| format!("{}{}", chars[i], chars[i + 1]))
            .collect(),
    }
}

fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0x20000..=0x2EBEF).contains(&cp)
        || (0x2F800..=0x2FA1F).contains(&cp)
        || (0x3040..=0x30FF).contains(&cp)
        || (0x31F0..=0x31FF).contains(&cp)
        || (0x1100..=0x11FF).contains(&cp)
        || (0xAC00..=0xD7AF).contains(&cp)
}

/// Minimal connector 字 (char, not word) — stripable only at run edges.
fn is_connector_char(c: char) -> bool {
    matches!(c, '的' | '了' | '与' | '和' | '及' | '或')
}

/// 2-char connector words, stripped only at run edges.
const CJK_CONNECTOR_WORDS: &[&str] = &["关于", "以及"];

/// Edge-only connector strip + guard. Returns the core (or the original run when
/// stripping would leave <2 chars); `None` when the whole run is connectors.
fn strip_edge_connectors(run: &str) -> Option<String> {
    let original: Vec<char> = run.chars().collect();
    if original.len() == 1 {
        return if is_connector_char(original[0]) {
            None
        } else {
            Some(original[0].to_string())
        };
    }
    let mut cur = run.to_string();
    loop {
        let before = cur.clone();
        for &w in CJK_CONNECTOR_WORDS {
            if let Some(rest) = cur.strip_prefix(w) {
                cur = rest.to_string();
                break;
            }
        }
        for &w in CJK_CONNECTOR_WORDS {
            if let Some(rest) = cur.strip_suffix(w) {
                cur = rest.to_string();
                break;
            }
        }
        if let Some(first) = cur.chars().next() {
            if is_connector_char(first) {
                if let Some(rest) = cur.strip_prefix(first) {
                    cur = rest.to_string();
                }
            }
        }
        if let Some(last) = cur.chars().next_back() {
            if is_connector_char(last) {
                if let Some(rest) = cur.strip_suffix(last) {
                    cur = rest.to_string();
                }
            }
        }
        if cur == before {
            break;
        }
    }
    match cur.chars().count() {
        0 => None,
        1 => Some(original.iter().collect()), // guard: don't shave a ≥2 run to 1 char
        _ => Some(cur),
    }
}

/// One whitespace-token containing non-ASCII chars: drop punctuation → split into
/// CJK/literal runs → edge-strip connectors on CJK runs → bigram-expand CJK runs,
/// keep literal runs whole. Pure-ASCII tokens never enter here.
fn tokenize_cjk_token(token: &str) -> Vec<String> {
    let cleaned: String = token.chars().filter(|c| c.is_alphanumeric()).collect();
    let chars: Vec<char> = cleaned.chars().collect();
    let mut terms = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let cjk_run = is_cjk(chars[i]);
        let mut j = i;
        while j < chars.len() && is_cjk(chars[j]) == cjk_run {
            j += 1;
        }
        let run: String = chars[i..j].iter().collect();
        if cjk_run {
            if let Some(core) = strip_edge_connectors(&run) {
                terms.extend(expand_cjk_run(&core));
            }
        } else {
            terms.push(run); // literal run kept whole (already lowercased)
        }
        i = j;
    }
    terms
}

/// Lowercase → split_whitespace → pure-ASCII tokens verbatim / CJK tokens via
/// [`tokenize_cjk_token`] → dedup, first-occurrence order.
fn tokenize_query(query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    let mut terms: Vec<String> = Vec::new();
    for raw in lower.split_whitespace() {
        let candidates: Vec<String> = if raw.is_ascii() {
            vec![raw.to_string()]
        } else {
            tokenize_cjk_token(raw)
        };
        for t in candidates {
            if !t.is_empty() && !terms.contains(&t) {
                terms.push(t);
            }
        }
    }
    terms
}

#[derive(Debug, Deserialize)]
struct RecallArgs {
    query: String,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

const DEFAULT_LIMIT: usize = 8;

/// The `recall` tool.
pub struct RecallTool {
    index: Arc<dyn RecallIndex>,
    /// PINNED sessions dir. `None` (standalone default) derives the project bucket
    /// from the live `ToolContext.working_dir` at each call — but that value MOVES
    /// when the model runs `cd`, silently pointing recall at a different project's
    /// bucket than the one the session hooks write. An assembly that owns a
    /// `SessionManager` pins the root here so recall and persistence always agree.
    sessions_dir: Option<std::path::PathBuf>,
}

impl Default for RecallTool {
    fn default() -> Self {
        Self {
            index: Arc::new(KeywordIndex),
            sessions_dir: None,
        }
    }
}

impl RecallTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a custom ranking backend (e.g. a future embedding index).
    pub fn with_index(index: Arc<dyn RecallIndex>) -> Self {
        Self {
            index,
            sessions_dir: None,
        }
    }

    /// Pin the sessions dir this tool searches (an assembly passes its
    /// `SessionManager::root()`), instead of re-deriving it from the live —
    /// `cd`-movable — working dir at each call.
    pub fn with_sessions_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.sessions_dir = Some(dir.into());
        self
    }

    /// The testable core: load every `*.jsonl` under `sessions_dir`, time-filter, rank,
    /// and format the result the model reads. Separated from `execute` so it is unit-
    /// testable against a temp dir without `$ATOMCODE_HOME`.
    pub fn search_dir(
        &self,
        sessions_dir: &Path,
        query: &str,
        after: Option<&str>,
        before: Option<&str>,
        limit: usize,
    ) -> SessionResult<String> {
        let after_ms = after.and_then(parse_date_bound);
        let before_ms = before.and_then(parse_date_bound);

        let records: Vec<TurnRecord> = load_records(sessions_dir)?
            .into_iter()
            .filter(|r| after_ms.is_none_or(|a| r.ts >= a))
            .filter(|r| before_ms.is_none_or(|b| r.ts < b))
            .collect();

        let q = RecallQuery {
            terms: tokenize_query(query),
            limit,
        };
        let hits = self.index.search(&records, &q);
        let mut out = format_hits(&hits);
        // Self-documenting fallback: point the model at the raw ground truth (prints the
        // REAL dir, so it never goes stale) and restate the freshness boundary right where
        // a confused "why is nothing here?" lands. Reading those `<id>.jsonl` files gives
        // the exact, full turn (incl. tool I/O) when the keyword digest above isn't enough.
        out.push_str(&format!(
            "\n(Raw per-turn transcripts: {} — one `<session_id>.jsonl` per session, full \
             text incl. tool I/O. The current in-progress turn is appended there only once \
             it finishes.)",
            sessions_dir.display()
        ));
        Ok(out)
    }
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Search this project's COMPLETED conversation turns — across all sessions, \
         including earlier turns of the CURRENT session — by topic and/or time. A turn is \
         indexed only AFTER it finishes, so the in-progress turn (what is happening right \
         now) is NOT here yet; for that, rely on your own context. Use it to recall a past \
         decision, bug, or approach, even from another session. Resolve relative dates \
         yourself (e.g. 'yesterday') into the `after`/`before` fields using the current \
         date. Read-only — the result footer shows where the raw per-turn transcripts live \
         if you need the exact, full text."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "keywords / topic to recall" },
                "after": { "type": "string", "description": "optional lower bound, inclusive — ISO datetime or YYYY-MM-DD (local time)" },
                "before": { "type": "string", "description": "optional upper bound, exclusive — ISO datetime or YYYY-MM-DD (local time)" },
                "limit": { "type": "integer", "description": "max turns to return (default 8)" }
            },
            "required": ["query"]
        })
    }

    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: RecallArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!("invalid recall arguments: {e}"),
                    is_error: true,
                    images: vec![],
                }
            }
        };
        let sessions_dir = match &self.sessions_dir {
            Some(d) => d.clone(),
            None => SessionManager::for_project(&ctx.working_dir)
                .root()
                .to_path_buf(),
        };
        let content = match self.search_dir(
            &sessions_dir,
            &a.query,
            a.after.as_deref(),
            a.before.as_deref(),
            a.limit.unwrap_or(DEFAULT_LIMIT),
        ) {
            Ok(content) => content,
            Err(error) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!("failed to read session transcripts: {error}"),
                    is_error: true,
                    images: vec![],
                }
            }
        };
        ToolResult {
            call_id: String::new(),
            content,
            is_error: false,
            images: vec![],
        }
    }
}

/// Load every transcript incrementally. Missing dir remains an empty history; a present
/// but corrupt/unsafe/future-schema file is explicit because silently omitting history
/// would make recall report a false success.
fn load_records(dir: &Path) -> SessionResult<Vec<TurnRecord>> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(source) => {
            return Err(SessionStoreError::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut total_bytes = 0usize;
    let mut total_lines = 0usize;
    for entry in rd {
        let entry = entry.map_err(|source| SessionStoreError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let file_bytes = regular_file_len(&path)?;
        let projected_bytes =
            total_bytes
                .checked_add(file_bytes)
                .ok_or(SessionStoreError::TooLarge {
                    kind: "recall transcripts",
                    limit: MAX_JSONL_BYTES,
                    actual: usize::MAX,
                })?;
        if projected_bytes > MAX_JSONL_BYTES {
            return Err(SessionStoreError::TooLarge {
                kind: "recall transcripts",
                limit: MAX_JSONL_BYTES,
                actual: projected_bytes,
            });
        }
        let (bytes, lines) = for_each_jsonl_line(&path, |line| {
            if out.len() >= MAX_JSONL_LINES {
                return Err(SessionStoreError::TooLarge {
                    kind: "recall transcript lines",
                    limit: MAX_JSONL_LINES,
                    actual: out.len() + 1,
                });
            }
            let rec: TurnRecord =
                serde_json::from_slice(line).map_err(|error| SessionStoreError::Corrupt {
                    kind: "transcript record",
                    message: format!("{}: {error}", path.display()),
                })?;
            if rec.v > crate::session::transcript::RECORD_VERSION {
                return Err(SessionStoreError::FutureSchema {
                    kind: "transcript record",
                    found: rec.v,
                    supported: crate::session::transcript::RECORD_VERSION,
                });
            }
            out.push(rec);
            Ok(())
        })?;
        total_bytes = total_bytes
            .checked_add(bytes)
            .ok_or(SessionStoreError::TooLarge {
                kind: "recall transcripts",
                limit: MAX_JSONL_BYTES,
                actual: usize::MAX,
            })?;
        total_lines = total_lines
            .checked_add(lines)
            .ok_or(SessionStoreError::TooLarge {
                kind: "recall transcript lines",
                limit: MAX_JSONL_LINES,
                actual: usize::MAX,
            })?;
        if total_bytes > MAX_JSONL_BYTES {
            return Err(SessionStoreError::TooLarge {
                kind: "recall transcripts",
                limit: MAX_JSONL_BYTES,
                actual: total_bytes,
            });
        }
        if total_lines > MAX_JSONL_LINES {
            return Err(SessionStoreError::TooLarge {
                kind: "recall transcript lines",
                limit: MAX_JSONL_LINES,
                actual: total_lines,
            });
        }
    }
    Ok(out)
}

/// Parse a bound as RFC-3339 datetime first, else `YYYY-MM-DD` at LOCAL midnight; return
/// UTC epoch milliseconds.
fn parse_date_bound(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    let naive = date.and_hms_opt(0, 0, 0)?;
    // `earliest()` (not `single()`) so an AMBIGUOUS local midnight (a fall-back DST
    // overlap) still resolves instead of silently dropping the whole time filter; for a
    // non-existent local midnight (a spring-forward gap) fall back to interpreting the
    // date as UTC, so the bound is always produced.
    Some(
        Local
            .from_local_datetime(&naive)
            .earliest()
            .map(|dt| dt.timestamp_millis())
            .unwrap_or_else(|| naive.and_utc().timestamp_millis()),
    )
}

fn format_hits(hits: &[&TurnRecord]) -> String {
    if hits.is_empty() {
        return "No matching turns found in this project's history.".to_string();
    }
    let mut out = format!(
        "Recalled {} matching turn(s) (project-local):\n",
        hits.len()
    );
    for h in hits {
        // CHAR-safe truncation (mirrors `truncate` below): a byte slice `[..8]` would
        // PANIC if the id has a multi-byte char straddling byte 8 — and session_id is an
        // unvalidated string read back from arbitrary on-disk `*.jsonl`. A panic in a
        // Tool::execute aborts the process under panic=abort.
        let short: String = h.session_id.chars().take(8).collect();
        let date = chrono::DateTime::from_timestamp_millis(h.ts)
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| h.iso.clone());
        let undone = if h.undone { " (undone)" } else { "" };
        out.push_str(&format!(
            "[session {short} · {date}{undone}] {}\n    {}\n",
            truncate(&h.user, 140),
            truncate(&h.assistant, 240),
        ));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ToolRecord, UsageRecord};

    fn rec(session: &str, ts: i64, user: &str, assistant: &str) -> TurnRecord {
        TurnRecord {
            v: crate::session::transcript::RECORD_VERSION,
            started_at: None,
            ts,
            iso: String::new(),
            session_id: session.into(),
            turn_id: 1,
            undone: false,
            user: user.into(),
            assistant: assistant.into(),
            reasoning: String::new(),
            tools: vec![],
            usage: UsageRecord::default(),
        }
    }

    fn write_jsonl(dir: &Path, name: &str, records: &[TurnRecord]) {
        let body: String = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn keyword_search_ranks_and_filters_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        // Two sessions' transcripts in the same project bucket.
        write_jsonl(
            dir.path(),
            "a.jsonl",
            &[
                rec(
                    "aaaa1111",
                    1000,
                    "fix the OAuth token refresh bug",
                    "traced it to SystemTime::now().unwrap()",
                ),
                rec("aaaa1111", 2000, "unrelated thing", "about formatting"),
            ],
        );
        write_jsonl(
            dir.path(),
            "b.jsonl",
            &[rec(
                "bbbb2222",
                3000,
                "another oauth question",
                "oauth oauth scopes",
            )],
        );

        let tool = RecallTool::new();
        let out = tool.search_dir(dir.path(), "oauth", None, None, 8).unwrap();
        // Both oauth turns matched; the one with more "oauth" occurrences ranks first.
        assert!(out.contains("Recalled 2 matching"), "got: {out}");
        let first = out.lines().nth(1).unwrap();
        assert!(
            first.contains("bbbb2222"),
            "higher keyword count ranks first: {out}"
        );
        assert!(!out.contains("unrelated thing"));
    }

    #[test]
    fn time_filter_bounds_are_inclusive_after_exclusive_before() {
        let dir = tempfile::tempdir().unwrap();
        // ts in ms; pick values around a day boundary using explicit epochs.
        let day = 24 * 3600 * 1000i64;
        write_jsonl(
            dir.path(),
            "a.jsonl",
            &[
                rec("s", day, "alpha early", "x"),
                rec("s", 3 * day, "alpha mid", "x"),
                rec("s", 5 * day, "alpha late", "x"),
            ],
        );
        let tool = RecallTool::new();
        // after = 2*day (inclusive lower), before = 5*day (exclusive upper) → only the 3*day turn.
        let after = chrono::DateTime::from_timestamp_millis(2 * day)
            .unwrap()
            .to_rfc3339();
        let before = chrono::DateTime::from_timestamp_millis(5 * day)
            .unwrap()
            .to_rfc3339();
        let out = tool
            .search_dir(dir.path(), "alpha", Some(&after), Some(&before), 8)
            .unwrap();
        assert!(out.contains("Recalled 1 matching"), "got: {out}");
        assert!(out.contains("alpha mid"));
    }

    #[test]
    fn no_match_is_a_clear_message_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(dir.path(), "a.jsonl", &[rec("s", 1, "hello", "world")]);
        let out = RecallTool::new()
            .search_dir(dir.path(), "nonexistent", None, None, 8)
            .unwrap();
        assert!(out.contains("No matching turns"));
    }

    #[test]
    fn output_footer_points_at_raw_transcripts_and_states_freshness() {
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(dir.path(), "a.jsonl", &[rec("s", 1, "hello", "world")]);
        let want_dir = dir.path().display().to_string();

        // On a hit: footer shows the REAL dir + restates the freshness boundary.
        let hit = RecallTool::new()
            .search_dir(dir.path(), "hello", None, None, 8)
            .unwrap();
        assert!(hit.contains(&want_dir), "footer shows the real dir: {hit}");
        assert!(
            hit.contains("in-progress turn"),
            "footer restates freshness: {hit}"
        );

        // On a no-match (where the "why is nothing here?" confusion lands): footer too.
        let miss = RecallTool::new()
            .search_dir(dir.path(), "nonexistent", None, None, 8)
            .unwrap();
        assert!(miss.contains("No matching turns"));
        assert!(
            miss.contains(&want_dir),
            "footer shows dir even on no-match: {miss}"
        );
    }

    #[test]
    fn limit_caps_results() {
        let dir = tempfile::tempdir().unwrap();
        let records: Vec<TurnRecord> = (0..10).map(|i| rec("s", i, "match me", "yes")).collect();
        write_jsonl(dir.path(), "a.jsonl", &records);
        let out = RecallTool::new()
            .search_dir(dir.path(), "match", None, None, 3)
            .unwrap();
        assert!(out.contains("Recalled 3 matching"), "got: {out}");
    }

    #[test]
    fn non_ascii_session_id_does_not_panic_on_format() {
        // session_id read back from on-disk jsonl is unvalidated; a multi-byte id whose
        // byte 8 is mid-codepoint would panic a byte slice — format must be char-safe.
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(
            dir.path(),
            "a.jsonl",
            &[rec("日本語のセッションid", 1, "match me", "yes")],
        );
        let out = RecallTool::new()
            .search_dir(dir.path(), "match", None, None, 8)
            .unwrap();
        assert!(
            out.contains("Recalled 1 matching"),
            "a non-ASCII id must not panic: {out}"
        );
        assert!(
            out.contains("日本語"),
            "short id is char-truncated, not byte-sliced: {out}"
        );
    }

    #[test]
    fn tool_result_includes_tool_text_in_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = rec("s", 1, "ran a command", "done");
        r.tools.push(ToolRecord {
            name: "bash".into(),
            args: "{\"cmd\":\"grep refresh_token\"}".into(),
            result: "found in auth.rs".into(),
            is_error: false,
        });
        write_jsonl(dir.path(), "a.jsonl", &[r]);
        // A term only present in the tool args/result still matches.
        let out = RecallTool::new()
            .search_dir(dir.path(), "refresh_token", None, None, 8)
            .unwrap();
        assert!(
            out.contains("Recalled 1 matching"),
            "tool text is searchable: {out}"
        );
    }

    #[test]
    fn corrupt_transcript_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.jsonl"), b"not-json\n").unwrap();

        assert!(matches!(
            RecallTool::new().search_dir(dir.path(), "anything", None, None, 8),
            Err(SessionStoreError::Corrupt {
                kind: "transcript record",
                ..
            })
        ));
    }

    #[test]
    fn future_transcript_schema_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut future = rec("s", 1, "hello", "world");
        future.v = crate::session::transcript::RECORD_VERSION + 1;
        write_jsonl(dir.path(), "future.jsonl", &[future]);

        assert!(matches!(
            RecallTool::new().search_dir(dir.path(), "hello", None, None, 8),
            Err(SessionStoreError::FutureSchema {
                kind: "transcript record",
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_transcript_is_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        write_jsonl(dir.path(), "target.txt", &[rec("s", 1, "hello", "world")]);
        symlink(&target, dir.path().join("linked.jsonl")).unwrap();

        assert!(matches!(
            RecallTool::new().search_dir(dir.path(), "hello", None, None, 8),
            Err(SessionStoreError::UnsafeFile { .. })
        ));
    }

    #[test]
    fn tokenize_query_preserves_ascii_behavior() {
        assert_eq!(
            tokenize_query("OAuth Refresh  TOKEN"),
            vec!["oauth", "refresh", "token"]
        );
        assert_eq!(tokenize_query("oauth.refresh"), vec!["oauth.refresh"]);
        assert!(tokenize_query("").is_empty());
        assert!(tokenize_query("   ").is_empty());
    }

    #[test]
    fn tokenize_query_expands_cjk_to_bigrams() {
        assert_eq!(tokenize_query("工作任务"), vec!["工作", "作任", "任务"]);
        assert_eq!(tokenize_query("工"), vec!["工"]);
        assert_eq!(tokenize_query("工作任务 工作"), vec!["工作", "作任", "任务"]);
    }

    #[test]
    fn tokenize_query_cleans_punctuation_and_connectors() {
        assert_eq!(tokenize_query("工作,任务"), vec!["工作", "作任", "任务"]);
        assert_eq!(tokenize_query("工作，任务"), vec!["工作", "作任", "任务"]);
        assert_eq!(tokenize_query("关于工作 任务"), vec!["工作", "任务"]);
        assert_eq!(tokenize_query("工作的"), vec!["工作"]);
        assert_eq!(tokenize_query("目的"), vec!["目的"]);
        assert!(tokenize_query("的 了 和").is_empty());
    }

    #[test]
    fn tokenize_query_handles_mixed_ascii_cjk() {
        assert_eq!(tokenize_query("OAuth的token"), vec!["oauth", "token"]);
        let kana = tokenize_query("日本語のセッションid");
        assert!(!kana.is_empty());
        assert!(kana.iter().any(|t| t == "id"));
    }

    #[test]
    fn zh_no_space_phrase_hits_split_document() {
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(
            dir.path(),
            "zh1.jsonl",
            &[rec("zh1", 1000, "请帮我把工作上的任务安排整理成清单", "好的")],
        );
        write_jsonl(
            dir.path(),
            "zh2.jsonl",
            &[rec("zh2", 2000, "关于咖啡豆的烘焙记录", "嗯")],
        );
        let out = RecallTool::new()
            .search_dir(dir.path(), "工作任务", None, None, 8)
            .unwrap();
        assert!(out.contains("Recalled 1 matching"), "got: {out}");
        assert!(out.contains("zh1"), "got: {out}");
        assert!(!out.contains("烘焙记录"), "unrelated turn must not match: {out}");
    }

    #[test]
    fn zh_punctuation_pollution_query_hits() {
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(dir.path(), "zh3.jsonl", &[rec("zh3", 1000, "工作任务", "收到")]);
        let tool = RecallTool::new();
        let ascii = tool
            .search_dir(dir.path(), "工作,任务", None, None, 8)
            .unwrap();
        assert!(ascii.contains("Recalled 1 matching"), "got: {ascii}");
        let fullwidth = tool
            .search_dir(dir.path(), "工作，任务", None, None, 8)
            .unwrap();
        assert!(fullwidth.contains("Recalled 1 matching"), "got: {fullwidth}");
    }

    #[test]
    fn zh_common_word_does_not_bury_all_terms_match() {
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(
            dir.path(),
            "za.jsonl",
            &[rec("za", 1000, "这周的工作任务都完成了", "好的")],
        );
        write_jsonl(
            dir.path(),
            "zb.jsonl",
            &[rec("zb", 2000, "工作", &"工作".repeat(20))],
        );
        write_jsonl(
            dir.path(),
            "zc.jsonl",
            &[rec("zc", 1500, "任务", &"任务".repeat(30))],
        );
        let out = RecallTool::new()
            .search_dir(dir.path(), "工作 任务", None, None, 8)
            .unwrap();
        assert!(out.contains("Recalled 3 matching"), "got: {out}");
        // All-terms match (za) ranks first despite being oldest/least frequent…
        assert!(
            out.lines().nth(1).unwrap().contains("za"),
            "coverage must win: {out}"
        );
        // …then same-coverage higher-count (zc, 31) before lower-count (zb, 21).
        assert!(
            out.lines().nth(3).unwrap().contains("zc"),
            "same coverage, higher count next: {out}"
        );
    }

    #[test]
    fn empty_and_all_stopword_queries_degrade_to_no_match() {
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(dir.path(), "a.jsonl", &[rec("s", 1, "工作 任务", "你好")]);
        let tool = RecallTool::new();
        for q in ["", "   ", "的 了 和"] {
            let out = tool.search_dir(dir.path(), q, None, None, 8).unwrap();
            assert!(out.contains("No matching turns"), "query {q:?}: got {out}");
        }
    }
}
