# AtomCode Runtime Context

AtomCode 的 coding 会话由单一运行时执行，多种交互视图可以同时观察并控制它。这里统一描述运行时与多视图同步相关的领域术语。

## Language

**Coding Runtime**:
一个活动 coding 会话的唯一执行所有者，持有对话状态、回合生命周期和可恢复快照。
_Avoid_: Live runtime, view runtime, secondary runtime

**Live View**:
观察同一 Coding Runtime 并可提交控制请求的交互参与者，例如终端、浏览器或移动端。
_Avoid_: Live session, mirror session

**Live View Hub**:
连接一个 Coding Runtime 与多个 Live View 的进程内多路复用边界，只负责观察分发、短期回放和控制路由，不拥有对话或执行生命周期。
_Avoid_: LiveSession, conversation owner

**Runtime Binding**:
Live View Hub 与一个确定的 runtime generation、session 和 working directory 之间的唯一关联。
_Avoid_: Best-effort attachment, implicit reuse

**Session Transition**:
由 Coding Runtime 原子提交 session identity、working directory、generation 和 session catalog visibility 的可等待替换操作；请求已接收不代表切换已提交。这里的 catalog 仅指持久化 session 列表，不指模型工具。
_Avoid_: Fire-and-forget session command, optimistic session reset

**MCP Scope**:
一个已 prepare 的 Coding Runtime generation 根据 working directory、MCP 配置和项目信任状态持有的外部能力集合。不同 generation 的 MCP Scope 和 Tool Catalog 互相隔离；旧 scope 的连接结果不能写入 replacement。
_Avoid_: Driver MCP registry, global model-facing registry

**Tool Catalog**:
一个 Coding Runtime 当前可向后续模型回合发布的完整工具集合。更新以整表原子替换完成，不属于 Session Transition 的提交内容。
_Avoid_: Session catalog, incrementally mutated tool map

**Tool Catalog Revision**:
同一个 Tool Catalog 每次原子发布后的单调版本。它描述工具可见性变化，不等同于 Runtime Generation。
_Avoid_: Runtime generation, MCP connection count

**Turn Tool Snapshot**:
回合开始时从 Tool Catalog 捕获的不可变工具定义和实现集合；该回合的 provider 请求与工具执行必须使用同一快照。
_Avoid_: Live tool lookup, mid-turn tool refresh

**MCP Readiness**:
MCP 服务器连接、发现和工具发布的附加能力状态。连接终态通知是可供多个消费者观察的 level-triggered 状态；Tool Catalog 提交完成才代表 headless barrier ready。交互式 Session Transition 不等待它；headless surface 可以设置首回合等待上限，超时不终止后台连接和后续发布。
_Avoid_: Runtime phase, session transition success

**View Projection**:
Live View 在 Runtime 终态之后派生的会话、目录和展示状态，不是运行时状态的第二所有者。
_Avoid_: Driver-owned session state, optimistic projection

**Committed Snapshot**:
Coding Runtime 在明确生命周期边界产生的、可恢复的完整会话状态。
_Avoid_: Transcript, display history

**Replay Window**:
Committed Snapshot 尚未覆盖的当前回合观察事件集合，供晚加入或重连的 Live View 衔接实时流。
_Avoid_: Second conversation, event-sourced session

**Pending Interaction**:
Coding Runtime 发出的、带相关 ID 且必须由某个 Live View 回答或显式终止的审批或结构化输入请求。
_Avoid_: Global approval slot, anonymous response
