use atomcode_config::schedule::{Schedule, ScheduleTask};

// ---- InstallState ----

#[derive(Debug, PartialEq, Eq)]
pub enum InstallState {
    Installed,
    Missing,
}

// ---- CommandRunner ----

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<std::process::Output>;
}

pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<std::process::Output> {
        std::process::Command::new(program).args(args).output()
    }
}

// ---- OsScheduler ----

pub trait OsScheduler {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()>;
    fn uninstall(&self, id: &str) -> anyhow::Result<()>;
    fn status(&self, id: &str) -> InstallState;
}

// ---- Shared helpers ----

fn hhmm(s: &str) -> anyhow::Result<(u8, u8)> {
    let (h, m) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("bad time {s}"))?;
    Ok((h.parse()?, m.parse()?))
}

// ---- systemd ----

#[derive(Debug, PartialEq, Eq)]
pub enum OnCalendar {
    Calendar(String),
    Interval(String),
}

pub fn systemd_calendar(s: &Schedule) -> anyhow::Result<OnCalendar> {
    Ok(match s {
        Schedule::Daily { time } => {
            let (h, m) = hhmm(time)?;
            OnCalendar::Calendar(format!("*-*-* {h:02}:{m:02}:00"))
        }
        Schedule::Weekly { weekday, time } => {
            let (h, m) = hhmm(time)?;
            OnCalendar::Calendar(format!("{} *-*-* {h:02}:{m:02}:00", dow_abbr(*weekday)?))
        }
        Schedule::Hourly => OnCalendar::Calendar("hourly".into()),
        Schedule::Interval { every_minutes } => OnCalendar::Interval(format!("{every_minutes}min")),
        Schedule::Cron { expr } => OnCalendar::Calendar(expr.clone()),
    })
}

fn dow_abbr(wd: u8) -> anyhow::Result<&'static str> {
    Ok(["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        .get((wd.max(1) - 1) as usize)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("bad weekday {wd}"))?)
}

// ---- launchd ----

#[derive(Debug, PartialEq, Eq)]
pub enum LaunchdTrigger {
    Calendar {
        hour: Option<u8>,
        minute: Option<u8>,
        weekday: Option<u8>,
    },
    Interval(u64),
}

pub fn launchd_calendar(s: &Schedule) -> anyhow::Result<LaunchdTrigger> {
    Ok(match s {
        Schedule::Daily { time } => {
            let (h, m) = hhmm(time)?;
            LaunchdTrigger::Calendar { hour: Some(h), minute: Some(m), weekday: None }
        }
        Schedule::Weekly { weekday, time } => {
            let (h, m) = hhmm(time)?;
            LaunchdTrigger::Calendar {
                hour: Some(h),
                minute: Some(m),
                weekday: Some(*weekday % 7), // launchd Sunday=0
            }
        }
        Schedule::Hourly => LaunchdTrigger::Calendar { hour: None, minute: Some(0), weekday: None },
        Schedule::Interval { every_minutes } => LaunchdTrigger::Interval(*every_minutes as u64 * 60),
        Schedule::Cron { .. } => {
            anyhow::bail!("cron schedules are not supported on macOS launchd; use a simple frequency")
        }
    })
}

// ---- schtasks ----

pub fn schtasks_args(s: &Schedule) -> anyhow::Result<Vec<String>> {
    let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    Ok(match s {
        Schedule::Daily { time } => {
            let (h, m) = hhmm(time)?;
            v(&["/SC", "DAILY", "/ST"])
                .into_iter()
                .chain([format!("{h:02}:{m:02}")])
                .collect()
        }
        Schedule::Weekly { weekday, time } => {
            let (h, m) = hhmm(time)?;
            v(&["/SC", "WEEKLY", "/D"])
                .into_iter()
                .chain([schtasks_dow(*weekday)?.into(), "/ST".into(), format!("{h:02}:{m:02}")])
                .collect()
        }
        Schedule::Hourly => v(&["/SC", "HOURLY"]),
        Schedule::Interval { every_minutes } => v(&["/SC", "MINUTE", "/MO"])
            .into_iter()
            .chain([every_minutes.to_string()])
            .collect(),
        Schedule::Cron { .. } => {
            anyhow::bail!(
                "cron schedules are not supported on Windows Task Scheduler; use a simple frequency"
            )
        }
    })
}

fn schtasks_dow(wd: u8) -> anyhow::Result<&'static str> {
    Ok(["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"]
        .get((wd.max(1) - 1) as usize)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("bad weekday {wd}"))?)
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_config::schedule::Schedule;

    #[test]
    fn systemd_oncalendar_translation() {
        assert_eq!(
            systemd_calendar(&Schedule::Daily { time: "09:30".into() }).unwrap(),
            OnCalendar::Calendar("*-*-* 09:30:00".into())
        );
        assert_eq!(
            systemd_calendar(&Schedule::Hourly).unwrap(),
            OnCalendar::Calendar("hourly".into())
        );
        assert_eq!(
            systemd_calendar(&Schedule::Interval { every_minutes: 30 }).unwrap(),
            OnCalendar::Interval("30min".into())
        );
        assert_eq!(
            systemd_calendar(&Schedule::Weekly { weekday: 1, time: "16:00".into() }).unwrap(),
            OnCalendar::Calendar("Mon *-*-* 16:00:00".into())
        );
    }

    #[test]
    fn launchd_calendar_translation() {
        // Daily 09:30 → {Hour:9, Minute:30}
        let d = launchd_calendar(&Schedule::Daily { time: "09:30".into() }).unwrap();
        assert_eq!(d, LaunchdTrigger::Calendar { hour: Some(9), minute: Some(30), weekday: None });
        // Interval 30min → StartInterval 1800
        assert_eq!(
            launchd_calendar(&Schedule::Interval { every_minutes: 30 }).unwrap(),
            LaunchdTrigger::Interval(1800)
        );
    }

    #[test]
    fn schtasks_args_translation() {
        let a = schtasks_args(&Schedule::Daily { time: "09:30".into() }).unwrap();
        assert!(
            a.contains(&"/SC".to_string())
                && a.contains(&"DAILY".to_string())
                && a.windows(2).any(|w| w[0] == "/ST" && w[1] == "09:30")
        );
    }

    #[test]
    fn cron_kind_rejected_on_launchd() {
        assert!(launchd_calendar(&Schedule::Cron { expr: "0 9 * * *".into() }).is_err());
    }
}
