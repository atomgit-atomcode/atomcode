# CodingRuntime 原生驱动全量迁移设计与实施记录

> 状态：已实施；旧调用链已退役（四态中的 ④）
>
> 实施起始核对基线：`release/v5.0.1@78a83206b8284218f168132b56681cbe6dd18180`
>
> 最终复核基线：`release/v5.0.1@bb05506584e88f7c7d2734fef500d249d234d7bc`
>
> 实施日期：`2026-07-17`
>
> 关联文档：
>
> - [`compact-native-migration-retrospective.md`](compact-native-migration-retrospective.md)
> - [`coding-runtime-incremental-migration.md`](coding-runtime-incremental-migration.md)
> - [`target-architecture.md`](target-architecture.md)
>
> 本文实施结论以最终复核基线为准；在后续分支引用时仍须重新搜索当前代码。旧文档中
> “55 个命令”“v1 AgentLoop 仍存在”“memory/background 仍有 core command”等历史结论
> 已经变化，不作为实施基线。

## 0. 实施结果

迁移已经达到“legacy 接口面已退役”，不是只完成新逻辑或切换默认路径。CLI、TUI、daemon、
background、ACP 和 clix 均由 `CodingRuntime` 持有 kernel agent 生命周期；生产代码中已无 core
`AgentCommand/AgentEvent/AgentClient`、bridge handler、daemon 双引擎开关或 bridge fallback。

### 0.1 新老链路对比

| 维度 | 旧链路 | 新链路 |
|---|---|---|
| 命令入口 | driver → core `AgentCommand` → bridge/daemon adapter → kernel | driver → `CodingRuntimeHandle` / `DriverCommand` → `CodingRuntime` → kernel 原生命令 |
| 事件出口 | kernel → bridge/core `AgentEvent` → driver | kernel `AgentEvent` → `CodingRuntimeEvent` → driver 本地展示协议 |
| 生命周期 owner | bridge 与 daemon 各自持有 config、provider、session、agent | `CodingRuntime` 单一 owner |
| provider/session/cd | adapter 重建 runtime，存在两套实现 | runtime actor 串行执行生命周期事务 |
| approval | core snapshot + 无统一 request 归属 | request id 关联，取消/重建/退出时 fail-closed |
| goal/loop | bridge 持有 Snapshot/continuation 状态机 | runtime controller 与 turn terminal、wakeup 同一生命周期 |
| fallback | v1/v2、bridge/native 可并行可达 | 无旧协议、无双 endpoint、无运行时切换开关 |

### 0.2 命令簇归属与删除结果

| 命令簇 | 新 owner | 已删除的旧接口面 |
|---|---|---|
| submit/steer/cancel/approval/compact | `CodingRuntime` + kernel 原生命令 | core turn command/event、bridge handler/converter |
| model/provider/reload | runtime provider factory/reassemble | bridge/daemon 重复 provider builder 与 respawn 路径 |
| fresh/resume/restore/undo/cd | runtime session lifecycle | core session command variants、双路径 snapshot 转换 |
| context/hooks/local shell/memory | runtime capability 或 driver local service | 不必要的运行中 engine 投递协议 |
| goal/self-paced loop | runtime controller | core/bridge goal、evaluation、loop 状态机 |
| background/live/daemon transport | 各 driver 的本地展示/传输层 | mixed legacy/native event、daemon `KernelDriver` |
| shutdown | runtime actor + kernel shutdown | bridge shutdown/fallback 与多 owner teardown |

### 0.3 实际删除

- 整个 `atomcode-bridge` crate、workspace 依赖和 lockfile 记录；
- core `agent` driver 协议及 goal/loop/compression/parallel-edit legacy 实现；
- core v1 `TurnRunner`、permission/loop guard/tool args/datalog/log 旧执行链；
- daemon `KernelToWebui`、`DaemonRuntimeEvent`、重复 kernel driver 和双路径入口；
- CLI、TUI、daemon、background 对 bridge/core legacy 类型的发送、消费和依赖；
- TUI 双 endpoint，改为 runtime control + TUI-local `UiEvent` 投影。

本阶段当时保留的 core live/session surface 已在后续
[`session-convergence-plan.md`](session-convergence-plan.md) 与
[`live-transport-convergence-plan.md`](live-transport-convergence-plan.md) 中完成退役。daemon 的
`legacy_convert.rs` 只处理历史 session/snapshot 数据兼容，不能投递旧 engine 命令。

### 0.4 daemon `/chat` 图片 parity 修复

文档收口时发现 daemon `/chat` 的文本模型路径没有像 `/live` 和 TUI 一样调用配置的 VL
预处理器。已先补端到端失败测试，再在 daemon/native host 边界复用统一预处理策略：原图继续
用于会话持久化和缩略图，kernel 输入只携带 VL 描述；没有恢复 bridge 或旧命令链。

后续章节保留迁移前的设计推导与验收约束，作为实现审计记录；其中描述“当前仍未迁移”的段落
应按本节的实施结果理解。

## 1. 结论

最终只保留一个运行时所有者：

```text
CLI / TUI / daemon / background
          │
          ▼
CodingRuntimeHandle
          │
          ▼
CodingRuntime owner
  - CodingAgentConfig
  - CodingParts
  - Provider
  - SessionBinding
  - current AgentHandle
  - generation / state
  - pending requests / snapshot broker
          │
          ▼
atomcode-kernel Agent
```

driver 不再发送 core `AgentCommand`，不再消费 core `AgentEvent`。daemon 不保留自己的
`KernelDriver` 生命周期实现；它只负责把统一 runtime event 映射到 HTTP/SSE/WebSocket
协议。

迁移完成的删除目标是：

- `atomcode-bridge` crate；
- core `AgentClient/AgentCommand/AgentEvent` driver 协议；
- daemon `KernelDriver/KernelToWebui` 重复实现；
- `ATOMCODE_DAEMON_ENGINE` 双路径开关；
- TUI `RuntimeEndpoint { legacy, native }` 双控制面；
- daemon `DaemonRuntimeEvent::{Legacy, Native}` 混合事件流；
- live runtime 中的 core ↔ kernel command/event/message 转换；
- CLI、TUI、daemon 对 `atomcode-bridge` 的依赖。

删除整个 `atomcode-core` crate 还需要继续外迁 plugin、live、旧 session 导入等非引擎模块。
这属于相邻清理，不能与“driver 调用链已退役”混为同一个完成口径。

