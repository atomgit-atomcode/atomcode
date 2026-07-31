//! `atomcode schedule` subcommand — add / list / remove / enable / disable.
//!
//! Task 4 will fill in the `Run` arm; for now it returns a non-zero exit
//! code with an informational message so callers can detect the stub.

use anyhow::{Context, Result};
use clap::Subcommand;

use atomcode_config::schedule::{self, Schedule, ScheduleTask};

// ── CLI enum ──────────────────────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum ScheduleCli {
    /// Add a new scheduled task.
    Add {
        /// Human-readable name for the task.
        #[arg(long)]
        title: String,

        /// Prompt text to send to the agent when the task fires.
        #[arg(long)]
        prompt: String,

        /// Working directory for the agent session (defaults to current dir).
        #[arg(long)]
        cwd: Option<String>,

        /// Schedule: run daily at HH:MM (e.g. "09:00").
        #[arg(long, value_name = "HH:MM", group = "freq")]
        daily: Option<String>,

        /// Schedule: run weekly, format N@HH:MM (N=1..7, 1=Mon).
        #[arg(long, value_name = "N@HH:MM", group = "freq")]
        weekly: Option<String>,

        /// Schedule: run every N minutes (e.g. "30m").
        #[arg(long, value_name = "Nm", group = "freq")]
        every: Option<String>,

        /// Schedule: run once per hour.
        #[arg(long, group = "freq")]
        hourly: bool,

        /// Schedule: cron expression (e.g. "0 9 * * 1-5").
        #[arg(long, value_name = "EXPR", group = "freq")]
        cron: Option<String>,

        /// Permission mode: plan | accept_edits | auto.
        #[arg(long, default_value = "plan")]
        mode: String,

        /// Notify level: off | important | all.
        #[arg(long, default_value = "important")]
        notify: String,
    },

    /// List all scheduled tasks.
    List,

    /// Remove a scheduled task by id.
    Remove {
        /// Task id (shown in `atomcode schedule list`).
        id: String,
    },

    /// Enable a scheduled task.
    Enable {
        /// Task id.
        id: String,
    },

    /// Disable a scheduled task (it will no longer fire).
    Disable {
        /// Task id.
        id: String,
    },

    /// Run a scheduled task immediately (implemented in Task 4).
    #[command(hide = true)]
    Run {
        /// Task id.
        id: String,
    },
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// Parse CLI frequency flags into a [`Schedule`].
///
/// Exactly one frequency flag must be present (enforced by clap's `group`).
/// Returns an error only when the flag value has an invalid format.
fn parse_schedule(
    daily: Option<&str>,
    weekly: Option<&str>,
    every: Option<&str>,
    hourly: bool,
    cron: Option<&str>,
) -> Result<Schedule> {
    if let Some(t) = daily {
        // Validate HH:MM format minimally.
        if !t.contains(':') {
            anyhow::bail!("--daily expects HH:MM format, got {:?}", t);
        }
        return Ok(Schedule::Daily { time: t.to_string() });
    }
    if let Some(w) = weekly {
        // Format: N@HH:MM
        let (n_str, time) = w
            .split_once('@')
            .with_context(|| format!("--weekly expects N@HH:MM format, got {:?}", w))?;
        let weekday: u8 = n_str
            .parse()
            .with_context(|| format!("--weekly weekday must be 1..7, got {:?}", n_str))?;
        if !(1..=7).contains(&weekday) {
            anyhow::bail!("--weekly weekday must be 1..7, got {}", weekday);
        }
        if !time.contains(':') {
            anyhow::bail!("--weekly time must be HH:MM, got {:?}", time);
        }
        return Ok(Schedule::Weekly {
            weekday,
            time: time.to_string(),
        });
    }
    if let Some(e) = every {
        // Format: Nm  (e.g. "30m")
        let minutes_str = e
            .strip_suffix('m')
            .with_context(|| format!("--every expects format like '30m', got {:?}", e))?;
        let every_minutes: u32 = minutes_str
            .parse()
            .with_context(|| format!("--every minutes value must be a positive integer, got {:?}", minutes_str))?;
        if every_minutes == 0 {
            anyhow::bail!("--every minutes must be > 0");
        }
        return Ok(Schedule::Interval { every_minutes });
    }
    if hourly {
        return Ok(Schedule::Hourly);
    }
    if let Some(expr) = cron {
        return Ok(Schedule::Cron { expr: expr.to_string() });
    }
    anyhow::bail!(
        "one frequency flag is required: --daily HH:MM | --weekly N@HH:MM | \
         --every Nm | --hourly | --cron EXPR"
    )
}

