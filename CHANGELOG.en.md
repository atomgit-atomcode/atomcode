# AtomCode Changelog

<!--
For whoever edits this file; this block is never shown.

- The English twin of `CHANGELOG.md`: one section per release, same version
  numbers. The English UI reads this file; a release with no section here is
  shown from the Chinese file instead.
- Each section starts `## vX.Y.Z`, optionally followed by a same-source version
  and a date: `## v5.2.2（v5.2.0） (2026-10-10)`. Newest first (order is by
  version number anyway).
- A section has three parts under these subheadings (tabs in `/changelog`):
  - `### Overview`: a paragraph or two on what the release is about, shown at
    the top of the Overview tab.
  - `### Changes`: the list of features and fixes, shown under the overview.
    Other subheadings inside it (`### Fixes`) count as changes too.
  - Issues are written once, in `CHANGELOG.md`, under their original titles; the
    English UI shows them from there. Leave them out of this file (an `### Issues`
    part written here would take their place).
  - Without any of the three, the whole section is changes.
- The one-line notice after an upgrade names the **bold lead** of the first
  three top-level items under Changes, so lead with what matters most:
  `- **Providers on the web page**: ...`
- Compiled into the binary; rebuild to see a change.
-->

## v5.2.2（v5.2.0） (2026-10-10)

### Overview

This release is about **rebuilding the architecture and the terminal UI**: the runtime is rewritten in layers, every front end shares one runtime, and a session is a log of events that comes back whole; the new full-screen terminal UI is the default, with settings, models, MCP, plugins and background sessions in panels.

### Changes

- **A new architecture**: the runtime is rebuilt in layers (agent loop / capability assembly / coding runtime) and the old engine is retired; the terminal, web UI, VS Code and ACP share one runtime and behave alike. A session is now a log of events: undo, rewind and compaction are appended rather than rewriting history, so a session comes back whole after an interruption or a crash.
- **The new terminal UI is the default**: running atomcode opens the new full-screen UI (switch back with --classic or [ui] screen); click, double-click a word, triple-click a line, right-click for a menu, drag-select across screens with auto-scroll, and click links and file paths to open them; the terminal title and tab show the session name and flag the session that is waiting for you.
- **Settings and management in panels**: /config opens a settings panel (Settings / Config / Status / Usage / Stats) with quota, daily usage and per-model usage; /provider manages accounts and models and checks connectivity once saved; /mcp turns MCP servers on and off one by one; /toolbox toggles single tools mid-session; /plugin manages plugins; /status shows the signed-in account and when the subscription ends.
- **Background sessions and teams**: /background (/bg) keeps the current session running in the background, grouped, switchable and retrievable; /review runs in the background by default; questions from background sessions come to the front to be answered in place; the team panel lists the main session, members and subagents with status, elapsed time and context use, and member sessions are saved and come back with their lead.
- **More reliable sessions**: /resume previews what a session last talked about, and Ctrl+D twice deletes one; sessions survive renaming the project folder; double Esc opens the rewind panel; in a Git repository /rewind can take the code back too, on by default (ATOMCODE_CODE_REWIND=0 turns it off).
- **New and better commands**: /changelog shows what changed in each release; /worktree creates a separate working copy and switches to it; /diff and /diff git show this session's and the workspace's changes; /loop 5m repeats on an interval; /todo add and /todo clear edit the plan; /cost itemizes tokens by model; /copy copies the last reply, /copy code the code blocks; /raw prints the whole conversation to the terminal for its own selection; !cmd runs a command locally; /cd keeps bookmarks and recent directories; the new UI also has /proxy, /schedule, /openrouter, /app, /webui and /sync, and /upgrade upgrades in place and restarts; the slash menu is ordered by how often you use each command.
- **Rebuilt model settings on the web**: grouped by provider, one card for the provider and its models, connectivity checked once saved; set the reasoning effort levels a model offers and its default.
- **Models and reasoning effort**: F2 / Shift+F2 switch models quickly, with effort chosen right after; /effort lists only the levels the model supports; GLM and Qwen reasoning is kept; when two accounts share a model name the status line says which provider; OpenRouter no longer sends you to the browser once authorized, and retired models drop off the list.
- **Smoother interaction**: paste screenshots and image paths into the composer, with long pastes folded; file writes and edits show their diff inline; Ctrl+R searches this project's input history; Ctrl+X interrupts the turn and sends queued messages at once; sudo and ssh in tools ask for the password in the composer instead of hanging.
- **Security, stability, Windows, and fixes**: reading credentials, keys or .env files asks for approval first; delegated subagents have clear limits; the repeated-call fuse is gentler and tunable with [coding] repeat_stop_rounds; images per request are capped and compressed so requests are no longer too large; legacy Windows consoles, colors, keys and box drawing work, and a console that cannot draw the new UI falls back to the classic one and says why; /upgrade on Windows no longer fails with "access denied" while an old version holds the slot; background processes in bash keep their output and stop reliably; switching to a strict provider no longer fails every request with 400.

