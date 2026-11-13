//! Default-on DAP debugging with lazy local adapter discovery. The prepared tool owns its session; no kernel state,
//! persisted handles, global process pool, or implicit attachment.
mod client;
mod discovery;
use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use client::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Default)]
pub struct Adapter {
    pub command: String,
    pub args: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct Settings {
    pub enabled: bool,
    pub adapters: BTreeMap<String, Adapter>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            adapters: BTreeMap::new(),
        }
    }
}

pub struct DebugTool {
    settings: Settings,
    session: Mutex<Option<Client>>,
}
impl DebugTool {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            session: Mutex::new(None),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Launch,
    Status,
    Wait,
    SetBreakpoints,
    Threads,
    StackTrace,
    Scopes,
    Variables,
    Evaluate,
    Continue,
    StepOver,
    StepIn,
    StepOut,
    Pause,
    Terminate,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Breakpoint {
    file: String,
    line: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: Action,
    session_id: Option<u64>,
    stop_id: Option<u64>,
    adapter: Option<String>,
    program: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    breakpoints: Vec<Breakpoint>,
    file: Option<String>,
    #[serde(default)]
    lines: Vec<u32>,
    thread_id: Option<i64>,
    frame_id: Option<i64>,
    variables_reference: Option<i64>,
    expression: Option<String>,
    start: Option<u32>,
    count: Option<u32>,
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for DebugTool {
    fn name(&self) -> &str {
        "debug"
    }
    fn description(&self) -> &str {
        "Debug runtime bugs or failing tests with a local DAP adapter when runtime evidence is needed. launch takes program, args and optional breakpoints [{file,line}]; auto-detects Python/debugpy or LLDB. Compile Rust/C/C++ with debug info first. Use status for adapter/dependency diagnostics. After launch/continue/step, wait for a stop, then threads -> stack_trace -> scopes -> variables. Except launch/status, actions require session_id; stack_trace/scopes/variables/evaluate and step/continue require current stop_id. Frame/variable references expire on resume. set_breakpoints replaces all breakpoints for file; lines:[] clears. start/count paginate (max 50). evaluate executes code and may mutate the target; all actions require execution approval. status includes output tail; terminate ends the session. Timeouts, cancellation, protocol errors and capability/session rebuild close it; provider-only switches preserve it. Local stdio only; no attach or automatic adapter installation. Do not launch merely because a task finished."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"required":["action"],"properties":{
            "action":{"enum":["launch","status","wait","set_breakpoints","threads","stack_trace","scopes","variables","evaluate","continue","step_over","step_in","step_out","pause","terminate"]},
            "session_id":{"type":"integer","minimum":1},"stop_id":{"type":"integer","minimum":1},
            "adapter":{"type":"string","enum":discovery::ids(&self.settings)},"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},
            "breakpoints":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["file","line"],"properties":{"file":{"type":"string"},"line":{"type":"integer","minimum":1}}}},
            "file":{"type":"string"},"lines":{"type":"array","items":{"type":"integer","minimum":1}},
            "thread_id":{"type":"integer"},"frame_id":{"type":"integer"},"variables_reference":{"type":"integer","minimum":1},
            "expression":{"type":"string"},"start":{"type":"integer","minimum":0},"count":{"type":"integer","minimum":1,"maximum":50},
            "timeout_secs":{"type":"integer","minimum":1,"maximum":30}
        }})
    }
    // Even variables may execute getters/pretty-printers. No read-only bypass.
    fn risk(&self, _: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let args: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("debug: {e}")),
        };
        if !self.settings.enabled {
            return err("debug is disabled");
        }
        let timeout = args.timeout_secs.unwrap_or(15).clamp(1, 30);
        let mut slot = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return err("debug cancelled"),
            guard = self.session.lock() => guard,
        };
        // Refuse stale/missing ids BEFORE taking ownership; an obsolete call must
        // never destroy a different session. launch never implicitly replaces one.
        if matches!(args.action, Action::Launch) && slot.is_some() {
            return err("debug session already exists; terminate it before launching another");
        }
        if !matches!(args.action, Action::Launch | Action::Status)
            && (slot.is_none() || args.session_id != slot.as_ref().map(|s| s.id))
        {
            return err("debug: no matching session; use status or launch");
        }
        let mut session = slot.take();
        let result = {
            let operation = self.run(&args, ctx, &mut session);
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => Err("cancelled; debug session terminated".into()),
                result = tokio::time::timeout(Duration::from_secs(timeout), operation) =>
                    result.unwrap_or_else(|_| Err("timeout; debug session terminated".into())),
            }
        };
        match result {
            Ok(value) => {
                *slot = session;
                ok(value.to_string())
            }
            Err(e) => {
                drop(session);
                err(format!("debug: {e}; session closed"))
            }
        }
    }
}

