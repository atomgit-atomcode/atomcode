//! `atomcode telemetry ...` subcommands.

use anyhow::Result;
use atomcode_telemetry::{
    config::{resolve, ProcessEnv, TelemetryConfig},
    queue::Queue,
    CliOverride, Event, Telemetry, TelemetryState,
};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::time::Duration;

pub fn status(atomcode_dir: &std::path::Path, cfg: &TelemetryConfig) -> Result<()> {
    let resolved = resolve(
        cfg,
        &CliOverride::default(),
        atomcode_dir.to_path_buf(),
        &ProcessEnv,
    );
    match &resolved.state {
        TelemetryState::Enabled => println!("Telemetry: enabled"),
        TelemetryState::Disabled(r) => println!("Telemetry: disabled (reason: {})", r),
    }
    println!(
        "Config:    enabled = {} ({}/config.toml)",
        match cfg.enabled {
            Some(v) => v.to_string(),
            None => "default-true".into(),
        },
        atomcode_dir.display()
    );
    let qdir = atomcode_dir.join("telemetry/queue");
    if qdir.exists() {
        let q = Queue::open(qdir)?;
        let s = q.stats()?;
        println!(
            "Queue:     {} events in {} segment(s) ({:.1} KB)",
            s.total_events,
            s.segment_count,
            s.total_bytes as f64 / 1024.0
        );
    } else {
        println!("Queue:     (empty)");
    }
    println!("Endpoint:  {}", resolved.endpoint);
    println!("Schema:    v{}", atomcode_telemetry::SCHEMA_VERSION);

    // Enrich with persistent health counters if available.
    let health_path = atomcode_dir.join("telemetry/health.json");
    if let Ok(json) = std::fs::read_to_string(&health_path) {
        if let Ok(h) =
            serde_json::from_str::<atomcode_telemetry::CountersSnapshot>(&json)
        {
            let last_post = if h.last_post_unix_ms == 0 {
                "never".to_string()
            } else {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let secs_ago = ((now_ms - h.last_post_unix_ms) / 1000).max(0);
                if secs_ago < 60 {
                    format!("{}s ago", secs_ago)
                } else if secs_ago < 3600 {
                    format!("{}min ago", secs_ago / 60)
                } else {
                    format!("{}h ago", secs_ago / 3600)
                }
            };
            println!(
                "Sent:      {} segments / {:.1} KB (last: {})",
                h.segments_posted,
                h.bytes_sent as f64 / 1024.0,
                last_post
            );
            println!("Tracked:   {} events", h.events_tracked);
            if h.events_dropped_mpsc + h.events_dropped_disk > 0 {
                println!(
                    "Dropped:   {} mpsc, {} disk",
                    h.events_dropped_mpsc, h.events_dropped_disk
                );
            }
        }
    }

    Ok(())
}

pub fn enable(cfg_path: &std::path::Path) -> Result<()> {
    write_flag(cfg_path, true)?;
    println!("Telemetry enabled.");
    Ok(())
}

pub async fn disable(cfg_path: &std::path::Path, tel: &Arc<Telemetry>) -> Result<()> {
    // Goodbye event: fire only if currently enabled (not flapping disable→disable).
    if tel.is_enabled() {
        tel.track(Event::TelemetryDisabled);
        // Give the background sender a moment to write the event to disk.
        // Shutdown just blocks on the in-memory drain + force-rolls the segment;
        // the actual POST may happen on this run or the next (queued).
        tel.shutdown(Duration::from_millis(500)).await;
    }
    write_flag(cfg_path, false)?;
    println!(
        "Telemetry disabled. Queued events retained for 7 days \
         (use `telemetry clear` to remove)."
    );
    Ok(())
}

pub fn dump(atomcode_dir: &std::path::Path, last: usize, pretty: bool) -> Result<()> {
    let qdir = atomcode_dir.join("telemetry/queue");
    if !qdir.exists() {
        println!("(no queued events)");
        return Ok(());
    }
    for line in collect_tail_lines(&qdir, last)? {
        if pretty {
            let v: serde_json::Value = serde_json::from_str(&line)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!("{}", line);
        }
    }
    Ok(())
}

fn collect_tail_lines(qdir: &std::path::Path, last: usize) -> Result<Vec<String>> {
    if last == 0 {
        return Ok(Vec::new());
    }

    let q = Queue::open(qdir.to_path_buf())?;
    let mut tail = VecDeque::with_capacity(last);
    for p in q.ready_segments_sorted()? {
        let file = std::fs::File::open(&p)?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            if tail.len() == last {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
    Ok(tail.into_iter().collect())
}

pub fn clear(atomcode_dir: &std::path::Path) -> Result<()> {
    let qdir = atomcode_dir.join("telemetry/queue");
    if !qdir.exists() {
        println!("(already empty)");
        return Ok(());
    }
    for e in std::fs::read_dir(&qdir)? {
        let p = e?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("ndjson") {
            std::fs::remove_file(&p)?;
        }
    }
    println!("Queue cleared.");
    Ok(())
}

fn write_flag(cfg_path: &std::path::Path, enabled: bool) -> Result<()> {
    let mut doc = if cfg_path.exists() {
        let s = std::fs::read_to_string(cfg_path)?;
        s.parse::<toml_edit::DocumentMut>().unwrap_or_default()
    } else {
        toml_edit::DocumentMut::new()
    };
    doc["telemetry"]["enabled"] = toml_edit::value(enabled);
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(cfg_path, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_tail_lines_keeps_only_last_non_empty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let qdir = dir.path().join("queue");
        std::fs::create_dir_all(&qdir).unwrap();
        std::fs::write(qdir.join("001.ndjson"), b"one\n\ntwo\n").unwrap();
        std::fs::write(qdir.join("002.ndjson"), b"three\nfour\n").unwrap();

        let lines = collect_tail_lines(&qdir, 2).unwrap();

        assert_eq!(lines, vec!["three".to_string(), "four".to_string()]);
    }
}