## v5.1.0（v5.2.1） (2026-09-18)

### Overview

This release widens what you can connect: the OpenAI Responses API and OpenRouter's free models, stronger AtomGit tools and code graph, plus opt-in code rewind and a daily work recap.

### Changes

- **More providers**: support for the OpenAI Responses API.
- **AtomGit tools and code graph**: atomgit_issue can update and close issues; atomgit_pr can list your own PRs (list_mine) and update a PR; the code-graph tools (list_symbols / read_symbol / trace_callers) now understand Kotlin.
- **Sessions and memory**: a new list_sessions tool lists this project's past sessions; a new machine-local memory layer (global > project > local).
- **Code Rewind (opt-in)**: set ATOMCODE_CODE_REWIND to roll back the code changes a session made.
- **Approvals and safety**: a new "allow all Bash" option, with the reason shown when you are asked again; press Tab in the approval panel to see the full bash command.
- **OpenRouter in one step (free models included)**: /openrouter [key] or an OAuth (PKCE) sign-in connects OpenRouter and sets up its free models automatically.
- **/worklog daily recap**: sums up the day's work across projects into a short report.
- **Faster, smoother TUI, and fixes**: the /resume list scans in parallel, about 4× faster; -c / resume only scans the current session's directory at startup.

## v5.0.9 (2026-08-27)

### Overview

This release lets AtomCode bring in more help: Codex and Claude Code as external subagents, a deep code-review mode, reasoning effort driven by configuration, and the ACP v2 protocol.

### Changes

- **External subagents (Codex / Claude Code)**: use Codex or Claude Code as a subagent backend.
- **Deep code review**: /review deep and verify review along several dimensions in parallel, check every finding on its own, then merge the duplicates.
- **Reasoning effort from configuration, with a new xhigh level**: effort levels come from configuration, including reasoning_effort_levels sent by the server.
- **ACP v2**: sessions and resume, elicitation forms, MCP, and v2 HTTP MCP connections.
- **Resume a session from the command line**: -p --resume and the resume subcommand reopen a given session; its id is printed on exit.
- **TUI and compaction**: the ● dot turns green when a tool call succeeds (serial, parallel and resumed alike); even one huge turn can be compacted; compaction always keeps the latest real exchange.
- **Fixes**: sturdier provider and model management and weak-model handling, plus sign-in and security fixes.

## v5.0.8 (2026-08-20)

### Overview

This release improves selecting and copying in the terminal, adds a Zhipu Coding Plan preset and more hook events, and cuts the round trips weak models make on large files.

### Changes

- **Select and copy with the mouse**: double-click a word, triple-click a line, and the selection is copied; tool blocks such as bash commands and output can be drag-selected too; mouse capture is now off by default, so the terminal's own selection works.
- **Zhipu Coding Plan preset**: add Zhipu Coding Plan straight from /provider; the protocol toggle gains Ollama, filling in the local address when left empty.
- **New hook events**: Stop, StopFailure and PostToolUseFailure let plugins see a turn end, an API error, or a failed tool call.
- **/code-review shows progress**: the current round (round X/N) and the findings so far.
- **Fewer round trips on large files**: read_file can jump to a spot from the outline and read several ranges at once.
- **Fixes**: an interactive /resume of a busy session now forks it; replies cut off by the length limit happen less often; the web UI can send messages over plain HTTP on a LAN and no longer stacks duplicate notices; an unmatched edit_file is a yellow warning, not an error.

## v5.0.7 (2026-08-19)

### Overview

This release focuses on the web UI: manage providers and discover models there, and install it as a desktop app; reasoning effort can be set per model, and the terminal gets much better interaction.

### Changes

- **Providers and model discovery in the web UI**: manage providers, discover and add models, and reuse an existing account when adding a model.
- **Install the web UI as an app, with notifications**: install it as a desktop app (PWA) and get a browser notification when a task finishes; a persistent Todo panel, a reworked sidebar, adjustable font size, and per-turn stats.
- **Reasoning effort per model**: configure effort levels per model; /provider in the TUI takes several at once, the web UI offers only what the endpoint supports, and the choice stays in sync across clients.
- **Much better TUI interaction**: select conversation content by meaning, select text with the mouse in the composer and history, preview sessions in the picker, and move Up/Down by wrapped lines; repeated pastes can be expanded; long tasks wrap more reliably.
- **Security**: the credential shell guard is configurable, and approval conditions you grant are remembered.
- **Fixes**: IME input being overwritten, sidebar flicker, the waiting indicator, lost turn timestamps, the notification permission prompt, and message queueing and sync in the web UI; Orca terminal support, table rendering on Windows, and a proxy loopback leak; truncated session tab titles in VS Code and the model dropdown highlight in JetBrains; duplicate plugin scans of the home directory, per-project prompt history, and folded Team tool output.

