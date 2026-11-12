> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

# Computer Use integration review and acceptance

Review baseline: release/v5.2.7, `1d946853286c98d2e7ec05b3b19b22ef18edb5bb`. Reviewed the retained image adapter, result DTOs, feature isolation, setup/probe/config ownership, skill classifier/static injection, addressed host control, TUI commands and their tests. Existing feature edits were preserved. The user's continuation follows the proposed review/commit and real-acceptance sequence; commits stay local unless pushing is separately requested.

## Finding fixed

The installed skill told the model to check `/mcp`, a human-only TUI command. A model cannot execute that command. The skill now checks its available catalog or normal MCP discovery, treats a loaded skill as guidance rather than proof of a live connection, and tells the user to inspect `/mcp` only when tools are unavailable. Paused tools need Resume; missing setup/permissions need diagnosis. The guidance forbids implicit reinstallation, OS grants or bypassing disabled tools. No tool approval policy changed.

No additional blocking issue was found within the implemented scope. This is not certification of strong desktop isolation: enforced application scope, cross-agent desktop exclusivity and interruption of dispatched remote actions remain unimplemented and explicitly documented. A normal MCP readOnlyHint still follows the product's existing read-only policy; trust=false/empty autoApprove do not invent a stricter read-only approval policy.

## Real backend acceptance

Inspected Open Computer Use npm package version 1.3.0, explicitly unpacked in a temporary directory, not globally installed. The real stdio server accepted the product's initialize request and negotiated **2025-03-26**. It exposes 15 tools. Tool schemas confirm window handles, screenshotId freshness, screenshot inclusion, and targeted keyboard/text input.

Added an opt-in ignored integration test in `crates/atomcode-capabilities/tests/mcp.rs` that goes through the production McpRegistry and McpToolAdapter. It sends initialize/tools/list only, checks required tools and schemas, and verifies click remains Risky without trust grants. It does not send tools/call. Reproduce on macOS with an explicitly installed executable:

```sh
ATOMCODE_COMPUTER_TEST_EXECUTABLE=/absolute/path/to/open-computer-use \
  cargo nextest run -p atomcode-capabilities --features mcp --test mcp \
  --run-ignored ignored-only -E 'test(installed_computer_backend)'
```

The real registry test passed. The 13 local setup/startup tests passed. Existing broader results and clean-HEAD baseline failures remain recorded in the image, backend and commands plans; they are not described as an unconditional green full suite.

A fresh `doctor --no-ui` reports Screen Recording=false and Accessibility=false. No application contents, screenshots or GUI input were requested. Real screenshot → model interpretation → input → observed-result acceptance is still pending OS permissions and a controlled target fixture. The user must grant the backend app's permissions themselves; the agent did not grant them or install a global entry.

After installing the backend explicitly, the remaining acceptance sequence is: run its permission onboarding, run `atomcode computer doctor`, enable, reload, then use a controlled test document/window. Avoid unrelated application content. Check an observed fresh screenshot, a harmless edit to that document, the next observation, and a denied approval. Do not treat disable or code rewind as an undo/stop guarantee for GUI effects.
