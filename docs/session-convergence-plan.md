# Session / Conversation 收口方案

> 状态：S0～S5 已完成；core session **持久化接口面**达到状态④；后续 live transport 收口也已完成。
>
> 核对基线：`release/v5.0.1@c212245d8916ff02b776122401bf633cb1ef4339`
>
> 当前四态：CLI、TUI、daemon 的生产持久化与磁盘读取均已切到 native；core session 模块、JSON writer、
> manager、磁盘读取投影和直接依赖已删除。legacy JSON 仅由 daemon 私有 DTO 只读导入。后续
> live transport 的实施结果见 [`live-transport-convergence-plan.md`](live-transport-convergence-plan.md)。

## 1. 结论

目标不是把四个文件机械合成一个文件，而是确定每类数据的唯一 owner：

- kernel `SessionSnapshot` 是运行中 conversation 和 resume 的权威数据；
- native session metadata 是列表、命名、working directory 和回合展示统计的权威数据；
- native transcript 是不压缩的 recall 记录，不用于重建完整 runtime snapshot；
- UI-only replay 数据与模型上下文分离；
- core `<id>.json` 最终只作为只读、幂等、显式失败的历史 importer 输入；
- 所有消费者切换后删除 core JSON writer、live mirror 和双向转换，保留 importer 不等于旧格式已删除。

不允许长期双写。迁移期间必须通过明确的 session 级存储所有权决定由谁写，不能同时把
core JSON 和 native 文件当作可相互覆盖的权威源。所有权缺失表示过渡期的未确认状态；
一旦在 native meta 中 commit `owner=native`，所有 core writer 必须拒绝再写该 session，不得等到全局
S4d 才停止。

## 2. 范围与非目标

### 范围

- core `Session/SessionMeta/SessionManager` 与 `ConversationSnapshot`；
- native `SessionSnapshot/SessionMeta/SessionManager/SnapshotHook/TranscriptHook`；
- CLI、TUI、daemon、background 的 list/load/save/rename/delete/resume/undo/session switch；
- headless、clix、ACP 的 native session 路径作为回归范围；
- core ↔ kernel message/snapshot 转换；
- macOS 历史 session 目录迁移；
- session id、project bucket、原子写、损坏文件和跨项目查找语义。

### 非目标

- 不创建通用 `atomcode-protocol`；
- 不创建大而全的 `atomcode-foundation`；
- 不同时迁移 plugin、MCP、LSP、provider 或 tool；
- 不删除用户磁盘上的历史 JSON；只有用户显式删除 session 时才删除该 session 的所有格式；
- 不修改版本号、发布配置或无关 UI；
- 不用长期双写、mtime 猜测或 silent fresh 作为兼容策略；
- 不从 JSONL transcript 重建 runtime snapshot。transcript 不包含完整 system/synthetic/cache epoch 语义。

## 3. 迁移前存储与调用关系

同一 `$ATOMCODE_HOME/sessions/<project_hash>/` bucket 中存在两套存储：

| 文件 | 当前 writer | 当前 reader | 当前职责 |
|---|---|---|---|
| `<id>.json` | CLI/TUI/daemon 的 core `SessionManager` 和 live mirror | CLI/TUI/daemon session UI、API、恢复兼容 | core 完整 session/UI 对象 |
| `<id>.snapshot` | `SnapshotHook`、runtime checkpoint | `CodingRuntime` resume/restore | compacted working set；runtime 权威 snapshot |
| `<id>.meta` | `SnapshotHook`、native rename | clix/coding；daemon 有部分直接扫描 | 快速列表和 turn stats |
| `<id>.jsonl` | `TranscriptHook` | recall | 不压缩的逐回合原始记录 |

当前结构性问题：

- core `list()` 只扫描 `*.json`，native `list()` 只扫描 `*.meta`，同一 session 有两套目录视图；
- core `delete()` 只删除 JSON，native `delete()` 只删除 snapshot/meta/jsonl，可能产生孤儿数据；
- core 与 native 都有 rename 能力，但不同入口可能只更新其中一份；S1b 只保证 native meta 内部的
  rename 与 turn-complete 不互相覆盖，不会同步 core JSON；
- TUI/daemon 会把 kernel snapshot 转成 core snapshot 保存，再在 resume 时转回 kernel；
- 双向转换不能保存全部 kernel sidecar，例如 `cache_epoch`、turn/request counter 和 `MessageMeta`；
- core `load_any()` 支持跨 bucket 按 id 查找，native manager 当前缺少等价入口；
- core timestamp 使用秒，native meta 使用毫秒；
- project hash 必须继续与历史 `Path::hash` 规则字节级一致，否则旧 session 会被“消失”。

## 4. 字段兼容矩阵

迁移前必须用符合真实 legacy schema 的合成 fixture 固化以下映射；“可推导”也必须有唯一算法
和回归测试，不使用真实用户数据。

| 语义 | core JSON | native 当前 | 目标处理 |
|---|---|---|---|
| session id | `SessionId(String)` | meta/binding `String` | 统一校验后保留原值，不重新生成 |
| name | `Session.name` | `SessionMeta.name` | native meta 权威 |
| user rename | `user_renamed` | `user_renamed` | 原样导入，用户命名优先 |
| AI naming | `ai_named` | `ai_named`（S1a additive） | native meta 可表达并在改名时保留；切换命名写入方后才成为权威值 |
| storage owner | 无 | S2 additive | `unconfirmed/legacy/native`；新 session 由 start 路径显式传入，历史缺省才是 unconfirmed |
| import info | 无 | S2 additive | 仅 legacy cutover 写入 schema、原始字节 SHA-256、导入器版本与导入类型 |
| working directory | `PathBuf` | meta `String` | 规范化后保存；保留原 project bucket |
| timestamps | 秒 | 毫秒 | checked `seconds * 1000`；拒绝溢出/非法值 |
| runtime messages | core `MessageContent` | kernel `Message` | importer 单向转换，native snapshot 权威 |
| tool calls/results | core enum / `ToolResultRef` | kernel flat message | 保持 call id、args、success；Ref 只能导入其已持久化 summary，必须记录该既有损失 |
| images | core `ImagePart` | kernel `ImageContent` | media type/base64 原样映射并做大小限制 |
| reasoning | `reasoning_content/thinking_blocks` | reasoning + provider-bound blocks | 保留 text/signature；无法证明 provider 时不得伪造新的 provider 归属 |
| synthetic/internal origin | 已有 | 已有 | 原样保留 |
| cold summaries | 独立 `Vec<String>` | synthetic message + internal origin | 导入一次，禁止重复插入 |
| cache epoch | 缺失 | snapshot 字段 | native 值权威；legacy-only 导入默认 0 |
| turn/request counter | 缺失 | snapshot 字段 | request counter 从 message meta 推导，legacy-only 无 meta 时为 0；若 legacy turn stats 被映射为稳定 turn id，则 turn counter 必须至少等于最大导入 turn id，避免 resume 后复用历史 id |
| display-only messages | `display_messages` | 缺失 | 迁到独立、版本化的 presentation sidecar，不进入模型上下文 |
| turn stats | `turn_count/tool_call_count/duration_ms/total_tokens/used_tokens/ctx_window` | `round_count/tool_call_count/duration_ms/total_tokens/used_tokens/ctx_window`（S1a） | core 每回合的 `turn_count` 映射为 native `round_count`；顶层 `SessionMeta.turn_count` 仍表示已完成用户回合数；现有 `after_message` 下标在 S1d 迁到稳定 `turn_id` 锚点 |
| message/file count | core list 动态计算 | native meta 持久化 | native meta 权威，导入时计算一次 |
| raw transcript | 无等价权威记录 | JSONL | 保持 native；legacy 导入不伪造历史 raw transcript |

