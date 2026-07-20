# Live Transport 收口方案

> 状态：LT0～LT5 已实施；live transport legacy 接口面达到状态④。
>
> 实施基线：`release/v5.0.1@97a21adb42ba69457cb6b7157f3681283e03a367`。
>
> 收口复核基线：`release/v5.0.1@a66b1433740f910a2b3b809201e248c35dd39d0f`。
>
> 当前四态：CLI、TUI、daemon 已使用 Coding Runtime 原生命令、事件和 snapshot；core `live`、
> `TurnEvent`、daemon 第二 runtime 和 TUI snapshot handoff 已删除，达到状态④。

## 1. 结论

不能把 `atomcode-core::live` 原样搬到其他 crate。当前 `LiveSession` 不只是 transport：它持有第二份
`Conversation`、turn 状态、取消令牌、审批/回答槽和回放缓冲；daemon 的 `KernelTurnExecutor` 又持有一套
持久 `CodingRuntime`。在嵌入 TUI 的 `/webui` / `/sync` 路径中，TUI foreground runtime 与 live runtime
并存，退出同步时再靠 snapshot handoff 合并。这正是需要消除的双 owner，而不是需要换目录保存的实现。

目标边界：

```text
Live View: TUI / WebUI / mobile
                │
                ▼
          Live View Hub
  fan-out / replay / correlated controls
                │ Runtime Binding
                ▼
          CodingRuntime
 conversation / turn / request / snapshot owner
```

- `CodingRuntime` 是 conversation、turn、provider、approval request、cancel、session binding 和 snapshot
  的唯一 owner；
- `Live View Hub` 只分发 native observation、维护尚未进入 committed snapshot 的 replay window，并把带
  correlation id 的控制请求路由给已绑定 runtime；
- headless daemon 可以作为 driver 创建一个 runtime 并绑定 hub；嵌入 TUI 时必须绑定现有 foreground
  runtime，不得再创建 `KernelTurnExecutor` runtime；
- 不创建新的大而全协议 crate。优先复用 kernel `AgentEvent/ToolBatchCall/SessionSnapshot` 和 coding
  `CodingRuntimeEvent/RuntimeRequest/DriverCommand`，Web SSE DTO 留在 daemon 边界；
- 完成标准已兑现：删除 core `LiveSession/TurnExecutor/TurnEvent` 及其生产依赖、重复转换和 TUI
  snapshot handoff fallback，不以“新 hub 已可用”冒充退役。

## 2. 实施结果

| 范围 | 当前 owner / 路径 | 结果 |
|---|---|---|
| core live | 已删除 `core/src/live` 和 `turn/event.rs` | 第二 conversation、turn guard 和孤儿 progress sender 已删除 |
| headless daemon | daemon driver 创建一个 `CodingRuntime` 并绑定 hub | 不再创建 `KernelTurnExecutor` 或 core conversation |
| TUI embedded | hub 绑定 foreground `RuntimeControl` | `/webui`、`/sync` 不再创建第二 runtime |
| 审批 / user input | hub 按 native request id 关联 | 错误 id、重复回答、generation 替换均 fail-closed |
| 多视图回放 | native committed snapshot + generation replay | session snapshot 未提交期间 join fail-closed |
| provider/mode/cd | 原生 reconfigure 命令 | UI 选择状态不再充当 runtime shadow owner |
| `/chat` | `CodingRuntimeEvent` → daemon projector → wire | 已删除 core `TurnEvent` 中间投影 |

当前生产消费者：

- daemon `/live` SSE、message、cancel、permission、user-input、provider、mode、cd、session switch；
- daemon `/chat` 的 `TurnEvent` 中间投影；
- TUI `/webui`、`/sync`、session switch、输入、cancel、审批、remote slash command 和 live forwarder；
- Web/mobile 的 `LiveWireEvent` 外部兼容面。

## 3. 不变量与失败语义

