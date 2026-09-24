//! MCP server instructions, as part of the system prompt.
//!
//! A server may say, in its `initialize` reply, how its tools are meant to be
//! used. That guidance goes in the system prompt — the fragment registry the
//! agent loop renders every round (`atomcode_harness::seams::SystemPromptSvc`),
//! in the slot the harness's own `mcp` row uses — so it is part of what the
//! model is told, not something said to it. It used to ride each request as a
//! trailing user message, and on a round with nothing after the tool results the
//! model answered it. A request-time hook cannot put it in the system prompt
//! instead: the host keeps only what a hook appends, and drops a hook's edit to
//! anything before (`host_rows::HostMiddleware`), so such a projection is simply
//! lost.
//!
//! The fragment follows the mounted MCP tools: the `mcp-host` row refreshes it
//! whenever it publishes or withdraws them, so a server contributes guidance
//! only while at least one of its tools is in front of the model. Unchanged,
//! it is the same bytes every round, and it is never written to the session.

use atomcode_capabilities::mcp::registry::MCP_SERVER_INSTRUCTIONS_TAG;
use atomcode_capabilities::mcp::McpRegistry;
use atomcode_harness::seams::PromptRegistry;

/// The system-prompt slot: the id and rank the harness's `mcp` row contributes
/// its instructions under (it is swapped out for `mcp-host` in this product).
pub(crate) const FRAGMENT: (&str, i32) = ("mcp", 62);

/// Wrap the aggregated guidance in its untrusted boundary — the one the persona
/// tells the model how to read.
pub(crate) fn render_block(instructions: &str) -> String {
    format!("<{MCP_SERVER_INSTRUCTIONS_TAG}>\n{instructions}\n</{MCP_SERVER_INSTRUCTIONS_TAG}>")
}

/// Put in the system prompt what the servers whose tools are `mounted` ask of
/// them — or take it out when there is none.
pub(crate) fn project(prompts: &PromptRegistry, registry: &McpRegistry, mounted: &[String]) {
    let (id, rank) = FRAGMENT;
    match registry.instructions_for_mounted_tools(mounted) {
        Some(instructions) => prompts.contribute(id, rank, render_block(&instructions)),
        None => prompts.remove(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guidance_is_wrapped_in_its_untrusted_boundary() {
        let block = render_block("MCP SERVER INSTRUCTIONS\nserver-scoped guidance");
        assert!(block.starts_with("<mcp-server-instructions>\n"));
        assert!(block.ends_with("\n</mcp-server-instructions>"));
        assert!(!block.contains("<system-reminder>"));
        assert!(block.contains("server-scoped guidance"));
    }

    /// Nothing mounted, nothing said — and what was said before is taken out.
    #[test]
    fn no_mounted_guidance_leaves_no_fragment() {
        let prompts = PromptRegistry::new();
        prompts.contribute(FRAGMENT.0, FRAGMENT.1, "stale guidance");
        project(&prompts, &McpRegistry::new(), &[]);
        assert!(!prompts.ids().contains(&FRAGMENT.0.to_string()));
    }
}
