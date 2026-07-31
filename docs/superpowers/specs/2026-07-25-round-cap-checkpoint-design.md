# 回合上限：从「硬错误中断」改为「检查点问询」

- 日期：2026-07-25
- 状态：设计已确认，待写实现计划
- 相关记忆：`project_runaway_toolcall_loop_repetition_fuse`（签名熔断）、`project_brainstorming_questions_via_request_user_input`（request/respond 往返）

## 背景与问题

单个用户回合内，模型每次调工具算「一轮」。kernel 在 `agent.rs:1663` 有一个粗兜底熔断：`round > max_rounds`（默认 200）时 `emit(AgentEvent::Error{"max rounds (200) reached"})` + `finish_turn(StopReason::MaxRounds)`。

真实场景暴露的毛病：一个**合法的长任务**（实测 `200 轮 = 222 工具 · 2h0m23s · 305K tokens`）撞到上限，被渲染成**红色 `✗ 已中断` 错误**。但用户点「继续」就能接着正常跑完——说明这不是一次真正的失控，硬停只是纯摩擦，且"错误"观感误导（让人以为崩了，其实只是长）。

关键事实：atomcode 已有**真正的失控防护**——跨轮签名熔断（3 轮 nudge / 6 轮 `StopReason::RepeatLoop`，见 `project_runaway_toolcall_loop_repetition_fuse`）。这个 200 轮熔断的**唯一剩余职责**是兜住签名熔断抓不住的 **args 漂移型失控**（每次工具入参都略不同，签名去重漏掉）。代码注释自称 "Coarse round-cap backstop"。

所以问题定性：**不在"要不要有上限"，而在"一个安全阀被画成了错误、且阈值/可发现性对长任务不够"。**

## 参考对照（opencode / codex）

- **opencode**：`maxSteps` 默认 `Infinity`（不限），per-agent `steps` 字段可配（`session/prompt.ts:1231`）；到达时不报错，而是注入 `MAX_STEPS_PROMPT` 让模型优雅转纯文本收尾。另有 "doom_loop" 重复检测（连续 3 次同工具同入参 → **弹权限问用户**，`session/processor.ts:35,519`）。
- **codex**：主循环 `turn.rs:225` **无回合计数器**，靠 token 预算 + 压缩兜底（注释：`turn.rs:345` "as long as compaction works well … we shouldn't worry about being in an infinite loop"）。
- 结论：两家都**刻意避免"到点硬报错中断"**。本设计取 opencode `doom_loop` 的"问用户"精神，但复用 atomcode 已有的 continue 语义，不新建审批流。

## 目标

1. 撞到回合上限时，不再是红色错误，而是一个**可上下选的「继续 / 停止」问询卡片**（样式与 `request_user_input` 一致）。
2. 保留"周期性拦截失控"这一核心价值：每次「继续」重新武装计数器。
3. 顺带补上**可发现性**：加 `[coding] max_rounds` TOML 配置段。
4. **对所有非 TUI 路径（daemon/webui/headless/测试）零行为变化、零挂起风险。**

## 非目标

- 不改签名熔断（3/6）——真死循环仍归它管。
- 不改 `StopReason::MaxRounds` / `finish_turn` / Cancel 路径的语义。
- webui/daemon 的镜像实现——**本版 TUI-only，webui 列为后续 defer**（符合项目 TUI-first 惯例）。
- 递增间隔（200→400→800…）——作为未来优雅升级，本版用恒定间隔。

## 设计

### 1. 触发点与控制流（kernel，`agent.rs` 约 1663）

现状：
```
round > max  →  emit(Error) + finish_turn(MaxRounds)
```
改为（仅当 checkpoint 开关打开时走此路径，见 §2）：
```
round > cap  →  resp = self.rt.request("round_cap_checkpoint", {round, cap}).await
              →  resp.continue == true  : cap += max（重新武装），continue 主循环
              →  否则 / Null            : finish_turn(StopReason::MaxRounds)   // 语义不变
```