| 场景 | 必须保持的行为 |
|---|---|
| 未绑定 runtime | 输入、审批、回答、cancel 显式拒绝；不得创建空会话或假成功 |
| 输入被接受 | 由绑定的 runtime 返回可用性结果；hub 不维护第二个 busy 状态机，不静默丢输入 |
| runtime 替换 | binding 必须携带 generation/session/cwd；旧 generation 迟到事件不得进入新 replay |
| 审批 / 输入请求 | 以 runtime request id 关联；错误 id、重复回答、replace/cancel/shutdown 后回答均 fail-closed |
| cancel | 只路由到当前 binding 的活动 turn；终态仍来自 runtime 事件 |
| 晚加入 / lag | 先取 authoritative committed snapshot，再重放当前 generation 的 replay window，再接实时流 |
| session/cd/provider reload | 走 runtime 原生命令与 reconfigure 终态；失败时保留旧 binding 或显式失败 |
| headless | daemon 是 runtime driver，可以创建 runtime；不得同时保留第二个 core conversation owner |
| TUI 嵌入 | hub 复用 foreground runtime 的 control 和事件 tee；退出同步不再做跨 runtime snapshot 合并 |
| wire 兼容 | 现有 SSE `type` 与字段保持；仅 daemon projector 负责 native → wire 转换 |

## 4. 实施切片

### LT0：基线与 characterization（完成）

- 固定当前 `/live` wire shape、join/replay、busy、cancel、审批多请求、错误 request id、session switch 和
  lag 行为；
- 明确哪些现有行为是兼容契约，哪些是双 owner 的临时实现；
- 产物：测试矩阵和本方案，不改生产 owner。

完成门槛：每个后续切片都能指出保护它的现有或新增失败测试。

### LT1：中立事件类型去重（完成）

- TUI/daemon 的 tool-batch 展示统一使用 kernel `ToolBatchCall`，删除非 live 消费者对 core duplicate 的依赖；
- `/chat` 不再把 core `TurnEvent` 当成公共协议，逐步改为 daemon projector 或直接消费 native runtime event；
- 不改变 Web SSE 字段。

预计删除：TUI/daemon 的 core `ToolBatchCall` 引用、kernel → core tool-batch 复制；为 LT2 缩小
`TurnEvent` 的真实消费者。

### LT2：建立无执行权的 Live View Hub（完成）

- 在 daemon 内实现 hub：native committed snapshot、generation-scoped replay、broadcast、runtime control
  binding 和 pending request 索引；
- hub 不接收 `TurnExecutor`，不持有 core `Conversation`，不读 provider/config，不创建 runtime；
- 先用测试 runtime binding 覆盖 submit/respond/cancel、迟到事件、lag/rejoin 和 replace。

预计删除：core coordinator 的第二 conversation、turn guard、审批/回答 sender 槽和 cancellation token 语义；
在消费者切换前 core 实现仍保留，状态仍是③。

### LT3：headless daemon 切换（完成）

- daemon 创建的 `CodingRuntime` 直接绑定 hub；runtime 事件单一消费方同时更新 hub 与 daemon API；
- `/live` message/permission/user-input/cancel/provider/mode/cd 全部路由到 binding；
- 删除 `KernelTurnExecutor`、`NativeRuntimeState`、`LIVE_EXECUTOR` 和 live 专用 provider/cwd/mode runtime
  shadow state；配置变化使用 runtime reconfigure 命令及终态。

预计删除：daemon `TurnExecutor` 实现、live runtime 二次 snapshot 写回和 core conversation 投影。

### LT4：TUI foreground runtime 复用（完成）

- TUI runtime event forwarder增加 hub tee，`/webui` / `/sync` 将当前 `RuntimeControl` 与当前 generation/session
  绑定到 hub；
- Live View 输入、respond、cancel 和模式切换投递到同一个 foreground runtime；
- session switch、provider reload、cd、shutdown 时原子 replace/unbind；
- 删除 attach/detach 时跨 runtime snapshot restore、`IdleHandoff` 和 `PendingLocalRuntimeSync` fallback。

预计删除：TUI 第二 runtime、sync 专用 input/approval/cancel 分支和 snapshot handoff。

### LT5：core legacy 接口面退役（完成）

- 切换所有生产消费者与测试；
- 删除 core `live` 模块、`TurnExecutor`、`TurnEvent`、相关 `ToolContext.event_tx` 和无消费者依赖；
- 全仓搜索 core live/turn 引用，核对 daemon、TUI、headless、background、resume、approval、cancel、provider
  reload、session/cd 和 wire 兼容测试；
