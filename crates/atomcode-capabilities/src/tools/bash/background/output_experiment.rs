//! Manual, finite real-process experiments. Ignored in normal CI: these measure
//! wall-clock RSS/pipe scheduling and require system Python and ps on Unix.
use super::*;
use crate::world::{LocalShell, Shell, SpawnOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BYTES_PER_JOB: usize = 10 * 1024 * 1024;

fn script() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("finite_output.py");
    std::fs::write(&path, "import os,sys,time\nfor fd, count in [(1,128),(2,32)]:\n for _ in range(count):\n  data = b'x' * 65536\n  while data:\n   n = os.write(fd, data)\n   data = data[n:]\n  if len(sys.argv)>1: time.sleep(float(sys.argv[1]))\n").unwrap();
    (dir, path.to_str().unwrap().to_owned())
}
async fn rss_kib() -> u64 {
    let out = tokio::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .await
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// A tiny loopback HTTP endpoint on the same executor, NOT the real daemon.
async fn health_monitor(stop: tokio_util::sync::CancellationToken) -> serde_json::Value {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 512];
            if socket.read(&mut buf).await.is_ok() {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            }
        }
    });
    let mut count = 0;
    let mut failures = 0;
    let mut max_ms = 0;
    let mut peak = rss_kib().await;
    while !stop.is_cancelled() {
        let started = std::time::Instant::now();
        let response = tokio::time::timeout(Duration::from_secs(2), async {
            let mut socket = tokio::net::TcpStream::connect(addr).await?;
            socket
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await?;
            let mut bytes = Vec::new();
            socket.read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes.ends_with(b"ok"))
        })
        .await;
        if !matches!(response, Ok(Ok(true))) {
            failures += 1;
        }
        max_ms = max_ms.max(started.elapsed().as_millis());
        count += 1;
        peak = peak.max(rss_kib().await);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.abort();
    serde_json::json!({"synthetic_health_probes":count,"synthetic_health_failures":failures,"max_probe_ms":max_ms,"peak_rss_kib":peak})
}

async fn paused_consumer(jobs: usize) {
    let (_dir, path) = script();
    let baseline = rss_kib().await;
    let stop = tokio_util::sync::CancellationToken::new();
    let monitor = tokio::spawn(health_monitor(stop.clone()));
    let mut processes = Vec::new();
    let start = std::time::Instant::now();
    for _ in 0..jobs {
        processes.push(
            LocalShell
                .spawn_program(
                    "/usr/bin/python3",
                    &[path.clone()],
                    &SpawnOptions::default(),
                )
                .await
                .unwrap(),
        );
    }
    // Deliberately do not consume any output. With a bounded pipe queue these
    // children would backpressure instead of completing before consumption.
    tokio::time::timeout(
        Duration::from_secs(30),
        futures::future::join_all(processes.iter().map(|p| p.wait())),
    )
    .await
    .unwrap()
    .into_iter()
    .for_each(|result| assert!(result.unwrap().success()));
    tokio::time::sleep(Duration::from_millis(250)).await;
    let queued_rss = rss_kib().await;
    let mut bytes = 0;
    for process in &processes {
        while let Some(chunk) = process.next_chunk().await {
            bytes += chunk.bytes().len();
        }
    }
    assert_eq!(bytes, BYTES_PER_JOB * jobs);
    stop.cancel();
    let mut result = monitor.await.unwrap();
    result["scenario"] = "production_local_shell_paused_consumer".into();
    result["jobs"] = jobs.into();
    result["produced_and_retained_bytes"] = bytes.into();
    result["baseline_rss_kib"] = baseline.into();
    result["queued_rss_kib"] = queued_rss.into();
    result["elapsed_ms"] = (start.elapsed().as_millis() as u64).into();
    println!("OUTPUT_EXPERIMENT {result}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual finite real-process RSS experiment"]
async fn local_output_experiment_paused_1() {
    paused_consumer(1).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual finite real-process RSS experiment"]
async fn local_output_experiment_paused_4() {
    paused_consumer(4).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual finite real-process RSS experiment"]
async fn local_output_experiment_paused_9() {
    paused_consumer(9).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual finite real-process RSS experiment"]
async fn local_output_experiment_background_9() {
    background_run(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual 64-second real-process poll experiment"]
async fn local_output_experiment_long_poll_9() {
    background_run(true).await;
}

async fn background_run(long: bool) {
    let (dir, path) = script();
    let context = ToolContext {
        working_dir: dir.path().to_owned(),
        cancel: Default::default(),
        progress: Default::default(),
        requester: None,
    };
    let world: Arc<dyn Shell> = Arc::new(LocalShell);
    let baseline = rss_kib().await;
    let stop = tokio_util::sync::CancellationToken::new();
    let monitor = tokio::spawn(health_monitor(stop.clone()));
    struct Jobs(Vec<String>);
    impl Drop for Jobs {
        fn drop(&mut self) {
            for id in &self.0 {
                let _ = kill(id);
            }
        }
    }
    let mut jobs = Jobs(Vec::new());
    let started = std::time::Instant::now();
    for _ in 0..9 {
        jobs.0.push(
            start(
                &world,
                &format!(
                    "/usr/bin/python3 '{path}' {}",
                    if long { "0.4" } else { "0" }
                ),
                &context,
            )
            .await
            .unwrap(),
        );
    }
    let longest_poll = AtomicU64::new(0);
    let polls = AtomicU64::new(0);
    tokio::time::timeout(
        Duration::from_secs(90),
        futures::future::join_all(jobs.0.iter().map(|id| async {
            loop {
                let before = std::time::Instant::now();
                wait_for_poll(id, 60_000, PollWaitUntil::Exit, &context).await?;
                longest_poll.fetch_max(before.elapsed().as_millis() as u64, Ordering::Relaxed);
                polls.fetch_add(1, Ordering::Relaxed);
                if store()
                    .lock()
                    .unwrap()
                    .get(id)
                    .unwrap()
                    .shared
                    .finished
                    .load(Ordering::Acquire)
                {
                    return Ok::<(), String>(());
                }
            }
        })),
    )
    .await
    .unwrap()
    .into_iter()
    .for_each(|result| result.unwrap());
    let retained_rss = rss_kib().await;
    let mut retained = 0;
    let mut produced = 0;
    for id in &jobs.0 {
        {
            let guard = store().lock().unwrap();
            let job = guard.get(id).unwrap();
            let output = job.shared.output.lock().unwrap();
            produced += output.total;
            retained += output.tail.len();
        }
        let result = poll(id).unwrap();
        assert!(!matches!(result.status, Status::Running));
    }
    assert_eq!(produced, 9 * BYTES_PER_JOB);
    assert!(retained <= 9 * MAX_OUTPUT_BYTES);
    stop.cancel();
    let mut result = monitor.await.unwrap();
    result["scenario"] = if long {
        "production_background_long_poll_9"
    } else {
        "production_background_start_poll_9"
    }
    .into();
    result["poll_calls"] = polls.load(Ordering::Relaxed).into();
    result["longest_poll_ms"] = longest_poll.load(Ordering::Relaxed).into();
    if long {
        assert!(polls.load(Ordering::Relaxed) >= 18);
        assert!((59_000..65_000).contains(&longest_poll.load(Ordering::Relaxed)));
    }
    result["produced_bytes"] = produced.into();
    result["ring_retained_bytes"] = retained.into();
    result["baseline_rss_kib"] = baseline.into();
    result["retained_rss_kib"] = retained_rss.into();
    result["elapsed_ms"] = (started.elapsed().as_millis() as u64).into();
    println!("OUTPUT_EXPERIMENT {result}");
}
