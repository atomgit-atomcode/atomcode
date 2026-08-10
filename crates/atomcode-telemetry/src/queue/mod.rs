//! Append-only NDJSON segment queue on disk.

pub mod roll;

use crate::event::Record;
use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const READY_EXT: &str = "ndjson";
const PARTIAL_EXT: &str = "partial";
const INVALID_EXT: &str = "invalid";
// Suffix appended to an active `.partial` to mark it as written by a lock-aware
// AtomCode version (`X.partial` -> `X.partial.owner`).
const MARKER_SUFFIX: &str = ".owner";
const SENDING_MARKER: &str = ".sending-";
// Old AtomCode versions did not lock active partials. Requiring a full day
// without a filesystem modification keeps cross-version recovery conservative.
const PARTIAL_QUIET_AFTER: chrono::Duration = chrono::Duration::days(1);
const RAW_RETENTION: chrono::Duration = chrono::Duration::days(90);
const CLAIM_STALE_AFTER: chrono::Duration = chrono::Duration::minutes(1);

pub struct Queue {
    dir: PathBuf,
    current: Option<Segment>,
    /// Cumulative dropped count (in-memory or on-disk FIFO eviction).
    pub dropped: u64,
}

pub struct Segment {
    pub path: PathBuf,
    ready_path: PathBuf,
    marker_path: PathBuf,
    writer: BufWriter<File>,
    events: u32,
    bytes: u64,
}

impl Segment {
    fn new(path: PathBuf, ready_path: PathBuf) -> Result<Self> {
        let marker_path = managed_marker_path(&path);
        let f = OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("creating segment {}", path.display()))?;
        f.lock_exclusive()
            .with_context(|| format!("locking active segment {}", path.display()))?;
        if let Err(error) = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&marker_path)
        {
            drop(f);
            let _ = fs::remove_file(&path);
            return Err(error)
                .with_context(|| format!("creating segment marker {}", marker_path.display()));
        }
        Ok(Self {
            path,
            ready_path,
            marker_path,
            writer: BufWriter::new(f),
            events: 0,
            bytes: 0,
        })
    }

    fn append(&mut self, r: &Record) -> Result<()> {
        let line = serde_json::to_string(r)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.events += 1;
        self.bytes += line.len() as u64 + 1;
        Ok(())
    }

    fn fsync(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }

    fn finish(mut self) -> Result<PathBuf> {
        self.fsync()?;
        let partial_path = self.path.clone();
        let ready_path = self.ready_path.clone();
        let marker_path = self.marker_path.clone();
        drop(self.writer);
        fs::rename(&partial_path, &ready_path).with_context(|| {
            format!(
                "rolling segment {} -> {}",
                partial_path.display(),
                ready_path.display()
            )
        })?;
        let _ = fs::remove_file(marker_path);
        Ok(ready_path)
    }
}

impl Queue {
    pub fn open(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;

        // Recover from previous crash / kill:
        //   1. .sending-* files are claimed segments whose HTTP POST never
        //      completed (process died mid-send).  Rename them back to
        //      .ndjson so the new process can retry.
        //   2. Stale, valid .partial files are interrupted active segments.
        //      Promote them to .ndjson without rewriting their original ts.
        //   3. Empty .partial files are segments created but never written.
        recover_stale_files_at(&dir, chrono::Utc::now(), false)?;

        Ok(Self {
            dir,
            current: None,
            dropped: 0,
        })
    }

