# CodingRuntime 渐进迁移设计与首个切片实施计划

> 状态：方案已收敛；`/compact` 的命令面、事件面和 runtime owner 三个垂直切片均已
> 实施，并在 `release/v5.0.0@a102ff814bb685c706b346fa9d29e2481c3680cf` 完成生命周期
> 加固。`/compact` 专属 legacy 接口面已经退役；其他 slash 命令和一般 core/bridge surface
> 仍保留。
>
> 最终实现结论、开发踩坑和后续命令复用清单见
> [`compact-native-migration-retrospective.md`](compact-native-migration-retrospective.md)。本文
> 后续章节保留各切片当时的设计基线和中间状态，不应脱离章节时间点引用其中的
> “尚未完成”结论。
>
> 分析起点：`release/v5.0.0@332f8771a51fb5ad1f6f97bcc2bc0016ce7dacd8`。
>
> 实施前重核基线：`release/v5.0.0@2d7e33360c10b3d33ee89e49c62c1d8aaa9fa430`。
> 实施期间分支新增了 kernel 超窗应急压缩和 TUI plugin UI 修改；已按新 HEAD
> 重新搜索并完成编译/测试，未发现与手动 compact 控制面冲突。
>
> 第二切片设计重核基线：`release/v5.0.0@a2608ffd746b76fae437543028082203b1079051`。
> 当前 core v1 `AgentLoop` 已删除，但 daemon 仍默认使用 bridge 适配路径，daemon
> kernel driver 仍是 opt-in；本文继续把 core driver 协议、bridge 转换和 fallback
> 视为 legacy surface，不把“底层使用 kernel”称为已经退役。
>
> 本文聚焦一个问题：如何在不一次性重写 CLI、TUI、daemon 和全部 slash
> 命令的前提下，引入一个 kernel-native、最终可脱离 `atomcode-core` 与
> `atomcode-bridge` 的 `CodingRuntime`。

## 1. 背景与目标

当前生产入口底层已经使用 kernel v2，但 CLI、TUI、daemon 的主要驱动契约仍是：

```text
CLI / TUI / daemon
        │
        │ atomcode_core::agent::{AgentClient, AgentCommand, AgentEvent}
        ▼
atomcode-bridge
        │
        │ kernel AgentCommand / AgentEvent
        ▼
atomcode-kernel
```

这意味着“底层使用 v2”并不等于“已经脱离 bridge”。只要 driver 仍发送 core
legacy 命令、消费 core legacy 事件，bridge 的命令处理、事件转换和 fallback 就仍然
可达。

本文目标是引入以下结构：

```text
CLI / TUI / daemon
        │
        ▼
CodingRuntime
        │
        ▼
kernel AgentHandle
```

迁移完成后：

- driver 不再依赖 core 的 `AgentClient/AgentCommand/AgentEvent`；
- runtime 直接驱动 kernel 原生命令和事件；
- session、provider、working directory、审批和重建语义具有唯一所有者；
- `atomcode-bridge` 及对应 legacy 类型、handler、转换、fallback 可以实际删除。

本文与以下文档的关系：

- [target-architecture.md](target-architecture.md)：定义最终依赖方向和北极星；
- [v5.0.0-retire-bridge-core-progress.md](v5.0.0-retire-bridge-core-progress.md)：记录 bridge/core 退役进度；
- [compact-native-migration-retrospective.md](compact-native-migration-retrospective.md)：记录
  `/compact` 最终实现、纠偏结论、开发踩坑和下一条命令的复用清单；
- 本文：定义 `CodingRuntime` 的近期承载位置、职责边界和小步迁移方法。

## 2. 术语

### 2.1 legacy 类型

本文中的 legacy 类型主要指：

```rust
atomcode_core::agent::AgentClient
atomcode_core::agent::AgentCommand
atomcode_core::agent::AgentEvent
```

它们仍然能工作，但属于旧 driver 协议。bridge 负责将它们转换为 kernel v2
命令和事件。

### 2.2 core-free

`core-free` 是架构属性，不是模块名，表示某个模块的实现和 Cargo 依赖图中不再包含
`atomcode-core`。

它不表示“没有核心逻辑”，也不表示“没有基础依赖”。例如一个 core-free runtime
仍然可以依赖：

```text
atomcode-config
atomcode-kernel
atomcode-capabilities
atomcode-coding
atomcode-telemetry
```

不应使用以下临时名称：

```text
core_free.rs
v2_runtime.rs
new_runtime.rs
legacy_free.rs
```

推荐使用稳定职责命名：

```text
atomcode-coding/src/runtime.rs
atomcode_coding::runtime::CodingRuntime
```

### 2.3 四种迁移状态

涉及 bridge/core 退役时必须区分：

1. **逻辑已实现**：新栈已有对应能力；
2. **driver 已切换**：driver 已调用新 runtime/kernel 路径；
3. **legacy fallback 仍保留**：旧 variant、handler、依赖或回退仍可达；
4. **legacy 接口面已退役**：旧发送点、variant、handler、依赖和 fallback 已删除。

只有第 4 种状态可以称为“已退役”。

## 3. 当前代码判断

### 3.1 kernel 已具备原生会话句柄

`atomcode-kernel` 已提供：

```rust
pub struct AgentHandle {
    pub commands: UnboundedSender<AgentCommand>,
    pub events: UnboundedReceiver<AgentEvent>,
    pub task: tokio::task::JoinHandle<()>,
}
```

kernel 原生命令保持精简：

```text
SendMessage
Respond
Snapshot
Compact
Cancel
Shutdown
```

这是运行中的 Agent 协议，不应继续加入 `/model`、`/resume`、`/cd`、
`/reload` 等外部生命周期命令。

### 3.2 `atomcode-coding` 是最接近的承载层

`atomcode-coding` 已拥有：

- `CodingAgentConfig`；
- `CodingParts`；
- `prepare`：加载 MCP、skills、memory、session、hooks；
- `assemble`：把 parts 与 provider 组装成 kernel `Agent`；
- session binding；
- plan、bypass、accept-edits 等共享模式状态；
- review/subagent provider slot；
- reassemble 时保留 session identity、snapshot、approval grant 和 hook 状态的基础能力。

因此 `CodingRuntime` 不需要重新实现 assembly，应建立在现有
`prepare → CodingParts → assemble → AgentHandle` 之上。

### 3.3 `atomcode-coding` 当前还不是真正 core-free

crate 文档声明其目标为零 core 参与，但当前 `Cargo.toml` 仍直接依赖
`atomcode-core`，主要来自两类调用：

1. `model_name_suggests_vision`；
2. CodingPlan 限流窗口类型和状态查询 client。

vision 判断已有 `atomcode-config::util::model_name_suggests_vision` 可复用。

CodingPlan 限流需要把“限流决策”和“窗口数据来源”解耦，例如：

```rust
trait RateLimitWindowSource {
    async fn fetch_windows(&self) -> Result<Vec<RateLimitWindow>, RateLimitSourceError>;
}
```

`atomcode-coding` 只保留中立限流决策和 hook；具体 HTTP/auth 实现由低层独立组件
或 driver 注入。

### 3.4 其他现有模块不适合作为 runtime 所有者

| 模块 | 结论 | 原因 |
|---|---|---|
| `atomcode-kernel` | 不放 | 必须保持中立，不知道具体 coding、config、session、provider reload |
| `atomcode-capabilities` | 不放 | 它是能力池，不应反向负责完整 coding 生命周期 |
| `atomcode-clix` | 只作参考 | 已有 kernel-native 驱动路径，但属于具体 CLI driver |
| CLI ACP engine | 只作参考 | 已有 `prepare → assemble → spawn`，但只覆盖 ACP 子集 |
| daemon `kernel_runtime.rs` | 不复用为目标 | 仍依赖 `BridgeConfig`、CoreCmd/CoreEv 和 bridge helper |
| `atomcode-bridge::runtime` | 只作语义参考 | 迁移目标是拆除它，而不是换名搬运 |

