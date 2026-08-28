//! `/worklog` data collection: gather a single local day's COMPLETED turns across
//! ALL projects, with per-turn *agent-active* durations, so a command can hand the
//! model an accurate, pre-computed recap to fill a fixed template (work items /
//! time / issues & evaluation).
//!
//! Deterministic and read-only — the narrative is left to the LLM; only the facts
//! (which turns, how long each ran, which tools, did anything error) are computed
//! here. Durations are `ts - started_at` (the turn's wall-clock): a PROXY for
//! effort, NOT real human work time — a turn left open or stuck on a hung tool
//! inflates it, so callers label it approximate and let the user override.

use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate};

use super::manager::for_each_jsonl_line;
use super::{CatalogEntry, SessionManager, TurnRecord};

/// Bound the number of session files read in one recap — a backstop for a
/// pathological store; a normal day overlaps a handful of sessions.
const MAX_SESSIONS_READ: usize = 800;
/// Defensive cap on turns pulled into one recap.
const MAX_WORKLOG_TURNS: usize = 4000;
/// Prompt-size guards: cap the turns RENDERED into the injected prompt so a busy
/// multi-project day can't blow the model's context window. The rest are folded
/// into a "+N more" line per project.
const MAX_TURNS_PER_PROJECT: usize = 25;
const MAX_TURNS_RENDERED: usize = 150;
/// A session whose last snapshot landed up to this long before the window start
/// is still read: the catalog `updated_at` is stamped (SnapshotHook) slightly
/// BEFORE the transcript `ts` (TranscriptHook) within the same terminal, so a
/// just-after-midnight first turn of the day can have `updated_at < after_ms`.
const OVERLAP_SKEW_MS: i64 = 10 * 60 * 1000;

/// One completed turn on the target day, distilled for the worklog prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct WorklogTurn {
    /// The session's project directory (recap is grouped by project).
    pub project: PathBuf,
    /// Raw user intent — the prompt that opened the turn.
    pub user: String,
    /// Agent-active wall-clock (`ts - started_at`), ms. `None` for records that
    /// predate `started_at`. A PROXY for effort, not real work time.
    pub duration_ms: Option<i64>,
    /// Tool names invoked, deduped in first-seen order (an activity fingerprint).
    pub tools: Vec<String>,
    /// Any tool errored this turn (feeds the "issues" column).
    pub had_error: bool,
    /// Turn end, epoch ms UTC (ordering).
    pub ts: i64,
}

impl WorklogTurn {
    fn from_record(project: PathBuf, r: &TurnRecord) -> Self {
        // Clamp to ≥0: clock skew / a repaired record must never yield a negative.
        let duration_ms = r.started_at.map(|s| (r.ts - s).max(0));
        let mut tools = Vec::new();
        let mut had_error = false;
        for t in &r.tools {
            had_error |= t.is_error;
            if !tools.iter().any(|n: &String| n == &t.name) {
                tools.push(t.name.clone());
            }
        }
        WorklogTurn {
            project,
            user: r.user.clone(),
            duration_ms,
            tools,
            had_error,
            ts: r.ts,
        }
    }
}

/// A session's lifetime `[created, updated]` overlaps the `[after, before)` day —
/// the cheap pre-filter that keeps a day recap from reading the whole history.
/// The lower bound is relaxed by [`OVERLAP_SKEW_MS`] to tolerate the snapshot/
/// transcript timestamp gap (see that constant).
fn session_overlaps_day(entry: &CatalogEntry, after_ms: i64, before_ms: i64) -> bool {
    entry.created_at_ms < before_ms && entry.updated_at_ms >= after_ms - OVERLAP_SKEW_MS
}