### Presentation sidecar 决策

UI-only replay 数据不应塞回 kernel message，也不应继续要求 core `Message`。目标增加一个版本化的
`<id>.ui.json`（名称可在实现前确认），只保存：

- schema version；
- 稳定的 `DisplayAnchor`：v1 只允许 `AtStart` 或 `AfterTurn(turn_id)`，不得使用可被
  compaction/undo 重排的消息数组下标；
- kernel-neutral 的展示文本/结构，不包含可被 provider 发送的 message 类型。

当前 production writer 只在 session 尾部追加纯文本，因此 v1 只支持 user/assistant 纯文本和上述
turn 级锚点。legacy `after_message` 只在 importer 中按确定性规则转换一次：映射到该位置所属的
已完成 turn；空会话映射为 `AtStart`；无法唯一映射时显式返回导入诊断，不静默猜测。
若后续证明存在图片、工具结构或回合中间插入等真实数据，必须在 fixture 中证明后再扩展 schema。

## 5. 目标所有权与 API

继续扩展现有 `atomcode-capabilities::session::SessionManager`，不先新建 repository/foundation crate。
目标能力按职责分三组：

### Runtime store

- `save_snapshot/load_snapshot`；
- snapshot version、cache epoch、turn/request counter 校验；
- compaction checkpoint 和 turn terminal 持久化；
- `SessionLease` 是跨进程 advisory lock 的 RAII guard，由 coding `SessionBinding/CodingRuntime`
  在 session 活跃期间持有；不使用单纯的存在标记或 TTL 猜测存活状态；
- importer、delete 和所有权切换必须持有同一 session 的排他 lease。

### Catalog store

- `list/read_meta/write_meta/latest`；
- `rename/delete/find_by_id_across_projects`；
- AI naming、turn stats、context restore、working directory；
- 逻辑 delete 必须删除 snapshot/meta/jsonl/presentation，并在显式用户删除时同时删除对应 legacy JSON。

### Compatibility importer

importer 暂时放在接入/兼容层，不允许 capabilities 或 coding 依赖 core。第一阶段收口到 CLI/TUI/daemon
已经共享依赖的 daemon 兼容模块（现有 `legacy_convert`，实施时可按职责改名为
`session_compat`）；不新建通用 crate，不让 core 反向依赖 kernel/native 类型，也不继续维护
TUI 和 daemon 两份独立转换实现。

importer 输出 kernel snapshot、native meta 和 presentation DTO；写盘仍通过 native manager 完成。
所有消费者切换后，importer 是 core session 类型唯一允许的生产消费者。

### Session 存储所有权

native meta 在 S2 增加带 serde default 的存储状态，与 import 详情分开：

- `owner` 缺失/未确认：历史过渡状态，必须在 lease 下根据实际文件集合判定；
- `owner=legacy`：core JSON 是唯一可写权威源，native 文件即使存在也只是未 commit 的准备数据；
- `owner=native`：native snapshot/meta/presentation 是唯一可写权威源；
- `import_info`：只在来源为 legacy 时记录 schema、原始字节 SHA-256、导入器版本和导入类型；
  fresh native session 不伪造 `import_info`。

新 session 的 owner 由 runtime/session start 路径显式传入，不根据某个文件当时是否存在来猜测；只有历史
serde 缺省值才进入 unconfirmed 判定。

`owner=native` 是多文件提交点，必须在该 session 的 native list/rename/delete/resume/UI/mutation
路径全部就绪后才最后写入。在该状态可见之前，生产 reader 不得将 staging 文件当成
已切换 session；在其可见之后，core writer 必须 fail-closed，不得继续更新 legacy JSON。

## 6. 导入状态机

禁止用文件 mtime 决定谁覆盖谁。catalog 的只读发现不会自动导入或切换所有权。只有显式进入
native cutover 时才调用下述状态机；调用前必须获取 session 排他 lease，然后以 native meta 中的
存储所有权和 `import_info` 判断：

```text
查找 session
  │
  ├─ meta.owner = native
  │    ├─ snapshot/meta 完整 → 直接使用 native
  │    │    └─ legacy 摘要与 import_info 不同
  │    │          → native 仍保持权威，返回 LegacyChangedAfterCutover 诊断
  │    └─ native 不完整 → 显式报损坏，不用 legacy 自动覆盖
  │
  ├─ meta.owner = legacy
  │    ├─ 未请求 cutover → 继续使用 core JSON，不把 native staging 当权威源
  │    └─ 请求 cutover → 按 legacy JSON 分支重新校验/转换，不信任旧 staging
  │
  ├─ native snapshot + meta 完整、owner 未确认
  │    → native messages 保持权威
  │    → 有 legacy 时只补齐可独立迁移的 metadata
  │    → presentation 必须与 native snapshot 同坐标；保留已有 native 文件，缺失时写空文件
  │    → 最后 commit owner=native，从此禁止 core writer
  │
  ├─ 只有 legacy JSON
  │    → 有界读取、解析、转换到内存
  │    → 校验 tool pairing/schema/path/id/presentation anchor
  │    → 写唯一 staging snapshot/presentation
  │    → 最后写 meta(owner=native + import_info) 作为 commit point
  │    → 从此禁止 core writer
  │
  ├─ 只有部分 native 文件、owner 未确认
  │    → 显式报不完整
  │    → 有 legacy 时可受控重建缺失文件，但不覆盖可验证的 native snapshot
  │    → 重建成功后再 commit owner=native
  │
  └─ 两边都不存在 → NotFound，不得 silent fresh
```

`import_info` 至少记录：

- legacy schema/来源；
- legacy 原始字节的 SHA-256，不对重序列化后的 JSON 计算摘要；
- 导入器版本；
- 是否只补 metadata、还是创建完整 native snapshot。

多文件导入不宣称文件系统级“整体原子 rename”，而使用明确的 commit protocol：

1. 持有 session 排他 lease；
2. 在目标目录使用唯一、`create_new` 的同级 staging 文件，不复用固定 `<path>.tmp`；
3. 全部内容完成 serde、大小、schema、pairing 和 regular-file 校验后才分别 rename 到最终路径；
4. 最后写 meta `owner=native`，该步骤是 reader 可见的 commit point；
5. 失败时保留原文件、清理本事务 staging；崩溃残留由下次持有 lease 的恢复流程识别和清理；
6. 在平台支持时对文件和父目录执行必要的 flush/sync，不把单纯 rename 误称为掉电耐久。

导入成功后不删除、不修改 legacy 文件。重复执行相同 legacy 内容必须幂等；legacy 后来变化时
返回可观察诊断，但 native 仍是权威源，不得自动回写或覆盖。

## 7. 安全与数据完整性

session 文件是外部输入，不能因为位于本地磁盘就默认可信：

