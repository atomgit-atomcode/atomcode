# Cadence Reflection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 atomcode 的 agent loop 加入周期性反思 checkpoint —— 每 N 次 tool call 后，在下一个 turn 开始前注入一段语言中立的 "restate goal / what ruled out / next concrete output" 提示，防止长尾任务方向漂移。

**Architecture:** 复用现有 `apply_post_turn_discipline` 钩子。新增两个纯函数：`should_inject_reflection(current, last, cadence)` 决定是否注入，`reflection_prompt(delta)` 渲染提示文本。触发条件 = `tool_call_count - last_reflection_at_tool_count >= cadence`。注入通过 `conversation.add_user_message` 完成，并更新标记。cadence 可配置（`Config.reflection_cadence: usize`，默认 10，0 禁用）。AgentLoop 层的集成只是 glue，靠类型系统保证，测试集中在两个纯函数。

**Tech Stack:** Rust, 现有 `AgentLoop` / `DisciplineState` / `Conversation` / `Config` / CLI — 无新依赖。

---

## File Structure

- Modify: `crates/atomcode-core/src/config/mod.rs` — `Config` 加 `reflection_cadence: usize` 字段（serde 默认 10）。
- Modify: `crates/atomcode-core/src/agent/mod.rs` — `DisciplineState` 加 `last_reflection_at_tool_count: usize` 字段；新 task 开始时与 `tool_call_count` 一同重置。
- Modify: `crates/atomcode-core/src/agent/discipline.rs` — 加 `should_inject_reflection` 和 `reflection_prompt` 两个自由函数 + 单测；在 `apply_post_turn_discipline` 顶部 wire。
- Modify: `crates/atomcode-cli/src/main.rs` — 加 `--reflection-cadence <N>` flag，覆盖 config。

---

### Task 1: Config 字段 + serde 默认

**Files:**
- Modify: `crates/atomcode-core/src/config/mod.rs:47-69` (Config struct)
- Modify: `crates/atomcode-core/src/config/mod.rs:86` (附近加 default helper)

- [ ] **Step 1: Write the failing tests**

在 `crates/atomcode-core/src/config/mod.rs` 末尾追加：

```rust
#[cfg(test)]
mod reflection_config_tests {
    use super::*;

    #[test]
    fn reflection_cadence_defaults_to_ten_when_missing_from_toml() {
        let toml_text = r#"
default_provider = "claude"
[providers]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parses minimal config");
        assert_eq!(cfg.reflection_cadence, 10);
    }

    #[test]
    fn reflection_cadence_zero_means_disabled() {
        let toml_text = r#"
default_provider = "claude"
reflection_cadence = 0
[providers]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parses config with 0");
        assert_eq!(cfg.reflection_cadence, 0);
    }

    #[test]
    fn reflection_cadence_custom_value_is_preserved() {
        let toml_text = r#"
default_provider = "claude"
reflection_cadence = 7
[providers]
"#;
        let cfg: Config = toml::from_str(toml_text).expect("parses");
        assert_eq!(cfg.reflection_cadence, 7);
    }
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test -p atomcode-core --lib config::reflection_config_tests
```

Expected: compile error `no field 'reflection_cadence' on type 'Config'`.

- [ ] **Step 3: Add the field**

In the `Config` struct (near the `auto_update: bool` field around L67-68), add:

```rust
    /// Every N tool calls, inject a "restate goal / what ruled out / next
    /// output" reflection prompt before the next turn. 0 disables the
    /// checkpoint entirely. Default 10 matches typical multi-file analysis
    /// step counts — below which the reflection is overhead, above which
    /// the agent can drift unnoticed.
    #[serde(default = "default_reflection_cadence")]
    pub reflection_cadence: usize,
```

Below the existing `fn default_true() -> bool { true }` (around L86), add:

```rust
fn default_reflection_cadence() -> usize { 10 }
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test -p atomcode-core --lib config::reflection_config_tests
```

Expected: 3 tests pass.

