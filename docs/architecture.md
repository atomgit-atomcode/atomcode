# AtomCode Architecture

本文描述当前生产架构及其依赖边界。历史迁移方案和已经完成的 bridge/core
退役过程不属于当前架构；如需了解迁移背景，应查阅相关 Git 历史和归档文档。

## 总体调用链

coding agent 的统一生产调用链为：

```text
CLI / TUI / daemon / background / ACP / clix code
                    │
                    ▼
       CodingRuntimeHandle / DriverCommand
                    │
                    ▼
               CodingRuntime
                    │
                    ▼
          atomcode-kernel Agent
```

- driver 负责输入、展示、传输以及明确的本地操作；
- `CodingRuntime` 是 coding agent 的唯一运行时所有者；
- `atomcode-kernel` 只负责中立的 agent 循环；
- `atomcode-capabilities` 提供 provider、工具、MCP、session 等可复用能力；
- provider、session、working directory、goal、loop、审批和 generation 等 coding
  生命周期状态不得由 driver 或 kernel 另建第二份所有权。

## 分层与依赖方向

```text
L3  drivers / services
    atomcode-cli  atomcode-tuix  atomcode-daemon  atomcode-clix  ACP
                              │
                              ▼
L2                     atomcode-coding
                  runtime owner + coding assembly
                       │                 │
                       ▼                 ▼
L1            atomcode-capabilities   atomcode-config/auth/...
              providers/tools/session
                       │
                       ▼
L0                     atomcode-kernel
                  neutral agent contracts
```

依赖只能从上层指向下层：

- `atomcode-kernel` 不得依赖 coding 产品语义、具体 provider、具体工具或 UI；
- `atomcode-capabilities` 可以依赖 kernel，但不得反向依赖 `atomcode-coding`、
  driver 或已退役的 core；
- `atomcode-coding` 组装 kernel 与 capabilities，并拥有 coding 生命周期；
- driver 通过 `CodingRuntimeHandle` 和 `DriverCommand` 驱动运行时，不得重建 live
  agent 生命周期；
- 独立业务 agent（例如 review）可以直接组装其所需的 kernel/capabilities，
  但不得复制 coding runtime 的状态所有权。

## 主要 crate

| Crate | 层级 | 职责 |
|---|---:|---|
| `atomcode-kernel` | L0 | 中立 Agent、`AgentCommand`/`AgentEvent`、message、provider/tool trait、middleware、hook 与 request 边界 |
| `atomcode-capabilities` | L1 | 具体 provider、文件与 shell 工具、MCP、skills、plugin、memory、session persistence、compaction 等可复用能力 |
| `atomcode-coding` | L2 | coding persona、能力装配、`CodingRuntime`、provider/session/controller、goal/loop、team/subagent 与产品执行策略 |
| `atomcode-config` | leaf | 配置模型、加载与产品配置策略 |
| `atomcode-auth` | leaf | 登录、OAuth 与凭据生命周期 |
| `atomcode-cli` | L3 | 可执行程序入口、参数解析、headless/TUI/ACP 等入口协调 |
| `atomcode-tuix` | L3 | retained-mode 终端 UI、事件循环、modal、命令与 runtime 事件投影 |
| `atomcode-daemon` | L3 | HTTP/WebUI/live hub、headless runtime 接入及历史 session 单向导入 |
| `atomcode-clix` | L3 | 独立 coding CLI driver |
| `atomcode-review` | L2/L3 | 基于 kernel + capabilities 的独立代码审查 agent |
| `atomcode-telemetry` | service | 遥测事件、配置和上报 |
| `atomcode-updater` | service | 安装包与版本更新能力 |
| `atomcode-codingplan` | capability | coding plan 相关能力；可选 crypto overlay 由发布构建注入 |

## atomcode-kernel：中立执行边界

kernel 定义可复用的 agent 执行协议，而不是 AtomCode 产品运行时：

- `Agent` 执行模型循环；
- `AgentCommand` / `AgentEvent` 构成中立命令与事件边界；
- `LlmProvider`、`Tool`、`ToolMiddleware`、hooks 和 request 是扩展 seam；
- message、stream、checkpoint 等类型不包含 UI 或 coding 产品所有权；
- kernel 不负责 provider 选择、session 切换、cwd、goal、loop 或持久化目标。

新增产品行为前应先判断它是否真是所有 agent 都需要的中立机制。coding 专属行为应
留在 `atomcode-coding`，具体实现应优先放在 `atomcode-capabilities`。

## atomcode-capabilities：可复用能力

capabilities 将具体能力实现挂载到 kernel seam，包括：

- OpenAI-compatible、Anthropic、Ollama 等 provider；
- read/write/edit/bash/grep/glob、code intelligence 与 Web 工具；
- MCP transport、registry、OAuth 和工具适配；
- skills、plugin、memory、通知和外部 hooks；
- compaction 策略；
- native session snapshot、append-only transcript、catalog、lease 与 recall。

能力通过 Cargo feature 按需启用。该 crate 必须保持 core-free，也不得依赖 L2/L3。

