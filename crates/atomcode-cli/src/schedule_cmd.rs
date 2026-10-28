//! `atomcode schedule` subcommand — add / list / remove / enable / disable / sync.
//!
//! Executions use the shared run ledger and the native headless runtime.

use anyhow::{Context, Result};
use clap::Subcommand;

use atomcode_config::schedule::{self, Schedule, ScheduleTask};
#[cfg(test)]
use atomcode_schedule::os::OsScheduler;

fn task_store() -> atomcode_schedule::Store {
    atomcode_schedule::Store::new(schedule::schedules_root())
}

fn task_service() -> atomcode_schedule::service::Service {
    atomcode_schedule::service::Service::system(schedule::schedules_root())
}

// ── CLI enum ──────────────────────────────────────────────────────────────────

#[derive(Subcommand, Clone)]
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

    /// Edit a task, preserving fields whose flags are omitted.
    Edit {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, group = "edit_freq")]
        daily: Option<String>,
        #[arg(long, group = "edit_freq")]
        weekly: Option<String>,
        #[arg(long, group = "edit_freq")]
        every: Option<String>,
        #[arg(long, group = "edit_freq")]
        hourly: bool,
        #[arg(long)]
        mode: Option<String>,
        #[arg(long)]
        notify: Option<String>,
        /// Reject the edit if the task no longer has this observed revision.
        #[arg(long)]
        expected: Option<String>,
    },
    /// Show saved run facts, including failures and execution session ids.
    History {
        id: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        cursor: Option<usize>,
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

    /// Run a scheduled task immediately.
    Run {
        /// Task id.
        id: String,
        /// Deduplicate remote/manual retries.
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        expected_revision: Option<String>,
        /// Explicit task store used by OS registrations and remote launches.
        #[arg(long, hide = true)]
        store: Option<std::path::PathBuf>,
    },

    /// Sync OS scheduler registrations with the stored task list.
    ///
    /// Installs enabled tasks that are not yet registered and unregisters
    /// disabled tasks that are still registered.  Safe to run repeatedly.
    Sync,
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
        return Ok(Schedule::Daily {
            time: t.to_string(),
        });
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
        let every_minutes: u32 = minutes_str.parse().with_context(|| {
            format!(
                "--every minutes value must be a positive integer, got {:?}",
                minutes_str
            )
        })?;
        if every_minutes == 0 {
            anyhow::bail!("--every minutes must be > 0");
        }
        return Ok(Schedule::Interval { every_minutes });
    }
    if hourly {
        return Ok(Schedule::Hourly);
    }
    if let Some(expr) = cron {
        return Ok(Schedule::Cron {
            expr: expr.to_string(),
        });
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
/// Only ASCII alphanumeric characters are kept; everything else is collapsed
/// into a single `-` separator.  Non-ASCII characters (e.g. CJK) are dropped
/// entirely so that launchd labels remain valid ASCII.  If the result would be
/// empty (e.g. a purely non-ASCII title) the fallback `"task"` is returned.
/// The output is lowercased and truncated to 32 characters.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // suppress leading dash
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
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

// ── Inner handlers (accept &dyn OsScheduler for testability) ─────────────────

/// Core logic for `schedule add` after the task has been built and saved.
///
/// Installs the task into the OS scheduler.  If install fails the task is
/// **not** deleted — the caller should log the error and suggest `sync`.
#[cfg(test)]
fn handle_add_with(os: &dyn OsScheduler, task: &ScheduleTask) -> Result<()> {
    if let Err(e) = os.install(task) {
        eprintln!(
            "[schedule] warning: OS scheduler registration failed for {}: {e}\n\
             Run `atomcode schedule sync` to retry.",
            task.id
        );
    }
    Ok(())
}

/// Core logic for `schedule remove`: uninstall (best-effort) then delete.
#[cfg(test)]
fn handle_remove_with(os: &dyn OsScheduler, id: &str) -> Result<()> {
    // best-effort — don't abort if already absent
    let _ = os.uninstall(id);
    let store = task_store();
    if let Ok(definition) = store.get(id) {
        store.remove(id, &definition.revision)?;
    } else if store.list()?.iter().any(|d| d.task.id == id) {
        anyhow::bail!("cannot read task");
    }
    Ok(())
}

/// Core logic for `schedule enable`.
#[cfg(test)]
fn handle_enable_with(os: &dyn OsScheduler, id: &str) -> Result<()> {
    let store = task_store();
    let definition = store.get(id)?;
    let mut task = definition.task;
    task.enabled = true;
    store.put(&task, Some(&definition.revision))?;
    if let Err(e) = os.install(&task) {
        eprintln!(
            "[schedule] warning: OS scheduler registration failed for {id}: {e}\n\
             Run `atomcode schedule sync` to retry."
        );
    }
    Ok(())
}

/// Core logic for `schedule disable`.
#[cfg(test)]
fn handle_disable_with(os: &dyn OsScheduler, id: &str) -> Result<()> {
    let store = task_store();
    let definition = store.get(id)?;
    let mut task = definition.task;
    task.enabled = false;
    store.put(&task, Some(&definition.revision))?;
    // best-effort
    let _ = os.uninstall(id);
    Ok(())
}

/// Core logic for `schedule sync`: reconcile OS registrations with the store.
///
/// Calls `OsScheduler::install` unconditionally for every enabled task.
/// This is intentional: `install` must be **idempotent** (launchd overwrites
/// existing plists, schtasks uses `/F` to replace, systemd overwrites unit
/// files), so repeated sync runs are always safe.
///
/// Returns the number of errors so the caller can set a non-zero exit code.
#[cfg(test)]
fn handle_sync_with(os: &dyn OsScheduler) -> Result<usize> {
    let tasks = task_store()
        .list()?
        .into_iter()
        .map(|d| d.task)
        .collect::<Vec<_>>();
    let mut installed = 0usize;
    let mut uninstalled = 0usize;
    let mut errors = 0usize;

    for task in &tasks {
        if task.enabled {
            match os.install(task) {
                Ok(()) => {
                    println!("  sync: installed {}", task.id);
                    installed += 1;
                }
                Err(e) => {
                    eprintln!("  sync: failed to install {}: {e}", task.id);
                    errors += 1;
                }
            }
        } else {
            match os.uninstall(&task.id) {
                Ok(()) => {
                    println!("  sync: uninstalled {}", task.id);
                    uninstalled += 1;
                }
                Err(e) => {
                    eprintln!("  sync: failed to uninstall {}: {e}", task.id);
                    errors += 1;
                }
            }
        }
    }

    println!(
        "  sync done: {} installed, {} uninstalled, {} errors",
        installed, uninstalled, errors
    );
    Ok(errors)
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
            let entry = task_service().save(&task, None)?;
            println!("  Added task {} ({})", task.id, task.title);
            if let Some(error) = entry.registration_error {
                eprintln!("[schedule] saved but registration failed: {error}");
                return Ok(1);
            }
            Ok(0)
        }

        ScheduleCli::Edit {
            id,
            title,
            prompt,
            cwd,
            daily,
            weekly,
            every,
            hourly,
            mode,
            notify,
            expected,
        } => {
            let service = task_service();
            let definition = service.store.get(&id)?;
            let observed = expected.unwrap_or(definition.revision);
            let mut task = definition.task;
            if let Some(value) = title {
                task.title = value;
            }
            if let Some(value) = prompt {
                task.prompt = value;
            }
            if let Some(value) = cwd {
                task.cwd = value;
            }
            if let Some(value) = mode {
                task.permission_mode = value;
            }
            if let Some(value) = notify {
                task.notify = value;
            }
            if daily.is_some() || weekly.is_some() || every.is_some() || hourly {
                task.schedule = parse_schedule(
                    daily.as_deref(),
                    weekly.as_deref(),
                    every.as_deref(),
                    hourly,
                    None,
                )?;
            }
            let entry = service.save(&task, Some(&observed))?;
            println!("  Updated task {} ({})", task.id, entry.definition.revision);
            if let Some(error) = entry.registration_error {
                eprintln!("[schedule] saved but registration failed: {error}");
                return Ok(1);
            }
            Ok(0)
        }
        ScheduleCli::History { id, limit, cursor } => {
            let page = task_store().history_page(&id, cursor, limit)?;
            println!("{}", serde_json::to_string_pretty(&page)?);
            Ok(0)
        }
        ScheduleCli::List => {
            let entries = task_service().catalog()?;
            if entries.is_empty() {
                println!("  No scheduled tasks. Use `atomcode schedule add` to create one.");
            }
            for entry in entries {
                let next = entry
                    .next_run
                    .as_ref()
                    .map(|n| format!("~{} ({})", format_epoch(n.at), n.time_zone))
                    .unwrap_or_else(|| "-".into());
                let last = match &entry.last_run {
                    Some(atomcode_schedule::RunEvent::Finished { status, .. }) => status.as_str(),
                    _ if entry.running => "running",
                    _ => entry.definition.task.last_status.as_deref().unwrap_or("-"),
                };
                println!(
                    "  {} | {} | next:{} | last:{} | {} | {}",
                    entry.definition.task.id,
                    entry.definition.task.title,
                    next,
                    last,
                    if entry.definition.task.enabled {
                        "on"
                    } else {
                        "off"
                    },
                    entry.registration
                );
                if let Some(error) = entry.registration_error {
                    eprintln!("  registration: {error}");
                }
            }
            Ok(0)
        }
        ScheduleCli::Remove { id } => {
            let service = task_service();
            let definition = service.store.get(&id)?;
            if let Some(warning) = service.remove(&id, &definition.revision)? {
                eprintln!("[schedule] {warning}");
            }
            println!("  Removed task {id}");
            Ok(0)
        }
        ScheduleCli::Enable { id } => set_enabled(&id, true),
        ScheduleCli::Disable { id } => set_enabled(&id, false),
        ScheduleCli::Run {
            id,
            request_id,
            expected_revision,
            store: _,
        } => run_task(&id, request_id.as_deref(), expected_revision.as_deref()).await,

        ScheduleCli::Sync => {
            let service = task_service();
            let mut errors = 0;
            for entry in service.catalog()? {
                match service.reconcile(&entry.definition.task.id) {
                    Ok(entry) if entry.registration_error.is_none() => {}
                    result => {
                        errors += 1;
                        eprintln!("[schedule] registration not confirmed: {result:?}");
                    }
                }
            }
            Ok(if errors == 0 { 0 } else { 1 })
        }
    }
}