## 4. 架构决策

### 4.1 不新增独立 crate

近期不新增 `atomcode-runtime` crate。新增独立 crate 现在没有经过第二种业务 runtime
验证，容易为了“通用”而定义过大的抽象，形成 bridge 2.0。

推荐在 `atomcode-coding` 内新增：

```text
atomcode-coding/src/runtime/
├── mod.rs
├── lifecycle.rs
├── session.rs
├── provider.rs
└── error.rs
```

第一阶段也可以先使用单个 `runtime.rs`，但公共 API 必须保持收敛。

### 4.2 稳定职责

`CodingRuntime` 的稳定职责是：

> 管理一个 coding agent 运行实例的创建、运行、停止、重建和恢复。

应包含：

- send/respond/compact/snapshot/cancel/shutdown；
- provider/model 参数变化后的 reassemble；
- MCP/hooks/skills/plugin 等变化后的 reprepare；
- fresh session、resume、working directory 切换、undo；
- session ID、snapshot、mode、approval、gateway affinity 的一致性维护。

不应包含：

- slash 字符串解析；
- TUI modal、状态栏和渲染；
- daemon HTTP/WebSocket 协议；
- OAuth 页面交互；
- 配置文件编辑 UI；
- plugin 市场 UI；
- legacy 命令/事件类型转换。

### 4.3 kernel 命令与 runtime 控制分离

直接作用于运行中 Agent 的操作继续使用 kernel 原生命令：

```text
SendMessage / Respond / Snapshot / Compact / Cancel / Shutdown
```

需要创建或替换 Agent 的操作属于 runtime 生命周期：

```text
ReloadProvider / Reprepare / FreshSession / Resume / ChangeDirectory / Undo
```

配置/session 生命周期命令不应加入中立的 kernel `AgentCommand`。

## 5. 建议的所有权模型

为了支持渐进迁移，需要区分 runtime 所有者、可克隆控制句柄和单消费者事件流：

```rust
pub struct CodingRuntime {
    config: CodingAgentConfig,
    parts: CodingParts,
    handle: AgentHandle,
    provider_factory: Arc<dyn ProviderFactory>,
}

#[derive(Clone)]
pub struct CodingRuntimeHandle {
    control_tx: Sender<RuntimeControl>,
    kernel_tx: UnboundedSender<atomcode_kernel::event::AgentCommand>,
}

pub struct CodingRuntimeEvents {
    event_rx: UnboundedReceiver<atomcode_kernel::event::AgentEvent>,
}
```

以上仅为 API 方向草案，最终需结合 TUI/daemon 的事件循环选择具体 channel。

不变量：

1. 一个 session 同时只能有一个 live `AgentHandle`；
2. 一个 kernel event receiver 只能有一个消费者；
3. provider reload、resume、cd、undo 等生命周期操作由 runtime 串行执行；
4. 不允许 bridge runtime 与新 runtime 各自启动一套 Agent；
5. 不允许 bridge 和 TUI 竞争读取同一个 kernel event receiver。

过渡结构应为：

```text
未迁移命令 ── Legacy Bridge Adapter ─┐
                                     ├─ CodingRuntime ── 唯一 AgentHandle
已迁移命令 ── Native RuntimeHandle ──┘
```

## 6. 生命周期操作分类

不能把所有切换统一实现成一个无差别 `restart()`。至少需要区分：

### 6.1 reassemble

复用同一 `CodingParts`，使用新 provider/config 重新组装 Agent。

适用：

```text
/model
/provider
/proxy
/think
/effort
provider-only reload
```

应保留：

- session ID；
- conversation snapshot；
- approval grants；
- mode 状态；
- hook 长生命周期状态；
- gateway affinity。

### 6.2 reprepare

重新执行有 I/O 的 `prepare`，再 assemble 新 Agent。

适用：

```text
MCP reload
hooks reload
skills/plugin reload
memory/project capability reload
```

需要处理：

- 旧 MCP 子进程退出；
- 新 capability graph 验证；
- snapshot/session identity 延续；
- prepare 失败时的回滚语义。

### 6.3 fresh

建立新的 session binding 和新的 `CodingParts`。

适用：

```text
/clear
/session
部分 /cd 语义
显式新会话
```

需要明确清除：

- 旧 conversation；
- pending approval；
- goal/loop controller；
- turn/UI 统计；
- 不应跨 session 保留的 hook 状态。

## 7. 渐进迁移策略

### 7.1 可以小步迁移，但不能双 runtime

渐进迁移允许两套调用接口暂时并存：

- legacy bridge adapter 服务未迁移命令；
- `CodingRuntimeHandle` 服务已迁移命令。

两套接口必须指向同一个 `CodingRuntime`，不能各自持有 Agent、session 或 event
receiver。

### 7.2 阶段 1：建立 runtime 基础，不改变外部行为

新增并验证：

```text
start
send
respond
compact
snapshot
cancel
shutdown
```

bridge 内部开始委托 `CodingRuntime`，但 CLI/TUI/daemon 仍可暂时使用 legacy
接口。

这一阶段只达到“逻辑已实现”，没有命令退役。

### 7.3 阶段 2：迁移简单命令的垂直切片

优先候选：

```text
/compact
/quit
/exit
/upgrade
/init
/review
/guide <name>
/skills <name>
```

一个命令只有同时完成以下动作才算退役：

1. CLI/TUI/daemon 的旧发送点全部切换；
2. core 对应 command variant 删除；
3. bridge 对应 handler 删除；
4. legacy 转换和测试删除或替换；
5. native 路径测试覆盖实际 driver。

如果只是 bridge handler 内部改为调用 runtime，该命令仍然经过 bridge，只能称为
“已委托”，不能称为“已退役”。

### 7.4 阶段 3：模式与审批簇

一起迁移：

```text
/plan
/build
/auto
AcceptEdits mode
approval Request/Respond
/context
```

共享状态包括：

- `plan_mode`；
- `bypass_mode`；
- `accept_edits`；
- pending request ID；
- approval grant；
- context usage/report。

### 7.5 阶段 4：provider 配置簇

一起设计、分批落地：

```text
/login
/logout
/model
/provider
/proxy
/think
/effort
/reload 的 provider-only 分支
```

共享语义包括 session、snapshot、gateway affinity、review/subagent provider、
approval 和 telemetry。

### 7.6 阶段 5：session/project 簇

一起设计、分批落地：

```text
/clear
/session
/resume
/rename
/undo
/cd
/bg
/background
```

不能把这些命令当成互不相关的局部功能，因为它们共享 session store、working
directory、snapshot、UI replay 和 runtime 重建语义。

### 7.7 阶段 6：goal/loop 生命周期

最后迁移：

```text
/goal
/loop
```

必须完整覆盖 evaluator、continuation、wakeup、delay、cancel、互斥、最大轮次、
turn completion 和 respawn 清理，不能只复制 bridge handler。

### 7.8 阶段 7：切换事件流并删除 bridge

命令发送可以渐进迁移，TUI kernel event receiver 的消费权建议作为单独里程碑切换。

切换前 bridge 可继续作为唯一事件适配器；切换后 TUI/daemon 直接消费 kernel
事件，并用少量 driver-domain 事件表达：

```text
ProviderChanged
SessionChanged
WorkingDirectoryChanged
ModeChanged
RuntimeRestarting
RuntimeRestarted
```

不要把整个 core `AgentEvent` 复制为一个新的大枚举。

