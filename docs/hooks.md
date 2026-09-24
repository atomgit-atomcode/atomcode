# AtomCode Hooks

Hooks let you run your own shell command at key points in a session — to audit,
gate, rewrite, or inject context — without changing the core.

> **What changed (read this if you used hooks before):** the older TOML system
> (`hooks.toml` with `[[hooks]]` / `[[webhooks]]` / `[[async_webhooks]]`, and the
> built-in Rust hooks) **no longer fires at runtime**. The one live hook system is
> the JSON one described here (`hooks.json` / `.hooks.json`). If you have a
> `hooks.toml`, migrate it to `.hooks.json` — a TOML hook is silently inert.

## Quick start

**1. Write the config** — project-level `<project>/.hooks.json` (or global
`$ATOMCODE_HOME/hooks.json`, where `$ATOMCODE_HOME` defaults to `~/.atomcode`):

```json
{
  "hooks": {
    "audit-bash": {
      "event": "PreToolUse",
      "matcher": "bash",
      "command": "/usr/local/bin/audit-bash.sh",
      "timeout_ms": 10000
    }
  }
}
```

**2. Write the command** — it receives a JSON payload on **stdin** and (for
gating events) prints a decision as JSON on stdout:

```bash
#!/bin/bash
payload=$(cat)                       # the event payload arrives on stdin
tool=$(printf '%s' "$payload" | jq -r '.tool_name // empty')
echo "saw $tool" >&2                 # stderr is for your own logging
echo '{"action":"allow"}'            # stdout is the decision (gating events)
```

That's it — the command runs on the matching event.

> **No `hooks.json` is a valid state** (no hooks). A **malformed** `hooks.json`
> is skipped and a warning is logged (`target=atomcode::hooks`, with the path and
> the parse error) — one stray comma disables that file's hooks, so check the log
> if a hook stops firing. JSON does **not** allow comments.

## Config file

| Scope | Path |
|-------|------|
| Global (user) | `$ATOMCODE_HOME/hooks.json` (default `~/.atomcode/hooks.json`) |
| Project | `<project>/.hooks.json` |

Both load; project hooks are added to the global ones. Each entry:

| Field | Required | Meaning |
|-------|:--:|---------|
| `event` | ✅ | One of the 8 events below (PascalCase or snake_case). |
| `command` | ✅ | The shell command line to run. **Not** env-var-expanded — use an **absolute path** (`~` and `$VARS` are NOT resolved). |
| `matcher` | — | Tool-name glob for tool events; `\|`-separated alternatives (e.g. `bash\|edit_file`, `write*`). Omit to match every tool. |
| `timeout_ms` | — | Per-run timeout (default `10000`). A timeout or crash is **fail-open** (treated as "proceed"). |
| `disabled` | — | `true` to keep the entry but not run it. |

The map key (`"audit-bash"` above) is for your own organization; it is **not**
retained after loading, so you cannot address a hook by it (see the CLI section).

## Events

All eight, with the snake_case alias each also accepts:

| Event | Alias | Fires | Can affect the flow? |
|-------|-------|-------|:--:|
| `PreToolUse` | `pre_tool_use` | before a tool runs | ✅ allow / deny / ask / modify |
| `PostToolUse` | `post_tool_use` | after a tool succeeds | ✅ block / rewrite output |
| `PostToolUseFailure` | `post_tool_use_failure` | after a tool **fails** | ✅ block / rewrite output |
| `UserPromptSubmit` | `user_prompt_submit` | on a submitted prompt | ✅ block / inject context |
| `SessionStart` | `session_start` | when a session begins | inject context |
| `SessionEnd` | `session_end` | when a session ends | fire-and-forget |
| `Stop` | `stop` | when a turn stops cleanly | fire-and-forget |
| `StopFailure` | `stop_failure` | when a turn stops on error | fire-and-forget |

Tool matchers (`matcher`) apply only to the three tool events.

## The stdin payload