/// Build a [`ScheduleTask`] from raw CLI arguments.  Pure function — no I/O.
///
/// `id` = `slug(title)-<first 6 chars of a new UUIDv4>`.
pub fn build_task(
    title: &str,
    prompt: &str,
    cwd: &str,
    sched: Schedule,
    permission_mode: &str,
    notify: &str,
) -> ScheduleTask {
    let slug = slug(title);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let id = format!("{}-{}", slug, &suffix[..6]);
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    ScheduleTask {
        id,
        title: title.to_string(),
        prompt: prompt.to_string(),
        cwd: cwd.to_string(),
        schedule: sched,
        permission_mode: permission_mode.to_string(),
        notify: notify.to_string(),
        enabled: true,
        created_at,
        last_run_at: None,
        last_status: None,
    }
}

/// Convert a title string to a slug usable as part of an id.
///
/// Lowercases, replaces non-alphanumeric runs with `-`, strips leading/trailing
/// dashes, truncates to 32 chars.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // suppress leading dash
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    // strip trailing dash
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "task".to_string()
    } else {
        out.chars().take(32).collect()
    }
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Dispatch `atomcode schedule <subcommand>`.  Returns an exit code.
pub async fn handle_schedule(cli: ScheduleCli) -> Result<i32> {
    match cli {
        ScheduleCli::Add {
            title,
            prompt,
            cwd,
            daily,
            weekly,
            every,
            hourly,
            cron,
            mode,
            notify,
        } => {
            let sched = parse_schedule(
                daily.as_deref(),
                weekly.as_deref(),
                every.as_deref(),
                hourly,
                cron.as_deref(),
            )?;
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            let task = build_task(&title, &prompt, &cwd, sched, &mode, &notify);
            schedule::save(&task)
                .with_context(|| format!("failed to save scheduled task {:?}", task.id))?;
            println!("  Added task {} ({})", task.id, task.title);
            Ok(0)
        }

        ScheduleCli::List => {
            let tasks = schedule::list();
            if tasks.is_empty() {
                println!("  No scheduled tasks. Use `atomcode schedule add` to create one.");
                return Ok(0);
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            for t in &tasks {
                let next = schedule::next_run(&t.schedule, now)
                    .map(|ts| format_epoch(ts))
                    .unwrap_or_else(|| "-".to_string());
                let last = t.last_status.as_deref().unwrap_or("-");
                let state = if t.enabled { "on" } else { "off" };
                println!(
                    "  {} | {} | next:{} | last:{} | {}",
                    t.id, t.title, next, last, state
                );
            }
            Ok(0)
        }

        ScheduleCli::Remove { id } => {
            schedule::remove(&id)
                .with_context(|| format!("failed to remove task {:?}", id))?;
            println!("  Removed task {}", id);
            Ok(0)
        }

        ScheduleCli::Enable { id } => {
            let mut task = schedule::load(&id)
                .with_context(|| format!("task {:?} not found", id))?;
            task.enabled = true;
            schedule::save(&task)
                .with_context(|| format!("failed to save task {:?}", id))?;
            println!("  Enabled task {} ({})", task.id, task.title);
            Ok(0)
        }

        ScheduleCli::Disable { id } => {
            let mut task = schedule::load(&id)
                .with_context(|| format!("task {:?} not found", id))?;
            task.enabled = false;
            schedule::save(&task)
                .with_context(|| format!("failed to save task {:?}", id))?;
            println!("  Disabled task {} ({})", task.id, task.title);
            Ok(0)
        }

        ScheduleCli::Run { id } => run_task(&id).await,
    }
}

// ── Pure helpers (also tested below) ──────────────────────────────────────────

/// Map a permission-mode string from [`ScheduleTask::permission_mode`] to the
/// corresponding [`atomcode_coding::RuntimeMode`].  Unknown strings default to
/// `Plan` (safe default: read-only, never auto-approves destructive operations).
pub(crate) fn mode_from_str(s: &str) -> atomcode_coding::RuntimeMode {
    match s {
        "accept_edits" => atomcode_coding::RuntimeMode::AcceptEdits,
        "auto" => atomcode_coding::RuntimeMode::Auto,
        _ => atomcode_coding::RuntimeMode::Plan, // plan + unknown → safe default
    }
}

/// Map a headless exit code to the `last_status` string stored in the task record.
pub(crate) fn last_status_for(exit_code: i32) -> &'static str {
    match exit_code {
        0 => "ok",
        130 => "cancelled",
        _ => "error",
    }
}

// ── Executor ──────────────────────────────────────────────────────────────────