## 8. Slash 命令迁移分类

当前 55 个内置 slash 命令可分为三组。

### 8.1 至少一个分支直接依赖 legacy runtime：32 个

```text
/login      /resume      /logout       /model
/provider   /proxy       /reload       /cd
/init       /bg          /background   /clear
/session    /context     /compact      /mcp
/undo       /upgrade     /plan         /build
/auto       /review      /think        /effort
/goal       /loop        /guide        /quit
/exit       /skills      /plugin       /setup
```

难度分层：

| 难度 | 命令 | 主要原因 |
|---:|---|---|
| 2/5 | `/compact`、`/init`、`/review`、`/guide`、`/skills`、`/quit`、`/exit`、`/upgrade` | 可直接映射 kernel 命令或 prompt |
| 3/5 | `/plan`、`/build`、`/auto`、`/context` | 共享模式、审批和上下文状态 |
| 4/5 | `/login`、`/logout`、`/model`、`/provider`、`/proxy`、`/think`、`/effort`、`/background`、`/mcp`、`/plugin`、`/setup`、`/clear`、`/session` | provider swap、reprepare、多 runtime 或 session fresh |
| 5/5 | `/reload`、`/resume`、`/cd`、`/bg`、`/undo`、`/goal`、`/loop` | 完整生命周期、双存储、项目切换、多 runtime、控制器 |

### 8.2 不直接发送引擎命令：20 个

```text
/rename     /whoami    /status    /config
/diff       /cost      /remember  /forget
/memory     /worktree  /help      /keys
/language   /welcome   /paste     /copy
/save       /view      /todo      /desktop
```

这些命令无需切换 engine command，但部分需要适配 native session/runtime 状态：

- `/rename` 写统一后的 session metadata；
- `/status`、`/cost` 读取 native provider/usage 状态；
- `/copy`、`/save`、`/todo` 适配 native 消息与 replay；
- `/paste` 最终经 kernel `SendMessage.images` 发送。

### 8.3 传输入口：3 个

```text
/webui
/sync
/app
```

命令执行本身不发送 engine command，但其后续对话经过 daemon。要做到全路径无
bridge，必须使 daemon live/chat 直接使用 `CodingRuntime`，并删除 daemon 的
bridge 默认路径和 fallback。

## 9. 实现前仍需完成的详细设计

当前已经确定模块归属和迁移方向，但以下问题必须在全面实现前明确。

### 9.1 Runtime 状态机

建议至少讨论以下状态：

```rust
enum RuntimeState {
    Starting,
    Ready,
    InTurn,
    WaitingApproval,
    Restarting,
    ShuttingDown,
    Stopped,
    Failed,
}
```

需要定义每个操作在各状态下是允许、排队、取消当前 turn，还是拒绝。

### 9.2 Session 数据所有权

需要统一：

- legacy `.json` 与 native `.snapshot/.meta/.jsonl`；
- `/resume` 的 UI replay 和统计恢复；
- `/undo` 对 append-only transcript 的处理；
- `/rename` 的 metadata owner；
- `/cd` 是更新当前 session 还是 fresh session；
- snapshot 保存失败时的继续/终止策略。

### 9.3 Provider factory 边界

需要决定 runtime 接收预构建 provider，还是持有可热切换的 provider factory。

具体实现必须覆盖：

- AtomGit gateway 签名；
- session affinity；
- proxy/TLS/user-agent；
- reasoning/chat options；
- review/subagent tier；
- provider 构建失败后的回滚。

### 9.4 Approval 生命周期

需要明确：

- kernel request ID 的唯一持有者；
- cancel/reload/respawn 时如何 fail-closed；
- Auto 和 AcceptEdits 的职责边界；
- 并发或连续 request 的处理；
- driver 消失时如何释放等待中的请求。

### 9.5 原子切换与失败恢复

每种重建操作都要回答：

- 新 provider 构建失败时旧 Agent 是否继续运行；
- reprepare 失败时旧 MCP 是否仍可用；
- 旧 Agent 已退出而新 Agent spawn 失败如何恢复；
- snapshot 保存失败是否阻止 respawn；
- 如何满足同一 `CodingParts` 最多一个 live Agent 的约束。

## 10. 测试与验收

### 10.1 Runtime 单元/集成测试

至少覆盖：

- start/send/terminal event；
- approval request/respond/cancel；
- snapshot/compact/shutdown；
- provider reload 保持 session ID 与 snapshot；
- reprepare 清理旧资源；
- resume/undo；
- respawn 失败回滚；
- 同一 parts 不产生双 live Agent。

### 10.2 Driver parity

每个迁移簇都要覆盖受影响的：

```text
CLI
TUI
daemon/webui
headless
session resume
approval
cancel
provider reload
```

### 10.3 退役验收

一个功能只有在以下 surface 均删除后才算退役：

- 所有 driver legacy 发送点；
- core `AgentCommand/AgentEvent` 对应 variant；
- bridge `on_command` handler；
- bridge event 转换；
- CLI/TUI/daemon 对 legacy 类型的依赖；
- v1/bridge fallback；
- 只验证旧路径的测试。

## 11. 建议的第一实施里程碑

第一里程碑采用“先建立稳定控制面，再完成一个命令的垂直退役”，不先实现完整
`CodingRuntime`。原因是当前 bridge 同时持有 provider 重建、session、approval、
goal/loop 等生命周期；首期直接搬走这些所有权，会把一个可独立验证的命令迁移扩大成
系统性重写。

第一里程碑顺序调整为：

1. 在 `atomcode-coding::runtime` 建立稳定的 `CodingRuntimeHandle` 控制面；
2. handle 不暴露 bridge 或 core 类型，只提供面向能力的方法；
3. bridge 作为当前临时 runtime owner，接收控制请求并转发给“当前” kernel
   `AgentHandle`；
4. TUI 的前台/后台 runtime 绑定 legacy client 与 native handle，避免 `/bg`、
   `/resume` 切换后串发到错误 runtime；
5. 将 `/compact` 的发送端切到 `CodingRuntimeHandle::compact`；
6. 删除 core `AgentCommand::Compact`、bridge 对应 handler、daemon legacy
   command translator 分支与只验证旧映射的测试；
7. 保留 compaction 的 legacy 事件适配和 daemon 离线 compact，直到各自的事件、
   session 生命周期切片迁移；
8. 针对性验证稳定 handle、TUI runtime 切换以及 kernel compaction 行为。

清除 `atomcode-coding` 当前全部 core 依赖仍是目标，但不是建立第一个控制句柄的硬前置。
目前 `rate_limit` 和 vision model 判断仍直接使用 core；把它们与 `/compact` 捆绑只会增加
无关风险。本里程碑因此只能称为“runtime 控制面 source-level 不使用 core”，不能称为
整个 `atomcode-coding` crate 已 core-free。

该里程碑完成后应明确报告：

- runtime 基础已建立；
- 哪些命令只是“已委托”；
- 哪些 legacy variant/handler 已实际删除；
- 哪些路径仍可达 bridge；
- 下一步唯一的垂直切片。

## 12. 未来是否拆出独立 runtime crate

`runtime` 未来可能物理迁出 `atomcode-coding`，但现在不应提前泛化。

只有出现以下证据时才考虑独立 `atomcode-runtime`：

1. coding、review 和其他业务都需要相同生命周期；
2. runtime 大部分代码不再引用 `CodingParts/CodingAgentConfig`；
3. 多个 L2 开始复制同一套 runtime；
4. 当前放置产生明确依赖环；
5. runtime 需要独立 feature、发布或测试边界。

当前正确策略是保持边界清晰，使未来拆分成为“移动模块”，而不是重新设计生命周期。