    /// Append and return true if a roll happened (current segment was closed).
    pub fn append(&mut self, r: &Record) -> Result<bool> {
        if self.current.is_none() {
            self.start_new_segment()?;
        }
        let seg = self.current.as_mut().unwrap();
        seg.append(r)?;

        if roll::should_roll(seg.events, seg.bytes) {
            self.roll()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Force roll even if segment isn't full (used on tick flush).
    /// Returns `Ok(None)` if nothing to roll.
    pub fn force_roll(&mut self) -> Result<Option<PathBuf>> {
        if let Some(seg) = self.current.take() {
            if seg.events == 0 {
                // empty: drop the file
                let path = seg.path.clone();
                let marker_path = seg.marker_path.clone();
                drop(seg.writer);
                let _ = fs::remove_file(path);
                let _ = fs::remove_file(marker_path);
                return Ok(None);
            }
            let p = seg.finish()?;
            self.enforce_cap()?;
            return Ok(Some(p));
        }
        Ok(None)
    }

    /// Closed, immutable segments ready to be sent.
    pub fn ready_segments_sorted(&self) -> Result<Vec<PathBuf>> {
        self.segments_with_extension(READY_EXT)
    }

    pub fn segments_sorted(&self) -> Result<Vec<PathBuf>> {
        self.ready_segments_sorted()
    }

    pub fn claim_oldest_segment(&self) -> Result<Option<PathBuf>> {
        for ready in self.ready_segments_sorted()? {
            let Some(name) = ready.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let claimed = ready.with_file_name(format!(
                "{}{}{}-{}",
                name,
                SENDING_MARKER,
                std::process::id(),
                Uuid::new_v4()
            ));
            match fs::rename(&ready, &claimed) {
                Ok(()) => {
                    if let Err(error) = filetime::set_file_mtime(
                        &claimed,
                        filetime::FileTime::from_system_time(std::time::SystemTime::now()),
                    ) {
                        let _ = fs::rename(&claimed, &ready);
                        return Err(error).with_context(|| {
                            format!("marking claimed segment {} active", claimed.display())
                        });
                    }
                    return Ok(Some(claimed));
                }
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "claiming segment {} -> {}",
                            ready.display(),
                            claimed.display()
                        )
                    });
                }
            }
        }
        Ok(None)
    }

    pub fn complete_claim(&self, path: &Path) -> Result<()> {
        self.delete(path)
    }

    pub fn restore_claim(&self, path: &Path) -> Result<Option<PathBuf>> {
        if !path.exists() {
            return Ok(None);
        }
        let Some(ready) = ready_path_for_claim(path) else {
            return Ok(None);
        };
        fs::rename(path, &ready)
            .with_context(|| format!("restoring claimed segment {}", path.display()))?;
        Ok(Some(ready))
    }

    pub fn restore_claims_for_current_process(&self) -> Result<usize> {
        let marker = format!("{}{}-", SENDING_MARKER, std::process::id());
        let claims: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(&marker))
            })
            .collect();
        let mut restored = 0;
        for claim in claims {
            if self.restore_claim(&claim)?.is_some() {
                restored += 1;
            }
        }
        Ok(restored)
    }

    /// Remove every inactive local telemetry artifact. Active partials are
    /// skipped when their advisory lock cannot be acquired.
    pub fn clear_inactive(&self) -> Result<ClearStats> {
        let mut stats = ClearStats::default();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let extension = path.extension().and_then(|ext| ext.to_str());
            match extension {
                Some(READY_EXT) | Some(INVALID_EXT) => {
                    fs::remove_file(&path)?;
                    stats.removed += 1;
                }
                Some(PARTIAL_EXT) => {
                    let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
                        stats.skipped += 1;
                        continue;
                    };
                    if file.try_lock_exclusive().is_err() {
                        stats.skipped += 1;
                        continue;
                    }
                    drop(file);
                    match fs::remove_file(&path) {
                        Ok(()) => {
                            let _ = fs::remove_file(managed_marker_path(&path));
                            stats.removed += 1;
                        }
                        Err(error) if error.kind() == ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                _ => {
                    if path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.contains(SENDING_MARKER))
                    {
                        stats.skipped += 1;
                    }
                }
            }
        }
        Ok(stats)
    }

    /// Explicitly recover legacy partials created before lock markers existed.
    /// Callers should ensure older AtomCode processes are stopped first.
    pub fn recover_legacy_partials(&self) -> Result<usize> {
        let before = self.ready_segments_sorted()?.len();
        recover_stale_files_at(&self.dir, chrono::Utc::now(), true)?;
        Ok(self.ready_segments_sorted()?.len().saturating_sub(before))
    }

    fn segments_with_extension(&self, ext: &str) -> Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(ext))
            .collect();
        v.sort();
        Ok(v)
    }

    pub fn delete(&self, path: &Path) -> Result<()> {
        fs::remove_file(path)?;
        Ok(())
    }

    pub fn stats(&self) -> Result<QueueStats> {
        let segs = self.segments_sorted()?;
        let mut partials = self.segments_with_extension(PARTIAL_EXT)?;
        partials.extend(self.segments_with_extension(INVALID_EXT)?);
        let mut total_bytes = 0u64;
        let mut total_events = 0u64;
        for p in &segs {
            let meta = fs::metadata(p)?;
            total_bytes += meta.len();
            total_events += count_non_empty_lines(p).unwrap_or_default();
        }
        Ok(QueueStats {
            segment_count: segs.len(),
            total_bytes,
            total_events,
            oldest: segs.first().cloned(),
            stranded_partial_count: partials.len(),
            stranded_partial_bytes: partials
                .iter()
                .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
                .sum(),
        })
    }

    fn start_new_segment(&mut self) -> Result<()> {
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let id = Uuid::new_v4();
        let path = self.dir.join(format!("{}-{}.{}", ts, id, PARTIAL_EXT));
        let ready_path = self.dir.join(format!("{}-{}.{}", ts, id, READY_EXT));
        self.current = Some(Segment::new(path, ready_path)?);
        Ok(())
    }

    fn roll(&mut self) -> Result<()> {
        self.force_roll()?;
        Ok(())
    }

    /// Delete oldest segments if over cap; bumps `dropped` by lines evicted.
    fn enforce_cap(&mut self) -> Result<()> {
        loop {
            let segs = self.segments_sorted()?;
            let total_bytes: u64 = segs
                .iter()
                .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
                .sum();
            if !roll::over_cap(segs.len(), total_bytes) {
                break;
            }
            if let Some(oldest) = segs.first() {
                self.dropped += count_non_empty_lines(oldest).unwrap_or_default();
                fs::remove_file(oldest)?;
            } else {
                break;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct QueueStats {
    pub segment_count: usize,
    pub total_bytes: u64,
    pub total_events: u64,
    pub oldest: Option<PathBuf>,
    pub stranded_partial_count: usize,
    pub stranded_partial_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClearStats {
    pub removed: usize,
    pub skipped: usize,
}

fn ready_path_for_claim(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let marker_start = name.rfind(SENDING_MARKER)?;
    let ready_name = &name[..marker_start];
    Some(path.with_file_name(ready_name))
}

fn count_non_empty_lines(path: &Path) -> Result<u64> {
    let file = File::open(path).with_context(|| format!("opening segment {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut count = 0u64;
    for line in reader.lines() {
        if !line?.is_empty() {
            count += 1;
        }
    }
    Ok(count)
}

/// Scan the queue directory for stale artifacts left by a previous process
/// that exited before completing its send or cleanup:
///
/// - `.sending-*` files → rename back to `.ndjson` so they can be re-sent.
/// - Stale, unlocked, valid `.partial` files within raw retention → `.ndjson`.
/// - Empty `.partial` files → delete (they contain no events).
fn recover_stale_files_at(
    dir: &Path,
    now: chrono::DateTime<chrono::Utc>,
    recover_legacy: bool,
) -> Result<()> {
    let entries: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading queue dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();

    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        // Recover .sending-* files: these were claimed for HTTP POST but the
        // process died before the request completed or restore_claim ran.
        // Rename back to the original .ndjson so the sender retries them.
        if let Some(marker_start) = name.find(SENDING_MARKER) {
            if !artifact_is_quiet(&path, now, CLAIM_STALE_AFTER) {
                continue;
            }
            let ready_name = &name[..marker_start];
            let ready_path = path.with_file_name(ready_name);
            match fs::rename(&path, &ready_path) {
                Ok(()) => {
                    tracing::info!(
                        "recovered stale .sending segment -> {}",
                        ready_path.display()
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        "failed to recover stale .sending segment {}",
                        path.display()
                    );
                }
            }
            continue;
        }

        // Recover interrupted active segments conservatively. The age check
        // protects partials written by pre-lock AtomCode versions; the lock
        // protects active segments written by this and later versions.
        if name.ends_with(PARTIAL_EXT) {
            let marker_path = managed_marker_path(&path);
            let managed = marker_path.exists();
            if !managed && !recover_legacy {
                continue;
            }
            if !managed && !partial_is_quiet(&path, now) {
                continue;
            }

            let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
                continue;
            };
            if file.try_lock_exclusive().is_err() {
                continue;
            }
            let empty = file.metadata().map(|m| m.len() == 0).unwrap_or(false);
            let age = partial_filename_age(name, now);
            if empty {
                drop(file);
                remove_recovered_artifact(&path, "empty telemetry .partial segment");
                let _ = fs::remove_file(&marker_path);
                continue;
            }
            if age.is_some_and(|age| age > RAW_RETENTION) {
                drop(file);
                remove_recovered_artifact(&path, "expired telemetry .partial segment");
                let _ = fs::remove_file(&marker_path);
                continue;
            }
            if !managed && !age.is_some_and(|age| age >= PARTIAL_QUIET_AFTER) {
                continue;
            }
            let valid = partial_lines_are_valid(&file);
            drop(file);
            if !valid {
                let invalid_path = path.with_extension(INVALID_EXT);
                match fs::rename(&path, &invalid_path) {
                    Ok(()) => tracing::warn!(
                        "quarantined malformed telemetry segment as {}",
                        invalid_path.display()
                    ),
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => tracing::warn!(
                        ?e,
                        "failed to quarantine telemetry .partial segment {}",
                        path.display()
                    ),
                }
                let _ = fs::remove_file(&marker_path);
                continue;
            }

            let ready_path = path.with_extension(READY_EXT);
            match fs::rename(&path, &ready_path) {
                Ok(()) => tracing::info!(
                    "recovered stale telemetry .partial segment -> {}",
                    ready_path.display()
                ),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(
                    ?e,
                    "failed to recover telemetry .partial segment {}",
                    path.display()
                ),
            }
            let _ = fs::remove_file(&marker_path);
        }
    }

    cleanup_invalid_files(dir, now)?;
    cleanup_orphan_markers(dir)?;

    Ok(())
}

fn managed_marker_path(partial: &Path) -> PathBuf {
    let name = partial
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("segment.partial");
    partial.with_file_name(format!("{name}{MARKER_SUFFIX}"))
}

/// Remove `.owner` markers whose `.partial` is gone — the segment was already
/// promoted or removed and the marker was orphaned (e.g. a crash between
/// `Segment::finish`'s rename and the marker cleanup). A marker whose `.partial`
/// still exists (a live, locked segment held by another process) is left alone.
fn cleanup_orphan_markers(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let is_marker = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(MARKER_SUFFIX));
        if !is_marker {
            continue;
        }
        // The marker for `X.partial` is `X.partial.owner`; drop the suffix.
        let partial = path.with_extension("");
        if !partial.exists() {
            remove_recovered_artifact(&path, "orphan telemetry segment marker");
        }
    }
    Ok(())
}

fn cleanup_invalid_files(dir: &Path, now: chrono::DateTime<chrono::Utc>) -> Result<()> {
    let mut invalid: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some(INVALID_EXT))
        .collect();
    invalid.sort();

    for path in &invalid {
        let expired = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| partial_filename_age(name, now))
            .is_some_and(|age| age > RAW_RETENTION);
        if expired {
            remove_recovered_artifact(path, "expired invalid telemetry segment");
        }
    }

    invalid.retain(|path| path.exists());
    // Quarantined segments share the queue budget with ready data instead of
    // opening a second one (which would let the on-disk footprint reach ~2× the
    // cap). Evict the oldest invalid files first while the COMBINED ready+invalid
    // footprint is over cap, so live telemetry keeps priority over quarantine.
    let (ready_count, ready_bytes) = ready_footprint(dir);
    let mut total_bytes: u64 = invalid
        .iter()
        .filter_map(|path| fs::metadata(path).ok().map(|meta| meta.len()))
        .sum();
    while roll::over_cap(ready_count + invalid.len(), ready_bytes + total_bytes) {
        let Some(oldest) = invalid.first().cloned() else {
            break;
        };
        let bytes = fs::metadata(&oldest).map(|meta| meta.len()).unwrap_or(0);
        remove_recovered_artifact(&oldest, "invalid telemetry segment over queue cap");
        invalid.remove(0);
        total_bytes = total_bytes.saturating_sub(bytes);
    }
    Ok(())
}