/// Collect every completed turn whose `ts` is in `[after_ms, before_ms)` across ALL
/// projects, sorted by `ts`. Only sessions whose lifetime overlaps the window are
/// read (cached catalog + per-file byte caps bound the cost). Best-effort: a
/// corrupt line/file is skipped, not fatal (a recap should degrade, not fail).
pub fn collect_day_turns(sessions_root: &Path, after_ms: i64, before_ms: i64) -> Vec<WorklogTurn> {
    let scan = SessionManager::scan_catalog(sessions_root);
    let mut out: Vec<WorklogTurn> = Vec::new();
    let mut sessions_read = 0usize;
    for entry in &scan.entries {
        if out.len() >= MAX_WORKLOG_TURNS || sessions_read >= MAX_SESSIONS_READ {
            break;
        }
        if !session_overlaps_day(entry, after_ms, before_ms) {
            continue;
        }
        sessions_read += 1;
        let path = sessions_root
            .join(&entry.project_bucket)
            .join(format!("{}.jsonl", entry.id));
        let _ = for_each_jsonl_line(&path, |line| {
            if out.len() >= MAX_WORKLOG_TURNS {
                return Ok(());
            }
            if let Ok(rec) = serde_json::from_slice::<TurnRecord>(line) {
                let in_day = rec.ts >= after_ms && rec.ts < before_ms;
                // Skip rewound turns and mid-turn synthetic continuations (no user text).
                if in_day && !rec.undone && !rec.user.trim().is_empty() {
                    out.push(WorklogTurn::from_record(entry.working_dir.clone(), &rec));
                }
            }
            Ok(())
        });
    }
    out.sort_by_key(|t| t.ts);
    out
}

/// Resolve a `/worklog` date argument against `today` (injected for testing):
/// empty / `today` → today; `yesterday` → today−1; `YYYY-MM-DD`, `YYYY/M/D`, or a
/// bare `M/D` / `M-D` (current year). Returns `None` on an unparseable argument.
pub fn resolve_worklog_date(arg: &str, today: NaiveDate) -> Option<NaiveDate> {
    let a = arg.trim();
    if a.is_empty() || a.eq_ignore_ascii_case("today") {
        return Some(today);
    }
    if a.eq_ignore_ascii_case("yesterday") {
        return today.pred_opt();
    }
    let parts: Vec<&str> = a.split(['-', '/']).collect();
    match parts.as_slice() {
        [y, m, d] => NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?),
        // Bare M/D → current year.
        [m, d] => NaiveDate::from_ymd_opt(today.year(), m.parse().ok()?, d.parse().ok()?),
        _ => None,
    }
}

fn fmt_duration(ms: i64) -> String {
    let secs = ms / 1000;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

/// Build the prompt injected as the user's turn: a localized template instruction
/// + the deterministic day data grouped by project, with pre-aggregated durations
/// so the `时长` / duration column is accurate (an agent-active proxy). Rendered
/// turns are capped to protect the model's context window. Pure ⇒ unit-testable.
pub fn build_worklog_prompt(date_label: &str, turns: &[WorklogTurn], english: bool) -> String {
    let mut s = String::new();
    if english {
        s.push_str(&format!("# Work recap for {date_label}\n\n"));
    } else {
        s.push_str(&format!("# {date_label} 工作复盘\n\n"));
    }
    if turns.is_empty() {
        s.push_str(if english {
            "No completed AtomCode sessions on this day. Reply that there is no work to recap for this date.\n"
        } else {
            "这一天在 AtomCode 上没有已完成的会话记录。请回复:该日期没有可复盘的工作记录。\n"
        });
        return s;
    }
    s.push_str(if english { INSTRUCTION_EN } else { INSTRUCTION_ZH });
    s.push_str(if english {
        "\n---\nRaw activity:\n"
    } else {
        "\n---\n原始工作痕迹:\n"
    });

    // Group by project (stable: first-seen order), preserving per-turn ts order.
    let mut projects: Vec<&PathBuf> = Vec::new();
    for t in turns {
        if !projects.iter().any(|p| **p == t.project) {
            projects.push(&t.project);
        }
    }
    let mut grand_total_ms: i64 = 0;
    let mut rendered = 0usize;
    for project in projects {
        let name = project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.to_string_lossy().into_owned());
        s.push_str(&format!(
            "\n## {} {name}\n",
            if english { "Project" } else { "项目" }
        ));
        let mut project_total_ms: i64 = 0;
        let mut shown = 0usize;
        let mut omitted = 0usize;
        for t in turns.iter().filter(|t| t.project == *project) {
            // Durations still count toward totals even for omitted turns.
            if let Some(ms) = t.duration_ms {
                project_total_ms += ms;
                grand_total_ms += ms;
            }
            if shown >= MAX_TURNS_PER_PROJECT || rendered >= MAX_TURNS_RENDERED {
                omitted += 1;
                continue;
            }
            let dur = match t.duration_ms {
                Some(ms) => {
                    let long = ms > 30 * 60 * 1000; // >30min: likely wait/hang, flag it.
                    let tag = if long {
                        if english {
                            " (incl. wait/hang)"
                        } else {
                            "（含等待/挂起）"
                        }
                    } else {
                        ""
                    };
                    format!("≈{}{tag}", fmt_duration(ms))
                }
                None => "?".to_string(),
            };
            let intent = one_line(&t.user, 100);
            let tools = if t.tools.is_empty() {
                String::new()
            } else {
                format!(
                    " · {}: {}",
                    if english { "tools" } else { "工具" },
                    t.tools.join(", ")
                )
            };
            let err = if t.had_error {
                if english {
                    " · ⚠ error"
                } else {
                    " · ⚠ 有报错"
                }
            } else {
                ""
            };
            s.push_str(&format!("- [{dur}] {intent}{tools}{err}\n"));
            shown += 1;
            rendered += 1;
        }
        if omitted > 0 {
            s.push_str(&format!(
                "- {}\n",
                if english {
                    format!("+{omitted} more turn(s) omitted")
                } else {
                    format!("+另有 {omitted} 轮已省略")
                }
            ));
        }
        if project_total_ms > 0 {
            s.push_str(&format!(
                "  {} ≈ {}\n",
                if english { "subtotal" } else { "小计" },
                fmt_duration(project_total_ms)
            ));
        }
    }
    if grand_total_ms > 0 {
        s.push_str(&format!(
            "\n{} ≈ {}{}\n",
            if english { "Day total" } else { "全天合计" },
            fmt_duration(grand_total_ms),
            if english {
                " (agent-active wall-clock)"
            } else {
                "（agent 活跃墙钟）"
            }
        ));
    }
    s
}