## v5.0.6 (2026-08-12)

### Overview

This release brings agents working in parallel (Agent Team) and semantic code intelligence (LSP) when you want it, more reliable recovery for long tasks, and an interactive configuration editor.

### Changes

- **Agent Team**: several agents work in parallel, with each member's status, task details and token use shown live.
- **Semantic code intelligence**: enable LSP when you need it for symbol reading, reference search and call-chain analysis, making large projects easier to analyze.
- **Goal recovery for long tasks**: progress and context survive a pause, compaction, a model switch or a crash, so work is neither repeated nor lost.
- **Interactive /config editor**: manage configuration inside the TUI; new Xiaomi MiMo and OpenCode Zen presets, and per-model vision settings.
- **MCP improvements**: follows the usage instructions an MCP server provides; tighter HTTP response limits, stdio timeout recovery and concurrency handling make it steadier and safer.
- **Sessions and web UI**: configurable /resume history truncation, continuing a session across workspaces, and adding messages while a task runs; TodoWrite rows name the task; better input and model switching on mobile.
- **Images and large files**: a model can declare native image support with supports_vision; read_file hints at paging so a huge file never lands in context all at once.
- **Fixes**: DeepSeek V4 tool calls, F2 skipping unavailable models, Goal/Todo panels out of sync, long-task wrapping, a lost input prefix, web UI queueing and sync, and internal reminders leaking into the session.

## v5.0.5 (2026-08-07)

### Overview

This release makes a goal something you can keep pursuing, lets you steer a running task and suggests the next step when a turn ends, and edits GBK-encoded files in place.

### Changes

- **Persistent goals**: a goal stays after it is met or runs out of rounds, and your next message carries it on; Esc pauses it and your next submit resumes it; its status stays on screen.
- **Steer mid-task, with next-step suggestions**: add guidance while a task runs; when a turn ends you get a suggested next prompt based on its results.
- **Edit GBK / GB18030 files in place**: the edit tools keep a file's original encoding.
- **Redesigned directory picker**: /cd has a new picker; when a typed path prefix does not exist, Enter takes the highlighted match.
- **Every server address is configurable**: change any service address in configuration, for private deployments.
- **Fixes**: comments in .mcp.json, normalized MCP tool names and protocol negotiation; the first transient 429 is retried silently; a missing path suggests its nearest existing parent; credential shell commands are no longer retried in a loop; text contrast on light and dark terminals; reflow after resizing the legacy Windows console.

## v5.0.4 (2026-08-04)

### Overview

This release adds local scheduled tasks that run unattended through the system scheduler, along with a /rewind command and input history search.

### Changes

- **Local scheduled tasks**: a new atomcode schedule subcommand (add / list / remove / enable / disable) registers with the system scheduler (launchd, systemd, Windows Task Scheduler) and refuses risky commands in unattended runs; /schedule lists the tasks in the TUI.
- **/rewind and history search**: /rewind opens the checkpoint picker; Ctrl+R searches your input history.
- **CodingPlan models sync after sign-in**: signing in to CodingPlan fetches the available models.
- **Custom project memory directory**: ATOMCODE_PROJECT_MEMORY_DIR sets where project memory is kept.
- **Fixes**: output received before a dropped connection is kept, with a hint at a likely proxy cause (10054); pasting screenshots on Windows; overlapping tool blocks; unreadable grey text on dark themes; updating the bundled binary in the VS Code and JetBrains plugins; faster startup.

## v5.0.3 (2026-07-30)

### Overview

This release rebuilds the /provider panel and adds checkpoint-based rewind, an archive for tool output and a round-limit checkpoint, keeping long sessions under control.

### Changes

- **Rebuilt /provider panel**: separate Accounts and Models tabs, adding from vendor presets (TaoToken added), and editing accounts and models; /model groups models by account.
- **Safe rewind**: workspace checkpoints take the conversation and its code changes back to an earlier step together.
- **Tool output archive**: very long tool output is stored aside and read page by page with fetch_output when needed, so long sessions stay lean.
- **Round-limit checkpoint**: on reaching [coding] max_rounds you are asked whether to continue or stop, instead of being cut off.
- **TUI**: your input sits on a background block; subtask progress is pinned in the footer; shortcuts to switch models; /skills takes several skills with fuzzy filtering; shell completions.
- **More accurate /cost**: priced from models.dev and broken down by provider and model.
- **Fixes**: Windows uses the system TLS to get past fingerprint blocking; AtomGit sign-in recovers after a 401; HarmonyOS falls back to sh when bash is absent; MCP stdio servers recover after their process exits; /compact in the web UI's sync mode, uploaded images surviving a refresh, and refusing a model switch mid-turn.