- 只有旧类型、调用点、fallback 和依赖全部删除后才声明状态④。

## 5. 验证矩阵

| 切片 | 最小测试 | 切片完成测试 |
|---|---|---|
| LT1 | daemon/TUI tool batch 与 wire projector | daemon、TUI 受影响测试 |
| LT2 | hub unit tests：binding、generation、replay、pending request | daemon lib tests |
| LT3 | `/live` API：message、cancel、approval、input、reload、headless | daemon 全 crate |
| LT4 | TUI sync：同 runtime、switch、detach、late event | TUI 全 crate + CLI all-targets |
| LT5 | 全仓符号/依赖搜索 | core、daemon、TUI、CLI 相关 workspace 检查 |

最终自动验证（2026-07-21）：

| 范围 | 结果 |
|---|---|
| core | 既有实施验证 1236 passed，1 ignored；本次未修改，不重复运行 |
| coding | 174 passed |
| daemon | 137 passed |
| TUI | 1406 passed；plugin target 1 passed |
| CLI | 82 passed |
| WebUI | 60 passed；TypeScript typecheck、production build 通过 |
| workspace | `cargo check --workspace --all-targets` 通过；仅保留既有 kernel liveness 测试 unused import 警告 |
| legacy 搜索 | 生产代码中无 `LiveSession/TurnExecutor/TurnEvent/core::live/live_sync` 引用 |

实际删除：core `live` 与 `turn/event` 模块、孤儿 `ToolContext.event_tx/current_call_id`、daemon
`KernelTurnExecutor/NativeRuntimeState/LIVE_EXECUTOR`、TUI `live_sync` 与 snapshot handoff 状态。
deferred runtime 事件保留 generation/sequence；Web 的 submit/respond/cancel/provider/mode/session/cd/reload
接口等待 Coding Runtime 真实终态，配置失败会回滚或显式报告。

## 6. 人工多端验收与收口修正

人工验收已在同一 TUI/WebUI runtime 上完成：

| 场景 | 结果 |
|---|---|
| WebUI → TUI、TUI → WebUI | 双向输入、响应和会话持久化一致 |
| approval | WebUI 与 TUI 同时显示请求；WebUI 批准后命令执行成功 |
| cancel | WebUI 停止后回到 idle；TUI 收到明确的 `cancelled` 终态 |
| session resume | WebUI 恢复旧会话后，TUI 同步显示相同会话与消息 |
| `/cd` | WebUI 发起目录切换后，TUI 与 runtime cwd 同步；可恢复原目录 |
| provider reload | provider/model 保持同一 registry identity；reload 后可立即继续输入 |

验收发现并修正的缺口：

| 缺口 | 修正结果 |
|---|---|
| TUI/WebUI 初始 provider、mode 可能不一致 | live binding 后从 foreground runtime 显式同步 provider 和 mode；CLI provider override 成为进程内默认值 |
| 请求当前 provider 仍触发 reload | 相同 provider 不再重建 runtime，避免无意义 generation 变化 |
| reload 完成与下一条命令存在 generation 竞态 | reconfigure 成功后同步提交新 generation，再接受后续命令 |
| runtime 事件把 adapter 类型误当 provider 名称 | 配置中保留稳定的 registry provider key，事件不再回退到错误模型 |
| 单个 orphan sidecar 阻断全部 session catalog | 扫描诊断显式告警；有效会话继续可用，直接访问损坏会话仍显式失败 |
| WebUI collection API 把错误 JSON 当数组 | 检查 HTTP 状态和数组结构，错误显式抛出，不再导致页面运行时崩溃 |

本次修正未恢复任何 core/bridge legacy variant、handler、依赖或 fallback；live transport 仍保持状态④。
历史 orphan sidecar 未被删除或改写，只作为可见诊断保留。

## 7. 当前唯一下一步

推送并评审本次收口提交。live transport 收敛方案本身已无待开发切片；历史损坏会话文件的清理属于独立的
数据维护任务，不作为迁移完成前置。