- session id 必须限制为安全文件名/合法 UUID 兼容形式，拒绝路径分隔符和 `..`；
- 限制 meta、snapshot 和 JSONL 单文件大小，并限制 JSONL 行长/行数、message 数量、单条文本、
  tool args/result 和 base64 image 大小；读取不得先无界加载到内存再校验；
- schema version 超出支持范围时 fail-closed；
- working directory 必须按现有跨平台规则规范化，不能改变历史 bucket；
- importer 不执行 session 中的命令、模板或路径内容；
- 读取和导入只接受预期根目录内的 regular file，拒绝 symlink、设备和越出根目录的路径；
- 临时文件必须位于目标文件同级，使用唯一名与 `create_new`，commit 前完成 serde、
  大小和 pairing 校验；不得通过可预测 `.tmp` 路径跟随 symlink；
- 写入权限必须至少维持当前平台安全语义；是否统一收紧为 user-only 权限需单独评估，不能在迁移中静默改变；
- 显式删除 session 时必须清理所有 native sidecar 和对应 legacy JSON，避免 raw transcript 泄漏；
- 任何失败都保留原文件，并返回可观察错误，禁止跳过后显示“恢复成功”。

S1b 已为 native meta 的 read-modify-write 引入每 session advisory lock，避免 rename 与 turn stats
互相覆盖。但这不等于同 session 的完整并发安全：core JSON、native snapshot 以及删除时仍在运行的 writer
没有统一所有权或冲突语义。消费者切换前仍必须解决这些生命周期问题，不能把 meta lock 当作多 writer
runtime 的许可。

## 8. 实施工作包

### S0：兼容特征基线

目标：不改生产行为，建立真实数据契约。

实施：

- 在 `atomcode-core/tests/fixtures/session/` 保存两份脱敏、合成的 legacy fixture，分别覆盖完整字段和旧字段缺省；
- 用保持通过的 characterization test 固化 legacy JSON 反序列化、project hash、跨 bucket
  `load_any` 以及 TUI/daemon 当前的 message/tool/reasoning/image/cold summary 转换；
- 盘点 TUI/daemon 的 display message 实际形状，决定 presentation v1 最小 schema；
- 证明或否定多进程同 session 写入场景。

S0 不提交预期失败的测试，也不为尚不存在的 importer、安全校验或冲突状态机伪造既有行为。双格式冲突、
不完整 native、future schema、非法 id 和超限输入等测试在 S1/S2 对应行为实现时按 red-green 增加。

当前盘点结论：

- production 中 `display_messages` 的写入点位于 daemon append API，当前只产生带 `after_message`
  锚点的 user/assistant 纯文本；现有 TUI/daemon replay 也只消费该锚点与 `MessageInfo` 投影。因此
  presentation v1 只需纯文本角色与锚点，但 legacy `after_message` 是可变数组下标，不能直接成为
  native 持久化合约；目标使用 `AtStart/AfterTurn(turn_id)`，不先支持图片或工具结构；
- TUI 与 daemon 都可通过独立 `SessionManager` 保存同一个 core JSON，native meta/snapshot 也没有跨进程
  统一所有权。原子 rename 能避免半文件，但不能避免 rename 与 turn-complete 的
  read-modify-write 丢失更新；S1b 已使用 meta lock 解决该局部冲突，S1e 仍需解决同 session
  多 runtime 的 snapshot 和删除所有权；
- macOS v4.16 前目录迁移依赖平台目录和真实文件布局，非 macOS CI 无法用当前 API 做隔离测试。S0 只记录
  现有路径与行为；在修改该路径前必须增加可注入目录 seam 或在 macOS runner 上补 characterization test，
  不得把未自动化验证写成已覆盖。

删除项：无。状态仍为③。

### S1：Native schema parity

目标：native store 能表达现有 UI/session 行为，但消费者暂不切换。

实施：

- additive 扩展 native meta：AI naming 和完整 turn/context restore 字段；存储所有权与
  `import_info` 随 S2 状态机一起引入，不在没有切换语义时预留死字段；
- 增加 versioned presentation sidecar；
- 增加安全的 session id、大小和 schema 校验；
- 增加 catalog 的跨项目查找和 logical delete；
- native meta 的 read-modify-write 使用每 session advisory lock；runtime snapshot 和删除生命周期仍需
  通过单 writer 所有权或显式冲突语义解决。

删除项：无。S1 新增的单项能力只达到“逻辑已实现”，整体仍为状态③；交付必须写
“legacy writer 尚未退役”，不把局部能力和整体迁移状态混用。

#### S1a：Native meta additive parity（已完成）

- `SessionMeta` 增加带 serde 缺省值的 `ai_named`；旧 `.meta` 可读取并在后续 rename/write 时保留，
  但当前 core AI naming writer 尚未切换，所以该字段只是可表达，不是跨路径权威值；
- native `TurnStat` 增加 `round_count/used_tokens/ctx_window`；`round_count` 表示单个用户回合内的
  LLM 请求轮数，避免与顶层已完成用户回合数 `SessionMeta.turn_count` 混淆；
- `SnapshotHook` 记录最终模型请求的 `prompt + completion` 为 `total_tokens`，并分别保留最终请求的
  `used_tokens`（上下文占用）与 `ctx_window`（上下文窗口），不再混用 token 语义；
- additive 字段全部有 serde default，旧 v1 meta 可向后兼容，因此 `META_VERSION` 继续为 1；
- 未引入 presentation sidecar、存储所有权/`import_info`、消费者切换或并发控制。

实际删除项：无。整体仍处于状态③，core JSON writer、native/core 双路径及 legacy 转换仍可达。

#### S1b：Native meta 并发写保护（已完成）

- 每个 session 使用持久的 `<id>.meta.lock` advisory lock，跨线程、跨进程串行化 native meta 写入；
- `SessionManager::update_meta` 将读取、字段修改和原子写入放在同一锁区间；native rename 使用该 API；
- `SnapshotHook` 使用同一锁区间执行“缺失时创建 + turn stats 更新”；损坏或 future meta 仍由 manager
  返回错误，best-effort hook 会记录错误并跳过该次 meta 更新，不会回退成 fresh meta 覆盖原文件；
- 失败测试固定复现 turn-complete 读旧值、rename 写新值、旧值回写覆盖的交错，修复后名称与 turn stat
  同时保留；
- `.meta` schema 与 `META_VERSION` 均未变化，没有引入 revision 字段。

边界：该锁不合并两个并发 runtime snapshot，不保护 core JSON，也不阻止已删除 session 被仍在运行的
writer 再次创建。实际删除项仍为无，整体仍处于状态③。

#### S1c：Native store 外部输入边界（已完成）

目标：在 importer 读取历史文件前，先让 native store 对路径、大小和 schema 失败可控。

实施：

- 先用失败测试固定非法 session id，至少覆盖空值、`.`、`..`、路径分隔符和绝对路径；
- 允许安全的历史非 UUID id，不为了理想 schema 让旧 session 消失；
- 定义 `SessionStoreError`，至少区分 `InvalidId/NotFound/TooLarge/FutureSchema/Corrupt/Io`；
- meta、snapshot、JSONL 使用有界读取，JSONL 按流处理并限制行长、行数与总量；
- 实现前先盘点 fixture 与正常生产上限，为 meta/snapshot/JSONL、JSONL 行长/行数、message、text、
  tool args/result 和 image 定义命名常量及选择依据；测试固定上限值与上限 + 1；