## v5.0.2 (2026-07-24)

### Overview

A stability update: custom command arguments work, a broken configuration no longer stops startup, and network compatibility improves.

### Changes

- **Custom command arguments work**: the args field of a custom command now takes effect, filled into the command's text.
- **Tolerant configuration**: an invalid provider section is set aside instead of stopping startup, and writes such as /model still go through.
- **Network compatibility**: AtomGit connections retry over TLS 1.2; one broken system root certificate no longer stops all networking.
- **Fixes**: the web UI keeps the TUI in sync when switching projects and polls the model selector less; the install script matches whole PATH entries.

## v5.0.1 (2026-07-24)

### Overview

This release lets the model ask you questions right in the UI, tightens trust for project MCP servers and plugin hooks, and connects better on corporate networks.

### Changes

- **Answer questions right in the UI**: when the model needs a decision, the TUI or web UI shows a single-choice, multiple-choice or free-text question, and several questions can be answered at once; on by default.
- **Trust for project MCP servers**: servers in a project's .mcp.json connect only after /mcp trust, and /mcp untrust takes it back; .mcp.json accepts comments; plugin hooks run only once trusted, and you are asked again when a plugin changes.
- **Corporate networks**: follows the system proxy (Windows / macOS); trusts the system root certificates and SSL_CERT_FILE; sign-in connection failures come with troubleshooting hints.
- **TUI**: a live token counter while waiting, so long tasks no longer look stuck; a redesigned /resume picker that shows the delete and rename keys; /status, /diff, /rename and more run while a task is in progress; /todo add adds a task directly.
- **Skills trigger more reliably**: the skill catalog is in the system prompt and a matching skill is used first; the subagent task tool is on by default.
- **Fixes**: a bash timeout or cancel ends the whole process tree (Windows / Unix); the edit tool tolerates whitespace differences; wide tables, QR codes in Windows Terminal, and arrow keys under TERM=dumb; a third-party model's 429 is no longer reported as CodingPlan quota exhaustion; room is kept for the reply near the context limit.

## v5.0.0 (2026-07-17)

### Overview

The first AtomCode 5.0 release: four unified execution modes with approvals, read-only tools in parallel, a new /usage panel, and a better Todo panel, plugin marketplace and /init.

### Changes

- **Better Todo panel**: tasks update one item at a time, the in-progress item is checked every round to stop drift, and the run continues once more when items are left; /todo clear empties the list.
- **Unified execution modes and approvals**: Tab cycles four modes: plan, build, auto (no approvals) and accept edits.
- **Read-only tools run in parallel**: file reads, grep and read-only bash run concurrently, speeding up multi-tool turns.
- **/usage panel (replaces /cost)**: the current 5-hour window and CodingPlan plan, a 60-day heatmap, per-model charts and a usage table.
- **Better /plugin page**: marketplace browsing with search, install scope shown, and a two-line list.
- **Better /init**: an agent analyzes the repository and writes AGENTS.md; skills in ~/.agents/skills and .agents/skills are found and shared across agents.
- **Context, stability and terminals**: tighter tool-output limits keep long sessions lean; the web UI reconnects on its own and saves incrementally on a crash.
- **Fixes**: the context window now reloads after a model switch, /clear really starts a new session, and approvals and turns display better.

## v4.26.0 (2026-07-09)

### Overview

This release brings subagent delegation across strong and fast models, /loop, and Todo tasks, a much stronger web UI, and smarter context compaction.

### Changes

- **Subagents across strong and fast models**: the main model orchestrates and routes by provider capability (a strong model plans, a fast one executes), with subtasks in parallel; off by default, turn it on with ATOMCODE_SUBAGENT=1.
- **/loop**: repeat a task on a self-paced rhythm or a fixed interval, with stopping, a round limit and cancellation all handled.
- **/app for the GitCode app**: pair the GitCode app with AtomCode to use AtomCode from GitCode.
- **Todo tasks**: the todowrite tool manages a task list, the current list is given to the model each round, the footer shows the current task and progress (N/M), and /todo shows the list.
- **A much stronger web UI**: slash commands throughout (/undo, /compact, /context, /status, /config, /diff, /cost, /memory, /skills and more); three approval modes (Build / Plan / Bypass); a cross-project session sidebar; searching and jumping within a session; message timestamps; output follows only when you are at the bottom, with a back-to-bottom button.
- **Context and compaction**: multi-level compaction on overflow (mechanical plus an LLM summary) that keeps recent working context and bounds the summary's input, cache-friendly history compaction, and compaction pressure recomputed for the new model's window after a switch.
- **Export and copy**: /save exports the conversation as Markdown and /copy msg copies a whole reply; /provider can set context_window and suggests 128k / 256k / 512k / 1m.
- **Fixes**: grep no longer reads a whole large file into memory and freezes low-end machines, plus other fixes.

