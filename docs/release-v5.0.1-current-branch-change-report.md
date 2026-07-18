# release/v5.0.1 当前分支改动与测试报告

> 报告日期：2026-07-18
>
> 对比分支：`release/v5.0.1_1` vs `origin/release/v5.0.1`
>
> 当前提交：`cfe4209c2779504e3ee80c523da042e2429f1ba4`
>
> 远端基准：`9fcc92d9a144b7f97b8375f235cfb9a96b0f4da5`

## 1. 结论

当前分支已完整包含最新 `release/v5.0.1`，提交关系为 **0 behind / 5 ahead**；报告基线采集时
工作区干净，本报告生成后仅新增本文档。

本分支的核心修改是彻底退役 `core/bridge` 旧引擎调用链，将 CLI、TUI、daemon、background、ACP
和 clix 统一到 kernel 原生命令、事件及 `CodingRuntime` 生命周期边界。最新分支新增的
`request_user_input` 已通过 native `CodingRuntimeEvent::Request / DriverCommand::Respond` 接入，
没有恢复已删除的 bridge 或 core legacy driver 协议。

按项目的四态口径，当前达到：

1. 新逻辑已实现；
2. driver 已切换；
3. 无 legacy fallback；
4. legacy 调用点、协议、handler、依赖和 bridge crate 已删除。

因此，**引擎 driver 调用链达到“legacy 接口面已退役”状态**。但最终树尚未完成所有受影响 crate
的完整测试和真实 provider 跨入口 smoke，当前测试证据足以支持合并后的编译与定向行为判断，
不足以替代发布前全量验证。

## 2. 对比基线与规模

| 项目 | 结果 |
|---|---|
| 当前分支 | `release/v5.0.1_1@cfe4209c` |
| 最新远端 | `origin/release/v5.0.1@9fcc92d9` |
| merge-base | `9fcc92d9` |
| 分叉关系 | 远端独有 0 个提交，当前独有 5 个提交 |
| 工作区 | 基线采集时 clean；生成报告后仅新增本文档 |
| 当前分支净改动 | 115 文件，`+12,860 / -21,905` |

当前独有提交：

| 提交 | 内容 |
|---|---|
| `59d4c284` | `refactor(runtime): retire legacy engine bridge` |
| `81985e9c` | `fix(auth): restore codingplan crypto feature` |
| `244fba39` | 合并早期 `release/v5.0.1` |
| `1e923fb7` | 合并 `request_user_input`、TUI、VSCode 等最新改动，并完成 native 协议适配 |
| `cfe4209c` | 合并 `request_user_input` 新版单选、多选和自由文本交互 |

其中真正的本地功能变更集中在前两个提交；后三个提交负责持续同步远端并解决新功能与 runtime
退役之间的协议冲突。

## 3. 当前修改内容

### 3.1 Runtime 与协议退役

旧链路：

```text
driver
  -> core AgentCommand
  -> atomcode-bridge / daemon adapter
  -> kernel
  -> core AgentEvent
  -> driver
```

新链路：

```text
CLI / TUI / daemon / background / ACP / clix
  -> CodingRuntimeHandle / DriverCommand
  -> CodingRuntime
  -> kernel AgentCommand / AgentEvent
  -> CodingRuntimeEvent
  -> driver-local projection
```

实际删除项：

- 整个 `atomcode-bridge` crate、Cargo 依赖和 lockfile 记录；
- core `agent` driver 协议及 goal、loop、compression、parallel-edit legacy 实现；
- core v1 `TurnRunner`、permission、loop guard、tool args、datalog、log 旧执行链；
- CLI、TUI、daemon、background 对 bridge/core legacy command/event 的发送和消费；
- daemon 重复 kernel driver、双引擎入口及 mixed legacy/native event；
- TUI legacy/native 双 endpoint。

静态搜索确认生产代码中不存在：

```text
atomcode_bridge
spawn_bridged_runtime_with_control
BridgedRuntime
BridgeConfig
atomcode_core::agent::AgentClient
atomcode_core::agent::AgentCommand
atomcode_core::agent::AgentEvent
ATOMCODE_DAEMON_ENGINE
DaemonRuntimeEvent::Legacy
DaemonRuntimeEvent::Native
```

### 3.2 生命周期统一

`CodingRuntime` 成为唯一 runtime owner，统一管理：