- future version、截断/损坏文件和超限输入由单文件 reader/recall 返回 typed error；
- 路径构造收口到经校验的 manager API，拒绝 symlink/非普通文件，写入也在落盘前拒绝明确超限数据。

S1c 不改 `list/latest` 的批量扫描合约；“有效条目 + 诊断”由 S1f 一次定义，避免一个损坏文件
拖垮整个 picker。不引入 presentation、存储所有权或消费者切换。删除项：无；整体仍为状态③。

完成情况：

- `SessionStoreError` 已区分 `InvalidId/NotFound/TooLarge/FutureSchema/Corrupt/Io`，并额外区分
  `UnsafeFile`；coding 兼容入口只在现有 `io::Result` 边界转换，不把 typed error 静默降级；
- 所有公开的按 id 路径构造 API 都先校验 id：拒绝空值、`.`、`..`、绝对路径、路径分隔符、
  控制/跨平台保留字符和设备名，同时保留安全的历史非 UUID id；
- meta 上限 4 MiB、snapshot 上限 64 MiB、单项目 recall JSONL 总量 512 MiB、单行 16 MiB、
  行数 1,000,000；另限制 snapshot message 数、meta turn stat 数、持久化字符串和 base64 image；
  这些是高于正常上下文载荷的防御上限，不是产品配额；
- meta/snapshot 先检查 regular-file 和文件大小，再有界读取；JSONL 使用流式逐行读取，限制单行、
  单文件和 recall 聚合总量，损坏/future record 由 recall 返回显式错误，不再跳过后假成功；
- snapshot future schema 已下沉到 store reader；meta 文件名 id 与内容 id 必须一致，meta mutation
  不得修改 session id；`list/latest` 仍维持 `Vec/Option` 合约，只在内部跳过无效条目；
- 写入在序列化和结构校验后才落盘；临时文件改为同级唯一名 + `create_new`，读取、append 和 lock
  在 Unix 使用 no-follow 打开并在打开后复核 regular-file，避免固定 `.tmp` 或 symlink 跟随；
- 已覆盖非法/安全历史 id、future snapshot/record、损坏 record、超限、边界/边界 + 1、symlink
  和固定 `.tmp` symlink 回归测试。

实际删除项：无。没有引入 presentation、owner/importer 或 consumer 切换，legacy writer 仍可达，
整体仍处于状态③。

#### S1d：Presentation store 与稳定 replay 锚点（已完成）

目标：用最小的 native sidecar 表达已证明存在的 UI-only replay 数据，并停止把可变消息下标
当作 presentation/turn stat 的长期锚点。

实施：

- 定义 versioned `<id>.ui.json`，v1 只包含 `DisplayAnchor + role + text`，anchor 仅支持
  `AtStart/AfterTurn(turn_id)`；
- native `TurnStat` 增加稳定 `turn_id` 锚点；旧 meta 的 `after_message` 只作兼容输入，新 writer/reader
  不再把可变数组下标当长期标识；
- manager 提供有界读写、append、按稳定 turn anchor 裁剪和 delete；
- 测试 undo/compaction 后锚点要么仍指向存活 turn、要么被明确裁剪，并证明 presentation
  不会进入 provider context；
- 本切片只实现 schema/store/测试，不切生产 writer 或 reader，也不删除 core `display_messages`；
  importer 和生产纵向切换分别由 S2、S4b 完成。

完成情况：

- 新增独立、版本化的 `<id>.ui.json`，DTO 只包含 `DisplayAnchor + role + text`，不引用 kernel
  `Message`；v1 anchor 仅允许 `AtStart` 和非零 `AfterTurn(turn_id)`，因此 display-only 数据不会
  混入 runtime snapshot 或 provider context；
- presentation reader/writer 复用 S1c 的 id、regular-file、symlink、大小和原子写安全边界，限制
  文件、entry 数和单条文本大小；manager 提供 read/write/append、按存活 `turn_id` 裁剪和 delete；
- 新增确定性的 legacy position 转换：`after_message=0` 映射 `AtStart`，其余位置映射到第一个覆盖
  该位置的 completed turn；缺失、越界、重复或非单调 turn map 返回 typed `Corrupt`，不猜测锚点；
- native `TurnStat` additive 增加 `turn_id`，旧 meta 缺省为 `0` 并继续保留 `after_message` 作为未转换
  标记；`SnapshotHook` 对真实回合写入 kernel-minted `turn_id`，普通 snapshot 缩短只按位置裁剪旧统计，
  native 统计由显式 surviving-turn 集合裁剪；
- 测试覆盖 schema wire shape、旧位置转换、future/超限输入、stable prune、旧 meta 兼容、snapshot
  缩短不误删 native stat、presentation 写入不改变 runtime snapshot，以及 delete 幂等清理 sidecar。

实际删除项：无。生产 UI writer/reader、legacy importer、owner/lease 均未接入，core
`display_messages` 和 legacy writer 仍可达；整体仍处于状态③。

#### S1e：Active session 单 writer 所有权（已完成）

目标：先在 native `CodingRuntime` 边界建立 active session 单 writer 原语，为后续所有权切换提供
可转移的排他 guard；生产逻辑 session 的全局单 writer 要等 legacy writer 进入 S2c owner gate、
并在 S4b cutover 时使用同一 lease 才成立。

实施：

- manager 提供跨进程 advisory lease，获取使用非阻塞 `try_lock`，冲突立即返回 `SessionInUse`；
  coding `SessionBinding/CodingRuntime` 持有 RAII `SessionLease`；
- fresh/resume 获取，session switch/shutdown/drop 释放；进程崩溃由 OS 释放 lock，不用 TTL 猜测；
- 不尝试合并两份并发 runtime snapshot；
- 同一 native session id 的重入、native manager 运行中 delete 和后台 runtime 冲突返回显式
  “session in use”错误；
- 覆盖所有使用同一持久 session id 创建 `CodingRuntime` 的 CLI/TUI/daemon deferred、
  headless/background 路径；clix/ACP 作为已走 native 路径的回归消费者。

完成情况：

- native manager 新增持久 `<id>.lease` advisory lock 和 cloneable RAII `SessionLease`；获取使用
  `try_lock_exclusive`，不等待，冲突返回 typed `SessionInUse`/`WouldBlock`；lock file 使用 S1c 的
  id、regular-file 和 no-follow 安全边界，普通释放和进程退出均由 OS 关闭文件描述符解锁，不使用 TTL；
- `SessionBinding` 持有 lease，fresh、resume 和 `ExternalSnapshot` 都在读取/使用 session 前获取；
  `prepare/assemble` 直接调用路径也不能绕过 lease；
- 同 session capability reload/reprepare 复用同一 lease clone；切换到其他 session 时先获取目标
  lease，preflight/assemble 成功后才释放旧 lease，冲突或构建失败保留旧 runtime 和旧 lease；
- runtime startup/reprepare 分别暴露 typed `RuntimeStartError::SessionInUse` 和
  `RuntimeError::SessionInUse`；shutdown 在发布 terminal 前释放 lease，startup failure 和 runtime drop
  也会释放，调用者可立即启动替代 runtime；
- native manager delete 在删除数据前尝试获取相同 lease，因此不会删除正由 `CodingRuntime` 持有的
  native session；持久 lease 文件不 unlink，避免旧 inode 和新 inode 同时被锁；S1f 再定义覆盖
  native + legacy 数据的 logical delete；