## v4.25.9 (2026-07-04)

### Overview

Sessions now name themselves, and the terminal tab shows the session name and its status, so several windows are easy to tell apart.

### Changes

- **Automatic session names**: after the first turn the AI names the session, never overriding a name you set with /rename, and the web UI shows it too; turn it off with [ui] ai_session_naming.
- **Tab status light**: the terminal tab title shows the session name with 🟢 idle, 🟡 working or 🔴 waiting for approval; turn it off with [ui] terminal_status_glyph.
- **/compact no longer hangs**: generating the summary is capped at 120 seconds.
- **Fixes**: /cd paths on Windows no longer carry the \\?\ prefix, and the /cd picker lists each directory once; > nul is rewritten to /dev/null under Git Bash; the web UI opens the browser correctly on Windows; web UI output no longer corrupts the TUI screen; the Ctrl+O expand hint appears as soon as a tool starts.

## v4.25.8 (2026-07-03)

### Overview

This release rounds out copying and exporting in the web UI, makes login state clear in /status, lets @ mentions step into directories, and improves bash detection on Windows.

### Changes

- **Web UI copy and export**: a copy button on messages copies the whole turn, tool calls included; sessions export to a Markdown file; image history displays properly, and a hard stop mid-turn no longer loses the whole turn.
- **Login state in /status**: the first line shows whether you are signed in and as whom (nickname and username), and an expired login clearly says to run /login; the Token line is gone.
- **@ mentions into directories**: @ completion steps into subdirectories and keeps the selected item in view; the / and @ menus share one highlight color.
- **Windows bash detection**: Git Bash is found on drives other than C:, WSL's app aliases are no longer mistaken for bash, and the model is told truthfully whether it is using Git Bash or cmd.exe.
- **VS Code extension**: @ file references, input history with the arrow keys, and an interface in Chinese and English.
- **Fixes**: auto-copy of code blocks is off by default and, when on, copies only when a reply has exactly one block; web_fetch decodes pages by their charset, so Chinese pages no longer garble and non-ASCII pages no longer crash it; paths from the model accept ~; MCP tool arguments show up to 450 characters; adding a plugin marketplace lists its plugins and the install command; tool names display correctly in resumed sessions; the input cursor shows while a reply streams; the web UI opens the browser reliably on Windows.

## v4.25.7 (2026-06-30)

### Overview

This release focuses on rate limits and long tasks: a 429 now waits or pauses based on your plan's window, sudo passwords can be entered in the interface, /goal is steadier, and compaction notices are unified.

### Changes

- **Waiting out rate limits**: on a 429, AtomCode uses the real reset time to decide whether to wait and resume automatically or pause with a countdown; the TUI and web UI show the reset time instead of an error.
- **sudo password prompt**: when a command needs a sudo or ssh password, the TUI shows a hidden input box; Ctrl+C cancels it and it closes when the turn ends.
- **Steadier /goal**: it keeps going after recoverable interruptions, gains round and duration limits, stops after repeated rounds without progress, and no longer comes back after the goal has ended.
- **Unified compaction notices**: automatic and manual compaction share one progress indicator and divider; the footer always shows how much context is used.
- **Double Esc to undo**: with an empty input box, press Esc twice to undo the last turn; the first press tells you to press again.
- **Vision models see images**: with a vision model, images read by read_file are passed to the model.
- **Long output no longer cut off**: when a reply hits the output limit it continues automatically, and large files are written in parts.
- **Fixes**: answers a model puts in its reasoning are shown instead of a blank turn; Windows bash detection, quoting and Chinese Python output are fixed; pasted text expands correctly in history recall and after sending; the terminal is restored after the process is killed by a signal; todo updates show the task title; web_search shows source domains; tildes are no longer read as strikethrough; /login shows your real plan name.

## v4.25.6 (2026-06-25)

### Overview

This release fixes a long list of Windows problems, improves automatic compaction and /cd, and adds a single network proxy setting; the web UI and JetBrains plugin also get many improvements.

### Changes

