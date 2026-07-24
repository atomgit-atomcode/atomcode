# `/skills` 多 skill 组合 — 设计

- 日期：2026-07-24
- 状态：设计已确认，待写实现计划
- 范围：`atomcode-tuix` 的 `/skills` 斜杠命令解析 + 一个新纯函数；不改内核、不改 `Skill`/`expand_for_injection`。

## 动机

用户希望针对**一个任务同时引用多个 skill**（例：A=agent skill、B=代码规范、C=执行流程），让它们的指令一起作用于该任务。

用户实际尝试了 `/skills adapt-agent skill-creator 路径在哪`（见反馈截图），结果模型卡死在 pondering。

## 现状（根因）

`commands.rs` 的 `"skills"` 分支用 `splitn(2, whitespace)` 解析参数：

```rust
let mut parts = arg_trim.splitn(2, char::is_whitespace);
let skill_name = parts.next().unwrap_or("");            // 只取第一个词
let skill_args = parts.next().unwrap_or("").trim_start(); // 剩余全部 = 该 skill 的参数
```

所以 `/skills adapt-agent skill-creator 路径在哪` 被解析成：加载 `adapt-agent`，把 `"skill-creator 路径在哪"` 当作它的输入。模型把 `skill-creator` 误当成"一个要查路径的对象"，陷入无效探索。

**结论：多 skill 组合从未被支持**（不是坏掉的 bug，是缺功能）。底层 `expand_skill` 一次只查一个 skill，将其正文注入成一条合成用户回合发给模型。

## 设计

### 1. 解析：空格分隔 + 贪婪前缀匹配（纯函数）

新增纯函数（便于 TDD、无 I/O）：

```rust
/// 从参数串前缀贪婪切分出 skill 名列表，返回 (skills, task_args)。
/// - 从左到右按 whitespace 分词；
/// - 只要当前词能解析到一个已知 user-invocable skill，就归入 skills；
/// - 遇到第一个无法解析的词，它及其之后的全部原样作为 task_args；
/// - skills 去重（保留首次出现顺序）。
fn split_skill_names(arg: &str, resolve: impl Fn(&str) -> bool) -> (Vec<String>, String)
```

`resolve(name)` 由调用点用 `SkillRegistry` 提供（判定"是否为已知 user-invocable skill"，复用 `expand_skill` 里已有的 `reg.get(name)` + `user_invocable` 判定口径，保持一致）。

**注意 `task_args` 的切分必须按原始字符串的偏移还原**，不能用 `split_whitespace().join(" ")` 重组——那会压平任务描述里的多空格/换行。实现按"已消费的前缀长度"从原串切尾。

### 2. 单 skill 完全向后兼容（重点）

单 skill 是"零个额外 skill 词"的**自然情形**，走同一套解析，不是特例分支：

- `/skills brainstorming` → skills=[brainstorming]，task_args=""（同今天）
- `/skills brainstorming 做个登录页` → skills=[brainstorming]，task_args="做个登录页"（`做个登录页` 首词非 skill → 整段是任务，同今天）
- `/skills adapt-agent skill-creator 路径在哪` → skills=[adapt-agent, skill-creator]，task_args="路径在哪"（新增）

因此现有单 skill 用法**逐字节行为不变**，无回归。

### 3. 组合注入（顺序 = 用户书写顺序）

对 `skills` 列表按顺序逐个调用现有 `expand_skill(ctx, name, task_args)`，将各自返回的注入文本拼接成**一条**合成用户回合，再 `submit_agent_turn`。

- **顺序尊重用户左到右书写**（A→B→C），不做优先级重排。
- 各 skill 之间插入清晰分隔（如 `\n\n---\n\n`），降低弱模型把 A 的内容误当 B 的概率。
- 任务参数 `task_args` **传给每个 skill 的 `expand`**（决策见下）。

**决策：task_args 传给每个 skill（而非只在末尾拼一次）**
- 理由：保留 `$ARGUMENTS`/`$N` 占位符语义——若组合里含命令式 skill，占位符能被正确填充；无占位符的 skill 会各自在末尾追加一行 `ARGUMENTS: <task>`。
- 代价：目标用例（agent / 规范 / 流程等行为型 skill，通常无占位符）下，任务会被追加多次（每个 skill 一次）。判断这对弱模型是**良性冗余甚至有强化作用**，可接受。
- 备选（若日后实测冗余噪声明显）：改为各 skill 传空参、任务只在整条消息末尾拼一次；代价是含 `$ARGUMENTS` 的 skill 在组合里占位符不被填充。v1 不采用。

### 4. 反馈：回显已加载 skills（防 typo 静默漂移）

提交合成回合前，渲染一行 `CommandOutput`（新增 i18n `Msg`）：

```
已加载 skills：adapt-agent · skill-creator
```

作用：贪婪匹配唯一暗坑是——第二个及之后的 skill 名**打错字**时，会掉进 `task_args` 被静默丢弃。回显让用户一眼看到"只加载了 N 个"，立即察觉误拼。低成本、高价值。

### 5. 错误与边界

- **首词非 skill**：`skills` 为空 → 沿用现有 `SkillUnknown { name }` 报错（同今天，零回归）。报错里的 `name` 用第一个词。
- **任务散文恰以某 skill 的 slug 开头**：会被贪婪吃掉当作 skill（贪婪匹配固有代价，用户已知情选择）。因只吃**前缀**，遇到首个非 skill 词即停，损害有限；用户重排词序即可绕过；回显帮助发现。v1 不引入 `--` 等显式分隔符（YAGNI）。
- **去重**：`/skills a a 任务` → skills=[a]，注入一次。

## 实现范围

- 改 `crates/atomcode-tuix/src/event_loop/commands.rs` 的 `"skills"` 分支：用 `split_skill_names` 替换 `splitn(2)`，循环 `expand_skill` 拼接，加回显。
- 新增纯函数 `split_skill_names`（同文件或就近模块）。
- 新增 1 条 i18n `Msg`（回显文案），en/zh 同步。
- 不改 `atomcode-core::skill`、不改 `expand_skill`/`expand_for_injection`、不改内核。

## 测试（TDD）

纯函数 `split_skill_names`（用假的 `resolve` 闭包，无需真 registry）：
- 多 skill + 任务：`"adapt-agent skill-creator 路径在哪"` → (`[adapt-agent, skill-creator]`, `"路径在哪"`)
- 单 skill + 任务（兼容）：`"brainstorming 做个登录页"` → (`[brainstorming]`, `"做个登录页"`)
- 单 skill 无任务（兼容）：`"brainstorming"` → (`[brainstorming]`, `""`)
- 首词非 skill：`"路径在哪"` → (`[]`, `"路径在哪"`)（调用点据空列表报 unknown）
- typo 中断：`"adapt-agent skil-creator 路径在哪"`（`skil-creator` 不解析）→ (`[adapt-agent]`, `"skil-creator 路径在哪"`)
- 去重：`"a a 任务"` → (`[a]`, `"任务"`)
- task_args 保留原始空白/换行（不被重组压平）
- 组合注入：多 skill 拼接顺序 = 输入顺序、含分隔符

## 非目标（defer）

- 让模型在一个任务里自主连续 `use_skill` 组合多个 skill（属提示词/persona 软调优，非机制缺口；`use_skill` 本就无"只能调一次"的硬限制）。
- `--` 等显式 skill/任务分隔语法。
- 组合时注入体量的预算/截断（显式调用视为用户已知情选择；如需可复用既有 catalog 预算思路，v1 不做）。