## 2. 当前事实与校正

### 2.1 当前不是两套引擎

core v1 `AgentLoop` 已删除。当前并行的是两条 driver/runtime 调用链：

```text
默认：driver -> core protocol -> bridge -> kernel

daemon opt-in：daemon -> core protocol -> daemon KernelDriver -> kernel
```

daemon opt-in 路径虽然绕过 bridge，仍然接收 core command、产生 core event，并复制了
provider reload、respawn、session restore、事件聚合和失败处理，所以不能作为最终架构继续扩展。

### 2.2 `/compact` 已完成垂直退役

`/compact` 当前走：

```text
CodingRuntimeHandle::compact
  -> CodingRuntime owner
  -> kernel AgentCommand::Compact
  -> CodingRuntimeEvent::CompactionStarted/Finished
```

它证明了以下迁移方法可行：

1. 单一 `AgentHandle` owner；
2. stable handle + generation；
3. owner 按事件类型截流，未迁移事件暂交 legacy adapter；
4. replace/shutdown 时排空旧 agent terminal；
5. 完整 terminal 后删除旧 sender、variant、handler、converter 和 fallback。

### 2.3 当前 CodingRuntime 仍只是局部 owner

当前 `CodingRuntimeHandle` 只有 `compact`；`CodingRuntime` owner 只处理 AgentHandle replace、
shutdown 和 compaction terminal。以下状态仍由 bridge 或 daemon 自己持有：

- `CodingAgentConfig` 和 `CodingParts`；
- provider 构建与 tier provider；
- provider reload/reassemble；
- prepare/reprepare/fresh/resume/cd；
- approval request mirror；
- turn stats 与 terminal Snapshot 聚合；
- goal/loop controller；
- AI session naming、local shell pending input 等 driver parity 状态。

因此下一步不是给 handle 批量增加转发方法，而是先让 `atomcode-coding` 成为完整 runtime owner。

### 2.4 atomcode-coding 尚未 core-free

当前仍有三类直接 core 依赖：

- `assemble.rs` / `parts.rs` 的 vision model 判定；
- `rate_limit.rs` 的 CodingPlan window 类型和 client；
- bridge 解析 plugin hooks 后注入 coding 的过渡关系。

详细迁移前置：

1. vision 判定统一使用 `atomcode-capabilities::provider::model_suggests_vision`；
2. CodingPlan 限流拆成中立决策与可注入 `RateLimitWindowSource`；
3. plugin hook 通过可重载 source 注入，不让 coding 反向读取 core plugin 系统。

## 3. 范围与非目标

### 3.1 本方案范围

- CLI/headless、TUI/background、daemon/webui 的运行时命令和事件；
- provider、session、working directory、mode、approval、snapshot、cancel、shutdown；
- MCP/plugin reprepare；
- goal 和 self-paced loop；
- `/compact` 已有 native control/event 的兼容与保留；
- 删除 bridge 和 core legacy driver surface。

### 3.2 非目标

- 不重做 compaction 算法；
- 不改变版本号、发布策略；
- 不把 `/model`、`/resume`、`/cd` 等生命周期操作加入 kernel `AgentCommand`；
- 不重写 TUI 渲染；
- 不顺带优化产品功能；
- 不在本迁移中承诺多进程同时写同一 session；
- 不以“把 bridge 代码原样搬进 coding”作为完成。

## 4. 架构边界与不变量

### 4.1 kernel 原生命令保持精简

kernel 继续只拥有：

```text
SendMessage
Respond
Snapshot
Compact
Cancel
Shutdown
```

它们只操作当前运行中的 Agent，不知道 config、provider key、项目、session picker、plugin 或
goal 产品语义。

### 4.2 CodingRuntime 生命周期操作

`CodingRuntime` 拥有：

```text
start
submit / respond / snapshot / compact / cancel / shutdown
set_mode
reassemble_provider
reprepare_capabilities
fresh_session
resume_session
change_directory
undo_to_prompt
start/stop goal
start/stop self-paced loop
```

### 4.3 driver/local 操作

driver 继续拥有：

- slash 字符串解析、modal 和渲染；
- OAuth/auth 文件读写；
- `!cmd` 的 shell 进程执行；
- updater、clipboard、file view/save；
- memory CRUD；
- fixed-interval `/loop` 的 slash 调度器；
- daemon wire protocol。

driver 可以把本地操作的结果作为结构化输入交给 runtime，但不能直接修改 runtime 持有的
conversation、provider 或 session 状态。

### 4.4 硬不变量

1. 同一个 `CodingParts` 最多一个 live `AgentHandle`；
2. 一个 kernel event receiver 只有 `CodingRuntime` 一个消费者；
3. 所有 lifecycle mutation 在一个 actor 中串行执行；
4. 每个 accepted operation 属于唯一 generation；
5. replacement generation 不接收旧 generation 的请求；
6. driver 只消费一个有序 runtime event channel；
7. session/provider/cd/undo 切换成功前，不发布成功事件；
8. 失败不能伪装成空 snapshot、no-op 或 fresh session；
9. approval 在 cancel/reconfigure/shutdown/driver 消失时 fail-closed；
10. native session snapshot 是运行时权威数据，core JSON 只能作为迁移期 mirror/import source。

## 5. Runtime 对外 API

以下是目标职责接口，不要求第一提交一次实现全部方法。公开 API 使用能力方法，不公开一个复制
core variant 的大 `RuntimeCommand` 枚举。

```rust
pub struct CodingRuntime {
    pub handle: CodingRuntimeHandle,
    pub events: CodingRuntimeEvents,
    pub task: tokio::task::JoinHandle<RuntimeExit>,
}

pub type CodingRuntimeEvents =
    tokio::sync::mpsc::UnboundedReceiver<SequencedRuntimeEvent>;

pub struct CodingRuntimeStart {
    pub agent: CodingAgentConfig,
    pub prepare: PrepareOptions,
    pub provider_factory: Arc<dyn CodingProviderFactory>,
    pub plugin_hooks: Arc<dyn PluginHookSource>,
}

impl CodingRuntime {
    pub async fn start(input: CodingRuntimeStart)
        -> Result<Self, RuntimeStartError>;
}
```

启动失败直接返回错误，不创建 `noop_handle`，也不返回看似可用但会丢命令的 degraded runtime。
daemon 或 TUI 若需要重试，应显式再次调用 `start`。

