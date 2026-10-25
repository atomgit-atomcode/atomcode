//! Lead-only host automation tools. Storage and OS operations are injected.
use super::approval::{ApprovalRequest, PermissionDecision};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

pub trait AutomationService: Send + Sync {
    fn call(
        &self,
        operation: &str,
        args: Value,
        working_dir: &Path,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Value, String>;
}
pub fn automation_tools(service: Arc<dyn AutomationService>) -> Vec<Arc<dyn Tool>> {
    [
        "schedule_create",
        "schedule_list",
        "schedule_update",
        "schedule_delete",
    ]
    .into_iter()
    .map(|name| {
        Arc::new(AutomationTool {
            name,
            service: service.clone(),
        }) as Arc<dyn Tool>
    })
    .collect()
}
struct AutomationTool {
    name: &'static str,
    service: Arc<dyn AutomationService>,
}
#[async_trait]
impl Tool for AutomationTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        match self.name {
            "schedule_create" => "Create an OS-scheduled independent task for the current project. Each occurrence starts a fresh session using the host default model. Uses execution-host local time, not browser time. If another time zone is requested, explain that only host-local scheduling is supported; do not silently approximate recurring DST rules. Requires explicit human approval; never claim success without a saved result. Use plan permissions by default; cron and one-shot rules are not supported.",
            "schedule_list" => "List this project's persisted automation tasks with revisions, registration status and estimated next time. Read before editing; registration error means the task is saved but may not run.",
            "schedule_update" => "Update a persisted automation task using its id and exact observed revision from schedule_list. Supply a complete task record retaining its id and cwd. Requires explicit approval; conflicts require a fresh read, not a blind retry.",
            _ => "Delete a scheduled task using its id and exact observed revision. Stops future triggers, retains execution sessions and run history, and does not cancel an active run. Requires explicit human approval.",
        }
    }
    fn parameters_schema(&self) -> Value {
        let rule = json!({"anyOf":[
            {"type":"object","properties":{"kind":{"type":"string","enum":["daily"]},"time":{"type":"string","description":"HH:MM in host local time"}},"required":["kind","time"],"additionalProperties":false},
            {"type":"object","properties":{"kind":{"type":"string","enum":["weekly"]},"time":{"type":"string"},"weekday":{"type":"integer","minimum":1,"maximum":7}},"required":["kind","time","weekday"],"additionalProperties":false},
            {"type":"object","properties":{"kind":{"type":"string","enum":["hourly"]}},"required":["kind"],"additionalProperties":false},
            {"type":"object","properties":{"kind":{"type":"string","enum":["interval"]},"every_minutes":{"type":"integer","minimum":1}},"required":["kind","every_minutes"],"additionalProperties":false}
        ]});
        let task_schema = json!({"type":"object","properties":{
            "id":{"type":"string"},"title":{"type":"string"},"prompt":{"type":"string"},"cwd":{"type":"string"},"schedule":rule.clone(),
            "permission_mode":{"type":"string","enum":["plan","accept_edits","auto"]},"notify":{"type":"string","enum":["off","important","all"]},
            "enabled":{"type":"boolean"},"created_at":{"type":"integer"},"last_run_at":{"type":["integer","null"]},"last_status":{"type":["string","null"]}
        },"required":["id","title","prompt","cwd","schedule","permission_mode","notify","enabled","created_at","last_run_at","last_status"],"additionalProperties":false});
        match self.name {
            "schedule_list" => {
                json!({"type":"object","properties":{},"additionalProperties":false})
            }
            "schedule_create" => {
                json!({"type":"object","properties":{"title":{"type":"string"},"prompt":{"type":"string"},"schedule":rule,"permission_mode":{"type":"string","enum":["plan","accept_edits"]}},"required":["title","prompt","schedule"],"additionalProperties":false})
            }
            "schedule_update" => {
                json!({"type":"object","properties":{"task":task_schema,"expected":{"type":"string"}},"required":["task","expected"],"additionalProperties":false})
            }
            _ => {
                json!({"type":"object","properties":{"id":{"type":"string"},"expected":{"type":"string"}},"required":["id","expected"],"additionalProperties":false})
            }
        }
    }
    fn risk(&self, _: &str) -> RiskLevel {
        // Writes perform their own mandatory per-change consent below. A generic
        // risk prompt would ask twice and its session grant/Auto bypass cannot
        // authorize persistent work. Coding plan policy explicitly blocks these persistent mutations.
        RiskLevel::Safe
    }
    fn read_only_hint(&self) -> bool {
        self.name == "schedule_list"
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let result = async {
            let value: Value=serde_json::from_str(args).map_err(|e|e.to_string())?;
            if self.name!="schedule_list" {
                let mut payload=serde_json::to_value(ApprovalRequest {call_id:ctx.progress.source_id().unwrap_or("").to_string(),tool:self.name.into(),args:args.into(),reason:Some(format!("This changes persistent background automation in {}. Approval applies to this exact change, not to arbitrary future tool permissions.",ctx.working_dir.display())),allow_all_bash:false}).map_err(|e|e.to_string())?;
                payload["requires_explicit_approval"] = Value::Bool(true);
                let reply=tokio::select! { _=ctx.cancel.cancelled()=>return Err("cancelled".into()), reply=ctx.request("approval",payload)=>reply };
                if !matches!(PermissionDecision::from_value(&reply),PermissionDecision::AllowOnce|PermissionDecision::AllowAlways) {return Err("automation change was not approved".into());}
            }
            if ctx.cancel.is_cancelled() {return Err("cancelled".into());}
            let service=self.service.clone(); let operation=self.name;let cwd=ctx.working_dir.clone(); let cancel=ctx.cancel.clone();
            tokio::task::spawn_blocking(move ||service.call(operation,value,&cwd,&cancel)).await.map_err(|e|e.to_string())?
        }.await;
        match result {
            Ok(value) => ToolResult {
                content: value.to_string(),
                ..Default::default()
            },
            Err(error) => ToolResult {
                content: error,
                is_error: true,
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::{event::AgentEvent, request::RequestCtx, tool::ProgressSink};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Backend(AtomicUsize);
    impl AutomationService for Backend {
        fn call(
            &self,
            operation: &str,
            _: Value,
            _: &Path,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<Value, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"operation":operation}))
        }
    }
    fn context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
            requester: None,
        }
    }
    #[tokio::test]
    async fn unattended_mutations_fail_closed_but_reading_is_available() {
        let backend = Arc::new(Backend(AtomicUsize::new(0)));
        let tools = automation_tools(backend.clone());
        for tool in tools.iter().filter(|t| t.name() != "schedule_list") {
            assert!(tool.execute("{}", &context()).await.is_error);
        }
        assert_eq!(backend.0.load(Ordering::SeqCst), 0);
        assert!(!tools[1].execute("{}", &context()).await.is_error);
        assert_eq!(backend.0.load(Ordering::SeqCst), 1);
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn consent_describes_exact_arguments_and_cancel_does_not_mutate() {
        let backend = Arc::new(Backend(AtomicUsize::new(0)));
        let tool = automation_tools(backend.clone()).remove(0);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let requests = RequestCtx::new(tx, Some(std::time::Duration::from_secs(30)));
        let mut ctx = context();
        ctx.requester = Some(requests.requester());
        let cancel = ctx.cancel.clone();
        let task =
            tokio::spawn(async move { tool.execute("{\"title\":\"Daily review\"}", &ctx).await });
        let AgentEvent::Request { kind, payload, .. } = rx.recv().await.unwrap() else {
            panic!("expected approval")
        };
        assert_eq!(kind, "approval");
        assert_eq!(payload["requires_explicit_approval"], true);
        assert_eq!(payload["args"], "{\"title\":\"Daily review\"}");
        cancel.cancel();
        assert!(task.await.unwrap().is_error);
        assert_eq!(backend.0.load(Ordering::SeqCst), 0);
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn explicit_consent_admits_one_change() {
        let backend = Arc::new(Backend(AtomicUsize::new(0)));
        let tool = automation_tools(backend.clone()).remove(0);
        let mut ctx = context();
        ctx.requester = Some(atomcode_kernel::request::Requester::from_fn(Arc::new(
            |kind, _payload| {
                assert_eq!(kind, "approval");
                Box::pin(async { json!({"decision":"allow"}) })
            },
        )));
        assert!(!tool.execute("{}", &ctx).await.is_error);
        assert_eq!(backend.0.load(Ordering::SeqCst), 1);
    }
}
