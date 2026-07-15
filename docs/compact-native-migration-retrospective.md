# `/compact` Native 迁移复盘与后续命令迁移手册

> 最终核对基线：`release/v5.0.0@a102ff814bb685c706b346fa9d29e2481c3680cf`
>
> 结论：`/compact` 专属的 core command/event variant、bridge handler、事件转换和 fallback
> 已删除，达到“legacy 接口面已退役”。一般 core/bridge 协议、`/context`、session/provider、
> goal/loop 以及 daemon 的后续整体架构改造不属于该结论。
>
> 后迁移正确性加固：idle `/compact` 的局部 durable checkpoint 和失败语义见
> [compact-durable-checkpoint-design.md](compact-durable-checkpoint-design.md)。该问题不改变上述
> legacy 退役结论。

## 1. 文档目的

本文不是重新设计 compaction 算法，而是记录首个 slash 命令从 legacy bridge 协议迁移到
kernel-native runtime 的完整过程，回答四个问题：

1. `/compact` 最终走哪条命令、事件和生命周期路径；
2. 哪些 legacy surface 已实际删除，哪些只是名字相似但不属于本命令；
3. 开发和 review 中暴露了哪些容易复发的架构、并发、driver 和流程问题；
4. 下一条 slash 命令怎样按同一套方法小步迁移，避免再次走弯路。

本文的判定对象是“`/compact` slash 命令”，不是“仓库内所有 compression/compaction
相关代码”。判定边界不清是本次最重要的教训之一。

## 2. 最终架构与路径

### 2.1 交互式 TUI

```text
TUI /compact
  -> CodingRuntimeHandle::compact(focus)
  -> CodingRuntimeControl::Compact { generation, focus }
  -> atomcode-coding runtime owner
  -> current kernel AgentHandle.commands
  -> kernel AgentCommand::Compact

kernel AgentEvent::CompactionStarted/Compacted
  -> atomcode-coding runtime owner
  -> CodingRuntimeEvent::CompactionStarted/CompactionFinished
  -> TUI native event consumer
```

`atomcode-bridge` 不接收 compact control，也不消费或转换 kernel compaction event。bridge
只通过 `KernelRuntimeAdapter` 继续处理尚未迁移的其他命令和事件。

### 2.2 CLI 与 clix

- 普通 CLI 负责创建 bridged runtime、在 driver 边界合流 legacy/native receiver，并消费
  `CodingRuntimeEvent`；它没有另一套独立的 `/compact` 字符串发送实现。
- `atomcode-clix` 是独立的 kernel-native driver，其 `/compact` 直接发送 kernel
  `AgentCommand::Compact`，不经过 core/bridge。

因此以后盘点“CLI 是否迁移”时，必须分清：

```text
命令解析/发送方
runtime 组装方
事件消费方
独立 driver（例如 clix）
```

不能用“CLI 消费了 native event”代替“所有 CLI 命令发送点已核对”。

### 2.3 daemon 的两类路径

daemon 当前存在两个不同场景，不能混成一条链路：

1. live kernel runtime：把 kernel `CompactionStarted/Compacted` 转成
   `CodingRuntimeEvent`，再映射到 daemon streaming surface；
2. `POST /command` 离线 `/compact`：读取持久化 session，调用
   `atomcode_coding::runtime::compact_snapshot`，再写回 session。

离线路径已经不调用 core compression，也不经过 bridge，但它是 stateless snapshot
执行路径，不是 `CodingRuntimeHandle -> runtime owner -> live AgentHandle`。所以准确表述是：

> daemon 离线 `/compact` 已脱离 core/bridge fallback，但仍是 daemon 整体新架构落地前的
> 过渡实现。

后续 daemon 重构应按新的 runtime/session 所有权整体处理，不能继续在本切片上堆局部补丁。

### 2.4 kernel 原生边界

以下类型属于 kernel 原生协议，不是 legacy：

```text
atomcode_kernel::event::AgentCommand::Compact
atomcode_kernel::event::AgentEvent::CompactionStarted
atomcode_kernel::event::AgentEvent::Compacted
```

kernel 负责 compaction 的串行执行、sacred floor、net-loss guard、cache epoch、tool pairing
等不变量；`atomcode-coding` runtime 负责稳定控制句柄、replace/stop/shutdown 和 driver-neutral
终态；driver 只负责展示和本地状态恢复。

## 3. 四态验收结论

