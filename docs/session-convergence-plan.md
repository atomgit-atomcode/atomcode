# Session / Conversation 收口方案

> 状态：待实施设计；本文件不代表生产路径已经切换。
>
> 核对基线：`release/v5.0.1@0b5fcb3264dafd63236efdd240feea348d5967ad`
>
> 当前四态：native runtime/session 逻辑已实现，但 CLI、TUI、daemon 仍读写 core session JSON，
> live 路径仍有 core ↔ kernel 双向转换，因此整体处于状态③：legacy writer/兼容路径仍可达。

## 1. 结论

目标不是把四个文件机械合成一个文件，而是确定每类数据的唯一 owner：

- kernel `SessionSnapshot` 是运行中 conversation 和 resume 的权威数据；
- native session metadata 是列表、命名、working directory 和回合展示统计的权威数据；
- native transcript 是不压缩的 recall 记录，不用于重建完整 runtime snapshot；
- UI-only replay 数据与模型上下文分离；
- core `<id>.json` 最终只作为只读、幂等、显式失败的历史 importer 输入；
- 所有消费者切换后删除 core JSON writer、live mirror 和双向转换，保留 importer 不等于旧格式已删除。

不允许长期双写。迁移期间必须通过明确的 session 级迁移状态决定由谁写，不能同时把 core JSON 和
native 文件当作可相互覆盖的权威源。

## 2. 范围与非目标

### 范围

- core `Session/SessionMeta/SessionManager` 与 `ConversationSnapshot`；
- native `SessionSnapshot/SessionMeta/SessionManager/SnapshotHook/TranscriptHook`；
- CLI、TUI、daemon、background 的 list/load/save/rename/delete/resume/undo/session switch；
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

## 3. 当前存储与调用关系

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
- core 与 native 都有 rename 能力，但不同入口可能只更新其中一份；
- TUI/daemon 会把 kernel snapshot 转成 core snapshot 保存，再在 resume 时转回 kernel；
- 双向转换不能保存全部 kernel sidecar，例如 `cache_epoch`、turn/request counter 和 `MessageMeta`；
- core `load_any()` 支持跨 bucket 按 id 查找，native manager 当前缺少等价入口；
- core timestamp 使用秒，native meta 使用毫秒；
- project hash 必须继续与历史 `Path::hash` 规则字节级一致，否则旧 session 会被“消失”。

## 4. 字段兼容矩阵

迁移前必须用真实 legacy fixture 固化以下映射；“可推导”也必须有唯一算法和回归测试。

| 语义 | core JSON | native 当前 | 目标处理 |
|---|---|---|---|
| session id | `SessionId(String)` | meta/binding `String` | 统一校验后保留原值，不重新生成 |
| name | `Session.name` | `SessionMeta.name` | native meta 权威 |
| user rename | `user_renamed` | `user_renamed` | 原样导入，用户命名优先 |
| AI naming | `ai_named` | 缺失 | 加入 native meta，防 resume 后重复命名 |
| working directory | `PathBuf` | meta `String` | 规范化后保存；保留原 project bucket |
| timestamps | 秒 | 毫秒 | checked `seconds * 1000`；拒绝溢出/非法值 |
| runtime messages | core `MessageContent` | kernel `Message` | importer 单向转换，native snapshot 权威 |
| tool calls/results | core enum / `ToolResultRef` | kernel flat message | 保持 call id、args、success；Ref 只能导入其已持久化 summary，必须记录该既有损失 |
| images | core `ImagePart` | kernel `ImageContent` | media type/base64 原样映射并做大小限制 |
| reasoning | `reasoning_content/thinking_blocks` | reasoning + provider-bound blocks | 保留 text/signature；无法证明 provider 时不得伪造新的 provider 归属 |
| synthetic/internal origin | 已有 | 已有 | 原样保留 |
| cold summaries | 独立 `Vec<String>` | synthetic message + internal origin | 导入一次，禁止重复插入 |
| cache epoch | 缺失 | snapshot 字段 | native 值权威；legacy-only 导入默认 0 |
| turn/request counter | 缺失 | snapshot 字段 | 从 message meta 推导；legacy-only 无 meta 时默认 0 |
| display-only messages | `display_messages` | 缺失 | 迁到独立、版本化的 presentation sidecar，不进入模型上下文 |
| turn stats | core 字段更全 | native meta 字段较少 | native schema 补齐 round count、used tokens、ctx window 等恢复 UI 所需字段 |
| message/file count | core list 动态计算 | native meta 持久化 | native meta 权威，导入时计算一次 |
| raw transcript | 无等价权威记录 | JSONL | 保持 native；legacy 导入不伪造历史 raw transcript |

### Presentation sidecar 决策

UI-only replay 数据不应塞回 kernel message，也不应继续要求 core `Message`。目标增加一个版本化的
`<id>.ui.json`（名称可在实现前确认），只保存：

- schema version；
- 以 native message position 为锚点的 display event；
- kernel-neutral 的展示文本/结构，不包含可被 provider 发送的 message 类型。