The command reads a JSON object on stdin. Fields depend on the event:

- **`PreToolUse`** — `session_id`, `hook_event_name`, `call_id`, `tool_name`,
  `tool_input` (the parsed argument object), `cwd`
- **`PostToolUse` / `PostToolUseFailure`** — `session_id`, `hook_event_name`,
  `call_id`, `tool_name`, `tool_response`, `cwd`
- **`UserPromptSubmit`** — `session_id`, `hook_event_name`, `cwd`, `prompt`
- **`SessionStart` / `SessionEnd`** — `session_id`, `hook_event_name`, `cwd`
- **`Stop` / `StopFailure`** — `session_id`, `hook_event_name`, `cwd`,
  `transcript_path`, `stop_hook_active`, `stop_reason`

> `call_id` correlates a `PreToolUse` with the matching `PostToolUse` /
> `PostToolUseFailure` of the same call — the only reliable way to pair
> arguments/result and to tell two concurrent calls of the same tool apart.
> (A call denied at `PreToolUse` still fires `PostToolUseFailure`, but without a
> `tool_name`; join it back to the Pre frame by `call_id`.)

## The stdout decision

Two things settle a hook's outcome: its **stdout** (the last JSON line) and its
**exit code**.

**Exit code (Claude-Code contract):** `0` = ok; **`2` = a deliberate block**
(give a reason on stdout/stderr); any other non-zero is a *non-blocking* error —
the tool proceeds (a broken hook must not wedge the turn).

**stdout JSON** — both an atomcode-native and a CC-compatible shape are accepted:

`PreToolUse` (gate + rewrite):

```json
{"action":"allow"}
{"action":"block","reason":"writing outside the workspace"}
{"action":"modify","args":{"path":"/safe/path"}}
```

or the CC shape:

```json
{"hookSpecificOutput":{"permissionDecision":"allow|deny|ask",
  "permissionDecisionReason":"...","updatedInput":{...},
  "additionalContext":"..."}}
```

`PostToolUse` / `PostToolUseFailure` (block or rewrite the result the model sees):

```json
{"decision":"block","reason":"..."}
{"hookSpecificOutput":{"updatedToolOutput":"redacted result"}}
```

`UserPromptSubmit` (block the prompt, or inject context):

```json
{"decision":"block","reason":"..."}
{"hookSpecificOutput":{"additionalContext":"extra context for the model"}}
```

When several hooks match one event, the **most-restrictive** decision wins
(`deny` > `ask` > `allow` > proceed); rewrites apply in registration order.

## CLI

```bash
atomcode hooks list           # loaded hooks, grouped by event
atomcode hooks paths          # the exact files that are read (with ✓/✗)
atomcode hooks test <NAME>    # dry-run a hook with a synthetic payload
```

`hooks test <NAME>` matches by **event name** (e.g. `PreToolUse`) or a
**substring of the command** — not the config key (keys are not retained). Run
`hooks test` with no match to see every loaded hook by event + command.

## Not firing? Six checks

| # | Check | Common cause |
|---|-------|--------------|
| 1 | The file parses | A stray comma disables the whole file — check the `atomcode::hooks` warning in the log; JSON allows no comments. |
| 2 | `command` is an absolute path | `~` / `$VARS` are not expanded. |
| 3 | `event` spelled right | See the table (either case). |
| 4 | `matcher` matches the tool | Tool events only; omit it to match all. |
| 5 | `disabled` not set | Defaults to enabled. |
| 6 | Not timing out | Default 10s; a timeout is fail-open (silently proceeds). |

## Security notes

1. Hooks **cannot bypass the permission system** — a `PreToolUse` deny does not
   override a user's stored allow, and an allow is convenience, never consent for
   a security boundary.
2. Commands run with **your** permissions — mind the command's own safety.
3. **Fail-open:** a timeout or crash is treated as "proceed", not "block".
4. On Windows, use an absolute path with an explicit interpreter; `~` is not
   expanded.
