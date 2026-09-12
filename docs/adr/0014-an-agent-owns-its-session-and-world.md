# Agent 拥有自己的会话与世界

状态: 已实现(2026-09-12)。承接 [`0013`](./0013-agent-product-host-ui.md) 缺口清单里的多会话项。

## 背景

`session` 行在树根 provide 一份 `SessionLog`,之后 21 处 `ctx.service::<SessionSvc>()`
都默认「树里只有一段对话」。一个 App 等于一个会话;ACP 的 `session/new` 在一个连接上
可调多次、daemon 多 tab、tui 不重启切会话、team 多 agent 各有对话,这些都要一个进程
里同时活多段。

更糟的是一个潜伏 bug:压缩、溢出重试、截断续写、repeat-fuse、memory 注入、telemetry
这些监听器注册在插件自己的 ctx(树根)上,却在处理某个 agent 的请求。它们通过自己的
ctx 找日志,永远找到树根那份——子 agent 的请求会让**父的历史**被压缩。

deepseek-harness 的做法是 `CreateAgentOptions.sessionId`(agent 与日志共用一个身份)、
`setup(agentCtx)`(发布前装配作用域世界)、`initiators` AsyncLocalStorage(监听器解析
「发起这条链的 agent」)。

## 决策

**不引入 session 实体。agent 是唯一的一等实体,会话是它的日志,session id 就是它的
身份。** 三条推论落成代码:

1. **`Agents::create(ctx, CreateAgent)`**(`harness/src/agent.rs`)。`CreateAgent` 带
   `id`(缺则铸)、`cwd`、`parent`、`seed` + `seed_len`、`resume`、`persist`、`setup`。
   创建顺序固定:fork realm → 往 realm 里 provide `SessionSvc` → 按 `cwd` 覆盖 `fs` →
   跑 `setup(realm)` → 进注册表 → 发 `AgentCreated`。发布前没人能看到半装配的 agent。
   `setup` 返回的 `Disposable` 由 agent 持有,`remove` 时一起撤销。
   `seed_len` 显式给:fork 的 seed 是父前缀,resume 的 seed 是自己全量,形状一样含义
   不同,不能从 `seed.len()` 推。
2. **任务局部的「当前 agent」**(`agent::current` / `scoped` / `as_agent`)。`agent-loop`
   的 `drive` 用 `as_agent(agent.ctx(), …)` 包住整个回合;树级监听器改用
   `crate::agent::scoped(&self.ctx).service::<SessionSvc>()`。工具并行执行用的是同一
   task 内的 `FuturesOrdered`,不跨 task,作用域自然继承。
3. **`session` 行只提供 `session-defaults`**(id、resume),由 `CreateAgent::root(ctx)`
   读取,用于前端自己那个 agent。`resume` 从 `session-persistence-jsonl` 行搬到
   `session` 行;持久化行不再 inject `sessions`,按 `committed.session` 给每个 agent
   的日志各落一份,`persist = false` 的(子 agent)不落。

`SessionSvc` 这个槽没有去掉,定义与 24 个取用点不变,变的是谁填、填在哪:每个 agent
的 realm 一份,树根没有。

## 为什么是 realm,不是每会话一个 App

一个 session 一个 App 机制零改动,但代码索引、MCP 连接、模型客户端每个 App 各一份,
跨会话的 recall 和 team 共享父工具都做不了。realm 那条路子 agent 已经走过
(`subagent.rs` 手工给子 agent 造日志、工具集、prompt 注册表),这次把手工动作变成
`Agents::create` 的正式参数,subagent、handle、tui、ui-* 五个创建点走同一条路。

## 闸门

- `tests/agent.rs`:两个顶层 agent 日志独立、cwd 独立、remove 撤销世界;树级
  waterfall 写的事实落在「当前回合的 agent」的日志里而不是另一个的;`session` 行只
  命名前端自己的 agent。
- `tests/harness.rs`:挂完 base bundle 树根**没有** `sessions`——全局日志偷偷回来
  这条测试就红。
- plexus 审计只把根 realm 的 provide 算作行的声明面(`owned_by_in`),agent realm 里
  的 provide 是 agent 的世界,不是行的表面。
- 差分基线 16 场景不变;harness 225 + tui 238 全绿。

## 补:header(同日)

会话有一份**不可变的 header**(`session::SessionHeader`:version、id、created_at、
cwd、parent、inherited),日志对象持有它,JSONL 文件第一行 `{"header": …}` 写它,不占
序号。它不是事件,所以 fork 继承父的事件前缀时**不会**带上父的 header:子会话拿到
自己的 header,`parent` 和 `inherited` 写在里面。header 在 `AgentCreated` 时同步写入
(`begin_sync`),保证先于第一条事件;resume 时从文件读回,`created_at` 是会话的不是
进程的;没有 header 的旧文件照常加载,不在磁盘上补造。

可变的事实走事件:`SessionEvent::Titled`,`log.title()` 只看继承段之后的事件,fork
出来的子会话在被命名前没有标题。持久化缝加 `begin` / `header` / `describe`,
`describe` 给 `session/list` 用:header、标题、回合数、事件数,不用把会话装进 agent。

`seed_len` 的第一个读者就是 header 的 `inherited`。

## 未做

- fork 只有接口(`seed` + `parent`),没有从活日志切前缀并校验(无悬空工具调用)
  的便捷函数。
- 没有人提交 `Titled`:`session-title` 缝仍是按需计算,接到前端或 ACP 的
  SessionInfoUpdate 时再落成事件。
- cwd 覆盖用 `LocalFs::new(cwd)`,不继承树级 `fs-readonly`;ACP 的 `session/new`
  接进来时要决定只读世界如何按 agent 覆盖。
- MCP 集合、模式仍是树级行,按 agent 覆盖走 `setup` 即可,尚无调用方。
- `recall` 与 `describe_self` 在回合外回退到「唯一 agent 的日志」(`OnlySession`),
  这是给单 agent 树的便利,两个 agent 时拒绝而不是猜。
