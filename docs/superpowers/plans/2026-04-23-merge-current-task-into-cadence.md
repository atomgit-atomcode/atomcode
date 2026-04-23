# Merge CURRENT TASK Injection Into Cadence Reflection

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把每轮注入的 `=== CURRENT TASK ===` block 合并进 `reflection_prompt`，由 cadence 统一调度；删除现有的 per-turn `render_turn_reminder` 路径和配套的 `prev_turn_edited_files` 辅助状态。

**Architecture:** 
- `reflection_prompt` 扩签名为 `(delta: usize, current_task: &str)`。当 task 非空时在 delta 行后插入 `=== ORIGINAL TASK ===\n<verbatim, 300 字截断>\n` 段；且 Q1 从 "Restate the original goal" 改为 "Does your current plan still match the task above?"——任务已 verbatim 可见，Q1 的真实价值是 coherence check，不是记忆测试。
- `apply_post_turn_discipline` 在 cadence 触发时传入 `self.current_task`。
- `CtxBuilder::render_turn_reminder` + `ctx::render::render_turn_reminder` 自由函数 + `AgentLoop::prev_turn_edited_files` 字段全部删除。调用点 `agent/mod.rs:930-932` 改为直接传 `""` 给下游 `turn_reminder`。
- `build_messages` / `run_with_filter` 的 `turn_reminder: &str` 形参**保留**——其他 caller（`basic_run`、子 agent `try_sub_agent_dispatch`）本就传 `""`，删签名只添 churn。
- Post-compression state restoration（`agent/mod.rs:1760-1790`）**不动**。它已经覆盖"压缩事件丢任务"这个独立场景，和 cadence 是互补而不是竞态。

**Tech Stack:** Rust，现有 AgentLoop / DisciplineState / CtxBuilder / Conversation。无新依赖、无新 config。

---

## File Structure

- Modify: `crates/atomcode-core/src/agent/discipline.rs` — `reflection_prompt` 新签名 + 现有 reflection_tests 更新 + 新 test
- Modify: `crates/atomcode-core/src/agent/mod.rs` — discipline 调用点新签名；删 `prev_turn_edited_files` 字段 / init / 赋值 / `render_turn_reminder` 调用点
- Modify: `crates/atomcode-core/src/ctx/render.rs` — 删 `render_turn_reminder` free fn + 相关测试（保留 `build_messages` 的 `turn_reminder` 形参路径）
- Modify: `crates/atomcode-core/src/ctx/mod.rs` — 删 `CtxBuilder::render_turn_reminder` trait 默认方法及其 doc

---

### Task 1: `reflection_prompt` 接收 current_task，插入 verbatim task 段 + 改 Q1

**Files:**
- Modify: `crates/atomcode-core/src/agent/discipline.rs:135-145` (reflection_prompt body)
- Modify: `crates/atomcode-core/src/agent/discipline.rs:193-267` (reflection_tests module)

- [ ] **Step 1: 更新现有测试使用新签名（传空 task）**

当前 reflection_prompt 测试都用 `reflection_prompt(N)`。先全部改成 `reflection_prompt(N, "")`，并调整内容断言以匹配"空 task 分支保持原样"。

替换 `crates/atomcode-core/src/agent/discipline.rs` 的 `reflection_prompt_is_language_neutral_and_mentions_delta` 测试为：

```rust
    #[test]
    fn reflection_prompt_is_language_neutral_and_mentions_delta() {
        // Empty task → prompt stays in the original "restate the goal" mode
        // so that callers without a task (future API consumers, first-turn
        // edge cases before handle_send_message fires) still get a working
        // checkpoint.
        let msg = reflection_prompt(12, "");

        assert!(msg.contains("12"), "prompt must include delta count, got: {}", msg);

        assert!(
            !msg.to_lowercase().contains("error"),
            "prompt must not frame as error, got: {}", msg
        );
        assert!(
            !msg.to_lowercase().contains("blocked"),
            "prompt must not look like a BLOCKED guard, got: {}", msg
        );

        // Empty-task branch falls back to the recall question.
        assert!(
            msg.contains("original task") || msg.contains("original goal") || msg.contains("restate"),
            "empty-task prompt must ask to restate the task/goal, got: {}", msg
        );
        assert!(
            msg.contains("rule out") || msg.contains("ruled out")
                || msg.contains("prove") || msg.contains("proved") || msg.contains("proven")
                || msg.contains("learned"),
            "prompt must ask what was learned/ruled out, got: {}", msg
        );
        assert!(
            msg.contains("next") && (msg.contains("concrete") || msg.contains("output")),
            "prompt must ask for the next concrete output, got: {}", msg
        );

        assert!(!msg.to_lowercase().contains("cargo"));
        assert!(!msg.to_lowercase().contains("grep"));
        assert!(!msg.to_lowercase().contains("npm"));
    }
```

