# Todo 与 Agent 并发展示设计

## 目标

当持久 Todo/Tasks 计划存在时，父任务面板继续占用 footer。同步 `task`
subagent 和异步 Team agent 的进度改为对话区动态 block，避免子任务覆盖父计划。
没有活动 Todo 时继续使用原有 Task/Team footer，保持既有紧凑布局。

## 状态与边界

运行状态仍由 coding runtime 和 Team manager 持有；TUI 只维护展示投影。
`SubtaskProgress` 是共同的 renderer-facing 快照。event loop 根据 Todo 是否未完成选择：

- Todo 活跃：footer 不携带 `subtasks`，按 Task call id / Team run id 发送相互独立的
  `UiLine::AgentGroup`；并发 Task 与 Team 不会互相覆盖；
- Todo 不活跃：沿用原 `active_subtasks` 或 `TeamProjection::panel()` footer；
- Todo 在 Agent 运行中完成：冻结正文最新快照，后续进度回到 footer。

## Retained 生命周期

Agent block 保存 header 和固定 child row 的绝对索引。更新只替换这些 cell，普通助手
文本或工具输出不会冻结 block。行进入终端原生 scrollback 后停止改写，避免 CUP 写错目标。
完成、失败和取消会写入最后快照并释放 live 索引；body log 合并同一 call id 的快照，
resize 重放只恢复最新状态，不重复输出每次活动事件。plain renderer 不做原位更新，
仅在收到终态时输出摘要。

## 验证

- Todo 与 Task/Team 同时存在时只有 Todo 占用 footer；
- 正文 Agent block 可跨普通正文输出继续更新；
- 完成和取消后 block 固化；
- resize 重放保留最终状态；
- 没有 Todo 时原固定 Task/Team 面板保持不变。