- **Unified proxy setting**: a new [network.proxy] section lets you follow the system proxy, pin a proxy, or use none, for every outbound connection; switch with /proxy in the TUI and check it with /status.
- **Windows experience**: the bash tool no longer flashes a console window each time; command output is decoded with the console's code page; CRLF line endings and GBK-encoded files no longer break reading and editing; /quit no longer hangs.
- **More reliable auto-compaction**: near the context limit it summarizes like /compact instead of only folding tool results; a very long paste no longer triggers empty summaries over and over; progress is shown while compacting.
- **Better /cd**: typing in the picker filters recent directories, and you can type a path directly; after changing directory, /resume lists that project's sessions.
- **Forgiving edits**: edits still match when tabs and spaces differ, so the model is no longer pushed into rewriting files with scripts.
- **Web UI**: a new welcome page, Skills and MCP menus, session search and date grouping; new sessions appear in the sidebar right away; the sync toggle survives a refresh; several instances no longer sign each other out.
- **JetBrains plugin**: a welcome page and onboarding, pasting or dragging in files, pasting images as attachments, and colors that follow the IDE theme.
- **Fixes**: MCP trust/autoApprove settings take effect and "Always" sticks for MCP tools; /setup writes MCP servers to the .mcp.json that is actually read; a connection reset mid-stream reconnects with a readable message; no more "authentication expired" after /login; resizing no longer duplicates output; a renamed session keeps its name; parallel_edit shows per-file progress.

## v4.25.5 (2026-06-23)

### Overview

A stability fix: a broken plugin hook no longer blocks every prompt, and empty model replies are retried automatically.

### Changes

- **Broken hooks no longer block**: a plugin hook that fails to launch or exits without giving a reason is no longer treated as a block, so prompts and tool calls go through.
- **Empty replies retried**: when the service occasionally returns a completely empty reply, AtomCode retries a few times with a notice instead of silently ending the turn.
- **Fixes**: the warning shown when running with administrator rights now advises running without elevation.

## v4.25.4 (2026-06-23)

### Overview

This release adds in-session code review with /review and persistent goals with /goal, ships a JetBrains IDE plugin, and improves context compaction.

### Changes

- **/review code review**: review your changes inside the session, with built-in rules per language, diff line numbers, and coverage driven by the changed files.
- **/goal persistent goals**: set a goal and the agent keeps working toward it across turns, checking progress each round.
- **JetBrains IDE plugin**: multi-tab chat, session state and a diff viewer, integrated with the IDE.
- **Context and compaction**: multi-level compaction on overflow (mechanical plus an LLM summary) and cache-friendly history compaction.
- **Fixes**: the context window now reloads after a model switch, /clear really starts a new session, and approvals and turns display better.

## v4.25.3 (2026-06-20)

### Overview

A small update: write approvals are scoped by path again, and mouse handling in Windows terminals works normally.

### Changes

- **Path-aware write approval**: ordinary edits inside the workspace are approved automatically; "Always" is remembered per path for files outside it; sensitive files such as .env or SSH keys are asked about every time.
- **Native mouse on Windows**: AtomCode no longer changes the console mode, so wheel scrolling, drag-select, copy and right-click paste all work on conhost and Windows Terminal.
- **Fixes**: no more garbled input from mouse movement in the JetBrains terminal.

## v4.25.2 (2026-06-19)

### Overview

This release moves to a new agent engine, makes cancelling, approvals and usage display more reliable, and fixes many web UI sync problems.

### Changes

- **Cancel means undo**: a turn cancelled with Esc no longer stays in the context, so the model won't bring it up later; a turn stuck while connecting can be cancelled at once.
- **A focus for /compact**: add a focus after /compact and the summary is written around it.
- **Truer usage in the footer**: each turn shows the tokens actually billed and the cache-hit share, instead of counting the re-sent context every round; token counts use K/M.
- **Sensitive reads need approval**: reading SSH keys, .env files, cloud credentials and similar files asks for approval first.
- **Fixes**: interactive approvals wait for your answer instead of timing out into a denial; new /exit; a "slow response" hint when the model is slow; private plugin marketplaces use your login and git no longer freezes the UI; stale connections recover on their own; the stream timeout rises to 300 seconds; web_fetch can return Markdown; skills in nested folders are found; table borders show on dark themes; the input box tops out at 6 rows; cancelled approvals no longer reappear after a /model switch; the web UI sidebar refreshes after starting a new chat.

## v4.25.1 (2026-06-12)

### Overview

This release adds /view for files and a WeChat channel, switches web search to Exa by default, and improves performance and rendering in large directories.

### Changes

- **/view files**: /view <path> shows a code file right in the terminal UI.
- **WeChat channel**: talk to AtomCode through a personal WeChat ClawBot; see the AtomCode-Channel repository for the plugin.
- **Exa web search by default**: WebSearch uses Exa by default, with the web_access skill as a fallback.
- **Performance and rendering**: faster @ file indexing in very large directories, whole-line CJK rendering fixed in the DevEco terminal, and stronger SSRF protection in WebFetch.
- **Fixes**: and other fixes.

## v4.25.0 (2026-06-06)