| 状态 | 最终结论 | 证据 |
|---|---|---|
| ① 逻辑已实现 | 是 | kernel `run_compaction -> CompactionStrategy::plan -> Conversation::apply_plan` |
| ② driver 已切换 | 是 | TUI 走 `CodingRuntimeHandle`；clix 直达 kernel；daemon 离线命令脱离 core compression |
| ③ legacy fallback 仍保留 | 否 | `/compact` 不再有 core command、bridge handler 或 core compaction event fallback |
| ④ legacy 接口面已退役 | 是 | 专属发送点、variant、handler、转换、旧 UI event 和旧测试均已删除或替换 |

这个结论只覆盖 `/compact`。其他 slash 命令仍可通过 `AgentClient` 和 bridge，不能由此宣称
整个 bridge 或 `atomcode-core` 已退役。

## 4. 实际删除和保留的 surface

### 4.1 已删除

- core `AgentCommand::Compact`；
- core `CompactionUi` / `CompactionUiKind`；
- bridge `on_runtime_control` 和 `forward_runtime_control`；
- bridge 对 compact control 的 select 分支；
- bridge `on_kernel_event` 中 compaction started/finished 转换；
- bridge compaction 后主动发送 legacy `ContextStats` 的路径；
- `CodingRuntimeControl::Kernel` 之类的通用 kernel 命令穿透入口；
- 旧的 `spawn_bridged_runtime` 包装入口；
- bridge 私有 `noop_handle()` 和生产代码中的旧 task 等待/替换路径；
- daemon 离线 `/compact` 对 core compression plan/apply 的调用；
- 只证明旧转发路径的测试。

### 4.2 仍保留，但不阻塞 `/compact` 退役

- core `AgentCommand/AgentEvent` 中其他命令和事件；
- bridge 的 session/provider/cd/resume/approval/goal/loop handler；
- core `ContextStats` 和 `/context` 的 legacy 查询协议；
- `atomcode-core::agent::compression` 模块；
- bridge AI session naming 对
  `atomcode_core::agent::compression::run_llm_summary` 的调用；
- daemon 当前持久化/session 边界及后续整体新架构改造。

`run_llm_summary` 当前用于生成 session title。它没有接收 `/compact`、没有修改 conversation、
没有调用 `maybe_compress_history`，因此不是 `/compact` fallback。它暴露的是模块职责放置和
core 整体清理问题，应单独立项，不能反向否定本命令的退役状态。

## 5. 实施分片与为什么必须小步推进

### 5.1 第一片：稳定 native control

先建立 `CodingRuntimeHandle::compact`，让 TUI 不再构造 core `AgentCommand::Compact`，同时
删除对应 core variant 和 bridge command handler。

这一片证明“命令可以逐条迁移”，但当时事件仍经 bridge，因此只能称为命令面退役，不能
称为 `/compact` 整体退役。

### 5.2 第二片：native compaction event

引入 core-free `CodingRuntimeEvent` 和 driver-neutral `CompactionOutcome`，让 CLI/TUI/daemon
消费 native compaction started/finished，并删除 core `CompactionUi` 和 bridge 重复格式化。

这一片解决事件面，但只要 compact control 或 kernel compaction event 仍由 bridge 接触，
就仍不能进入第 4 状态。

### 5.3 第三片：runtime owner 接管 AgentHandle

`atomcode-coding` runtime owner 成为 kernel `AgentHandle` 的唯一所有者：

- compact control 直接进入 owner；
- compaction event 由 owner 截获并进入 native channel；
- 非 compaction command/event 经 adapter 暂时交给 bridge；
- replace/stop/shutdown 只能由 owner 执行。

这个中间态允许一个命令完全绕过 bridge，同时避免一次性重写所有 slash 命令。关键不是
建立第二套 runtime，而是在同一个 AgentHandle owner 上提供两套暂时并存的访问面。

### 5.4 review 加固：补全终态与重建竞争

首次实现只完成了“能发送、能收到结果”，review 后才发现 runtime replace/shutdown 可能
发生在 compaction started 与 compacted 之间。最终增加：

- stable handle generation；
- suspend/resume；
- stop/replace 时继续排空旧 agent event；
- `Completed` 与 `Interrupted` 两类终态；
- 已接收 compact 的 exactly-once terminal；
- CLI/TUI/background 对中断状态的恢复。

这一步说明：涉及运行中 Agent 的命令，迁移完成度不能只看 happy path；runtime 重建是
命令协议的一部分。

