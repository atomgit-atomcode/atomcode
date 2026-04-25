# P2: /doctor + /review + Notebook + TodoWrite

Date: 2026-04-23

## 1. /doctor

Slash command. No AgentLoop involvement — pure TUI-side diagnostics.

Checks:
- Provider: send a minimal API call (list models or 1-token completion), report latency or error
- MCP: read from `mcp::get_statuses()`
- Settings: load and report allow/deny/hook counts
- Git: run `git status --porcelain` + `git branch --show-current`
- Project: check `.atomcode.md` existence
- Tools: count registered tools from ToolRegistry (passed via LoopCtx or static)

Output format: `✓`/`✗` per component with detail.

Files: `commands.rs` (slash handler), `commands.rs` (register in help)

## 2. /review

Slash command that reads `git diff` and sends it as a review prompt to the agent.

Implementation:
1. Run `git diff` (or `git diff --staged` if arg is `--staged`)
2. If diff is empty, show "No changes to review"
3. Construct prompt: `"Review the following code changes for bugs, security issues, and improvements:\n\n```diff\n{diff}\n```"`
4. Send as `AgentCommand::SendMessage(prompt)`
5. Agent streams the review as normal text response

Files: `commands.rs` (slash handler + register)

## 3. Notebook (.ipynb) support

Add `.ipynb` parsing to `read_file` tool. No new files — extend existing `read.rs`.

When `read_file` encounters a `.ipynb` file:
1. Parse as JSON (`serde_json::Value`)
2. Extract `cells` array
3. For each cell: render `cell_type` (code/markdown), `source` lines, and `outputs` (text/plain only)
4. Return formatted text like:
```
[Cell 1 - code]
import pandas as pd
df = pd.read_csv("data.csv")

[Output]
     name  age
0   Alice   30
1     Bob   25

[Cell 2 - markdown]
# Analysis
This notebook analyzes...
```

Files: `crates/atomcode-core/src/tool/read.rs` (add ipynb handler before binary fallback)

## 4. TodoWrite tool

New tool `todo` registered in ToolRegistry. In-memory task list per session.

```rust
pub struct TodoTool {
    items: Arc<Mutex<Vec<TodoItem>>>,
}

struct TodoItem {
    id: usize,
    content: String,
    status: TodoStatus, // Pending | InProgress | Completed
}
```

Actions: `add`, `update`, `complete`, `list`
- `add`: push new item, return id
- `update`: change status to in_progress
- `complete`: mark done
- `list`: return all items with status

LLM uses this to track multi-step tasks. `/todo` slash command shows current list.

Files: `crates/atomcode-core/src/tool/todo.rs` (new), `tool/mod.rs`, `cli/main.rs` (register), `commands.rs` (slash /todo)
