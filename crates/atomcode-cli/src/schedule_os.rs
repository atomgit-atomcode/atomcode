use atomcode_config::schedule::{Schedule, ScheduleTask};
use std::path::PathBuf;
use std::sync::Arc;

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

/// Run a command and bail if it exits non-zero.
fn run_checked(
    runner: &dyn CommandRunner,
    prog: &str,
    args: &[String],
) -> anyhow::Result<()> {
    let out = runner.run(prog, args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{prog} failed: {stderr}");
    }
    Ok(())
}

// ---- OsScheduler ----

pub trait OsScheduler {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()>;
    fn uninstall(&self, id: &str) -> anyhow::Result<()>;
    fn status(&self, id: &str) -> InstallState;
}

// ---- Launchd (macOS) ----

pub struct Launchd {
    pub root: PathBuf,
    pub runner: Arc<dyn CommandRunner + Send + Sync>,
}

impl Launchd {
    fn label(id: &str) -> String {
        format!("com.atomcode.schedule.{id}")
    }

    fn plist_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.plist", Self::label(id)))
    }

    fn render_plist(id: &str, exe: &str, trigger: &LaunchdTrigger) -> String {
        let label = Self::label(id);
        let trigger_xml = match trigger {
            LaunchdTrigger::Calendar { hour, minute, weekday } => {
                let mut dict = String::from("\t\t<dict>\n");
                if let Some(h) = hour {
                    dict.push_str(&format!("\t\t\t<key>Hour</key>\n\t\t\t<integer>{h}</integer>\n"));
                }
                if let Some(m) = minute {
                    dict.push_str(&format!("\t\t\t<key>Minute</key>\n\t\t\t<integer>{m}</integer>\n"));
                }
                if let Some(wd) = weekday {
                    dict.push_str(&format!("\t\t\t<key>Weekday</key>\n\t\t\t<integer>{wd}</integer>\n"));
                }
                dict.push_str("\t\t</dict>");
                format!("\t<key>StartCalendarInterval</key>\n{dict}")
            }
            LaunchdTrigger::Interval(secs) => {
                format!("\t<key>StartInterval</key>\n\t<integer>{secs}</integer>")
            }
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>schedule</string>
		<string>run</string>
		<string>{id}</string>
	</array>
{trigger_xml}
	<key>RunAtLoad</key>
	<false/>
</dict>
</plist>
"#
        )
    }
}

impl OsScheduler for Launchd {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()> {
        #[cfg(not(unix))]
        {
            anyhow::bail!("launchd is only available on macOS/Unix");
        }
        let trigger = launchd_calendar(&task.schedule)?;
        let exe = std::env::current_exe()?;
        let exe_str = exe.to_string_lossy();
        let plist_content = Self::render_plist(&task.id, &exe_str, &trigger);
        std::fs::create_dir_all(&self.root)?;
        let path = self.plist_path(&task.id);
        std::fs::write(&path, plist_content)?;
        let uid = get_uid();
        let domain = format!("gui/{uid}");
        run_checked(
            self.runner.as_ref(),
            "launchctl",
            &[
                "bootstrap".to_string(),
                domain,
                path.to_string_lossy().to_string(),
            ],
        )?;
        Ok(())
    }

    fn uninstall(&self, id: &str) -> anyhow::Result<()> {
        let uid = get_uid();
        let domain_target = format!("gui/{uid}/{}", Self::label(id));
        let _ = self.runner.run(
            "launchctl",
            &["bootout".to_string(), domain_target],
        );
        let path = self.plist_path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    fn status(&self, id: &str) -> InstallState {
        if self.plist_path(id).exists() {
            InstallState::Installed
        } else {
            InstallState::Missing
        }
    }
}

// ---- SystemdTimer (Linux) ----

pub struct SystemdTimer {
    pub root: PathBuf,
    pub runner: Arc<dyn CommandRunner + Send + Sync>,
}

impl SystemdTimer {
    fn service_name(id: &str) -> String {
        format!("atomcode-schedule-{id}.service")
    }

    fn timer_name(id: &str) -> String {
        format!("atomcode-schedule-{id}.timer")
    }

    fn service_path(&self, id: &str) -> PathBuf {
        self.root.join(Self::service_name(id))
    }

    fn timer_path(&self, id: &str) -> PathBuf {
        self.root.join(Self::timer_name(id))
    }