## 6. 开发过程中暴露的坑

### 6.1 把“默认 v2”误认为“脱离 bridge”

CLI 和 daemon 默认启动 v2，只说明底层 agent 是 kernel。只要 driver 仍发送 core command、
消费 core event，或者 bridge 仍有对应 handler/fallback，该命令就没有退役。

后续每次必须同时搜索：

```text
driver sender
core command/event variant
bridge command handler
bridge event converter
v1/fallback
driver event consumer
tests and feature flags
```

### 6.2 只新增 native 旁路，没有删除旧路径

新增 `runtime.compact()` 后，如果 core variant 和 bridge handler 仍在，就是双路径，不是退役。
迁移任务必须在设计阶段列出“预计删除项”，交付时列出“实际删除项”。

### 6.3 误建双 runtime 或双 event consumer

渐进迁移不是让 bridge 和新 runtime 各持一个 Agent。一个 session 只能有一个 live
`AgentHandle`，一个 kernel event receiver 只能有一个消费者。否则会出现：

- conversation/session 分叉；
- provider、approval、mode 状态不一致；
- event 被随机一方抢走；
- shutdown/replace 互相踩踏。

正确中间态是单 owner，legacy adapter 与 native handle 都指向同一个 owner。

### 6.4 stable handle 不等于永远可用

provider reload、resume、clear、cd、undo 等操作会替换 AgentHandle。driver 中长期持有的
control handle 必须稳定指向 owner，而不是捕获某一代 agent sender。同时需要显式表示：

```text
available
suspended/restarting
generation
stopped
```

否则旧请求可能落到新 session、新 provider 或 replacement agent。

### 6.5 stop/replace 时丢失 compact terminal

早期 owner 在 stop/replace 中只等待旧 kernel task，没有继续读取旧 `AgentHandle.events`。
如果 TUI 已收到 `CompactionStarted`，而 `Compacted` 留在旧 receiver 中，UI 会永久停在
compacting/Streaming。

修复原则：

1. 停止旧 agent 时继续消费 compaction event；
2. task 结束后再 `try_recv` 排空 receiver；
3. 没有 kernel terminal 的已接收请求产生一次 `Interrupted`；
4. 清空 tracker，禁止重复 terminal。

不能把 runtime 被替换伪装成 `committed=false`。后者表示正常 no-op，而不是中断。

### 6.6 shutdown 与刚刚入队请求的竞争

仅把 available 设为 false 不能覆盖“sender 已读到旧状态、尚未完成 send”的窗口。shutdown
需要先关闭 control receiver，再排空已经被 channel 接收的请求；关闭后的 send 必须失败。
这才形成清晰的线性化边界。

### 6.7 跨 legacy/native channel 不存在天然总序

compaction native event 与其他 legacy event 暂时走不同 channel，driver 本地 merger 只能
提供符合当前 UI 生命周期所需的偏序，不能声称获得了全局严格总序。

本切片保证：

- 同一 native compaction 的 Started 在 Finished 前；
- terminal 最终释放 compacting 状态；
- 晚到的 background terminal 不把 Done/Cancelled/Error 降级为 Idle。

如果后续命令依赖跨协议强顺序，应设计 sequence id 或统一 runtime event stream，而不是
继续增加 `biased select` 假装总序存在。

### 6.8 driver 状态恢复不能只处理成功

至少要分别验证：

```text
manual committed
manual no-op
manual interrupted
auto committed
auto no-op
auto interrupted
runtime unavailable
late terminal after background/turn terminal
```

TUI spinner、phase、last response、context gauge、background state 都可能被晚到或中断事件
污染。driver reducer 必须保持终态单调，不能把更强终态降级。

### 6.9 保持 `/compact` parity，不等于迁移 `/context`

旧 bridge 会在 compaction 后刷新 context usage。事件面迁移后，如果完全删除该行为，TUI
footer 会暂时显示压缩前占用；但直接迁移 `/context` 又会扩大本次任务。

最终采用临时 post-compaction usage projection：`CompactionOutcome` 更新 TUI 本地占用，
下一次真实 provider Usage 再成为权威值。这样保持 `/compact` 体验，同时明确 `/context`
command/event 仍未迁移。

教训是：允许做被迁移命令的必要 parity 修复，但必须标明它不是相邻命令的迁移进度。

### 6.10 daemon 持久化转换可能是有损的

