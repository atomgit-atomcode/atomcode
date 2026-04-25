//! Append-only NDJSON segment queue on disk.

pub mod roll;

use crate::event::Record;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct Queue {
    dir: PathBuf,
    current: Option<Segment>,
    /// Cumulative dropped count (in-memory or on-disk FIFO eviction).
    pub dropped: u64,
}

pub struct Segment {
    pub path: PathBuf,
    writer: BufWriter<File>,
    events: u32,
    bytes: u64,
}

impl Segment {
    fn new(path: PathBuf) -> Result<Self> {
        let f = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("creating segment {}", path.display()))?;
        Ok(Self {
            path,
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
        if let Some(mut seg) = self.current.take() {
            if seg.events == 0 {
                // empty: drop the file
                drop(seg.writer);
                let _ = fs::remove_file(&seg.path);
                return Ok(None);
            }
            seg.fsync()?;
            let p = seg.path.clone();
            self.enforce_cap()?;
            return Ok(Some(p));
        }
        Ok(None)
    }

    pub fn segments_sorted(&self) -> Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("ndjson"))
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
        let path = self.dir.join(format!("{}-{}.ndjson", ts, Uuid::new_v4()));
        self.current = Some(Segment::new(path)?);
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
        assert!(
            q.force_roll().unwrap().is_none(),
            "no current segment after roll"
        );
    }
}
