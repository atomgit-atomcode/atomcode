# AtomCode Changelog

<!--
For whoever edits this file; this block is never shown.

- The English twin of `CHANGELOG.md`: one section per release, same version
  numbers. The English UI reads this file; a release with no section here is
  shown from the Chinese file instead.
- Each section starts `## vX.Y.Z`, optionally followed by a date:
  `## v5.2.2 (2026-10-08)`. Newest first (order is by version number anyway).
- The one-line notice after an upgrade names the **bold lead** of the first
  three top-level list items, so lead with what matters most:
  `- **Providers on the web page**: ...`
- Compiled into the binary; rebuild to see a change.
-->

## v5.2.2

- **A new architecture**: the runtime is rebuilt in layers (agent loop / capability assembly / coding runtime) and the old engine is retired; the terminal, web UI, VS Code and ACP share one runtime and behave alike. A session is now a log of events: undo, rewind and compaction are appended rather than rewriting history, so a session comes back whole after an interruption or a crash.
- **The new terminal UI is the default**: running atomcode opens the new full-screen UI (switch back with --classic or [ui] screen); click, double-click a word, triple-click a line, right-click for a menu, drag-select across screens with auto-scroll, and click links and file paths to open them; the terminal title and tab show the session name and flag the session that is waiting for you.
- **Settings and management in panels**: /config opens a settings panel (Settings / Config / Status / Usage / Stats) with quota, daily usage and per-model usage; /provider manages accounts and models and checks connectivity once saved; /mcp turns MCP servers on and off one by one; /toolbox toggles single tools mid-session; /plugin manages plugins; /status shows the signed-in account and when the subscription ends.
- **Background sessions and teams**: /background (/bg) keeps the current session running in the background, grouped, switchable and retrievable; /review runs in the background by default; questions from background sessions come to the front to be answered in place; the team panel lists the main session, members and subagents with status, elapsed time and context use, and member sessions are saved and come back with their lead.
- **More reliable sessions**: /resume previews what a session last talked about, and Ctrl+D twice deletes one; sessions survive renaming the project folder; double Esc opens the rewind panel.
- **New and better commands**: /changelog shows what changed in each release; /worktree creates a separate working copy and switches to it; /diff and /diff git show this session's and the workspace's changes; /loop 5m repeats on an interval; /todo add and /todo clear edit the plan; /cost itemizes tokens by model; /copy copies the last reply, /copy code the code blocks; /raw prints the whole conversation to the terminal for its own selection; !cmd runs a command locally; /cd keeps bookmarks and recent directories; the new UI also has /proxy, /schedule, /openrouter, /app, /webui and /sync, and /upgrade upgrades in place and restarts; the slash menu is ordered by how often you use each command.
- **Rebuilt model settings on the web**: grouped by provider, one card for the provider and its models, connectivity checked once saved; set the reasoning effort levels a model offers and its default.
- **Models and reasoning effort**: F2 / Shift+F2 switch models quickly, with effort chosen right after; /effort lists only the levels the model supports; GLM and Qwen reasoning is kept; when two accounts share a model name the status line says which provider; OpenRouter no longer sends you to the browser once authorized, and retired models drop off the list.
- **Smoother interaction**: paste screenshots and image paths into the composer, with long pastes folded; file writes and edits show their diff inline; Ctrl+R searches this project's input history; Ctrl+X interrupts the turn and sends queued messages at once; sudo and ssh in tools ask for the password in the composer instead of hanging.
- **Security, stability, Windows, and fixes**: reading credentials, keys or .env files asks for approval first; delegated subagents have clear limits; the repeated-call fuse is gentler and tunable with [coding] repeat_stop_rounds; images per request are capped and compressed so requests are no longer too large; legacy Windows consoles, colors, keys and box drawing work, and a console that cannot draw the new UI falls back to the classic one and says why; /upgrade on Windows no longer fails with "access denied" while an old version holds the slot; background processes in bash keep their output and stop reliably; switching to a strict provider no longer fails every request with 400.

## v5.1.0 (2026-09-18)

- **More providers**: support for the OpenAI Responses API.
- **AtomGit tools and code graph**: atomgit_issue can update and close issues; atomgit_pr can list your own PRs (list_mine) and update a PR; the code-graph tools (list_symbols / read_symbol / trace_callers) now understand Kotlin.
- **Sessions and memory**: a new list_sessions tool lists this project's past sessions; a new machine-local memory layer (global > project > local).
- **Code Rewind (opt-in)**: set ATOMCODE_CODE_REWIND to roll back the code changes a session made.
- **Approvals and safety**: a new "allow all Bash" option, with the reason shown when you are asked again; press Tab in the approval panel to see the full bash command.
- **OpenRouter in one step (free models included)**: /openrouter [key] or an OAuth (PKCE) sign-in connects OpenRouter and sets up its free models automatically.
- **/worklog daily recap**: sums up the day's work across projects into a short report.
- **Faster, smoother TUI, and fixes**: the /resume list scans in parallel, about 4× faster; -c / resume only scans the current session's directory at startup.

