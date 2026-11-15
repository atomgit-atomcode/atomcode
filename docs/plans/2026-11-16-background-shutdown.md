# 后台会话退出等待

## 边界

后台管理器只管理槽位；CodingRuntime 仍是执行、清理与会话租约的唯一 owner。
现有 stop_current_agent 会在 kernel 五秒未退出时 abort 并等待其任务结束；
runtime 在团队成员、kernel、MCP 清理及租约释放后才发布 shutdown terminal。

## 修改

- 后台 stop 对取消回执最多等待五秒。失败或超时记录警告，随后继续正式 shutdown。
- CodingRuntimeHandle::shutdown 在排队前触发现有 stop token，使 owner 内部可取消的
  图片预处理等等待能够结束，不依赖 owner 先读到 Shutdown 命令。
- 保留对 shutdown terminal 的等待，不在 driver 超时后遗弃 runtime，不强制释放租约。

这限制的是关闭前的取消等待，不是整个进程退出的绝对时间上限。
同步阻塞或未响应 stop token 的 owner 工作仍需逐条审查，不能通过丢弃 future 宣称已终止。
不改日志格式、审批策略和回合恢复语义。

## 验证

虚拟时钟测试检查：停止信号先于命令消费发出；发信号不等于关闭完成；
只有 owner 发布终态后 shutdown 才返回；重复关闭返回同一终态。
真实 owner 测试检查：永不返回的图片预处理能被直接 shutdown 中断，提交返回 NotSent。
复跑 runtime 关闭/取消测试、后台会话集成测试、编译门和格式检查。