- 已覆盖第二 owner、最后 clone 才释放、symlink lock、运行中 delete、direct prepare、startup failure、
  runtime drop、同 session reload、跨 session 冲突回滚/lease 转移，以及 daemon deferred runtime
  将冲突投影为 TUI 可消费的显式 Failed 状态。使用相同持久 session id 的 CLI、TUI、daemon deferred、
  headless/background、clix 和 ACP `CodingRuntime` 创建路径自动继承该约束。

实际删除项：无。meta lock 继续只解决短临界区 RMW，active lease 解决 runtime 生命周期所有权；
生产 catalog、legacy core JSON writer/delete 和 live mirror 尚未切换或删除，整体仍处于状态③。
特别是 daemon `/chat` 目前仍以随机 native id 启动单回合 runtime、再以另一个 core session id 持久化，
不能把本阶段结果解释为生产逻辑 session 已全局单 writer；直接把它改成 `ExternalSnapshot` 会提前制造
native 权威文件，必须由 S2c writer gate 和 S4b 单提交点 cutover 一起解决。

#### S1f：Catalog 原语与完整删除（已完成）

目标：在切消费者前，native manager 先具备 core catalog 的必要语义，delete 强制依赖 S1e lease。

实施：

- 按完整 id/安全前缀跨 project bucket 查找，多匹配返回歧义错误；
- 定义 `CatalogScan { entries, diagnostics }`，每个 entry 至少包含 id、project bucket、working directory
  和物理来源 `LegacyOnly/NativeOnly/Both`；S1f 不在 `owner` 字段出现前猜测
  `unconfirmed/legacy/native` 权威状态，该状态随 S2 状态机一次引入；list/latest/search 不将损坏或
  future meta 伪装成“不存在”，也不因单个损坏文件丢掉其他有效 session；
- 查找先收集全部精确 id，只有唯一命中才返回；没有精确命中时再收集安全前缀。无论精确还是前缀，
  跨 bucket 多匹配都返回带候选位置的歧义错误，不按目录顺序或更新时间猜一个；
- catalog 只用 capabilities 内的有界 metadata DTO 读取 legacy JSON，不依赖 core，也不在只读扫描时导入；
- logical delete 必须接收同一 id、同一 project bucket 的有效 `SessionLease`，先校验所有目标是普通文件，
  再覆盖删除 snapshot/meta/jsonl/ui 和对应 legacy JSON；错误 bucket 的 lease 必须显式拒绝；
- 持久 `.meta.lock`/lease lock 不得在普通 delete 中 unlink，避免旧 inode 与新 inode 同时被锁；
- 保持历史 project hash 字节级兼容；跨 bucket scanner 接收显式 sessions root，作为 macOS 旧目录发现
  后续可注入、可测试的 seam，本阶段不偷偷执行迁移或修改旧目录。

删除项：无；此阶段不先切某一个 driver。

完成情况：

- capabilities 新增 `CatalogScan { entries, diagnostics }`、`CatalogEntry`、`CatalogPresence` 和 typed
  diagnostic；跨 bucket 扫描接收显式 sessions root，缺失 root 返回空 catalog，单个目录/文件损坏不会
  丢掉其他有效条目；`scan_all` 才使用生产默认 root；
- entry 只报告 `LegacyOnly/NativeOnly/Both` 物理来源，不提前猜 S2 的 storage owner；native meta 与
  legacy JSON 同 bucket 同 id 合并为一个条目，排序统一使用毫秒时间；
- legacy catalog 使用 capabilities 内的有界、core-free DTO，限制文件为 64 MiB，并用 `IgnoredAny`
  校验 `messages` 必须是数组而不加载消息内容；文件名/id、working directory、字符串和秒→毫秒溢出
  都显式校验；future/corrupt/oversized/unsafe 文件和 orphan native sidecar 进入 diagnostics；
- `CatalogScan::find` 先收集全部精确 id、再收集安全前缀；精确或前缀跨 bucket 多命中都返回
  `AmbiguousId` 及候选 bucket，不按目录顺序或更新时间猜测；`latest/search_name` 复用同一 scan，调用者
  始终同时保有 diagnostics；
- logical delete 改为必须接收 `SessionLease`，并校验 lease 的 id 与 bucket 路径；先验证全部目标均为
  普通文件，再幂等删除 snapshot/meta/jsonl/ui/legacy JSON，持久 `.lease/.meta.lock` 保留；错误 bucket
  返回 `LeaseMismatch`，symlink 不会导致半删或影响目标文件；
- 已覆盖物理来源合并、corrupt/future 诊断、orphan sidecar、精确/前缀歧义、错误 bucket lease、完整删除、
  重复删除、预校验与 lock 保留。未切 CLI/TUI/daemon 消费者，也未执行 importer。

实际删除项：无。core catalog/delete 和 legacy writer 仍为生产路径，整体仍处于状态③。

### S2：单一 importer

目标：legacy → native 只有一个转换和导入状态机。

#### S2a：收口解析与转换

- 在 daemon 接入兼容模块定义单一 legacy DTO → kernel snapshot/native meta/presentation DTO 转换；
- 用 S0 fixture 固定文本、图片、tool pairing、reasoning、cold summary 和旧字段缺省；
- CLI/TUI/daemon 以及 daemon `/command` 共用该转换，删除 TUI `runtime_convert` 和 daemon command
  中重复的 core → kernel 转换；只保留服务 live mirror 的 kernel → core 方向；
- legacy thinking block 保留签名，但无法从 legacy 数据证明 provider 时保持 `provider=None`，不得按当前
  实现猜成 Anthropic；legacy turn stats 按原有顺序分配单调、非零的稳定 turn id，并同步抬高 snapshot
  turn counter，避免 resume 后复用这些 id；request counter 无可推导数据时保持 0。

完成情况：daemon `legacy_convert` 已提供无写盘副作用的完整转换结果（kernel snapshot、native meta、
presentation），覆盖完整/最小 fixture、tool pairing、时间和计数边界；无法证明来源的 reasoning signature
不再伪造 provider。TUI `runtime_convert.rs` 与 daemon command 中的重复 core → kernel 实现已删除，
TUI/daemon 生产调用统一使用该兼容模块。kernel → core live mirror 仍保留，owner/importer commit 尚未引入，
因此整体仍是状态③。

#### S2b：事务导入与所有权 commit

- 实现第 6 节状态机和 staging/commit/recovery protocol，不宣称多文件整体原子；
- 在此切片引入 `owner` 与 `import_info`，meta `owner=native` 最后写入作为 commit point；
- 只有已经不使用 core session writer 的 native-only driver 才为 fresh session 创建 `owner=native`；
  兼容 driver 在 S4 cutover 前仍为 `owner=legacy`；历史 owner 未确认的 session 必须在 lease 下判定；
- 覆盖 legacy-only、native-only、双格式、partial native、重复导入、staging 崩溃残留和 legacy 后改动；
- 有效 native snapshot 永不被 legacy 覆盖，失败原样传播到 UI/API。

