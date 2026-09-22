//! 调用说明：模型每次工具调用都用一句话讲清这次调用的**目的**。
//!
//! 屏幕上的工具行一直是把参数里某个键猜出来当"主体"（`content.rs` 的
//! `subject_of`）：`── ReadFile(a.rs)` 里的 `a.rs` 是猜的，不是模型说的。这一行
//! 让模型自己说——"读这个文件是为了找 X"——TUI 把这句话画在调用自己的第一行上。
//!
//! ## 为什么是一个参数，而不是一个新字段
//!
//! `intent` 走的是 `ToolCall.arguments` 这条**既有**管道：模型把它当普通参数吐出来，
//! 于是它自动出现在 `AssistantMessage.tool_calls` 里、进会话日志、被 transcript
//! 折成工具块。全程没有一个新字段：kernel 的 `ToolCall`、`SessionEvent`、
//! `format_version` 都不动，daemon/ACP 的 wire 投影也不动。
//!
//! ## 为什么执行前要剥掉
//!
//! 剥掉不是洁癖，是因为底下有一批**按整串参数扫描**的闸门，说明留在里面会被误读：
//!
//! - `sensitive_path::references_sensitive_path` 对整串 args 做**裸子串**匹配，标记表
//!   里有 `/.ssh`、`id_rsa`、`/.aws`、`.env"`、`.pem`……一句"为了看 .env 怎么加载"
//!   就能把一次普通 `read_file` 变成敏感路径审批。它的递归版
//!   (`decoded_json_references_sensitive_path`) 更是遍历**所有**字符串值，不只目标键。
//! - `bash::allow_all_group` 内部就调 `references_sensitive_path(args)`；命中则
//!   **不再提供**「本会话允许所有 Bash」。`shell_always_grant_scope` 同理，把 tool-wide
//!   授权降级成 command-scoped。
//! - MCP 适配器把**整个** arguments 转发给第三方 server（`mcp/tool.rs`），说明会作为
//!   一个对方 schema 里没有的键发出去。
//! - 吃默认 `always_grant_scope`（= 整串 args）的工具，会把说明算进授权 scope 里，
//!   于是同一件事换个说法就要重问一次。
//!
//! 在 `tools/execute-batch` 最外层摘掉之后，闸门、授权 scope、MCP 看到的参数与我们
//! 加这个功能之前**逐字节相同**。而屏幕不受影响：TUI 的工具块是从
//! `AssistantMessage.tool_calls` 建的，不是从 `ToolStarted`（`modules/transcript.rs`
//! 里 `ToolStarted` 出现 0 次），两条路天然分得开——`agent_loop` 先提交
//! `AssistantMessage`（带说明），再把 `response.tool_calls` 送进 `execute_batch`（被剥）。
//!
//! ## 已知的代价
//!
//! 说明随 `AssistantMessage` 回流给模型（history 由它派生），所以每一轮会多带上
//! 之前每次调用的那句话。这是刻意的：模型因此看得到自己上一步为什么那么做。要切断
//! 得在 `derive_messages` 上再剥一次，而那会把"可回溯"也一起切掉。
//!
//! ## 软合同
//!
//! `intent` **不进** `required`。老会话、忽略指令的弱模型、schema 不是 object 的工具
//! （少数 MCP 服务器）都可能没有它——此时 TUI 逐字节回落到今天的样子
//! （`subject_of` 猜出来的主体）。

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::events::{
    AgentRequest, ModelRequest, ModelResponse, RequestError, ToolBatch, ToolsExecuteBatch,
};
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde_json::Value;

/// The argument the model fills in on every call.
///
/// Also spelled in `atomcode-tui`'s `content.rs`, which must read it without
/// depending on this crate. Two places knowing one string is the same trade the
/// TUI already makes for tool names: guessing wrong costs a duller line, never a
/// wrong result.
pub const INTENT_ARG: &str = "intent";

/// The property's own line, read where the model fills the field in.
///
/// Short on purpose: this text is repeated once per tool in every request, so a
/// sentence here costs its length times the size of the catalog. The convention
/// itself is stated once, in [`INTENT_GUIDE`].
const INTENT_PROPERTY: &str =
    "One sentence on why you are making THIS call. Shown to the person — not to the tool.";

/// The convention, said once in the system prompt.
const INTENT_GUIDE: &str = "Every tool call carries an `intent` argument: one short sentence \
     saying WHY you are making this call — the purpose, not the mechanics, in the language the \
     person is speaking. It is shown to the person on the call's own line, so give a reason they \
     would recognise (\"finding where credentials are loaded\", not \"reading a file\"). Fill it \
     in on every call.";

pub struct ToolIntentPlugin;

#[async_trait]
impl Plugin for ToolIntentPlugin {
    fn name(&self) -> &'static str {
        "tool-intent"
    }
    fn description(&self) -> &'static str {
        "every tool call states its reason in one line, shown to the person and stripped before the call runs"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        // Appended, not prepended: a listener that adds a tool to this request
        // should have its tool covered too, and running last is what sees them.
        let _ = ctx.on_waterfall::<AgentRequest>(Arc::new(Ask), false);
        // Prepended: the reason has to be gone before ANY listener reads the
        // call's arguments, up to and including the terminal that commits
        // `ToolStarted` and hands the bytes to the tool.
        let _ = ctx.on_waterfall::<ToolsExecuteBatch>(Arc::new(Strip), true);
        // Ranked just under the per-tool paragraphs (50–56): this is the rule
        // those paragraphs are instances of.
        atomcode_harness::plugins::tools::contribute_prompt(ctx, "tool-intent", 45, INTENT_GUIDE);
        Ok(())
    }
}