## 13. 当前结论

- 不新增名为 `core-free`、`v2` 或 `new` 的模块；
- 不新增独立 runtime crate；
- 在 `atomcode-coding` 中新增职责稳定的 `runtime` 模块；
- 先清除 `atomcode-coding` 的实际 core 依赖；
- 采用单一 `CodingRuntime` + legacy bridge adapter 的过渡结构；
- 允许小步迁移，但必须按共享状态簇推进；
- 每个完成的垂直切片必须同步删除旧 variant、handler、依赖和 fallback；
- 事件流保持单消费者，不能让 bridge 和新 driver 竞争读取；
- session、approval、provider、goal/loop 和失败恢复仍需在实施前做详细设计。

## 14. `/compact` 首个垂直切片

### 14.1 实施前调用链

当前 TUI 手动压缩路径为：

```text
TUI /compact
  → core AgentCommand::Compact
  → bridge on_command
  → kernel AgentCommand::Compact
  → kernel CompactionStarted / Compacted
  → bridge CompactionUi 事件转换
  → TUI spinner / mark
```

clix 已直接发送 kernel `AgentCommand::Compact`。daemon 的 kernel translator 仍接受
core `AgentCommand::Compact`；daemon `commands.rs` 还存在一条独立的离线 session
压缩路径，两者不是同一命令通道。

### 14.2 为什么选择 `/compact`

`/compact` 适合作为首个切片，因为它已经有稳定的 kernel 命令语义，且不要求 runtime
重建。它比以下候选更适合：

- `/init`、`/review` 仍复用 legacy `SendMessage`；
- `/quit` 涉及整个 driver 与 task 的关闭握手；
- `/undo`、`/resume`、`/cd` 涉及 snapshot/session/runtime 重建；
- `/goal`、`/loop` 涉及持有回合、evaluation、cancel、wakeup 和 continuation。

### 14.3 稳定控制句柄

TUI 不能直接缓存 `kernel AgentHandle.commands`。bridge 会在 `/model`、`/provider`、
`/cd`、`/resume`、`/clear` 等操作中替换 kernel `AgentHandle`；直接缓存 sender 会在
respawn 后继续指向旧 agent。

首期引入：

```rust
CodingRuntimeHandle::compact(focus: Option<String>)
```

handle 向长生命周期 owner 发送 runtime control。bridge 暂时承担 owner，并在收到请求
时把命令投递给它当前持有的 kernel handle。因此 native handle 在 kernel agent respawn
前后保持稳定。

边界约束：

- driver 不持有或替换 kernel raw sender；
- public 方法表达能力，不接受 slash command 字符串；
- channel 关闭返回明确错误，不静默吞掉；
- control receiver 保持单消费者；
- 本切片只增加 `compact` 方法，不提前虚构完整 session/provider API。

### 14.4 TUI runtime 绑定

TUI 支持多个同时存活的 bridge runtime。每个 runtime 必须把以下两者作为同一个
endpoint 保存：

```text
legacy AgentClient       处理尚未迁移的命令
CodingRuntimeHandle      处理已迁移的 native 控制
```

`/bg`、`/background`、`/bg resume` 和 session respawn 必须整体保存、恢复该 endpoint。
仅在 `LoopCtx` 增加一个未参与后台切换的全局 handle，会导致命令发往错误会话。

### 14.5 本切片实际删除目标

完成后应删除：

- `atomcode_core::agent::AgentCommand::Compact`；
- TUI 对该 core variant 的发送；
- `atomcode-bridge::runtime::on_command` 的 `CoreCmd::Compact` 分支；
- daemon kernel translator 的 `CoreCmd::Compact` 分支；
- 只验证 core compact 到 kernel compact 映射的测试断言。

完成后仍保留，且必须明确标记“尚未退役”：

- core `AgentEvent::CompactionUi`；
- bridge/daemon 的 kernel compaction 事件转换；
- daemon `commands.rs` 的离线 session compact；
- bridge 及 TUI 的其他 core command/event 依赖；
- v1/legacy engine 中与离线/session compaction 相关的实现；
- `atomcode-coding` 的其他直接 core 依赖。

因此，本切片达到：

```text
逻辑已实现                 是
TUI 手动 compact driver 已切换 是
core Compact 命令 variant 已退役 是
跨 driver 的 /compact 已退役    否
compaction 整体接口面已退役      否
bridge fallback 已删除         否
```

### 14.6 验证矩阵

至少验证：

1. `CodingRuntimeHandle::compact` 产生正确 kernel command；
2. handle channel 关闭时返回错误；
3. bridge 从稳定 control receiver 投递到当前 kernel handle；
4. TUI 新建、后台化、恢复 runtime 时 native handle 与 legacy client 同步切换；
5. `/compact [focus]` 不再构造 core `AgentCommand`；
6. 全仓搜索不存在 legacy `AgentCommand::Compact/CoreCmd::Compact`；
7. `atomcode-coding`、`atomcode-bridge`、`atomcode-tuix`、`atomcode-daemon`
   受影响测试通过；
8. 实际可行时运行更广 workspace check。

## 15. 首个切片实施结果

### 15.1 已实现

- 新增 `atomcode_coding::runtime::CodingRuntimeHandle`；
- `compact(focus)` 直接构造 kernel `AgentCommand::Compact`；
- bridge 持有单一 control receiver，并把请求转发给当前 kernel sender；
- legacy client 或 native handle 任一仍存活时，bridge owner 不会因另一通道关闭而提前退出；
- bridge degraded keep-alive 同时消费 legacy/native 控制面并给出错误事件；
- TUI 使用 `RuntimeEndpoint { legacy, native }` 绑定两条过渡命令面；
- `/bg`、`/background`、`/bg resume`、drop 和新 runtime spawn 同步保存/恢复 endpoint；
- TUI `/compact [focus]` 已切换到 native runtime handle，并显式显示 runtime 不可用错误。

### 15.2 已删除的 legacy surface

- core `AgentCommand::Compact` variant；
- TUI 对 core compact variant 的发送点；
- bridge `CoreCmd::Compact` handler；
- daemon kernel translator 的 `CoreCmd::Compact` 分支；
- daemon 中只验证该旧映射的测试断言。

全仓搜索已确认不再存在 `CoreCmd::Compact`，也不存在带 legacy `prompt` 字段的
`AgentCommand::Compact` 发送点。clix、kernel 测试和新 runtime handle 保留的
`AgentCommand::Compact { focus }` 均为 kernel 原生命令，不属于 legacy surface。

### 15.3 仍可达的 legacy surface

本切片尚未使 compaction 整体退役，仍保留：

- core `AgentEvent::CompactionUi` 与 `CompactionUiKind`；
- bridge/daemon 的 `CompactionStarted/Compacted → CompactionUi` 转换；
- TUI 对 core `CompactionUi` 的消费；
- daemon `commands.rs` 的离线 session compact；
- 其他 slash 命令的 bridge/core command 路径；
- `atomcode-coding` 中 rate-limit 和 vision 判断的直接 core 依赖；
- bridge fallback 本身。

当前迁移状态为：`/compact` 的 TUI driver 已切换，core
`AgentCommand::Compact` **协议 variant 已退役**；但跨 driver 的 `/compact` 功能尚未
退役。WebUI `/compact` 已核实通过 `execServerCommand` 进入 daemon `commands.rs` 的离线
session 压缩，不经过已删除的 daemon translator 分支。compaction 的 legacy **事件面、
WebUI 离线 session 路径和 bridge fallback 均尚未退役**。

### 15.4 验证结果

