# 大模型与小模型混合使用

让主会话使用大模型做任务拆分与最终验收，将适合独立完成的简单任务交给小模型。
AtomCode 的 `task` 和 `team` 都支持为子 agent 指定模型；主会话不会因此切换模型。
这是一种由主 agent 编排的工作方式，不是框架自动判断任务复杂度后切换模型。

## 先配同一账号下的两个模型

推荐使用 `provider_accounts` 保存连接信息，在 `models` 中声明两个模型。以下是独立的
`config.toml` 示例；合入已有配置时，将顶层 `default_model` 放在任何 TOML 表之前，
并替换服务地址、环境变量和实际模型名。选择 ID 是 `big` / `small`，不是接口模型名。

```toml
default_model = "big"

[provider_accounts.gateway]
provider = "openai"
base_url = "https://your-gateway.example/v1"
api_key = "${MY_GATEWAY_API_KEY}"

[models.big]
account = "gateway"
model = "your-capable-model"
context_window = 128000
capable_model = 5

[models.small]
account = "gateway"
model = "your-fast-model"
context_window = 64000
capable_model = 0

[subagent]
max_concurrent = 3
max_rounds = 200
```

两个模型必须确实能通过该账号访问。不要为了绕过账号边界，把不同凭据的模型写成同一个
`account`。旧 `[providers.*]` 仍可使用 `capable_model`，但不同 provider 条目的账号身份
需要按实际模型目录核对；共享服务地址或相同密钥字符串不等于模型目录中的同一账号。

## 路径一：让主 agent 用 task 指定模型

在大模型主会话中，可以这样提出任务：

> 先拆分任务。适合独立完成的简单子任务用 task 委派给 small；复杂推理和最终验收由你完成。
> 先用 describe_self(aspect="models") 核对可委派的模型。汇总子任务结果后，检查改动并运行必要验证。

`task` 的 `model` 参数使用模型选择 ID。**不传 `model` 时继承主会话模型**，不是自动选择
小模型。`capable_model` 会影响可委派列表和排序，但不会替每次 `task` 调用选择 fast/capable。
工具和模型目录需要由宿主挂载；若工具不可用，应先检查当前运行方式是否启用了子 agent。

模型自己选择委派目标时，规则是：

- 当前会话模型始终可用。
- 其他候选必须属于同一账号。
- 主模型和候选都有 `capable_model`：候选排名不能高于主模型。
- 两者都没有排名：可以作为未排序候选；框架不会根据名字猜价格或能力。
- 只有一方有排名：该候选不提供给模型自行选择。

因此，推荐给同账号的大、小模型都标排名，并让大模型主持。让小模型主持，再用 `task`
自行选择排名更高的大模型做 review，会被能力上限拒绝。应由大模型主会话验收，或者由用户
预先配置 reviewer 角色。

## 路径二：用 team 角色固定分工

可以在用户目录的 `agents/` 中配置角色。默认目录为 `~/.atomcode/agents/`；使用
`ATOMCODE_HOME` 时为该目录下的 `agents/`。角色 ID 来自文件名。默认先读项目角色，再读用户角色；同名时用户角色覆盖项目角色，宿主配置的额外目录还可继续覆盖。

`implementer.md`：

```markdown
---
permission: worker
difficulty: simple
model: small
when: 实现边界明确、可以独立验证的简单子任务
---
按 lead 给出的范围实现并验证。报告修改、验证结果和未解决问题。
```

`reviewer.md`：

```markdown
---
permission: explore
difficulty: hard
model: big
when: 审查实现、检查边界条件和验证证据
---
检查实现是否满足任务要求，报告具体问题与证据，不修改文件。
```

然后要求主 agent 用 `team` 将简单实现交给 `implementer`，将审查交给 `reviewer`，
由主会话汇总和最终验收。角色是编排约定，不保证模型一定委派；可在请求中明确分工。

成员模型的优先级是：本次 `delegate` 调用的 `model` → 角色文件的 `model` →
`difficulty: simple` 使用已挂载的 `llm-utility` → 主会话模型。
显式模型不存在或不允许委派时会报错，不会悄悄回退。若希望使用角色指定的模型，本次调用
应省略 `model`；调用里传入的值会覆盖角色配置，并按模型自行选择的边界校验。

**跨账号要使用用户目录中的角色。** 用户目录的角色 `model:` 属于用户预先配置，可以
指向目录中存在的另一账号模型；项目目录（默认 `<项目>/.atomcode/agents/`）的角色仍按
模型自行选择的规则检查，不能借仓库文件跨账号。接受不同账号也意味着接受对应的服务商、
数据流向和账单。配置应由用户明确决定。

## 路径三：simple 角色使用辅助模型

不在角色中固定 `model`，而只写 `difficulty: simple` 时，team 会尝试使用辅助模型
`llm-utility`。当前 coding 装配在有模型目录时挂载 `llm-utility-selected`，默认从
**允许委派的同账号模型**中选择有能力排名且排名最低的模型。没有适合的辅助模型时继承
主会话模型。`difficulty: hard` 没有显式模型时也继承主会话，不自动升级到最强模型。
辅助模型还可用于标题等旁路调用，不是每种子任务通用的复杂度路由器。

代码中另有 `subagent_tiers::resolve_tier_keys`：只有 host 的接口模型名匹配到有排名的
配置，且有至少两个参与排名的模型时，才返回最低/最高排名的 fast/capable 选择；否则返回
`None`。这套 tier provider 准备逻辑不等于当前 harness `task` 的默认选模规则。
不要把它的 host 参与条件泛化成“host 未标排名时，所有多模型委派都不生效”。

## capable_model 与 effort 的区别

`capable_model` 是用户配置的相对能力排名，数值越大排名越高；它不是 token 价格、性能
实测结果，也不是思考强度。多个候选同排名时按选择 ID 确定顺序。

`effort` 是某个子 agent 的思考强度，`task` / `team` 调用和角色文件均可指定；配置文件
中的模型默认强度使用 `reasoning_effort`。只填写目标模型支持的档位，不支持时会报错。
提高 effort 不会把小模型切成大模型。

## 如何确认配置生效

1. 用 `/model` 确认主会话选中大模型；核对当前配置中的选择 ID。
2. 让主 agent 调用 `describe_self(aspect="models")`，查看可委派模型及支持的强度。
   该列表有意不列跨账号目标，不能据此判断用户角色配置无效。
3. 先委派一个边界明确的小任务，显式指定 `small`，查看子 agent 实际使用的模型，确认
   子调用的用量记录；不要仅凭回复自称使用了某个模型就认定路由成功。
4. 小模型未出现时，检查账号是否一致、双方是否同时有/无排名，以及主模型排名是否足够。
   角色跨账号失败时，检查文件是否在用户目录，是否被后加载的额外角色目录覆盖，以及调用是否覆盖了 `model`。
5. 主会话负责检查子任务产出和测试证据。子 agent 返回“完成”不等于验收通过。

[Plan / Build 模式模型](mode-models.md) 配置的是主会话跨模式切换时的模型，
与这里同一任务中的子 agent 分工是两件事，可以组合使用。
