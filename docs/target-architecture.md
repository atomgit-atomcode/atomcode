# 目标架构与当前收口方向

> 状态：当前有效的方向性约束。
>
> core driver 协议、v1 engine 和 `atomcode-bridge` 已退役。当前工作不再是继续迁移 bridge，
> 而是收敛接入层仍保留的 session/conversation 双模型、历史兼容和基础设施重复实现。
>
> 目标是单一状态所有权、清晰依赖方向和可验证兼容性；`atomcode-core` 是否最终移除，
> 取决于它是否自然失去职责和消费者，不作为独立 KPI。

## 1. 当前目标调用链

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

`atomcode-review` 等其他 L2 可以装配并驱动自己的 kernel agent；但每个业务只能有一个明确的
运行时 owner，不能让 driver、adapter 和 L2 同时持有多套 live `AgentHandle`。

## 2. 分层与依赖方向

```text
kernel ← capabilities ← L2 specialization ← frontend/transport

叶子基础设施：config、auth、telemetry、updater 等按职责被上层依赖
兼容边界：legacy session importer，只允许从旧格式流向当前模型
```

| 层 | 拥有 | 禁止 |
|---|---|---|
| `atomcode-kernel` | 中立 agent 循环、hook/middleware/tool/provider trait、kernel message/event | coding、approval、plan、plugin、具体 provider/tool 实现 |
| `atomcode-capabilities` | provider、tools、MCP、skills、session、memory、codeintel 等可复用能力 | 依赖 core、L2 或前端；读取前端状态 |
| `atomcode-coding` | coding persona、runtime 生命周期、provider/session reassemble、goal/loop、审批协调 | 依赖 core；UI、HTTP、终端渲染 |
| CLI/TUI/daemon | 输入、展示、HTTP/WS/SSE、本地明确操作、历史格式接入 | 第二 runtime owner；把 coding 生命周期直接塞进 kernel 命令 |
| `atomcode-core` 兼容负担 | 当前仍被接入层使用的 session/conversation、plugin、live、部分旧能力 | 恢复旧 driver 协议、bridge 或 runtime fallback |

编译期不变量：

- kernel 不依赖 capabilities、L2 或前端；
- capabilities 不依赖 core、L2 或前端；
- coding 不依赖 core 或前端；
- frontend 可以依赖 L2 和叶子基础设施，但不得持有第二套业务 runtime。

## 3. Runtime 所有权

`CodingRuntime` 统一拥有：

- live agent、config、parts、provider、session binding；
- generation、pending request、snapshot broker；
- submit/steer/cancel/approval/request/compact；
- provider/model reload、fresh/resume/restore/undo/cd；
- goal/self-paced loop 和 shutdown。

driver 可以执行不需要运行中状态的本地操作。凡是会改变 conversation、snapshot、provider、
session binding 或 agent generation 的行为，必须通过 runtime 的显式事务完成，不能用本地文件写入
绕过 runtime。

kernel `AgentCommand/AgentEvent` 是运行时执行边界，不是承载所有产品命令的公共总线。

## 4. 当前剩余问题

### 4.1 Session/conversation 双模型

当前同一 project bucket 中并存：

- core `<id>.json`：完整 UI/session 对象，仍被 CLI/TUI/daemon 列表、重命名、删除、恢复和镜像写入；
- native `<id>.snapshot`：kernel working-set snapshot，供 runtime resume；
- native `<id>.meta`：快速列表元数据；
- native `<id>.jsonl`：不压缩的逐回合 transcript，用于 recall。

native snapshot 是运行中 conversation 的权威数据，但 core JSON 目前仍包含 UI-only message、
cold summaries、命名状态、turn stats 等接入层语义，不能直接删除。目标是先补齐 native store 的
必要语义，再把 core JSON 降为只读、幂等、可失败的历史 importer，最后删除 live 双写和双向转换。

### 4.2 基础设施重复

core 中仍有 plugin、live、MCP、LSP、provider、tool、graph、semantic 等实现。处理顺序必须由真实
消费者决定：先切消费者和状态 owner，再删除旧实现。只复制到新 crate、保留两份实现不算进度。

## 5. Protocol 与 foundation 的决策门槛

不预设先创建 `atomcode-protocol`。只有同时出现以下需求之一时才拆纯协议叶子：

- HTTP/WS 对外 schema 需要独立版本；
- 非 Rust 客户端需要稳定 codegen；
- kernel 类型演进已对外部消费者造成实际耦合。

拆分前应先证明现有 kernel/coding 中立类型不能满足需求，且新 crate 会删除现有重复协议，而不是
再增加一套类型。

不创建大而全的 `atomcode-foundation`。config、auth、plugin、session、transport、process utilities
应按内聚职责复用现有叶子 crate 或单独拆分；目标是减少耦合，不是把 core 改名。

## 6. 收口顺序

1. 收敛 session/conversation 持久化和恢复语义；
2. 将 core session JSON 降为独立单向 importer，删除 live 双写/双向转换；
3. 按职责收口 plugin、live transport、MCP host；
4. 消费者归零后删除 core 中重复的 provider/tool/MCP/LSP/graph/semantic 实现；
5. 重新评估 core 剩余职责；只有自然为空时才移出 workspace。

每个垂直切片必须实际减少至少一项：状态 owner、数据模型、转换链、直接依赖或 fallback。
不得以移动文件、增加 facade、新建 crate 或净删除行数冒充架构进度。

## 7. 兼容与失败原则

- 历史格式读取必须有显式 schema/字段映射和真实 fixture 测试；
- importer 必须幂等，导入失败不得覆盖旧文件或生成可被误判为成功的半成品；
- legacy 与 native 同时存在时必须有明确冲突规则，禁止按 mtime 猜测后静默覆盖；
- runtime rebuild 失败必须显式失败或回滚，禁止 silent fresh、空 snapshot、noop handle；
- pending approval/request 在 cancel、reload、session switch 和 shutdown 时 fail-closed；
- 旧 generation 的迟到事件不得进入 replacement runtime；
- 未删除旧 writer、handler、依赖或 fallback 时必须明确“尚未退役”。