目标 handle：

```rust
impl CodingRuntimeHandle {
    pub async fn submit(&self, input: UserInput)
        -> Result<SubmitReceipt, RuntimeError>;
    pub async fn respond(&self, id: RequestId, value: serde_json::Value)
        -> Result<(), RuntimeError>;
    pub async fn snapshot(&self)
        -> Result<Arc<SessionSnapshot>, RuntimeError>;
    pub async fn compact(&self, focus: Option<String>)
        -> Result<CompactReceipt, RuntimeError>;
    pub async fn cancel(&self)
        -> Result<(), RuntimeError>;
    pub async fn shutdown(&self)
        -> Result<RuntimeExit, RuntimeError>;

    pub async fn set_mode(&self, mode: RuntimeMode)
        -> Result<(), RuntimeError>;
    pub async fn context_stats(&self)
        -> Result<RuntimeContextStats, RuntimeError>;
    pub async fn queue_local_context(&self, input: LocalContextInput)
        -> Result<(), RuntimeError>;
    pub async fn reassemble_provider(&self, next: CodingAgentConfig)
        -> Result<RuntimeGeneration, RuntimeError>;
    pub async fn reprepare(&self, next: ReprepareInput)
        -> Result<RuntimeGeneration, RuntimeError>;
    pub async fn fresh_session(&self, next: FreshSessionInput)
        -> Result<SessionChanged, RuntimeError>;
    pub async fn resume_session(&self, id: String)
        -> Result<SessionChanged, RuntimeError>;
    pub async fn change_directory(&self, dir: PathBuf)
        -> Result<SessionChanged, RuntimeError>;
    pub async fn undo_to_prompt(&self, nth: Option<usize>)
        -> Result<UndoResult, RuntimeError>;
    pub async fn start_goal(&self, condition: String)
        -> Result<ControllerId, RuntimeError>;
    pub async fn stop_goal(&self)
        -> Result<(), RuntimeError>;
    pub async fn start_self_paced_loop(&self, prompt: String)
        -> Result<ControllerId, RuntimeError>;
    pub async fn stop_self_paced_loop(&self)
        -> Result<(), RuntimeError>;
}
```

`RuntimeMode` 是 coding/runtime 的 core-free 类型，穷举四种现有语义：

```rust
pub enum RuntimeMode {
    Build,
    AcceptEdits,
    Auto,
    Plan,
}
```

daemon wire mode、TUI `AgentMode` 和 config 值都在 driver 边界显式映射；不得把 core `Mode` 留作
runtime 的隐藏依赖。

`submit` 的 receipt 必须区分：

```rust
pub enum SubmitReceipt {
    Started { generation: u64, turn_id: u64 },
    Steered { generation: u64, turn_id: u64 },
}
```

第二个输入在运行中的 turn 内进入 kernel steer buffer，不产生独立 `TurnFinished`。

### 5.1 Provider factory

runtime 必须持有可重复构建 provider 的 factory，而不是只接收一次性 provider：

```rust
pub trait CodingProviderFactory: Send + Sync {
    fn build(
        &self,
        config: &CodingAgentConfig,
        session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError>;

    fn refresh_subagent_tiers(
        &self,
        config: &mut CodingAgentConfig,
        session_id: Option<&str>,
    );
}
```

bridge 当前的 OpenAI/Claude/Ollama 选择、UA、TLS、reasoning、AtomGit signing 和 tier provider
逻辑必须迁入一个共享实现。gateway signing 所需的低层能力应下沉到 auth/atomgit capability，
不能让 `atomcode-coding` 反向依赖 bridge。

### 5.2 Plugin hook source

`reprepare` 必须重新读取最新 plugin hooks，不能捕获启动时的静态 `Vec<HookConfig>`：

```rust
pub trait PluginHookSource: Send + Sync {
    fn load(&self) -> Result<Vec<HookConfig>, PluginHookLoadError>;
}
```

迁移期由 CLI/daemon 注入基于现有 plugin loader 的实现；plugin 模块外迁后替换实现，runtime API
不变。

## 6. Runtime 状态机

```rust
pub enum RuntimePhase {
    Ready,
    InTurn { turn_id: u64 },
    WaitingApproval { turn_id: u64, request_id: RequestId },
    Reconfiguring { operation: ReconfigureKind },
    ShuttingDown,
    Stopped,
    Failed,
}
```

`start` 成功前没有可用 handle，因此 `Starting` 不作为可接收命令的公开状态。

| 操作 | Ready | InTurn | WaitingApproval | Reconfiguring | ShuttingDown/Stopped/Failed |
|---|---|---|---|---|---|
| submit | 开新 turn | steer 当前 turn | steer，待审批后折入 | 拒绝 Busy | 拒绝 Unavailable |
| respond | stale | stale | 仅匹配 id 可用 | stale | stale |
| snapshot | 立即请求 | 排队到 turn terminal | 排队，不用于审批即时持久化 | 拒绝 Busy | 拒绝 |
| compact | 执行 | kernel 排队到边界 | 排队 | 返回 Interrupted | 拒绝 |
| cancel | no-op success | cancel | fail-closed + cancel | 不打断事务 | no-op/拒绝 |
| set_mode | 立即 | 立即影响后续工具 | 不追溯改变当前审批 | 拒绝 Busy | 拒绝 |
| reassemble/reprepare | 执行 | 先 settle 当前 turn | fail-closed 后 settle | 拒绝 Busy | 拒绝 |
| fresh/resume/cd/undo | 执行 | 先 settle 当前 turn | fail-closed 后 settle | 拒绝 Busy | 拒绝 |
| shutdown | 关闭 | cancel + settle + 关闭 | fail-closed + settle + 关闭 | 登记关闭，事务到安全边界后执行 | 幂等返回同一结果 |

runtime actor 使用单一 control receiver 和单一 kernel event receiver。状态变化只在 actor 内发生；
handle 上的原子/watch 状态只用于快速拒绝，不能成为第二状态所有者。

重配置进入 commit 段后不能被 shutdown 从中间 abort，否则可能同时丢失 old 和 candidate。shutdown
在 `Reconfiguring` 中只线性化为“下一个最高优先级操作”：candidate 尚未 commit 时先回滚/丢弃，已经
commit 时先完成新 generation 发布，再立即关闭。调用方等待同一个 shutdown terminal，不观察半提交
状态。