替换 `reflection_prompt_flags_itself_as_system_meta`：

```rust
    #[test]
    fn reflection_prompt_flags_itself_as_system_meta() {
        let msg = reflection_prompt(5, "");
        assert!(
            msg.contains("not a user message") || msg.contains("System meta"),
            "prompt must self-flag as system meta / non-user, got: {}", msg
        );
    }
```

替换 `reflection_prompt_avoids_verbose_command_phrasing`：

```rust
    #[test]
    fn reflection_prompt_avoids_verbose_command_phrasing() {
        let msg = reflection_prompt(5, "").to_lowercase();
        assert!(
            !msg.contains("answer in plain text"),
            "prompt must not repeat the verbose original phrasing, got: {}", msg
        );
        assert!(
            !msg.contains("answer these"),
            "prompt must not repeat the verbose original phrasing, got: {}", msg
        );
    }
```

- [ ] **Step 2: 追加新测试 — verbatim task 段、截断、Q1 改写**

在 `reflection_tests` 模块末尾（`}` 前）追加：

```rust
    #[test]
    fn reflection_prompt_embeds_verbatim_task_when_provided() {
        // Task is short, non-empty: must appear verbatim under a clearly
        // flagged "ORIGINAL TASK" header so the model doesn't have to
        // reconstruct intent from compressed history.
        let msg = reflection_prompt(7, "fix the auth token refresh loop");
        assert!(
            msg.contains("ORIGINAL TASK"),
            "task branch must carry an ORIGINAL TASK marker, got: {}", msg
        );
        assert!(
            msg.contains("fix the auth token refresh loop"),
            "verbatim task must appear, got: {}", msg
        );
    }

    #[test]
    fn reflection_prompt_task_branch_asks_coherence_check_not_recall() {
        // With the task verbatim in the prompt, Q1 becomes a coherence
        // check against the current trajectory — "restate the original
        // goal" would be busywork.
        let msg = reflection_prompt(3, "refactor the cache layer").to_lowercase();
        assert!(
            msg.contains("current plan") && msg.contains("match"),
            "task branch Q1 must ask whether the plan still matches the task, got: {}", msg
        );
        // And MUST NOT ask the model to restate what's already in front of it.
        assert!(
            !msg.contains("restate"),
            "task branch must not ask the model to restate a visible task, got: {}", msg
        );
    }

    #[test]
    fn reflection_prompt_truncates_task_at_300_chars() {
        // Long tasks get clipped so the checkpoint itself doesn't explode
        // into a huge injection — the first 297 chars + "..." is the
        // same budget the previous per-turn reminder used.
        let long = "x".repeat(500);
        let msg = reflection_prompt(4, &long);
        assert!(msg.contains("xxx..."), "truncation marker missing: {}", msg);
        assert!(
            !msg.contains(&"x".repeat(400)),
            "prompt must not carry 400+ contiguous x's (truncation failed): {}", msg
        );
    }

    #[test]
    fn reflection_prompt_empty_task_omits_original_task_block() {
        // When there is no active task, skip the block entirely rather
        // than emitting "ORIGINAL TASK: (empty)" noise.
        let msg = reflection_prompt(5, "");
        assert!(
            !msg.contains("ORIGINAL TASK"),
            "empty-task prompt must omit the task block, got: {}", msg
        );
    }
```

- [ ] **Step 3: 运行测试确认失败**

```bash
cargo test -p atomcode-core --lib agent::discipline::reflection_tests
```