完成情况：native meta 已增加 serde additive 的 `owner/import_info`；daemon 兼容模块在同一 session lease
下处理 legacy-only、native-only、双格式、partial native、重复导入及 legacy 后改动。全部载荷先完成
转换、大小和 schema 校验，唯一 `create_new` staging 残留会在下次持 lease 时清理，snapshot/presentation
先发布，`meta(owner=native)` 最后作为 commit point。已有 native message/cache epoch/request counter 保持
权威；仅在导入稳定 turn anchor 所必需时单调抬高 turn counter，不用 legacy 替换 native messages。

#### S2c：Writer gate 与 owner-aware facade

- 在任何生产入口允许 commit `owner=native` 前，让 CLI/TUI/daemon/background/live 的所有
  session 写入经过同一 owner-aware facade；
- `owner=legacy` 时只允许兼容模块写 core JSON；`owner=native` 时路由到已实现的 native 操作，
  不存在 native 实现的操作必须阻止 cutover，不能在切换后才向用户报功能不可用；
- owner gate 不能是各 driver 复制的 `if`；运行中旧 writer 未持有 lease 时也不得绕过；
- S2 只交付 importer/facade/gate 机制和测试，不对兼容 driver 的生产 session 执行 `owner=native`
  提交；真正 cutover 在 S4b 按 session 纵向完成；
- 失败测试固定：人工构造 `owner=native` 后直接调用旧 core turn-complete、rename、
  append UI-only、background save 均被拒绝且不改变 legacy JSON；成功路由到 native 由 S4a/S4b 测试。

方案校正与完成情况：没有新增一个跨 core/native、会在 S5 立即删除的大 facade。所有权规则下沉到两个
真实写入底边：core `SessionManager` 的普通/指定 bucket save 与 delete 统一拒绝 `owner=native`；native
snapshot/meta/presentation/transcript 的普通 writer 统一拒绝 `owner=legacy`，只有持有匹配 lease 的 importer
commit 可发布切换。这样 daemon/TUI/background/live 的现有 core 保存调用自动受同一 gate 约束，driver
不复制 `if owner`。各操作成功路由到 native 仍按 S3/S4 纵向切换。

#### S2d：Legacy 发现

- catalog 同时枚举 native meta 和经校验的 legacy candidate，不让 legacy-only session 在切换后消失；
- 明确只读列表/查看不触发导入；恢复或写操作只能通过 S4b cutover 进入 importer，
  不用 mtime 猜测权威源；
- 保留 macOS 旧目录发现行为，修改前增加可注入目录 seam 或 macOS runner 测试。

完成情况：S1f 的统一 catalog scanner 已同时枚举受限 native meta 与 legacy candidate，并输出物理来源和
逐项诊断；list/search/find 本身不调用 importer。scanner 接受显式 sessions root，保留 macOS 旧目录的
可注入测试 seam；生产旧目录迁移行为尚未删除。

预计删除：重复的 core → kernel import helper；服务 live mirror 的 kernel → core 转换暂留。状态仍为③。

### S3：Catalog 与用户操作切换

目标：按“一类操作的全部消费者”纵向切换，避免一次提交同时改完所有用户操作。

#### S3a：只读 catalog

- CLI `--continue`、TUI picker/resume lookup、daemon list/search/resolve 同时切换到 native catalog；
- legacy-only 条目通过 S2d 只读发现，列表不触发导入或所有权切换；损坏或歧义显式展示错误；
- 删除这类操作的 core list/load_any 和重复扫描调用点。

完成情况：CLI `--continue`、TUI picker/resume lookup、daemon list/search/resolve 已使用统一 catalog；
legacy-only 条目保持只读发现，损坏和歧义显式返回，生产入口不再各自扫描 core catalog。

#### S3b：Rename 与 AI naming

- 所有入口改用 owner-aware facade；`owner=native` 写 native meta，`owner=legacy` 暂由单一兼容模块
  写 core JSON，保持 `user_renamed` 优先级；
- 删除各 driver 直接调用 core rename/AI naming 的路径；兼容模块的 legacy writer 到 S4d/S5 再删除。

完成情况：所有入口已通过 owner-aware 兼容模块；S4b cutover 后 rename/AI naming 在同一 lease 下先迁移
再写 native meta，legacy JSON 不再被生产写路径修改。

#### S3c：Delete

- 所有入口改用 owner-aware facade，在 active lease 下执行删除，运行中 session 显式拒绝；
- `owner=native` 清理 snapshot/meta/jsonl/ui 和对应 legacy；`owner=legacy` 清理 legacy 及非权威 native staging；
  重复删除幂等；
- 删除各 driver 直接调用 core delete 或自行删文件的路径。

完成情况：TUI picker 与 daemon API 已使用统一删除；删除在 native lease 下校验 active session，清理
snapshot/meta/jsonl/presentation/legacy JSON，拒绝路径穿越并保持幂等。

picker/API metadata 和 context restore 随其所属的上述操作切片切换，不另留一个长期双源步骤。
core JSON writer 仍在时，状态仍为③。

### S4：Runtime 与持久化切换

目标：runtime 和 UI 不再通过 core snapshot 往返，最终停止 core JSON writer。

#### S4a：Native 操作纵向就绪

此切片先完成 `owner=native` 所需的全部操作，但不对兼容 driver 的生产 session 提交所有权：

- UI-only append 切片同时实现 native sidecar writer 和 TUI/web/API/background reader，不先删 writer、
  后切 reader；
- undo 切片同时更新 native snapshot/meta/presentation 并切所有调用者；
- compact/checkpoint 切片同时更新 native snapshot/meta/presentation，校验 stable turn anchor、
  cache epoch 和 turn/request counter；
- owner-aware facade 对 `owner=native` 的 list/rename/delete/UI/replay/undo/compact 全部有可用实现；
- 任何一项缺失都必须阻止 S4b cutover，不允许用功能降级作为中间态。

#### S4b：按 session 单提交点切换所有权与 resume

- 非活跃 session 先获取排他 lease；当前进程已持有该 session lease 时，先停止接收新回合、
  完成可确认 terminal checkpoint，再复用/转移同一 guard；其他进程持有时返回 `SessionInUse`，
  不等待或抢锁；
- 对 legacy/unconfirmed session 执行 S2 importer，验证 S4a 的所有 native 操作已就绪；
- 最后 commit `owner=native`，紧接着用 native `SessionMode::Resume` 启动 runtime；所有步骤共享
  同一 lease，并将 guard 移交给 coding `SessionBinding/CodingRuntime` 继续持有，不暴露
  “已导入但 core 仍可写”窗口；
- fresh/session switch 也按相同 owner 规则执行；已切换 driver 的 fresh session 直接为 native；
- S4b 启用后，对 `owner=legacy` 的 rename/AI naming/UI append 等写操作先 cutover 再执行 native 操作；
  显式 delete 可在 lease 下直接清理 legacy 而不导入；只读列表/查看仍不触发 cutover；
- 验证 cwd、session id、provider/model、approval、gateway affinity 和 provider reload/deactivate 语义；
- CLI/TUI/daemon 中仅为 core session 迁移服务的 `ExternalSnapshot` 调用点在各自切换后删除。

#### S4c：Native lifecycle 持久化

- turn terminal、cancel、中途失败、session switch、shutdown 只更新 native snapshot、meta 和 presentation；
- 不丢失最后已确认 checkpoint，不因 provider reload/deactivate 重建错误 session id、cwd 或 lease；
- 删除已切换 session 的 kernel snapshot → core snapshot 持久化往返。