- **再武装机制**：把原来固定的 `max` 换成可变的 `cap`（初值 = `max`）。每次「继续」`cap += max`，实现恒定 `max` 轮的间隔。最坏情况恒定：任何时刻一个静默失控最多再烧 `max` 轮就会再次拦到人。
- **往返传输复用现成机制**：`RequestCtx::request(kind, payload)`（`request.rs`）。`self.rt` 在熔断点就是 `RequestCtx`（`agent.rs:951/850`）。主循环已在抽 `AgentCommand::Respond`/`Cancel`（`agent.rs:1225/1372`），park 在 `request().await` 上是安全的——`request_user_input` 工具已证明这条路径可用。
- **kind 与 schema 的归属**：kernel 已经硬编码了 `"max rounds ({max}) reached"` 与 `StopReason::MaxRounds`，本就不对这个熔断完全 agnostic。因此 kernel 直接持有固定 kind 字符串、最小 payload `{round, cap}`、最小响应 `{continue: bool}`。响应解析 `{continue:bool}` 与驱动无关，保持简单。
- **kind 常量单一来源**：kernel 导出 `pub const ROUND_CAP_CHECKPOINT_KIND: &str = "round_cap_checkpoint"`，TUI 分派臂 import 同一常量（不重复字面量），与现有 `REQUEST_USER_INPUT_KIND` 的做法对齐。

### 2. 关键安全设计：Builder 开关 `round_cap_checkpoint(bool)`，默认 `false`

这是防止"哑驱动 park 死等"的核心决定。

- **默认 `false`** → 完全保持**今天的行为**（`emit(Error)` + `finish_turn(MaxRounds)`）。daemon/webui/headless/测试**零变化**，且因为根本走不到 `request()` 路径，**不可能挂起**。
- **只有 TUI 驱动**（`coding/parts.rs`，即挂 checkpoint 渲染臂的那条装配路径）把它设 `true`。
- 好处：一个不实现 `round_cap_checkpoint` kind 的驱动永远收不到该 Request，不会因无人 `Respond` 而 park；fail-closed 天然成立；headless 的 `request_timeout` 问题自动消失（它压根不开这个开关）。

`AgentBuilder` 新增 `round_cap_checkpoint(bool)`（默认 false）。kernel 熔断点按此开关二选一：开→走 §1 的往返；关→走今天的 `emit(Error)+MaxRounds`。

### 3. 渲染（TUI）—— 样式 B（信息型）

新增 `kind == "round_cap_checkpoint"` 的分派臂（挨着 `event_loop/mod.rs:14830` 现有 `REQUEST_USER_INPUT_KIND` 臂）。

**复用 `build_user_input_rows`（`render/retained.rs:2729`，Single 模式，`custom:false`）** 渲染，样式与 `request_user_input` 逐字一致。目标外观：

```
  轮次上限                                             ← Header chip：粗体·橙色 (Role::Plan)
                                                        ← 空行
  已运行 200 轮（222 工具 · 2h0m · 305K tokens），继续吗？  ← 问题行：粗体 (Role::Secondary)
                                                        ← 空行
❯ 1. 继续                                              ← 光标行：❯ + 粗体橙
     再跑 200 轮后重新确认                             ← 暗淡描述 (Role::Muted faint)
  2. 停止
     结束本回合（等同 MaxRounds）
                                                        ← 空行
  ↑/↓ 选择 · Enter 确认 · Esc 停止                      ← 底部 hint（暗淡）
```

- **统计数字（222 工具 · 2h0m · 305K）由 TUI 用自己的轮次追踪状态填**（这些数字 TUI 渲染 `已中断` summary 时本就在手），kernel 的 payload 只传 `{round, cap}`。TUI 用 `cap` 渲染问题行的「200 轮」与描述里的「再跑 200 轮」。
- **响应按索引映射**（0=继续 / 1=停止）转成 `{continue: bool}` 后再 `AgentCommand::Respond`——**不把中文标签写进 kernel/wire**，kernel 只认 `continue` 布尔。
- **实现取向**：复用 `UserInputPanelView` + `build_user_input_rows` 做渲染，但 checkpoint 的按键/响应是 TUI 本地的两选项逻辑，产出 `{continue:bool}`；不改动 `request_user_input` 已测试的响应路径（`build_response → UserInputResponse`）。
- **非 unicode 降级**：`❯→>`、`▏→|` 走现有 glyph 门控（`retained.rs:2782`），豆腐块问题已有 backstop。

