//! Kernel `Tool` adapter — surfaces a discovered MCP tool as a neutral kernel
//! `Tool`. Replaces core's `tool_adapter.rs` (which targeted the core Tool trait
//! with its `definition()`/`approval()` model). Here the adapter speaks the kernel
//! trait directly: split metadata accessors, `risk()` instead of `approval()`, and
//! a non-`Result` `execute` that maps every failure to `ToolResult { is_error: true }`
//! (the kernel PANIC CONTRACT: a tool must never panic).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};

use super::client::McpToolInfo;
use super::registry::McpRegistry;

/// Replace every character that OpenAI/litellm forbids in a `function.name`
/// (anything outside `[a-zA-Z0-9_-]`) with `-`. Real MCP servers routinely
/// declare server/tool names containing spaces, dots, colons or CJK characters;
/// unsanitized they break the whole request with a 400
/// `invalid_request_error` (see issue #1289).
pub fn sanitize_name_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The name the LLM sees for an MCP tool. Keeps the `mcp__{server}__{tool}`
/// shape while making every segment a valid OpenAI function-name fragment.
pub fn mcp_tool_full_name(server: &str, tool: &str) -> String {
    format!(
        "mcp__{}__{}",
        sanitize_name_segment(server),
        sanitize_name_segment(tool)
    )
}

/// Wraps one MCP tool (`server` + `tool`) as a kernel `Tool`. The LLM sees it as
/// `mcp__{server}__{tool}`; calls route through the shared [`McpRegistry`].
pub struct McpToolAdapter {
    registry: Arc<McpRegistry>,
    server: String,
    tool: String,
    full_name: String,
    description: String,
    schema: serde_json::Value,
    /// Server-declared `annotations.readOnlyHint` — a read-only tool has no side
    /// effects, so it is `Safe` (no approval) and allowed during plan mode.
    read_only: bool,
}

impl McpToolAdapter {
    /// Build an adapter from a discovered tool's [`McpToolInfo`] and the live
    /// registry that owns the server connection.
    pub fn new(registry: Arc<McpRegistry>, info: McpToolInfo) -> Self {
        let full_name = mcp_tool_full_name(&info.server_name, &info.tool_name);
        let description = if info.description.is_empty() {
            format!(
                "MCP tool from server '{}'. See input schema for details.",
                info.server_name
            )
        } else {
            format!("[MCP:{}] {}", info.server_name, info.description)
        };
        Self {
            registry,
            server: info.server_name,
            tool: info.tool_name,
            full_name,
            description,
            schema: info.input_schema,
            read_only: info.read_only,
        }
    }

    /// The mounted name (`mcp__{server}__{tool}`).
    pub fn full_name(&self) -> &str {
        &self.full_name
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    /// MCP servers are external code, so `Risky` by default — the approval middleware
    /// gates the call (the kernel never sandboxes). EXCEPTION: a server configured
    /// `trust: true`, or a tool on its `autoApprove` allowlist (or granted "Always"),
    /// is `Safe` so it skips the prompt — this is what makes `.mcp.json` trust actually
    /// take effect (it was silently ignored before). The `mcp__{server}__{tool}` name
    /// conveys the origin to the prompt when it does fire.
    fn risk(&self, _args: &str) -> RiskLevel {
        // A server-declared read-only tool has no side effects → Safe (no approval),
        // matching codex's `readOnlyHint` handling. Otherwise trust/auto-approve decide.
        if self.read_only
            || self.registry.is_server_trusted(&self.server)
            || self.registry.is_tool_auto_approved(&self.full_name)
        {
            RiskLevel::Safe
        } else {
            RiskLevel::Risky
        }
    }

    /// Server-declared `annotations.readOnlyHint`. Plan mode reads this to allow
    /// read-only external queries (distinct from `risk()`, which also folds in trust).
    fn read_only_hint(&self) -> bool {
        self.read_only
    }

    /// "Always" approves THIS tool (`mcp__{server}__{tool}`) regardless of the call's
    /// arguments — an empty scope keys the grant on the tool name alone. Without this
    /// the default per-args grant means a differing query re-prompts every time.
    fn always_grant_scope(&self, _args: &str) -> String {
        String::new()
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let trimmed = args.trim();
        let arguments: serde_json::Value = if trimmed.is_empty() || trimmed == "{}" {
            json!({})
        } else {
            match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    return ToolResult {
                        call_id: String::new(),
                        content: format!("invalid MCP tool arguments: {e}"),
                        is_error: true,
                        images: vec![],
                    };
                }
            }
        };