## v5.0.9 (2026-08-27)

- **External subagents (Codex / Claude Code)**: use Codex or Claude Code as a subagent backend.
- **Deep code review**: /review deep and verify review along several dimensions in parallel, check every finding on its own, then merge the duplicates.
- **Reasoning effort from configuration, with a new xhigh level**: effort levels come from configuration, including reasoning_effort_levels sent by the server.
- **ACP v2**: sessions and resume, elicitation forms, MCP, and v2 HTTP MCP connections.
- **Resume a session from the command line**: -p --resume and the resume subcommand reopen a given session; its id is printed on exit.
- **TUI and compaction**: the ● dot turns green when a tool call succeeds (serial, parallel and resumed alike); even one huge turn can be compacted; compaction always keeps the latest real exchange.
- **Fixes**: sturdier provider and model management and weak-model handling, plus sign-in and security fixes.

## v5.0.8 (2026-08-20)

- **Select and copy with the mouse**: double-click a word, triple-click a line, and the selection is copied; tool blocks such as bash commands and output can be drag-selected too; mouse capture is now off by default, so the terminal's own selection works.
- **Zhipu Coding Plan preset**: add Zhipu Coding Plan straight from /provider; the protocol toggle gains Ollama, filling in the local address when left empty.
- **New hook events**: Stop, StopFailure and PostToolUseFailure let plugins see a turn end, an API error, or a failed tool call.
- **/code-review shows progress**: the current round (round X/N) and the findings so far.
- **Fewer round trips on large files**: read_file can jump to a spot from the outline and read several ranges at once.
- **Fixes**: an interactive /resume of a busy session now forks it; replies cut off by the length limit happen less often; the web UI can send messages over plain HTTP on a LAN and no longer stacks duplicate notices; an unmatched edit_file is a yellow warning, not an error.

## v5.0.7 (2026-08-19)

- **Providers and model discovery in the web UI**: manage providers, discover and add models, and reuse an existing account when adding a model.
- **Install the web UI as an app, with notifications**: install it as a desktop app (PWA) and get a browser notification when a task finishes; a persistent Todo panel, a reworked sidebar, adjustable font size, and per-turn stats.
- **Reasoning effort per model**: configure effort levels per model; /provider in the TUI takes several at once, the web UI offers only what the endpoint supports, and the choice stays in sync across clients.
- **Much better TUI interaction**: select conversation content by meaning, select text with the mouse in the composer and history, preview sessions in the picker, and move Up/Down by wrapped lines; repeated pastes can be expanded; long tasks wrap more reliably.
- **Security**: the credential shell guard is configurable, and approval conditions you grant are remembered.
- **Fixes**: IME input being overwritten, sidebar flicker, the waiting indicator, lost turn timestamps, the notification permission prompt, and message queueing and sync in the web UI; Orca terminal support, table rendering on Windows, and a proxy loopback leak; truncated session tab titles in VS Code and the model dropdown highlight in JetBrains; duplicate plugin scans of the home directory, per-project prompt history, and folded Team tool output.

## v5.0.6 (2026-08-12)

- **Agent Team**: several agents work in parallel, with each member's status, task details and token use shown live.
- **Semantic code intelligence**: enable LSP when you need it for symbol reading, reference search and call-chain analysis, making large projects easier to analyze.
- **Goal recovery for long tasks**: progress and context survive a pause, compaction, a model switch or a crash, so work is neither repeated nor lost.
- **Interactive /config editor**: manage configuration inside the TUI; new Xiaomi MiMo and OpenCode Zen presets, and per-model vision settings.
- **MCP improvements**: follows the usage instructions an MCP server provides; tighter HTTP response limits, stdio timeout recovery and concurrency handling make it steadier and safer.
- **Sessions and web UI**: configurable /resume history truncation, continuing a session across workspaces, and adding messages while a task runs; TodoWrite rows name the task; better input and model switching on mobile.
- **Images and large files**: a model can declare native image support with supports_vision; read_file hints at paging so a huge file never lands in context all at once.
- **Fixes**: DeepSeek V4 tool calls, F2 skipping unavailable models, Goal/Todo panels out of sync, long-task wrapping, a lost input prefix, web UI queueing and sync, and internal reminders leaking into the session.

## v5.0.5 (2026-08-07)

- **Persistent goals**: a goal stays after it is met or runs out of rounds, and your next message carries it on; Esc pauses it and your next submit resumes it; its status stays on screen.
- **Steer mid-task, with next-step suggestions**: add guidance while a task runs; when a turn ends you get a suggested next prompt based on its results.
- **Edit GBK / GB18030 files in place**: the edit tools keep a file's original encoding.
- **Redesigned directory picker**: /cd has a new picker; when a typed path prefix does not exist, Enter takes the highlighted match.
- **Every server address is configurable**: change any service address in configuration, for private deployments.
- **Fixes**: comments in .mcp.json, normalized MCP tool names and protocol negotiation; the first transient 429 is retried silently; a missing path suggests its nearest existing parent; credential shell commands are no longer retried in a loop; text contrast on light and dark terminals; reflow after resizing the legacy Windows console.

