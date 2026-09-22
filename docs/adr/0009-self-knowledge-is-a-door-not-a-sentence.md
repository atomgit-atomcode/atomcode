# 自我认知是一扇门,不是一句话

状态: 已实现(`plugins/self_knowledge.rs`,`tests/self_knowledge.rs`)

## 背景

在真实使用中,用户问了两个问题:

> 你的会话记录记在哪里了

agent 开始 `ls .atomcode/`、读 `knowledge.md`,推理"候选答案:① memory.md
② knowledge.md ③ graph.bin",最后猜了一个错的。

> 你当前的 session_id 是多少

> 不知道,也没法知道 —— 我在脑里没有 session id 这个东西。

**两个答案运行时都握着**:`SessionSvc::id()`(`session.rs:273`)、
`~/.atomcode/sessions/<id>.jsonl`(`plugins/session.rs:190`)。
`session-persistence-jsonl` 每回合都在用前者拼后者。

它不是不知道,是**没有门**。而一个没有门的 agent 会去 grep 仓库猜,
而且猜得很自信 —— 这正是过期文档比没有文档更危险的那个机制,
只不过这次过期的不是文档,是"训练时见过的某个仓库长什么样"。

一个运行时装配的 harness 尤其如此:**没有固定的功能集**,
只有这棵树恰好挂了哪些行。任何编译期烧进去的自我描述都是错的。

## 决策

**给模型一扇通向活树的门,而不是往 prompt 里写一段关于树的话。**

- `describe_self` 工具(`RiskLevel::Safe`,只读):`session` / `services` /
  `tools` 三个切面,全部在调用时从活对象读。
- prompt 片段只承载**跨会话不变**的部分:你是运行时装配的、
  被问到自己是什么就调 `describe_self`、别去翻仓库猜。

路径由**拥有它的那一行**回答:`SessionPersistence::location()` 是 store 的方法,
因为 store 是唯一知道的人。调用方按同一份配置重算路径 = 规则的第二份拷贝,
而**分叉的永远是拷贝**。

## 放弃了什么

### 一、把构成写进 prompt(整棵树的描述)

写下来的那一刻它就是树的第二份表示。`seam_map.rs` 的头注释是这条的正面:
「Nothing here is hand-maintained, so nothing here can go stale.」
已经有一份不会过期的自我描述了,缺的只是接到模型嘴边。

### 二、把 session id 写进 prompt(**先做了,然后被判据推翻**)

诱惑很直接:告诉它 id,就永远不用调工具。第一版就是这么写的。

`tests/front_ends.rs:114`(换前端不能改变模型被告知的内容)当场判红 ——
两个 app 的 prompt 不再相等,因为 id 每个会话都不同。

它替我记着一件我本来知道的事:prompt cache 那场仗的根因 E 就是
「system 逐轮重建」,当时专门把 system prompt 冻结到会话级。
**一个每会话唯一的 system prompt,跨会话前缀缓存全部作废。**
为省一次工具调用押上缓存,这笔账是亏的。

装置:`tests/self_knowledge.rs::the_prompt_stays_identical_across_sessions` ——
两个真实会话,id 必须不同,prompt 必须相同。谁再往片段里塞易变的事实,它判红。

### 三、在片段里说"你是 coding agent"

`tests/harness.rs:296` 判红:拆掉 `persona-coding` 之后 prompt 里不该还有
"coding agent"。**编码是一种特化,是可拆的一行**;self-knowledge 是中立行,
review / security 特化也挂它。措辞越界了,改掉。

## 失效条件

- 出现第二个 UI 或第二种 driver,而它有自己的会话概念 ——
  届时 `describe_self` 的 `session` 切面要问的可能不再是 `SessionSvc`。
- 若将来 provider 支持在 system prompt 中间放缓存断点,
  "易变事实必须离开 prompt"这条的理由会减弱(但不消失:判据仍在守可组合性)。
