# 同目录切会话复用 MCP 连接(runtime 内连接池)设计

状态: 已实现(2026-09-24,分支 `feat/mcp-connection-pool`)。第 5 节三个问题的结论见各条末尾。

## 1. 问题

同一个 `CodingRuntime` 里每次切会话(新建 / 恢复 / 带租约恢复)都会重走
`parts::prepare`,新建一个 `McpRegistry`,把每个 MCP server 重新拉起、重新握手;
旧能力树提交后被丢弃,旧连接随之关掉。判据
`runtime_criteria::a_switched_session_keeps_the_mcp_tools` 实测:启动 + 两次切换 =
server 进程启动 3 次。

后果:

1. **切换变慢。** cbb18bd1c 之后 daemon 的切换要等 MCP 就位才返回(封顶 30s);
   反馈里 9 个 `npx` server,每切一次都是一轮冷启动。
2. **有状态 server 每切一次丢状态**:浏览器(打开的页、登录态)、数据库连接、
   HTTP MCP 的 `Mcp-Session-Id`。
3. 无谓的进程抖动。

## 2. 约束(现有 ADR 与文档)

| 来源 | 规则 | 对本设计的含义 |
|---|---|---|
| ADR 0002 | CodingRuntime 是 model-facing MCP Scope 的唯一 owner;每个候选 `CodingParts` 持有独立 registry 与 Tool Catalog,候选的发现结果不能污染 replacement generation | registry(及其授权、别名、取消闩、事件)**仍按 generation 独立**,不共享整份 registry |
| ADR 0002 | 配置 / trust / auth 变化一律走 capability reload 建新 generation;reload 开始先撤下旧工具;即便 replacement 失败,已撤销的东西也不能经旧 catalog 生效 | reload、trust/untrust、logout、enable、改目录:**不复用,连接池清空** |
| ADR 0002 | 「后续若需要按 server 增量复用连接,可以在 CodingRuntime 内增加 connection pool,但不得恢复 driver registry 或让 capabilities 拥有 runtime generation」 | 本设计正是这条:池归 CodingRuntime 持有;capabilities 只提供机制类型 |
| ADR 0001 | 候选必须在中断当前 agent 前完成所有可失败步骤;失败时旧 generation 继续可用 | 候选从池里「借」连接,失败丢弃候选时**不得**关掉池里的连接 |
| docs/mcp.md 缓存红线 | MCP 工具定义属于请求缓存前缀;`/mcp reload` 是新前缀世代 | 复用同一连接 → 工具定义不变 → 前缀不变;reload 仍是新世代 |
| 90a927332 → a28c21585 | 曾有进程级 `static` 缓存(按目录 + 配置指纹复用整份 registry),47 分钟后被 ADR 0002 的 runtime-owned 设计取代 | 不走回头路:不做进程级全局缓存、不共享整份 registry |

## 3. 现状要点(调研结论)

- registry 只在 `parts.rs:789` 创建,构造时同步读配置(`load_mcp_config`,`${VAR}` 此时展开)、
  信任(`partition_by_trust`)、extra servers,然后后台并行连接。
- `McpWorkGuard::drop` → `cancel_pending_work()`:**一次性闩**,只中断在途 `initialize`,
  不杀已连上的 client;client 在最后一个持有者释放时 drop(stdio `kill_on_drop`,HTTP 发 DELETE)。
- 连接事件是单消费者 `mpsc`,构造时固定;`initial_ready`、`cancelled` 都是置位后不复位。
- 按 generation 累积、只增不减的状态:`auto_approved_tools`(含「始终允许」的会话授权)、
  `trusted_servers`、`tool_aliases`、失败 / 覆盖状态。**整份共享会把会话 A 的授权漏给会话 B**。
- Reprepare 各目标:`Fresh` / `Resume` / `ResumeWithLease` 不撤回 MCP;`Reload` /
  `ReloadConfig` 先 `withdraw_mcp_tools` 再建;`ChangeDirectory` 同目录为 no-op、否则新目录新会话;
  McpAct 里除 `Disable` 外都随后走 `Reload`。切模型 / undo / 恢复快照不重建 parts,本就复用。