- [ ] **Step 5: Check for broken Config constructors**

```bash
cargo build 2>&1 | grep -E "missing field|E0063"
```

Expected: empty output. If any literal `Config { ... }` constructor breaks (typically in tests or CLI), add `reflection_cadence: 10` to it.

- [ ] **Step 6: Full workspace build**

```bash
cargo build
```

Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-core/src/config/mod.rs
git commit -m "feat(config): add reflection_cadence (default 10, 0 disables)"
```

---

### Task 2: DisciplineState 字段 + reset

**Files:**
- Modify: `crates/atomcode-core/src/agent/mod.rs:197-221` (DisciplineState struct)
- Modify: `crates/atomcode-core/src/agent/mod.rs:787` (reset block after `self.tool_call_count = 0;`)

- [ ] **Step 1: Add the field**

In the `DisciplineState` struct, add this field near `file_read_counts` (around L210):

```rust
    /// Snapshot of `AgentLoop.tool_call_count` at the last cadence
    /// reflection injection. The delta
    /// `tool_call_count - last_reflection_at_tool_count` feeds
    /// `should_inject_reflection`. Resets together with `tool_call_count`
    /// when a new user task starts.
    pub last_reflection_at_tool_count: usize,
```

`DisciplineState` already derives `Default`, so `usize` zeros automatically — no changes to the `DisciplineState::default()` path.

- [ ] **Step 2: Add reset alongside tool_call_count**

Find `self.tool_call_count = 0;` at L787. Immediately after it add:

```rust
self.discipline_state.last_reflection_at_tool_count = 0;
```

Rationale: if `tool_call_count` rewinds but the marker doesn't, the next
`tool_call_count - last_reflection_at_tool_count` subtraction produces a
nonsense delta (the `usize::saturating_sub` wouldn't underflow, but the
semantic is still wrong — the very first tool call of the new task would
look like "0 calls since checkpoint" instead of "1 of N").

- [ ] **Step 3: Verify build**

```bash
cargo build -p atomcode-core
```

Expected: clean build (no test added yet — field is pure data; its use is tested in Tasks 3/5).

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-core/src/agent/mod.rs
git commit -m "feat(agent): track last_reflection_at_tool_count in DisciplineState"
```

---

### Task 3: Pure fn `should_inject_reflection` + tests

**Files:**
- Modify: `crates/atomcode-core/src/agent/discipline.rs` — add free fn at bottom, add `#[cfg(test)] mod reflection_tests` block

- [ ] **Step 1: Write the failing tests**

Append to `crates/atomcode-core/src/agent/discipline.rs`:

```rust
#[cfg(test)]
mod reflection_tests {
    use super::should_inject_reflection;

    #[test]
    fn no_injection_when_cadence_is_zero() {
        // cadence = 0 is the "disabled" sentinel — must never fire.
        assert_eq!(should_inject_reflection(50, 0, 0), None);
        assert_eq!(should_inject_reflection(1, 0, 0), None);
    }

    #[test]
    fn no_injection_when_delta_below_cadence() {
        // 9 tool calls since last checkpoint, cadence = 10 → not yet.
        assert_eq!(should_inject_reflection(9, 0, 10), None);
    }

    #[test]
    fn injection_when_delta_meets_cadence() {
        // Exactly at the threshold → fire.
        assert_eq!(should_inject_reflection(10, 0, 10), Some(10));
    }

    #[test]
    fn injection_when_delta_exceeds_cadence_after_batched_turn() {
        // A single turn can burn multiple tool calls, so the delta may
        // jump past the cadence in one go (13 - 0 = 13 ≥ 10).
        assert_eq!(should_inject_reflection(13, 0, 10), Some(13));
    }

    #[test]
    fn honors_prior_reflection_marker() {
        // After a checkpoint at count=10, the next one fires at count=20
        // (delta = 10 since last marker), not at count=10 trivially.
        assert_eq!(should_inject_reflection(19, 10, 10), None);
        assert_eq!(should_inject_reflection(20, 10, 10), Some(10));
    }

    #[test]
    fn marker_ahead_of_count_is_safe() {
        // Defensive: if the marker somehow exceeds current (should never
        // happen, but usize subtraction would underflow), saturate to 0
        // and do not fire.
        assert_eq!(should_inject_reflection(5, 10, 10), None);
    }
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test -p atomcode-core --lib agent::discipline::reflection_tests
```