### Overview

This release adds ! to run commands directly and reasoning effort for DeepSeek V4, simplifies approvals, and improves WebFetch and the web UI.

### Changes

- **Run commands with !**: type ! and a command, such as !git status, to run it directly; its output goes into the model's context.
- **DeepSeek V4 reasoning effort**: pick high or max with /effort.
- **Simpler approvals**: approval rules are simpler; see the approvals page in the docs.
- **Faster WebFetch**: no more max_chars limit, and fetched HTML is converted to Markdown.
- **Web UI**: switching the working directory, model and effort in the browser works better.
- **Fixes**: and other fixes.

## v4.24.2 (2026-06-03)

### Overview

This release adds /undo to roll back the conversation, ties the web UI and TUI more closely together, and fixes several causes of periodic drops in the cache hit rate.

### Changes

- **/undo the conversation**: roll the conversation's memory back to an earlier prompt, so what came after leaves the context.
- **Web UI and TUI in sync**: model switches carry over both ways in real time; /webui opens the TUI's current session; approvals from the TUI work in sync mode, and a second tool in the same turn is no longer denied instantly.
- **Fuller session restore**: resuming a conversation brings back the dividers between turns along with each turn's token and tool counts.
- **Steadier caching**: the session's system prompt stays fixed, Plan mode notes and compaction summaries move out of it, and earlier file reads stay unchanged, so the prefix cache no longer collapses periodically.
- **Image paths recognised**: a local image path typed in, or pasted on Windows, is sent as an image attachment.
- **Fixes**: rate-limit retries wait for the cooldown the gateway suggests; web UI live output no longer leaks into other sessions, finished turns no longer leave approval cards behind, and the default port moves to 13457 to avoid clashing with VSCode; Windows system folders get sensitive-path protection, and Windows paths and multi-line content are no longer mis-escaped; files are found when you point at .atomcode, .claude and similar folders explicitly; CodingPlan no longer falsely reports model-list drift.

## v4.24.1 (2026-06-03)

### Overview

This release introduces a new local web UI that shares the same live session with the terminal, adds the $ skills menu, and simplifies adding a provider.

### Changes

- **A new local web UI**: /webui starts a web server in-process and opens the browser, with streaming chat, tool runs, approvals, a session sidebar and directory switching; the terminal and the browser share one live session, and remote access is supported.
- **$ skills menu**: type $ at the start of a line to list and filter skills, Tab to complete, and $name with arguments to run one.
- **Simpler /provider add**: pasted curl, JSON or TOML is recognized, the Base URL comes first and the type is filled in.
- **Bash**: long tasks can run in the background (run_in_background), and destructive commands are flagged before they run.
- **Fixes**: and other fixes.

## v4.24.0 (2026-06-01)

### Overview

This release centers on plugins, with an interactive /plugin manager and an official marketplace, and adds a -y flag to skip permission prompts and a much higher prompt cache hit rate.

### Changes

- **Interactive plugin manager**: type /plugin on its own to browse marketplaces and install or uninstall plugins in one step, with no name@marketplace to remember; plugins can also be installed and removed by name alone.
- **Official marketplace**: the official marketplace is now the default, marketplaces sync at startup and newly added plugins install automatically; plugins can come from a git subdirectory.
- **Skip permission prompts**: new --dangerously-skip-permissions (short form -y), shown as a red BYPASS badge in the footer.
- **Higher cache hit rate**: the system prompt stays identical across launches and compaction no longer fires needlessly, lifting the cache hit rate from about 79% to about 96%.
- **AGENTS.md support**: a project's AGENTS.md is read as project instructions; new /guide to ask how to use AtomCode; configurable hooks, including webhooks.
- **Fixes**: a failed turn shows "interrupted" instead of the success banner; the input box no longer flickers while streaming; pasting tab-indented text no longer misplaces the cursor; shell prompt characters are stripped from pastes; Windows no longer steers the model toward bash syntax; HarmonyOS self-update picks the right package; CodingPlan reports an exhausted monthly quota correctly; a failing UserPromptSubmit hook no longer blocks the conversation.

## v4.23.3 (2026-05-28)

### Overview

A fix update focused on display and Chinese output in Windows terminals, and on how much context thinking models use in long turns.

### Changes

- **Windows terminal improvements**: fixes duplicated Chinese characters and a flickering input box in PowerShell 7 and similar setups; Chinese text in bash output is no longer garbled.
- **Long turns stay within context**: compaction keeps recent content by token count, so models with heavy thinking output no longer overflow the context window.
- **CodingPlan usage notice**: when the monthly quota is used up, a single line says how many days to wait, and /status shows durations the same way as /login.
- **Fixes**: slash commands go into input history and come back with Up; web searches stop immediately on Ctrl+C; a stray {} at the end of a path is removed automatically; a 1M context window shows as 1m; Markdown table borders and widths are measured correctly; uninstall no longer triggers an automatic update first.

