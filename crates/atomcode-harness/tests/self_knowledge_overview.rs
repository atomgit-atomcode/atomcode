//! Default self inspection stays compact while explicit details remain reachable.
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::{
    plugins,
    seams::{Aspect, OperationsSvc, ToolsSvc},
};
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use atomcode_plexus::{App, ConfigTree, Layer};
use serde_json::{json, Value};

struct VerboseTool {
    name: String,
    description: String,
}
#[async_trait]
impl Tool for VerboseTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _: &str, _: &ToolContext) -> ToolResult {
        unreachable!()
    }
}

async fn assembly() -> App {
    let layer = Layer::from_toml(
        r#"
[[insert]]
name = "operations"
[[insert]]
name = "commands"
[[insert]]
name = "tools"
[[insert]]
name = "system-prompt"
[[insert]]
name = "self-knowledge"
"#,
    )
    .unwrap();
    let mut app = App::new(
        plugins::catalog(),
        ConfigTree::from_layers([layer]).unwrap(),
    );
    app.start().await.unwrap();
    let tools = app.context().service::<ToolsSvc>().unwrap();
    for i in 0..96 {
        tools
            .register(Arc::new(VerboseTool {
                name: format!("fixture_{i}"),
                // Exactly the failure: a long description without a newline.
                description: format!(
                    "DETAILED TOOL DOCUMENTATION {}",
                    "long description ".repeat(30)
                ),
            }))
            .unwrap();
    }
    app.context()
        .service::<OperationsSvc>()
        .unwrap()
        .contribute(
            Aspect::Operations,
            "fixture-model",
            0,
            "LIVE MODEL: fixture-model. DETAILED SETUP DOCUMENTATION".repeat(200),
        );
    app
}

async fn describe(app: &App, args: &str) -> String {
    let result = app
        .context()
        .service::<ToolsSvc>()
        .unwrap()
        .get("describe_self")
        .unwrap()
        .execute(
            args,
            &ToolContext {
                working_dir: std::env::temp_dir(),
                cancel: Default::default(),
                progress: Default::default(),
                requester: None,
            },
        )
        .await;
    assert!(!result.is_error);
    result.content
}

#[tokio::test]
async fn default_and_invalid_requests_do_not_expand_tool_or_setup_documentation() {
    let app = assembly().await;
    let overview = describe(&app, "{}").await;
    assert!(overview.contains("fixture_95"));
    assert!(overview.contains("aspect: session"));
    assert!(!overview.contains("DETAILED TOOL DOCUMENTATION"));
    assert!(!overview.contains("DETAILED SETUP DOCUMENTATION"));
    for args in [
        "",
        "not json",
        r#"{"aspect":"typo"}"#,
        r#"{"aspect":"overview"}"#,
    ] {
        assert_eq!(describe(&app, args).await, overview);
    }
    let all = describe(&app, r#"{"aspect":"all"}"#).await;
    assert!(all.contains("DETAILED TOOL DOCUMENTATION"));
    assert!(all.contains("LIVE MODEL: fixture-model"));
    assert!(
        overview.len() * 10 < all.len(),
        "overview={}, all={}",
        overview.len(),
        all.len()
    );
}

#[tokio::test]
async fn detailed_queries_still_report_live_tool_and_model_information() {
    let app = assembly().await;
    let tools = describe(&app, r#"{"aspect":"tools"}"#).await;
    assert!(tools.contains("DETAILED TOOL DOCUMENTATION"));
    let operations = describe(&app, r#"{"aspect":"operations"}"#).await;
    assert!(operations.contains("LIVE MODEL: fixture-model"));
    app.context()
        .service::<ToolsSvc>()
        .unwrap()
        .register(Arc::new(VerboseTool {
            name: "added_after_mount".into(),
            description: "late tool details".into(),
        }))
        .unwrap();
    assert!(describe(&app, "{}").await.contains("added_after_mount"));
    assert!(describe(&app, r#"{"aspect":"tools"}"#)
        .await
        .contains("late tool details"));
}
