//! One management surface for CLI, TUI, daemon and injected model tools.
use crate::{
    os::{InstallState, OsScheduler},
    timing, Definition, RunEvent, Store,
};
use anyhow::Result;
use atomcode_config::schedule::ScheduleTask;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct Service {
    pub store: Store,
    os: Option<Arc<dyn OsScheduler + Send + Sync>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    #[serde(flatten)]
    pub definition: Definition,
    pub registration: String,
    pub registration_error: Option<String>,
    pub next_run: Option<timing::NextRun>,
    pub last_run: Option<RunEvent>,
    pub running: bool,
}
impl Service {
    pub fn new(store: Store, os: Option<Arc<dyn OsScheduler + Send + Sync>>) -> Self {
        Self { store, os }
    }
    pub fn system(root: impl Into<std::path::PathBuf>) -> Self {
        Self::new(Store::new(root), crate::os::current().ok().map(Arc::from))
    }
    pub fn catalog(&self) -> Result<Vec<Entry>> {
        self.store
            .list()?
            .into_iter()
            .map(|d| self.entry(d, None))
            .collect()
    }
    pub fn inspect(&self, id: &str) -> Result<Entry> {
        self.entry(self.store.get(id)?, None)
    }
    fn entry(&self, definition: Definition, registration_error: Option<String>) -> Result<Entry> {
        let id = &definition.task.id;
        let mut registration_error = registration_error.or(self.store.registration_error(id)?);
        let registration = if registration_error.is_some() {
            "error"
        } else {
            match self.os.as_ref().map(|os| os.status(id)) {
                Some(InstallState::Installed) => "registered",
                Some(InstallState::Missing) => "missing",
                Some(InstallState::Unknown(error)) => {
                    registration_error = Some(error);
                    "unknown"
                }
                None => "unknown",
            }
        }
        .to_string();
        self.store.recover_interrupted(id)?;
        let last_run = self.store.latest_event(id)?;
        let running = self.store.running(id)?;
        let next_run = definition
            .task
            .enabled
            .then(|| timing::next(&definition.task, crate::now()))
            .flatten();
        Ok(Entry {
            definition,
            registration,
            registration_error,
            next_run,
            last_run,
            running,
        })
    }
    /// Persist intent first; registration failure is visible and repairable.
    pub fn save(&self, task: &ScheduleTask, expected: Option<&str>) -> Result<Entry> {
        let _operation = self.store.operation_lock()?;
        self.save_locked(task, expected)
    }
    fn save_locked(&self, task: &ScheduleTask, expected: Option<&str>) -> Result<Entry> {
        let previous = expected.map(|_| self.store.get(&task.id)).transpose()?;
        if let (Some(previous), Some(expected)) = (&previous, expected) {
            if previous.revision != expected {
                anyhow::bail!("task changed; reload before editing");
            }
        }
        let mut normalized = task.clone();
        normalized.schedule = timing::normalized_schedule(&task.schedule)?;
        let same_rule = previous.as_ref().is_some_and(|previous| {
            timing::normalized_schedule(&previous.task.schedule)
                .ok()
                .and_then(|rule| serde_json::to_value(rule).ok())
                == serde_json::to_value(&normalized.schedule).ok()
        });
        let registration_changed = previous.as_ref().is_none_or(|previous| {
            (task.enabled && !same_rule) || previous.task.enabled != task.enabled
        });
        if let Some(previous) = &previous {
            normalized.created_at = previous.task.created_at;
            if same_rule {
                // Preserve an equivalent legacy clock spelling and its revision.
                normalized.schedule = previous.task.schedule.clone();
            }
            if matches!(
                normalized.schedule,
                atomcode_config::schedule::Schedule::Interval { .. }
            ) && (!same_rule || (!previous.task.enabled && normalized.enabled))
            {
                normalized.created_at = crate::now();
            }
        } else {
            normalized.created_at = crate::now();
        }
        let definition = self.store.put(&normalized, expected)?;
        if !registration_changed {
            // Content edits must not restart OS interval timers. A failed
            // registration remains visible until an explicit repair or rule edit.
            return self.entry(definition, None);
        }
        let error = match &self.os {
            Some(os) => if task.enabled {
                os.install_at(&normalized, &self.store.root)
            } else {
                os.uninstall(&task.id)
            }
            .err()
            .map(|e| format!("{e:#}")),
            None => Some("OS scheduler unavailable".into()),
        };
        self.store.record_registration(&task.id, &error)?;
        self.entry(definition, error)
    }
    pub fn remove(&self, id: &str, revision: &str) -> Result<Option<String>> {
        let _operation = self.store.operation_lock()?;
        self.remove_locked(id, revision)
    }
    fn remove_locked(&self, id: &str, revision: &str) -> Result<Option<String>> {
        self.store.remove(id, revision)?;
        // Saved definition is authoritative: a leftover OS trigger cannot run a
        // deleted task. Still report cleanup failure instead of false success.
        Ok(self
            .os
            .as_ref()
            .and_then(|os| os.uninstall(id).err())
            .map(|e| format!("Task definition deleted; OS registration cleanup failed: {e:#}")))
    }
    pub fn reconcile(&self, id: &str) -> Result<Entry> {
        let _operation = self.store.operation_lock()?;
        let definition = self.store.get(id)?;
        let error = match &self.os {
            Some(os) => if definition.task.enabled {
                timing::validate(&definition.task)?;
                os.install_at(&definition.task, &self.store.root)
            } else {
                os.uninstall(id)
            }
            .err()
            .map(|e| format!("{e:#}")),
            None => Some("OS scheduler unavailable".into()),
        };
        self.store.record_registration(id, &error)?;
        self.entry(definition, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct BrokenOs;
    impl OsScheduler for BrokenOs {
        fn install(&self, _: &ScheduleTask) -> Result<()> {
            anyhow::bail!("scheduler permission denied")
        }
        fn uninstall(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn status(&self, _: &str) -> InstallState {
            InstallState::Missing
        }
    }
    #[test]
    fn registration_failures_survive_reload_without_losing_the_definition() {
        let root = tempfile::tempdir().unwrap();
        let service = Service::new(Store::new(root.path()), Some(Arc::new(BrokenOs)));
        let task = serde_json::from_value(serde_json::json!({"id":"test", "title":"Test", "prompt":"Review", "cwd":std::env::temp_dir(), "schedule":{"kind":"daily","time":"09:00"}})).unwrap();
        let result = service.save(&task, None).unwrap();
        assert_eq!(result.registration, "error");
        assert!(result
            .registration_error
            .unwrap()
            .contains("permission denied"));
        let restored = Service::new(Store::new(root.path()), Some(Arc::new(BrokenOs)));
        assert_eq!(restored.catalog().unwrap()[0].registration, "error");
        assert_eq!(restored.store.get("test").unwrap().task.title, "Test");
        assert!(restored.store.history("test").unwrap().is_empty());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Mutation {
    Create {
        task: ScheduleTask,
    },
    Update {
        task: ScheduleTask,
        expected: String,
    },
    Delete {
        id: String,
        expected: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MutationResult {
    pub entry: Option<Entry>,
    pub deleted_id: Option<String>,
    pub warning: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    fingerprint: String,
    result: MutationResult,
}
impl Service {
    /// A successfully recorded operation can be retried after losing its HTTP
    /// reply, without repeating OS mutations or resurrecting a deleted task.
    pub fn apply(&self, request_id: &str, mutation: &Mutation) -> Result<MutationResult> {
        self.apply_if(request_id, mutation, || false)
    }
    pub fn apply_if(
        &self,
        request_id: &str,
        mutation: &Mutation,
        cancelled: impl Fn() -> bool,
    ) -> Result<MutationResult> {
        use std::io::Write;
        if !crate::valid_id(request_id) || request_id.len() > 128 {
            anyhow::bail!("invalid operation id");
        }
        let _operation = self.store.operation_lock()?;
        if cancelled() {
            anyhow::bail!("cancelled before automation mutation");
        }
        let dir = self.store.root.join("receipts");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{request_id}.json"));
        let fingerprint = crate::revision(&serde_json::to_vec(mutation)?);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let receipt: Receipt = serde_json::from_slice(&bytes)?;
                if receipt.fingerprint != fingerprint {
                    anyhow::bail!("operation id was already used for a different request");
                }
                return Ok(receipt.result);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let result = match mutation {
            Mutation::Create { task } => MutationResult {
                entry: Some(self.save_locked(task, None)?),
                deleted_id: None,
                warning: None,
            },
            Mutation::Update { task, expected } => MutationResult {
                entry: Some(self.save_locked(task, Some(expected))?),
                deleted_id: None,
                warning: None,
            },
            Mutation::Delete { id, expected } => {
                let warning = self.remove_locked(id, expected)?;
                MutationResult {
                    entry: None,
                    deleted_id: Some(id.clone()),
                    warning,
                }
            }
        };
        let receipt = Receipt {
            fingerprint,
            result: result.clone(),
        };
        let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
        temp.write_all(&serde_json::to_vec(&receipt)?)?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;
        Ok(result)
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    #[test]
    fn replaying_an_acknowledged_create_never_resurrects_a_later_deleted_task() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(Store::new(dir.path()), None);
        let task: ScheduleTask = serde_json::from_value(serde_json::json!({"id":"test", "title":"Test", "prompt":"Review", "cwd":std::env::temp_dir(), "schedule":{"kind":"daily","time":"09:00"}})).unwrap();
        let create = Mutation::Create { task: task.clone() };
        let created = service
            .apply("create-operation", &create)
            .unwrap()
            .entry
            .unwrap();
        service
            .apply(
                "delete-operation",
                &Mutation::Delete {
                    id: task.id.clone(),
                    expected: created.definition.revision,
                },
            )
            .unwrap();
        assert!(service
            .apply("create-operation", &create)
            .unwrap()
            .entry
            .is_some());
        assert!(service.catalog().unwrap().is_empty());
        let mut changed = task;
        changed.title = "Different".into();
        assert!(service
            .apply("create-operation", &Mutation::Create { task: changed })
            .is_err());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaunchReceipt {
    pub request_id: String,
    pub task_id: String,
    pub revision: String,
    pub state: String,
    pub error: Option<String>,
}
impl Service {
    pub fn launch(&self, id: &str, request_id: &str, expected: &str) -> Result<LaunchReceipt> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        if !crate::valid_id(request_id) || request_id.len() > 128 {
            anyhow::bail!("invalid run request id");
        }
        let _operation = self.store.operation_lock()?;
        let dir = self.store.root.join("launches");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{request_id}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => {
                let receipt: LaunchReceipt = serde_json::from_slice(&bytes)?;
                if receipt.task_id != id || receipt.revision != expected {
                    anyhow::bail!("run request id already belongs to another task");
                }
                return Ok(receipt);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let definition = self.store.get(id)?;
        if definition.revision != expected {
            anyhow::bail!("task changed; reload before running");
        }
        let task = definition.task;
        if !task.enabled {
            anyhow::bail!("task is disabled");
        }
        if self.store.running(id)? {
            anyhow::bail!("task is already running");
        }
        let binary = crate::os::execution_binary()?;
        let mut receipt = LaunchReceipt {
            request_id: request_id.into(),
            task_id: id.into(),
            revision: expected.into(),
            state: "uncertain".into(),
            error: Some(
                "launch was admitted; inspect task history before requesting another run".into(),
            ),
        };
        let save = |receipt: &LaunchReceipt| -> Result<()> {
            let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
            temp.write_all(&serde_json::to_vec(receipt)?)?;
            temp.as_file().sync_all()?;
            temp.persist(&path).map_err(|e| e.error)?;
            Ok(())
        };
        save(&receipt)?;
        match Command::new(binary)
            .args([
                "schedule",
                "run",
                id,
                "--store",
                self.store
                    .root
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("schedule path must be valid Unicode"))?,
                "--request-id",
                request_id,
                "--expected-revision",
                expected,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let store = self.store.clone();
                let task_id = id.to_string();
                let request_id = request_id.to_string();
                std::thread::spawn(move || {
                    let result = child.wait();
                    // A CLI that exited before acquiring its admission claim
                    // cannot have started model/tool work. Record that failure
                    // rather than leaving a successful spawn as false success.
                    if matches!(store.request_claimed(&task_id, &request_id), Ok(false)) {
                        let _ = store.record_rejected(
                            &task_id,
                            &request_id,
                            format!("worker exited before run initialization: {result:?}"),
                        );
                    }
                });
                receipt.state = "accepted".into();
                receipt.error = None;
            }
            Err(error) => {
                receipt.state = "error".into();
                receipt.error = Some(error.to_string());
            }
        }
        save(&receipt)?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod edit_timing_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Default)]
    struct Scheduler {
        installs: AtomicUsize,
        removals: AtomicUsize,
    }
    impl OsScheduler for Scheduler {
        fn install(&self, _: &ScheduleTask) -> Result<()> {
            self.installs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn uninstall(&self, _: &str) -> Result<()> {
            self.removals.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn status(&self, _: &str) -> InstallState {
            InstallState::Installed
        }
    }
    fn task() -> ScheduleTask {
        serde_json::from_value(serde_json::json!({"id":"edit", "title":"Review", "prompt":"Original", "cwd":std::env::temp_dir(),"created_at":1000,"schedule":{"kind":"interval","every_minutes":10}})).unwrap()
    }
    #[test]
    fn content_edits_and_noops_keep_the_timer_and_stale_noops_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = Arc::new(Scheduler::default());
        let service = Service::new(Store::new(dir.path()), Some(scheduler.clone()));
        let original = service.store.put(&task(), None).unwrap();
        let mut changed = original.task.clone();
        changed.title = "Renamed".into();
        changed.prompt = "New instruction".into();
        let saved = service.save(&changed, Some(&original.revision)).unwrap();
        assert_eq!(saved.definition.task.created_at, 1000);
        assert_eq!(scheduler.installs.load(Ordering::SeqCst), 0);
        let bytes = std::fs::read(service.store.path("edit").unwrap()).unwrap();
        let repeated = service
            .save(&saved.definition.task, Some(&saved.definition.revision))
            .unwrap();
        assert_eq!(repeated.definition.revision, saved.definition.revision);
        assert_eq!(
            bytes,
            std::fs::read(service.store.path("edit").unwrap()).unwrap()
        );
        assert!(service
            .save(&saved.definition.task, Some(&original.revision))
            .is_err());
        assert_eq!(scheduler.installs.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn editing_a_paused_task_does_not_touch_registration_and_resuming_reanchors_it() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = Arc::new(Scheduler::default());
        let service = Service::new(Store::new(dir.path()), Some(scheduler.clone()));
        let mut paused = task();
        paused.enabled = false;
        let original = service.store.put(&paused, None).unwrap();
        paused.schedule = atomcode_config::schedule::Schedule::Interval { every_minutes: 20 };
        let edited = service.save(&paused, Some(&original.revision)).unwrap();
        assert_eq!(scheduler.removals.load(Ordering::SeqCst), 0);
        assert_eq!(scheduler.installs.load(Ordering::SeqCst), 0);
        // A resumed OS interval starts afresh, including when its old anchor is stale.
        let mut resumed = edited.definition.task.clone();
        resumed.enabled = true;
        resumed.created_at = 1000;
        let resumed = service
            .save(&resumed, Some(&edited.definition.revision))
            .unwrap();
        assert!(resumed.definition.task.created_at > 1000);
        assert_eq!(scheduler.installs.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn interval_changes_reset_the_anchor_but_equivalent_clock_edits_do_not_register() {
        let dir = tempfile::tempdir().unwrap();
        let scheduler = Arc::new(Scheduler::default());
        let service = Service::new(Store::new(dir.path()), Some(scheduler.clone()));
        let original = service.store.put(&task(), None).unwrap();
        let mut changed = original.task;
        changed.schedule = atomcode_config::schedule::Schedule::Interval { every_minutes: 20 };
        let saved = service.save(&changed, Some(&original.revision)).unwrap();
        assert!(saved.definition.task.created_at > 1000);
        assert_eq!(scheduler.installs.load(Ordering::SeqCst), 1);
        let mut daily = task();
        daily.id = "daily".into();
        daily.schedule = atomcode_config::schedule::Schedule::Daily {
            time: "9:00".into(),
        };
        let original = service.store.put(&daily, None).unwrap();
        daily.schedule = atomcode_config::schedule::Schedule::Daily {
            time: "09:00".into(),
        };
        let saved = service.save(&daily, Some(&original.revision)).unwrap();
        assert_eq!(saved.definition.revision, original.revision);
        assert_eq!(scheduler.installs.load(Ordering::SeqCst), 1);
    }
}
