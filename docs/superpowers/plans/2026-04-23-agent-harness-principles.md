# atomcode Agent Harness —— 四条设计原则

**Status:** roadmap / 顶层设计文档,不是可执行 plan。每条原则对应一个或多个 implementation plan。

**Scope:** 这份文档规定 atomcode 作为**通用 agent framework** 该做什么,不该做什么。所有 "agent 层" 改动(prompt、guard、discipline、tool trait 扩展)都应该能映射到下面四条之一;映射不上就说明在做特定生态优化,不属于 harness 本身。

---

## 为什么要有这份 doc

2026-04-23 session 做 cadence reflection 时,scope 从"让 agent 快速高效搞定任务"的四原则框架收窄到了只实现第 3 条,后面关于反思响应率的所有讨论都绕着 1/4 的功能打转。dogfooding 反向暴露:

- Agent 前 N 步无目标乱撞 → 原则 1 缺失
- `cargo check` 失败后瞎试不同 flag → 原则 2 缺失
- `sed -i` 全项目改源码无人拦 → 原则 4 缺失
- 长任务目标漂移 → 原则 3(已部分落地)

这份 doc 把四条写死,避免未来再次 scope 收窄。

---

## 核心诊断(一句话)

**Agent 慢/走偏 = 搜索空间没收敛**。harness 的使命是帮 agent 快速收敛搜索空间,不是教它特定生态的坑。四条原则是收敛搜索空间的四个正交向量。

```
 假设层 │ 原则 1 Hypothesis-first   ─ 动手前声明预期
 反馈层 │ 原则 2 Actionable failure ─ 失败时给下一步候选
 节拍层 │ 原则 3 Cadence reflection ─ 周期自检目标
 代价层 │ 原则 4 Cost awareness     ─ 掂量每个动作的不可逆度/token/time
```

---

## 原则 1 · Hypothesis-first —— 让搜索空间显式化

### What

在 tool args 的 schema 里加可选字段 `hypothesis: string`。高成本 tool(bash 复杂命令、大文件 read、全仓 grep)调用时,建议(不强制)声明"我期望看到 X"。Framework 执行后把 tool 输出和 hypothesis 对比,落差大时注入反思 prompt。

### Why(dogfooding 证据)

- dead-code 任务中 agent 瞎试 25 次 RUSTFLAGS,没在第 1 次失败后问"为什么空输出"
- session.rs 调查中 agent 在错误 crate 里反复 grep,没声明"我在验证 session 持久化在 tuix 还是 daemon"

这是**所有 agent 框架的共性缺陷**,不是模型特异的。模型训练默认"产出下一个动作",不默认"验证上一个动作的预期"。

### How

- Tool args schema 扩展:可选 `hypothesis` 字段
- Framework 层:tool 执行后 diff(output, hypothesis),差异大注入提示
- Tool 层:按 cost 决定是否"推荐填 hypothesis"

### 跨模型可靠度

中。依赖模型愿意填字段,但至少**每个 tool call 都变成"验证"载体**,比每 N 步一次反思密度更高。

### Status: 未启动

### Related plan: TBD(尚未开)

---

## 原则 2 · Actionable failure —— 失败是开始不是终点

### What

每个 tool 的**失败分支**必须返回 2-3 条 **candidate next actions**。候选**由 tool 自己提供**(它知道自己的 domain),framework **不代说**。

示例:
- `read_file` 大文件 → skeleton + `read offset=X limit=Y` 候选(**已实现** ✓,见 `crates/atomcode-core/src/tool/read.rs`)
- `grep` 0 匹配 → "放宽 regex / 换路径 / 改大小写敏感" 候选
- `bash` 非零 exit → stderr 的前 10 行 + 建议加 `-v` / `--verbose`

### Why

dogfooding 每次都看到同一模式:tool 失败 → agent 不知道下一步是啥 → 换种参数再试 → 又失败 → 循环。agent 元认知弱时尤其严重。**让 tool 自己说 "下一步试这个"** 是最便宜的救命稻草。