## atomcode-coding：运行时所有者

`atomcode-coding` 将中立 kernel 和具体 capabilities 组装为完整 coding agent：

- `prepare` / `assemble` 构建 provider、工具、MCP、skills、session hooks 等 parts；
- `CodingRuntime` 持有 live `AgentHandle`、配置、parts、provider、session binding、
  generation、controller、pending request 和 snapshot broker；
- `CodingRuntimeHandle` 是 driver 的控制句柄；
- `DriverCommand` 表达 submit、steer、cancel、approval、compact、provider reload、
  session/working-directory 变更、goal/loop 等操作；
- `CodingRuntimeEvent` 和明确终态将运行结果投影给各 driver。

runtime 重建或切换时必须保持 session、cwd、provider、审批、gateway affinity 和持久化
目标的一致性。旧 generation 的迟到事件不得污染 replacement runtime；失败不得静默
降级为 fresh session、空 snapshot、noop handle 或假成功。

## Driver 与服务边界

### CLI / TUI

`atomcode-cli` 负责进程入口和模式选择；`atomcode-tuix` 负责终端交互与展示。TUI
消费 runtime event 并产生 driver command，不拥有第二套 agent、provider 或 session
状态机。

### daemon / WebUI

`atomcode-daemon` 通过 `CodingRuntime` 提供 headless chat，并通过 live hub 复用 TUI
附加的运行时。WebUI/HTTP DTO 是传输和展示投影，不是运行时状态的权威来源。

### ACP / clix / background

这些入口同样应通过 coding runtime 驱动 coding agent。它们可以适配各自协议和 I/O，
但不能重新实现 provider/session/cancel/reload 生命周期。

## Session 持久化

native session 模型是唯一可写持久化模型：

- `SessionManager`、native `SessionMeta`、`SessionSnapshot` 和 `PresentationFile`
  定义持久化边界；
- snapshot 用于恢复运行上下文；append-only transcript 用于完整历史与 recall；
- catalog 负责发现会话，lease 负责跨进程互斥；
- session binding 和 lease 必须随 runtime 生命周期转移，不得由 UI 根据 session id
  猜测磁盘位置；
- 写入只能走 native store，不存在 core writer 或双向格式投影。

### 历史 core JSON importer

daemon 仍保留私有、冻结 DTO，用于读取历史 `core-session-json` 并单向收敛为 native
session。这是兼容输入，不是当前 runtime 或持久化模型：

```text
historical core JSON
          │  daemon private reader DTO
          ▼
native SessionManager / snapshot / transcript
```

禁止从 native 数据反向写回 core JSON，也禁止让 importer DTO 进入 kernel、coding runtime
或公共 session API。只有历史格式消费者归零后，才能删除该 importer。

## `atomcode-core` 状态

`atomcode-core` 已从 workspace 和生产依赖中退役，当前不存在现役的
`crates/atomcode-core`：

- core legacy `AgentClient/AgentCommand/AgentEvent` 和 v1 engine 已退役；
- `atomcode-bridge`、双 endpoint、v1/v2 选择开关和 core driver fallback 已退役；
- 生产代码不得重新创建 `atomcode-core` facade、兼容 crate 或第二 runtime owner；
- 源码注释中出现“ported from core”“retired core”表示历史来源，不表示运行时依赖；
- daemon 的历史 JSON importer 是当前唯一允许保留的 core 格式兼容面。

如果新需求似乎需要恢复 core 类型，应先定位当前状态 owner，并将能力放入 kernel、
capabilities、coding 或 driver 的正确层，而不是重建 core。

## 运行时生命周期不变量

涉及 submit、steer、cancel、approval、request、compact、provider/model reload、
session/resume、fresh、undo、cd、goal、loop 或 shutdown 时，必须保证：

1. live agent 和 coding 状态只有一个 runtime owner；
2. accepted operation 对 success、error、cancel、replace、shutdown 都有终态；
3. cancel/reload/session switch/shutdown 时 pending approval/request fail-closed；
4. generation 隔离迟到事件；
5. build/prepare/assemble/restore 失败显式返回或回滚；
6. turn completion 复用既有 `LifecycleHooks::turn_complete` 和 kernel 终止路径；
7. compaction 复用 capabilities 的现有实现，不创建第二压缩状态机；
8. 历史数据只作为 importer 输入，runtime snapshot 具有明确权威来源。

## 架构变更检查清单

修改公共协议、runtime、session、安全边界或跨 crate 依赖前，应至少检查：

- 状态的唯一 owner 和所有生产/消费入口；
- CLI、TUI、daemon、headless、background、ACP、clix 的实际影响；
- 命令、事件、持久化格式和转换边界；
- failure、cancel、replace、resume 与降级语义；
- 是否引入反向依赖、第二生命周期、静默 fallback 或新的 legacy writer；
- 相关 crate 测试以及必要的跨 crate 编译验证。

当前架构以“单一状态所有权、单向依赖、显式终态、单向历史导入”为原则。文档与代码
冲突时，应先以当前代码核实事实，再同步修正文档，不能把历史结构当作实现前提。
