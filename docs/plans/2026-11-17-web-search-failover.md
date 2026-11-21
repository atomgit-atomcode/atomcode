# Web Search 多后端与自动切换实施计划

> Implementation owner: Codex，用户直接授权实施；不属于 AtomCode 委派任务。

基线：release/v5.2.8，4ed548db8。工作区已有 provider/recovery/kernel 及文档并行改动，全部保留，不归本任务所有。

目标：统一 web_search 工具接入 Exa、Parallel、Tavily、Firecrawl、TinyFish；保留显式 DuckDuckGo。未配置时默认 auto，显式后端固定使用。无 Key 使用相应匿名路径，有 Key 使用对应环境变量；已有 web_search.api_key 继续只代表 Exa，不进入行配置。

所有权：capabilities 的 WebSearchTool 实例持有凭据、成功后端偏好和固定大小冷却表；同一运行时内复用。ToolContext 没有会话 id，不冒充会话隔离，不改变 kernel 协议。重建工具会重置健康状态。harness 挂载与现有 coding 配置边界继续复用。

自动策略：优先上次成功后端，其次按稳定候选顺序尝试；每个查询每个后端最多一次。429 尊重 Retry-After（秒或 HTTP 日期，缺失默认 60 秒）；网络、超时、认证、5xx、协议失败进入短期冷却并切换；合法零结果直接成功。每次自动尝试最多 8 秒、整个查询最多 45 秒；固定后端保持 25 秒预算。取消不触发后续请求或健康惩罚。全部不可用时返回有界、脱敏的后端错误摘要，不 sleep 等待恢复。

安全：固定 HTTPS 地址、拒绝跳转、响应体上限 256 KiB；错误不输出带 Key 的请求 URL，响应回显 Key 也脱敏。成功前不因单个后端连接失败把整机标成离线。TinyFish 有 Key 时完成 MCP initialize/initialized，会话标识仅用于本次搜索。

步骤：
1. 在 capabilities 中补后端协议适配、统一传输及错误类型；保持旧 Exa/DDG API 兼容。
2. 加入工具实例级健康状态、超时、取消与有界切换。
3. 修改 config/harness 默认为 auto，保留显式选择及 Exa 宿主凭据注入；补使用说明与 /config 后端选择。
4. 用本地 HTTP fixture 和脚本化后端测试协议、响应限制、Key 脱敏、429 冷却、故障与取消；运行受影响测试、全 workspace 编译门及格式检查。

验证不调用真实公网搜索消耗配额；实际网络可达性和各上游匿名额度不由单元测试保证。不更改发布版本，不自动提交推送。

初次实施结果：五个协议适配、自动路由、配置默认值和 /config 选择器均已完成。保留固定 Exa/DDG 和已有 Exa Key 的兼容语义。初次提交保留旧配置的显式 Exa，后续经用户明确授权按下述规则迁移。

验证记录：capabilities/config/harness/CLI 的 3435 项库测试通过（7 项跳过）；coding mount_wiring 的 18 项测试通过，包括凭据不进入配置树、搜索默认值和项目覆盖；最终搜索专项 23 项通过，覆盖无 Key/带 Key 协议、TinyFish 握手、限流冷却、故障切换、超时取消、脱敏、响应限制和固定后端。未做真实公网额度/可达性验证。

完整编译门最终通过（含两种 canary 自检）；格式与 diff 检查通过，Cargo.lock 无改动且无禁止的三个 package。首轮门在并行构建/修改期间最后一次自检未通过；随后独立 workspace 编译成功，再跑完整门全部通过，不将首轮失败计作通过。

后续默认值迁移：用户明确要求旧 Exa 配置也改成 auto。配置读取将未带 `auto_default_migrated` 标记的 Exa 改为 auto，并通过 ConfigStore 的文件锁与原子文档替换保存标记；其他后端不改。严格/容错解析均有内存迁移，保证只读文件仍使用 auto，不将迁移写入失败变成启动失败。/config 显式保存同时写标记，用户之后选择 Exa 不会再被覆盖。迁移保留 Key、未知字段、注释；环境变量覆盖保持现有优先级。测试覆盖重复读取、后续固定 Exa、其他后端、无法写盘时的解析回退，以及未先读取就直接 /config 选择 Exa。

迁移验证：配置库与 CLI settings 共 377 项测试通过；最后补充的写盘失败路径包含在另跑的两项迁移专项中，均通过。完整编译门重跑通过，格式与 diff 检查通过。首轮编译曾因并行门删除 `__feature_gate_canary.rs` 报文件不存在，不计为通过。日志：`/tmp/atomcode-search-migration-tests.log`、`/tmp/atomcode-search-migration-fallback.log`、`/tmp/atomcode-search-migration-compile-final.log`。