- 新旧并存窗口:候选 prepare(`runtime.rs:6289`)起到提交 `runtime = candidate`(`:6493`)止,
  两份 registry 同时存在;会话切换在有活动回合时被拒(Busy),窗口内旧 agent 空闲。
- team 成员 / task 子 agent 不建自己的 registry,按名字从主 agent 的 catalog 拷工具,不受影响。
- stdio 断线是**惰性**重连(下一次请求失败才重启);初次连接失败永不自动重试,
  只有新 registry 能自愈。HTTP 的 OAuth token 每次请求现读,不固化进连接。

## 4. 设计

### 4.1 什么被复用

复用的单位是**一个已连接的 server client**(stdio 子进程或 HTTP 会话),不是 registry。
每个 generation 仍新建自己的 `McpRegistry`,只是它的某些 server 不自己连,而是从池里借一个
现成的 client。于是 ADR 0002 的隔离不变:授权、别名、取消闩、事件、失败状态都在各自 registry 里。

### 4.2 池

```text
CodingRuntime(owner 循环里的状态,跨 generation 存活)
  └─ McpConnectionPool
       key:   server name
       value: { identity: McpConnectionIdentity, client: Arc<dyn McpClient>(已完成 initialize), instructions, timeout, tools }
       owner: 在用 registry 的 id(清空后为空)
```

- **类型放 capabilities**(`mcp::pool`),只是机制;**实例由 CodingRuntime 持有**,
  经 `PrepareOptions` 传给 prepare。capabilities 不持有 generation,不做全局静态。
- **连接标识,逐字段相等而不是一个哈希**:`McpConnectionIdentity` 由解析后的完整
  `McpServerConfig`(`${VAR}` 已展开的 command/args/env/url/headers、transport、timeout、
  auth 类型、source)、working_dir(stdio 子进程的 cwd)与项目信任状态组成,判等用 `==`。
  判不等时能说出是哪个字段变了(写进 trace),比指纹好查。`autoApprove` / `trust: true`
  不进标识(它们是 registry 的审批状态,每代按配置重新种)。OAuth token 每次请求现读、
  不固化在连接里,所以不进标识;logout 走整池清空。
- **借用条件,四个同时成立**:标识相等;该 client 的 `initialize` 已完成;连接未关闭
  (stdio 子进程未退出、状态 `Connected`)。
  否则照常新连,连上后放进池。
- **借用连同工具列表**:池条目存连接时拉到的工具列表,借用方直接用它,不再 `tools/list`。
  工具定义因此逐字节不变,请求缓存前缀不被打破(见缓存红线)。借用不产生
  `McpConnectEvent::Connected`(不计一次连接尝试,遥测不虚增);mcp-host 行挂载时本就先发布
  `list_all_tools()`,初次连接完成时再全量对账,借来的工具在两处都会出现。
- **池只收在用 registry 的连接(owner)**:每个 registry 有唯一 id;启动、提交、候选失败
  回退时,在用的 registry settle 成为 owner,池只持有它自己的连接。`put` 在池锁内核对 owner,
  于是候选、被替换的、prepare 半途失败留下的孤儿 registry,无论何时连上都进不了池、不会挤掉
  在用连接,随各自 registry 关闭。清空(撤回)后 owner 为空;被撤回的 registry 不再成为 owner。
  (初稿用「池代数」只挡住了清空后的迟到连接,评审指出挡不住候选与被替换的 registry,
  改为 owner。)
- **归还 / 淘汰**:提交新 generation 后,池只保留新 registry 实际在用的键,其余逐个关闭
  (stdio kill、HTTP DELETE)。**候选失败丢弃时不淘汰任何东西**——旧 generation 还在用。
- **失败的 server 不进池**:下一代照常重连,保留「重建即自愈」。

### 4.3 什么时候整池清空(不复用)