## 7. generation、sequence 与事件协议

现有 `CodingRuntimeEvent` 继续作为 driver-neutral event kind 扩展；所有事件通过一个新 envelope
发布，避免一次性把现有 compact consumer 改成另一套同名类型：

```rust
pub struct SequencedRuntimeEvent {
    pub generation: u64,
    pub sequence: u64,
    pub event: CodingRuntimeEvent,
}
```

- `generation` 在 Agent replacement 线性化时递增；
- `sequence` 在整个 runtime 生命周期单调递增；
- replacement 成功事件发布后，不再发布旧 generation 的普通 display/tool/request 事件；
- 旧 generation 尚未完成的 accepted operation，只能再发布对应的 Interrupted/Cancelled terminal。

目标 event kind：

```rust
pub enum CodingRuntimeEvent {
    Agent(AgentEvent),
    Request(RuntimeRequest),
    TurnFinished(TurnCompletion),
    CompactionStarted { trigger: CompactTrigger },
    CompactionFinished { completion: CompactionCompletion },
    ModeChanged { mode: RuntimeMode },
    ProviderChanged { provider: String, model: String },
    SessionChanged(SessionChanged),
    WorkingDirectoryChanged(PathBuf),
    GoalUpdate(GoalProgress),
    LoopUpdate(LoopProgress),
    Reconfiguring { operation: ReconfigureKind },
    Reconfigured { operation: ReconfigureKind },
    RuntimeStopped(RuntimeExit),
    RuntimeFailed(RuntimeFailure),
}
```

`Agent(AgentEvent)` 只转发 kernel 的观察事件：TurnStarted、text/reasoning、tool streaming/tool
batch/tool progress/tool result、usage、warning、rate-limit、steered、error、cancelled。runtime 用
穷举 match 截获下列事件；它们不得出现在 `Agent(...)` 中。这样复用 kernel 中立协议，而不是重新
复制一个字段基本相同的 core `AgentEvent`。

runtime 截获而不直接外泄：

- kernel `Snapshot`；
- kernel `TurnComplete`；
- kernel compaction started/terminal；
- runtime 已识别的 approval request。

### 7.1 Turn terminal

kernel `TurnComplete` 到达后：

1. runtime 保存 stop reason；
2. 发送一次 kernel `Snapshot`；
3. Snapshot 到达后发布一次 `TurnFinished`；
4. 清理 pending request、turn stats 和 snapshot broker；
5. 状态回到 Ready，或交给 goal/loop controller 决定继续。

terminal 不能携带伪造的空 conversation：

```rust
pub enum TurnCompletion {
    Completed {
        reason: StopReason,
        snapshot: Arc<SessionSnapshot>,
        stats: RuntimeTurnStats,
    },
    SnapshotUnavailable {
        reason: StopReason,
        error: RuntimeSnapshotError,
        stats: RuntimeTurnStats,
    },
}
```

持久 session 的 fallback 只能显式读取 `SnapshotHook` 在 `turn_complete` 已写入的 canonical
snapshot，并记录来源；不得用 `SessionSnapshot::default()` 假装成功。ephemeral runtime 无 snapshot
时发布 `SnapshotUnavailable`，driver 必须释放 busy 状态并显示持久化失败。

### 7.2 Snapshot broker

kernel Snapshot 没有 request id，且 turn 中的 Snapshot 会排队到 turn 结束。runtime 必须保证同一
时间只有一个 kernel Snapshot 在途，并在内部标记用途：

```text
TurnTerminal
ExplicitQuery(waiters)
Undo
Reconfiguration
```

多个显式查询可以共享同一个结果。terminal snapshot 优先；状态不能靠“下一个 Snapshot 大概属于
谁”猜测。

### 7.3 Approval

approval 事件保留 kernel request id：

```rust
pub struct RuntimeRequest {
    pub id: RequestId,
    pub kind: String,
    pub payload: serde_json::Value,
}
```

对 coding approval 可提供解析后的 view，但 response 始终使用原 id。新请求到达时不得覆盖旧请求；
若 UI 只支持一个 panel，旧请求要先 fail-closed，再展示新请求。

未知 `kind` 作为 opaque `RuntimeRequest` 原样交给 driver，runtime 仍登记 id。driver 不支持、event
receiver 关闭或请求被 replacement 失效时，必须显式 Null/deny 或 cancel；任何未知请求都不能默认
批准，也不能因无法渲染而悬挂 kernel turn。

审批期间不能把 core `ApprovalNeeded.snapshot` 的空值继续冒充实时 snapshot。kernel 当前会把 turn
中的 Snapshot 排队到 turn 结束，因此本阶段契约是：

- runtime endpoint 留在原 background slot 时，pending approval 与 runtime 一起保留；
- crash/reconfigure/shutdown 时审批 fail-closed，terminal snapshot 在 turn 结束后产生；
- driver 不以 approval event 中的 snapshot 作为持久化权威；
- 若未来要求“崩溃后恢复同一个 pending approval”，需单独新增可恢复 request journal，不在本迁移
  中伪造。

## 8. 生命周期事务

### 8.1 Shutdown

1. actor 将状态线性化为 ShuttingDown，拒绝新 operation；
2. 若有 pending request，发送 Null/deny 或利用 kernel Cancel 的 `cancel_pending` fail-closed；
3. 若有 turn，Cancel 并等待 terminal；
4. 继续排空 compaction terminal；
5. 发送 kernel Shutdown；
6. bounded await task，超时 abort；
7. 关闭 event source，发布 exactly-once `RuntimeStopped`；
8. 所有并发 shutdown waiter 得到同一结果。

关闭 control sender 与显式 shutdown 使用同一 teardown funnel。

### 8.2 Provider reassemble

适用 `/model`、`/provider`、`/proxy`、`/think`、`/effort` 和 provider-only reload。

```text
build next provider（旧 agent 仍可用）
  -> 失败：返回错误，旧 runtime 不变
settle current turn
shutdown old agent
assemble SAME parts + next provider
  -> 成功：commit config/provider，generation++
  -> 失败：用 old config/provider reassemble 回滚
       -> 回滚失败：RuntimeFailed，禁止假成功
```

必须保留：

- session id 和 snapshot；
- mode；
- approval/grant stores；
- hook 长生命周期状态；
- gateway affinity；
- review/subagent provider slot 与 tier provider session binding；
- telemetry attribution。

