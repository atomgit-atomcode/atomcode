//! What a mount attributes a turn to: telemetry, the datalog, and the session's
//! own cost record.
//!
//! All three follow the model the mount was built for, which is what makes a
//! `/login` or `/model` switch honest — the tree is rebuilt (or patched) with the
//! new config, and everything downstream must report the model the person is now
//! on rather than the one the session started with.

mod support;

use std::sync::Arc;

use atomcode_coding::{prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::LlmProvider;
use support::{allow, mount_parts, quiet_options, turn};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// Answers with a little text and a usage report — enough for a metering
/// decorator to fold a `TokenUsage` and emit one `LlmChat`.
struct CannedProvider;

#[async_trait::async_trait]
impl LlmProvider for CannedProvider {
    fn model_name(&self) -> &str {
        "canned"
    }
    async fn chat_stream(
        &self,
        _: &[Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        use atomcode_kernel::stream::{StreamEvent, TokenUsage};
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("looks good".into()),
            StreamEvent::Usage(TokenUsage {
                prompt: 500,
                completion: 30,
                cached: 0,
            }),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

/// A session launched before a provider was resolvable has no model name yet.
/// Once `/login` resolves one, the turn must be billed to THAT model — the
/// telemetry envelope is built with the mount, not frozen at prepare.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn telemetry_reports_the_model_the_mount_was_built_for() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let (telemetry, captured) = atomcode_telemetry::Telemetry::in_memory("test".into());
    let project = tempfile::tempdir().unwrap();
    // Onboarding: no resolvable provider, so no model name at prepare.
    let mut cfg = CodingAgentConfig::new("k", "http://localhost", "", project.path());
    cfg.telemetry = Some(telemetry);
    let opts = quiet_options();
    let parts = prepare(&cfg, opts.clone()).await.unwrap();

    // `/login` resolves the real provider: the config picks up the model and the
    // tree is mounted with it.
    cfg.model = "swapped-model".to_string();
    let mut mounted = mount_parts(&parts, &cfg, &opts, Arc::new(CannedProvider)).await;
    let _ = turn(&mut mounted.handle, "hi", allow()).await;
    mounted.shutdown().await;

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let records = captured.lock().await;
    let chat = records
        .iter()
        .find(|r| matches!(r.event, atomcode_telemetry::Event::LlmChat { .. }))
        .expect("the turn must emit one LlmChat");
    assert_eq!(
        chat.envelope.model.as_deref(),
        Some("swapped-model"),
        "the model active at mount, not the one prepare saw"
    );
}

/// With `[datalog]` on, the turn is written where the person pointed it.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn the_configured_datalog_records_the_turn() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let datalog_root = home.path().join("custom-datalog");
    let mut cfg = CodingAgentConfig::new("k", "http://localhost", "logged-model", project.path());
    cfg.datalog.enabled = true;
    cfg.datalog.dir = Some(datalog_root.display().to_string());

    let opts = quiet_options();
    let parts = prepare(&cfg, opts.clone()).await.unwrap();
    let mut mounted = mount_parts(&parts, &cfg, &opts, Arc::new(CannedProvider)).await;
    let _ = turn(&mut mounted.handle, "record this turn", allow()).await;
    mounted.shutdown().await;

    let project_dir = std::fs::read_dir(&datalog_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let files: Vec<_> = std::fs::read_dir(project_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let markdown = files
        .iter()
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
        .expect("turn markdown");
    let jsonl = files
        .iter()
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("per-round request jsonl");
    assert!(std::fs::read_to_string(markdown)
        .unwrap()
        .contains("**Response:**\nlooks good"));
    let request = std::fs::read_to_string(jsonl).unwrap();
    assert!(request.contains("\"model\":\"logged-model\""));
    assert!(request.contains("record this turn"));
}

/// A session that switches models keeps both models' spend, each under its own
/// name — the session's cost record is what `/cost` and the catalog read.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn a_sessions_cost_is_recorded_per_model_across_a_switch() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let mut cfg = CodingAgentConfig::new("k", "http://localhost", "model-a", project.path());
    cfg.provider_name = "provider-a".into();

    let opts = PrepareOptions {
        session: SessionMode::Fresh,
        ..quiet_options()
    };
    let mut parts = prepare(&cfg, opts.clone()).await.unwrap();
    let binding = parts.session.as_ref().unwrap();
    let manager = binding.manager.clone();
    let session_id = binding.id.clone();

    let mut first = mount_parts(&parts, &cfg, &opts, Arc::new(CannedProvider)).await;
    parts.publish_staged_session().unwrap();
    let _ = turn(&mut first.handle, "first", allow()).await;
    first.shutdown().await;

    cfg.provider_name = "provider-b".into();
    cfg.model = "model-b".into();
    let mut second = mount_parts(&parts, &cfg, &opts, Arc::new(CannedProvider)).await;
    let _ = turn(&mut second.handle, "second", allow()).await;
    second.shutdown().await;

    let report = atomcode_capabilities::session::aggregate_session_cost(
        &manager.read_meta(&session_id).unwrap(),
    );
    assert_eq!(report.models.len(), 2, "{report:?}");
    assert_eq!(report.models[0].provider_id, "provider-a");
    assert_eq!(report.models[0].model_id, "model-a");
    assert_eq!(report.models[1].provider_id, "provider-b");
    assert_eq!(report.models[1].model_id, "model-b");
}