| 触发 | 理由 |
|---|---|
| `Reload` / `ReloadConfig`(`/mcp reload`、`/reload`、插件安装、登录等) | ADR 0002:reload 就是重连 |
| McpAct 中的 Trust / Untrust / Logout / Enable | 权限或配置变化;Untrust/Logout 还要先撤下旧工具 |
| `ChangeDirectory` 到另一目录 | 键里有 working_dir,自然不命中;提交后旧目录的连接全部淘汰 |
| runtime 关闭 | 池随 owner 释放 |

`Fresh` / `Resume` / `ResumeWithLease` 在**同一目录**时借用;`ResumeWithLease` 指向另一目录时,
按键不命中,效果同改目录。

### 4.4 失败与生命周期语义

- **候选 prepare / assemble 失败**:候选 registry 被丢弃;它借的 client 仍在池里、仍被旧
  registry 使用。旧 generation 完全不受影响(ADR 0001)。
- **撤回(untrust / logout / reload)**:先 `withdraw_mcp_tools`(旧 catalog 撤下工具、
  旧 registry 取消闩置位),再**清空池并关闭连接**,然后才改磁盘状态 / 重建。
  这样即使重建失败,被撤销的权限也不会经旧连接继续生效。
- **池中连接已坏**(子进程退出):借用前检查状态,坏的直接丢弃重连。借用后才坏的,
  沿用现有惰性重连。
- **两代同时持有同一 client**:只发生在并存窗口,旧 agent 空闲(有活动回合时切换被拒);
  client 本身支持并发请求(按 JSON-RPC id 对应)。
- **会话授权不外泄**:「始终允许」写在各代 registry 的 `auto_approved_tools`,不在 client 上;
  新会话照配置重新种,会话 A 的内存授权不带到会话 B(写入 `autoApprove` 的那部分本就是持久意图)。

### 4.5 不在本次范围(另立题)

这些路径是**新起一个 `CodingRuntime`**,而不是在同一 runtime 里切换,池帮不到:

- daemon `ensure_headless_runtime`:`/live` 请求的会话或目录与现有 headless runtime 不符时,
  整个 shutdown 再 start(`native_live.rs:518`, `:559`)。可改为在现有 runtime 上做
  `resume_session_with_lease`,让它也吃到连接池——建议作为下一步单独做。
- daemon `/chat`:每个请求一个 runtime,回合结束即关。
- ACP:每个会话一个 runtime。
- 旧 TUIX 的 `/session`、`/bg`、磁盘 `/resume`:整进程级重建 runtime(TUIX 在退役中,不做)。

### 4.6 后续(不在本次范围)

- **`notifications/tools/list_changed`**:目前不处理。连接复用后一个连接活得更久,
  server 端工具列表变了而本端仍用旧列表的窗口变长。收到时重列该 server、更新池条目的
  工具列表,并按 ADR 0002 走一次发布——这会产生新的请求缓存前缀,是有意的。
- **断线主动重连**:目前 stdio 在下一次请求失败时才惰性重连,初次连接失败永不重试。
  可在连接关闭时主动重连,带退避与熔断(限定窗口内的最大尝试次数),独立于本设计。
- **跨进程的工具目录缓存**:按连接标识缓存工具列表(限时),让「新起一个 runtime」的路径
  (daemon `/chat`、ACP、headless runtime 重建)在 server 慢时先拿到工具定义。
  它补的正是 4.5 里连接池够不到的那些路径。

## 5. 待先确认的问题

1. **疑似既有 bug**:`ApplyUndo` / `RestoreSnapshot` 在同一 parts 上重挂树时,新旧两棵树的
   mcp-host 行共享 `tool_names`,旧树卸载会清空这份名单,可能让新树的记录丢失
   (调研阅读推断,未实测)。本设计会碰这块代码,先写判据确认。
   **结论**:推断的「名单被清空」只是短窗口;实测到的是另一个问题——新树只在后台任务里发布,
   重挂后第一轮请求常先于发布发出、没有 MCP 工具。已修(`493e5a028`,已合入 5.2.0):registry
   记下每个 server 上次列出的工具,挂载时先同步发布;旧行卸载只在仍是当前树时清共享名单。
