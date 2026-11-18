# 只读浏览的运行状态与时间

基线 `release/v5.2.8`，`f8f63650f`。工作区已有产物预览/工作区和 Todo 并行修改，保留原样；Chat/i18n 仅增补本修复所需内容。

## 根因与边界

1. SessionBrowse 会轮询正在其他终端运行的会话，但 BrowseMessagesView 把 busy/streaming 固定为 false，导致当前回合显示结束时间、复制按钮及完成态折叠。
2. 原生日志的时间投影在 TurnStart 时把 completed_at 初始化为开始时间，未结束的回合因此也有一个固定的“完成时间”。

会话活动仍由持有租约的运行时负责，页面仅消费 activity；时间只来自原生事件日志或历史 transcript。没有增加运行时 owner、终态钩子或会话 writer。内存 TurnTimestamp 的完成时间改为 Option，只有 TurnEnd 填充；旧 transcript 的真实 ts 继续映射为 Some。持久化格式不变，现有 created_at 可选字段无需新增 wire 字段；HTTP 历史与 live snapshot 使用同一时间投影。

新用户消息尚无 round meta/已完成 TurnStat 时，只在其位于有效已完成前缀之后、且日志只有唯一未结束回合时，使用该 TurnStart 的真实起点。多条未结束记录或已重写的位置边界不能可靠关联，继续省略时间，不以浏览器时钟补造。

## 显示规则

- 当前回合按 running/waiting 显示活动状态，不展示结束 footer，不自动折叠。历史已完成回合保持原来的时间与复制入口。
- 仅在有真实用户开始时间时显示每秒更新的用时；不知道起点时不编造时钟。
- 活动变为 idle/open 后，等最终历史补刷成功才释放完成态；被跳过或失败时保留同步状态并重试，避免旧页面短暂伪装成最终结果。
- 计时器随状态/会话卸载清理，浏览不连接第二运行时，也不附加对原会话的控制权。

## 验证范围

单元测试覆盖未结束/已结束的事件时间、旧 transcript 兼容、HTTP/live 投影、缺失起点及新用户尚无回复。浏览器 fixture 覆盖已带 checkpoint 时间的活动消息、动态时钟、等待交互、失败补刷与完成 footer；同时回归原有会话浏览和过程折叠。测试不操作用户真实会话。

验证结果：WebUI 491 项、会话模块 362 项、daemon 433 项通过；WebUI 类型检查/构建、三个浏览器套件（session-fork/session-browse/turn-process）、完整编译门与格式检查通过。扩展 session-fork fixture 已验证运行时只保留历史回合 footer，用时独立于轮询逐秒更新，等待状态不变成完成态，补刷失败保持同步状态。截图 `/tmp/atomcode-browse-running-status.png`。

一次 capabilities+daemon 全量测试被工作区另一组 Todo 改动的 `todowrite_full_plan_gets_compact_confirmation` 失败中止，2426 项通过、1 项失败，其余未执行，未将其记为全量通过；之后单独完成受影响会话模块与 daemon 回归。该 Todo 实现未纳入本修复。日志 `/tmp/atomcode-browse-clock-rust-tests.log`、`/tmp/atomcode-browse-clock-session-tests.log`、`/tmp/atomcode-browse-clock-daemon-final.log`、`/tmp/atomcode-browse-activity-compile-final.log`。尚未使用用户实际运行的 TUI/WebUI 会话做联调。
