# 旁路模型缝 `llm-utility` 与会话标题

状态: 已实现(2026-09-12)。承接 [`0014`](./0014-an-agent-owns-its-session-and-world.md) 的
「没有人提交 `Titled`」。

## 背景

会话标题此前只能按需算:`session-title` 缝唯一的提供方拿第一条提问截断,repl 的
`/title` 算完打印就丢,不进日志,`session/list` 和 ACP 的 SessionInfoUpdate 都拿不到。
用户要的是**用模型按第一个问题起名**,而且要**用便宜的模型**、**和模型回答并行**。

这类调用不止起名一个:压缩摘要、`/compact <focus>` 定向摘要、下一步建议、子 agent
的快档,coding 那边都有,缺口清单里标着「缺 tier provider」。它们的共性是:一次性、
结果由程序消费、没有前缀可缓存(模型单价就是全部成本)、不该和对话抢同一个网关的
限速。

## 决策

**1. 一个新缝 `llm-utility`。** face 和 `llm` 一样是 `dyn LlmProvider`,语义是「程序消费
结果的旁路模型」。两个提供方行:`llm-utility-openai-compat`(自己的 `model`,`base_url`
和 key 缺省沿用主行读的环境)和 `llm-utility-replay`(测试用脚本)。BASE 不挂它,
消费者按需解析。

**2. 消费者不回退到 `llm`。** `session-title-model` 找不到 `llm-utility` 时回退到第一条
提问截断,而不是借主模型。理由两条:和第一回合抢同一个 adapter 的限速会把 429 砸在
用户等着的回答上;测试里会吃掉 replay 脚本的下一行,而且因为并行触发,吃哪一行是
不确定的。「便宜模型要显式配置」比「静默用贵的」好。

**3. 怎么起名和什么时候起名是两个行。**
- `session-title-first-prompt` / `session-title-model` 填 `session-title` 缝,回答「怎么起」。
  模型版:system 说「不超过八个词、用用户的语言、不要引号句号」,`max_tokens` 32、
  超时 15 秒,回来后取第一行、去引号、按词数字节封顶、去尾部标点;失败回退截断。
- `session-title-on-first-prompt` 是策略行,回答「什么时候起、落在哪」。监听
  `SessionEventCommitted`,看到 `UserMessage` 且日志没有标题就 spawn 起名任务。第一条
  提问在模型请求发出**之前**就 commit 进日志(`agent_loop.rs`),所以标题和回答并行。
  回来后 commit 一条 `Titled` 事件。每个会话同时只有一个起名任务在飞。

**4. 用户改名永远赢。** 策略行只在没有标题时动手,commit 前再查一次;repl 的
`/title <名字>` 直接 commit `Titled`,事件里最新的一条生效。不给 `Titled` 加「谁起的」。

**5. 默认不开模型起名。** BASE 挂策略行 + 第一条提问版。要模型起名,patch 文件两行:

```toml
[[patch]]
id = "session-title-first-prompt"
name = "session-title-model"

[[insert]]
id = "llm-utility"
name = "llm-utility-openai-compat"
config = { model = "deepseek-v4-flash" }
```

## 权衡过、没做的

**给人看的回复摘要。** 用便宜模型把模型的长回复摘成几行覆盖给用户看。不做,三条理由:
流式 TUI 里用户已经看完了流,摘要来得太晚;模型回复里的命令、路径、代码恰恰不能被
摘要替掉;真正的长输出是工具结果,而工具结果已有折叠。如果要做,形状是一条
`Digest { of: seq, text }` 事件加 tui 的呈现折叠,原文不动、模型可见的消息不动,只对
工具结果开。等 dogfood 发现折叠不够再说。

## 闸门

`tests/session_title.rs` 五条:第一条提问后日志里出现一条 `Titled`,第二个回合不再加;
patch 成模型版且挂 `llm-utility-replay` 时标题来自旁路脚本,主脚本未被消耗,引号句号
被清掉;模型版没有 `llm-utility` 时回退到第一条提问且主脚本未被消耗;用户先起的名字
不被覆盖;删掉策略行会话就没有标题。harness + tui 475 全绿,差分基线未动。

## 下一个消费者

压缩摘要:做一个 `compaction-summary` 行填 `compaction` 缝,把 capabilities 里
`OverflowCompaction` 的 stub + 模型摘要接到 harness,摘要走 `llm-utility`。提醒:摘要
模型不能太便宜,标题起坏了只是难看,摘要写坏了主对话从此带着错误的记忆。
