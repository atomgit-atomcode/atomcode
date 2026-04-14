<p align="center">
  <pre align="center">
     _   _                  ____          _
    / \ | |_ ___  _ __ ___ / ___|___   __| | ___
   / _ \| __/ _ \| '_ ` _ \ |   / _ \ / _` |/ _ \
  / ___ \ || (_) | | | | | | |__| (_) | (_| |  __/
 /_/   \_\__\___/|_| |_| |_|\____\___/ \__,_|\___|
  </pre>
</p>

<p align="center">
  <strong>Open-source terminal AI coding agent written in Rust</strong>
</p>

<p align="center">
  <a href="#installation">Install</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#features">Features</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#development">Development</a> ·
  <a href="#contributing">Contributing</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-2.1.0-blue" alt="version">
  <img src="https://img.shields.io/badge/rust-1.75%2B-orange" alt="rust">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="license">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey" alt="platform">
</p>

---

> **This project is 100% AI-generated.** Every line of code, every architectural decision's implementation, and every commit was written by AI. The human developer serves solely as the decision-maker and product manager — defining what to build, not how to build it.

---

AtomCode is an AI coding agent that lives in your terminal. Give it a task in natural language, and it will read your codebase, edit files, run commands, and verify its work — autonomously.

Think of it as an open-source alternative to Claude Code / Cursor Agent, but running entirely in your terminal, connecting to any OpenAI-compatible API.

## Features

### Agent Loop

- **Autonomous multi-step execution** — reads files, edits code, runs tests, fixes errors, all in a loop
- **8 built-in tools**: `read_file`, `write_file`, `edit_file`, `bash`, `grep`, `glob`, `list_directory`, `change_dir`
- **Verification loop** — automatically verifies edits by running syntax checks before declaring success
- **Dynamic step limits** — 25 base + 5 per edited file, max 50 steps per turn
- **Loop detection** — detects and breaks out of repetitive tool call patterns
- **3-layer JSON repair** — handles malformed tool call arguments from weaker models

### Multi-Provider Support

Connect to any LLM that supports OpenAI's function calling API:

| Provider | Function Calling | Tested Models |
|----------|:---:|---|
| OpenAI | Yes | GPT-4o, GPT-4.1 |
| Claude (Anthropic) | Yes | Claude Sonnet 4.5/4.6, Opus 4.6 |
| DeepSeek | Yes | DeepSeek V3, DeepSeek R1 |
| Zhipu (GLM) | Yes | GLM-4, GLM-5 |
| Qwen (Alibaba) | Yes | Qwen-Plus, Qwen-Max |
| SiliconFlow | Yes | Various open models |
| Ollama (local) | Partial | Llama 3, Qwen2, etc. |
| Any OpenAI-compatible API | Yes | — |

### Terminal UI

- **Real-time streaming** with markdown rendering and syntax highlighting
- **Code blocks** with language labels, line numbers, and `base16-ocean.dark` theme
- **Multi-line input** with Shift+Enter, auto-growing height, input history
- **Text selection** with mouse drag, auto-scroll, and clipboard copy
- **Slash commands** — `/model`, `/provider`, `/clear`, `/compact` with autocomplete
- **File attachment** — paste file paths to attach content as context
- **Bracketed paste** — long paste content collapsed to a compact indicator

### Safety

- **Destructive command detection** — `rm -rf`, `git push --force`, `DROP TABLE`, etc. require explicit approval
- **Sensitive file protection** — writes to `/etc`, `~/.ssh`, shell configs require approval
- **Per-session permission grants** — approve once per tool pattern, or always-allow
- **Source file deletion requires approval** — `rm` on code files is never auto-approved

### Weak Model Optimization

AtomCode is specifically engineered to work well with weaker/cheaper models (DeepSeek V3, GLM-5, Qwen-Plus):