const INSTRUCTION_ZH: &str = "\
以下是这一天（跨所有项目）从 AtomCode 会话记录里确定性提取的工作痕迹。请据此生成一张 Markdown 表格,列为:\n\
`工作内容` | `时长` | `问题与评价`。要求:\n\
- 工作内容:按主题归并同类轮次(可跨项目/会话),用简洁的动宾短语,合并重复。\n\
- 时长:填我给出的每项聚合时长(标了 ≈ 的是 agent 活跃墙钟,是工作量的近似、非真实工时;\
带「含等待/挂起」的更不可信)。用户会手改成真实工时,你照原样填即可、不要自己编。\n\
- 问题与评价:根据当天出现的报错/取消/反复重试,提炼遇到的问题与简短评价;没有则留「—」。\n\
只输出表格,不要额外解释。\n";

const INSTRUCTION_EN: &str = "\
Below is a deterministic extract of this day's activity (across all projects) from AtomCode's \
session records. Produce ONE Markdown table with columns: `Work item` | `Time` | `Issues & notes`. \
Rules:\n\
- Work item: merge related turns by topic (across projects/sessions) into concise action phrases; \
deduplicate.\n\
- Time: fill in the aggregated duration I give per item (a `≈` value is agent-active wall-clock — \
an approximation of effort, NOT real work time; `(incl. wait/hang)` is even less reliable). The user \
will hand-edit these to real hours, so copy them as-is; do NOT invent times.\n\
- Issues & notes: from the day's errors/cancellations/retries, summarize problems and a brief \
assessment; use `—` when none.\n\
Output only the table, no extra prose.\n";

