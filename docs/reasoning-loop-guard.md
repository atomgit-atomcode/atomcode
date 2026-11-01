# 推理复读保护

kernel Agent 与 harness agent-loop 默认监测 unsigned reasoning 的精确连续重复。
检测窗口最多保存 4096 个 Unicode 字符，每新增 128 字符扫描一次；结束事件
也会检查剩余后缀。默认周期范围为 32–1024 字符，至少重复 4 次，且重复部分
覆盖至少 1024 字符。数字、空白和标点的重复本身不触发；不做语义相似度判断。

仅在当前请求尚未产生非空正文、工具调用/参数片段或签名推理边界时监测。
这些边界一旦出现，本次请求不再触发自动重放。正文和工具循环仍由各自已有
机制处理。检测器不依赖模型名，也不把 idle watchdog 的持续进展当作正常保证。

命中后丢弃当前请求的推理预览、立即释放流，并通过 Notice/Warning 提示：
“检测到推理复读，正在重新生成（1/1）...”。harness 每个逻辑请求最多重试
一次（通用恢复重入不重置这个额度），kernel 每个用户回合最多一次；共享
provider 请求预算仍可阻止重试。再次命中则明确报错结束，不无限重试，不切模型。
重试仍发送原请求；本版不插入模型纠偏提示，也不改变采样参数。

已经广播的内容用瞬时 `ReasoningDiscarded` 边界标记，TUI 将当前推理块替换为
丢弃标记并关闭，然后为新尝试建立独立块。它和 AssistantChunk 一样不落盘，
不会成为模型上下文；失败响应不提交为 AssistantMessage。无需重写日志或提高
原生会话格式版本。WebUI 当前不展示 reasoning，提示仍从 Warning 通道到达。
已观察到的 usage 保留；若上游仅在末尾报告 usage，中途取消无法获得完整用量，
不伪造为精确的节省 token 数。用户取消仍由原有运行时取消路径处理。

## 配置接口

Rust 宿主可通过 `ChatOptions.reasoning_loop_guard: Option<ReasoningLoopConfig>`
设置；`None` 使用上述默认值，`enabled = false` 关闭检测。其余字段为
`min_period`、`max_period`、`min_repeats`、`min_repeated_chars`。为限制内存和计算，
周期限制在 16–1024、重复次数在 3–16、最小重复字符在 512–4096，越界值收敛到
安全范围。选择大于窗口可容纳范围的组合不会检测到重复，应保持阈值可满足。

coding 的已有 `chat-options` 行支持同名 `reasoning_loop_guard` 对象，可由宿主
layer 配置，并在 provider 重建时按 `CodingAgentConfig.chat_options` 更新。
这是本地流策略，不是模型 API 参数，也不是新顶层 config.toml 设置。例如宿主
在默认装配后增加此 layer（patch 替换配置，需保留该行其他已配置的选项）：

```toml
[[patch]]
id = "chat-options"
config = { reasoning_loop_guard = { enabled = false } }
```

本版不检测轻微改写的语义循环，不保证任意重复都命中；正常任务若需要长段
逐字重复推理，可以关闭保护。签名/工具边界后的重复也不在本版自动重试范围内。
