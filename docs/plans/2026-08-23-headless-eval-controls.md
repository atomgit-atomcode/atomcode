# Headless 评测控制实施计划

## 目标

为 `atomcode -p` 增加两个可组合的评测开关：

- `--ephemeral`：使用正常 auth/config/provider，但不创建、恢复或写入 session 聚合；
- `--no-tools`：在 coding capability 装配边界挂载空工具目录，并关闭 MCP、skills 工具、review 与子 Agent。

两者只允许用于 headless 输入（`-p` 或 `--prompt-file`），`--ephemeral` 与
`--continue` 冲突。CodingRuntime 仍是唯一 live runtime owner，kernel 命令与事件不变。

## 实施步骤

1. 为 CLI 参数解析增加 headless 参数组、冲突与单元测试。
2. 为 `PrepareOptions` 增加 driver-owned 的工具挂载策略，并用测试证明关闭后挂载目录为空。
3. 将两个 CLI 开关传入 `spawn_native_cli_runtime`：ephemeral 映射为
   `SessionMode::Disabled`，no-tools 映射为关闭工具装配。
4. 运行 CLI 参数测试、coding parts 相关测试和格式检查；检查 diff 不覆盖现有 dirty worktree。

## 第二、三阶段：机器可读事件与指标

- `--output-format jsonl` 在 stdout 输出 `schema_version = 1` 的逐行事件；默认 text
  行为不变；
- 事件覆盖 message/reasoning delta、工具起止、逐轮 usage、warning/error、限流、
  retry 和唯一 turn terminal；
- 最终事件聚合 TTFT、duration、round/tool 数、prompt/completion/cached token 与
  cache hit rate；
- OpenAI-compatible provider 将 DeepSeek 的 cache hit/miss 拆分归一为
  `prompt = hit + miss`（仅在 aggregate prompt 缺失时）。

## 状态所有权与失败语义

- live agent、provider、generation、pending request 与终态仍由 CodingRuntime 管理；
- ephemeral 没有持久化 owner，也不取得 session lease；
- no-tools 只改变本 generation 的 capability catalog，不修改 kernel 协议；
- 参数组合非法时由 clap 在 runtime 启动前失败；provider、submit、cancel、shutdown
  继续沿用现有显式错误与终态。