- provider 构建、reload 和 reassemble；
- fresh、resume、restore、undo、change directory；
- session id、working directory、snapshot 和 generation；
- submit、steer、cancel、approval 和 shutdown；
- compact exactly-once terminal；
- goal/self-paced loop 的互斥、held turn、wakeup 和 terminal；
- background、daemon、TUI 和 headless 的统一事件来源。

主要不变量：

- 一个 `CodingParts` 不产生两个 live agent；
- lifecycle 操作由 actor 串行化；
- provider/session 切换失败不得静默回退 fresh session；
- pending request 在 cancel、reload、session switch 和 shutdown 时 fail-closed；
- stale generation 事件不得进入 replacement runtime；
- snapshot、provider、working directory、approval grants 和 gateway affinity 不因 reassemble 丢失。

### 3.3 Daemon 与 driver

Daemon 改为使用统一 `CodingRuntime`：

- 删除重复的 `KernelDriver` 生命周期实现；
- 增加 native runtime host；
- `legacy_convert.rs` 仅用于历史 session/snapshot 数据转换，不能投递旧 engine command；
- HTTP、SSE、WebSocket 只负责 native runtime event 的传输映射；
- `/chat` 文本模型图片输入复用 VL 预处理策略，原图继续用于持久化和缩略图，kernel 输入使用 VL 描述。

CLI/headless、clix 和 daemon 对无法交互处理的非 approval request 统一返回 `Null`，避免工具回合永久挂起。

### 3.4 request_user_input 合并适配

最新 `release/v5.0.1` 新增了 `request_user_input`。本分支保留其产品行为，但将协议接入点迁到
native runtime：

- kernel 增加 `Requester` 和 `ToolContext.request`；
- capability 注册 `request_user_input`，默认关闭，由 `ATOMCODE_REQUEST_USER_INPUT` 控制；
- 工具名始终进入 coding allowlist，实际 mount 仍以环境开关和注册结果为准；
- persona 提示与工具开关保持一致；
- runtime 关联 request id，并在 cancel/reload/shutdown 时 fail-closed；
- TUI 支持 single、multiple、text、自定义文本、Esc、Ctrl+C 和 bypass 自动跳过；
- single 模式在选项行按 Enter 立即提交，不显示单独 Submit 行；
- multiple 模式保留 Submit 行，并可附加自由文本；
- CLI/headless、clix 和 daemon 对非交互 request 返回 `Null`。

此合并没有恢复远端曾新增的 core `AgentEvent::Request / AgentCommand::Respond` bridge passthrough，
旧协议删除状态保持不变。

### 3.5 AtomGit/GitCode 网关认证

请求签名逻辑从 bridge/core 下沉到 `atomcode-auth` 和 capabilities provider：

- 网关识别按 HTTPS scheme 和精确 host 判断；
- OAuth token、user id、timestamp 和随机 nonce 统一参与签名；
- 源码构建暴露 unavailable signer，不伪造签名成功；
- 官方构建通过 `atomcode-core/codingplan-crypto -> atomcode-auth/codingplan-crypto` 启用闭源 overlay；
- `81985e9c` 恢复了迁移中遗漏的 feature 声明和依赖透传。

### 3.6 TUI 与 VSCode

TUI：

- 使用 runtime control 和 TUI-local `UiEvent`；
- 新增结构化用户输入面板；
- `/usage` 按显示宽度处理 CJK 对齐；
- foreground/background、approval、mode、session replay 和 undo 继续使用统一 runtime。

VSCode：

- chat font family 和输入框字体行为调整；
- provider queue 在无 focused panel 时仍按 session 正确排队和出队；
- approval mode pending 状态不会被 initial state 覆盖；
- session 展示继续按当前 workspace/project 过滤。

## 4. 影响范围

| 影响面 | 风险 | 说明 |
|---|---|---|
| Runtime 生命周期 | 高 | provider、session、snapshot、cancel、shutdown 和 generation 全部跨 crate 改动 |
| Driver parity | 高 | CLI、TUI、daemon、background、ACP、clix 同时切换协议 |
| Approval/request | 高 | request id 关联或 fail-close 出错会导致越权、误拒绝或回合挂起 |
| Goal/loop | 高 | 与 held turn、snapshot、continuation 和 wakeup 同生命周期 |
| Daemon transport | 中 | HTTP/SSE/WebSocket 事件映射改变，但不再持有第二套 runtime |
| Provider/auth | 中 | 网关签名位置和 feature 透传改变 |
| TUI/VSCode UI | 中 | 输入面板、队列、审批状态和渲染路径变化 |
| 构建依赖 | 中 | bridge crate 删除、core/coding/capabilities 依赖边界变化 |
| 文档 | 低 | 部分旧 README 架构描述尚未同步 |