### 8.3 Capability reprepare

适用 MCP/plugin/hooks/skills 等 capability graph 变化。

```text
prepare candidate parts（旧 agent 继续运行）
  -> 失败：丢弃 candidate，旧 runtime 不变
settle current turn
shutdown old agent
assemble candidate + current provider
  -> 成功：commit parts，generation++，释放旧 MCP 资源
  -> 失败：用 old parts/provider reassemble 回滚
       -> 回滚失败：RuntimeFailed
```

candidate 必须使用当前 session id 的 `SessionMode::Resume`，并显式迁移 mode 和 grant stores。不能
通过把旧 `CodingParts` 字段逐个拍脑袋复制来维持状态；应提供一个集中、穷举的
`RuntimeContinuity::transfer(old, candidate)`，新增状态字段时编译或测试必须暴露遗漏。

### 8.4 Fresh / Resume / ChangeDir

三者都创建 candidate parts，但目标不同：

| 操作 | candidate session | working dir | goal/loop | 失败语义 |
|---|---|---|---|---|
| fresh | 新 id | 当前 | 清除 | 保留/恢复旧 runtime |
| resume | 指定 id | session metadata 所属目录 | 清除 | 旧 runtime 不变 |
| cd/project switch | 新 id | 新目录 | 清除 | 旧 runtime 不变 |

操作成功后才发布 `SessionChanged/WorkingDirectoryChanged`。driver 不得先更新 header/cwd，再等待
runtime 结果。

`/cd` 与 agent 工具内部改变 `shared_cwd` 要区分：

- slash/project switch：new project + fresh session + reprepare；
- tool `change_dir`：当前 runtime 内更新 shared cwd，默认不清 conversation。

### 8.5 Undo

1. 通过 snapshot broker 获取 exact live snapshot；
2. 对真实 user prompt 计算截断，跳过 synthetic user；
3. out-of-range 返回错误，不改变状态；
4. 保存 original snapshot；
5. settle/stop old agent；
6. durable write truncated candidate；
7. reassemble current parts/provider；
8. assemble 失败时恢复 original snapshot，并尝试恢复旧 agent；
9. 新 agent ready 后才发布 `UndoCompleted`。

`compute_undo` 应移动到 core-free 的 coding/session 模块。不得把 core message 往返转换后再截断。

### 8.6 Local shell input

`!cmd` 的进程执行、stdout/stderr 展示仍在 TUI。runtime 只接收结构化结果并加入
`pending_local_context`；下一次 `submit` 在 user text 前合并。它不单独启动 LLM turn，也不新增
kernel command。

## 9. Goal 与 Loop

### 9.1 Goal

Goal controller 属于 CodingRuntime，因为它依赖：

- 当前 conversation snapshot；
- turn terminal；
- evaluator provider；
- continuation；
- cancel、round/duration fuse；
- reconfigure/session switch 清理。

一个用户 goal 对 driver 表现为一个持续 operation：内部每个 kernel turn 仍正常触发
`LifecycleHooks::turn_complete` 和持久化，但只有 goal met/cap/unproductive/cancel/error 时发布外层
terminal。每轮发布 `GoalUpdate`。

evaluator 必须运行在可取消 task 中，结果携带 generation + controller id；晚到结果不能驱动新
session。provider reload 可选择取消当前 evaluator 后用新 provider 重新评估，不能让旧 provider
结果跨 generation 生效。

### 9.2 Loop

self-paced prompt loop 迁入 CodingRuntime，覆盖：

- continuation 和 wakeup；
- max rounds；
- delay/clock；
- cancel/stop/replace；
- session/provider reconfiguration；
- turn completion。

fixed-interval `/loop <duration> <payload>` 可以重复 slash command，runtime 不应解析 slash 字符串，
因此保留为 driver `LoopScheduler`。它必须绑定 runtime id/generation，foreground 切换、session fresh、
resume、drop slot 时自动取消。它不属于 legacy engine protocol，不能计入 bridge 退役缺口。

goal 与 self-paced loop 互斥。新 controller 开始前必须明确终止旧 controller，并发布旧 controller
terminal，不能静默覆盖。

## 10. Session 数据所有权

目标权威存储为 `atomcode-capabilities::session::SessionManager`：

```text
<id>.snapshot  canonical working set
<id>.meta      name/cwd/turn stats/list metadata
<id>.jsonl     append-only transcript
```

迁移要求：

1. TUI、daemon 的 list/resume/rename/delete 最终读取同一 manager；
2. core `<id>.json` 只作为迁移期 UI mirror 或一次性 import source；
3. 新 runtime 永不从 core JSON fallback 启动 live agent；
4. legacy import 保留 core-only metadata 时使用 patch/import，不做全量有损 round-trip；
5. import 完成后写 native snapshot/meta，并记录已导入；
6. 删除 live core conversion 后，可暂留独立 importer，但不得重新进入 runtime path。

同一 session 在一个进程内最多一个 live runtime。TUI ↔ live daemon sync 使用显式 handoff：

```text
old runtime settle + snapshot
  -> stop old runtime
  -> start target owner from exact snapshot
  -> target ack
  -> UI/route ownership switch
```

handoff 失败时恢复旧 owner；不能同时启动两个 agent 再抢占 event。

多进程同时写同一 session 需要 revision/file lock/lease，属于后续独立设计。

### 10.1 `/context` 的权威数据

`RuntimeContextStats` 不能继续沿用 bridge/daemon 当前的全零占位字段。精确查询的数据源固定为：

1. snapshot broker 返回的 canonical conversation；
2. 当前 generation 的 `CodingParts.system_prompt`、ctx builder 和 tool registry；
3. `ctx.build_messages(...)` 实际构造的消息；
4. 最近一次 kernel `Usage(MessageMeta)`，仅补充 provider 报告的 usage/窗口，不覆盖本地精确分解。

core `compute_rich_context_stats` 的纯计算逻辑迁到 core-free 的 coding/context 模块，daemon、TUI 和
runtime 共用。`/context prompt` 返回当前 generation 真正组装的 system prompt；不能用空字符串表示
“支持”。

查询语义分两层：

- kernel `Usage` 产生轻量、可流式更新的 context projection；
- `context_stats()` 是精确查询，Ready 时立即计算，InTurn/WaitingApproval 时通过 snapshot broker 排到
  turn terminal 后完成。