/// Tells the model, once per request, that every tool takes a reason.
struct Ask;

#[async_trait]
impl Waterfall<AgentRequest> for Ask {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        for tool in &mut req.tools {
            offer(&mut tool.parameters);
        }
        next.run(req).await
    }
}

/// Offers `intent` on one tool's schema, leaving a schema it cannot amend alone.
///
/// NOT added to `required`: the contract is soft, and a tool that never receives
/// one must keep working exactly as it did (`docs/adr/0023` §7's stance on
/// guidance the model may decline).
fn offer(parameters: &mut Value) {
    let Some(map) = parameters.as_object_mut() else {
        return;
    };
    // A schema declaring another shape takes no properties. Writing one anyway
    // would hand a strict validator a key it rejects.
    if let Some(Value::String(kind)) = map.get("type") {
        if kind != "object" {
            return;
        }
    }
    let properties = map
        .entry("properties")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(properties) = properties.as_object_mut() else {
        return;
    };
    // A tool that already declares this name owns it — its meaning may be its own.
    if properties.contains_key(INTENT_ARG) {
        return;
    }
    properties.insert(
        INTENT_ARG.to_string(),
        serde_json::json!({ "type": "string", "description": INTENT_PROPERTY }),
    );
}

/// Takes the reason back off, before anything reads the call's arguments.
struct Strip;

#[async_trait]
impl Waterfall<ToolsExecuteBatch> for Strip {
    async fn handle(
        &self,
        batch: &mut ToolBatch,
        next: Next<'_, ToolsExecuteBatch>,
    ) -> Vec<ToolResult> {
        for call in &mut batch.calls {
            take(&mut call.arguments);
        }
        next.run(batch).await
    }
}

/// One call's arguments without the reason in them.
fn take(arguments: &mut String) {
    let Ok(mut value) = serde_json::from_str::<Value>(arguments) else {
        return;
    };
    let Some(map) = value.as_object_mut() else {
        return;
    };
    if map.remove(INTENT_ARG).is_none() {
        return;
    }
    match serde_json::to_string(&value) {
        Ok(text) => *arguments = text,
        // Serializing a value that just came out of the parser cannot fail. If
        // it somehow did, leaving the call as the model wrote it is the safe
        // direction: the tool ignores the extra key.
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{offer, take, INTENT_ARG};

    fn args(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn a_reason_is_offered_on_an_ordinary_schema() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": { "file_path": { "type": "string" } },
            "required": ["file_path"]
        });
        offer(&mut schema);
        assert_eq!(schema["properties"][INTENT_ARG]["type"], "string");
        // Soft: the tool's own requirement is untouched, and the reason joins none.
        assert_eq!(schema["required"], serde_json::json!(["file_path"]));
    }

    #[test]
    fn an_empty_schema_still_gets_one() {
        // `list_directory`-shaped tools and MCP tools whose server declared
        // nothing: `{}` is still an object schema the model fills in.
        let mut schema = serde_json::json!({});
        offer(&mut schema);
        assert_eq!(schema["properties"][INTENT_ARG]["type"], "string");
    }

    #[test]
    fn a_schema_that_is_not_an_object_is_left_alone() {
        // Writing `properties` onto an array schema hands a strict validator a
        // key its own schema rejects.
        let mut schema = serde_json::json!({ "type": "array", "items": { "type": "string" } });
        let before = schema.clone();
        offer(&mut schema);
        assert_eq!(schema, before);
    }

    #[test]
    fn a_tool_that_already_means_something_by_the_name_keeps_it() {
        // None does today; the guard is here so that the day one does, this row
        // is not the thing that silently overwrote its meaning.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": { INTENT_ARG: { "type": "string", "enum": ["plan", "act"] } }
        });
        offer(&mut schema);
        assert_eq!(
            schema["properties"][INTENT_ARG]["enum"],
            serde_json::json!(["plan", "act"])
        );
    }

    #[test]
    fn the_reason_is_taken_back_off_before_the_call_runs() {
        let mut arguments =
            args(r#"{"file_path":"a.rs","intent":"finding where credentials are loaded"}"#);
        take(&mut arguments);
        let parsed: serde_json::Value = serde_json::from_str(&arguments).unwrap();
        assert!(parsed.get(INTENT_ARG).is_none(), "{arguments}");
        assert_eq!(parsed["file_path"], "a.rs", "{arguments}");
    }

    #[test]
    fn a_call_without_a_reason_keeps_the_bytes_it_had() {
        // The negative control for the strip: a model that ignored the guide, or
        // an old log replayed, must reach the tool exactly as it always did.
        let original = args(r#"{"file_path":"a.rs"}"#);
        let mut arguments = original.clone();
        take(&mut arguments);
        assert_eq!(arguments, original);
    }

    #[test]
    fn arguments_that_are_not_json_are_left_alone() {
        // A weak model's broken args are the repair row's business, not this
        // one's; touching them here would remove the parse error it reports.
        let mut arguments = args("{file_path: a.rs");
        let before = arguments.clone();
        take(&mut arguments);
        assert_eq!(arguments, before);
    }

    #[test]
    fn a_non_string_reason_is_still_taken_off() {
        // Whatever the model put there, it is not a tool argument.
        let mut arguments = args(r#"{"command":"ls","intent":{"why":"because"}}"#);
        take(&mut arguments);
        assert_eq!(arguments, r#"{"command":"ls"}"#);
    }
}