Expected: 新测试 compile 失败（`reflection_prompt` 目前只接受 1 个参数），或 compile 通过后新的 `_verbatim_task_`/`_coherence_check_`/`_truncates_`/`_empty_task_omits_` 四个断言失败。现有 6 个测试因签名变更也会编译失败。

- [ ] **Step 4: 实现新签名 + 任务分支**

把 `crates/atomcode-core/src/agent/discipline.rs` 的 `reflection_prompt` 整体替换为：

```rust
pub(crate) fn reflection_prompt(delta: usize, current_task: &str) -> String {
    // Preamble shared by both branches.
    let mut out = String::new();
    out.push_str("[System meta · not a user message]\n");
    out.push_str(&format!(
        "{} tool calls elapsed since the last self-check.\n",
        delta
    ));

    if current_task.is_empty() {
        // No verbatim task available — fall back to the recall question.
        // Callers without a task (future API consumers, or edge cases
        // before handle_send_message fires) still get a useful checkpoint.
        out.push_str("Before the next tool call, answer:\n");
        out.push_str(&format!(
            "1. Restate the original goal in one sentence.\n\
             2. What did those {} steps prove or rule out?\n\
             3. What is the next concrete output, and how close is it?\n",
            delta
        ));
    } else {
        // Task is visible: bias Q1 from recall to coherence check.
        // Truncation mirrors the budget that the former per-turn reminder
        // used (297 chars + "...") so cadence-merged output stays small.
        let task_short = if current_task.chars().count() > 300 {
            format!("{}...", current_task.chars().take(297).collect::<String>())
        } else {
            current_task.to_string()
        };
        out.push_str(&format!(
            "\n=== ORIGINAL TASK ===\n{}\n\n",
            task_short
        ));
        out.push_str("Before the next tool call, answer:\n");
        out.push_str(&format!(
            "1. Does your current plan still match the task above? If not, correct course now.\n\
             2. What did those {} steps prove or rule out?\n\
             3. What is the next concrete output, and how close is it?\n",
            delta
        ));
    }

    out
}
```

- [ ] **Step 5: 运行测试确认通过**

```bash
cargo test -p atomcode-core --lib agent::discipline::reflection_tests
```

Expected: 10 个测试全部通过（6 个原有改写 + 4 个新增）。

- [ ] **Step 6: 更新调用点**

`crates/atomcode-core/src/agent/discipline.rs:31` 当前：

```rust
            let msg = reflection_prompt(delta);
```

改为：

```rust
            let msg = reflection_prompt(delta, &self.current_task);
```

- [ ] **Step 7: 确保 core crate 通过构建**

```bash
cargo build -p atomcode-core
```

Expected: clean build.

- [ ] **Step 8: Commit**

```bash
git add crates/atomcode-core/src/agent/discipline.rs
git commit -m "feat(discipline): inject verbatim task into cadence reflection"
```

---

### Task 2: 删除 per-turn `render_turn_reminder` 路径 + `prev_turn_edited_files` 字段

**Files:**
- Modify: `crates/atomcode-core/src/ctx/render.rs:20-67` (render_turn_reminder 自由函数 + 其 rustdoc)
- Modify: `crates/atomcode-core/src/ctx/render.rs:927-967` (render_turn_reminder 的 5 个 unit test)
- Modify: `crates/atomcode-core/src/ctx/mod.rs:72-90` (CtxBuilder::render_turn_reminder trait 方法 + rustdoc)
- Modify: `crates/atomcode-core/src/agent/mod.rs:310-313` (prev_turn_edited_files 字段声明)
- Modify: `crates/atomcode-core/src/agent/mod.rs:545` (init)
- Modify: `crates/atomcode-core/src/agent/mod.rs:805` (赋值)
- Modify: `crates/atomcode-core/src/agent/mod.rs:929-932` (turn_reminder 计算)

- [ ] **Step 1: 删除 `render_turn_reminder` 相关测试**