因此实现不能用最后一次 Usage 假装“当前精确上下文”，也不能为了即时响应增加第二个 conversation
reader。

## 11. Daemon-first 迁移

### 11.1 原则

不继续给 daemon `KernelDriver` 补 goal/loop/undo。daemon 是共享 CodingRuntime 的第一个 driver，
用于验证 native API 和事件 parity。

目标 daemon 结构：

```text
HTTP / WS / live session
  -> DaemonRuntimeRegistry
       session key -> CodingRuntimeHandle
  -> CodingRuntimeEvents
  -> DaemonEventAdapter
  -> TurnEvent / SSE / WS
```

`DaemonEventAdapter` 只做 wire shape 和展示字段映射，不持有 provider、parts、approval、session 或
AgentHandle，不执行 respawn。

### 11.2 parity gate

删除 daemon fallback 前必须覆盖：

| 能力 | 必测 |
|---|---|
| turn | text/reasoning、正常 terminal、provider error、timeout |
| tools | streaming、batch、progress、result、duration |
| approval | allow、always、deny、cancel、driver disconnect |
| usage | token usage、context projection、rate limit |
| persistence | user prompt crash-save、terminal snapshot、immediate resume |
| lifecycle | reload provider、resume、fresh、cd、shutdown |
| compaction | started、committed、no-op、failed、interrupted |
| handoff | existing session seed、sync detach/reattach |

daemon native path通过 parity 后：

1. 设为唯一 daemon path；
2. 删除 `engine_is_kernel` 和环境开关；
3. 删除 daemon 对 `spawn_bridged_runtime_with_control` 的调用；
4. 删除 `KernelDriver/KernelToWebui`；
5. 删除 `DaemonRuntimeEvent` mixed protocol；
6. daemon 不再依赖 `atomcode-bridge`。

### 11.3 迁移期 bridge 适配边界

M0 到 CLI/TUI 完成切换前，bridge 只允许保留协议适配：

```text
legacy core AgentCommand
  -> LegacyRuntimeAdapter
  -> CodingRuntimeHandle

SequencedRuntimeEvent
  -> LegacyRuntimeAdapter
  -> core AgentEvent
```

adapter 可以做类型转换和旧事件 shape 兼容，但不得持有 `AgentHandle`、provider、parts、session、
approval、goal/loop 或 respawn 状态。遇到新 runtime 无法无损表达的错误，必须映射成显式 legacy
error；不得回落到 bridge 自己执行。这样 M0 的所有权是实际迁移，不是把旧调用再套一层 facade。

## 12. CLI 与 TUI rollout

项目约束要求 daemon parity 在前，然后处理 goal/loop，再切 CLI/TUI。

### 12.1 CLI/headless

- `main.rs` 直接 `CodingRuntime::start`；
- prompt 使用 `submit`；
- approval 使用原 request id；
- terminal 使用 `TurnCompletion`；
- shutdown 使用 runtime terminal；
- 删除 `spawn_bridged_runtime_with_control`；
- ACP、clix 已是 kernel-native 参考路径，只统一 provider factory 和 event projection，不倒退到 facade。

### 12.2 TUI/background

`RuntimeEndpoint` 收敛为一个 handle：

```rust
pub struct RuntimeEndpoint {
    pub runtime: CodingRuntimeHandle,
}
```

background manager 以 runtime id 持有 endpoint 和单一 event stream。foreground/background 切换只
转移 driver 绑定，不替换 agent owner。

TUI 命令切换顺序：

1. shutdown：`/quit`、`/exit`、`/upgrade`；
2. turn：普通输入、custom command、`/init`、`/review`、`/guide`、`/skills`、`/setup`；
3. approval/cancel/mode/context；
4. provider/config cluster；
5. MCP/plugin reprepare；
6. session/resume/cd/undo/bg；
7. goal/self-paced loop。

每个垂直切片切完所有实际 sender 后，立即删除对应 core command variant 和 bridge handler；不要等到
最后一次性清枚举。

## 13. 56 个内置命令归属

| 类别 | 命令 | 目标 owner |
|---|---|---|
| 已迁移 | `/compact` | CodingRuntime native compaction |
| prompt/退出 | `/init` `/review` `/guide` `/skills` `/setup` `/quit` `/exit` `/upgrade` | runtime submit/shutdown |
| mode/context | `/plan` `/build` `/auto` `/context` | runtime mode/query |
| provider | `/login` `/logout` `/model` `/provider` `/proxy` `/reload` `/think` `/effort` | local config + runtime reassemble |
| session/project | `/resume` `/rename` `/cd` `/bg` `/background` `/clear` `/session` `/undo` `/worktree` | runtime/session manager/runtime pool |
| transport | `/webui` `/sync` `/app` | daemon registry + runtime handoff |
| capability | `/remember` `/forget` `/memory` `/mcp` `/plugin` | local capability + runtime reprepare |
| controller | `/goal` `/loop` | runtime goal/self-paced；driver fixed interval |
| 纯前端 | `/whoami` `/status` `/config` `/diff` `/usage` `/cost` `/help` `/keys` `/language` `/welcome` `/paste` `/copy` `/save` `/view` `/todo` `/desktop` | driver/local service |

还必须迁移普通输入、custom command、approval key、Esc/Ctrl-C、`!cmd`、启动 continue、modal 回调和
daemon live endpoints；只改 slash match arms 不算 driver 切换完成。

## 14. Core command 删除映射

| core variant | native replacement | 删除切片 |
|---|---|---|
| SendMessage | runtime submit → kernel SendMessage | turn |
| Cancel | runtime cancel → kernel Cancel | turn |
| ApproveTool/Always/DenyTool | runtime respond(id,value) | approval |
| AppendInput | 删除；mid-turn submit = steer | turn |
| SyncMessages | runtime snapshot broker | snapshot |
| Shutdown | runtime shutdown | shutdown |
| SetPlanMode | 删除；统一 mode | mode |
| SetMode | runtime set_mode | mode |
| RefreshContextStats | runtime context query/event | context |
| LocalShell | driver execution + runtime pending local context | turn |
| ReloadConfig | runtime reassemble/reprepare | provider |
| ReloadHooks | runtime reprepare | capability |
| ChangeDir | runtime change_directory | session/project |
| ClearConversation | runtime fresh_session | session |
| SetConversation | runtime resume/install | session |
| SetSessionId | SessionBinding 创建参数 | session |
| UndoToPrompt | runtime undo_to_prompt | session |
| SetGoal/ClearGoal | runtime goal controller | controller |
| SetLoop/ClearLoop | runtime self-paced loop controller | controller |