若现状盘点证明 production 只需要纯文本，则第一版只支持纯文本，拒绝为历史未使用字段设计通用
UI 协议。若存在图片、工具结构或其他真实数据，必须在 fixture 中证明后再扩展 schema。

## 5. 目标所有权与 API

继续扩展现有 `atomcode-capabilities::session::SessionManager`，不先新建 repository/foundation crate。
目标能力按职责分三组：

### Runtime store

- `save_snapshot/load_snapshot`；
- snapshot version、cache epoch、turn/request counter 校验；
- compaction checkpoint 和 turn terminal 持久化。

### Catalog store

- `list/read_meta/write_meta/latest`；
- `rename/delete/find_by_id_across_projects`；
- AI naming、turn stats、context restore、working directory；
- 逻辑 delete 必须删除 snapshot/meta/jsonl/presentation，并在显式用户删除时同时删除对应 legacy JSON。

### Compatibility importer

importer 暂时放在接入/兼容层，不允许 capabilities 或 coding 依赖 core。优先集中到一个共享的 core
compatibility module，由 CLI/TUI/daemon 调用；不得继续维护 TUI 和 daemon 两份独立转换实现。

importer 输出 kernel snapshot、native meta 和 presentation DTO；写盘仍通过 native manager 完成。
所有消费者切换后，importer 是 core session 类型唯一允许的生产消费者。

## 6. 导入状态机

禁止用文件 mtime 决定谁覆盖谁。以 native meta 中显式的 import 标记和 legacy 内容摘要判断：

```text
查找 session
  │
  ├─ native snapshot + meta 完整
  │    ├─ 已有 import marker → 直接使用 native
  │    └─ 无 marker、legacy 同时存在
  │          → native messages 保持权威
  │          → 只补齐缺失 metadata/presentation
  │          → 写 import marker
  │
  ├─ 只有 legacy JSON
  │    → 解析和校验
  │    → 转换到内存
  │    → 校验 tool pairing/schema/path/id
  │    → 原子写 snapshot/presentation
  │    → 最后写 meta + import marker 作为 commit point
  │
  ├─ 只有部分 native 文件
  │    → 显式报损坏/不完整
  │    → 有 legacy 时允许受控修复，但不得覆盖有效 native snapshot
  │
  └─ 两边都不存在
       → NotFound，不得 silent fresh
```

import marker 至少记录：

- legacy schema/来源；
- legacy 内容摘要；
- 导入器版本；
- 是否只补 metadata、还是创建完整 native snapshot。

导入成功后不删除、不修改 legacy 文件。重复执行相同 legacy 内容必须幂等；legacy 内容后来发生变化时
必须显式报告冲突，不能自动覆盖 native。

## 7. 安全与数据完整性

session 文件是外部输入，不能因为位于本地磁盘就默认可信：

- session id 必须限制为安全文件名/合法 UUID 兼容形式，拒绝路径分隔符和 `..`；
- 限制单文件大小、message 数量、单条文本、tool args/result 和 base64 image 大小；
- schema version 超出支持范围时 fail-closed；
- working directory 必须按现有跨平台规则规范化，不能改变历史 bucket；
- importer 不执行 session 中的命令、模板或路径内容；
- 临时文件必须位于目标文件同级，commit 前完成 serde 和 pairing 校验；
- 写入权限必须至少维持当前平台安全语义；是否统一收紧为 user-only 权限需单独评估，不能在迁移中静默改变；
- 显式删除 session 时必须清理所有 native sidecar 和对应 legacy JSON，避免 raw transcript 泄漏；
- 任何失败都保留原文件，并返回可观察错误，禁止跳过后显示“恢复成功”。

如果允许 TUI 与 daemon 同时写同一 session，在 native meta 成为唯一权威源前必须引入 revision/lock
方案；否则 read-modify-write 会丢失 rename 或 turn stats。未证明不存在多进程并发前，不能把它当作
“后续优化”。

## 8. 实施工作包

### S0：兼容基线与失败测试

目标：不改生产行为，建立真实数据契约。

实施：

- 从测试构造生成完整 legacy fixture，覆盖所有字段和旧字段缺省；
- 固化 project hash、macOS 旧目录、跨 bucket `load_any`；
- 固化 message/tool/reasoning/image/cold summary 转换；
- 增加双格式冲突、不完整 native、损坏 JSON、future schema、非法 id/超限输入失败测试；
- 盘点 TUI/daemon 的 display message 实际形状，决定 presentation v1 最小 schema；
- 证明或否定多进程同 session 写入场景。

删除项：无。状态仍为③。

### S1：Native schema parity

目标：native store 能表达现有 UI/session 行为，但消费者暂不切换。

实施：

- additive 扩展 native meta：AI naming、完整 turn/context restore 字段、import marker；
- 增加 versioned presentation sidecar；
- 增加安全的 session id、大小和 schema 校验；
- 增加 catalog 的跨项目查找和 logical delete；
- 明确多进程策略：证明单 writer，或实现 revision/lock。

删除项：无。仅达到状态①，交付必须写“legacy writer 尚未退役”。

### S2：单一 importer

