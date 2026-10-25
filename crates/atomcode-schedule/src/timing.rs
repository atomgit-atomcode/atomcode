//! Calendar previews use the execution host's local zone, matching OS adapters.
//! They are estimates, not claims that an OS registration will actually fire.
use anyhow::{bail, Result};
use atomcode_config::schedule::{Schedule, ScheduleTask};
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NextRun {
    pub at: i64,
    pub time_zone: String,
    pub estimated: bool,
}

pub fn validate(task: &ScheduleTask) -> Result<()> {
    if task.title.trim().is_empty() || task.title.chars().count() > 120 {
        bail!("title must contain 1–120 characters");
    }
    if task.prompt.trim().is_empty() || task.prompt.len() > 65536 {
        bail!("instruction must contain 1–65536 bytes");
    }
    if !std::path::Path::new(&task.cwd).is_absolute() {
        bail!("working directory must be absolute");
    }
    if !matches!(
        task.permission_mode.as_str(),
        "plan" | "accept_edits" | "auto"
    ) {
        bail!("invalid permission mode");
    }
    if !matches!(task.notify.as_str(), "off" | "important" | "all") {
        bail!("invalid notification level");
    }
    match &task.schedule {
        Schedule::Daily { time } => {
            clock(time)?;
        }
        Schedule::Weekly { weekday, time } => {
            clock(time)?;
            if !(1..=7).contains(weekday) {
                bail!("weekday must be 1–7");
            }
        }
        Schedule::Interval { every_minutes } if *every_minutes == 0 => {
            bail!("interval must be at least one minute")
        }
        Schedule::Cron { .. } => bail!(
            "cron is not supported by the OS adapters; choose daily, weekly, hourly or an interval"
        ),
        _ => {}
    }
    Ok(())
}
fn clock(time: &str) -> Result<(u32, u32)> {
    let (h, m) = time
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected HH:MM"))?;
    let h: u32 = h.parse()?;
    let m: u32 = m.parse()?;
    if h > 23 || m > 59 {
        bail!("time must be between 00:00 and 23:59");
    }
    Ok((h, m))
}

pub fn next(task: &ScheduleTask, now: i64) -> Option<NextRun> {
    let current = Local.timestamp_opt(now, 0).single()?;
    let at = next_in(task, current)?;
    Some(NextRun {
        at,
        time_zone: format!("host local ({})", current.offset()),
        estimated: true,
    })
}
fn next_in<T: TimeZone>(task: &ScheduleTask, now: DateTime<T>) -> Option<i64> {
    match &task.schedule {
        Schedule::Interval { every_minutes } => {
            let period = i64::from(*every_minutes).checked_mul(60)?;
            if period == 0 {
                return None;
            }
            let anchor = if task.created_at > 0 {
                task.created_at
            } else {
                now.timestamp()
            };
            let count = now
                .timestamp()
                .saturating_sub(anchor)
                .div_euclid(period)
                .max(0)
                .checked_add(1)?;
            anchor.checked_add(count.checked_mul(period)?)
        }
        Schedule::Cron { .. } => None,
        _ => {
            let (h, m) = match &task.schedule {
                Schedule::Daily { time } | Schedule::Weekly { time, .. } => clock(time).ok()?,
                Schedule::Hourly => (now.hour(), 0),
                _ => return None,
            };
            let zone = now.timezone();
            for day in 0..15 {
                let date = now.date_naive().checked_add_signed(Duration::days(day))?;
                if let Schedule::Weekly { weekday, .. } = task.schedule {
                    if date.weekday().number_from_monday() != u32::from(weekday) {
                        continue;
                    }
                }
                let hours: Vec<u32> = if matches!(task.schedule, Schedule::Hourly) {
                    (0..24).collect()
                } else {
                    vec![h]
                };
                for hour in hours {
                    // A missing local time is skipped, an overlap chooses the
                    // earlier instant. Do not shift a calendar rule through DST.
                    if let Some(candidate) = zone
                        .from_local_datetime(&date.and_hms_opt(hour, m, 0)?)
                        .earliest()
                    {
                        if candidate.timestamp() > now.timestamp() {
                            return Some(candidate.timestamp());
                        }
                    }
                }
            }
            None
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    fn task(schedule: serde_json::Value) -> ScheduleTask {
        serde_json::from_value(serde_json::json!({"id":"test", "title":"Test", "prompt":"Review", "cwd":std::env::temp_dir(), "created_at":1000, "schedule":schedule})).unwrap()
    }
    #[test]
    fn weekly_obeys_weekday_and_intervals_do_not_move_when_queried() {
        let weekly = task(serde_json::json!({"kind":"weekly","weekday":1,"time":"09:00"}));
        let now = Utc.with_ymd_and_hms(2026, 10, 24, 12, 0, 0).unwrap();
        assert_eq!(
            next_in(&weekly, now),
            Some(
                Utc.with_ymd_and_hms(2026, 10, 26, 9, 0, 0)
                    .unwrap()
                    .timestamp()
            )
        );
        let interval = task(serde_json::json!({"kind":"interval","every_minutes":10}));
        assert_eq!(
            next_in(&interval, Utc.timestamp_opt(1100, 0).unwrap()),
            Some(1600)
        );
        assert_eq!(
            next_in(&interval, Utc.timestamp_opt(1200, 0).unwrap()),
            Some(1600)
        );
    }
    #[test]
    fn invalid_or_unimplemented_rules_are_rejected_before_registration() {
        assert!(validate(&task(serde_json::json!({"kind":"daily","time":"25:90"}))).is_err());
        assert!(validate(&task(serde_json::json!({"kind":"cron","expr":"* * * * *"}))).is_err());
        assert!(validate(&task(
            serde_json::json!({"kind":"interval","every_minutes":0})
        ))
        .is_err());
    }
}

#[cfg(test)]
mod dst_tests {
    use super::*;
    use chrono::Utc;
    #[test]
    fn local_calendar_preview_skips_a_dst_gap_and_chooses_only_the_earlier_overlap() {
        let zone = chrono_tz::America::New_York;
        let mut task: ScheduleTask=serde_json::from_value(serde_json::json!({"id":"dst","title":"DST","prompt":"Review","cwd":std::env::temp_dir(),"schedule":{"kind":"daily","time":"02:30"}})).unwrap();
        let before_gap = zone.with_ymd_and_hms(2026, 3, 7, 12, 0, 0).unwrap();
        assert_eq!(
            next_in(&task, before_gap),
            Some(
                zone.with_ymd_and_hms(2026, 3, 9, 2, 30, 0)
                    .unwrap()
                    .timestamp()
            )
        );
        task.schedule = Schedule::Daily {
            time: "01:30".into(),
        };
        let before_overlap = zone.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap();
        assert_eq!(
            next_in(&task, before_overlap),
            Some(
                Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0)
                    .unwrap()
                    .timestamp()
            )
        );
        let after_early = Utc
            .with_ymd_and_hms(2026, 11, 1, 5, 45, 0)
            .unwrap()
            .with_timezone(&zone);
        assert_eq!(
            next_in(&task, after_early),
            Some(
                zone.with_ymd_and_hms(2026, 11, 2, 1, 30, 0)
                    .unwrap()
                    .timestamp()
            )
        );
    }
}