在 `crates/atomcode-core/src/ctx/render.rs` 中删除以下五个测试（保留 `apply_model_directives_*` 和其他不相关测试）：
- `render_turn_reminder_empty_when_no_state`
- `render_turn_reminder_includes_prev_files_only`
- `render_turn_reminder_includes_current_task_only`
- `render_turn_reminder_truncates_long_task_at_300_chars`
- `render_turn_reminder_task_appears_after_prev_files`

- [ ] **Step 2: 运行 core 测试确认编译失败（因 render_turn_reminder 还在）**

```bash
cargo test -p atomcode-core --lib ctx::render 2>&1 | tail -20
```

Expected: 通过（测试文件此时没有引用 render_turn_reminder 的断言了），剩余测试通过。若仍有编译错误说明漏删引用，修掉再继续。

- [ ] **Step 3: 删除自由函数 `render_turn_reminder`**

在 `crates/atomcode-core/src/ctx/render.rs` 删除 L20-67（`Render the per-turn dynamic reminder ...` rustdoc 块 + `pub fn render_turn_reminder(...) -> String { ... }` 整体）。

- [ ] **Step 4: 删除 trait 方法 `CtxBuilder::render_turn_reminder`**

在 `crates/atomcode-core/src/ctx/mod.rs` 删除 L72-90（从 `/// Render the per-turn dynamic reminder from agent state.` 到闭合 `}`）。

- [ ] **Step 5: 删除 AgentLoop 的 `prev_turn_edited_files` 字段 / init / 赋值**

`crates/atomcode-core/src/agent/mod.rs` 三处删除：

- L310-313 声明：
  ```rust
      /// Files edited in the previous turn — injected into system prompt so the model
      /// knows where to start when the user reports the same issue again.
      prev_turn_edited_files: Vec<String>,
  ```
  整段删掉。

- L545 init：
  ```rust
              prev_turn_edited_files: Vec::new(),
  ```
  删掉这一行。

- L805 赋值：
  ```rust
          // Save current turn's edits before clearing — used in next turn's system prompt
          self.prev_turn_edited_files = self.files_edited_this_turn.clone();
  ```
  整两行删掉（注释 + 赋值）。

- [ ] **Step 6: 修改 turn_reminder 计算点**

`crates/atomcode-core/src/agent/mod.rs:929-932` 当前：

```rust
            let system_prompt = self.build_system_prompt();
            let turn_reminder = self
                .ctx
                .render_turn_reminder(&self.prev_turn_edited_files, &self.current_task);
```

改为：

```rust
            let system_prompt = self.build_system_prompt();
            // Per-turn reminder removed: verbatim task now rides on the
            // cadence reflection checkpoint (see agent::discipline::reflection_prompt).
            // The turn_reminder parameter is kept on the `build_messages`
            // side because basic_run and the sub-agent path both already
            // pass "" — changing the signature buys no code, only churn.
            let turn_reminder = String::new();
```

- [ ] **Step 7: 全工作区构建**

```bash
cargo build
```

Expected: clean。若有任何 `no method named 'render_turn_reminder'` / `no field 'prev_turn_edited_files'` 报错，回到对应 step 补删；这是 schema 变更导致的 downstream 影响，不要引入 shim。

- [ ] **Step 8: 全工作区测试**

```bash
cargo test
```

Expected: 所有测试通过。重点关注：
- `agent::discipline::reflection_tests` 10 项（Task 1 的全部）
- `ctx::render::tests` 剩余项（仅 apply_model_directives / build_messages / microcompact 相关）
- `agent::mod` / TUI 里任何对 `prev_turn_edited_files` 的引用都应已被 Step 5 清干净

- [ ] **Step 9: Commit**

```bash
git add crates/atomcode-core/src/ctx/render.rs \
        crates/atomcode-core/src/ctx/mod.rs \
        crates/atomcode-core/src/agent/mod.rs
git commit -m "refactor(ctx): drop per-turn render_turn_reminder and prev_turn_edited_files"
```

---

### Task 3: 端到端验证

**Files:** 无代码修改；验证 + 跨模型对照。

- [ ] **Step 1: datalog smoke test — Claude**

在一个小 demo 工作区跑一个长任务（≥ `reflection_cadence=10` 次 tool call），模型用 `claude-opus-4-7` 或 `claude-sonnet-4-6`：

