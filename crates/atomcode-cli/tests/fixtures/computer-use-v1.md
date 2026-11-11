---
name: computer-use
auto-load: computer-use
description: Operate a user-selected macOS application through AtomCode's explicitly enabled computer-use MCP server; inspect current state, make scoped changes, and verify the result.
---

Use the tools exposed by the `computer-use` MCP server. Do not invoke a nested agent or assume Codex-only computer tools exist.

1. Look for the `computer-use` server's tools in your available tool catalog or the runtime's normal MCP tool discovery. `/mcp` is a human TUI command, not a tool you can execute. A loaded skill does not prove the backend is connected. If tools are unavailable, stop and ask the user to inspect `/mcp`; a paused server needs Resume, not reinstallation. For missing setup or permissions, explain `/computer doctor` and `/computer enable` (or their `atomcode computer` terminal equivalents). Do not install software, grant OS permissions or bypass a disabled server implicitly. The integration currently targets macOS 14 or later and a directly vision-capable model for screenshot tasks.
2. Use `list_apps` only to identify the application requested by the user. Observe that app with `get_app_state`, or use `list_windows` and `get_window_state` to select the exact window. Do not inspect unrelated application contents.
3. Use element IDs from the latest observation. Coordinate actions require that window's latest screenshot and its screenshot ID. Respect the backend's pixel-to-window mapping; never invent IDs, scale coordinates blindly, or reuse a screenshot after the window changes.
4. Prefer element-targeted operations and editable-control values. Re-observe the same target after each action or error, and verify the requested state instead of trusting a successful click alone.
5. Honor the user's existing task authorization and AtomCode's approvals. Ask only when an action falls outside that authorization or an existing approval requires it. Keep global pointer, foreground input, focus and launch opt-in flags disabled; do not work around backend refusals with shell automation.
6. On timeout, disconnection or cancellation, stop issuing actions. Already dispatched effects may have occurred: re-observe and reconcile before retrying. Never replay an unknown-outcome send, purchase, deletion or submission. Code rewind cannot undo external GUI effects.

`atomcode computer status` reads configuration. `doctor` checks permissions without opening onboarding. `disable` changes configuration for future/reloaded sessions; it does not guarantee interruption of an already dispatched operation. The user manages macOS Accessibility and Screen Recording permissions for the backend application.
