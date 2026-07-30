# WORKFLOW 意图理解引导 —— 设计文档

- 日期：2026-07-30
- 分支：release/v5.0.3
- 范围：`crates/atomcode-coding/src/persona.rs` 的 `RULES` 常量（`## WORKFLOW` 与 `## OUTPUT` 两节）
- 类型：纯提示词（persona）改动，无新工具、无新模式、无新 env 门控

## 背景与缺口

调研 opencode / codex / oh-my-pi 三家在「用户输入问题后，如何让 LLM 明确用户想实现什么」上的做法，共识是：

1. **分两条策略、按模式切换**：默认模式假设优先、边做边对齐；显式 Plan/Goal 模式才澄清优先、先钉死意图再动手。
2. **澄清能力三层**：提示词软引导 + 澄清工具（带互斥选项）+ Plan 模式强制流程。
3. **「理解的证明」= 计划本身**（codex：`update_plan` "demonstrates that you've understood the task"；opencode / pi 的 Plan 里都要求先复述用户字面 ask），而不是一句口头确认。
4. **澄清有成本、默认克制**：三家的澄清工具都内置「先自己探索/用约定解决，只在高影响歧义时才问」的门控。

atomcode 现状：

- `crates/atomcode-coding/src/persona.rs` 的 `## WORKFLOW`（L524-534）非小任务线是 `SEARCH → PLAN (one sentence) → EDIT → VERIFY → SUMMARIZE`。这里的 PLAN 是**实现方案**的一句话，**没有**对「用户真正要的结果 + 范围」的对齐/复述——这正是缺口。
- `request_user_input` 工具已在所有 coding 路径注入（`## ASKING THE USER`，默认 ON），非 brainstorming 专属。
- `todowrite` 有 `## TASK TRACKING` 引导（默认 ON，`ATOMCODE_TODO` 门控），3+ 步 / 多文件 / 多请求触发。
- Plan 模式 TUI 已有四档 pill（Build / AcceptEdits / Auto / Plan），但语义是「只读 + 审批」，不是 codex 式「探索→意图对齐→复述目标」的结构化阶段。

**已识别的内部冲突**：`## OUTPUT`（L585）现写 `Do NOT restate what the user said — just do it.`。若新引导让模型「复述用户意图」，会与此正面打架，弱模型尤其会困惑。设计必须调和：理解应**表达为 todo 计划 / 一句目标**，而非把用户原话再抄一遍。

## 决策（brainstorming 收敛结果）

| 决策点 | 结论 |
|---|---|
| 落地范围 | 两阶段：阶段一（本 spec）= persona 软引导；阶段二 = Plan 模式三阶段化（**defer，不在本 spec**） |
| 激进度 | **分层**：小任务/纯问答/单步编辑 → 直接做、不复述；多步/多文件/模糊任务 → 动手前复述目标+范围 |
| 承载形式 | 非小任务的复述**落到 `todowrite` 首项**（计划本身即理解的证明）；todo 未启用时退回「一句话目标」 |
| 放置方式 | **扩写现有 `## WORKFLOW`**，总是注入、无新 env 门控、不新增小节 |
| 弱模型额外加固 | **不做**（作为备选保留；先靠本方案观察 GLM/DeepSeek 是否自发承载） |

## 具体改动

### 改动 1 —— `## WORKFLOW`（persona.rs 约 L526 与 Guidelines 列表）

非小任务线加一个 `UNDERSTAND` 前置步：

```diff
- For non-trivial features or multi-file changes: SEARCH → PLAN (one sentence) → EDIT → VERIFY → SUMMARIZE.
+ For non-trivial features or multi-file changes: UNDERSTAND → SEARCH → PLAN (approach, one sentence) → EDIT → VERIFY → SUMMARIZE.
```

`Guidelines:` 列表**最前面**新增一条 bullet：

