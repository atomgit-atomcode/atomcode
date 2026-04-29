//! Append-only NDJSON segment queue on disk.

pub mod roll;

use crate::event::Record;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Write};
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
            // Line-count approx via file size / avg_line is avoided here for accuracy:
            let contents = fs::read_to_string(p).unwrap_or_default();
            total_events += contents.lines().filter(|l| !l.is_empty()).count() as u64;
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
                // Count lines before delete to update dropped.
                if let Ok(contents) = fs::read_to_string(oldest) {
                    self.dropped += contents.lines().filter(|l| !l.is_empty()).count() as u64;
                }
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
            },
            event: Event::OpenAtomcode,
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
}