Expected: compile error `cannot find function 'should_inject_reflection'`.

- [ ] **Step 3: Implement the pure function**

At the bottom of `crates/atomcode-core/src/agent/discipline.rs`, **outside** any `impl` block, add:

```rust
/// Decide whether to inject a cadence-reflection prompt.
///
/// Returns `Some(delta)` when the number of tool calls since the last
/// reflection meets or exceeds `cadence`. Returns `None` otherwise,
/// including the `cadence == 0` "disabled" case.
///
/// The returned `delta` tells the caller how many tool calls have elapsed
/// since the last checkpoint, for use in the rendered prompt.
pub(crate) fn should_inject_reflection(
    current_tool_count: usize,
    last_reflection_at: usize,
    cadence: usize,
) -> Option<usize> {
    if cadence == 0 {
        return None;
    }
    let delta = current_tool_count.saturating_sub(last_reflection_at);
    if delta >= cadence {
        Some(delta)
    } else {
        None
    }
}
```

- [ ] **Step 4: Run to verify they pass**

```bash
cargo test -p atomcode-core --lib agent::discipline::reflection_tests
```

Expected: 6 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/agent/discipline.rs
git commit -m "feat(discipline): add should_inject_reflection pure fn + tests"
```

---

### Task 4: Pure fn `reflection_prompt` + tests

**Files:**
- Modify: `crates/atomcode-core/src/agent/discipline.rs` — add another free fn + test

- [ ] **Step 1: Write the failing test**

Append inside the same `reflection_tests` module (above the last `}`):

```rust
    use super::reflection_prompt;

    #[test]
    fn reflection_prompt_is_language_neutral_and_mentions_delta() {
        let msg = reflection_prompt(12);

        // The delta must appear so the model sees the scale of the gap.
        assert!(msg.contains("12"), "prompt must include delta count, got: {}", msg);

        // Must NOT pretend this is an error — it's a scheduled recalibration.
        assert!(
            !msg.to_lowercase().contains("error"),
            "prompt must not frame as error, got: {}", msg
        );
        assert!(
            !msg.to_lowercase().contains("blocked"),
            "prompt must not look like a BLOCKED guard, got: {}", msg
        );

        // Must ask the three canonical language-neutral questions.
        assert!(
            msg.contains("original task") || msg.contains("restate"),
            "prompt must ask to restate the task, got: {}", msg
        );
        assert!(
            msg.contains("ruled out") || msg.contains("learned") || msg.contains("proven"),
            "prompt must ask what was learned/ruled out, got: {}", msg
        );
        assert!(
            msg.contains("next") && (msg.contains("concrete") || msg.contains("output")),
            "prompt must ask for the next concrete output, got: {}", msg
        );

        // Must NOT embed language-/tool-specific hints (this is the whole
        // point of the reflection being generic).
        assert!(!msg.to_lowercase().contains("cargo"));
        assert!(!msg.to_lowercase().contains("grep"));
        assert!(!msg.to_lowercase().contains("npm"));
    }
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test -p atomcode-core --lib agent::discipline::reflection_tests::reflection_prompt_is_language_neutral_and_mentions_delta
```

Expected: compile error `cannot find function 'reflection_prompt'`.

- [ ] **Step 3: Implement**

At the bottom of `crates/atomcode-core/src/agent/discipline.rs` (next to `should_inject_reflection`), add:

```rust
/// Render the cadence-reflection prompt injected every `cadence` tool
/// calls. Language- and ecosystem-neutral by design — the three
/// questions apply to any task, any tool. Phrased as a scheduled
/// checkpoint (not a corrective intervention) because this fires
/// every N steps whether or not the agent appears stuck.
pub(crate) fn reflection_prompt(delta: usize) -> String {
    format!(
        "[Checkpoint — not an interruption, just a scheduled recalibration.]\n\
         You have made {} tool calls since the last checkpoint. \
         Before your next tool call, answer in plain text:\n\
         1. Restate the original task in one sentence.\n\
         2. What have the last {} steps proven or ruled out?\n\
         3. What is the next concrete output (an edit, an answer, a summary), \
         and roughly how many steps away?\n",
        delta, delta
    )
}
```

- [ ] **Step 4: Run to verify**

```bash
cargo test -p atomcode-core --lib agent::discipline::reflection_tests
```

Expected: 7 tests pass (6 from Task 3 + 1 new).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/agent/discipline.rs
git commit -m "feat(discipline): add reflection_prompt pure fn + tests"
```