#### S4d：停止 live mirror

- CLI/TUI/daemon/background/live 停止 kernel snapshot → core snapshot 保存和 core JSON 双写；
- 验证 shutdown、cancel、provider reload/deactivate、session switch 和 terminal 持久化；
- 删除 kernel → core 持久化转换及实时 mirror 调用点。

S4d 完成时只能声明“core JSON writer 子接口面已退役”；legacy read importer 仍保留，
整体 session/conversation legacy 接口面仍是状态③，直到 S5 完成才能声明整体达到状态④。

完成情况：fresh/resume/session switch 通过同一 lease 建立或导入 native session；turn terminal、cancel、
undo、compact/checkpoint、rename、AI naming、UI-only append 均只写 native 文件。CLI/TUI/daemon/
background/live 的 core JSON save 与 live mirror writer 已删除。当前可以声明 core JSON writer 子接口面
已退役，但 core 读取投影、反向转换与 importer DTO 仍可达，整体仍是状态③。

#### S4b 补充审计：TUI `/resume` 精确定位与事务提交

2026-07-21 复核发现，上述“resume 通过同一 lease 建立或导入 native session”的完成声明对启动时
`--continue` 成立，但 TUI `/resume` 仍有两个缺口：picker 从当前项目 catalog 得到条目后丢弃
`project_bucket`，Enter 又按 id 全局查询；加载后使用 `RestoreSnapshot` 替换旧 binding 的 snapshot，
而不是让 runtime 切换 `SessionBinding`。因此同一 id 跨 bucket 时会误报歧义，即使加载成功也可能让
UI session 与 runtime 持久化 owner 不一致。S4b 在以下补充项完成前不得视为 TUI 入口已完成：

1. picker 的选择值保留 `(project_bucket, id)`；load、rename、delete 使用同一位置，global find 继续
   fail-closed，不用时间戳或目录顺序消歧；
2. Enter 在目标 bucket 下获取 lease 并执行 legacy/unconfirmed → native 收敛，导入结果和同一 lease
   一起交给 `CodingRuntime` 的 resume reprepare；禁止导入后释放 lease 再重新竞争；
3. runtime 在旧 agent 仍可用时预构建目标 session candidate；成功后才发布新 generation 和
   `SessionChanged`，失败时保持旧 binding、cwd、snapshot、provider、grant 和 UI 不变；
4. TUI 等待一个显式 resume terminal；成功后才提交 `current_session`、telemetry、todo/context 投影和
   replay，失败只显示错误。禁止 `.ok()` 吞掉 delivery/runtime 错误；
5. `RestoreSnapshot` 仅保留给同一 session 内的 undo/局部历史替换，不再承担 session switch；
6. 通用 `SessionChanged` 消费方按事件携带的 working directory 计算 project bucket 并精确读取，避免
   runtime 已成功切换后又走一次全局 ambiguous 查询；WebUI/live resume 也必须使用
   当前 live binding 的 bucket 和同一 lease，`/chat` 携带 session id 时也按请求 working directory
   精确读取；TUI 中另起 `FreshSession + RestoreSnapshot` 的无生产方旧分支直接退役；
7. 回归测试覆盖：同 id 跨 bucket 精确选择、legacy 越界 boundary 导入、目标 lease 冲突、candidate
   构建失败回滚、成功后只写目标 session、旧 generation 迟到事件不污染 UI。

2026-07-21 上述补充项已完成：TUI picker、live 切换和 `/chat` 的定位路径已改为
project-scoped，picker/live 的 cutover lease 会直接转移给 runtime；TUI 的旧二次恢复分支及其
UI event surface 已删除。该子切片已达到退役态，但整体 session 收口仍保留 core 读取投影、
反向转换与 legacy importer，仍是整体状态③。

边界：重复 session 的数据清理不是恢复事务的一部分；没有显式选择和内容比对时不得自动删除或合并。
MCP 启动失败也属于独立 capability 配置问题，不与 session 恢复补丁混做。

### S5：Core session 持久化 surface 退役

前置：生产写路径已经 native-only；接下来的工作只退役磁盘读取投影、兼容 DTO 和 core session
持久化直接依赖，不得重新引入第二条持久化路径。

方案校正：当前 core `LiveSession/TurnExecutor` 的公开签名直接持有 core `Conversation`、`TurnEvent`、
provider/tool 类型。把该内存协议改为 kernel 需要迁移 live transport owner、事件 DTO、审批与多视图回放，
不是 session 磁盘模型的局部替换。为保持垂直切片，本方案只删除 live 路径的 core **磁盘读写/seed 来源**；
运行中 core conversation 投影暂保留并明确列为后续 live transport 任务，不得误报为已退役。

#### S5a：Native loaded-session 聚合读取

- 在 capabilities session store 提供唯一的 native 聚合读取结果，包含 `SessionMeta`、
  kernel `SessionSnapshot` 和 `PresentationFile`；
- 聚合读取严格要求 `owner=native` 且三个权威文件完整、版本有效，不用默认值掩盖 partial commit；
- daemon/TUI 后续只消费该聚合，不再各自拼字段或自行决定缺失文件语义。

完成情况：capabilities 已提供 `LoadedSession` 与严格的 `load_native_session`；只有 `owner=native`
且 meta/snapshot/presentation 三件套都存在并通过版本、大小和内容校验时才返回，未确认 owner 与缺失
sidecar 均显式失败。

#### S5b：Daemon 持久化读取切换

- command context/cost/todo、API detail/replay、live/chat 的磁盘 seed 直接读取 S5a 聚合与 kernel snapshot；
- wire/API DTO 在边界就地映射，不用 core `Session` 充当磁盘中间模型；
- 删除 daemon 的 core `SessionManager/Session/SessionMeta/SessionId` 持久化依赖和只为磁盘加载服务的
  kernel → core session 转换；live transport 边界所需的 message 投影暂保留并集中在一个 adapter。

完成情况：command context/cost/todo、API detail/replay、live/chat 磁盘 seed 已统一读取 native catalog、
`LoadedSession` 与 kernel snapshot；daemon 不再依赖 core `SessionManager/Session/SessionMeta/SessionId`。
kernel → core 的剩余转换集中在 live/provider adapter，只服务仍保留的 core conversation 协议，不读写磁盘。

#### S5c：TUI 持久化会话模型切换

- 将 picker、resume、session switch、background、replay/stats 的内存模型改为 driver-local native view，
  其字段只来自 S5a 聚合；
- 删除 TUI 的 core `Session/SessionMeta/SessionId` 持久化直接依赖；仅 live sync/handoff 协议所需的
  `ConversationSnapshot` 投影集中保留，不得再用于磁盘读写；
- 清理已无真实消费者的 `ExternalSnapshot` 参数/调用点；若仍有非迁移用途，明确保留理由。

完成情况：picker、resume、session switch、background 和 replay 使用 TUI-local session view，字段来自
native catalog 聚合；CLI/TUI 已无 core session 持久化类型依赖。TUI 重复的 kernel → core 转换已删除，
复用 daemon 的 live adapter。`ExternalSnapshot` 仍由 daemon live 单回合执行使用：它把已连接
`LiveSession` 的内存前缀交给临时 native runtime，不是 legacy 磁盘迁移 fallback，因此保留。

#### S5d：Importer legacy DTO 收窄