### How

Tool trait 扩展:

```rust
trait Tool {
    // 现有
    fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult>;

    // 新增
    fn candidate_next_actions_on_failure(&self, failure: &ToolError) -> Vec<String> {
        vec![]  // 默认空,各 tool 按需实现
    }
}
```

Framework 在 tool 失败时把 candidate 拼到 error message 尾部,进 conversation。

### 跨模型可靠度

**高**。不依赖模型产生 idea,候选来自 tool 代码。模型只需"从 3 条里挑 1 条"——这是模型最擅长的任务。

### Status: 部分实现(只有 `read_file` skeleton 做了)

### Related plan: `tool-failure-hints`(未开)

---

## 原则 3 · Cadence reflection —— 周期反思不依赖是否卡住

### What

每 N 次 tool call,post-turn 注入一段 "restate goal / what ruled out / next output" 的反思 prompt。**不是 guard(卡了才拦),是节拍器(周期自检)**。

### Why

agent 在"慢慢走错路"时 guard 不触发,但走完后回头一看已经浪费 10+ 步。周期反思让 agent 每 N 步校准一次目标。

### How

已实现。见 `crates/atomcode-core/src/agent/discipline.rs` 的 `should_inject_reflection` + `reflection_prompt`。默认 `reflection_cadence = 7`,0 禁用。

### 跨模型可靠度

**低到中**。2026-04-23 dogfooding 证实:

- Claude 类模型能按 prompt 三点作答,收敛效果好
- GLM-5.1 无论措辞(建议式/命令式/温和元提示)全部忽略,继续瞎试

说明 cadence reflection 是**认知层软约束**,对模型顺从性敏感。**不能作为主防线**,只能锦上添花。跨模型硬约束要靠原则 2 和 4。

### Status: 已 ship(v4.20)

### Related plans:
- `2026-04-23-cadence-reflection.md`(已完成)
- `tui-render-silent-injections`(未开 —— 解决 TUI 看不到注入的架构 gap)

---

## 原则 4 · Cost awareness —— 把代价做进框架

### What

Agent 对"错了撤销多贵"无感。framework 的独特价值就是**算给它看**:

| 轴 | 指标 | 怎么让 agent 感知 |
|---|---|---|
| 爆炸半径 | `read-only / mutating / irreversible` 三级 | Tool 声明自己属于哪级,framework 据此决定 approval |
| Token 代价 | 本 turn 累计 / context budget | 超阈值在 tool output 尾部追加 "context 70% used" |
| 时间代价 | wall clock | 超阈值在下 turn 注入 "elapsed 3min, $0.xx" |

### Why

dogfooding 三次都见 agent 干 `sed -i` 全项目改源码:

- reflection 拦不住(软约束,模型依赖)
- guard 没这层(原 Pattern 1/2 只 catch read_file)
- 模型自己分不清 `ls` 和 `sed -i` 的爆炸半径

**只有 framework 能可靠地做代价分级**。模型能读到分级,但自己算不出。

### How

Tool trait 扩展:

```rust
trait Tool {
    fn reversibility(&self, args: &str) -> Reversibility {
        Reversibility::ReadOnly  // 默认最低爆炸半径
    }
}

enum Reversibility {
    ReadOnly,       // ls, cat, grep
    Mutating,       // mv, edit_file(自家文件)
    Irreversible,   // sed -i, rm -rf, git reset --hard
}
```

Framework 按分级决定 `approval`:
- `ReadOnly` → auto
- `Mutating` → session-default(user 可设)
- `Irreversible` → 强制 approval prompt

Token/time 代价通过 AgentEvent 推到 UI + 超阈值注入 meta message。

### 跨模型可靠度

**最高**。分级由 tool 代码决定,model 只能看不能改。即使模型想无视 approval,framework 层直接拒绝执行。

### Status: 未启动