- **Compact system prompt** (~1.5K tokens) with rules at the END (recency effect)
- **No source file pre-reading** — the model reads what it needs, avoiding attention dilution
- **Token-budget-aware conversation windowing** — hot/cold zones with tool result condensation
- **System reminders** every 4 steps to keep the model on track
- **Specialized JSON repair** for models that produce malformed function call arguments
- **Edit surrounding context** — returns 10 lines around each edit to help the model stay oriented

## Installation

### From Source (recommended)

```bash
git clone https://gitcode.com/bangxu/atomcode.git
cd atomcode
cargo build --release
```

The binary will be at `target/release/atomcode`. Add it to your PATH:

```bash
# macOS / Linux
cp target/release/atomcode ~/.local/bin/
# or
sudo cp target/release/atomcode /usr/local/bin/
```

### Requirements

- Rust 1.75+ (for building)
- An API key from any supported provider

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
  [3] OpenAI Compatible (Deepseek, Qwen, Zhipu, Moonshot...)
  [4] Ollama (local)
```

### 2. Configuration

Config is stored at `~/.atomcode/config.toml`:

```toml
default_provider = "deepseek"

[providers.deepseek]
provider_type = "openai"
api_key = "sk-..."
model = "deepseek-chat"
base_url = "https://api.deepseek.com/v1"
context_window = 64000
```

You can configure multiple providers and switch between them with `/model` or `/provider`.

### 3. Start Coding

```bash
# Open in your project directory
cd your-project
atomcode

# Or specify directory
atomcode -C /path/to/project

# Or specify model
atomcode --model gpt-4o
```

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
| `Shift+Enter` | New line |
| `Esc` | Clear input / Cancel stream |
| `Up/Down` | Browse input history |
| `Tab` | Accept suggestion |
| `Ctrl+U` | Clear line |
| `Ctrl+W` | Delete word |
| `Ctrl+K` | Delete to end of line |

### Navigation

| Key | Action |
|-----|--------|
| `Ctrl+Up/Down` | Scroll chat (3 lines) |
| `PageUp/PageDown` | Scroll chat (page) |
| `Ctrl+L` | Clear conversation |
| `Ctrl+Shift+C` | Copy selection |
| `Ctrl+C` | Cancel operation (double-tap to exit) |

### Commands

| Command | Action |
|---------|--------|
| `/model` | Switch model |
| `/provider` | Manage providers |
| `/clear` | Clear conversation |
| `/compact` | Compact conversation history |

## Architecture

AtomCode is a Rust workspace with 3 crates:

```
atomcode/
  crates/
    atomcode-core/     # Headless library — no TUI dependency
      agent/           # AgentLoop: autonomous tool-use loop
      config/          # Config loading, provider configs
      conversation/    # Message types, windowed context
      provider/        # LlmProvider trait + OpenAI/Claude/Ollama
      tool/            # Tool trait + 8 tool implementations
      stream/          # StreamEvent protocol

    atomcode-tui/      # Terminal UI — ratatui + crossterm
      app.rs           # App state machine
      ui/              # Render: chat, input, status bar, markdown

    atomcode-cli/      # Binary entry point
      main.rs          # CLI args, first-run wizard, launch
```

### Design Principles

1. **Tech-stack agnostic** — never hardcodes language-specific logic. Detects project type dynamically from descriptor files (package.json, Cargo.toml, pyproject.toml, etc.)

2. **Decoupled agent** — `AgentLoop` runs as an independent async task, communicating with the TUI via channels (`AgentCommand` / `AgentEvent`). The core library has zero TUI dependencies.

3. **Tool safety** — all destructive operations require explicit user approval. Tool failures become LLM observations, never panics.

4. **Context-aware** — token-budget-aware conversation windowing, project file tree injection, and per-turn system reminders keep the model focused without exceeding context limits.

## Project Instruction File

Create a `.atomcode.md` file in your project root to give AtomCode persistent context:

```markdown
# Project Instructions

This is a Vue 3 + TypeScript project using Pinia for state management.