## v5.0.4 (2026-08-03)

- **Local scheduled tasks**: a new atomcode schedule subcommand (add / list / remove / enable / disable) registers with the system scheduler (launchd, systemd, Windows Task Scheduler) and refuses risky commands in unattended runs; /schedule lists the tasks in the TUI.
- **/rewind and history search**: /rewind opens the checkpoint picker; Ctrl+R searches your input history.
- **CodingPlan models sync after sign-in**: signing in to CodingPlan fetches the available models.
- **Custom project memory directory**: ATOMCODE_PROJECT_MEMORY_DIR sets where project memory is kept.
- **Fixes**: output received before a dropped connection is kept, with a hint at a likely proxy cause (10054); pasting screenshots on Windows; overlapping tool blocks; unreadable grey text on dark themes; updating the bundled binary in the VS Code and JetBrains plugins; faster startup.

## v5.0.3 (2026-07-30)

- **Rebuilt /provider panel**: separate Accounts and Models tabs, adding from vendor presets (TaoToken added), and editing accounts and models; /model groups models by account.
- **Safe rewind**: workspace checkpoints take the conversation and its code changes back to an earlier step together.
- **Tool output archive**: very long tool output is stored aside and read page by page with fetch_output when needed, so long sessions stay lean.
- **Round-limit checkpoint**: on reaching [coding] max_rounds you are asked whether to continue or stop, instead of being cut off.
- **TUI**: your input sits on a background block; subtask progress is pinned in the footer; shortcuts to switch models; /skills takes several skills with fuzzy filtering; shell completions.
- **More accurate /cost**: priced from models.dev and broken down by provider and model.
- **Fixes**: Windows uses the system TLS to get past fingerprint blocking; AtomGit sign-in recovers after a 401; HarmonyOS falls back to sh when bash is absent; MCP stdio servers recover after their process exits; /compact in the web UI's sync mode, uploaded images surviving a refresh, and refusing a model switch mid-turn.

## v5.0.2 (2026-07-24)

- **Custom command arguments work**: the args field of a custom command now takes effect, filled into the command's text.
- **Tolerant configuration**: an invalid provider section is set aside instead of stopping startup, and writes such as /model still go through.
- **Network compatibility**: AtomGit connections retry over TLS 1.2; one broken system root certificate no longer stops all networking.
- **Fixes**: the web UI keeps the TUI in sync when switching projects and polls the model selector less; the install script matches whole PATH entries.

## v5.0.1 (2026-07-24)

- **Answer questions right in the UI**: when the model needs a decision, the TUI or web UI shows a single-choice, multiple-choice or free-text question, and several questions can be answered at once; on by default.
- **Trust for project MCP servers**: servers in a project's .mcp.json connect only after /mcp trust, and /mcp untrust takes it back; .mcp.json accepts comments; plugin hooks run only once trusted, and you are asked again when a plugin changes.
- **Corporate networks**: follows the system proxy (Windows / macOS); trusts the system root certificates and SSL_CERT_FILE; sign-in connection failures come with troubleshooting hints.
- **TUI**: a live token counter while waiting, so long tasks no longer look stuck; a redesigned /resume picker that shows the delete and rename keys; /status, /diff, /rename and more run while a task is in progress; /todo add adds a task directly.
- **Skills trigger more reliably**: the skill catalog is in the system prompt and a matching skill is used first; the subagent task tool is on by default.
- **Fixes**: a bash timeout or cancel ends the whole process tree (Windows / Unix); the edit tool tolerates whitespace differences; wide tables, QR codes in Windows Terminal, and arrow keys under TERM=dumb; a third-party model's 429 is no longer reported as CodingPlan quota exhaustion; room is kept for the reply near the context limit.

## v5.0.0 (2026-07-17)

- **Better Todo panel**: tasks update one item at a time, the in-progress item is checked every round to stop drift, and the run continues once more when items are left; /todo clear empties the list.
- **Unified execution modes and approvals**: Tab cycles four modes: plan, build, auto (no approvals) and accept edits.
- **Read-only tools run in parallel**: file reads, grep and read-only bash run concurrently, speeding up multi-tool turns.
- **/usage panel (replaces /cost)**: the current 5-hour window and CodingPlan plan, a 60-day heatmap, per-model charts and a usage table.
- **Better /plugin page**: marketplace browsing with search, install scope shown, and a two-line list.
- **Better /init**: an agent analyzes the repository and writes AGENTS.md; skills in ~/.agents/skills and .agents/skills are found and shared across agents.
- **Context, stability and terminals**: tighter tool-output limits keep long sessions lean; the web UI reconnects on its own and saves incrementally on a crash.
- **Fixes**: the context window now reloads after a model switch, /clear really starts a new session, and approvals and turns display better.