        match self
            .registry
            .call_tool(&self.server, &self.tool, arguments)
            .await
        {
            Ok(content) => ToolResult {
                call_id: String::new(),
                content,
                is_error: false,
                images: vec![],
            },
            Err(e) => ToolResult {
                call_id: String::new(),
                content: e.to_string(),
                is_error: true,
                images: vec![],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(server: &str, tool: &str) -> McpToolInfo {
        McpToolInfo {
            server_name: server.to_string(),
            tool_name: tool.to_string(),
            description: String::new(),
            input_schema: json!({}),
            read_only: false,
        }
    }

    #[test]
    fn trusted_server_makes_its_tools_safe() {
        let reg = Arc::new(McpRegistry::new());
        reg.mark_server_trusted("docs");
        let adapter = McpToolAdapter::new(reg, info("docs", "query"));
        assert_eq!(adapter.risk("{}"), RiskLevel::Safe);
    }

    #[test]
    fn untrusted_server_tool_is_risky() {
        let reg = Arc::new(McpRegistry::new());
        let adapter = McpToolAdapter::new(reg, info("docs", "query"));
        assert_eq!(adapter.risk("{}"), RiskLevel::Risky);
        assert!(
            !adapter.read_only_hint(),
            "unannotated tool is not read-only"
        );
    }

    #[test]
    fn read_only_hint_tool_is_safe_even_when_untrusted() {
        // A server-declared read-only tool (readOnlyHint: true) has no side effects →
        // Safe (skips approval) and exposes read_only_hint() for plan mode.
        let reg = Arc::new(McpRegistry::new());
        let mut ro = info("docs", "query");
        ro.read_only = true;
        let adapter = McpToolAdapter::new(reg, ro);
        assert_eq!(adapter.risk("{}"), RiskLevel::Safe);
        assert!(adapter.read_only_hint());
    }

    #[test]
    fn auto_approved_tool_is_safe_but_only_that_tool() {
        let reg = Arc::new(McpRegistry::new());
        reg.mark_tool_auto_approved("mcp__docs__query");
        assert_eq!(
            McpToolAdapter::new(reg.clone(), info("docs", "query")).risk("{}"),
            RiskLevel::Safe
        );
        // A different tool from the same server is NOT covered.
        assert_eq!(
            McpToolAdapter::new(reg, info("docs", "search")).risk("{}"),
            RiskLevel::Risky
        );
    }

    #[test]
    fn always_grant_scope_is_tool_wide_not_per_args() {
        let adapter = McpToolAdapter::new(Arc::new(McpRegistry::new()), info("docs", "query"));
        // Same scope regardless of args → "Always" persists across differing calls.
        assert_eq!(
            adapter.always_grant_scope(r#"{"q":"a"}"#),
            adapter.always_grant_scope(r#"{"q":"b"}"#)
        );
        assert_eq!(adapter.always_grant_scope("{}"), "");
    }

    /// The mounted full name must be a valid OpenAI function name, i.e. only
    /// `[a-zA-Z0-9_-]` (litellm rejects any other character with a 400
    /// `invalid_request_error`). Real MCP servers routinely declare server/tool
    /// names with spaces, colons, dots — see issue #1289 where a server with
    /// such a name broke the whole request.
    #[test]
    fn full_name_sanitizes_characters_opena_llm_rejects() {
        let adapter = McpToolAdapter::new(
            Arc::new(McpRegistry::new()),
            info("docs w/ spaces", "query#result"),
        );
        let name = adapter.full_name();
        // No character outside the allowed set may survive into the function name.
        for ch in name.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '_' || ch == '-',
                "unexpected char {ch:?} in {name:?}"
            );
        }
        assert!(name.starts_with("mcp__"), "must keep mcp__ prefix: {name}");
    }

    /// Chinese / other non-ASCII server or tool names (seen with CJK-region MCP
    /// servers) must likewise sanitize to a valid OpenAI function name.
    #[test]
    fn non_ascii_names_are_sanitized() {
        let adapter = McpToolAdapter::new(
            Arc::new(McpRegistry::new()),
            info("文档服务", "读取文件"),
        );
        let name = adapter.full_name();
        assert!(name.is_ascii(), "must be pure ASCII: {name}");
        for ch in name.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '_' || ch == '-',
                "unexpected char {ch:?} in {name:?}"
            );
        }
    }
}