## 15. 实施里程碑与四态

| 里程碑 | 内容 | 四态结论 | 预计删除 |
|---|---|---|---|
| P0 | coding core-free 前置、共享 provider factory、可重载 plugin hook source | 前置清理；未迁移 driver | coding 的 core 依赖；bridge provider helper 在消费者切换后删除 |
| M0 | 完整 runtime owner、state/event、provider/session lifecycle、shutdown；bridge 降为 legacy adapter | ①逻辑实现；尚未退役 | bridge 的 AgentHandle/config/parts/provider/session 所有权 |
| M1 | daemon turn/approval/provider/session parity | daemon ②；fallback ③仍在 | daemon KernelDriver 重复逻辑逐步删 |
| M2 | goal/self-paced loop controller | controller ①/daemon ② | bridge goal/loop state 和 helper |
| M3 | daemon 唯一路径 | daemon ④ | daemon flag、bridge fallback、mixed event、bridge Cargo dep |
| M4 | CLI/headless 切换 | CLI ④ | CLI bridge spawn/dependency、对应 core sender |
| M5 | TUI/background 按垂直切片切换 | 各命令逐项④ | core variant、bridge handler/converter、双 endpoint |
| M6 | live transport/session handoff 收口 | driver 全部②，fallback 清零 | live core conversion/fallback |
| M7 | 删除 bridge/core driver protocol | 全调用链④ | bridge crate、core AgentClient/Command/Event、旧测试 |

未删除旧 variant、handler 或 fallback 时，交付必须写“尚未退役”。

### 15.1 可执行工作包

| 工作包 | 前置 | 实施内容 | 完成标准与当场删除 |
|---|---|---|---|
| W0.1 coding core-free | 无 | vision、rate-limit 类型/数据源反转 | coding 生产依赖无 core；删除对应 import/adapter |
| W0.2 shared provider | W0.1 | provider factory、signing、tier provider | bridge/daemon/ACP/clix parity；删除私有重复 builder |
| W0.3 reloadable hooks | W0.1 | `PluginHookSource` 与 reload parity | reprepare 可取最新 hooks；不复制 plugin owner |
| W1.1 runtime skeleton | W0.* | actor、phase、generation、sequence、start/shutdown | shutdown 失败测试通过；bridge 不再持有 AgentHandle |
| W1.2 turn protocol | W1.1 | submit/steer/respond/cancel/terminal/snapshot/context | legacy adapter parity；bridge 对应 turn 聚合状态删除 |
| W1.3 lifecycle owner | W1.2 | provider/reprepare/fresh/resume/cd/undo 事务 | config/parts/provider/session 只由 runtime 持有；bridge respawn 删除 |
| W2 daemon adoption | W1.3 | registry、wire adapter、全 parity gate | daemon 只调用 runtime；旧路径暂保留但不得新增能力 |
| W3 controllers | W2 | goal 与 self-paced loop | daemon parity；bridge controller 状态/helper 删除 |
| W4 daemon retirement | W3 | native 设为唯一 daemon path | flag、KernelDriver、mixed event、bridge 依赖删除 |
| W5 CLI retirement | W4 | headless/CLI sender 与 event consumer 全切 | CLI bridge spawn/依赖及无剩余 sender 的 core variant 删除 |
| W6.1 TUI turn vertical | W5 | shutdown、turn、approval、cancel、mode、context | 对应 sender/handler/converter 删除 |
| W6.2 TUI config vertical | W6.1 | provider/config、MCP/plugin reprepare | reload legacy surface 删除 |
| W6.3 TUI session vertical | W6.2 | session/resume/cd/undo/background | 双 endpoint 与 session core conversion 删除 |
| W6.4 TUI controller vertical | W6.3 | goal/loop 与 fixed-loop generation binding | controller core variant/handler 删除 |
| W7 live handoff | W6.* | sync/webui/app owner handoff | live fallback/core event conversion 删除 |
| W8 protocol deletion | W7 | 删除 bridge crate 和 core driver protocol | 静态退役检查零命中，workspace 验证通过 |

工作包允许拆成多个小提交，但不能跨包宣称退役。例如 W1.2 完成只代表 runtime turn 逻辑和 adapter
parity 已实现；daemon、CLI、TUI sender 未删除前，turn protocol 仍是状态③。

## 16. 第一实施切片：P0 core-free 前置

详细复核后，第一片不应直接做 runtime owner。当前 provider factory、plugin hook source 和 coding
自身的 core 依赖尚未收口，直接让 runtime 接管所有权会迫使实现继续反向调用 bridge/core，形成
新的循环或“owner 名义在 coding、构建能力仍在 bridge”的假迁移。

P0 只清理完整 owner 的依赖前置，不改变 driver 行为。

### 16.1 实际改动

1. `assemble.rs`、`parts.rs` 改用 capabilities 的统一 vision detector；
2. 在 coding 中定义中立 `RateLimitWindow`/限流决策，HTTP/auth 数据源通过
   `RateLimitWindowSource` 注入；
3. 建立 `CodingProviderFactory` 及默认共享实现；
4. 把 bridge 的 provider 类型选择、UA、TLS、reasoning、max tokens、tier provider 构建迁入共享
   factory；
5. AtomGit signing 低层能力移到 auth/atomgit capability，factory 调用该能力；
6. 建立 `PluginHookSource` trait，bridge 暂时提供基于现有 plugin loader 的实现；
7. `atomcode-coding/Cargo.toml` 删除生产 `atomcode-core` 依赖；
8. CLI ACP、clix、bridge、daemon 的 provider 构建改用同一 factory，避免下一阶段继续复制。

### 16.2 行为 parity 测试

P0 是搬迁和依赖反转，不改变 provider 行为。必须锁定：

1. OpenAI/Claude/Ollama provider dispatch；
2. AtomGit signing 与普通 endpoint 非签名路径；
3. reasoning history、reasoning effort、thinking type/keep；
4. max tokens fallback；
5. stream/request timeout；
6. User-Agent 和 skip TLS；
7. session id/gateway affinity；
8. fast/capable tier lazy build、host-equal collapse、model swap reset；
9. vision model 判定与迁移前逐例一致；
10. CodingPlan window source 失败时保留现有 fail-open/fail-closed 决策，不吞错；
11. `cargo tree -p atomcode-coding` 生产依赖中无 `atomcode-core`。