```bash
atomcode --provider claude
# 输入一个会触发多次 read/grep/edit 的任务，比如：
# "找到 src/foo.rs 里的所有 TODO 并改成 FIXME，然后写一个总结"
```

在相同工作区下查 datalog：

```bash
ls -t ~/.atomcode/datalog/*/llm/*.json | head -20
grep -l "ORIGINAL TASK" ~/.atomcode/datalog/*/llm/*.json | head -5
grep -l "System meta" ~/.atomcode/datalog/*/llm/*.json | head -5
```

Expected: 至少一个 llm request 文件包含 `=== ORIGINAL TASK ===` 块（在第 ≥10 次 tool call 之后）。第一轮 request 不该包含——说明 per-turn 注入已被移除。

- [ ] **Step 2: datalog smoke test — GLM**

同一命题换 `--provider glm`（或 memory 里登记的 GLM 入口）跑一次。

```bash
atomcode --provider glm
```

再 grep 同样 pattern。memory `feedback_cross_model_verify.md` 明确要求 Claude + GLM 双跑。

Expected: 行为一致——第一轮无 ORIGINAL TASK 块，第 ≥10 次 tool call 后出现。两模型都是。

- [ ] **Step 3: 记录结论到 memory**

如果两跑都按预期工作，更新 `feedback_cross_model_verify.md`（或追加 memo）记录本次是双模型验证后 ship 的，形成 positive 判例。如果有差异（例如某个模型对新 Q1 wording 响应率明显不同），把差异写进 project 级 memory，**不要** ship。

```bash
# 如果两模型都过 — 更新 memory
# 如果任一模型异常 — 停，不要 merge，回到设计
```

- [ ] **Step 4: （可选但推荐）SWE-bench A/B**

memory `project_swebench_phase1.md` 表明 eval/swebench 已落地 main，predict + grade 双阶段 + dual-score。本改动是对 agent 行为的直接修改，推荐：

```bash
# 切回 main 基线跑一次（旧 render_turn_reminder 仍在）
git checkout main
cd eval/swebench
# 按 README 跑 predict + grade，保存 dual-score → baseline_score.json

# 切回本分支跑一次
git checkout <this-branch>
cd eval/swebench
# 再跑一次，对比 dual-score → after_score.json

diff baseline_score.json after_score.json
```

判据：after_score ≥ baseline_score（允许小幅浮动，因 LLM 随机性）。若后者显著下降，回到设计层反思 Q1 改写或截断长度。

- [ ] **Step 5: final commit（如果有 memory 更新）**

```bash
git add /Users/lichao/.claude/projects/-Users-lichao-project-gitcode-ai-atomcode/memory/
git commit -m "docs(memory): record cross-model verification of cadence-merged task reminder"
```

---

## Self-Review Checklist

- **Spec coverage:** 每条设计决策都对应 task —— reflection_prompt 合并（Task 1 Steps 1-5）、discipline 调用点（Task 1 Step 6）、per-turn 路径删除（Task 2 Steps 3-6）、prev_turn_edited_files 字段清理（Task 2 Step 5）、post-compression 机制不动（显式不在 scope 中）、跨模型验证（Task 3 Steps 1-3）。
- **Placeholder scan:** 无 TODO/TBD；每个 code step 给出完整替换内容；每个测试给出完整断言；每个命令附 expected output 说明。
- **Type consistency:** `reflection_prompt` 签名在 Task 1 Step 4 确定为 `(delta: usize, current_task: &str) -> String`，Step 6 调用点与 Step 2/4 新测试断言一致；`prev_turn_edited_files` 字段名在 Task 2 Step 5 三个删除点完全一致。

---

## 范围外（已刻意不做）

- **"Do NOT search for files you already know about." 这句行为劝诫的归宿**。本 plan 只删 per-turn 注入，这句话随之消失。是否要归入 system prompt 或独立机制留给后续 PR——它和 current_task 回显无关，本 plan 不混进来。
- **`reflection_cadence` 默认值 / config 调整**。cadence 机制本身不动，只是换注入内容。
- **Post-compression state restoration 重写**。`agent/mod.rs:1760-1790` 已独立工作，合并 cadence 的正当性不依赖它。