- `cargo test -p atomcode-coding runtime::tests`：2 passed；
- `cargo test -p atomcode-bridge runtime_control_tests`：1 passed；
- `cargo test -p atomcode-tuix resume_restores_the_native_handle_for_that_runtime`：1 passed；
- `cargo test -p atomcode-daemon shutdown_maps_directly`：1 passed；
- `cargo test -p atomcode-core --lib`：1555 passed，1 ignored；
- `cargo test -p atomcode-kernel --test compaction`：13 passed；
- `cargo check -p atomcode-coding -p atomcode-bridge -p atomcode-tuix \
  -p atomcode-daemon -p atomcode`：通过。

仓库当前全量 `cargo fmt --all -- --check` 会报告大量与本切片无关的既有格式差异，
因此没有执行会重写全仓的格式化；新增 `runtime.rs` 已单文件 rustfmt。

### 15.5 唯一下一步

建立 `CodingRuntime` 的 core-free compaction 事件模型，并通过当前唯一 runtime owner
输出一条严格有序的过渡事件流。不能让 TUI 与 bridge 竞争读取 kernel event receiver，
也不能简单增加互相独立的 legacy/native receiver；后者会让
`CompactionStarted → Compacted → ContextStats → 后续输出` 因异步转发而乱序。

第二切片先把 TUI、CLI headless、daemon bridge path 和 daemon kernel path 全部切到
native compaction 事件，再删除 core `CompactionUi`、bridge/daemon 对应转换以及重复的
格式化逻辑。daemon 离线 compact 仍作为后续 session 所有权切片单独处理。

## 16. 第二个垂直切片：compaction native 事件面

### 16.1 目标与非目标

本切片目标是退役 compaction 的 core legacy **事件 variant**，不是一次性切换全部
runtime 事件，也不是删除整个 bridge。

目标：

1. 定义不依赖 `atomcode-core` 的 compaction runtime 事件；
2. 保持 kernel event receiver 单一所有者；
3. 通过一条有序过渡事件流同时承载尚未迁移的 core 事件和已迁移的 native 事件；
4. 切换 TUI、CLI headless、daemon bridge path 和 daemon kernel path；
5. 删除 core `CompactionUi` 及所有生产者、消费者、转换和旧测试；
6. 保持 compaction 后 context usage 立即刷新的现有语义。

非目标：

- 不迁移其他 core `AgentEvent`；
- 不改变 kernel compaction 算法、阈值或锚定压缩实现；
- 不迁移 daemon `commands.rs` 的离线 session `/compact`；
- 不迁移 session/provider/cd/resume/approval 生命周期；
- 不删除 daemon bridge fallback；
- 不顺带清除 `atomcode-coding` 的全部 core 依赖。

### 16.2 当前事件调用链

当前本地 TUI 路径：

```text
kernel CompactionStarted / Compacted
  → bridge 读取唯一 kernel event receiver
  → core CompactionUi(Begin / End / Mark)
  → TUI spinner / marker
```

daemon 有两条实现不同、输出相同的路径：

```text
daemon bridge path
  → atomcode-bridge
  → core CompactionUi
  → daemon live/chat

daemon kernel path
  → KernelToWebui
  → core CompactionUi
  → daemon live/chat
```

bridge 和 daemon kernel translator 还各自重复维护：

```text
last_usage
estimate_after_tokens
compaction_mark_label
manual_noop_result
fmt_k_tokens
```

CLI headless 只消费 `CompactionUi::Mark` 并向 stderr 输出 committed marker。TUI 的
background runtime 当前不缓存 compaction UI，最终依靠 terminal snapshot 得到压缩后的
conversation。WebUI `/compact` 则通过 server command 进入 daemon 离线 session 压缩，
不经过已迁移的 TUI runtime control，也不经过已删除的 core Compact command variant。

### 16.3 必须保持的所有权与顺序

以下约束是本切片的硬不变量：

1. 每个 live agent 的 kernel event receiver 只能有一个消费者；
2. 当前 bridge 或 daemon kernel driver 继续作为临时 runtime owner；
3. TUI/CLI/daemon 不直接与 owner 竞争读取 kernel receiver；
4. 同一个 runtime 的 legacy/native 事件必须进入同一个有序输出 channel；
5. runtime respawn 后 driver 仍持有同一稳定事件流，不缓存旧 kernel receiver；
6. 不允许 bridge runtime 与 native runtime 各自启动一套 Agent。

必须保持的可观察顺序是：

```text
CompactionStarted
CompactionFinished
ContextStats（committed 时）
后续 TextDelta / ToolStarted / TurnComplete
```

不采用以下双 receiver 结构：

```text
legacy_event_rx ── async forwarder ─┐
                                    ├─ driver fan-in
native_event_rx ── async forwarder ─┘
```

两个异步 forwarder 无法保证跨 channel 的相对顺序，可能出现模型已经继续输出，TUI
才开始显示“正在压缩”的回归。为少改接口而接受乱序不符合本切片的 parity 要求。

### 16.4 core-free 事件模型

在 `atomcode-coding::runtime` 中新增中立事件类型：

```rust
#[non_exhaustive]
pub enum CodingRuntimeEvent {
    CompactionStarted {
        trigger: CompactTrigger,
    },
    CompactionFinished {
        outcome: CompactionOutcome,
    },
}

pub struct CompactionOutcome {
    pub trigger: CompactTrigger,
    pub epoch: u64,
    pub removed_messages: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub committed: bool,
    pub estimated_tokens_before: usize,
    pub estimated_tokens_after: usize,
}
```

公共事件只表达 runtime 事实，不携带：

- core `AgentEvent` 或 `CompactionUiKind`；
- 已本地化的 UI 字符串；
- TUI spinner、phase 或 renderer 状态；
- daemon `TurnEvent`/SSE 类型。

`CompactionStarted` 保持 kernel 现有语义：只有 strategy 将执行较慢摘要时才产生；stub
fold 和无需摘要的 no-op 不产生 Started。`CompactionFinished` 对 committed、refused 和
no-op 均产生，因此消费者必须能在没有先收到 Started 的情况下独立处理 Finished。

`CompactionOutcome` 的 token 数是估算值：优先使用最近一次真实 provider usage 作为
before，再按 kernel 报告的 byte ratio 估算 after；没有 usage 时回退为 `bytes / 4`。
字段名必须显式包含 `estimated`，不能把估算值伪装成精确 kernel token 统计。

### 16.5 展示文本与 runtime 事件分离

bridge 和 daemon 当前各有一套完全重复的本地化与 token 格式化函数。本切片不应把
这些字符串复制到 TUI、CLI 和 daemon 第三次。

推荐在 `atomcode-config::i18n` 提供接受纯数值的共享展示函数：

```rust
format_compaction_mark(
    removed_messages,
    estimated_tokens_before,
    estimated_tokens_after,
)

format_compaction_noop(
    estimated_tokens_before,
    estimated_tokens_after,
    summary_would_grow,
)
```

`atomcode-config` 不依赖 coding/kernel 类型；driver 从 `CompactionOutcome` 取数后调用
格式化函数。这样同时满足：

- runtime 事件保持 UI-neutral；
- 本地化逻辑只有一个实现；
- bridge/daemon 重复 helper 可以实际删除；
- 未来物理拆分 runtime 时不会携带 TUI/daemon 协议。

### 16.6 bridge 的有序过渡事件流

bridge 增加明确标记为临时的输出 envelope：

```rust
pub enum BridgedRuntimeEvent {
    Legacy(atomcode_core::agent::AgentEvent),
    Native(atomcode_coding::runtime::CodingRuntimeEvent),
}
```

`Bridge` 内部只保留一个输出 sender：

```rust
fn emit_legacy(&self, event: CoreEv);
fn emit_native(&self, event: CodingRuntimeEvent);
```

