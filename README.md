<div align="center">
<pre>
      _   _                  ____          _
     / \ | |_ ___  _ __ ___ / ___|___   __| | ___
    / _ \| __/ _ \| '_ ` _ \ |   / _ \ / _` |/ _ \
   / ___ \ || (_) | | | | | | |__| (_) | (_| |  __/
  /_/   \_\__\___/|_| |_| |_|\____\___/ \__,_|\___|
</pre>
</div>

<p align="center">
  <strong>Open-source terminal AI coding agent written in Rust</strong>
</p>

<p align="center">
  English · <a href="./README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <a href="#installation">Install</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#features">Features</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#development">Development</a> ·
  <a href="#contributing">Contributing</a> ·
  <a href="#community">Community</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-5.0.6-blue" alt="version">
  <img src="https://img.shields.io/badge/rust-1.88%2B-orange" alt="rust">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="license">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20HarmonyOS%20PC%20%7C%20Windows-lightgrey" alt="platform">
  <a href="https://atomgit.com/atomgit_atomcode/atomcode" target="_blank">
    <img src="https://atomgit.com/atomgit_atomcode/atomcode/star/badge.svg" alt="AtomGit Star"/>
  </a>
</p>

---

> **This project is 100% AI-generated.** Every line of code, every architectural decision's implementation, and every commit was written by AI. The human developer serves solely as the decision-maker and product manager — defining what to build, not how to build it.

---

AtomCode is an AI coding agent that lives in your terminal. Give it a task in natural language, and it will read your codebase, edit files, run commands, and verify its work — autonomously.

Think of it as an open-source alternative to Claude Code / Cursor Agent, but running entirely in your terminal and connecting to any OpenAI-compatible API.

## Features

### Agent Loop

- **Autonomous multi-step execution** — reads files, edits code, runs tests, fixes errors, all in a loop
- **Verification loop** — automatically verifies edits via syntax checks before declaring success
- **Dynamic step budget** — scales with the number of edited files, capped per turn to bound cost
- **Loop detection** — detects and breaks out of repetitive tool-call patterns
- **3-layer JSON repair** — recovers malformed tool-call arguments
- **Turn-level datalog** — structured per-turn logs for replay, debugging, and eval harnesses

### Modes & Autonomy

- **Plan / Build modes** — `/plan` switches to read-only exploration (the agent investigates without touching files); `/build` switches back to full execution
- **Goal mode** — `/goal <text>` sets a completion condition and the agent loops autonomously, turn after turn, until the goal is met
- **Code review** — `/review` reviews your current changes, `/review staged` the staged diff, and `/review <base>` against a base ref
- **Background sessions** — `/bg` runs work in detached slots so you can keep using the TUI while a long task progresses

### Built-in Tools

File & shell:

- `read_file`, `write_file`, `edit_file`, `search_replace`
- `bash`, `grep`, `glob`, `list_directory`, `change_dir`
- `web_search`, `web_fetch`

Code graph (language-aware code intelligence):

- `list_symbols`, `read_symbol`, `find_references`
- `trace_callers`, `trace_callees`, `trace_chain`
- `file_deps`, `blast_radius`

Automation:

- `auto_fix` — automatic lint/typecheck fix loop
- `use_skill` — invoke a user-defined skill

### Multi-Provider Support

Connect to any LLM that supports OpenAI's function-calling API:

| Provider | Function Calling | Tested Models |
|----------|:---:|---|
| Claude (Anthropic) | Yes | Claude Sonnet 4.5/4.6, Opus 4.6 |
| OpenAI | Yes | GPT-4o, GPT-4.1 |
| DeepSeek | Yes | DeepSeek V3, DeepSeek R1, DeepSeek V4 |
| Zhipu (GLM) | Yes | GLM-4, GLM-5, GLM-5.2 (AtomCode Pro plan exclusive model) |
| Qwen (Alibaba) | Yes | Qwen-Plus, Qwen-Max |
| SiliconFlow | Yes | Various open models |
| Ollama (local) | Partial | Llama 3, Qwen2, etc. |
| Any OpenAI-compatible API | Yes | — |

### Sessions & Login