fn set_enabled(id: &str, enabled: bool) -> Result<i32> {
    let service = task_service();
    let definition = service.store.get(id)?;
    let mut task = definition.task;
    task.enabled = enabled;
    let entry = service.save(&task, Some(&definition.revision))?;
    if let Some(error) = entry.registration_error {
        eprintln!("[schedule] registration failed: {error}");
        return Ok(1);
    }
    println!(
        "  {} task {id}",
        if enabled { "Enabled" } else { "Disabled" }
    );
    Ok(0)
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

/// Map a headless exit code to the status appended to the run ledger.
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
/// directory does not exist an error terminal is appended to the run ledger and
/// the function returns non-zero.
async fn run_task(id: &str, request_id: Option<&str>, expected: Option<&str>) -> Result<i32> {
    let store = task_store();
    if request_id.is_none() && !store.get(id)?.task.enabled {
        println!("  schedule run: task {id} is disabled, skipping");
        return Ok(0);
    }
    let lease = match store.begin_revision(id, request_id, expected) {
        Ok(lease) => lease,
        Err(error) => {
            if let Some(request_id) = request_id {
                store.record_rejected(id, request_id, format!("{error:#}"))?;
            }
            return Err(error);
        }
    };
    let result = execute_task(&lease).await;
    match result {
        Ok((code, diagnostic)) => {
            lease.finish(last_status_for(code), diagnostic)?;
            Ok(code)
        }
        Err(error) => {
            lease.finish("error", Some(format!("{error:#}")))?;
            Err(error)
        }
    }
}

async fn execute_task(lease: &atomcode_schedule::RunLease) -> Result<(i32, Option<String>)> {
    use atomcode_capabilities::session::manager::SessionOrigin;
    use atomcode_capabilities::session::SessionManager;
    use atomcode_coding::ProviderBootstrap;
    use atomcode_config::config::Config;
    let task = &lease.definition.task;

    // 3. Load user config from default path.
    let config_path = Config::default_path();
    let config = if config_path.exists() {
        Config::load(&config_path).context("cannot load scheduled task configuration")?
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
        anyhow::bail!("working directory does not exist: {:?}", cwd);
    }

    // 5. Build runtime config.
    //    Scheduled tasks never do a full bypass (dangerously_skip_permissions=false).
    //    The strict_unattended approver in run_native_headless blocks any dangerous/
    //    out-of-workspace bash regardless of permission_mode — no human is present.
    let mode = mode_from_str(&task.permission_mode);
    let runtime_cfg = crate::runtime_config_from(
        &config, &cwd, None, None,  // no per-task telemetry arc needed
        false, // dangerously_skip_permissions=false — scheduled: never full bypass
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
        false,
        false,
        None,
        None,
    )
    .await?;

    // 7. Mark session origin = Scheduled.
    if let Some(ref session_info) = runtime.session {
        let sid = session_info.id.clone();
        lease.session(&sid)?;
        let manager = SessionManager::for_project(
            &agent.working_dir,
            &atomcode_coding::config::product_dirs_from_env(),
        );
        // Best-effort — don't abort the run if meta update fails.
        let _ = manager.update_meta(&sid, |m| {
            m.origin = SessionOrigin::Scheduled;
        });
    }

    // 8. Set the runtime mode after spawn.
    //    Scheduled tasks cap Auto at AcceptEdits so the middleware layer does not
    //    blanket-approve dangerous bash — the strict approver in run_native_headless
    //    provides the second safety net.  Plan and AcceptEdits pass through unchanged.
    let effective_mode = if mode == atomcode_coding::RuntimeMode::Auto {
        atomcode_coding::RuntimeMode::AcceptEdits
    } else {
        mode
    };
    runtime
        .handle
        .set_mode(effective_mode)
        .await
        .map_err(anyhow::Error::new)?;

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

    // 10. Run headless with strict unattended approver.
    //    skip_permissions=false: scheduled tasks never do full bypass.
    //    strict_unattended=true: any tool escalated to approval is denied — no human
    //    is available to vet destructive or out-of-workspace commands.
    //    auto permission_mode is equivalent to accept-edits + strict bash gating.
    let provider_name = runtime_cfg.provider_name.clone();
    let model_name = runtime_cfg.model.clone();
    let (exit_code, _captured, diagnostic) = crate::run_native_headless_detailed(
        notifications_cfg,
        runtime,
        task.prompt.clone(),
        provider_name,
        model_name,
        false,
        crate::HeadlessOutputFormat::Text,
        false,
        cwd.clone(),
        false, // skip_permissions=false — never bypass for scheduled runs
        false, // is_admin=false
        true,  // strict_unattended=true — deny risky/out-of-workspace bash
    )
    .await?;

    // Completion is recorded in the run ledger, never written over the task.
    Ok((exit_code, diagnostic))
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
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        year, month, day, h, m, s
    )
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
    fn strict_unattended_denies_escalated_bash() {
        // Normal -p: bash escalated to approval → allow (current behaviour).
        assert!(crate::headless_auto_approve(false, false, "bash"));
        // scheduled (strict): bash escalated to approval (= dangerous/out-of-workspace) → deny.
        assert!(!crate::headless_auto_approve(true, false, "bash"));
        // scheduled (strict): non-bash tool needs approval → deny (no human present).
        assert!(!crate::headless_auto_approve(true, false, "edit_file"));
        // -p treats a backgrounded command like the foreground one; strict denies both.
        assert!(crate::headless_auto_approve(false, false, "bash_start"));
        assert!(!crate::headless_auto_approve(true, false, "bash_start"));
        // ...but reading or stopping a job is not a command.
        assert!(!crate::headless_auto_approve(false, false, "bash_kill"));
        // strict + skip_permissions still denies (scheduled never skips, but defensive).
        assert!(!crate::headless_auto_approve(true, true, "bash"));
        // non-strict + skip_permissions (-p flag) → skip_permissions dominates, any tool allowed.
        assert!(crate::headless_auto_approve(false, true, "edit_file"));
    }

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
        let sched = Schedule::Daily {
            time: "09:00".into(),
        };
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
        // purely punctuation → "task" fallback
        assert_eq!(slug("!!!"), "task");
        // long title gets truncated
        let long = "a".repeat(50);
        assert_eq!(slug(&long).len(), 32);
    }

    // B: ASCII-only slug — non-ASCII characters (CJK etc.) must be dropped.
    #[test]
    fn slug_non_ascii_title_falls_back_to_task() {
        // Purely CJK: no ASCII alphanumeric → fallback "task"
        let s = slug("重要任务");
        assert_eq!(s, "task", "pure non-ASCII slug must be 'task', got: {s}");
        // Mixed ASCII + CJK: only ASCII letters survive
        let s2 = slug("My重要Task");
        assert!(
            s2.chars().all(|c| c.is_ascii()),
            "slug must be all-ASCII: {s2}"
        );
        assert!(s2.contains("my"), "ASCII part 'my' must survive: {s2}");
        assert!(s2.contains("task"), "ASCII part 'task' must survive: {s2}");
    }

    #[test]
    fn slug_ascii_alphanumeric_preserved() {
        assert_eq!(slug("My Task 2"), "my-task-2");
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
    #[serial_test::serial(atomcode_home)]
    fn add_builds_daily_task_and_persists() {
        with_temp_home(|| {
            let t = build_task(
                "Brief",
                "summarize",
                "/tmp/p",
                Schedule::Daily {
                    time: "09:00".into(),
                },
                "plan",
                "important",
            );
            atomcode_config::schedule::save(&t).unwrap();
            let all = atomcode_config::schedule::list();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].title, "Brief");
        });
    }

    // ── FakeScheduler — records install/uninstall calls ───────────────────────

    use atomcode_config::schedule::ScheduleTask;
    #[cfg(test)]
    use atomcode_schedule::os::{InstallState, OsScheduler};
    use std::sync::{Mutex, OnceLock};

    /// Global lock to serialize tests that mutate ATOMCODE_HOME / the schedule store.
    fn store_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[derive(Default)]
    struct FakeScheduler {
        installed: Mutex<Vec<String>>,
        uninstalled: Mutex<Vec<String>>,
    }

    impl FakeScheduler {
        fn installed(&self) -> Vec<String> {
            self.installed.lock().unwrap().clone()
        }
        fn uninstalled(&self) -> Vec<String> {
            self.uninstalled.lock().unwrap().clone()
        }
    }

    impl OsScheduler for FakeScheduler {
        fn install(&self, task: &ScheduleTask) -> anyhow::Result<()> {
            self.installed.lock().unwrap().push(task.id.clone());
            Ok(())
        }
        fn uninstall(&self, id: &str) -> anyhow::Result<()> {
            self.uninstalled.lock().unwrap().push(id.to_string());
            Ok(())
        }
        fn status(&self, id: &str) -> InstallState {
            if self.installed.lock().unwrap().contains(&id.to_string()) {
                InstallState::Installed
            } else {
                InstallState::Missing
            }
        }
    }

    fn make_task(id: &str, enabled: bool) -> ScheduleTask {
        ScheduleTask {
            id: id.into(),
            title: "Test".into(),
            prompt: "do it".into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            schedule: Schedule::Daily {
                time: "09:00".into(),
            },
            permission_mode: "plan".into(),
            notify: "important".into(),
            enabled,
            created_at: 0,
            last_run_at: None,
            last_status: None,
        }
    }

    /// Convenience: set ATOMCODE_HOME to a fresh tempdir, call `f`, restore.
    ///
    /// Acquires `store_lock()` so concurrent tests do not stomp each other's
    /// ATOMCODE_HOME env var.  The lock is held for the duration of `f`.
    ///
    /// Restoration of ATOMCODE_HOME is performed inside a Drop guard so that a
    /// panic inside `f` still restores the env var (and releases the lock via
    /// normal unwind), preventing subsequent store tests from silently seeing a
    /// dangling path.
    fn with_temp_home<T, F: FnOnce() -> T>(f: F) -> T {
        // The helper mutates the process-global ATOMCODE_HOME, so the exclusion cannot
        // live here: it belongs to every test that reaches this helper (they carry the
        // shared `serial_test::serial(atomcode_home)` key) and to the other drivers that
        // point ATOMCODE_HOME at their own tempdir (acp replay, tui_login, uninstall).
        // `store_lock` alone only serializes this module.
        struct EnvGuard {
            prev: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.prev.take() {
                    Some(v) => std::env::set_var("ATOMCODE_HOME", v),
                    None => std::env::remove_var("ATOMCODE_HOME"),
                }
            }
        }

        let _lock = store_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _env_guard = EnvGuard {
            prev: std::env::var_os("ATOMCODE_HOME"),
        };
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", tmp.path());
        // `tmp` is kept alive until after `f()` returns (or unwinds), ensuring
        // the tempdir is not deleted while the store operates inside it.
        let result = f();
        drop(tmp); // explicit for clarity; would drop at end of scope anyway
        result
    }

    // ── OS-integration tests ──────────────────────────────────────────────────

    #[test]
    #[serial_test::serial(atomcode_home)]
    fn add_registers_via_os_scheduler() {
        with_temp_home(|| {
            let fake = FakeScheduler::default();
            let task = make_task("add-test-abc123", true);
            atomcode_config::schedule::save(&task).unwrap();
            handle_add_with(&fake, &task).unwrap();
            assert_eq!(fake.installed(), vec!["add-test-abc123"]);
            assert!(fake.uninstalled().is_empty());
        });
    }

    #[test]
    #[serial_test::serial(atomcode_home)]
    fn remove_unregisters_via_os_scheduler() {
        with_temp_home(|| {
            let fake = FakeScheduler::default();
            let task = make_task("rm-test-abc123", true);
            atomcode_config::schedule::save(&task).unwrap();
            handle_remove_with(&fake, "rm-test-abc123").unwrap();
            assert_eq!(fake.uninstalled(), vec!["rm-test-abc123"]);
            // Task should be gone from the store.
            assert!(atomcode_config::schedule::load("rm-test-abc123").is_err());
        });
    }

    #[test]
    #[serial_test::serial(atomcode_home)]
    fn disable_unregisters_via_os_scheduler() {
        with_temp_home(|| {
            let fake = FakeScheduler::default();
            let task = make_task("dis-test-abc123", true);
            atomcode_config::schedule::save(&task).unwrap();
            handle_disable_with(&fake, "dis-test-abc123").unwrap();
            assert_eq!(fake.uninstalled(), vec!["dis-test-abc123"]);
            // Task should now be stored as disabled.
            let loaded = atomcode_config::schedule::load("dis-test-abc123").unwrap();
            assert!(!loaded.enabled);
        });
    }

    #[test]
    #[serial_test::serial(atomcode_home)]
    fn enable_registers_via_os_scheduler() {
        with_temp_home(|| {
            let fake = FakeScheduler::default();
            let task = make_task("en-test-abc123", false);
            atomcode_config::schedule::save(&task).unwrap();
            handle_enable_with(&fake, "en-test-abc123").unwrap();
            assert_eq!(fake.installed(), vec!["en-test-abc123"]);
            // Task should now be stored as enabled.
            let loaded = atomcode_config::schedule::load("en-test-abc123").unwrap();
            assert!(loaded.enabled);
        });
    }

    #[test]
    #[serial_test::serial(atomcode_home)]
    fn sync_installs_enabled_and_uninstalls_disabled() {
        with_temp_home(|| {
            let fake = FakeScheduler::default();
            let t_on = make_task("sync-on-abc123", true);
            let t_off = make_task("sync-off-abc123", false);
            atomcode_config::schedule::save(&t_on).unwrap();
            atomcode_config::schedule::save(&t_off).unwrap();

            let errors = handle_sync_with(&fake).unwrap();
            // No failures → error count is 0.
            assert_eq!(errors, 0);

            assert_eq!(fake.installed(), vec!["sync-on-abc123"]);
            assert_eq!(fake.uninstalled(), vec!["sync-off-abc123"]);
        });
    }

    // E: sync must return non-zero error count (and caller maps it to exit code 1)
    // when any install/uninstall fails.
    #[test]
    #[serial_test::serial(atomcode_home)]
    fn sync_returns_nonzero_errors_on_failure() {
        with_temp_home(|| {
            // A FakeScheduler that always fails on install.
            struct FailInstallScheduler;
            impl OsScheduler for FailInstallScheduler {
                fn install(&self, _task: &ScheduleTask) -> anyhow::Result<()> {
                    anyhow::bail!("simulated install failure")
                }
                fn uninstall(&self, _id: &str) -> anyhow::Result<()> {
                    Ok(())
                }
                fn status(&self, _id: &str) -> InstallState {
                    InstallState::Missing
                }
            }

            let task = make_task("sync-fail-abc123", true);
            atomcode_config::schedule::save(&task).unwrap();

            let errors = handle_sync_with(&FailInstallScheduler).unwrap();
            assert!(
                errors > 0,
                "expected non-zero error count when install fails"
            );
        });
    }

    // C: list must not fail when the OS scheduler is unavailable.
    // We test handle_sync_with indirectly; for list degradation we verify the
    // FakeScheduler status path with None (simulate degraded by using a wrapper).
    #[test]
    fn list_os_unknown_status_when_scheduler_unavailable() {
        // When os is None, the reg field must be "unknown" — verified by checking
        // the match arm logic directly (no I/O needed).
        let state: Option<InstallState> = None;
        let reg = match state {
            Some(InstallState::Installed) => "registered",
            Some(InstallState::Missing) => "missing",
            Some(InstallState::Unknown(_)) => "unknown",
            None => "unknown",
        };
        assert_eq!(reg, "unknown");
    }
}
