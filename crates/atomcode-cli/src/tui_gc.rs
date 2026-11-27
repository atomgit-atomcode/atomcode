//! TUI entry point for previewing and explicitly cleaning up old sessions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use atomcode_capabilities::session::manager::{SessionGcCandidate, SessionOrigin};
use atomcode_capabilities::session::{CatalogPresence, SessionManager};
use atomcode_i18n::screen::{t, Msg};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
use atomcode_tui::plugin::CommandsSvc;
use serde_json::Value;
use tokio::sync::Mutex;

const ROW: &str = "tui-session-gc";
const MIN_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const KEEP_GLOBAL: usize = 20;
const KEEP_PER_PROJECT: usize = 10;
const LOW_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(crate) fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

pub(crate) fn low_space_notice(sessions_root: &Path) -> Option<String> {
    let directory = sessions_root.ancestors().find(|path| path.is_dir())?;
    let remaining = fs2::available_space(directory).ok()?;
    (remaining < LOW_DISK_BYTES).then(|| {
        t(Msg::GcLowSpace {
            free: &size(remaining),
        })
        .into_owned()
    })
}

pub(crate) struct GcRow {
    pub sessions_root: PathBuf,
}

struct GcCommands {
    sessions_root: PathBuf,
    plan: Mutex<Option<Preview>>,
}

#[async_trait]
impl Plugin for GcRow {
    fn name(&self) -> &'static str {
        ROW
    }

    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }

    fn description(&self) -> &'static str {
        "preview and explicitly delete old session history"
    }

    async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
        ctx.require::<CommandsSvc>()
            .map_err(|error| error.to_string())?
            .add(Arc::new(GcCommands {
                sessions_root: self.sessions_root.clone(),
                plan: Mutex::new(None),
            }))
    }
}

#[async_trait]
impl CommandSet for GcCommands {
    fn id(&self) -> &'static str {
        ROW
    }

    fn commands(&self) -> Vec<Command> {
        vec![Command::said_taking(
            "gc",
            "[--apply]".into(),
            t(Msg::GcAbout),
        )]
    }

    async fn run(&self, _: &str, args: &str, _: &Context) -> Outcome {
        let Ok(mut plan) = self.plan.try_lock() else {
            return Outcome::Refused(t(Msg::GcBusy).into_owned());
        };
        match args.trim() {
            "" => {
                *plan = None;
                let root = self.sessions_root.clone();
                match tokio::task::spawn_blocking(move || preview(&root, SystemTime::now())).await {
                    Ok(preview) => {
                        let text = preview.summary();
                        *plan = Some(preview);
                        Outcome::Said(text)
                    }
                    Err(error) => failure(&error.to_string()),
                }
            }
            "--apply" => {
                let Some(preview) = plan.take() else {
                    return Outcome::Refused(t(Msg::GcPreviewFirst).into_owned());
                };
                match tokio::task::spawn_blocking(move || preview.apply()).await {
                    Ok(report) => Outcome::Said(report.summary()),
                    Err(error) => failure(&error.to_string()),
                }
            }
            _ => Outcome::Refused(t(Msg::GcUsage).into_owned()),
        }
    }
}

fn failure(error: &str) -> Outcome {
    Outcome::Refused(t(Msg::GcFailed { error }).into_owned())
}

#[derive(Default)]
struct Preview {
    used: u64,
    estimated: u64,
    skipped: usize,
    candidates: Vec<(PathBuf, SessionGcCandidate)>,
}

