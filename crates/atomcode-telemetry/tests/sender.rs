use atomcode_telemetry::event::*;
use atomcode_telemetry::queue::Queue;
use atomcode_telemetry::sender::{http::HttpSender, SenderRuntime};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
            app_version: "test".into(),
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

#[tokio::test]
async fn sender_posts_and_deletes_segment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let d = TempDir::new().unwrap();
    let mut q = Queue::open(d.path().to_path_buf()).unwrap();
    for _ in 0..5 {
        q.append(&rec()).unwrap();
    }
    q.force_roll().unwrap();

    let q = Arc::new(Mutex::new(q));
    let http = HttpSender::new(format!("{}/v1/events", server.uri()), "test".into());
    let counters = Arc::new(atomcode_telemetry::Counters::default());
    let health_path = d.path().join("health.json");
    let rt = SenderRuntime::new(q.clone(), http, counters, health_path);

    assert!(rt.flush_one().await.unwrap().is_some());
    assert!(
        rt.flush_one().await.unwrap().is_none(),
        "queue should be empty after flush"
    );
    assert!(q.lock().await.segments_sorted().unwrap().is_empty());
}

#[tokio::test]
async fn sender_drops_segment_on_400() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;

    let d = TempDir::new().unwrap();
    let mut q = Queue::open(d.path().to_path_buf()).unwrap();
    q.append(&rec()).unwrap();
    q.force_roll().unwrap();

    let q = Arc::new(Mutex::new(q));
    let http = HttpSender::new(format!("{}/v1/events", server.uri()), "test".into());
    let counters = Arc::new(atomcode_telemetry::Counters::default());
    let health_path = d.path().join("health.json");
    let rt = SenderRuntime::new(q.clone(), http, counters, health_path);

    rt.drain_with_backoff().await;
    assert!(q.lock().await.segments_sorted().unwrap().is_empty());
}

#[tokio::test]
async fn only_one_sender_can_claim_a_ready_segment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let d = TempDir::new().unwrap();
    let mut q = Queue::open(d.path().to_path_buf()).unwrap();
    q.append(&rec()).unwrap();
    q.force_roll().unwrap();

    let q1 = Arc::new(Mutex::new(Queue::open(d.path().to_path_buf()).unwrap()));
    let q2 = Arc::new(Mutex::new(Queue::open(d.path().to_path_buf()).unwrap()));
    let counters1 = Arc::new(atomcode_telemetry::Counters::default());
    let counters2 = Arc::new(atomcode_telemetry::Counters::default());
    let rt1 = SenderRuntime::new(
        q1,
        HttpSender::new(format!("{}/v1/events", server.uri()), "test".into()),
        counters1,
        d.path().join("health-1.json"),
    );
    let rt2 = SenderRuntime::new(
        q2,
        HttpSender::new(format!("{}/v1/events", server.uri()), "test".into()),
        counters2,
        d.path().join("health-2.json"),
    );

    assert!(rt1.flush_one().await.unwrap().is_some());
    assert!(rt2.flush_one().await.unwrap().is_none());
}

use atomcode_telemetry::config::{ResolvedConfig, TelemetryState};
use atomcode_telemetry::Telemetry;

#[tokio::test]
async fn track_writes_to_disk_queue() {
    let d = tempfile::TempDir::new().unwrap();
    let cfg = ResolvedConfig {
        state: TelemetryState::Enabled,
        endpoint: "http://127.0.0.1:1/v1/events".into(),
        atomcode_dir: d.path().to_path_buf(),
    };
    let tel = Telemetry::init(cfg, "test".into());
    tel.track(Event::OpenAtomcode {
        dangerously_skip_permissions: false,
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    tel.shutdown(std::time::Duration::from_millis(500)).await;

    // "Written to the disk queue" is satisfied by the event landing in ANY segment:
    // a ready `.ndjson`, the still-open `.partial`, or a claimed `.ndjson.sending-*`.
    // `shutdown` does a bounded best-effort POST to the (unreachable) endpoint; when
    // that send doesn't finish within the budget the segment is left CLAIMED
    // (`.sending-*`) and recovered → `.ndjson` on the next startup. Scanning only
    // `.ndjson` made this test depend on that POST's timing against a dead endpoint
    // (it left the segment `.sending-*` here, so the event was missed). Scan every
    // file for the persisted event instead.
    let mut found = false;
    for e in std::fs::read_dir(d.path().join("telemetry/queue")).unwrap() {
        let p = e.unwrap().path();
        let Ok(c) = std::fs::read_to_string(&p) else {
            continue;
        };
        if c.lines()
            .any(|l| l.contains(r#""event_id":"open_atomcode""#))
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "expected an open_atomcode event persisted in the disk queue"
    );
}

#[tokio::test]
async fn counters_increment_on_post() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let d = TempDir::new().unwrap();
    let mut q = Queue::open(d.path().to_path_buf()).unwrap();
    q.append(&rec()).unwrap();
    q.force_roll().unwrap();

    let q = Arc::new(Mutex::new(q));
    let http = HttpSender::new(format!("{}/v1/events", server.uri()), "test".into());
    let counters = Arc::new(atomcode_telemetry::Counters::default());
    let health_path = d.path().join("health.json");
    let rt = SenderRuntime::new(q.clone(), http, counters.clone(), health_path.clone());

    rt.flush_one().await.unwrap();

    let snap = counters.snapshot();
    assert_eq!(snap.segments_posted, 1);
    assert!(snap.bytes_sent > 0, "bytes_sent should be > 0");
    assert!(
        snap.last_post_unix_ms > 0,
        "last_post_unix_ms should be > 0"
    );
    assert!(
        !snap.last_post_iso.is_empty(),
        "iso should be set when ms > 0"
    );
    assert!(
        snap.last_post_iso.contains('T'),
        "should be RFC 3339 format: {}",
        snap.last_post_iso
    );
    assert!(health_path.exists(), "health.json should be written");
}