### Related plans:
- ~~`mutating-bash-approval`~~ —— 取消。原构想是重新引入 `sed -i` / `perl -pi` / `awk -i` 等 pattern 识别,但 upstream 于 2026-04-22 (`ff540aa`) 明确以 effect-based 机制(post-exec `snapshot_workspace_changes` diff + 文本 nudge)替代 pattern 枚举,判定 pattern list 必然 whack-a-mole。要重开需先 brainstorm 与 upstream 决策对齐的新方向(例如 pre-exec 预测、或升级 effect nudge 为 pre-exec gate)。
- `tool-reversibility-trait`(更广,所有 tool 声明 reversibility)
- `token-cost-feedback`(P2)

---

## 原则之间的关系

```
原则 1 Hypothesis        ├ 前置约束:动手前声明预期
原则 2 Actionable failure├ 反馈约束:失败时给候选  ← 硬约束(tool 代码)
原则 3 Cadence reflection├ 周期约束:每 N 步校准目标 ← 软约束(模型依赖)
原则 4 Cost awareness    └ 代价约束:每个动作的代价  ← 硬约束(tool 代码)
```

**原则 2 和 4 是"跨模型硬约束"** —— 不管什么模型都收效。
**原则 1 和 3 是"cognition 层软约束"** —— 收效取决于模型顺从性。

**推论:应优先实现硬约束层**。dogfooding 证据(GLM 忽略 reflection)已经支持这个排序。

---

## Ship 顺序(建议)

| 阶段 | 原则 | Plan | 估规模 | 理由 |
|---|---|---|---|---|
| 已 ship | 3(部分)| `cadence-reflection` | 8 commits, ~400 loc | 最早可行,最小 blast radius |
| 已 ship | 4(路径轴)| upstream `ff540aa` path-aware approval | — | 敏感路径 / workspace-escape 自动走 `RequireApprovalAlways`,不占本 roadmap 一刀 |
| ~~下一刀~~ | ~~4(命令模式识别)~~ | ~~`mutating-bash-approval`~~ | — | **取消**。与 upstream 2026-04-22 effect-based 决策冲突,见上方"Related plans"脚注 |
| **下一刀** | **2** | **`tool-failure-hints`** | ~10 tasks | Tool trait 扩展 + 所有现有 tool 的 failure path。结构性投入大,但一次到位;纯加法,不与 upstream 任何决策冲突 |
| 长尾 | 1 | `hypothesis-slot` | ~8 tasks | schema 层 + 对比机制,需要 2/4 已稳了再推 |
| 长尾 | 4(剩余)| `tool-reversibility-trait` + `token-cost-feedback` | 各 ~5 tasks | 4 的完整形态,增量做 |

**不做的决策**(刻意列出):

- ❌ 在 BLOCKED 文案里硬编码 "use grep / pandoc / cargo clean" —— 那是 tool 自己该说的,framework 说就越权
- ❌ System prompt 堆特定生态 knowledge —— rules 要瘦,domain 要外置
- ❌ Per-language 优化(Rust / TS / Python 各一套) —— atomcode 的价值是通用 agent loop,不是多语言认识
- ❌ 只在单个模型上验证就 ship agent 行为改动 —— 见 memory `feedback_cross_model_verify.md`

---

## 元原则(overarching)

> **Framework 不教 agent 特定工具怎么用,framework 规定 agent 必须回答哪些问题:假设是什么 / 代价是什么 / 失败的候选路径是什么 / 每 N 步目标还对吗。**

这四个问题**完全语言/生态/任务中立**。答不出来时 framework 有依据拦(hard guard);答得出来时 agent 自然走得快。

---

## Revision policy

- 每完成一个 related plan 更新对应原则的 Status 段
- Ship 顺序可以随 dogfooding 新证据调整,但**不要轻易加第 5 条原则**。新原则出现前先问:能否映射到 1-4 之一?
- 这份 doc 的改动应与代码 commit 同批次(不是独立 commit)
