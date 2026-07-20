//! Append-only NDJSON segment queue on disk.

pub mod roll;

use crate::event::Record;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const READY_EXT: &str = "ndjson";
const PARTIAL_EXT: &str = "partial";
const SENDING_MARKER: &str = ".sending-";

pub struct Queue {
    dir: PathBuf,
    current: Option<Segment>,
    /// Cumulative dropped count (in-memory or on-disk FIFO eviction).
    pub dropped: u64,
}

pub struct Segment {
    pub path: PathBuf,
    ready_path: PathBuf,
    writer: BufWriter<File>,
    events: u32,
    bytes: u64,
}

impl Segment {
    fn new(path: PathBuf, ready_path: PathBuf) -> Result<Self> {
        let f = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("creating segment {}", path.display()))?;
        Ok(Self {
            path,
            ready_path,
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
        drop(self.writer);
        fs::rename(&partial_path, &ready_path).with_context(|| {
            format!(
                "rolling segment {} -> {}",
                partial_path.display(),
                ready_path.display()
            )
        })?;
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
        //   2. Empty .partial files are segments created but never written
        //      to before the process exited.  Safe to delete.
        recover_stale_files(&dir)?;

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
                drop(seg.writer);
                let _ = fs::remove_file(path);
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
                Ok(()) => return Ok(Some(claimed)),
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
/// - Empty `.partial` files → delete (they contain no events).
fn recover_stale_files(dir: &Path) -> Result<()> {
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

        // Clean up empty .partial files: these were created but never
        // received any events before the process exited.
        if name.ends_with(PARTIAL_EXT) {
            if let Ok(meta) = fs::metadata(&path) {
                if meta.len() == 0 {
                    match fs::remove_file(&path) {
                        Ok(()) => {
                            tracing::info!("removed empty .partial segment {}", path.display());
                        }
                        Err(e) => {
                            tracing::warn!(
                                ?e,
                                "failed to remove empty .partial segment {}",
                                path.display()
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(())
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

        // Re-open the queue — recover_stale_files should restore it.
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
    fn open_removes_empty_partial_files() {
        let d = TempDir::new().unwrap();

        // Simulate stale empty .partial files left by a previous crash.
        let empty_partial = d.path().join("20260512-000000-deadbeef.partial");
        fs::File::create(&empty_partial).unwrap();
        assert_eq!(fs::metadata(&empty_partial).unwrap().len(), 0);

        // Also create a non-empty .partial that should NOT be deleted.
        let nonempty_partial = d.path().join("20260512-000001-alivecafe.partial");
        fs::write(&nonempty_partial, b"some data\n").unwrap();

        let _q = Queue::open(d.path().to_path_buf()).unwrap();

        assert!(
            !empty_partial.exists(),
            "empty .partial file should be removed on Queue::open"
        );
        assert!(
            nonempty_partial.exists(),
            "non-empty .partial file should be kept"
        );
    }

    #[test]
    fn count_non_empty_lines_ignores_blank_lines() {
        let d = TempDir::new().unwrap();
        let path = d.path().join("segment.ndjson");
        fs::write(&path, b"{\"a\":1}\n\n{\"b\":2}\n\n").unwrap();

        assert_eq!(count_non_empty_lines(&path).unwrap(), 2);
    }
}
