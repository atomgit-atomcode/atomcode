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

**Committed Snapshot**:
Coding Runtime 在明确生命周期边界产生的、可恢复的完整会话状态。
_Avoid_: Transcript, display history

**Replay Window**:
Committed Snapshot 尚未覆盖的当前回合观察事件集合，供晚加入或重连的 Live View 衔接实时流。
_Avoid_: Second conversation, event-sourced session

**Pending Interaction**:
Coding Runtime 发出的、带相关 ID 且必须由某个 Live View 回答或显式终止的审批或结构化输入请求。
_Avoid_: Global approval slot, anonymous response
