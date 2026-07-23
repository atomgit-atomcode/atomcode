//! Background sender task: drains rolled segments and POSTs them.

pub mod http;

use crate::queue::Queue;
use crate::runtime::Counters;
use http::{HttpSender, SendError};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::warn;

pub struct SenderRuntime {
    queue: Arc<Mutex<Queue>>,
    http: HttpSender,
    counters: Arc<Counters>,
    health_path: PathBuf,
}

impl SenderRuntime {
    pub fn new(
        queue: Arc<Mutex<Queue>>,
        http: HttpSender,
        counters: Arc<Counters>,
        health_path: PathBuf,
    ) -> Self {
        Self {
            queue,
            http,
            counters,
            health_path,
        }
    }

    /// Process one pending segment (oldest). Returns `Ok(None)` if queue empty.
    pub async fn flush_one(&self) -> Result<Option<PathBuf>, SendError> {
        let (seg, dropped) = {
            let q = self.queue.lock().await;
            (
                q.claim_oldest_segment().map_err(|_| SendError::Other)?,
                q.dropped,
            )
        };
        let seg = match seg {
            Some(s) => s,
            None => return Ok(None),
        };

        // Mirror queue's disk-eviction counter into our atomics (absolute value).
        self.counters
            .events_dropped_disk
            .store(dropped, Ordering::Relaxed);

        // Read segment size before deleting it.
        let bytes = std::fs::metadata(&seg).map(|m| m.len()).unwrap_or(0);

        match self.http.send_segment(&seg, dropped).await {
            Ok(()) => {}
            Err(SendError::BadRequest) => {
                let q = self.queue.lock().await;
                q.complete_claim(&seg).map_err(|_| SendError::Other)?;
                return Err(SendError::BadRequest);
            }
            Err(e) => {
                let q = self.queue.lock().await;
                if let Err(restore_err) = q.restore_claim(&seg) {
                    warn!(?restore_err, segment = %seg.display(), "telemetry segment restore failed");
                }
                return Err(e);
            }
        }

        {
            let q = self.queue.lock().await;
            if let Err(e) = q.complete_claim(&seg) {
                warn!(?e, "delete segment failed");
            }
        }

        self.counters
            .segments_posted
            .fetch_add(1, Ordering::Relaxed);
        self.counters.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.counters
            .last_post_unix_ms
            .store(now_ms, Ordering::Relaxed);

        tracing::info!(segment = %seg.display(), bytes, "telemetry segment posted");

        // Persist health snapshot (best-effort).
        self.persist_health();

        Ok(Some(seg))
    }

    /// Backoff schedule per spec §6: 2s, 8s, 30s, 120s, 300s (capped).
    pub fn backoff(attempt: u32) -> Duration {
        match attempt {
            0 => Duration::from_secs(2),
            1 => Duration::from_secs(8),
            2 => Duration::from_secs(30),
            3 => Duration::from_secs(120),
            _ => Duration::from_secs(300),
        }
    }

    /// Drain-loop step. Caller owns the tick-or-shutdown select.
    pub async fn drain_with_backoff(&self) {
        let mut attempt = 0u32;
        loop {
            match self.flush_one().await {
                Ok(None) => break,
                Ok(Some(_)) => {
                    attempt = 0;
                }
                Err(SendError::Unauthorized) => {
                    warn!("telemetry unauthorized — holding for 1h");
                    sleep(Duration::from_secs(3600)).await;
                    attempt = 0;
                }
                Err(SendError::BadRequest) => {
                    // Corrupt segment was already claimed and deleted by flush_one.
                    warn!("telemetry 400 — dropped claimed segment");
                    attempt = 0;
                }
                Err(SendError::RateLimited(Some(d))) => {
                    sleep(d).await;
                    attempt += 1;
                }
                Err(e) => {
                    warn!(?e, "telemetry send error; backing off");
                    sleep(Self::backoff(attempt)).await;
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }

    fn persist_health(&self) {
        let snap = self.counters.snapshot();
        if let Ok(json) = serde_json::to_string(&snap) {
            if let Some(parent) = self.health_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&self.health_path, json);
        }
    }
}