### 16.3 文件影响预估

| 文件 | 预期改动 |
|---|---|
| `atomcode-coding/src/assemble.rs` | vision detector 切换 |
| `atomcode-coding/src/parts.rs` | vision detector、plugin source 接口 |
| `atomcode-coding/src/rate_limit.rs` | 数据源反转、core-free 类型 |
| `atomcode-coding/src/provider_factory.rs` | 新共享 factory |
| `atomcode-coding/src/config.rs` | 去 bridge 语义和 core 类型 |
| `atomcode-coding/src/lib.rs` | 导出 factory/source |
| `atomcode-coding/Cargo.toml` | 删除 core 依赖 |
| `atomcode-bridge/src/runtime.rs` | 改为调用共享 factory；暂不迁 lifecycle |
| `atomcode-bridge/src/sign.rs` | 下沉后删除或只留短期兼容入口 |
| CLI ACP、clix、daemon provider 构建点 | 改用共享 factory |

### 16.4 P0 删除与保留

实际删除：

- coding 对 core vision/rate-limit 类型和 client 的引用；
- bridge 私有 provider 构建实现（所有消费者切换后）；
- provider 构建的重复测试副本。

暂时保留：

- bridge command/event/lifecycle；
- daemon KernelDriver 和 feature flag；
- TUI/CLI legacy endpoint；
- `/compact` 现有 native API。

P0 不达到任何命令退役状态，交付必须写“driver legacy surface 未变化”。

### 16.5 下一切片入口门槛

只有满足以下条件才进入 M0 runtime owner：

- coding 生产依赖 core 为零；
- bridge、daemon、ACP/clix 使用同一 provider factory；
- plugin hook reload 可以通过 source 重新读取；
- provider parity tests 通过；
- 没有新增 bridge → coding → core/bridge 依赖环。

M0 随后一次性接管 config、parts、provider、session binding 和 AgentHandle；bridge 只保留 legacy
command/event adapter。不能只接管 AgentHandle 却让 bridge 继续修改 parts/provider/session。

建议 M0 按职责落到以下模块，避免继续膨胀当前单文件 runtime：

| 模块 | 唯一职责 |
|---|---|
| `runtime/mod.rs` | public start/handle/events 类型 |
| `runtime/actor.rs` | control 与 kernel event 的唯一 select loop |
| `runtime/state.rs` | phase、generation、accepted operation |
| `runtime/events.rs` | sequence envelope、terminal/event projection |
| `runtime/snapshot.rs` | 单在途 snapshot broker |
| `runtime/lifecycle.rs` | provider/reprepare/session replacement 事务 |
| `runtime/context.rs` | core-free 精确 context stats |
| `runtime/controller.rs` | goal/self-paced loop；M2 再接入 |

bridge 的 `LegacyRuntimeAdapter` 仍放在 bridge crate，不能放回 coding 形成 core 依赖。

M0 的 shutdown 生命周期必须先写失败测试：running turn、parked approval、并发 shutdown、bounded
abort、线性化后拒绝、event sequence、exactly-once RuntimeStopped，以及 start 失败不返回 noop
handle。M0 完成后仍是“逻辑已实现，尚未退役”；core `Shutdown` 要等 CLI/TUI sender 全切换后删除。

## 17. 测试与验收

### 17.1 Runtime 单元/集成测试

- state operation matrix；
- start/shutdown/failure；
- submit/steer/turn terminal；
- request id correlation 与 fail-closed；
- snapshot broker；
- compaction exactly-once terminal；
- provider reassemble success/build-failure/assemble-failure/rollback-failure；
- reprepare candidate failure 与 old-runtime rollback；
- fresh/resume/cd/undo；
- generation 和 late event；
- goal/loop cancel/reload/session switch；
- 一个 `CodingParts` 不产生两个 live Agent。

### 17.2 Driver parity

| driver | 场景 |
|---|---|
| daemon/webui | live turn、approval、cancel、reload、resume、cd、compact、disconnect、`/chat` 文本模型图片 VL 预处理 |
| CLI/headless | prompt、approval policy、cancel、rate limit、terminal、shutdown |
| TUI | foreground、background、mode、context、modal reload、session replay、undo |
| clix/ACP | 共享 provider/event 语义不回退 |

### 17.3 静态退役检查

最终生产代码必须无命中：

```text
spawn_bridged_runtime_with_control
BridgedRuntime
BridgeConfig
KernelRuntimeAdapter
RuntimeEndpoint.legacy
DaemonRuntimeEvent::Legacy
DaemonRuntimeEvent::Native
atomcode_core::agent::AgentClient
atomcode_core::agent::AgentCommand
atomcode_core::agent::AgentEvent
ATOMCODE_DAEMON_ENGINE
```

kernel `AgentCommand/AgentEvent`、native `CodingRuntimeEvent` 和独立 legacy session importer 应按目标
边界保留。

### 17.4 测试强度

- 开发中运行当前逻辑单元的最小测试；
- 一个切片完成后运行受影响 crate 完整测试；
- command/event 公共协议或跨 crate lifecycle 改动运行相关 workspace all-target check；
- 最终删除 bridge/core driver surface 时运行 workspace test/check；
- 同一失败在代码和环境未变化时不盲目重跑。

## 18. Review 检查表

每个实现 PR 必须回答：

1. 本次用户入口和所有 driver sender 是哪些？
2. operation 在哪个状态 accepted？线性化点在哪里？
3. success/error/cancel/replace/shutdown terminal 分别是什么？
4. 是否可能跨 generation？
5. session/provider/mode/grants/gateway affinity 保留了什么？
6. 哪些旧 variant、handler、converter、fallback 已实际删除？
7. 哪些 legacy surface 仍可达？
8. daemon、CLI、TUI、background 中哪些路径已验证？
9. 是否引入第二 AgentHandle owner 或第二 event consumer？
10. 是否用空 snapshot、noop handle、fallback fresh 掩盖失败？

## 19. 唯一下一步

进行一次接真实 provider 的跨入口人工 smoke，覆盖 CLI、TUI、daemon 的 turn、approval、cancel、
resume、provider reload 和图片输入；真实凭据不应在仓库测试中伪造。
