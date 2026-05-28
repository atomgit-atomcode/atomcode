# AtomCode Telemetry

AtomCode ships anonymous usage telemetry by default. This page tells you what is
collected, why, and how to turn it off.

## Summary

- **Default:** enabled. **Anonymous:** yes. **Opt-out:** four ways (below).
- **Where it goes:** `https://acs.atomgit.com/api/v1/events` (our self-hosted server).
- **Retention:** 90 days raw, indefinite aggregates.

## What we send

Exactly 7 event types, each with a common "envelope" of identifiers/metadata.

### Envelope (on every event)

| Field | Meaning |
|---|---|
| `device_id` | UUIDv4 generated on first run, stored at `~/.atomcode/device_id`. Persists across login/logout. Resets only if you delete `~/.atomcode/`. |
| `account_id` | Your AtomGit user ID — only included when logged in. |
| `session_id` | Per process launch (CLI) or per conversation session (daemon). |
| `mode` | Event source: `headless` (non-interactive CLI), `tui` (interactive CLI), `ide` (daemon process serving IDE integrations). |
| `turn_id` | Per agent turn (inside one LLM interaction). |
| `ts`, `schema_version`, `app_version`, `os`, `arch`, `locale` | Static context. |
| `provider`, `model` | Current LLM provider/model name (during agent turns). |
| `repo_origin` | `{host: gitcode\|atomgit\|github\|gitlab\|other\|none, has_git}` — we do **not** send the URL. |

### Events

| event_id | type | 何时触发 | payload |
|---|---|---|---|
| `open_atomcode` | / | 启动 atomcode（非 meta 命令）| 无 |
| `llm_chat` | — | 每个 LLM turn 完成时 | `duration_ms, tool_calls_count, input_tokens, output_tokens, cached_tokens, had_error` |
| `use_command` | 具体指令字符串 | 每次执行 slash 命令 | — |
| `login_success` | / | OAuth 登录成功 | 无 |
| `take_codingplan` | `success` / `fail` | `atomcode login` / `/login`（含隐藏的 `atomcode codingplan` 别名）结束 | — |
| `panic` | / | 程序崩溃 | `location, message_head, thread, backtrace_top_5`（已 scrub） |
| `telemetry_disabled` | / | 用户执行 `atomcode telemetry disable` 时（仅当原本是启用） | 无 |

### NEVER collected

- ❌ Prompt text / LLM response text
- ❌ File paths, file contents, git remote URLs
- ❌ Tool call argument values
- ❌ Environment variable values
- ❌ Local paths in panic backtraces (scrubbed to `<HOME>` / `<CWD>`)

If you find any of the above leaking in a real event, please file an issue at
`https://atomgit.com/atomgit_atomcode/atomcode/issues`.

## How to disable

Any one of these works (higher precedence overrides lower):

1. `export ATOMCODE_TELEMETRY=0` (environment, single process)
2. `export DO_NOT_TRACK=1` (industry-standard signal)
3. `atomcode --no-telemetry <command>` (single invocation)
4. `atomcode telemetry disable` (persistent — writes to `~/.atomcode/config.toml`)

`atomcode telemetry status` shows which rule applies.

## Daemon behavior

The `atomcode daemon` process (backend for VS Code and other IDE integrations)
shares the same telemetry pipeline as the CLI.

### Startup status line

On launch, the daemon prints one line to stdout:

```
Telemetry: enabled
```

or, if disabled:

```
Telemetry: disabled (reason: env:ATOMCODE_TELEMETRY=0)
```

The reason string matches the output of `atomcode telemetry status`.

### `--no-telemetry` flag

```sh
atomcode daemon --port 13456 --no-telemetry
```

Disables telemetry for this daemon process only (equivalent to
`atomcode --no-telemetry` for CLI invocations).

### Graceful shutdown flush

When the daemon receives `SIGINT` / `SIGTERM` (or Ctrl+C on Windows), it:

1. Stops accepting new HTTP connections.
2. Waits for in-flight requests to complete.
3. Flushes any buffered telemetry events to disk (timeout: 500 ms).
4. Exits.

Events that cannot be sent within the 500 ms budget remain in the local queue
and are retried on the next process start.

### Shared state with CLI

The daemon and CLI share the same on-disk identity and queue:

| Path | Purpose |
|---|---|
| `~/.atomcode/device_id` | Stable device UUID (created on first run by whichever process starts first) |
| `~/.atomcode/telemetry/queue/` | NDJSON event queue — both processes write segments concurrently using a claim-based mechanism to avoid corruption |

No daemon-specific files are introduced. Both processes read the same
`~/.atomcode/config.toml` for the `[telemetry].enabled` setting.

### Filtering daemon events

`atomcode telemetry status` and `atomcode telemetry dump` work for daemon events
too — they read from the same shared queue. To show only daemon-originated
events:

```sh
atomcode telemetry dump --last 100 --pretty | jq 'select(.mode == "ide")'
```

## Inspect what will be sent

```sh
atomcode telemetry dump --last 50 --pretty
```

Prints the exact NDJSON records queued on disk waiting to be sent.
Nothing is hidden.