    fn render_service(id: &str, exe: &str) -> String {
        format!(
            "[Unit]\nDescription=Atomcode scheduled task: {id}\n\n\
             [Service]\nType=oneshot\nExecStart=\"{exe}\" schedule run {id}\n"
        )
    }

    fn render_timer(id: &str, cal: &OnCalendar) -> String {
        let on_cal = match cal {
            OnCalendar::Calendar(s) => format!("OnCalendar={s}"),
            OnCalendar::Interval(s) => format!("OnUnitActiveSec={s}"),
        };
        format!(
            "[Unit]\nDescription=Atomcode scheduler timer: {id}\n\n\
             [Timer]\n{on_cal}\nPersistent=true\n\n\
             [Install]\nWantedBy=timers.target\n"
        )
    }
}

impl OsScheduler for SystemdTimer {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()> {
        let cal = systemd_calendar(&task.schedule)?;
        let exe = std::env::current_exe()?;
        let exe_str = exe.to_string_lossy();
        std::fs::create_dir_all(&self.root)?;
        std::fs::write(self.service_path(&task.id), Self::render_service(&task.id, &exe_str))?;
        std::fs::write(self.timer_path(&task.id), Self::render_timer(&task.id, &cal))?;
        run_checked(self.runner.as_ref(), "systemctl", &[
            "--user".to_string(),
            "daemon-reload".to_string(),
        ])?;
        run_checked(self.runner.as_ref(), "systemctl", &[
            "--user".to_string(),
            "enable".to_string(),
            "--now".to_string(),
            Self::timer_name(&task.id),
        ])?;
        Ok(())
    }

    fn uninstall(&self, id: &str) -> anyhow::Result<()> {
        let _ = self.runner.run("systemctl", &[
            "--user".to_string(),
            "disable".to_string(),
            "--now".to_string(),
            Self::timer_name(id),
        ]);
        for path in [self.timer_path(id), self.service_path(id)] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let _ = self.runner.run("systemctl", &[
            "--user".to_string(),
            "daemon-reload".to_string(),
        ]);
        Ok(())
    }

    fn status(&self, id: &str) -> InstallState {
        if self.timer_path(id).exists() {
            InstallState::Installed
        } else {
            InstallState::Missing
        }
    }
}

// ---- TaskSched (Windows) ----

pub struct TaskSched {
    pub runner: Arc<dyn CommandRunner + Send + Sync>,
}

impl TaskSched {
    fn task_name(id: &str) -> String {
        format!("atomcode\\schedule\\{id}")
    }
}

impl OsScheduler for TaskSched {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()> {
        let exe = std::env::current_exe()?;
        let exe_str = exe.to_string_lossy();
        let tr = format!("\"{}\" schedule run {}", exe_str, task.id);
        let tn = Self::task_name(&task.id);
        let sched_args = schtasks_args(&task.schedule)?;
        let mut args = vec![
            "/Create".to_string(),
            "/F".to_string(),
            "/TN".to_string(),
            tn,
            "/TR".to_string(),
            tr,
        ];
        args.extend(sched_args);
        run_checked(self.runner.as_ref(), "schtasks", &args)?;
        Ok(())
    }

    fn uninstall(&self, id: &str) -> anyhow::Result<()> {
        let tn = Self::task_name(id);
        let _ = self.runner.run("schtasks", &[
            "/Delete".to_string(),
            "/F".to_string(),
            "/TN".to_string(),
            tn,
        ]);
        Ok(())
    }

    fn status(&self, id: &str) -> InstallState {
        // schtasks doesn't use files in a local root; query via runner.
        // For a reliable cross-platform test path, we rely on the runner returning
        // success to indicate the task exists. In real usage, we'd parse output.
        let tn = Self::task_name(id);
        let result = self.runner.run("schtasks", &[
            "/Query".to_string(),
            "/TN".to_string(),
            tn,
        ]);
        match result {
            Ok(out) if out.status.success() => InstallState::Installed,
            _ => InstallState::Missing,
        }
    }
}

// ---- Platform UID helper ----

fn get_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid() is always safe to call
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

// ---- current() — platform selector ----

#[cfg(target_os = "macos")]
pub fn current() -> anyhow::Result<Box<dyn OsScheduler + Send + Sync>> {
    let root = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join("Library/LaunchAgents");
    Ok(Box::new(Launchd {
        root,
        runner: Arc::new(RealCommandRunner),
    }))
}

#[cfg(target_os = "linux")]
pub fn current() -> anyhow::Result<Box<dyn OsScheduler + Send + Sync>> {
    let root = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".config/systemd/user");
    Ok(Box::new(SystemdTimer {
        root,
        runner: Arc::new(RealCommandRunner),
    }))
}