impl DebugTool {
    async fn run(
        &self,
        args: &Args,
        ctx: &ToolContext,
        session: &mut Option<Client>,
    ) -> Result<Value, String> {
        if matches!(args.action, Action::Launch) {
            let program = args
                .program
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("program is required")?;
            let (adapter_id, adapter) = discovery::resolve(
                &self.settings,
                args.adapter.as_deref(),
                program,
                &ctx.working_dir,
            )
            .await?;
            static NEXT_ID: AtomicU64 = AtomicU64::new(1);
            let mut client = Client::spawn(
                &adapter.command,
                &adapter.args,
                &ctx.working_dir,
                NEXT_ID.fetch_add(1, Ordering::Relaxed),
            )?;
            client.capabilities = client
                .request(
                    "initialize",
                    json!({"clientID":"atomcode","adapterID":adapter_id,
                "pathFormat":"path","linesStartAt1":true,"columnsStartAt1":true,
                "supportsRunInTerminalRequest":false,"supportsStartDebuggingRequest":false,
                "supportsVariablePaging":true}),
                )
                .await?;
            // launch can respond only AFTER configurationDone. Do not await it here.
            let launch = client
                .send(
                    "launch",
                    json!({"program":super::resolve_path(program, &ctx.working_dir),
                "args":args.args,"cwd":ctx.working_dir,"noDebug":false,"stopOnEntry":false,
                "console":"internalConsole","subProcess":false}),
                )
                .await?;
            while !client.initialized {
                client.pump().await?;
                client.check_request_failure(launch)?;
            }
            let mut files: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
            for bp in &args.breakpoints {
                if bp.line == 0 {
                    return Err("breakpoint lines start at 1".into());
                }
                files.entry(&bp.file).or_default().push(bp.line);
            }
            let mut breakpoints = Vec::new();
            for (file, lines) in files {
                let response = set_breakpoints(&mut client, file, &lines, ctx).await?;
                breakpoints.push(json!({"file":file,"result":response}));
            }
            if client.capabilities["supportsConfigurationDoneRequest"] == true {
                client.request("configurationDone", json!({})).await?;
            }
            client.response(launch).await?;
            if client.state == "starting" {
                client.state = "running";
            }
            client.drain().await?;
            let result = bounded_result(
                &mut client,
                json!({"adapter":adapter_id,"program":program,"breakpoints":breakpoints}),
            );
            *session = Some(client);
            return Ok(result);
        }
        let Some(client) = session.as_mut() else {
            let mut status = discovery::status(&self.settings, &ctx.working_dir);
            status["state"] = json!("idle");
            return Ok(status);
        };
        client.drain().await?;
        if matches!(args.action, Action::Terminate) {
            if client.state != "terminated" {
                client
                    .request("disconnect", json!({"terminateDebuggee":true}))
                    .await?;
            }
            *session = None;
            return Ok(json!({"state":"terminated"}));
        }
        if matches!(args.action, Action::Wait) {
            while !matches!(client.state, "stopped" | "terminated") {
                client.pump().await?;
            }
            return Ok(bounded_result(client, json!({})));
        }
        if matches!(args.action, Action::Status) {
            return Ok(bounded_result(client, json!({})));
        }
        if matches!(
            args.action,
            Action::StackTrace
                | Action::Scopes
                | Action::Variables
                | Action::Evaluate
                | Action::Continue
                | Action::StepOver
                | Action::StepIn
                | Action::StepOut
        ) {
            if client.state != "stopped" || args.stop_id != Some(client.stop_id) {
                return Err(
                    "operation requires the current stopped state and stop_id; use wait/status"
                        .into(),
                );
            }
        }
        let count = args.count.unwrap_or(20).clamp(1, 50);
        let thread = args.thread_id.or(client.thread);
        let (command, parameters) = match args.action {
            Action::SetBreakpoints => {
                let file = args.file.as_deref().ok_or("file is required")?;
                let result = set_breakpoints(client, file, &args.lines, ctx).await?;
                return Ok(bounded_result(client, result));
            }
            Action::Threads => ("threads", json!({})),
            Action::StackTrace => (
                "stackTrace",
                json!({"threadId":thread.ok_or("thread_id is required")?,"startFrame":args.start.unwrap_or(0),"levels":count}),
            ),
            Action::Scopes => (
                "scopes",
                json!({"frameId":args.frame_id.ok_or("frame_id is required")?}),
            ),
            Action::Variables => (
                "variables",
                json!({"variablesReference":args.variables_reference.filter(|n| *n > 0).ok_or("variables_reference must be positive")?,"start":args.start.unwrap_or(0),"count":count}),
            ),
            Action::Evaluate => (
                "evaluate",
                json!({"expression":args.expression.as_deref().ok_or("expression is required")?,"frameId":args.frame_id.ok_or("frame_id is required")?,"context":"watch"}),
            ),
            Action::Continue
            | Action::StepOver
            | Action::StepIn
            | Action::StepOut
            | Action::Pause => {
                let command = match args.action {
                    Action::Continue => "continue",
                    Action::StepOver => "next",
                    Action::StepIn => "stepIn",
                    Action::StepOut => "stepOut",
                    _ => "pause",
                };
                if matches!(args.action, Action::Pause) && client.state != "running" {
                    return Err("pause requires a running target".into());
                }
                let thread = thread.ok_or("thread_id is required")?;
                // Invalidate references before execution; a stopped event can arrive
                // before the response, so never overwrite its state afterwards.
                client.state = "running";
                client.thread = None;
                (command, json!({"threadId":thread,"singleThread":false}))
            }
            _ => return Err("invalid debug action".into()),
        };
        let result = client.request(command, parameters).await?;
        client.drain().await?;
        Ok(bounded_result(client, result))
    }
}