daemon 离线 compact 最初把全部 session message 做 core -> kernel -> core 往返，可能丢失
kernel 模型没有承载的 core-only 字段，例如 tool result metadata 或 provider-specific thinking
数据。

最终让 runtime 返回明确 mutation：

```text
Noop
RewriteOnly
Replace { old_start, old_end, new_end }
```

持久化层复用 surviving core message，只转换 kernel 新生成的 summary/note，并按真实
replacement span 调整 display/turn anchor。

通用教训：跨模型转换只有在证明 lossless 时才能全量 round-trip；否则必须返回 patch/
mutation，让持久化 owner 保留原对象。

daemon 后续将按新架构整体改造。本条只作为迁移风险记录，不建议继续对当前 daemon
过渡结构做无边界修补。

### 6.11 名字相似不等于调用链可达

`atomcode-core::agent::compression` 仍存在，`run_llm_summary` 仍被 session naming 使用；
但这不能推出 `/compact` fallback 仍存在。

判断 legacy 必须以用户入口的可达调用链为准：

```text
用户入口 -> sender -> protocol -> handler -> engine -> event -> consumer
```

对于未被该链路引用的历史模块，应记录为“core 整体清理债务”，不能混入当前命令四态。
反过来也一样：函数改名或移动到新 crate，不代表旧 sender/handler 已经删除。

### 6.12 测试通过不等于接口退役

编译和行为测试只能证明新路径可用，不能证明旧路径不存在。必须额外执行静态退役检查，
确认旧 variant、handler、sender、converter 和 fallback 无生产代码命中。

测试报告还必须写清覆盖位置。例如 daemon kernel translator 覆盖 started/committed/no-op，
Interrupted 的消费覆盖在 live API，而终态产生和并发边界覆盖在 coding runtime owner；不能
笼统写成“某一个文件覆盖全部四态”。

### 6.13 不要因相邻问题打乱迁移节奏

第二切片完成后曾出现“转去迁移 `/context`”的倾向，但 `/compact` 自己仍有 bridge control
和 runtime lifecycle 缺口。正确做法是先完成一个命令的完整垂直退役，再开始下一命令。

review 发现问题时也要判断：

- 是当前命令的正确性/退役阻塞项：本轮修；
- 是后续架构会整体替换的局部问题：记录并延期；
- 是无关产品增强：不计入迁移进度。

### 6.14 Git 和协作流程也是迁移质量的一部分

- 修改完成后先 review 当前 diff 和测试结果；
- 不得未经明确授权自动 commit、push 或创建 PR；
- 用户要求提交时，只 stage 已复核的本任务文件；
- 拉取/合并时阅读双方逻辑，不能用整侧覆盖解决冲突；
- 每个切片单独提交，提交信息表达“删除了哪个 legacy surface”，不要只写“支持 native”。

这样才能在长周期迁移中保留清晰、可回滚的检查点。

## 7. 后续 slash 命令迁移执行模板

### 7.1 修改前

1. 记录 branch、HEAD SHA、worktree；
2. 搜索用户入口和所有 driver sender；
3. 搜索 core command/event variant；
4. 搜索 bridge command/event handler；
5. 搜索 v1、feature flag、fallback 和旧测试；
6. 查看相关文件近期 Git 历史，避免重复实现；
7. 写明本次目标四态和预计删除项；
8. 明确不在范围内的相邻命令和产品增强。

建议保存以下盘点表：

| 检查面 | 当前路径 | 目标路径 | 本次删除 | 本次保留 |
|---|---|---|---|---|
| TUI sender |  |  |  |  |
| CLI/headless/clix sender |  |  |  |  |
| daemon/webui sender |  |  |  |  |
| core command/event |  |  |  |  |
| bridge handler/converter |  |  |  |  |
| runtime lifecycle |  |  |  |  |
| driver consumer/state |  |  |  |  |
| v1/fallback/tests |  |  |  |  |

### 7.2 设计时

按命令性质先分类：

- 纯 driver/local side effect：可直接迁到 local service，但完成后删除旧协议；
- 运行中 Agent command：进入 kernel command 或 runtime native control；
- snapshot/provider/session/working-directory mutation：进入 CodingRuntime 生命周期，不得
  伪装成普通 kernel command；
- goal/loop：按完整 controller 生命周期设计，不复制普通 handler。

所有运行中命令都必须回答：