/// Run a scheduled task by id, reusing the same headless bootstrap as `-p`.
///
/// Returns an exit code:  0 = ok, 130 = cancelled (SIGINT), other non-zero = error.
/// If the task is disabled this returns 0 immediately.  If the task's working
/// directory does not exist the task's `last_status` is set to "error" and the
/// function returns non-zero.
async fn run_task(id: &str) -> Result<i32> {
    use atomcode_capabilities::session::manager::SessionOrigin;
    use atomcode_capabilities::session::SessionManager;
    use atomcode_coding::ProviderBootstrap;
    use atomcode_config::config::Config;

    // 1. Load task record.
    let mut task = schedule::load(id)
        .with_context(|| format!("task {:?} not found", id))?;

    // 2. Skip if disabled.
    if !task.enabled {
        println!("  schedule run: task {} is disabled, skipping", task.id);
        return Ok(0);
    }

    // 3. Load user config from default path.
    let config_path = Config::default_path();
    let config = if config_path.exists() {
        Config::load(&config_path).unwrap_or_default()
    } else {
        Config::default()
    };

    // 4. Resolve working directory.
    let cwd = std::path::PathBuf::from(&task.cwd);
    if !cwd.exists() {
        eprintln!(
            "[schedule] working directory {:?} does not exist for task {}",
            cwd, task.id
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        task.last_run_at = Some(now);
        task.last_status = Some("error".to_string());
        let _ = schedule::save(&task);
        return Ok(1);
    }

    // 5. Build runtime config.  auto → skip_permissions; plan/accept_edits → false.
    let mode = mode_from_str(&task.permission_mode);
    let runtime_cfg = crate::runtime_config_from(
        &config,
        &cwd,
        None,
        None, // no per-task telemetry arc needed
        mode.is_auto(),
        false, // headless: fail-closed approval timeout
    );

    // 6. Spawn runtime (Fresh session — no resume).
    // Headless path always uses ProviderBootstrap::Required so it fails fast
    // when no provider is configured, mirroring the `-p` / `--prompt` flow.
    let (runtime, agent, _cont) = crate::spawn_native_cli_runtime(
        &runtime_cfg,
        None,
        ProviderBootstrap::Required,
        false,
        false,
    )
    .await?;

    // 7. Mark session origin = Scheduled.
    if let Some(ref session_info) = runtime.session {
        let sid = session_info.id.clone();
        let manager = SessionManager::for_project(&agent.working_dir);
        // Best-effort — don't abort the run if meta update fails.
        let _ = manager.update_meta(&sid, |m| {
            m.origin = SessionOrigin::Scheduled;
        });
    }

    // 8. For non-auto modes, set the runtime mode explicitly after spawn.
    //    (auto was already set inside spawn_native_cli_runtime via dangerously_skip_permissions.)
    if !mode.is_auto() {
        runtime
            .handle
            .set_mode(mode)
            .await
            .map_err(anyhow::Error::new)?;
    }

    // 9. Build notification config.
    //    "off" → disabled config; anything else → use the user's config.
    let notifications_cfg = if task.notify == "off" {
        atomcode_config::config::NotificationConfig {
            enabled: false,
            ..Default::default()
        }
    } else {
        config.notifications.clone()
    };

    // 10. Run headless.
    let (exit_code, _captured) = crate::run_native_headless(
        notifications_cfg,
        runtime,
        task.prompt.clone(),
        None,
        false,
        false,
        cwd.clone(),
        mode.is_auto(),
        false,
    )
    .await?;

    // 11. Write back last_run_at and last_status.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    task.last_run_at = Some(now);
    task.last_status = Some(last_status_for(exit_code).to_string());
    let _ = schedule::save(&task);

    Ok(exit_code)
}

/// Format an epoch-seconds timestamp as a human-readable UTC string.
fn format_epoch(epoch_secs: i64) -> String {
    // Simple UTC formatting without pulling in chrono.
    // epoch_secs → days/hours/minutes.
    let secs = epoch_secs;
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Rough Gregorian calendar: good enough for display.
    // 2000-01-01 = day 10957 since epoch.
    let (year, month, day) = days_to_ymd(days);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z", year, month, day, h, m, s)
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    // Proleptic Gregorian, no negative dates needed (all future timestamps).
    let mut year = 1970i64;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let leap = is_leap(year);
    let month_days: &[u32] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u32;
    for &md in month_days {
        if days < md as i64 {
            break;
        }
        days -= md as i64;
        month += 1;
    }
    (year, month, days as u32 + 1)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pure-function tests (no I/O, no env) ──────────────────────────────────

    #[test]
    fn permission_mode_str_maps_to_runtime_mode() {
        use atomcode_coding::RuntimeMode;
        assert_eq!(mode_from_str("plan"), RuntimeMode::Plan);
        assert_eq!(mode_from_str("accept_edits"), RuntimeMode::AcceptEdits);
        assert_eq!(mode_from_str("auto"), RuntimeMode::Auto);
        assert_eq!(mode_from_str("bogus"), RuntimeMode::Plan); // safe default
    }

    #[test]
    fn exit_code_maps_to_last_status() {
        assert_eq!(last_status_for(0), "ok");
        assert_eq!(last_status_for(130), "cancelled");
        assert_eq!(last_status_for(1), "error");
    }

    #[test]
    fn build_task_daily_fields() {
        let sched = Schedule::Daily { time: "09:00".into() };
        let t = build_task("Brief", "summarize", "/tmp/p", sched, "plan", "important");
        assert_eq!(t.title, "Brief");
        assert_eq!(t.prompt, "summarize");
        assert_eq!(t.cwd, "/tmp/p");
        assert_eq!(t.permission_mode, "plan");
        assert_eq!(t.notify, "important");
        assert!(t.enabled);
        assert!(!t.id.is_empty());
        // id starts with slug
        assert!(t.id.starts_with("brief-"), "id={}", t.id);
        // id ends with 6-char hex suffix
        let parts: Vec<&str> = t.id.rsplitn(2, '-').collect();
        assert_eq!(parts[0].len(), 6, "suffix len, id={}", t.id);
        assert!(t.created_at > 0);
        assert!(t.last_run_at.is_none());
        assert!(t.last_status.is_none());
    }

    #[test]
    fn slug_handles_special_chars() {
        assert_eq!(slug("Hello World!"), "hello-world");
        assert_eq!(slug("  leading-trailing  "), "leading-trailing");
        // Rust's char::is_alphanumeric includes Unicode letters, so CJK stays.
        // The slug lowercases ASCII only; non-ASCII letters pass through.
        let s = slug("日本語");
        assert!(!s.is_empty(), "CJK is alphanumeric in Rust; slug should not be empty");
        // purely punctuation → "task" fallback
        assert_eq!(slug("!!!"), "task");
        // long title gets truncated
        let long = "a".repeat(50);
        assert_eq!(slug(&long).len(), 32);
    }

    #[test]
    fn parse_schedule_daily_ok() {
        let s = parse_schedule(Some("09:30"), None, None, false, None).unwrap();
        assert!(matches!(s, Schedule::Daily { time } if time == "09:30"));
    }

    #[test]
    fn parse_schedule_weekly_ok() {
        let s = parse_schedule(None, Some("3@14:00"), None, false, None).unwrap();
        assert!(matches!(s, Schedule::Weekly { weekday: 3, time } if time == "14:00"));
    }

    #[test]
    fn parse_schedule_weekly_bad_weekday() {
        assert!(parse_schedule(None, Some("8@09:00"), None, false, None).is_err());
    }

    #[test]
    fn parse_schedule_every_ok() {
        let s = parse_schedule(None, None, Some("30m"), false, None).unwrap();
        assert!(matches!(s, Schedule::Interval { every_minutes: 30 }));
    }

    #[test]
    fn parse_schedule_every_bad_format() {
        assert!(parse_schedule(None, None, Some("30"), false, None).is_err());
    }

    #[test]
    fn parse_schedule_every_zero() {
        assert!(parse_schedule(None, None, Some("0m"), false, None).is_err());
    }

    #[test]
    fn parse_schedule_hourly_ok() {
        let s = parse_schedule(None, None, None, true, None).unwrap();
        assert!(matches!(s, Schedule::Hourly));
    }

    #[test]
    fn parse_schedule_cron_ok() {
        let s = parse_schedule(None, None, None, false, Some("0 9 * * *")).unwrap();
        assert!(matches!(s, Schedule::Cron { expr } if expr == "0 9 * * *"));
    }

    #[test]
    fn parse_schedule_no_freq_errors() {
        assert!(parse_schedule(None, None, None, false, None).is_err());
    }

    #[test]
    fn format_epoch_known_date() {
        // 2026-07-31 09:00:00 UTC — rough sanity check.
        // 2026-07-31 = days since epoch: (2026-1970)*365 + leap_days + day_in_year
        // just check it doesn't panic and contains "2026"
        let s = format_epoch(1785661200);
        assert!(s.contains("2026"), "got: {}", s);
    }

    // ── Store-effect test (uses shared isolated ATOMCODE_HOME from #[ctor]) ───

    #[test]
    fn add_builds_daily_task_and_persists() {
        // Use a dedicated tempdir so this test is isolated from any other
        // schedule tests running concurrently under the same binary.
        let tmp = tempfile::tempdir().unwrap();
        // Save and restore ATOMCODE_HOME so any pre-existing value set by a
        // per-process ctor (or another test) is not permanently discarded.
        let prev = std::env::var("ATOMCODE_HOME").ok();
        std::env::set_var("ATOMCODE_HOME", tmp.path());

        let t = build_task(
            "Brief",
            "summarize",
            "/tmp/p",
            Schedule::Daily { time: "09:00".into() },
            "plan",
            "important",
        );
        atomcode_config::schedule::save(&t).unwrap();
        let all = atomcode_config::schedule::list();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Brief");

        // Restore the previous value (or remove the var if it was absent).
        match prev {
            Some(v) => std::env::set_var("ATOMCODE_HOME", v),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
    }
}
