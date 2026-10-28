# VPN 切换后的代理自动恢复

用户已确认：跟随系统自动恢复；本地代理明确拒绝连接时临时直连；代理恢复后重新使用；固定代理不绕过。不增加确认框。

基线：`release/v5.2.5`，`dfa998f49383445f934680eabeb4e0e8d2ea0e86`。

## 边界与失败语义

- config 叶子负责新鲜的系统代理解析，保留启动环境变量的优先级；不改配置文件、不发布临时环境变量。
- capabilities 的 HTTP 客户端负责请求前探测回环代理、按路由变化替换连接池。覆盖 Chat Completions、Responses、Anthropic、Ollama，所以 CLI/TUI/daemon/headless/background/ACP 复用同一行为。
- runtime 继续独占会话、provider、审批、取消和 generation；不重建 agent，不引入新的重试状态机，不重放工具操作。
- 只在明确的回环端口 connection refused 时直连。其他探测错误保留代理。刷新/客户端构建失败显式返回错误，旧客户端保留。
- 代理刷新最多等待 2 秒；macOS 查询子进程在 1 秒后终止并回收。取消后迟到的解析不能替换客户端。Windows 区分不存在的可选设置和真实读取错误，读取失败不视为关闭代理。
- 不记录含凭据的代理 URL，不改变 session 格式或公共协议。本次不改 OAuth、更新器、MCP 等独立 HTTP 客户端的恢复生命周期。
- 模型发现与账号探测也使用同一请求局部策略。切换记入诊断日志，`/proxy` 的中英文说明标明自动直连行为。

## 实施和验证

1. 去掉系统代理的进程永久缓存，提供不修改环境的跟随系统快照；测试系统开关和启动环境优先级。
2. 增加请求局部代理策略：探测回环端口（含 IPv6/localhost）、只移除明确拒绝的路由；显式配置 reqwest 以免环境代理重新接管。
3. 共用可替换客户端在每次 HTTP OPEN 前刷新路由，变化才替换连接池；原 transport rebuild 保留当前路由。Ollama 接入相同 seam。
4. 测试本地代理停止/恢复、远程和固定代理不绕过、NO_PROXY、路由更新后真实 HTTP 请求能走直连/代理，及构建失败保留旧客户端。
5. 运行受影响 crate 的 nextest、`bash gates/compile.sh`、`cargo fmt --all` 和格式检查，审查 diff/Cargo.lock。

## 完成验证

- 审计后 config/capabilities/i18n 完整 nextest：1,517 项全部通过。
- `bash gates/compile.sh` 通过（workspace、feature 门控和两类阴性对照）。
- `cargo fmt --all -- --check`、`git diff --check` 通过；Cargo.lock 无修改，三个禁止的 package 名均为 0。
- 真实本地 TCP/HTTP 测试覆盖代理停止/恢复和路由切换；未操作用户 VPN 做实机验收，未验证 Windows 系统代理的实际切换。

## 审计修正

- 系统代理刷新原先位于 HTTP 超时计时前，macOS 子进程和阻塞队列可能使请求无限等待。补充独立刷新上界与子进程终止/回收，超时返回可重试错误，不作为直连信号。
- Windows 读取设置时的访问失败不能降级为空配置；只有不存在的可选值可取默认值。已启用代理但地址读取失败时保持显式失败。
- 新增虚拟时钟测试确认刷新超时语义与取消后不发生迟到替换，并用真实子进程测试验证超时终止。
