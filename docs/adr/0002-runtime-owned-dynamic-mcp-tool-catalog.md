# MCP readiness is decoupled from session transitions

## Context

MCP transport initialization and `tools/list` can wait for DNS、TLS、OAuth、stdio 子进程或服务器超时。旧实现把这段网络等待放在 `CodingParts::prepare`，因此 fresh、resume、change-directory 和 capability reload 都会被最慢的 MCP server 阻塞；CLI/TUI 同时维护管理 registry，又让 CodingRuntime 建立一套 model-facing registry，造成重复连接和刷新语义分叉。

## Decision

CodingRuntime 是 model-facing MCP Scope 的唯一 owner。`prepare` 只加载配置并启动后台连接，不等待 readiness；每个候选 `CodingParts` 持有独立 registry 和 Tool Catalog，候选提交后其后台发现结果只能发布到自己的 catalog，无法污染 replacement generation。每个 server 连接成功后只发现并发布该 server 的工具，初始连接全部进入终态后再做一次完整 reconciliation。后台 publisher 本身没有 driver 启动超时，因此晚于 headless 等待上限才连接的 server 仍会发布到后续 turn。

kernel 的 `MountedTools` 支持整表原子发布。Agent 在 turn 开始时捕获一个不可变 Turn Tool Snapshot，provider tool definitions 和该 turn 后续执行查找都使用这个快照。后台 MCP 工具只从下一 turn 可见。

连接终态使用 level-triggered broadcast readiness，多个并发 waiter 不会互相消费通知。交互 surface 使用后台 readiness；headless surface 在首个 submit 前显式等待“连接结果已提交到 Tool Catalog”，由调用方设置有界等待，超时只放行首回合，不取消后台 publisher。

`tools/list` 网络 I/O 不持有 catalog publication lock；锁只覆盖内存注册和整表发布。MCP 配置、trust 和 auth 变化统一通过 capability reload 构建新的 runtime generation；配置清空也必须走同一路径以移除旧工具。reload 开始时先关闭旧 scope 的后续发布并原子撤下 MCP 工具，撤销不会被慢 server 的网络超时阻塞；logout、untrust 等缩减权限的磁盘 mutation 必须先等待该撤下终态，终态被拒绝时不得写盘。即使 replacement 构建失败，已撤销的 trust、auth 或配置也不会通过旧 catalog 继续生效。候选被丢弃或 scope 被撤销时会取消其连接/发现任务，stdio 子进程随 client 生命周期终止。

CLI、TUI 和 clix 不再建立第二套 model-facing registry。`/mcp status` 和 `/mcp tools` 查询当前 CodingRuntime；daemon 的兼容 MCP API 可以保留非 model-facing registry，但其 reload/trust mutation 必须同步刷新 native CodingRuntime。

状态查询覆盖 connecting、connected、untrusted blocked、连接失败、配置解析失败和 `tools/list` 失败。配置错误不得降级成“未配置”，空工具列表也必须携带 server 的准确状态。

## Consequences

- session candidate 的提交时延不再受 MCP 网络超时支配；
- 一个 turn 内工具定义与执行集合保持一致；
- 动态发布会改变后续请求的工具前缀并使对应 provider cache prefix 失效，这是避免阻塞交互切换的明确 trade-off；
- 刚完成切换的第一个交互 turn 可能尚未看到仍在连接的 MCP 工具，用户可从下一 turn 使用；
- capability reload 仍会替换 runtime generation，而不是在当前 generation 中原地改 trust/auth policy；旧 MCP authority 在 preflight 前先撤下，replacement 失败时保持 fail-closed；
- 后续若需要按 server 增量复用连接，可以在 CodingRuntime 内增加 connection pool，但不得恢复 driver registry 或让 capabilities 拥有 runtime generation。