- **Persistent sessions** — every conversation is saved; continue the last session with `atomcode --continue` / `-c`, or resume/switch inside the TUI with `/resume`
- **AtomGit OAuth login** — `/login` (or `atomcode login`) pairs your CLI with your AtomGit account
- **SSO login** — `/login-with-sso` for GitCode internal users
- **Headless mode** — `atomcode -p "..."` runs a single prompt non-interactively and streams the reply on stdout (Claude Code `-p` style); approval-required `bash` calls are auto-approved, while other approval-required tools are denied
- **Daemon mode** — `atomcode-daemon` exposes an HTTP API for session history and SSE streaming chat

### Terminal UI

- **Real-time streaming** with markdown rendering and syntax highlighting
- **Code blocks** with language labels, line numbers, and `base16-ocean.dark` theme
- **Multi-line input** with Shift+Enter (or `\` + Enter), auto-growing height, input history
- **Task completion notifications** — long-running tasks trigger terminal-native notifications first (kitty / WezTerm / iTerm2), falling back to OS-native alerts
- **Text selection** with mouse drag, auto-scroll, and clipboard copy
- **Slash commands** — `/model`, `/provider`, `/resume`, `/bg`, `/diff`, `/undo`, `/cost`, `/clear`, `/compact`, etc. (see table below)
- **File attachment** — paste file paths to attach content as context
- **Bracketed paste** — long paste content collapsed to a compact indicator
- **Skills** — user-defined commands loaded from your skill directory, invoked like any slash command

### Web UI

- **`/webui`** (in the TUI) or **`atomcode webui`** (CLI) launches a local browser UI as an alternative to the terminal interface — same agent, same sessions, rendered in your browser
- **Loopback only** — the server binds to `127.0.0.1` and uses a one-time token; nothing is exposed to the network
- **`/webui stop`** stops the in-process server (a later `/webui` restarts it)

### App Remote Access

- **`/app`** (in the TUI) enables mobile remote access — prints a QR code; scan it with the GitCode mobile app from any network to connect to your current session
- **Any-network reachable** — your PC connects to a public relay via a reverse WSS tunnel; the phone reaches your PC through the relay. No public IP, DDNS, or port forwarding required
- **Bidirectional real-time sync** — messages from either end appear on the other in real time (streaming replies, tool call cards, token usage)
- **Remote commands** — the phone can run `/status`, `/cost`, `/diff`, `/whoami` etc., which execute on the desktop and echo results back
- **Switch projects / sessions** — switch projects or open a history session on the phone, and the desktop follows immediately
- **Model sync** — switching models on either end keeps the other in sync
- **`/app stop`** disconnects remote access

### Safety

- **Destructive command detection** — `rm -rf`, `git push --force`, `DROP TABLE`, etc. require explicit approval
- **Path-aware confirmations** — external reads, sensitive paths, and all writes outside the workspace can require confirmation depending on risk level
- **Sensitive file protection** — protected system paths, credential directories, shell configs, `.env` files, and key/cert files receive stronger confirmation rules
- **Shell bypass protection** — common shell file commands like `cat`, `head`, `ls`, `cp`, `mv`, and `tee` inherit the same path approval model as file tools
- **Per-session permission grants** — approve once per tool pattern, or always-allow
- **Source file deletion requires approval** — `rm` on code files is never auto-approved
- **Undo** — `/undo` rolls back the last turn's file edits via file-history snapshots

See [Permission Model](./docs/security/permission-model.md) for the full design and current boundaries.

### Privacy

- 📊 Anonymous telemetry (opt-out) — see [docs/telemetry.md](docs/telemetry.md)

## Installation

### From Source (recommended)

```bash
git clone https://atomgit.com/atomgit_atomcode/atomcode.git
cd atomcode
cargo install --path crates/atomcode-cli --locked
```

The binary will be generated at `target/release/atomcode` and installed to
`~/.cargo/bin/atomcode` for macOS / Linux / HarmonyOS PC and `$env:USERPROFILE/.cargo/bin/atomcode.exe`
for Windows. Make sure that `~/.cargo/bin` (or `%USERPROFILE%\.cargo\bin` on Windows) is
in your `PATH`.

To compile without installing, run:

```bash
cargo build --release
```

and the binary will be generated at `target/release/atomcode`.

### Package Managers

AtomCode CLI can also be installed via the following package managers:

```bash
# Install using npm
npm install -g @atomgit.com/atomcode

# Install using Homebrew
brew install --cask atomcode
```

### Shell Completion

AtomCode can generate completion scripts for Bash, Zsh, Fish, PowerShell, and
Elvish. For example:

```bash
# Bash (current session)
source <(atomcode completion bash)

# Zsh (persistent)
mkdir -p ~/.zfunc
atomcode completion zsh > ~/.zfunc/_atomcode
# Also add `fpath=(~/.zfunc $fpath)` before `compinit` in ~/.zshrc.

# Fish (persistent)
mkdir -p ~/.config/fish/completions
atomcode completion fish > ~/.config/fish/completions/atomcode.fish
```

For PowerShell, run `atomcode completion powershell | Out-String |
Invoke-Expression`. Run `atomcode completion --help` for the complete shell
list. This affects command-line completion only and does not change the TUI's
`Tab` mode switching.

### Requirements

- Rust 1.88+ (for building; older Cargo versions cannot parse the current lockfile)
- An API key from any supported provider (or an AtomGit account for `/login`)

### Permissions — don't run with `sudo`

Run AtomCode as your **normal user**, never with `sudo`. AtomCode keeps its
config, sessions, and logs under `~/.atomcode`; running once as root leaves
root-owned files there, so every later non-root start fails at runtime init with:

```
coding runtime assemble failed: Permission denied (os error 13)
```

(the message may say `prepare` instead of `assemble` — same cause.) If you hit
this, reclaim ownership and stop using `sudo`:

```bash
sudo chown -R "$(id -un):$(id -gn)" ~/.atomcode
atomcode        # start WITHOUT sudo
```

On a Linux guest, a working directory on a VirtualBox shared folder
(`/media/sf_*`, owned by `root:vboxsf`) can also trigger permission errors — add
yourself to the group with `sudo usermod -aG vboxsf "$USER"` and re-login, rather
than using `sudo`.

### Uninstall

Remove AtomCode and (optionally) its data:

```bash
atomcode uninstall                # interactive: per-group prompts
atomcode uninstall --keep-data    # only remove binary + PATH edit
atomcode uninstall --purge        # remove everything, including ~/.atomcode
atomcode uninstall --dry-run      # show plan, change nothing
```

If the binary is already broken or missing:

```bash
curl -fsSL https://raw.atomgit.com/atomgit_atomcode/atomcode/raw/main/scripts/uninstall.sh | sh
# Windows:
irm https://raw.atomgit.com/atomgit_atomcode/atomcode/raw/main/scripts/uninstall.ps1 | iex
```

By default credentials (`auth.toml`, `mcp.json`, `config.toml`, `ATOMCODE.md`) are kept; pass `--purge` to remove them too.

## Quick Start

### 1. First Run

```bash
atomcode
```

On first run, a setup wizard will guide you through configuring your LLM provider:

```
Welcome to AtomCode! Let's set up your first provider.

Select provider:
  [1] Claude (Anthropic)
  [2] OpenAI
  [3] OpenAI Compatible (DeepSeek, Qwen, Zhipu, Moonshot...)
  [4] Ollama (local)
```

### 2. Configuration

Config is stored at `~/.atomcode/config.toml`. A minimal single-provider
setup looks like this:

```toml
default_provider = "deepseek"

[providers.deepseek]
type           = "openai"
api_key        = "sk-..."
model          = "deepseek-chat"
base_url       = "https://api.deepseek.com/v1"
context_window = 64000
```

You can declare multiple providers and switch between them with `/model`
or `/provider`. A **complete reference** covering Claude / OpenAI /
OpenAI-compatible endpoints (DeepSeek, GLM, SiliconFlow, OpenRouter...) /
Ollama, plus the `[datalog]` section, lives at
[`docs/config.example.toml`](docs/config.example.toml) — copy and edit the
bits you need.

After editing `config.toml` by hand, run `/reload` inside atomcode to pick
up the changes without restarting.

### 3. Start Coding

```bash
# Open in your project directory
cd your-project
atomcode

# Or specify directory
atomcode -C /path/to/project

# Or specify model
atomcode --model gpt-4o

# Headless (single prompt, reply on stdout)
atomcode -p "Explain the agent loop in this repo"

# Read prompt from file
atomcode --prompt-file task.md
```

In headless mode, approval-required `bash` calls are auto-approved and logged to stderr; other approval-required tools are denied.

Then just type what you want:

```
> Fix the login bug where users get redirected to 404 after OAuth callback

> Add a dark mode toggle to the settings page

> Refactor the database module to use connection pooling

> Write tests for the payment processing module
```

## Keybindings

### Input

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Shift+Enter` | New line (requires Kitty keyboard protocol) |
| `Ctrl+Enter` | New line (requires Kitty keyboard protocol) |
| `Ctrl+J` | New line (when the terminal distinguishes the chord) |
| `Alt+Enter` | New line (most terminals; see compatibility note below) |
| `\` + `Enter` | New line (works on all terminals — type a `\` and press Enter; the `\` is consumed) |
| `Esc` | Clear input / Cancel stream |
| `Esc` ×2 | Undo the previous turn |
| `Up/Down` | Browse input history |
| `Tab / Shift+Tab` | Accept completion when a menu is open; otherwise cycle next / previous mode |
| `F2 / Shift+F2` | Switch to next / previous model (usually `Fn+F2 / Fn+Shift+F2` on Mac) |
| `Ctrl+R` | Reverse-search input history |
| `Ctrl+T` | Cycle `reasoning_effort` |
| `Ctrl+U` | Clear line |
| `Ctrl+W` | Delete word |
| `Ctrl+K` | Delete to end of line |
| `Ctrl+V / Ctrl+Alt+V` | Paste text or image from clipboard (Windows can also use `/paste`) |

> **Terminal compatibility for newline chords:**
> - `Shift+Enter` and `Ctrl+Enter` need a terminal that speaks the Kitty keyboard protocol — kitty, WezTerm, Alacritty, iTerm2 ≥3.5, Windows Terminal ≥1.21. Older terminals (and Windows, where atomcode doesn't enable the protocol) collapse them to plain `Enter` (which sends the message) — use `\` + `Enter`, which works everywhere.
> - AtomCode enables the Kitty keyboard protocol only for known-compatible terminals. Generic web terminals such as JumpServer use legacy key reporting by default. Set `ATOMCODE_KITTY=1` to force it on or `ATOMCODE_KITTY=0` to force it off.
> - `Alt+Enter` works at the byte level on most terminals, but **Windows Terminal binds it to "toggle full screen" by default** — remove that binding under Settings → Actions to free it up.
> - Xshell does not support the Kitty protocol; in its keymap settings, map a free chord to send `ESC, Enter` (`\x1b\r`) to get the same effect, or paste multi-line text via the clipboard (bracketed paste is enabled).

> **Pasting images on Windows:**
> Windows Terminal and conhost bind `Ctrl+V` to their own `paste` action, which only forwards `CF_UNICODETEXT` from the clipboard — an image-only clipboard sends nothing, so the in-app `Ctrl+V` handler never fires. Two ways out:
> 1. Use **`/paste`** — the slash command pulls the clipboard image and attaches it as `[Image #N]`. Works in every terminal, including Windows Terminal, PowerShell 7, conhost, and git bash. The TUI's bottom-right hint on Windows says `Image in clipboard · /paste` automatically.
> 2. If you want `Ctrl+V` muscle memory: open Windows Terminal `settings.json` (`Ctrl+,` → "Open JSON file") and either delete the `{ "command": "paste", "keys": "ctrl+v" }` entry under `"actions"`, or rebind it to `ctrl+shift+v`. After a restart, `Ctrl+V` passes through to atomcode.
>
> Git Bash (MinTTY) doesn't intercept `Ctrl+V`, so it works there out of the box.

### Navigation

| Key | Action |
|-----|--------|
| `Shift+Up/Down` | Scroll chat one line |
| `PageUp/PageDown` | Scroll chat 10 lines |
| `Alt+Up/Down` | Jump to previous / next message |
| `Ctrl+Up/Down` | Jump to previous / next user message |
| Empty input + `Home/End` | Jump to top / bottom of conversation |
| `Ctrl+Shift+C` | Copy selection |
| `Ctrl+C` | Cancel operation (double-tap to exit) |

### Slash Commands

Type `/` in the TUI to browse the full list with live completion; `/help` shows commands and shortcuts.

**Sessions & workspace**

| Command | Action |
|---------|--------|
| `/resume` | Resume or switch session |
| `/session` | Start a new session |
| `/rename <name>` | Rename the current session |
| `/clear` | Start a new conversation (clears context + screen) |
| `/bg` | Background current session; subcommands: `/bg list`, `/bg <N>`, `/bg drop <N>`, `/bg help` |
| `/background <task>` | Compatibility alias: start a one-shot task in a `/bg` slot |
| `/cd` | Change working directory and start a new session |
| `/worktree` | Git worktree isolation (`create` / `list` / `done` / `cleanup`) |
| `/webui` | Launch the browser webui (subcommands: `stop`, `lan`, `--host <addr>`) |
| `/sync` | Attach to the live webui session (`/sync off` to detach) |

**Modes, autonomy & review**

| Command | Action |
|---------|--------|
| `/plan` | Switch to Plan mode (read-only exploration) |
| `/build` | Switch to Build mode (full execution) |
| `/goal <text>` | Set a completion goal — the agent loops autonomously until it's met |
| `/review` | Code review the current changes (`/review` · `/review staged` · `/review <base>`) |
| `/think` | Control extended thinking (on / off / budget N) |
| `/effort` | DeepSeek reasoning effort control (high / max / off) |

**Providers & account**

| Command | Action |
|---------|--------|
| `/model` | Switch model / provider |
| `/provider` | Manage providers (add / edit / delete) |
| `/proxy` | Switch outbound proxy mode |
| `/login` | Sign in with AtomGit OAuth and claim CodingPlan free models |
| `/logout` | Sign out of AtomGit |
| `/whoami` | Show the current logged-in user |
| `/status` | Show login status and model info |

**Files, edits & context**

| Command | Action |
|---------|--------|
| `/diff` | Show git diff of current changes |
| `/undo` | Undo a turn's file edits (`/undo` or `/undo N`) |
| `/view <filepath>` | View file content in an overlay modal |
| `/paste` | Attach an image from the clipboard (Windows fallback for Ctrl+V) |
| `/copy` | Copy a code block from the last reply (`/copy`, `/copy N`, `/copy all`) |
| `/cost` | Show token usage for this session |
| `/context` | Show the context budget breakdown |
| `/compact` | Compact conversation history |

**Memory**

| Command | Action |
|---------|--------|
| `/remember <fact>` | Save a fact to memory (`--global` for all projects) |
| `/forget <query>` | Remove matching memories |
| `/memory` | Show all saved memories |

**Extensions**

| Command | Action |
|---------|--------|
| `/mcp` | MCP server status (subcommands: `reload`, `tools`, `login`, `logout`) |
| `/plugin` | Plugin marketplace (`marketplace` / `install` / `uninstall` / `list`) |
| `/skills` | Browse loaded skills |

**Project & system**

| Command | Action |
|---------|--------|
| `/init` | Generate `.atomcode.md` project instructions from the working directory |
| `/config` | Show config path |
| `/reload` | Reload `~/.atomcode/config.toml` from disk |
| `/upgrade` | Upgrade atomcode to latest (subcommand: `rollback`) |
| `/setup` | First run: install the recommended skill and run it |
| `/welcome` | Re-run the onboarding wizard |
| `/language` | Switch display and default Git commit-message language |
| `/guide <question>` | Ask atomcode-guide how to use AtomCode |
| `/keys` | Show keyboard shortcuts |
| `/help` | Show commands & shortcuts |
| `/quit`, `/exit` | Exit AtomCode (or Ctrl+C ×2) |

> **AtomGit Issues.** `/issue` has been removed. After `/login`, ask in natural
> language—for example, “Create an AtomGit issue for this bug”—and AtomCode uses
> its built-in `atomgit_issue` tool. Reading issues is automatic; creating an
> issue or adding, editing, or deleting comments still requires approval.
>
> **Plugin commands.** Beyond the built-ins above, plugins can register their own slash commands. For example, install the official channel plugin to get `/wechat` (shows the AtomCode WeChat community group QR code):
>
> ```text
> /plugin marketplace add https://atomgit.com/atomgit_atomcode/AtomCode-Channel
> /plugin install weixin@atomcode-channel
> ```

### Custom Commands

Beyond built-ins and plugin commands, you can define your own slash commands as Markdown template files — perfect for prompt patterns you use frequently.

**Locations** (lowest to highest priority):

| Location | Scope |
|---------|-------|
| `$ATOMCODE_HOME/commands/` (default `~/.atomcode/commands/`) | Global — applies to every project |
| `<project>/.atomcode/commands/` | Project-level — overrides same-named global commands |
| `plugins/<name>/commands/` | Plugin-contributed — installed via `/plugin install` |

**File format:**

```markdown
---
name: explain
description: Explain how a specific function or module works
args: required
---

Explain the following code in detail:

$ARGUMENTS

Cover: function signature & parameters, core business logic, data flow & side effects.
```

- **`name`** — Required. The command name; `/explain` triggers it.
- **`description`** — Optional. Shown in Tab completion.
- **`args`** — Optional. Controls argument expectation and UX:

  | Value | Menu Enter | Empty-arg submit |
  |-------|-----------|------------------|
  | `none` (default) | Execute immediately | Accepted (substitutes `""`) |
  | `optional` | Complete to `/name `, wait for input | Accepted |
  | `required` | Complete to `/name `, wait for input | Rejected with error message |

  The template variable `$ARGUMENTS` / `${ARGUMENTS}` is always replaced with whatever the user types after the command name (empty string if nothing is typed).

- **Template body** — The prompt sent to the AI when the command is invoked. `$ARGUMENTS` or `${ARGUMENTS}` is replaced with whatever the user types after the command name.

**Example: create a code-review command**

```bash
mkdir -p .atomcode/commands

cat > .atomcode/commands/codereview.md << 'EOF'
---
name: codereview
description: Review the current git diff
args: optional
---

Review all changes in the current git diff.
If specific files are given, review only: $ARGUMENTS
EOF
```

Run `/help commands` to list all loaded custom commands.

> **Priority rule.** A custom command cannot shadow a built-in command with the same name. If a built-in `/review` already exists, a project-level `review.md` won't appear in completion or dispatch.

## Architecture

AtomCode is a layered Rust workspace:

```
atomcode/
  crates/
    atomcode-kernel/        # Neutral agent loop and runtime traits
    atomcode-capabilities/  # Providers, tools, MCP, skills, sessions, memory
    atomcode-coding/        # Coding specialization and CodingRuntime lifecycle
    atomcode-review/        # Review specialization
    atomcode-tuix/          # Terminal UI
    atomcode-cli/           # TUI and headless entry point
    atomcode-daemon/        # HTTP/SSE/WebSocket transport + legacy session importer
```

The coding path is `CLI/TUI/daemon → CodingRuntime → kernel`. The retired core
agent protocol and `atomcode-bridge` are no longer part of the runtime path.

### Design Principles

1. **Tech-stack agnostic** — never hardcodes language-specific logic. Detects project type dynamically from descriptor files (`package.json`, `Cargo.toml`, `pyproject.toml`, `pom.xml`, etc.).

2. **Single runtime owner** — `CodingRuntime` owns the live coding agent, provider/session lifecycle, pending requests, snapshots, and controllers. Drivers handle input, presentation, and transport without rebuilding a second agent runtime.

3. **Tool safety** — all destructive operations require explicit user approval. Tool failures become LLM observations, never panics.

4. **Context-aware** — token-budget-aware conversation windowing, project file-tree injection, and per-turn system reminders keep the model focused without exceeding context limits.

5. **Directed dependencies** — kernel stays neutral; capabilities and coding stay free of `atomcode-core`; legacy session data is handled at an explicit compatibility boundary rather than as a runtime fallback.

## Project Instruction File

Create a `.atomcode.md` file in your project root to give AtomCode persistent context:

```markdown
# Project Instructions

This is a Vue 3 + TypeScript project using Pinia for state management.

- Always use Composition API with `<script setup>`
- Use TailwindCSS for styling, no inline styles
- Run `npm run lint` after editing .vue/.ts files
```

AtomCode reads this file automatically and includes it in the system prompt. AtomCode also supports `AGENTS.md` (the [open standard](https://agents.md/) for AI coding agents) as an alternative — if both files exist, `.atomcode.md` takes priority.

## Development

### Prerequisites

- **Rust 1.88+** — install via [rustup](https://rustup.rs/)
- **Git**
- A supported LLM provider API key (for runtime testing)

### Build from Source

```bash
git clone https://atomgit.com/atomgit_atomcode/atomcode.git
cd atomcode

# Debug build (fast compilation, slower runtime)
cargo build

# Release build (slower compilation, optimized binary)
cargo build --release
```

### Run in Development

```bash
# Run the TUI directly (debug mode)
cargo run -p atomcode-cli

# With arguments
cargo run -p atomcode-cli -- -C /path/to/project
cargo run -p atomcode-cli -- --model gpt-4o

# Headless mode
cargo run -p atomcode-cli -- -p "summarize this repo"

# Daemon (HTTP API)
cargo run -p atomcode-daemon
```

### Testing

```bash
# Run all tests
cargo test

# Run tests for a specific crate
cargo test -p atomcode-capabilities
cargo test -p atomcode-tuix

# Run a specific test
cargo test -p atomcode-capabilities test_name
```

### Useful Commands

```bash
# Check compilation without building
cargo check

# Format code
cargo fmt

# Run linter
cargo clippy

# Build and install to ~/.cargo/bin
cargo install --path crates/atomcode-cli
```

## Contributing

Contributions are welcome! AtomCode is in active development.

### How to Contribute

1. **Fork** the repository on AtomGit
2. **Clone** your fork locally:
   ```bash
   git clone https://atomgit.com/<your-username>/atomcode.git
   cd atomcode
   ```
3. **Create a branch** for your change:
   ```bash
   git checkout -b feat/your-feature
   # or
   git checkout -b fix/your-bugfix
   ```
4. **Make your changes**, ensure the project builds and tests pass:
   ```bash
   cargo build && cargo test && cargo clippy
   ```
5. **Commit** with a clear message:
   ```bash
   git commit -m "feat: add xxx support"
   ```
6. **Push** and open a **Pull Request** against `main`

### Branch Naming

| Prefix | Purpose |
|--------|---------|
| `feat/` | New feature |
| `fix/` | Bug fix |
| `refactor/` | Code refactoring (no behavior change) |
| `docs/` | Documentation only |
| `chore/` | Build, CI, tooling changes |

### Guidelines

- Follow the project's core principles — especially **tech-stack neutrality**
  (no language/framework-specific logic in the core engine; detect via probes
  like `package.json` / `Cargo.toml` / `pom.xml` and route through adapters)
- All tool failures must be graceful — return the error as an observation to the LLM, never panic
- Destructive operations must require user approval
- Keep the system prompt compact (~1.5K tokens)
- Run `cargo fmt` and `cargo clippy` before submitting

### Where to Start

- **Add a new tool** — implement the `Tool` trait in `crates/atomcode-capabilities/src/tools/`
- **Add a new provider** — implement `LlmProvider` in `crates/atomcode-capabilities/src/provider/`
- **Improve the UI** — rendering lives in `crates/atomcode-tuix/src/render/`
- **Fix bugs** — check [Issues](https://atomgit.com/atomgit_atomcode/atomcode/issues) for open bugs

### Non-Rust Contributions

Don't know Rust? No problem! There are many ways to contribute without writing Rust code:

- **📝 Documentation** — Improve the README, fix typos, enhance the [official docs site](https://atomcode.atomgit.com/docs/en/), or add examples. Docs live in the root `docs/` directory, `site/docs/`, and the main README files.
- **🌐 Localization & Translation** — Help translate the docs site, README, or UI strings into more languages. Check `site/docs/` for existing translations.
- **🧩 Skills & Plugins** — Create new [skills](https://gitcode.com/atomgit_atomcode/atomcode-skills) (Markdown + JSON, no Rust needed) that extend AtomCode's capabilities. Skills are loaded from `~/.atomcode/skills/`.
- **🐛 Bug Reports** — Found a bug? Open an [Issue](https://atomgit.com/atomgit_atomcode/atomcode/issues) with clear reproduction steps, screenshots, and environment info. High-quality bug reports are invaluable.
- **🧪 Test Cases & Examples** — Add test scenarios, example projects, or usage demos that help validate features and onboard new users.
- **💬 Community Support** — Help answer questions in the community group, write tutorials, or create video guides.

Every contribution, code or not, makes AtomCode better for everyone. When in doubt, open an Issue or start a Discussion!

## Community
---

Scan the QR code below with WeChat to join the AtomCode community group — share feedback, report issues, and talk to other users and maintainers:

<p align="center">
  <img src="https://cdn-news.gitcode.com/news/AtomCode_qun.png" alt="AtomCode WeChat community QR code" width="220">
</p>

## Donate
---

☕ AtomCode is free, and the Coding Plan is free too. If it's saved you a bit of time, consider buying the author a coffee — it keeps us motivated to keep making it better.

<p align="center">
  <img src="https://cdn-news.gitcode.com/news/alipay_1782981974317.png" alt="AtomCode Alipay donate QR code" width="220">
  <img src="https://cdn-news.gitcode.com/news/wechatpay_1782982603403.png" alt="AtomCode WeChat Pay donate QR code" width="240">
</p>

## License

MIT License. See [LICENSE](LICENSE) for details.

---

<p align="center">
  Built with Rust, ratatui, and a lot of late nights.
</p>