- 在 daemon 兼容模块定义只用于反序列化旧 JSON 的私有 DTO，并直接转换为 kernel/native schema；
- importer 不再构造或依赖 core runtime/session 类型；
- 保留只读 legacy fixtures 和格式边界，仍不允许生产写 legacy JSON。

完成情况：daemon 兼容模块使用私有、冻结的 `LegacySession/LegacyDisplayMessage/LegacyTurnStat` DTO
反序列化旧 JSON，并直接转换为 kernel snapshot、native meta 与 presentation；不再构造或依赖 core
session runtime/persistence 类型。完整、最小和损坏 fixture 继续覆盖格式边界。

#### S5e：接口面与依赖清理

- 全仓复核并删除生产代码剩余的 core session 持久化类型、manager、磁盘转换和无消费者接口；
- 删除 CLI/TUI/daemon 对 **core session 持久化 API** 的直接依赖。其他 core 功能和 live transport 仍被
  使用时，不得误报为这些 crate 对整个 `atomcode-core` 依赖已删除；
- 调用点搜索、相关 crate 完整测试和真实目录副本 smoke 均通过后，声明 core session 持久化接口面
  达到状态④；core conversation/live transport 仍是独立的状态③任务。

完成情况：已删除 `atomcode-core/src/session.rs`、core 模块导出、旧 manager/DTO 兼容测试，以及
CLI/TUI/daemon 的全部 core session 持久化调用点；共享 project bucket hash 下沉到 leaf config helper，
native store 与 MCP trust 保持既有磁盘 key。全仓搜索不再存在 `atomcode_core::session` 调用。

真实目录副本 smoke 共枚举 1842 个会话：1818 个完成 legacy → native 转换并通过严格聚合读取；11 个
catalog 文件健康诊断被显式报告；24 个结构损坏会话被 fail-closed 拒绝（7 个回合中悬空 tool call、
6 个重复 call id、4 个孤儿/重复 result、7 个非法 turn/presentation 边界）。没有修改真实目录，也没有
用 silent fresh、猜测配对或丢弃统计掩盖损坏数据；验证完成后已删除临时副本，真实目录始终未写入。

因此 core session 持久化接口面达到状态④；上述 24 个历史数据诊断属于显式数据修复/取舍问题，
不是第二条持久化路径或未删除的 legacy API。

最终报告必须分别声明：

- core session **写接口面已退役**；
- core session **磁盘读取接口面已退役**；
- legacy JSON **仍可由 importer 读取，格式本身尚未删除**；
- core live transport 的 conversation 投影 **仍保留、尚未退役**；
- 只有 session 持久化相关删除项全部通过调用点搜索与验证后，才声明该接口面达到状态④，
  不再把 live transport 的独立迁移混入同一个完成结论。

## 9. 验证矩阵

这是核心生命周期和持久化修改，每个行为切片必须先写失败测试。

| 范围 | 必测场景 |
|---|---|
| schema | 旧字段缺省、future version、未知字段、损坏/截断文件 |
| import | legacy-only、native-only、双格式、partial native、重复导入、staging 崩溃残留、legacy 后改动诊断 |
| messages | text、multipart image、tool call/result、失败 result、ToolResultRef、reasoning/signature、synthetic/internal origin |
| snapshot | cache epoch、turn/request counter、compaction 后 resume、dangling tool pairing 修复 |
| metadata | rename、user_renamed、ai_named、cwd、秒→毫秒、turn stats 稳定 turn anchor、used/context window |
| presentation | legacy position 映射、stable turn anchor、undo/compaction 后裁剪、不会进入 provider context |
| ownership | unconfirmed/legacy/native、cutover commit point、owner 后改动、旧 core writer 被拒绝、列表不触发 cutover |
| lifecycle | fresh、resume、session switch、cd、provider reload/deactivate、cancel、shutdown、中途失败 |
| catalog | list/latest/find-any、跨 bucket、Windows path hash、macOS 旧目录 |
| delete | snapshot/meta/jsonl/presentation/legacy JSON 全部清理，重复删除幂等 |
| concurrency | rename 与 turn complete 并发、daemon/TUI 同 session、active lease 冲突、运行中 delete、崩溃后 OS 释放 lease |
| filesystem | 路径穿越、symlink/非普通文件、唯一 `create_new` staging、大小/行数边界、父目录同级 commit |
| driver parity | CLI、TUI、daemon、headless、background 使用同一 session 集合和错误语义；clix、ACP native 路径不回退 |

验证强度：

- 开发中运行当前工作包的最小测试；
- 每个工作包完成后运行 capabilities、coding 及实际受影响 driver 的完整测试；
- 公共持久化 schema、跨 crate 依赖或最终删除 core surface 时运行相关 workspace all-target 检查；
- 最终使用真实 session 目录副本做人工 smoke，不在真实用户目录上直接试迁移；
- 不伪造“导入成功”，任何未覆盖的 legacy 字段或并发语义必须在交付中明确。

最终退役门槛不是某个新 API 可用，而是同时满足：

- 上述 driver 矩阵全部通过；
- 全仓搜索证明 core session writer、live mirror、双向转换和旧调用点已删除；
- 导入、损坏数据、future schema、并发和跨平台路径均有显式结果；
- 使用真实目录副本完成 smoke，并记录可回退方式；
- 未修改版本号或发布配置，除非另有明确任务。

## 10. S0 出口决策

| 问题 | S0 结论 |
|---|---|
| display message 最小 schema | 当前 legacy 形状是 `after_message + role + text`；目标 v1 为 `version + DisplayAnchor + role + text`，anchor 仅为 `AtStart/AfterTurn(turn_id)` |
| native meta 补齐字段 | additive 增加 `ai_named`；turn stat 增加 `round_count/used_tokens/ctx_window`，保留现有 `tool_call_count/duration_ms/total_tokens/errored`；顶层 `turn_count` 语义不变 |
| 同 session 并发写 | 可能；S1b 已用每 session advisory lock 解决 native meta rename/turn-complete 丢更新，但 core JSON、snapshot 与删除生命周期仍需单 writer 所有权或冲突语义 |
| legacy/native 冲突 | 有效 native snapshot 永不被 legacy 覆盖；只允许补可独立迁移的 metadata。presentation 必须与 snapshot 同坐标：已有 native 文件则保留，缺失则写空文件，禁止导入 legacy presentation；最后提交 `owner=native` 并按需写 `import_info` |
| session id 兼容 | 新建值继续使用 UUID；legacy 当前接受任意字符串，S1 校验必须允许安全文件名形式的历史非 UUID id，同时拒绝分隔符、`.`、`..` 和空值 |
| fixture 下界 | `legacy_minimal.json` 省略所有现有 serde additive 字段，覆盖当前代码能推导出的最老可读结构；没有使用真实用户数据 |
| 首个生产切换单元 | S3 先整体切 `list/latest/find-any`，不是只切一个 driver；CLI/TUI/daemon 同步切换并删除 core catalog 读调用点，避免会话集合分裂 |

S1 按行为切片先写失败测试，再实现 native schema parity。S0 的 characterization tests 保持绿色，
不把未来行为提前写成永久失败测试。

后续任务已在 [`live-transport-convergence-plan.md`](live-transport-convergence-plan.md) 完成：
Coding Runtime 成为唯一 owner，core live/turn legacy 接口面已删除。