---

### Task 5: Wire into apply_post_turn_discipline

**Files:**
- Modify: `crates/atomcode-core/src/agent/discipline.rs:9-50` (apply_post_turn_discipline body)

- [ ] **Step 1: Insert cadence check at the top of the body**

Edit `apply_post_turn_discipline`. The function currently starts (after its doc comment):

```rust
    pub(crate) fn apply_post_turn_discipline(&mut self) {
        // Re-read guard: when the same *region* of a file is read 2+ times,
        ...
```

Insert a new block **before** the re-read guard:

```rust
    pub(crate) fn apply_post_turn_discipline(&mut self) {
        // Cadence reflection: every N tool calls, inject a scheduled
        // "restate goal / what ruled out / next output" prompt regardless
        // of whether the agent appears stuck. Language-neutral, domain-
        // neutral. Fires one turn after the Nth tool call, so the agent
        // answers the three questions at the start of the following turn.
        if let Some(delta) = should_inject_reflection(
            self.tool_call_count,
            self.discipline_state.last_reflection_at_tool_count,
            self.config.reflection_cadence,
        ) {
            let msg = reflection_prompt(delta);
            self.conversation.add_user_message(&msg);
            self.discipline_state.last_reflection_at_tool_count = self.tool_call_count;
        }

        // Re-read guard: when the same *region* of a file is read 2+ times,
        // ... (existing code continues unchanged)
```

- [ ] **Step 2: Verify build**

```bash
cargo build -p atomcode-core
```

Expected: clean build. If `should_inject_reflection` or `reflection_prompt` are not in scope from inside the `impl AgentLoop { ... }` block, prefix with `self::` or move them inside the `impl` (but free fn with `pub(crate)` should be directly visible within the same module).

- [ ] **Step 3: Full crate tests to check no regression**

```bash
cargo test -p atomcode-core --lib 2>&1 | tail -6
```

Expected: previous pass count + 7 new tests from Tasks 3/4. Preexisting `self_update::tests::is_newer_semver` may still fail — unrelated.

- [ ] **Step 4: Manual smoke verification**

(No automated end-to-end test — constructing `AgentLoop` in a unit test costs far more than this glue is worth. The two pure fns are fully covered; this step is a one-time sanity check.)

Edit your local `~/.config/atomcode/config.toml` to set `reflection_cadence = 2`. Run atomcode against any repo and issue a task requiring ≥ 3 tool calls. Open the turn datalog and confirm the `[Checkpoint — ...]` user message appears after the 2nd tool call.

Revert the config override.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/agent/discipline.rs
git commit -m "feat(discipline): wire cadence reflection into apply_post_turn_discipline"
```

---

### Task 6: CLI flag override

**Files:**
- Modify: `crates/atomcode-cli/src/main.rs` around L371 (Cli struct) and L663 (config wiring)

- [ ] **Step 1: Add CLI field**

Find the `max_turns: Option<usize>` declaration around L371 in `crates/atomcode-cli/src/main.rs`. Immediately below it add:

```rust
    /// Inject a scheduled reflection prompt every N tool calls.
    /// 0 disables. Overrides the value in config.toml for this run.
    #[arg(long, value_name = "N")]
    reflection_cadence: Option<usize>,