/// Count and size the ready `.ndjson` segments currently on disk (best-effort).
fn ready_footprint(dir: &Path) -> (usize, u64) {
    let mut count = 0usize;
    let mut bytes = 0u64;
    if let Ok(entries) = fs::read_dir(dir) {
        for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
            if path.extension().and_then(|ext| ext.to_str()) == Some(READY_EXT) {
                count += 1;
                bytes += fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            }
        }
    }
    (count, bytes)
}

fn partial_filename_age(
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::Duration> {
    let Some(stamp) = name.get(..15) else {
        return None;
    };
    let Ok(naive) = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S") else {
        return None;
    };
    Some(now.signed_duration_since(naive.and_utc()))
}

fn partial_is_quiet(path: &Path, now: chrono::DateTime<chrono::Utc>) -> bool {
    artifact_is_quiet(path, now, PARTIAL_QUIET_AFTER)
}

fn artifact_is_quiet(
    path: &Path,
    now: chrono::DateTime<chrono::Utc>,
    quiet_after: chrono::Duration,
) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|m| m.modified()) else {
        return false;
    };
    now.signed_duration_since(chrono::DateTime::<chrono::Utc>::from(modified)) >= quiet_after
}

fn remove_recovered_artifact(path: &Path, label: &str) {
    match fs::remove_file(path) {
        Ok(()) => tracing::info!("removed {label} {}", path.display()),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(?e, "failed to remove {label} {}", path.display()),
    }
}