尚未迁移的事件继续包装为 `Legacy`；kernel `CompactionStarted/Compacted` 改为产生
`Native`。因为两者在 owner 的同一个 select loop 中发送到同一个 channel，顺序与
kernel 观察顺序一致。

committed compaction 的 owner 处理顺序为：

1. 读取修改前的 `last_usage`；
2. 构造 `CompactionOutcome`；
3. 更新缓存的 `last_usage.used_tokens`；
4. 发送 native `CompactionFinished`；
5. 发送现有 legacy `ContextStats`。

`ContextStats` 本切片不迁移，但必须保持 marker 先于刷新后的 context stats，和当前
`CompactionUi::Mark → ContextStats` 行为一致。

旧 tuple 形式 `spawn_bridged_runtime` 目前只被 daemon 使用。daemon 两个调用点切换到
新输出后，应删除该兼容 wrapper，不保留一个会丢弃 native 事件或把 native 重新投影成
core `CompactionUi` 的 fallback。

### 16.7 TUI 适配

TUI 定义本地 driver envelope：

```rust
pub enum RuntimeEventPayload {
    Legacy(AgentEvent),
    Native(CodingRuntimeEvent),
}

pub struct RuntimeEvent {
    pub runtime_id: RuntimeId,
    pub event: RuntimeEventPayload,
}
```

建议把初始 runtime 与 `/session`、`/bg`、disk `/resume` 创建的 runtime 统一成：

```rust
pub struct SpawnedRuntime {
    pub endpoint: RuntimeEndpoint,
    pub event_rx: UnboundedReceiver<RuntimeEventPayload>,
}
```

避免初始路径继续使用 `core AgentHandle + CodingRuntimeHandle`，而后续 spawn 使用另一种
结构。cloneable command endpoint 与 single-consumer event receiver 仍必须分开持有。

native compaction 处理规则：

| 事件 | TUI 行为 |
|---|---|
| Started | `compacting=true`；Idle 手动压缩临时进入 Streaming；turn 中自动压缩保持原 phase |
| Finished + committed | 清 spinner，渲染 `CompactionMark`，必要时恢复 Idle |
| Finished + manual no-op | 清 spinner，通过共享文本渲染 helper 展示“无需压缩” |
| Finished + auto/overflow no-op | 只清 spinner，不输出提示 |

manual no-op 不应继续让 bridge 伪造 core `TextDelta`。TUI 可把普通文本渲染分支抽成
内部 helper，由 core TextDelta 和 native manual-noop 共同调用，保持 `/copy`、最后回复
缓存和可见文本状态的现有行为，但不重新构造 legacy event。

background runtime 保持当前策略：不缓存 compaction UI；terminal snapshot 负责保存
压缩后的 conversation。必须覆盖两种边界：

1. Started 在 foreground，随后 `/bg`：新 foreground 状态不能继承旧 spinner；
2. Started 在 background 被忽略，Finished 在 `/bg resume` 后到达：即使未见 Started，
   Finished 也能安全清理状态并展示 committed marker。

### 16.8 CLI headless 适配

CLI 不再把 bridge runtime 重新包装为只支持 core event 的 `atomcode_core::agent::AgentHandle`。
headless event loop 直接消费 bridge 的有序 envelope：

- `Legacy`：继续执行现有逻辑；
- native committed compaction：向 stderr 输出 `[compact] <label>`；
- native manual no-op：复用普通文本输出 helper；
- Started 和 auto/overflow no-op：headless 无 spinner，保持静默。

普通 send、approval、cancel、shutdown 等命令在本切片仍可通过 legacy `AgentClient`；
停止使用 core `AgentHandle` 容器不等于这些命令已经退役。

### 16.9 daemon 两条路径适配

daemon bridge path 与 kernel path 必须收敛到同一种 daemon 本地输入：

```rust
enum DaemonRuntimeEvent {
    Legacy(AgentEvent),
    Native(CodingRuntimeEvent),
}
```

bridge adapter 从 `BridgedRuntimeEvent` 顺序映射到该类型；daemon kernel driver 直接产生
该类型。不能让两个 engine 分支返回不同 receiver 后在 live/chat 循环里复制整套 match。

native compaction 到 daemon `TurnEvent` 的映射：

| 事件 | daemon 行为 |
|---|---|
| Started | 当前 SSE/WebUI 无 spinner，忽略 |
| Finished + committed | `TurnEvent::Warning(localized_mark)`，保持现有 wire 行为 |
| Finished + manual no-op | `TurnEvent::TextDelta(localized_noop)` |
| Finished + auto/overflow no-op | 静默 |

必须同时切换：

- persistent `/live` `KernelTurnExecutor`；
- `/chat` 的 `run_chat_turn_v2`；
- daemon bridge fallback；
- daemon opt-in kernel driver；
- `agent_to_turn` 及其测试。

live-sync 当前会把 daemon committed marker 作为 Warning 传给远端视图，本切片保持 wire
兼容，不引入新的 core `TurnEvent::Compaction`。跨进程 native runtime event 属于未来
daemon driver 协议切片。

### 16.10 实际删除清单

本切片完成时必须实际删除：

- `atomcode_core::agent::CompactionUiKind`；
- `atomcode_core::agent::AgentEvent::CompactionUi`；
- bridge 的 `CompactionStarted/Compacted → CompactionUi` 转换；
- daemon kernel translator 的相同转换；
- TUI 对 core `CompactionUi` 的 handler 和状态注释；
- CLI headless 对 core `CompactionUi` 的 match arm；
- daemon live/chat 对 core `CompactionUi` 的 match arm；
- `agent_to_turn` 的 core compaction 分支和旧测试；
- bridge/daemon 重复的 compaction label/no-op/token helper；
- 只验证 core `CompactionUi` 转换的测试；
- 已无调用方的旧 `spawn_bridged_runtime` tuple wrapper。

不得只新增 native 事件、同时保留上述旧生产点和消费者；那只能称为双路径，不能称为
compaction legacy 事件面退役。

### 16.11 完成后仍可达的 legacy surface

即使本切片全部完成，以下部分仍明确“尚未退役”：

- TUI、CLI、daemon 的其他 core `AgentEvent`；
- bridge 的其他 kernel → core 事件转换；
- compaction 后仍经 core 投递的 `ContextStats`；
- daemon 默认 bridge fallback；
- daemon `commands.rs` 的离线 session `/compact`；
- daemon kernel runtime 对 `BridgeConfig`、core command 和 bridge helper 的依赖；
- session/provider/cd/resume/approval/goal/loop 生命周期；
- `atomcode-coding` 中 rate-limit 和 vision 判断等其他直接 core 依赖。

完成后的迁移状态应报告为：

```text
kernel compaction 逻辑已实现                 是
TUI manual compact 命令 driver 已切换         是
compaction native 事件 driver 已切换           是
core CompactionUi event variant 已退役         是
跨 driver /compact 整体接口面已退役            否
bridge fallback 已删除                         否
```

### 16.12 难度与影响范围

本切片难度评估为 **4/5**。难点不是新增 enum，而是：

- 保证 legacy/native 事件严格有序；
- TUI 多 runtime、background 和 resume；
- daemon bridge/kernel 两条路径 parity；
- 保留 compaction 后 context usage 的即时刷新；
- manual no-op 不再借用 core TextDelta；
- CLI/TUI/daemon 共用本地化展示而不把字符串塞入 runtime event。

预计影响：

```text
atomcode-coding
atomcode-config
atomcode-bridge
atomcode-core
atomcode-cli
atomcode-tuix
atomcode-daemon
```

不影响 kernel compaction strategy、anchor 算法和 WebUI 离线压缩实现。

### 16.13 实施顺序

