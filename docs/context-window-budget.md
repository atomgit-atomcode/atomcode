# 上下文窗口与配置预算 / Context windows and configured budgets

`context_window` 是 AtomCode 为当前模型配置使用的上下文预算，参与用量显示和压缩
决策。它不是向服务端申请更大窗口，也不是客户端实测容量。未设置时使用默认值。
模型资料卡显示当前配置或默认值；保存的配置不记录该数值最初来自手动输入还是模型
发现，因此不推断历史来源。

这是单次请求的上下文容量预算，不是会话累计消费额度。系统提示、历史对话、代码和
工具结果都会占用上下文，也需要为回复预留空间。接近压缩阈值时可压缩历史后继续，
因此会话累计用量可以超过该值。`max_tokens` 单独限制一次回复的输出，账号额度则由
服务商决定。

## 模型发现和手动设置

通过 TUI `/provider` 或 WebUI 服务商设置，从发现列表添加模型时，服务端声明的
`context_window` / `context_length` 可预填建议值。仅返回模型 ID 时，窗口保持未指定，
保存时使用协议默认值。标称容量不一定等于代理、部署或账号允许的容量，保存前应核对。

可在保存前手动调整，也可在配置文件中显式设置。重新发现不会自动改写已保存模型，
WebUI 的发现选择保留已有草稿调整值。TUI 明确选择另一个发现模型会替换表单建议参数，
需要时在选择后再调整。本次说明不改变请求、输出预算和压缩行为。

## `auto` 路由

```toml
[models."freellm/auto"]
account = "freellm"
model = "auto"
context_window = 128000
```

`128000` 是用户选定的预算，不表示路由器保证所有候选模型均有 128K 窗口。应按服务端
实际候选模型及代理限制设置保守值，并为输出和压缩留出空间。候选集合不明确时，没有
通用且保证安全的数值；要可靠使用更长窗口，优先选择已知容量的具体模型或服务端确实
执行的受限路由集合。

响应中的 `model` 只能说明本次使用的模型；即使本次用了 1M 模型，也不会把后续预算
自动提升到 1M，下一次路由仍可能改变。`context_window = 0` 不是自动发现开关。本次
不增加策略配置、后台探测、响应驱动扩容或自动候选集合推断。

## English

`context_window` is the local context budget used for accounting and compaction.
It neither enlarges nor measures server capacity. An omitted value uses the
default. Saved profiles do not retain whether a capacity originally
came from discovery or manual entry, so the UI reports configuration/default
provenance without inventing historical detail.

This is a per-request context budget, not a cumulative session quota. System
instructions, conversation history, code and tool results occupy context, with
room also needed for the reply. Compaction can make room for continued work, so
cumulative session usage can exceed this budget. `max_tokens` separately limits
one reply's output; account quotas are determined by the provider.

When adding from discovery, TUI and WebUI prefill reported capacity metadata;
users can adjust it before saving. ID-only listings use the protocol default.
Rediscovery does not rewrite saved profiles; WebUI preserves existing tuned
draft rows. Selecting another discovered model in TUI replaces the suggestions,
which can then be adjusted. Advertised model capacity may differ from deployment
or account limits.

For `auto`, configure a conservative budget for the actual eligible models and
proxy limits, leaving room for output and compaction. Unknown candidate sets
have no universally safe budget. Prefer a concrete model or an enforced restricted
route when reliable larger capacity is required. A response's model is evidence
for that request, not a promise for the next. AtomCode does not auto-expand the
budget from it; zero is not an automatic-discovery sentinel.