2. 连接标识是否要纳入父进程环境(stdio 子进程继承 daemon 的环境,不止 `${VAR}` 展开值)。
   倾向不纳入:环境在一个进程生命周期内基本不变;需要时 `/mcp reload` 强制重连。
   **结论**:不纳入。
3. `McpClient` 目前是 `Box<dyn McpClient>` 独占于 registry 的 map;池化要改成 `Arc` 共享,
   要确认 stdio `owns_transport_lifetime` 与恢复克隆的所有权在共享下仍然成立。
   **结论**:registry 里本就存 `Arc<dyn McpClient>`;真正拥有子进程的只有一个 `StdioClient`,
   在最后一个 `Arc` 释放时才 kill,恢复克隆不拥有。共享成立,不需要退路。

## 6. 判据(先写判据再实现)

| 判据 | 断言 |
|---|---|
| 同目录切会话复用连接 | 启动 + 新建会话 + 切回:server 进程启动 **1** 次(现在是 3 次) |
| 切换后不等待也看得到工具 | 去掉 `wait_mcp_ready`,切换后第一轮请求即含 `mcp__t__echo` |
| reload 重连 | `/mcp reload` 后进程启动次数 +1,旧进程已退出 |
| 改目录不复用 | 切到另一目录后是新连接,旧目录的进程已退出 |
| 配置改了的 server 重连,其余复用 | 两次切换之间改一个 server 的 args:它重启,另一个不重启 |
| 工具定义不变 | 切会话前后发给 provider 的 MCP 工具定义逐字节相同(请求缓存前缀不破) |
| 非在用 registry 的连接不入池 | 慢 server 启动途中切会话:被替换的进程退出,下次切换接手的是在用连接;pool 单元测试覆盖候选 / 清空后 / 另一 owner 的 put 被拒 |
| 坏连接不复用 | 杀掉子进程后切会话:该 server 重连,工具可用 |
| 授权不外泄 | 会话 A 仅内存授权(持久化失败)的工具,在新会话 B 中仍需审批 |
| 撤回先于改状态 | untrust / logout 后旧 catalog 无 MCP 工具,池已清空,进程已退出 |
| 候选失败不伤旧代 | 让候选 assemble 失败:旧 generation 的 MCP 工具仍可调用,池未清空 |
| 撤销 / 恢复快照不丢工具 | 对应第 5 节问题 1 |

## 7. 实施步骤

1. 判据先行(上表),其中第 1、2 行在当前代码上应为红。
2. capabilities:`McpClient` 改为可共享(`Arc`);新增 `mcp::pool::McpConnectionPool`
   (连接标识、借用条件、owner、settle、整池清空);`McpRegistry` 新增「带池构造」入口,
   借到的 server 直接登记、未借到的照常后台连接并回填池。
3. coding:`PrepareOptions` 增加池句柄;owner 循环持有池;Reprepare 提交后按新 registry
   在用集合淘汰;Reload / ReloadConfig / 撤回路径在 `withdraw_mcp_tools` 之后清空池。
4. 文档:ADR 0002「后果」补一条「已实现 runtime 内按 server 复用」,`docs/mcp.md`
   写明何时复用、何时强制重连(`/mcp reload`)。
5. 跑 `gates/compile.sh`、coding / capabilities(`--features mcp`)/ daemon 测试;
   真机用多个 `npx` server 量切换耗时(改前 / 改后)。

## 8. 风险

- 共享 client 的所有权改造(第 5 节问题 3)是最大不确定点,若 stdio 恢复路径与共享冲突,
  退路是池只收 HTTP + 健康的 stdio,且借用前做一次 `ping`。
- 淘汰时机写错会泄漏进程(没淘汰)或误杀在用连接(淘汰太早);判据里「改目录后旧进程已退出」
  与「候选失败不伤旧代」分别卡这两头。
