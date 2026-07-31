# 工具前进度话术(PROGRESS SIGNPOSTS / preamble)—— 设计文档

- 日期：2026-07-30
- 分支：release/v5.0.3
- 范围：`crates/atomcode-coding/src/persona.rs`（`RULES` 常量的两节 + `FIRM_EXECUTION_DISCIPLINE` 常量 + 测试模块）
- 类型：纯提示词（persona）改动，无新工具、无新模式、无新 env 门控

## 背景与现象

实测同一任务（"帮我打通这台机器的免密登录…"）：

- **GLM-5.2**：每批工具调用前都有一句话术（"我来帮你配置免密登录。让我先检查当前的 SSH 配置和密钥情况。"），用户能跟上进度。
- **deepseek-v4-flash**：**零话术**，用户输入后直接甩出 Bash 工具调用，用户看不到"它要做什么"。

## 根因诊断

deepseek 零话术是提示词层面三因素叠加、且**只对 deepseek 生效**：

1. `## OUTPUT`（两模型都有）：`Lead with action, not reasoning.` —— deepseek 读成"别说话，直接干"。
2. `## EXECUTION DISCIPLINE (MANDATORY)`（`FIRM_EXECUTION_DISCIPLINE`，`model_needs_firm_execution` **只 deepseek**，GLM 显式排除）：`Act decisively` / `FINISH THE JOB` 的 execute-now 硬框架**强化**了"立即执行"，却**没有任何"先报一句"的对冲**。
3. 意图/话术引导只在软 `RULES`（如 UNDERSTAND 的"一句话目标"）：deepseek 对软规则权重低（这正是 FIRM 块存在的原因），故忽略。

GLM 无 EXECUTION DISCIPLINE 硬块、更 capable，自然遵循软引导 → 每批工具前来一句 signpost。

## 三家对比（决定取向）

| 工具 | 立场 | 关键原文 |
|---|---|---|
| **codex** | 强制 preamble（**通用**，非分模型） | `Before making tool calls, send a brief preamble to the user explaining what you're about to do`；配 1-2 句/8-12 词上限 + 相关动作合并 + trivial-read 例外 + 8 例句 |
| **opencode** | 分模型：强模型鼓励、简洁模型抑制 | beast: `Always tell the user what you are going to do before making a tool call`；gemini/default/kimi: `Avoid preambles... Get straight to the action` |
| **oh-my-pi** | 通用抑制（action-first） | `<critical> NEVER narrate ...`；子代理 `No progress updates` |

**取向决策**：采纳 **codex 派（选项 A）** —— 通用"工具前报一句 signpost"，两模型都要，字数严格上限防话痨。用户偏好（GLM 那张图是"想要的样子"）与 codex 派一致。触发频率取 **codex 式：成批发 + trivial-read 例外**（不是每次调用都发，也不是只在大任务前发）。

## 具体改动

### 改动 1 —— 新增通用 `## PROGRESS SIGNPOSTS` 小节（persona.rs 的 `RULES` 常量）

放在 `RULES` 内，**紧接 `## OUTPUT` 之前**（与沟通节奏相关，且 OUTPUT 改动 2 会引用本节名）。文案：

```
## PROGRESS SIGNPOSTS:
Before a batch of tool calls, send ONE short line saying what you're about to do — a signpost the user follows along with, not a reasoning dump. Keep it to a single sentence (aim ≤12 words). Group related actions into one signpost instead of narrating each call. After the first batch, connect briefly to what you just learned. Skip the signpost for a single trivial read (one file read or one lookup) unless it's part of a larger action. A run of tool calls with zero text leaves the user blind — that is worse than one plain line.
```

约束：**不点名任何工具**（吸取上一改动的门控不变式教训——`RULES` 无条件注入，不得引用 env 门控工具）。字数上限 + trivial-read 例外防话痨。

### 改动 2 —— 松绑 `## OUTPUT` 里与之打架的一句（persona.rs `RULES`，约 L584）

```diff
- When executing tasks: keep text brief and direct. Lead with action, not reasoning.
+ When executing tasks: keep text brief and direct. Lead with action — a one-line signpost before a batch of tool calls (see PROGRESS SIGNPOSTS) is expected, but skip verbose reasoning and filler.
```

### 改动 3 —— deepseek 的 FIRM 硬版（`FIRM_EXECUTION_DISCIPLINE` 常量，deepseek-only）

在 `FIRM_EXECUTION_DISCIPLINE` 里补一条硬 bullet，并与现有 `Act decisively` / `FINISH THE JOB` 咬合，防止两条指令打架：

```
- SIGNPOST BEFORE ACTING: before each batch of tool calls, say in ONE short sentence (≤12 words) what you're about to do. A run of tool calls with zero text leaves the user blind. This is the required progress signpost, NOT the verbose reasoning banned elsewhere; 'Act decisively' / 'FINISH THE JOB' mean act WITH a one-line heads-up, never in silence.
```

理由：deepseek 对软规则权重低，且 FIRM 块的 execute-now 基调是零话术主推手；只有把话术写成 FIRM 硬版并显式和 execute-now 对齐，才可靠地到达 deepseek。这条是 atomcode 特有的（三家都没有，因为它们的模型都够强，通用软规范即可）。

## 行为效果

- **deepseek**：通用 SIGNPOSTS + FIRM 硬版双保险 → 每批工具前一句 signpost，不再零话术（接近 GLM）。
- **GLM**：本就遵循，现有通用规范托底 + 字数上限防话痨。
- **两模型**：`## OUTPUT` 不再把"简洁"塌成"零文字"。

## 兼容性与依赖

- 纯文本改动：`RULES` 总是注入（两节覆盖所有默认 coding 对话）；`FIRM_EXECUTION_DISCIPLINE` 仅 `model_needs_firm_execution`（deepseek）注入。
- 不新增工具/模式/env 门控。`## PROGRESS SIGNPOSTS` 不点名工具，故不受 `ATOMCODE_TODO` / `ATOMCODE_REQUEST_USER_INPUT` 门控影响、不撞门控不变式测试。
- ⚠️ 落地注意 `\` 续行焊接坑（上一改动 code-review 抓到的真 bug）：新 bullet/段落之间用**字面换行**，行尾**不要**误加 `\`；测试补边界断言。

## 测试计划

- `## PROGRESS SIGNPOSTS` 存在且含关键短语（如 `Before a batch of tool calls` / `leaves the user blind`）；该节内**不出现**裸工具名 `todowrite` / `request_user_input`（门控不变式）。
- `## OUTPUT` 不再含裸 `Lead with action, not reasoning.`（锁死回归）；含新的 `signpost before a batch of tool calls` 措辞。
- `model_needs_firm_execution` 模型（`coding_persona("deepseek-v4-flash", …)`）含 `SIGNPOST BEFORE ACTING`；GLM（`coding_persona("glm-5.2", …)`）**不含** `SIGNPOST BEFORE ACTING` 但**含** `## PROGRESS SIGNPOSTS`（验证分层）。
- 边界断言：新增小节/bullet 未被 `\` 续行焊进相邻行（参考上次 `proceed.\n- REPRODUCE` 断言的形式）。
- `cargo test -p atomcode-coding` 全绿。

## 真机验证

提示词行为改动，需真机观察：deepseek-v4-flash 多步任务是否每批工具前出现一句 signpost、是否仍不话痨（trivial read 不报）、GLM 是否仍正常且未变冗长。合并前按惯例标注「未真机」。
