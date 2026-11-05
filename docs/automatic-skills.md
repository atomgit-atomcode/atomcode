# 静态调试技能自动加载 / Static debugging skill auto-loading

运行时可在真实用户回合的第一个模型请求前，确定性地提供一个匹配的静态调试
工作流。它保证正文先于任务动作进入上下文，不保证模型一定遵循工作流。低置信
请求继续由模型自主选择；没有通用工具门禁，也不会为了加载技能增加一次模型请求。

## 哪些技能参与

可在已有 SKILL.md frontmatter 声明：

```yaml
---
name: investigate-project
description: Diagnose failed tests and investigate regressions
# 仅支持此类别；多个同优先级候选会跳过自动选择。
auto-load: debugging
---
先复现问题，收集证据，再定位根因并验证最小修复。
```

`auto-load: off` 只关闭自动加载，手动调用不变。未知值或重复声明会产生诊断并关闭
该技能的自动加载。未声明时，只兼容名称为 `systematic-debugging`（可带命名空间）
或 `atomcode-workflows:debug`、且描述明确涉及调试的技能；显式声明优先。

仅匹配有限的中英文明确故障表达，例如“测试失败，帮我修复”“第一次重试等了
200ms，是不是算错了”“Fix this regression”。纯改名、格式化、翻译、文案修改，
以及只出现在引用、代码块中的报错，不应触发。该规则不试图识别所有隐含需求。

## 关闭与安全边界

当前用户消息中写 `[no-auto-skills]` 或明确说“不要使用技能”即可跳过该次自动加载；
引用或代码中的拒绝文字不作为指令。“直接改”本身不等于拒绝技能。
宿主可通过已有 HostPlugins 层关闭整个能力，层片段如下（不是 config.toml 顶层设置）：

```toml
[[patch]]
id = "skills"
config = { auto_load = false }
```

技能仍出现在原目录中，`use_skill` 与用户命令照常可用。用户已经显式加载技能时，
不会再自动加一个调试工作流。

自动展开不执行 `!` 后接反引号的内嵌命令，也不会猜测参数。含动态命令、要求参数
或超出预算的技能直接跳过。模板最多 12 KiB，展开结果最多 16 KiB；实际注入还受
模型窗口预算限制，窗口未知时上限 8 KiB。不截断正文再注入。

自动内容通过现有 StepDecision → Injected(Reminder) 路径写入会话日志，复用安装目录
资源说明。不会激活技能的 allowed-tools，不替代审批，也不直接修改 pre_request 的
临时消息。手动技能调用原有行为不变。

## 恢复与观测

同一 agent 当前上下文已有相同正文时不重复注入；resume 从日志恢复后仍适用。
压缩或撤销移除正文后，新的匹配回合可再次加载。不同 agent 各自判断；内部唤醒与
同一回合的后续工具轮次不自动选择。

日志 target `atomcode_skills` 记录 `loaded` / `skipped` / `disabled`、原因、技能名称和
注入字节数，不记录匹配用的用户原文。常见跳过原因包括 `user_opt_out`、
`no_confident_match`、`ambiguous_skills`、`dynamic_shell`、`arguments_required`、
`body_budget`、`context_budget`、`explicit_skill` 和 `already_loaded`。

## 实测记录（2026-11-05）

基于 `c8c2e7e28` 工作树中的本功能，通过生产 prepare/mount 入口连接
`deepseek-v4.1-flash-1017`（low，单请求输出上限 4096 tokens），按固定顺序执行
14 个独立临时工程用例，无失败重跑，共 29 次模型请求：

| 请求 | 次数 | 结果 |
| --- | ---: | --- |
| 测试失败了，帮我查原因并修好 | 5 | 5/5 首次请求已有正文，先于首个工具动作 |
| 第一次重试等了200ms，是不是算错了 | 5 | 5/5 首次请求已有正文，先于首个工具动作 |
| 改名、格式化、翻译引用中的报错、明确不要使用技能 | 各 1 | 4/4 无自动注入，也未手动调用 use_skill |

使用本机安装的 `systematic-debugging/SKILL.md`，未添加 auto-load 声明，覆盖旧技能
名称兼容路径。只复制正文文件，未复制附属资源。仅开放 read_file、list_directory、
use_skill、list_skills；系统指令限定只读诊断，不修改文件，不执行测试命令。
因此结果证明正文可用与动作顺序，不证明完整调试流程遵循、修复正确性或其他模型
命中率。未使用 CLI/TUI 二进制，不保存真实会话，不改全局配置。

可选评测入口为 `coding/tests/skill_auto_load_live.rs`，默认 ignored，需显式提供配置、
模型、技能源文件和新的报告路径，运行会消耗模型额度。报告只保存布尔结果、工具名、
请求数与耗时，不保存用户原文、模型回答、推理或凭据。

## English summary

The existing skills row may supply one eligible static debugging workflow before
its first request for a real user turn. Opt in with `auto-load: debugging`, opt out
with `auto-load: off`, or put `[no-auto-skills]` in the current user request.
Only narrow English/Chinese debugging patterns participate; ambiguous candidates
and simple or quoted-only tasks are skipped. No embeddings or tool-denial loop is
used. Automatic loading never executes shell expansions or grants allowed-tools.
Bodies enter the canonical event log as runtime reminders, are reused across
resume, and can be loaded again after compaction removes them. Manual invocation
and the original skill catalog remain in place. This is context availability,
not a guarantee that a model will obey the workflow.