```

- [ ] **Step 2: Wire to Config**

Find where `agent_loop.set_max_turns(cli.max_turns);` is called around L663. The `config` variable is still in scope there (or accessible via `agent_loop.config`). Add immediately before `AgentLoop::new(...)` (or wherever `config` is finalized — follow the local pattern):

```rust
if let Some(n) = cli.reflection_cadence {
    config.reflection_cadence = n;
}
```

If the wiring point uses `agent_loop.config` after construction, instead do:

```rust
if let Some(n) = cli.reflection_cadence {
    agent_loop.config.reflection_cadence = n;
}
```

(Pick whichever matches the `max_turns` pattern in the same function.)

- [ ] **Step 3: Verify build**

```bash
cargo build
```

Expected: clean build.

- [ ] **Step 4: Manual smoke test**

```bash
cargo run -- --reflection-cadence 3 --help 2>&1 | grep reflection-cadence
```

Expected: the flag description appears in help output.

```bash
cargo run -- --reflection-cadence 0
```

Start atomcode, confirm the checkpoint message is absent after many tool calls (0 disables). Exit.

```bash
cargo run -- --reflection-cadence 3
```

Start atomcode, run a 4-step task, confirm the checkpoint appears after step 3.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-cli/src/main.rs
git commit -m "feat(cli): --reflection-cadence flag overrides config.toml"
```

---

## Self-Review

**1. Spec coverage:**
- "Every N tool calls, inject reflection" → Task 5 ✓
- "Language-agnostic prompt (no cargo/grep/npm mentions)" → Task 4 test asserts absence ✓
- "Configurable default" → Task 1 (toml + serde default) ✓
- "CLI override" → Task 6 ✓
- "0 disables" → Task 3 test `no_injection_when_cadence_is_zero` ✓
- "Reset on new task chain" → Task 2 Step 2 resets alongside `tool_call_count` ✓
- "Pure fn for testability" → Tasks 3, 4 both are free fns ✓

**2. Placeholder scan:**
- No "TBD", no "implement later", no "similar to task N".
- Every code step has full code.
- Every command has expected output or expected failure.
- Task 5 Step 4 and Task 6 Step 4 are manual smoke tests — these are acknowledged explicitly as one-time verifications, not automated coverage, because the pure-fn tests already cover the logic and AgentLoop construction cost exceeds the benefit.

**3. Type consistency:**
- `reflection_cadence: usize` — consistent across `Config` (Task 1), CLI (Task 6), fn signature (Task 3), and the call site in `apply_post_turn_discipline` (Task 5).
- `last_reflection_at_tool_count: usize` — declared in Task 2, used in Task 5.
- `should_inject_reflection(usize, usize, usize) -> Option<usize>` — declared in Task 3, called in Task 5 with matching argument order (current, last, cadence).
- `reflection_prompt(usize) -> String` — declared in Task 4, called in Task 5.
- `Option<usize>` on the CLI (Task 6) unwraps to `usize` on the `Config` field — matches the `max_turns` pattern already in the codebase.

---

## Out of Scope (deferred to future plans)

- **Hard-enforcing reflection**: blocking the next `tool_call` until the agent produces a minimum amount of text. If soft injection proves insufficient during dogfooding, a follow-up plan can add this as a `discipline` layer on top.
- **Dynamic cadence**: tightening N after a BLOCKED event or loosening it after a successful edit. Static cadence first; adapt only if dogfooding shows the constant misses.
- **Per-task-type cadence**: different N for "diagnosis" vs "implementation" phases. Single N first; revisit if dogfooding shows one number doesn't fit.
- **Subagent integration**: each subagent has its own loop, so the same mechanism transplants, but not in this plan's scope.
- **Reflection quality scoring**: measuring whether the agent's reflection text actually addresses the three questions. Future work if soft injection proves too easy to ignore.