fn preview(root: &Path, now: SystemTime) -> Preview {
    let mut report = Preview {
        used: SessionManager::history_bytes(root),
        ..Preview::default()
    };
    let Some(cutoff) = now.checked_sub(MIN_AGE) else {
        return report;
    };
    let Ok(age) = cutoff.duration_since(UNIX_EPOCH) else {
        return report;
    };
    let cutoff_ms = i64::try_from(age.as_millis()).unwrap_or(i64::MAX);
    let mut entries = SessionManager::scan_catalog(root).entries;
    entries.sort_unstable_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.project_bucket.cmp(&b.project_bucket))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut per_project = HashMap::new();
    let mut eligible = std::collections::BTreeMap::<_, Vec<_>>::new();
    for (index, entry) in entries.into_iter().enumerate() {
        let kept = per_project.entry(entry.project_bucket.clone()).or_insert(0);
        *kept += 1;
        if index < KEEP_GLOBAL
            || *kept <= KEEP_PER_PROJECT
            || entry.updated_at_ms >= cutoff_ms
            || entry.presence != CatalogPresence::NativeOnly
            || entry.needs_newer_version
            || entry.origin != SessionOrigin::Manual
        {
            continue;
        }
        eligible
            .entry(entry.project_bucket)
            .or_default()
            .push((entry.id, entry.updated_at_ms));
    }
    for (bucket, sessions) in eligible {
        let bucket = root.join(bucket);
        for candidate in SessionManager::with_root(&bucket).preview_gc(&sessions, cutoff) {
            match candidate {
                Ok(Some(candidate)) => {
                    report.estimated = report.estimated.saturating_add(candidate.bytes);
                    report.candidates.push((bucket.clone(), candidate));
                }
                Ok(None) => report.skipped += 1,
                Err(error) => {
                    tracing::warn!(%error, "could not inspect session for gc");
                    report.skipped += 1;
                }
            }
        }
    }
    report
}

impl Preview {
    fn summary(&self) -> String {
        let summary = t(Msg::GcPreview {
            used: &size(self.used),
            count: self.candidates.len(),
            estimated: &size(self.estimated),
            skipped: self.skipped,
        });
        if self.candidates.is_empty() {
            summary.into_owned()
        } else {
            format!("{summary}\n{}", t(Msg::GcApplyHint))
        }
    }

    fn apply(self) -> ApplyReport {
        let mut report = ApplyReport::default();
        for (bucket, candidate) in self.candidates {
            match SessionManager::with_root(bucket).apply_gc(&candidate) {
                Ok(Some(bytes)) => {
                    report.deleted += 1;
                    report.bytes = report.bytes.saturating_add(bytes);
                }
                Ok(None) => report.skipped += 1,
                Err(error) => {
                    tracing::warn!(%error, "session gc could not finish deleting session");
                    report.failed += 1;
                }
            }
        }
        report
    }
}

#[derive(Default)]
struct ApplyReport {
    deleted: usize,
    skipped: usize,
    failed: usize,
    bytes: u64,
}

impl ApplyReport {
    fn summary(&self) -> String {
        t(Msg::GcApplied {
            deleted: self.deleted,
            skipped: self.skipped,
            failed: self.failed,
            removed: &size(self.bytes),
        })
        .into_owned()
    }
}

fn size(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::session::{SessionMeta, StorageOwner};
    use atomcode_kernel::session::SessionHeader;
    use std::fs::{File, FileTimes};

    #[test]
    fn gc_preview_keeps_recent_per_project_and_busy_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let old = now - MIN_AGE - Duration::from_secs(60);
        let old_ms = i64::try_from(old.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap();
        let store = SessionManager::with_root(dir.path().join("0123456789abcdef"));
        let other = SessionManager::with_root(dir.path().join("abcdef0123456789"));
        let create = |store: &SessionManager, id: &str, updated_at: i64| {
            let lease = store.acquire_lease(id).unwrap();
            let mut meta = SessionMeta::new(id, "/workspace", updated_at);
            meta.owner = StorageOwner::Native;
            store
                .create_event_session(&lease, &SessionHeader::new(id), &meta)
                .unwrap();
            File::options()
                .write(true)
                .open(store.events_path(id).unwrap())
                .unwrap()
                .set_times(FileTimes::new().set_modified(old))
                .unwrap();
        };
        for index in 0..KEEP_GLOBAL + 2 {
            create(&store, &format!("session-{index}"), old_ms - index as i64);
        }
        create(&other, "other", old_ms - 100);
        let busy_id = format!("session-{}", KEEP_GLOBAL + 1);
        let candidate_id = format!("session-{KEEP_GLOBAL}");
        let busy = store.acquire_lease(&busy_id).unwrap();
        let plan = preview(dir.path(), now);
        assert_eq!(plan.skipped, 1);
        assert_eq!(plan.candidates.len(), 1);
        assert!(plan.estimated > 0);
        assert!(store.is_event_session(&candidate_id));
        let report = plan.apply();
        assert_eq!(report.deleted, 1);
        assert_eq!(report.failed, 0);
        assert!(!store.is_event_session(&candidate_id));
        assert!(store.is_event_session("session-0"));
        assert!(other.is_event_session("other"));
        assert!(store.is_event_session(busy.id()));
    }
}