async fn set_breakpoints(
    client: &mut Client,
    file: &str,
    lines: &[u32],
    ctx: &ToolContext,
) -> Result<Value, String> {
    if lines.iter().any(|line| *line == 0) {
        return Err("breakpoint lines start at 1".into());
    }
    client
        .request(
            "setBreakpoints",
            json!({"source":{"path":super::resolve_path(file, &ctx.working_dir)},
        "breakpoints":lines.iter().map(|line| json!({"line":line})).collect::<Vec<_>>()}),
        )
        .await
}

fn bounded_result(client: &mut Client, mut data: Value) -> Value {
    fn prune(value: &mut Value, depth: usize, truncated: &mut bool) {
        match value {
            Value::String(s) if s.len() > 2048 => {
                let mut end = 2048;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
                s.push_str("…");
                *truncated = true;
            }
            Value::Array(items) => {
                if depth >= 6 {
                    *value = json!("<expand on demand>");
                    *truncated = true;
                    return;
                }
                if items.len() > 50 {
                    items.truncate(50);
                    *truncated = true;
                }
                for item in items {
                    prune(item, depth + 1, truncated);
                }
            }
            Value::Object(fields) => {
                if depth >= 6 {
                    *value = json!("<expand on demand>");
                    *truncated = true;
                    return;
                }
                for item in fields.values_mut() {
                    prune(item, depth + 1, truncated);
                }
            }
            _ => {}
        }
    }
    let mut truncated = false;
    prune(&mut data, 0, &mut truncated);
    let encoded = data.to_string();
    if encoded.len() > 32768 {
        data = json!({"message":"Result exceeds 32 KiB; request fewer items or expand a specific reference."});
        truncated = true;
    }
    json!({"session":client.snapshot(),"data":data,"truncated":truncated})
}

#[cfg(test)]
mod tests;
