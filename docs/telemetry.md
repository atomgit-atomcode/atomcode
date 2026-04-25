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
| `session_id` | Per process launch. |
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
| `take_codingplan` | `success` / `fail` | `atomcode codingplan` / `/codingplan` 结束 | — |
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

## Inspect what will be sent

```sh
atomcode telemetry dump --last 50 --pretty
```

Prints the exact NDJSON records queued on disk waiting to be sent.
Nothing is hidden.