- Always use Composition API with `<script setup>`
- Use TailwindCSS for styling, no inline styles
- Run `npm run lint` after editing .vue/.ts files
```

AtomCode reads this file automatically and includes it in the system prompt.

## Comparison with Claude Code

| Feature | AtomCode | Claude Code |
|---------|:---:|:---:|
| Open source | Yes | No |
| Custom LLM provider | Yes (any OpenAI-compatible) | Claude only |
| Local model support | Yes (Ollama) | No |
| Terminal UI | Yes | Yes |
| Autonomous agent loop | Yes | Yes |
| File editing | Yes | Yes |
| Command execution | Yes | Yes |
| Safety approvals | Yes | Yes |
| MCP support | Planned | Yes |
| Multi-file context | Planned | Yes |
| Cost | Your API costs only | Subscription + API |

## Roadmap

- [ ] MCP (Model Context Protocol) server support
- [ ] Multi-file context window with smart selection
- [ ] Image/screenshot understanding (vision models)
- [ ] Git-aware context (branch, diff, blame)
- [ ] Plugin system for custom tools
- [ ] Conversation branching and checkpoints
- [ ] Persistent memory across sessions
- [ ] Web UI mode (optional browser interface)

## Development

### Prerequisites

- **Rust 1.75+** — install via [rustup](https://rustup.rs/)
- **Git**
- A supported LLM provider API key (for runtime testing)

### Build from Source

```bash
git clone https://gitcode.com/bangxu/atomcode.git
cd atomcode

# Debug build (fast compilation, slower runtime)
cargo build

# Release build (slower compilation, optimized binary)
cargo build --release
```

### Run in Development

```bash
# Run directly with cargo (debug mode)
cargo run

# Run with arguments
cargo run -- -C /path/to/project
cargo run -- --model gpt-4o

# Run release build
cargo run --release
```

### Testing

```bash
# Run all tests
cargo test

# Run tests for a specific crate
cargo test -p atomcode-core
cargo test -p atomcode-tui

# Run a specific test
cargo test -p atomcode-core test_name
```

### Project Structure

```
atomcode/
  Cargo.toml                 # Workspace root — version defined here
  crates/
    atomcode-core/           # Headless library (no TUI dependency)
      src/
        agent/               # AgentLoop: autonomous tool-use loop
        config/              # Config loading, provider configs
        conversation/        # Message types, windowed context
        provider/            # LlmProvider trait + OpenAI/Claude/Ollama
        tool/                # Tool trait + built-in tool implementations
        stream/              # StreamEvent protocol
    atomcode-tui/            # Terminal UI (ratatui + crossterm)
      src/
        app.rs               # App state machine
        ui/                  # Render: chat, input, status bar, markdown
    atomcode-cli/            # Binary entry point
      src/
        main.rs              # CLI args, first-run wizard, launch
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

1. **Fork** the repository on GitHub
2. **Clone** your fork locally:
   ```bash
   git clone https://gitcode.com/<your-username>/atomcode.git
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

- Follow the principles in `ATOMCODE.md` — especially **tech-stack neutrality**
- All tool failures must be graceful — return error as observation to the LLM, never panic
- Destructive operations must require user approval
- Keep the system prompt compact (~1.5K tokens)
- Run `cargo fmt` and `cargo clippy` before submitting

### Where to Start

- **Add a new tool** — implement the `Tool` trait in `crates/atomcode-core/src/tool/`
- **Add a new provider** — implement `LlmProvider` in `crates/atomcode-core/src/provider/`
- **Improve the UI** — rendering lives in `crates/atomcode-tui/src/ui/`
- **Fix bugs** — check [Issues](https://gitcode.com/bangxu/atomcode/issues) for open bugs

## License

MIT License. See [LICENSE](LICENSE) for details.

---

<p align="center">
  Built with Rust, ratatui, and a lot of late nights.
</p>