#[cfg(target_os = "windows")]
pub fn current() -> anyhow::Result<Box<dyn OsScheduler + Send + Sync>> {
    Ok(Box::new(TaskSched {
        runner: Arc::new(RealCommandRunner),
    }))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn current() -> anyhow::Result<Box<dyn OsScheduler + Send + Sync>> {
    // Fallback for other platforms — returns SystemdTimer as best-effort.
    let root = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
        .join(".config/systemd/user");
    Ok(Box::new(SystemdTimer {
        root,
        runner: Arc::new(RealCommandRunner),
    }))
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
    if wd == 0 || wd > 7 {
        anyhow::bail!("bad weekday {wd}");
    }
    Ok(["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][(wd - 1) as usize])
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
            if *weekday == 0 || *weekday > 7 {
                anyhow::bail!("bad weekday {weekday}");
            }
            let (h, m) = hhmm(time)?;
            LaunchdTrigger::Calendar {
                hour: Some(h),
                minute: Some(m),
                weekday: Some(*weekday % 7), // launchd Sunday=0; 7 % 7 == 0
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
    if wd == 0 || wd > 7 {
        anyhow::bail!("bad weekday {wd}");
    }
    Ok(["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"][(wd - 1) as usize])
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

    #[test]
    fn cron_kind_rejected_on_schtasks() {
        assert!(schtasks_args(&Schedule::Cron { expr: "0 9 * * *".into() }).is_err());
    }

    #[test]
    fn launchd_weekly_sunday_conversion() {
        // weekday=7 (Sun in 1-7 convention) must map to launchd weekday=0 (Sun)
        assert_eq!(
            launchd_calendar(&Schedule::Weekly { weekday: 7, time: "08:00".into() }).unwrap(),
            LaunchdTrigger::Calendar { hour: Some(8), minute: Some(0), weekday: Some(0) }
        );
    }

    #[test]
    fn invalid_weekday_zero_rejected() {
        assert!(dow_abbr(0).is_err());
        assert!(launchd_calendar(&Schedule::Weekly { weekday: 0, time: "08:00".into() }).is_err());
        assert!(schtasks_args(&Schedule::Weekly { weekday: 0, time: "08:00".into() }).is_err());
    }

    // ---- FakeRunner for OsScheduler end-to-end tests ----

    struct FakeRunner {
        calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
        /// If Some((prog_substr, arg_substr)), return non-zero exit for calls where
        /// the program contains prog_substr AND any arg contains arg_substr.
        fail_if: Option<(String, String)>,
    }

    impl FakeRunner {
        fn new() -> Arc<Self> {
            Arc::new(Self { calls: Default::default(), fail_if: None })
        }

        /// Return a FakeRunner that fails when program matches prog_substr AND
        /// any argument contains arg_substr.
        fn failing(prog_substr: &str, arg_substr: &str) -> Arc<Self> {
            Arc::new(Self {
                calls: Default::default(),
                fail_if: Some((prog_substr.into(), arg_substr.into())),
            })
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, p: &str, a: &[String]) -> std::io::Result<std::process::Output> {
            self.calls.lock().unwrap().push((p.into(), a.to_vec()));
            if let Some((prog_sub, arg_sub)) = &self.fail_if {
                if p.contains(prog_sub.as_str())
                    && a.iter().any(|x| x.contains(arg_sub.as_str()))
                {
                    return Ok(std::process::Output {
                        status: {
                            #[cfg(unix)]
                            { std::os::unix::process::ExitStatusExt::from_raw(1) }
                            #[cfg(not(unix))]
                            { Default::default() }
                        },
                        stdout: vec![],
                        stderr: b"simulated failure".to_vec(),
                    });
                }
            }
            Ok(std::process::Output { status: Default::default(), stdout: vec![], stderr: vec![] })
        }
    }

    fn sample_task(id: &str) -> ScheduleTask {
        atomcode_config::schedule::ScheduleTask {
            id: id.into(),
            title: "Test task".into(),
            prompt: "do something".into(),
            cwd: "/tmp".into(),
            schedule: Schedule::Daily { time: "09:30".into() },
            permission_mode: "plan".into(),
            notify: "important".into(),
            enabled: true,
            created_at: 0,
            last_run_at: None,
            last_status: None,
        }
    }

    // ---- SystemdTimer fake-runner tests ----

    #[test]
    fn systemd_install_writes_units_and_enables() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        let sched = SystemdTimer { root: tmp.path().to_path_buf(), runner: runner.clone() };
        let task = sample_task("t1");

        sched.install(&task).unwrap();

        // .service and .timer written
        assert!(tmp.path().join("atomcode-schedule-t1.service").exists());
        let timer_content = std::fs::read_to_string(
            tmp.path().join("atomcode-schedule-t1.timer")
        ).unwrap();
        assert!(timer_content.contains("OnCalendar=*-*-* 09:30:00"), "missing OnCalendar: {timer_content}");
        assert!(timer_content.contains("Persistent=true"), "missing Persistent=true: {timer_content}");

        // runner called systemctl --user enable --now
        let calls = runner.calls();
        assert!(
            calls.iter().any(|(p, a)| p == "systemctl" && a.iter().any(|x| x == "enable")),
            "no systemctl enable call found: {calls:?}"
        );
        assert!(
            calls.iter().any(|(p, a)| p == "systemctl" && a.iter().any(|x| x == "--now")),
            "no --now flag in systemctl call: {calls:?}"
        );

        // status reflects installed
        assert_eq!(sched.status("t1"), InstallState::Installed);

        // uninstall removes files
        sched.uninstall("t1").unwrap();
        assert!(!tmp.path().join("atomcode-schedule-t1.timer").exists());
        assert!(!tmp.path().join("atomcode-schedule-t1.service").exists());
        assert_eq!(sched.status("t1"), InstallState::Missing);

        // uninstall is idempotent
        sched.uninstall("t1").unwrap();
    }

    #[test]
    fn systemd_interval_uses_on_unit_active_sec() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        let sched = SystemdTimer { root: tmp.path().to_path_buf(), runner: runner.clone() };
        let mut task = sample_task("t2");
        task.schedule = Schedule::Interval { every_minutes: 15 };

        sched.install(&task).unwrap();

        let timer_content = std::fs::read_to_string(
            tmp.path().join("atomcode-schedule-t2.timer")
        ).unwrap();
        assert!(
            timer_content.contains("OnUnitActiveSec=15min"),
            "expected OnUnitActiveSec=15min, got: {timer_content}"
        );
        assert!(timer_content.contains("Persistent=true"));
    }

    #[test]
    fn systemd_service_contains_quoted_exec_start() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        let sched = SystemdTimer { root: tmp.path().to_path_buf(), runner: runner.clone() };
        let task = sample_task("t3");

        sched.install(&task).unwrap();

        let svc_content = std::fs::read_to_string(
            tmp.path().join("atomcode-schedule-t3.service")
        ).unwrap();
        // ExecStart must quote the exe path
        assert!(svc_content.contains("ExecStart=\""), "ExecStart must open with quote: {svc_content}");
        assert!(svc_content.contains("schedule run t3"), "no 'schedule run t3' in service: {svc_content}");
        assert!(svc_content.contains("Type=oneshot"), "no Type=oneshot in service: {svc_content}");
    }

    /// #3: install must propagate non-zero exit from systemctl enable as an error.
    #[test]
    fn systemd_install_fails_when_enable_returns_nonzero() {
        let tmp = tempfile::tempdir().unwrap();
        // Fail when systemctl is called with "enable" as an argument
        let runner = FakeRunner::failing("systemctl", "enable");
        let sched = SystemdTimer { root: tmp.path().to_path_buf(), runner };
        let task = sample_task("t_fail");

        let result = sched.install(&task);
        assert!(result.is_err(), "install should fail when systemctl enable returns non-zero");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("systemctl"), "error should mention systemctl: {msg}");
    }

    // ---- Launchd fake-runner tests ----

    #[test]
    fn launchd_install_writes_plist_and_bootstraps() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        let sched = Launchd { root: tmp.path().to_path_buf(), runner: runner.clone() };
        let task = sample_task("lt1");

        sched.install(&task).unwrap();

        // plist file written with the right label
        let plist_path = tmp.path().join("com.atomcode.schedule.lt1.plist");
        assert!(plist_path.exists(), "plist not written at {}", plist_path.display());
        let plist_content = std::fs::read_to_string(&plist_path).unwrap();
        assert!(plist_content.contains("com.atomcode.schedule.lt1"), "label missing");
        assert!(plist_content.contains("schedule"), "ProgramArguments missing schedule");
        assert!(plist_content.contains("StartCalendarInterval"), "trigger missing");
        // #7: plist key/integer must be on separate lines
        assert!(
            !plist_content.contains("<key>Hour</key><integer>"),
            "Hour key and integer must be on separate lines: {plist_content}"
        );

        // launchctl bootstrap called with gui/<uid> domain (#4)
        let calls = runner.calls();
        let bootstrap_call = calls.iter().find(|(p, a)| {
            p == "launchctl" && a.iter().any(|x| x == "bootstrap")
        });
        assert!(bootstrap_call.is_some(), "no launchctl bootstrap call: {calls:?}");
        let (_, boot_args) = bootstrap_call.unwrap();
        assert!(
            boot_args.iter().any(|x| x.starts_with("gui/")),
            "bootstrap domain must be gui/<uid>: {boot_args:?}"
        );
        // plist path must appear in the bootstrap args (#4)
        assert!(
            boot_args.iter().any(|x| x.ends_with(".plist")),
            "bootstrap args must include plist path: {boot_args:?}"
        );

        assert_eq!(sched.status("lt1"), InstallState::Installed);

        sched.uninstall("lt1").unwrap();
        assert!(!plist_path.exists());
        assert_eq!(sched.status("lt1"), InstallState::Missing);

        // idempotent second uninstall
        sched.uninstall("lt1").unwrap();
    }

    #[test]
    fn launchd_interval_uses_start_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new();
        let sched = Launchd { root: tmp.path().to_path_buf(), runner: runner.clone() };
        let mut task = sample_task("lt2");
        task.schedule = Schedule::Interval { every_minutes: 10 };

        sched.install(&task).unwrap();

        let plist_content = std::fs::read_to_string(
            tmp.path().join("com.atomcode.schedule.lt2.plist")
        ).unwrap();
        // 10 minutes = 600 seconds
        assert!(plist_content.contains("StartInterval"), "no StartInterval: {plist_content}");
        assert!(plist_content.contains("600"), "wrong interval seconds: {plist_content}");
    }

    // ---- TaskSched fake-runner tests ----

    #[test]
    fn tasksched_install_calls_schtasks_create() {
        let runner = FakeRunner::new();
        let sched = TaskSched { runner: runner.clone() };
        let task = sample_task("ws1");

        sched.install(&task).unwrap();

        let calls = runner.calls();
        let create_call = calls.iter().find(|(p, a)| {
            p == "schtasks" && a.iter().any(|x| x == "/Create")
        });
        assert!(create_call.is_some(), "no schtasks /Create call: {calls:?}");

        let (_, args) = create_call.unwrap();
        assert!(args.iter().any(|x| x.contains("ws1")), "task id missing: {args:?}");
        assert!(args.iter().any(|x| x == "DAILY"), "DAILY flag missing: {args:?}");
    }

    #[test]
    fn tasksched_uninstall_calls_delete_with_task_name() {
        let runner = FakeRunner::new();
        let sched = TaskSched { runner: runner.clone() };

        sched.uninstall("ws1").unwrap();

        let calls = runner.calls();
        let delete_call = calls.iter().find(|(p, a)| {
            p == "schtasks" && a.iter().any(|x| x == "/Delete")
        });
        assert!(delete_call.is_some(), "no schtasks /Delete call: {calls:?}");
        let (_, del_args) = delete_call.unwrap();
        // #5: assert /TN is present and task name contains the id
        assert!(
            del_args.iter().any(|x| x == "/TN"),
            "/TN flag missing in delete call: {del_args:?}"
        );
        assert!(
            del_args.iter().any(|x| x.contains("ws1")),
            "task name containing 'ws1' missing in delete call: {del_args:?}"
        );
    }

    /// #3: install must propagate non-zero exit from schtasks /Create as an error.
    #[test]
    fn tasksched_install_fails_when_schtasks_returns_nonzero() {
        // Fail when schtasks is called with "/Create" as an argument
        let runner = FakeRunner::failing("schtasks", "/Create");
        let sched = TaskSched { runner };
        let task = sample_task("ws_fail");

        let result = sched.install(&task);
        assert!(result.is_err(), "install should fail when schtasks /Create returns non-zero");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("schtasks"), "error should mention schtasks: {msg}");
    }
}