## v4.23.2 (2026-05-28)

### Overview

This release mostly improves the terminal interface: history flows into the terminal's native scrollback and the mouse wheel, selection and copy go back to the terminal. It also fixes many Windows display problems, and destructive commands always need approval.

### Changes

- **Native scrolling and copy**: AtomCode no longer captures the mouse, so the wheel scrolls through history and selection and copy work the terminal's own way.
- **Windows display fixes**: duplicated or shifted characters in Chinese locales and laggy menu navigation are fixed, and wide characters and emoji are measured more accurately.
- **Destructive actions always ask**: dangerous commands such as rm -rf or a force push, and edits to sensitive files such as .env or keys, are never skipped because of a session-wide grant.
- **Install with npm**: AtomCode can now be installed through npm, including on HarmonyOS.
- **Faster startup**: plugin and skills marketplace setup runs in the background, so the input box is ready sooner and new skills appear without a restart.
- **More models supported**: tool calls that Qwen3 and similar models write as text are recognised instead of shown as plain text.
- **Clearer CodingPlan usage**: /codingplan status shows usage per time window and says plainly when the monthly quota is used up.
- **Fixes**: sessions resumed after compression still begin with the original request; reading large files uses less memory; cancelling or timing out ends bash's child processes too; \t and similar sequences in Windows paths are no longer read as escapes; skills refresh after /setup, and /plugin gains reload; table rows with code or \| split correctly; startup no longer crashes when both tokens have expired.

## v4.23.1 (2026-05-24)

### Overview

A fix update: approval prompts are clearer and more reliable, reading outside the workspace needs confirmation, and an occasional long bash hang is fixed.

### Changes

- **Better approval prompts**: prompts show the replacement details and wrap in narrow windows so Y/A/N stay visible; the input box no longer disappears after you answer; while waiting the spinner says it is waiting for approval instead of counting up.
- **Stricter permissions**: actions that always need approval can no longer be passed by a session-wide grant; searching, diagnostics and symbol listing outside the workspace also ask first.
- **Bash hang fixed**: the workspace snapshot taken around each command now times out instead of holding a command up for minutes.
- **Consistent ATOMCODE_HOME**: when set, ATOMCODE_HOME is used as the config directory itself, without an extra .atomcode level.
- **Fixes**: Tab switches modes even with text in the input box; an expired token is refreshed and retried once, and silent refreshes no longer type into the input box; Enter on the QR step opens the link in a browser; headless mode no longer breaks thinking output onto a line per token; paths starting with ~ resolve correctly; automatic upgrades for Linux ARM64.

## v4.23.0 (2026-05-21)

### Overview

This release rebuilds first launch around a WeChat QR sign-in that also claims CodingPlan on one page, and adds /setup project recommendations, light and dark themes, and syntax highlighting in code blocks.

### Changes

- **QR quick start**: first launch shows a QR code; scan it with WeChat and AtomCode detects completion, saves the sign-in, claims CodingPlan and opens the interface.
- **/setup recommendations**: analyses the current project and recommends skills, MCP servers, hooks and more, installed in one step; also available as atomcode setup on the command line.
- **Light and dark themes**: colours follow the terminal's background automatically, with higher contrast on light backgrounds; code blocks are syntax-highlighted by language.
- **Default skills marketplace**: the official skills marketplace is installed on first launch, and installed marketplaces update after an upgrade.
- **Open files for preview**: a new open_file tool opens generated pages, PDFs and images in the system's default app, and says so plainly over SSH or without a display.
- **/keys shortcut reference**: lists every keyboard shortcut and notes which newline combinations your terminal may not pass through; in multi-line input Up and Down move between lines before browsing history.
- **Safer git commands**: force pushes, history rewrites, interactive rebases, forced checkouts or branch deletions, and skipping hooks now need approval.
- **Fixes**: approval prompts tell apart parallel calls on files with the same name; tools already allowed for the session no longer prompt again; upstream errors read more clearly (429 shortened to one line); the total request timeout rises to 30 minutes; finish notifications in Ghostty; the VS Code extension gains sessions in multiple tabs, bulk session deletion and a workspace file picker.

## v4.22.3 (2026-05-18)

### Overview

This release adds background session commands, launches the redesigned website, and fixes a few problems that got in the way.

### Changes

- **Background sessions**: the new /bg commands run a session in the background, list background sessions, and bring one back.
- **Redesigned website**: atomcode.atomgit.com has a new look.
- **Fixes**: the /codingplan 401 error; Ctrl+C failing to copy a selection on Windows; WebFetch parsing errors; cd switching directories by itself.