fn partial_lines_are_valid(file: &File) -> bool {
    let Ok(cloned) = file.try_clone() else {
        return false;
    };
    let mut saw_event = false;
    for line in BufReader::new(cloned).lines() {
        let Ok(line) = line else {
            return false;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            return false;
        };
        if value.get("event_id").and_then(|v| v.as_str()).is_none()
            || value.get("ts").and_then(|v| v.as_i64()).is_none()
        {
            return false;
        }
        saw_event = true;
    }
    saw_event
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::*;
    use tempfile::TempDir;

    fn rec() -> Record {
        Record {
            envelope: Envelope {
                device_id: Uuid::nil(),
                launch_id: Uuid::nil(),
                account_id: None,
                session_id: Uuid::nil(),
                turn_id: None,
                ts: 0,
                schema_version: 1,
                app_version: "x".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                locale: "en".into(),
                provider: None,
                provider_host: None,
                model: None,
                repo_origin: None,
                mode: None,
                surface: None,
            },
            event: Event::OpenAtomcode {
                dangerously_skip_permissions: false,
            },
        }
    }

    #[test]
    fn append_rolls_after_500() {
        let d = TempDir::new().unwrap();
        let mut q = Queue::open(d.path().to_path_buf()).unwrap();
        for _ in 0..499 {
            assert!(!q.append(&rec()).unwrap());
        }
        assert!(q.append(&rec()).unwrap(), "500th append should roll");
        let segs = q.segments_sorted().unwrap();
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn active_partial_segment_is_not_ready() {
        let d = TempDir::new().unwrap();
        let mut q = Queue::open(d.path().to_path_buf()).unwrap();
        q.append(&rec()).unwrap();

        assert!(
            q.ready_segments_sorted().unwrap().is_empty(),
            "active .partial segment must not be visible to senders"
        );
        let partials: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(PARTIAL_EXT))
            .collect();
        assert_eq!(partials.len(), 1);
    }

    #[test]
    fn force_roll_empty_is_noop() {
        let d = TempDir::new().unwrap();
        let mut q = Queue::open(d.path().to_path_buf()).unwrap();
        assert!(q.force_roll().unwrap().is_none());
    }

    #[test]
    fn force_roll_closes_current_and_deletes_empty() {
        let d = TempDir::new().unwrap();
        let mut q = Queue::open(d.path().to_path_buf()).unwrap();
        q.append(&rec()).unwrap();
        let p = q.force_roll().unwrap().unwrap();
        assert!(p.exists(), "rolled segment should remain");
        assert_eq!(p.extension().and_then(|s| s.to_str()), Some(READY_EXT));
        let c = fs::read_to_string(&p).unwrap();
        assert!(
            c.contains(r#""event_id":"open_atomcode""#),
            "rolled segment should contain appended event"
        );
        assert!(
            q.force_roll().unwrap().is_none(),
            "no current segment after roll"
        );
    }

    #[test]
    fn claim_oldest_segment_is_exclusive() {
        let d = TempDir::new().unwrap();
        let mut q1 = Queue::open(d.path().to_path_buf()).unwrap();
        q1.append(&rec()).unwrap();
        q1.force_roll().unwrap();

        let q2 = Queue::open(d.path().to_path_buf()).unwrap();
        let claimed = q1.claim_oldest_segment().unwrap();
        assert!(claimed.is_some(), "first claimant should get the segment");
        assert!(
            q2.claim_oldest_segment().unwrap().is_none(),
            "second claimant should not see the claimed segment"
        );
    }

    #[test]
    fn restore_claim_makes_segment_ready_again() {
        let d = TempDir::new().unwrap();
        let mut q = Queue::open(d.path().to_path_buf()).unwrap();
        q.append(&rec()).unwrap();
        q.force_roll().unwrap();

        let claimed = q.claim_oldest_segment().unwrap().unwrap();
        assert!(q.ready_segments_sorted().unwrap().is_empty());

        let restored = q.restore_claim(&claimed).unwrap().unwrap();
        assert!(restored.exists());
        assert_eq!(q.ready_segments_sorted().unwrap(), vec![restored]);
    }

    #[test]
    fn open_recovers_stale_sending_files() {
        let d = TempDir::new().unwrap();

        // Simulate a previous process: create a .ndjson, then "claim" it
        // by renaming to .sending-* (as if HTTP POST was in-flight when
        // the process crashed).
        let mut q = Queue::open(d.path().to_path_buf()).unwrap();
        q.append(&rec()).unwrap();
        let rolled = q.force_roll().unwrap().unwrap();
        let claimed = rolled.with_file_name(format!(
            "{}{}12345-abcdef",
            rolled.file_name().unwrap().to_str().unwrap(),
            SENDING_MARKER
        ));
        fs::rename(&rolled, &claimed).unwrap();

        // The .sending file should not appear in ready_segments.
        assert!(q.ready_segments_sorted().unwrap().is_empty());

        recover_stale_files_at(
            d.path(),
            chrono::Utc::now() + CLAIM_STALE_AFTER + chrono::Duration::seconds(1),
            false,
        )
        .unwrap();
        let q2 = Queue::open(d.path().to_path_buf()).unwrap();
        let ready = q2.ready_segments_sorted().unwrap();
        assert_eq!(
            ready.len(),
            1,
            "stale .sending file should be recovered as .ndjson"
        );
        assert!(
            !claimed.exists(),
            "original .sending file should have been renamed away"
        );

        // The recovered file should contain the original event.
        let contents = fs::read_to_string(&ready[0]).unwrap();
        assert!(
            contents.contains(r#""event_id":"open_atomcode""#),
            "recovered segment should contain original event data"
        );
    }

    #[test]
    fn recovery_removes_empty_and_quarantines_malformed_partial_files() {
        let d = TempDir::new().unwrap();
        let stamp = (chrono::Utc::now() - chrono::Duration::days(2)).format("%Y%m%d-%H%M%S");

        // Simulate stale empty .partial files left by a previous crash.
        let empty_partial = d.path().join(format!("{stamp}-deadbeef.partial"));
        fs::File::create(&empty_partial).unwrap();
        assert_eq!(fs::metadata(&empty_partial).unwrap().len(), 0);

        let nonempty_partial = d.path().join(format!("{stamp}-alivecafe.partial"));
        fs::write(&nonempty_partial, b"some data\n").unwrap();

        recover_stale_files_at(
            d.path(),
            chrono::Utc::now() + chrono::Duration::days(2),
            true,
        )
        .unwrap();

        assert!(
            !empty_partial.exists(),
            "quiet empty .partial file should be removed"
        );
        assert!(
            !nonempty_partial.exists(),
            "malformed .partial file should be renamed"
        );
        assert!(nonempty_partial.with_extension(INVALID_EXT).exists());
    }

    #[test]
    fn open_recovers_stale_valid_partial_without_rewriting_event() {
        let d = TempDir::new().unwrap();
        let stamp = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y%m%d-%H%M%S");
        let partial = d.path().join(format!("{stamp}-alivecafe.partial"));
        let original = serde_json::to_string(&rec()).unwrap();
        fs::write(&partial, format!("{original}\n")).unwrap();

        recover_stale_files_at(
            d.path(),
            chrono::Utc::now() + chrono::Duration::days(2),
            true,
        )
        .unwrap();
        let q = Queue::open(d.path().to_path_buf()).unwrap();
        let ready = q.ready_segments_sorted().unwrap();
        assert_eq!(ready.len(), 1);
        assert!(!partial.exists());
        assert_eq!(
            fs::read_to_string(&ready[0]).unwrap(),
            format!("{original}\n")
        );
    }

    #[test]
    fn open_does_not_recover_recent_partial() {
        let d = TempDir::new().unwrap();
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let partial = d.path().join(format!("{stamp}-alivecafe.partial"));
        fs::write(
            &partial,
            format!("{}\n", serde_json::to_string(&rec()).unwrap()),
        )
        .unwrap();

        let q = Queue::open(d.path().to_path_buf()).unwrap();
        assert!(q.ready_segments_sorted().unwrap().is_empty());
        assert!(partial.exists());
        let stats = q.stats().unwrap();
        assert_eq!(stats.stranded_partial_count, 1);
        assert!(stats.stranded_partial_bytes > 0);
    }

    #[test]
    fn open_does_not_automatically_recover_unmarked_legacy_partial() {
        let d = TempDir::new().unwrap();
        let stamp = (chrono::Utc::now() - chrono::Duration::days(2)).format("%Y%m%d-%H%M%S");
        let partial = d.path().join(format!("{stamp}-legacy.partial"));
        fs::write(
            &partial,
            format!("{}\n", serde_json::to_string(&rec()).unwrap()),
        )
        .unwrap();
        filetime::set_file_mtime(
            &partial,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400),
            ),
        )
        .unwrap();

        let q = Queue::open(d.path().to_path_buf()).unwrap();
        assert!(q.ready_segments_sorted().unwrap().is_empty());
        assert!(partial.exists());
        assert_eq!(q.recover_legacy_partials().unwrap(), 1);
        assert!(!partial.exists());
    }

    #[test]
    fn open_automatically_recovers_managed_partial_after_crash() {
        let d = TempDir::new().unwrap();
        let partial = {
            let mut q = Queue::open(d.path().to_path_buf()).unwrap();
            q.append(&rec()).unwrap();
            q.current.as_ref().unwrap().path.clone()
        };
        assert!(partial.exists());
        assert!(managed_marker_path(&partial).exists());

        let q = Queue::open(d.path().to_path_buf()).unwrap();
        assert_eq!(q.ready_segments_sorted().unwrap().len(), 1);
        assert!(!partial.exists());
        assert!(!managed_marker_path(&partial).exists());
    }

    #[test]
    fn clear_inactive_skips_locked_partial_and_removes_other_artifacts() {
        let d = TempDir::new().unwrap();
        let mut active = Queue::open(d.path().to_path_buf()).unwrap();
        active.append(&rec()).unwrap();
        let active_path = active.current.as_ref().unwrap().path.clone();
        fs::write(d.path().join("old.ndjson"), b"event\n").unwrap();
        fs::write(d.path().join("bad.invalid"), b"bad\n").unwrap();

        let passive = Queue::open(d.path().to_path_buf()).unwrap();
        let cleared = passive.clear_inactive().unwrap();
        assert_eq!(cleared.removed, 2);
        assert_eq!(cleared.skipped, 1);
        assert!(active_path.exists());
        assert!(active.force_roll().unwrap().unwrap().exists());
    }

    #[test]
    fn recovery_removes_partial_outside_raw_retention() {
        let d = TempDir::new().unwrap();
        let stamp = (chrono::Utc::now() - RAW_RETENTION - chrono::Duration::days(1))
            .format("%Y%m%d-%H%M%S");
        let partial = d.path().join(format!("{stamp}-expired.partial"));
        fs::write(
            &partial,
            format!("{}\n", serde_json::to_string(&rec()).unwrap()),
        )
        .unwrap();

        recover_stale_files_at(
            d.path(),
            chrono::Utc::now() + chrono::Duration::days(2),
            true,
        )
        .unwrap();
        let q = Queue::open(d.path().to_path_buf()).unwrap();
        assert!(q.ready_segments_sorted().unwrap().is_empty());
        assert!(!partial.exists());
    }

    #[test]
    fn open_does_not_recover_locked_stale_partial() {
        let d = TempDir::new().unwrap();
        let stamp = (chrono::Utc::now() - PARTIAL_QUIET_AFTER - chrono::Duration::minutes(1))
            .format("%Y%m%d-%H%M%S");
        let partial = d.path().join(format!("{stamp}-locked.partial"));
        fs::write(
            &partial,
            format!("{}\n", serde_json::to_string(&rec()).unwrap()),
        )
        .unwrap();
        let active = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&partial)
            .unwrap();
        active.lock_exclusive().unwrap();

        recover_stale_files_at(
            d.path(),
            chrono::Utc::now() + chrono::Duration::days(2),
            true,
        )
        .unwrap();
        let q = Queue::open(d.path().to_path_buf()).unwrap();
        assert!(q.ready_segments_sorted().unwrap().is_empty());
        assert!(partial.exists());
    }

    #[test]
    fn recovery_does_not_delete_active_zero_length_partial() {
        let d = TempDir::new().unwrap();
        let mut q = Queue::open(d.path().to_path_buf()).unwrap();
        q.append(&rec()).unwrap();
        let active_path = q.current.as_ref().unwrap().path.clone();
        assert_eq!(fs::metadata(&active_path).unwrap().len(), 0);

        recover_stale_files_at(
            d.path(),
            chrono::Utc::now() + chrono::Duration::days(2),
            false,
        )
        .unwrap();

        assert!(active_path.exists());
        let ready = q.force_roll().unwrap().unwrap();
        assert!(ready.exists());
    }

    #[test]
    fn count_non_empty_lines_ignores_blank_lines() {
        let d = TempDir::new().unwrap();
        let path = d.path().join("segment.ndjson");
        fs::write(&path, b"{\"a\":1}\n\n{\"b\":2}\n\n").unwrap();

        assert_eq!(count_non_empty_lines(&path).unwrap(), 2);
    }

    #[test]
    fn invalid_files_share_the_queue_cap_with_ready_segments() {
        let d = TempDir::new().unwrap();
        // Fill ready segments up to the cap.
        for i in 0..roll::MAX_SEGMENT_FILES {
            fs::write(d.path().join(format!("20260101-000000-{i:08x}.ndjson")), b"{}\n").unwrap();
        }
        // Quarantined segments must not open a second, independent budget.
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        for i in 0..10 {
            fs::write(d.path().join(format!("{stamp}-{i:08x}.invalid")), b"x\n").unwrap();
        }

        recover_stale_files_at(d.path(), chrono::Utc::now(), false).unwrap();

        let count = |ext: &str| {
            fs::read_dir(d.path())
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(ext))
                .count()
        };
        let combined = count(READY_EXT) + count(INVALID_EXT);
        assert!(
            combined <= roll::MAX_SEGMENT_FILES,
            "ready + invalid ({combined}) must stay within the shared cap {}",
            roll::MAX_SEGMENT_FILES
        );
    }

    #[test]
    fn recovery_removes_orphan_owner_markers() {
        let d = TempDir::new().unwrap();
        // A marker left behind after its .partial was already promoted/removed
        // (crash between finish()'s rename and the marker cleanup).
        let orphan = d.path().join("20260101-000000-deadbeef.partial.owner");
        fs::write(&orphan, b"").unwrap();

        recover_stale_files_at(d.path(), chrono::Utc::now(), false).unwrap();

        assert!(
            !orphan.exists(),
            "orphan .owner marker (no matching .partial) should be cleaned up"
        );
    }
}