### 4. 配置（可发现性）

新增 `[coding]` TOML 段的 `max_rounds` 字段，对齐已有的 `[subagent] max_rounds`（默认 200）与 `[loop_config] max_rounds`（默认 100）。

- 优先级：**env `ATOMCODE_TURN_MAX_ROUNDS` > `[coding] max_rounds` (TOML) > 默认 200**。
- `0` = 关闭检查点、回到无限（复用现有 `if cfg.max_rounds != 0` 门控，`parts.rs:1332` / `assemble.rs:112`）。
- `save()` 写该段时带解释性注释（与 datalog/notifications 等段一致），让用户能看见并编辑。

### 5. 不动的东西

- 签名熔断（3 nudge / 6 停 `RepeatLoop`）完全不变。
- `StopReason::MaxRounds`、`finish_turn`、`cancel_pending` 语义不变。

## 边界与错误处理

- **无 requester（测试/未接驱动）**：`ctx.request` 无 requester → `Null`（`tool.rs:157`）。但这些场景 checkpoint 开关为 false，走不到 request——保持今天行为。
- **checkpoint 期间 Cancel**：`cancel_pending()` 把 pending 往返解成 `Null`（`request.rs`），走 `Cancelled` 路径。需测试确认不卡死、不误判为「继续」。
- **`Null` 响应**：任何降级 `Null` 一律映射为「停止」（fail-closed）→ `finish_turn(MaxRounds)`。
- **park 位置**：检查点在主循环顶部、构建下一次请求之前触发，与 approval 的 park 位置同类，安全。

## 测试

- **kernel**（`tests/failure_perception.rs` / `tests/tool_loop_guard.rs` 扩展）：
  - checkpoint 开、响应「继续」→ `cap` 抬高、回合继续、不终结；
  - 响应「停止」→ `StopReason::MaxRounds`；
  - 响应 `Null`（fail-closed）→ `StopReason::MaxRounds`；
  - checkpoint 期间 Cancel → `Cancelled`，不卡死；
  - checkpoint 关（默认）→ 保持今天的 `emit(Error)+MaxRounds`（回归保护）。
- **config**：`[coding] max_rounds` 解析；env 覆盖 TOML；`0` 关闭；缺省 → 200。
- **TUI**：分派臂渲染 checkpoint picker；Enter on「继续」发 `{continue:true}`；Esc / 选「停止」发 `{continue:false}`；ASCII 降级；统计数字取自 TUI 轮次状态。

## 落点清单（file:line 锚）

- `crates/atomcode-kernel/src/agent.rs:1663` — 熔断分支改造 + `cap` 再武装 + Builder 字段读取。
- `crates/atomcode-kernel/src/agent.rs`（Builder 区，约 3310 `max_rounds` setter 附近）— 新增 `round_cap_checkpoint(bool)`。
- `crates/atomcode-kernel/src/request.rs` — 复用，无需改（除非需暴露辅助）。
- `crates/atomcode-tuix/src/event_loop/mod.rs:14830` 附近 — 新 kind 分派臂 + 响应映射。
- `crates/atomcode-tuix/src/render/retained.rs:2729` — 复用 `build_user_input_rows`（可能抽薄封装）。
- `crates/atomcode-tuix/src/render/mod.rs:607` — checkpoint 面板视图状态（复用或轻量新建）。
- `crates/atomcode-coding/src/parts.rs:1332` — 装配处：TUI 路径开 `round_cap_checkpoint(true)`。
- `crates/atomcode-coding/src/assemble.rs:112` — 非 TUI 装配：保持 checkpoint 关。
- `crates/atomcode-coding/src/config.rs:384 default_turn_max_rounds` — 接 `[coding] max_rounds` TOML。
- `crates/atomcode-config/src/config/mod.rs` — 新增 `CodingConfig { max_rounds }` 段 + `save()` 注释。

## 后续 defer

- webui/daemon 的检查点镜像（本版 TUI-only）。
- 递增间隔（200→400→800…）作为恒定间隔的优雅升级。