建议按以下顺序实现，每一步保持可编译但不把中间态称为已退役：

1. 在 `atomcode-coding::runtime` 增加事件与 outcome 计算测试；
2. 在 `atomcode-config::i18n` 收敛 compaction 展示 helper；
3. bridge 输出有序 `Legacy/Native` envelope；
4. CLI/TUI 切换 envelope 和 native compaction handler；
5. daemon bridge/kernel/live/chat 全路径切换；
6. 删除 core `CompactionUi` 及所有旧转换、helper 和测试；
7. 全仓搜索和针对性测试；
8. 运行实际可行的最广 workspace check。

### 16.14 验证矩阵

runtime/domain：

- usage 存在时的 before/after token 估算；
- usage 缺失时 `bytes / 4` fallback；
- `bytes_before == 0`；
- committed drain、committed stub、manual no-op、auto no-op、overflow no-op；
- outcome 保留 trigger、epoch 和 exact byte/message 数据。

bridge/order：

- Started → Finished → ContextStats 顺序；
- Finished 后的 TextDelta/ToolStarted 不越过 native 事件；
- respawn/reload 后稳定输出 receiver 仍有效；
- legacy sender 或 native handle 单独存活时 owner 生命周期不提前结束。

TUI：

- Idle 手动 compact spinner；
- turn 中自动 compact 不提前结束 Streaming；
- manual no-op 文本、committed marker 和自动 no-op 静默；
- `/bg`、`/background`、`/bg resume`；
- Started/Finished 跨 foreground/background 切换；
- plain/retained 两种 renderer；
- compaction 后 `/context` 读取刷新后的统计。

CLI/daemon：

- headless committed marker；
- daemon bridge 与 kernel path 输出一致；
- `/live` 与 `/chat` 映射一致；
- approval、cancel、provider reload、working-dir reload 和 terminal snapshot 的 legacy
  事件仍透明通过有序 envelope；
- WebUI 离线 `/compact` 不受影响。

### 16.15 实施结果（release/v5.0.0）

本切片已按本节设计落地。实施基线为
`release/v5.0.0@0b94fa8a1a7fcb585f43456564b0b98b67707e86`。

实际完成：

- `atomcode-coding::runtime` 现在拥有 driver-neutral 的
  `CodingRuntimeEvent` 与 `CompactionOutcome`；
- bridge、TUI 和 daemon 分别使用单 channel 的有序 `Legacy/Native` envelope；
- CLI headless、TUI foreground/background、daemon `/live`、`/chat`、bridge fallback
  和 daemon kernel path 均已切换 native compaction event；
- committed compaction 后仍按 `CompactionFinished → ContextStats` 的顺序立即刷新 usage；
- compaction 展示文案统一由 `atomcode-config::i18n` 格式化；
- 已删除 core `CompactionUiKind`、`AgentEvent::CompactionUi`、所有生产/消费分支、
  重复 helper、旧测试以及旧 tuple 版 `spawn_bridged_runtime`。

迁移状态：

```text
kernel compaction 逻辑已实现                 是
TUI manual compact 命令 driver 已切换         是
compaction native 事件 driver 已切换           是
core CompactionUi event variant 已退役         是
跨 driver /compact 整体接口面已退役            否
bridge fallback 已删除                         否
```

仍可达的 legacy surface 与 16.11 一致。该结果只完成了 compaction 事件面的迁移，
不能据此转去 `/context`：`/compact` 控制仍由 bridge 转发、结果仍由 bridge 消费，
daemon 离线 `/compact` 仍使用 core compression，因此 `/compact` 整体尚未退役。

退役验收：

```text
rg "CompactionUi|CompactionUiKind" crates
```

结果必须为空；同时检查旧 bridge/daemon helper、旧 spawn wrapper 和只验证 legacy
转换的测试均已删除。之后依次运行受影响 crate 的针对性测试和实际可行的最广
workspace check。

### 16.16 第二切片验收口径与后续唯一下一步

第二切片只有在 core variant、生产者、消费者和转换全部删除后，才能称为
“compaction legacy 事件面已退役”。若只是 driver 能收到 native 事件但旧路径仍可达，
必须报告为“driver 已切换、legacy fallback 仍保留”。

完成第二切片后的唯一下一步是继续完成 `/compact`，而不是迁移 `/context`。只有控制、
事件、compaction 后统计刷新和 daemon 离线执行都不再经过 bridge/core compression，
才能把该 slash 命令标记为退役。

## 17. 第三切片：`/compact` 完整脱离 bridge

### 17.1 当前基线与目标状态

设计复核基线为 `release/v5.0.0@cfda1c319fe3f34406fafba992019009ed65ab56`。
当前工作树无未提交改动，分支相对远端 ahead 1 / behind 5；本切片不得顺带拉取、合并、
提交或推送。

当前调用链为：

```text
TUI /compact
  -> CodingRuntimeHandle
  -> bridge CodingRuntimeControlReceiver
  -> bridge 当前 AgentHandle.commands
  -> kernel Compact
  -> bridge 当前 AgentHandle.events
  -> bridge CompactionStarted/Compacted 转换
  -> driver native event
```

因此第二切片后的准确状态是：逻辑已实现、driver 已切换、core `CompactionUi` 已退役，
但 bridge fallback 仍可达，`/compact` 整体尚未退役。

第三切片目标是让 `atomcode-coding` runtime 成为当前 kernel `AgentHandle` 的唯一所有者：

```text
driver CodingRuntimeHandle -- Compact --> coding runtime owner --> kernel
kernel -- CompactionStarted/Compacted --> coding runtime owner --> native driver receiver
kernel -- other events --> coding runtime adapter --> bridge --> legacy driver receiver
bridge -- replace/stop/shutdown --> coding runtime owner --> current kernel AgentHandle
```

这不是把整个 bridge 一次搬进 coding。bridge 仍负责尚未迁移的 legacy 命令、事件转换及
goal/loop/session 协调，但不再接触 compaction 控制或 compaction kernel event。

### 17.2 所有权与中间态约束

- coding runtime owner 独占 kernel event receiver，避免 bridge 与 runtime 竞争消费；
- driver 持有稳定 `CodingRuntimeHandle`，provider/session respawn 后仍指向 owner 当前 agent；
- bridge 通过内部 adapter 发送其他 kernel 命令、接收过滤后的非 compaction 事件；
- handle 替换必须经 `replace_agent`，停止必须经 `stop_agent`，最终退出经 `shutdown`；
- owner 必须保留 session id、working directory、snapshot、provider、approval 和 gateway
  affinity 的现有 respawn 时序；本切片不改变这些策略，只改变 AgentHandle 所有权；
- native compaction receiver 与 legacy receiver 分离，driver 在本地合流；不得重新包装成
  core event，也不得由 bridge 重新转发 native event；
- committed compaction 不再触发 bridge `ContextStats`。`/context` 本身的 legacy 查询路径
  保留，本切片不迁移它；compaction outcome 已携带展示所需的 before/after 数据。

### 17.3 本切片预计删除的 legacy surface

- bridge 的 `CodingRuntimeControlReceiver` select 分支和 `forward_runtime_control`；
- bridge 的 `on_runtime_control`；
- bridge `on_kernel_event` 中 `CompactionStarted`、`Compacted` 两个分支；
- bridge compaction 后调用 `emit_context_stats()` 的路径；
- bridge 只验证 compact 转发的测试；
- `BridgedRuntimeEvent::Native` 及 bridge 对 native compaction 的生产职责；
- daemon bridge fallback 对 `BridgedRuntimeEvent::Native` 的依赖；
- daemon 离线 `/compact` 对 `atomcode_core::agent::compression` 的调用。