## 5. 已知问题与审计结论

### 5.1 文档静态验收项不完全一致

迁移设计文档把 `KernelRuntimeAdapter` 列入最终静态零命中项，但当前
`atomcode-coding/src/runtime.rs` 仍保留同名内部类型。该类型服务于 kernel agent 管理和 compaction，
不依赖 bridge，也不能投递 core legacy command，因此不构成 legacy fallback；但文档的字面验收标准
与代码不一致，应修正文档或重命名该内部类型。

### 5.2 README 架构描述陈旧

中英文 README 仍描述 core `AgentLoop` 通过 `AgentCommand / AgentEvent` 与 TUI 通信，和当前
`CodingRuntime` 架构不一致。这不影响运行时行为，但会误导后续维护者。

### 5.3 删除旧测试后的覆盖迁移

bridge stream timeout、core turn runner、hook integration 等旧链路测试随实现删除是合理的；对应行为
必须由 kernel、capabilities、CodingRuntime 和各 driver parity 测试继续覆盖。不能只依据删除代码量判断
退役完成。

## 6. 测试依据

### 6.1 最终合并树 `cfe4209c`

| 命令 | 结果 | 覆盖 |
|---|---|---|
| `cargo check -p atomcode -p atomcode-daemon -p atomcode-clix --all-targets` | 通过 | CLI、daemon、clix 及依赖链 all-target 编译 |
| `cargo test -p atomcode-capabilities -p atomcode-kernel -p atomcode-coding -p atomcode-tuix request_user_input` | 通过 | capability 10 项、persona 1 项；其余同名过滤项无失败 |
| `cargo test -p atomcode-tuix user_input` | 19/19 通过 | single/multiple/text、自定义文本、Submit、Esc、Ctrl+C、bypass、render |
| `npm run test:webview` | 通过 | webview test runner 全部通过；可见 31 项 node:test 断言通过，并包含静默 provider queue regression |

编译期间存在一个既有 warning：`atomcode-kernel/tests/liveness.rs` 的 `SilentStreamProvider` unused import。
该 warning 不影响本次验证结果，但应在后续维护中清理。

### 6.2 迁移提交父树的补充测试证据

在合入最新版 `request_user_input` 前，曾从干净 `HEAD` 快照执行更广范围测试：

| 范围 | 结果 |
|---|---|
| CLI | 50 个 lib、25 个 main、12 个 integration 测试通过 |
| atomcode-auth | 33 项通过 |
| atomcode-capabilities | 748 项通过；唯一 askpass Unix socket 用例因沙箱权限失败，沙箱外单独复核通过 |
| atomcode-coding | 161 个单测通过 |
| coding integration | assemble、cache prefix、full assembly、overflow recovery、plan mode、sensitive path 通过 |
| permission grants | 在仓库工作区单独复核通过 |
| runtime request | request 关联及 shutdown fail-close 定向测试通过 |
| kernel requester | Request/Respond round-trip 定向测试通过 |

上述补充结果证明迁移主体已有较广测试基础，但它们不是最终 `cfe4209c` 全量测试的替代品。

### 6.3 未完成验证

- 最终合并树尚未运行所有受影响 crate 的完整 `cargo test --all-targets`；
- 尚未完成真实 provider 的 CLI、TUI、daemon 跨入口 smoke；
- 尚未在真实交互中覆盖 approval、cancel、resume、provider reload、图片 VL 预处理和
  `request_user_input` 全模式组合；
- 未执行发布构建中的闭源 `codingplan-crypto` overlay 验证。

## 7. 迁移状态

| 判定项 | 当前状态 |
|---|---|
| 逻辑已实现 | 是 |
| driver 已切换 | 是 |
| legacy fallback 仍可达 | 否 |
| legacy 接口面已退役 | 是 |
| 最新 `release/v5.0.1` 已合入 | 是，0 behind |
| 原代码工作区可提交 | 是；当前仅新增本文档 |
| 发布级全量测试完成 | 否 |

## 8. 唯一下一步

在磁盘空间充足的环境中，对最终 `cfe4209c` 运行受影响 crate 的完整 all-target 测试，然后使用真实
provider 做一次 CLI、TUI、daemon 跨入口 smoke，覆盖 turn、approval、cancel、resume、provider
reload、图片输入和 `request_user_input`；只有这一步完成后，才能把当前结论提升为发布级验证完成。