目标：legacy → native 只有一个转换实现。

实施：

- 实现上述导入状态机和原子 commit；
- TUI、daemon 共用 importer；
- legacy-only session 首次访问后可以从 native resume；
- native 已存在时只补缺失 metadata/presentation，不覆盖 snapshot；
- 导入错误原样传播到 UI/API。

预计删除：TUI/daemon 重复的 core → kernel import helper；仍服务 live mirror 的 kernel → core 转换暂留。
状态仍为③。

### S3：Catalog 与用户操作切换

目标：所有 session 列表和管理操作以 native catalog 为准。

实施顺序：

1. CLI/TUI/daemon list/latest/find-any；
2. rename 和 AI naming；
3. delete；
4. picker/API metadata 与 context restore。

每一步同时切所有实际消费者，不能让 TUI 和 daemon 展示不同 session 集合。

预计删除：core `SessionMeta` 的生产消费者、core list/load_any/rename/delete 调用点及相应重复扫描逻辑。
core JSON writer 仍在时，状态仍为③。

### S4：Resume、undo 与持久化切换

目标：runtime 和 UI 不再通过 core snapshot 往返。

实施：

- fresh/resume/session switch 直接使用 native `SessionSnapshot`；
- undo/compact/terminal 直接更新 native snapshot、meta 和 presentation；
- UI replay 从 native snapshot + presentation + turn stats 投影；
- 停止 core JSON live mirror 和双写；
- 停止 kernel snapshot → core snapshot → kernel snapshot 往返；
- shutdown、cancel、provider reload 和 session switch 保持 terminal/persistence 语义。

预计删除：live core JSON save 路径、kernel → core 持久化转换、`ExternalSnapshot` 中仅为 core 迁移服务的调用点
（variant 是否保留由其他真实消费者决定）。此时 core JSON write surface 才达到状态④；legacy read importer 仍保留。

### S5：Core session surface 退役

前置：全仓生产代码除单一 importer 外不再使用 core Session/ConversationSnapshot。

实施：

- 将 importer 使用的 legacy DTO 收窄为兼容专用类型；
- 删除 core `SessionManager` 和 live session writer；
- 删除 TUI/daemon 双向 runtime conversion；
- 删除 CLI/TUI/daemon 对 core session/conversation 的直接依赖；
- 保留只读 importer 及真实 legacy fixtures，直到产品明确停止支持旧格式。

最终报告必须分别声明：

- core session **写接口面已退役**；
- live runtime 双模型 **已退役**；
- legacy JSON **仍可由 importer 读取，格式本身尚未删除**。

## 9. 验证矩阵

这是核心生命周期和持久化修改，每个行为切片必须先写失败测试。

| 范围 | 必测场景 |
|---|---|
| schema | 旧字段缺省、future version、未知字段、损坏/截断文件 |
| import | legacy-only、native-only、双格式、partial native、重复导入、legacy 后改动冲突 |
| messages | text、multipart image、tool call/result、失败 result、ToolResultRef、reasoning/signature、synthetic/internal origin |
| snapshot | cache epoch、turn/request counter、compaction 后 resume、dangling tool pairing 修复 |
| metadata | rename、user_renamed、ai_named、cwd、秒→毫秒、turn stats、used/context window |
| presentation | display event 锚点、undo/compaction 后裁剪、不会进入 provider context |
| lifecycle | fresh、resume、session switch、cd、provider reload、cancel、shutdown、中途失败 |
| catalog | list/latest/find-any、跨 bucket、Windows path hash、macOS 旧目录 |
| delete | snapshot/meta/jsonl/presentation/legacy JSON 全部清理，重复删除幂等 |
| concurrency | rename 与 turn complete 并发、daemon/TUI 同 session、revision 冲突 |
| driver parity | CLI、TUI、daemon、headless、background 使用同一 session 集合和错误语义 |

验证强度：

- 开发中运行当前工作包的最小测试；
- 每个工作包完成后运行 capabilities、coding 及实际受影响 driver 的完整测试；
- 公共持久化 schema、跨 crate 依赖或最终删除 core surface 时运行相关 workspace all-target 检查；
- 最终使用真实 session 目录副本做人工 smoke，不在真实用户目录上直接试迁移；
- 不伪造“导入成功”，任何未覆盖的 legacy 字段或并发语义必须在交付中明确。

## 10. 第一实施切片入口条件

开始 S1 前必须完成 S0，并回答：

1. display message 的真实最小 schema 是什么；
2. `ai_named`、turn count、used tokens、ctx window 如何进入 native meta；
3. TUI 与 daemon 是否可能并发写同一 session；
4. legacy 与已有 native snapshot 冲突时，哪些字段允许补齐、哪些绝不覆盖；
5. session id 接受的历史格式和新建格式分别是什么；
6. 第一批 fixture 是否覆盖当前仓库可读的最老 schema；
7. S1 新增字段和 sidecar 后，哪一个生产消费者会在 S3 首先切换，以及届时删除什么。

唯一下一步：执行 S0，只增加 fixture、兼容矩阵验证和失败测试，不修改生产读写路径。