不会删除的一般 legacy surface：core `ContextStats`、其他 core command/event、bridge 的
session/provider/cd/resume/approval/goal/loop handler，以及 daemon kernel path 为兼容现有
WebUI wire 所做的其他 core event 转换。

### 17.4 daemon 离线 `/compact`

`POST /command` 的 `/compact` 是同一个用户命令的另一入口，不能留到 `/context` 或笼统的
session 切片后仍宣称完成。它应复用 kernel `Conversation`、`CompactionStrategy::plan` 和
`Conversation::apply_plan`，由 kernel 保持 sacred floor、net-loss guard、cache epoch 与
tool pair 不变量；持久化层仅负责 core session message 的边界转换和 display/turn anchor
重建。不得继续调用 core `compression_plan/run_llm_summary/try_apply_compression`。

### 17.5 验收状态

本切片只有同时满足以下条件才完成：

```text
kernel compaction 逻辑已实现                  是
CLI/TUI/daemon compact driver 已切换           是
compact 控制经过 bridge                       否
compact kernel event 经过 bridge              否
compact 后由 bridge 发送 ContextStats          否
daemon 离线 compact 使用 core compression      否
core Compact/CompactionUi variant 可达          否
其他 slash 命令的 bridge fallback              允许保留
```

若 daemon 离线路径或任一 driver 仍可达旧实现，最终说明必须写“`/compact` 尚未退役”，
不能因为交互式 TUI 已工作而标记完成。

### 17.6 实施结果

本切片已在上述基线落地：

- `atomcode-coding::runtime` 现在持有当前 kernel `AgentHandle`，稳定 handle 在
  provider/session agent replacement 后仍直接命中当前 agent；
- runtime owner 独占 kernel event receiver，compaction started/finished 直接进入 native
  receiver，其他事件才进入 `KernelRuntimeAdapter`；
- bridge 不再接收 runtime control，不再匹配 compaction kernel event，也不再因 compaction
  发送 core `ContextStats`；
- CLI/TUI/daemon bridge fallback 分别接收 legacy 与 native channel，并只在 driver 本地合流；
- daemon kernel driver 的 compaction 结果不再附带 legacy `ContextStats`；
- daemon 离线 `/compact` 已改用 v2 `OverflowCompaction` 生成 plan，并由 kernel
  `Conversation::apply_plan` 执行；core compression 调用已删除；
- daemon 本地 provider adapter 仅承担持久化命令边界的 core provider stream → kernel
  provider stream 映射，不经过 bridge runtime、`AgentClient` 或 bridge handler。

本切片实际删除：bridge control select/forwarder、bridge compaction event handler、bridge
compaction 后 stats 发送、`BridgedRuntimeEvent::Native`、对应 bridge 转发测试，以及 daemon
离线命令的 core compression 路径。

完成状态：

```text
kernel compaction 逻辑已实现                  是
CLI/TUI/daemon compact driver 已切换           是
compact 控制经过 bridge                       否
compact kernel event 经过 bridge              否
compact 后由 bridge/daemon 发送 ContextStats   否
daemon 离线 compact 使用 core compression      否
core Compact/CompactionUi variant 可达          否
/compact legacy 接口面已退役                   是
其他 slash 命令的 bridge fallback              仍保留
```

下一步应先验证本切片的最广测试与实际交互，再选择下一个 slash 命令；不得把仍保留的一般
core event、session/provider/goal/loop bridge surface 计入 `/compact` 的完成度，也不得据此
宣称整个 bridge 已退役。

### 17.7 合并后 review 加固

2026-07-14 在 `release/v5.0.0@9d649c2340e7f18fdb84fe15f9619dc39f3cad29`
及其未提交迁移 diff 上重新复核后，第三切片增加以下限定修复，范围仍只覆盖
`/compact`：

- daemon 离线压缩不再把全部 session message 经过 lossy core/kernel 双向转换；runtime
  返回 `Noop`、`RewriteOnly` 或精确 `Replace` mutation，持久化层复用所有 surviving core
  message，只把 kernel 新生成的 summary/note 转回 core。`ToolResultRef` 的 hash/byte_size、
  Claude thinking signature 和其他 core-only 字段因此不会因 `/compact` 丢失；
- UI anchor 只在 `Replace` mutation 下按真实 old/new span 重排；多个不连续的原位 rewrite
  不再被误判成一个大 replacement，从而不会删除中间的 display message 或 turn stat；
- runtime owner 在 respawn/provider reload 前暂停 compaction control，replacement 成功后才
  恢复；stop/degraded 状态会拒绝新 compact，失败路径不再由 noop agent 伪造
  `committed=false` 结果；
- CLI、TUI 和 daemon bridge fallback 各自用一个有偏序 merger task 合流 legacy/native
  receiver；background consumer 同时保证晚到的 manual finish 不能把 Done/Cancelled/Error
  降级为 Idle；
- committed outcome 在 TUI 内建立临时 post-compaction usage projection。旧
  `RefreshContextStats` 返回的 pre-compaction sent tokens 在下一次真实 TokenUsage 前不会覆盖
  它。该逻辑只是 `/compact` 的兼容投影，`/context` command/event 协议仍明确尚未迁移。

本切片保证 native compaction 自身的 Started/Finished 顺序，并加固 compact 与 terminal 的
消费顺序；它没有引入全局 sequence id，也不宣称所有 legacy/native runtime event 已形成
统一的严格总序。全局混合事件协议、`/context` 和其他 slash 命令仍属于后续切片。

加固后的迁移判定不变：`/compact` 专用 core variant、bridge compact handler、core
compression fallback 已退役；一般 core `ContextStats`、bridge session/provider/goal/loop
surface 仍可达，不能据此宣称整个 core/bridge 已退役。core compression 模块中的
`run_llm_summary` 目前仍被 bridge 的 AI session naming 复用，但它已不在 `/compact` 调用链，
因此既不能算 `/compact` fallback，也不能宣称整个 compression 模块已经删除。

### 17.8 runtime 重建期间的 compact 终态

第二轮 review 发现，旧 owner 在 `Stop/Replace` 中只等待 kernel task，不继续消费旧
`AgentHandle.events`。若 driver 已收到 `CompactionStarted`，旧 agent 随后产生的
`Compacted` 会随 receiver 一起被丢弃，TUI 可能永久停在 compacting/Streaming。

本切片将 compact 生命周期约束补充为：runtime 接受的每个请求必须产生且只产生一个
`CompactionFinished`。终态分为正常 `Completed(CompactionOutcome)` 和
`Interrupted { trigger, reason }`；runtime 重建或停止不得伪装成 `committed=false` 的
“无需压缩”。具体边界如下：

- owner 记录已投递的 manual compact 和已开始的 auto/overflow compact；
- Stop/Replace 等待旧 task 时继续抽取 compact event，task 结束后再次排空 receiver；
- kernel 未产生 `Compacted` 时，owner 为剩余请求产生一次 `Interrupted`，随后清空旧状态；
- control 携带 runtime generation。suspend 立即关闭可用性并推进 generation，旧 generation
  请求只能结束为 `Interrupted`，不得在 replacement agent 或新 session 上延迟执行；
- provider reload 必须先 suspend compact，再 settle turn、assemble、replace、resume；
- CLI、TUI、daemon 对 Interrupted 只显示中断结果，不能输出成功 marker 或“无需压缩”；
  TUI 同时清除 compacting 和由 compact 强制进入的 Streaming 状态。

该修复只扩展 `atomcode-coding` 的 native compact terminal，不恢复 core
`Compact/CompactionUi`，不新增 bridge compact handler，也不改变 kernel compaction command、
capability strategy、`/context` 或其他 slash 命令协议。