```text
请求何时算 accepted？
是否有 request id/generation？
正常 terminal 是什么？
cancel/replace/shutdown terminal 是什么？
旧请求能否跨到新 agent/session？
driver 消失时如何释放？
事件是否依赖跨 channel 顺序？
```

### 7.3 实施时

1. 先建立最小 native API；
2. 在一个 driver 打通 happy path；
3. 补齐 runtime replace/cancel/shutdown；
4. 切换其余实际入口；
5. 切换事件消费者和 UI/background reducer；
6. 删除旧 sender、variant、handler、converter、fallback；
7. 删除或改写只验证旧路径的测试；
8. 全仓静态搜索确认旧符号不可达。

不要为了“以后可能需要”提前把完整 bridge command 枚举复制进 runtime。每个切片只增加
本命令所需的最小稳定 API。

### 7.4 验证矩阵

至少覆盖：

| 维度 | 必测场景 |
|---|---|
| 命令结果 | success、no-op、error、unavailable |
| 生命周期 | idle、in-turn、approval、cancel、replace、shutdown |
| 重建 | provider reload、session resume/fresh、working directory change（受影响时） |
| driver | TUI、CLI/headless、clix、daemon/webui 的实际相关入口 |
| UI 状态 | spinner/phase、background terminal、late event |
| 持久化 | snapshot、resume、metadata preservation、anchor/index |
| 退役检查 | old sender/variant/handler/converter/fallback 搜索为空 |

验证顺序：先受影响 crate 的针对性测试，再做实际可行的最广 workspace check。无法运行的
测试必须说明原因，不能把“未运行”写成“通过”。

### 7.5 交付格式

最终说明固定包含：

```text
验证基线
达到的四态
实际删除项
仍可达的 legacy surface
测试结果
唯一下一步
```

如果旧 variant、handler 或 fallback 仍可达，必须写“尚未退役”。如果只是名字相似的
模块仍存在，但不在该命令调用链，应放入独立清理债务，不得错误降低本命令状态。

## 8. `/compact` 最终回归检查清单

### 8.1 静态检查

生产代码中应不存在：

```text
CoreCmd::Compact
atomcode_core::agent::AgentCommand::Compact
CompactionUi
CompactionUiKind
on_runtime_control
forward_runtime_control
CodingRuntimeControl::Kernel
```

kernel `AgentCommand::Compact`、kernel `CompactionStarted/Compacted` 和 clix 对 kernel command
的直接发送应保留，它们属于目标架构。

### 8.2 行为检查

- manual committed 显示 marker；
- manual no-op 显示明确提示；
- auto no-op 静默；
- interrupted 不伪装为 no-op；
- replace/shutdown 后 compacting 状态释放；
- stale generation 不进入 replacement agent；
- background late terminal 不降级已有终态；
- compaction 后 context gauge 先显示投影值，下一次真实 Usage 接管；
- daemon 离线保存不丢 surviving message 的 core-only metadata。

### 8.3 `a102ff81` 提交前验证记录

本次迁移在提交前执行并通过：

```text
cargo test -p atomcode-coding                         104 个单元测试及集成/doc tests 通过
cargo test -p atomcode-bridge                         47 passed
cargo test -p atomcode-daemon                         135 passed
cargo test -p atomcode --bin atomcode                 24 passed
cargo test -p atomcode-tuix compaction_               10 passed
cargo test -p atomcode-config format_compaction       4 passed
cargo check --workspace --all-targets                 passed
git diff --check                                      passed
```

普通 Clippy 检查完成且新 runtime 没有新增告警。workspace 严格 `-D warnings` 仍会被既有
告警阻塞；`cargo check --workspace --all-targets` 当时唯一记录的既有告警是
`atomcode-kernel/tests/liveness.rs` 中未使用的 `SilentStreamProvider`。交付时应明确区分
“本次新增告警”和“仓库既有告警”，不能把非全绿检查省略成“全部通过”。

## 9. 后续使用原则

下一条命令迁移开始前，先复制第 7 节盘点表和验证矩阵，再根据该命令的状态所有权删减，
不要从 `/compact` 的具体类型直接复制实现。应复用的是方法：

> 单 owner、最小 native API、完整 terminal、逐 driver 切换、实际删除 legacy surface、静态
> 退役检查和明确的范围边界。

`/compact` 证明了 slash 命令可以逐条迁移，也证明“逐条迁移”不等于“只改一个发送点”。
真正可复用的最小单位是一个用户命令从入口到 terminal、生命周期、driver 状态和 legacy
删除的完整垂直切片。