```
- UNDERSTAND: before diving in, pin down what the user actually wants — the concrete
  outcome and its scope, not implementation detail. For multi-step work this IS the
  task plan: its first items are the outcomes the user asked for; when a task plan
  isn't in play, state the goal in one sentence as part of PLAN. Capture the goal AS the
  plan — don't echo the request back as prose. Only if the goal itself is genuinely
  ambiguous (not an implementation choice you can reasonably pick) ask the user before
  starting; otherwise take the sensible default and proceed.
```

> **实现期修正(已评审接受)**：措辞刻意**不点名** `todowrite` / `request_user_input` 两个工具。
> 原因:`RULES` 是**无条件注入**的,而这两个工具受 env 门控(`ATOMCODE_TODO` /
> `ATOMCODE_REQUEST_USER_INPUT`);在无条件块里点名它们会撞坏已有门控不变式测试
> (`todo_guidance_present_only_when_enabled`、`skills_block_points_at_ui_answering`),
> 且会在工具未挂载时引用不存在的工具。工具级指令仍由**门控的** `## TASK TRACKING`
> (点名 `todowrite`)与 `## ASKING THE USER`(点名 `request_user_input`)承载,默认态下
> 二者与本泛化引导自然连上。

`For simple changes ... : just do it` 那一行**保持不变**——这是分层策略里「小任务不复述、零噪音」的分支。

### 改动 2 —— `## OUTPUT`（persona.rs 约 L585），消除与改动 1 的冲突

```diff
- Do NOT restate what the user said — just do it.
+ Do NOT restate what the user said as filler — just do it. (Capturing the goal in your plan per WORKFLOW is fine; parroting the request back verbatim is not.)
```

## 行为效果

- **小任务**（rename / 一行修复 / config 调整 / 纯信息问答）→ 照旧直接做，零复述。
- **多步任务** → `UNDERSTAND` 落到 `todowrite` 首项 = 用户要的产出（复用现有 `## TASK TRACKING` 的 3+ 步触发）；todo 未启用时退回一句话目标（即现有 `PLAN` 步）。
- **目标真歧义**（多种不同成功解释，且非可自行合理决定的实现细节）→ 才走 `request_user_input` 澄清，再动手。
- **OUTPUT 冲突** → 两处措辞明确「计划里承载目标 OK，原样复读用户话不行」，不互相打架。

## 兼容性与依赖

- 纯 `RULES` 常量文本改动，`RULES` 总是注入，覆盖所有默认 coding 对话路径（TUI / CLI / 三端共享 persona）。
- 复用既有零件：`todowrite`（`ATOMCODE_TODO`）、`request_user_input`（`ATOMCODE_REQUEST_USER_INPUT`），本改动不新增门控、不改注入逻辑。
- todo 或 request_user_input 被 opt-out 关闭时，引导措辞仍成立（退回一句话目标 / 不提澄清工具亦不矛盾，因为「Only if ... ask」是条件句）。

## 测试计划

- persona 单测（`persona.rs` 既有 `#[cfg(test)]`）：断言 `RULES` 含新的 `UNDERSTAND` 步与新 bullet 关键短语；断言 `## OUTPUT` 已改为 `as filler` 变体（防回归到旧的无条件 `Do NOT restate`）。
- `cargo check -p atomcode-coding` 编译通过。
- 全量 coding / persona 相关测试绿。

## 阶段二（defer，不在本 spec 实现）

将现有 Plan 模式从「只读 + 审批」升级为 codex 式三阶段：探索 → 意图对齐（复述 goal + 成功标准）→ 实现规约；并把 `request_user_input` 作为 Plan 模式下的通用澄清入口。待阶段一真机验证有效后另起 spec。

## 真机验证

本改动为提示词行为改动，需真机观察（尤其 GLM/DeepSeek 弱模型是否在多步任务自发把目标落到 todowrite 首项、小任务是否仍零复述）。合并前按 atomcode 惯例标注「未真机」。