/// Collapse to a single trimmed line, truncated to `max` chars with an ellipsis.
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let head: String = flat.chars().take(max).collect();
        format!("{head}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(project: &str, user: &str, dur_ms: Option<i64>, ts: i64) -> WorklogTurn {
        WorklogTurn {
            project: PathBuf::from(project),
            user: user.into(),
            duration_ms: dur_ms,
            tools: vec![],
            had_error: false,
            ts,
        }
    }

    #[test]
    fn resolve_worklog_date_handles_relative_and_explicit_forms() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 28).unwrap();
        assert_eq!(resolve_worklog_date("", today), Some(today));
        assert_eq!(resolve_worklog_date("today", today), Some(today));
        assert_eq!(
            resolve_worklog_date("yesterday", today),
            NaiveDate::from_ymd_opt(2026, 8, 27)
        );
        assert_eq!(
            resolve_worklog_date("2026-08-27", today),
            NaiveDate::from_ymd_opt(2026, 8, 27)
        );
        assert_eq!(
            resolve_worklog_date("8/27", today),
            NaiveDate::from_ymd_opt(2026, 8, 27),
            "bare M/D uses the current year"
        );
        assert_eq!(resolve_worklog_date("nonsense", today), None);
        assert_eq!(resolve_worklog_date("2026-13-40", today), None);
        // `.` is NOT an accepted separator (grammar == advertised forms).
        assert_eq!(resolve_worklog_date("2026.08.27", today), None);
    }

    #[test]
    fn session_overlaps_day_tolerates_the_snapshot_transcript_skew() {
        let mut e = CatalogEntry {
            id: "a".into(),
            name: "n".into(),
            fork_root_id: None,
            project_bucket: "b".into(),
            working_dir: PathBuf::from("/p"),
            created_at_ms: 100,
            updated_at_ms: 200,
            message_count: 1,
            turn_count: 1,
            presence: crate::session::CatalogPresence::NativeOnly,
        };
        assert!(session_overlaps_day(&e, 150, 300));
        // Session ended (updated=200) far before the window start, beyond the skew.
        assert!(!session_overlaps_day(&e, 10_000_000, 20_000_000));
        // updated_at just below after_ms but WITHIN the skew margin → still read
        // (the near-midnight first-turn case).
        e.created_at_ms = 0;
        e.updated_at_ms = 1_000_000;
        assert!(session_overlaps_day(&e, 1_000_000 + OVERLAP_SKEW_MS - 1, 2_000_000));
        // Beyond the skew margin → excluded.
        assert!(!session_overlaps_day(&e, 1_000_000 + OVERLAP_SKEW_MS + 1, 2_000_000));
    }

    #[test]
    fn build_worklog_prompt_groups_by_project_flags_long_turns_and_localizes() {
        let turns = vec![
            turn("/w/atomcode", "quantize GLM", Some(2 * 3600 * 1000), 10),
            turn("/w/atomcode", "ascend port", Some(6 * 3600 * 1000), 20), // >30min → flagged
            turn("/w/other", "tune qwen", Some(60 * 1000), 30),
        ];
        let zh = build_worklog_prompt("8/27", &turns, false);
        assert!(zh.contains("工作内容") && zh.contains("时长") && zh.contains("问题与评价"));
        assert!(zh.contains("项目 atomcode") && zh.contains("项目 other"), "grouped: {zh}");
        assert!(zh.contains("≈2h0m") && zh.contains("含等待/挂起"), "long flagged: {zh}");
        assert!(zh.contains("全天合计"), "grand total: {zh}");

        let en = build_worklog_prompt("8/27", &turns, true);
        assert!(en.contains("Work item") && en.contains("Issues & notes"), "en template: {en}");
        assert!(!en.contains("工作内容") && !en.contains("项目"), "no Chinese in en: {en}");
        assert!(en.contains("incl. wait/hang") && en.contains("Day total"), "{en}");
    }

    #[test]
    fn build_worklog_prompt_caps_rendered_turns_but_totals_stay_complete() {
        // 40 turns of 1 minute each in one project; per-project render cap is 25.
        let turns: Vec<WorklogTurn> = (0..40)
            .map(|i| turn("/w/p", &format!("task {i}"), Some(60 * 1000), i))
            .collect();
        let out = build_worklog_prompt("8/27", &turns, false);
        let shown = out.matches("- [≈1m]").count();
        assert_eq!(shown, super::MAX_TURNS_PER_PROJECT, "rendered turns capped: shown={shown}");
        assert!(out.contains("已省略"), "omitted count surfaced: {out}");
        // 40 min total still accounts for ALL turns, not just the rendered 25.
        assert!(out.contains("40m"), "totals cover omitted turns too: {out}");
    }

    #[test]
    fn build_worklog_prompt_empty_day_asks_for_a_no_work_reply() {
        assert!(build_worklog_prompt("8/27", &[], false).contains("没有"));
        assert!(build_worklog_prompt("8/27", &[], true).contains("no work"));
    }

    #[test]
    fn fmt_duration_scales_minutes_and_hours() {
        assert_eq!(fmt_duration(60 * 1000), "1m");
        assert_eq!(fmt_duration(90 * 60 * 1000), "1h30m");
        assert_eq!(fmt_duration(0), "0m");
    }
}
