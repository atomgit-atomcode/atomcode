# 项目全局开发约束

## 适用范围与使用方式

本文件记录当前架构边界和长期开发约束，不再承担历史迁移进度报告的职责。历史方案、旧基线和已完成的 bridge 退役过程应留在 `docs/`，不得继续作为当前实现前提。

涉及以下范围时，设计或修改前必须先核对当前代码、调用方和近期 Git 历史：

- `crates/atomcode-kernel/`；
- `crates/atomcode-capabilities/`；
- `crates/atomcode-coding/` 的 runtime、provider、session、controller；
- CLI、TUI、daemon、ACP、clix 的 runtime、session、command/event 接入；
- daemon 中保留的历史 core JSON 单向 importer 与兼容 DTO；
- 公共协议、持久化格式、审批、安全边界或跨 crate 依赖方向。

本文件描述的是约束，不是永远正确的现状快照。若约束中的事实与当前代码冲突，以当前代码为准；先说明差异，再修正文档或实现，不得按旧路径盲目补代码。

## 当前架构事实

当前 coding agent 的目标调用链是：

```text
CLI / TUI / daemon / background / ACP / clix code
                    │
                    ▼
       CodingRuntimeHandle / DriverCommand
                    │
                    ▼
               CodingRuntime
                    │
                    ▼
          atomcode-kernel Agent
```

- `CodingRuntime` 是 coding agent 的运行时所有者；driver 不应重新持有或重建第二套 live agent 生命周期。
- kernel `AgentCommand/AgentEvent` 是运行时执行边界。coding 产品 driver 应使用 `CodingRuntime`；其他业务 driver 可以驱动其 L2 已装配的 kernel agent，但不得另建第二生命周期 owner，也不得把 provider、session、cd、goal、loop 等 coding 生命周期重新塞进 kernel 命令。
- core legacy `AgentClient/AgentCommand/AgentEvent`、v1 engine 和 `atomcode-bridge` 已退役。不得重新引入 bridge、双 endpoint、v1/v2 选择开关或 core driver fallback。
- `atomcode-kernel`、`atomcode-capabilities`、`atomcode-coding` 的生产依赖必须保持 core-free；尤其禁止 capabilities 反向依赖 core、L2 或前端。
- native `SessionManager/SessionMeta/SessionSnapshot/PresentationFile` 是唯一 session 持久化模型；core session 模块与持久化 API 已退役。历史 core JSON 只允许由 daemon 私有 DTO 单向导入，禁止恢复 legacy writer、core 磁盘投影或双向持久化转换。
- `atomcode-core` crate 已从 workspace 删除；生产代码不得重新依赖或重建同名兼容层。历史 core JSON 只由 daemon 私有 DTO 单向导入。
- daemon/TUI 的 live/provider/UI 投影必须直接使用 kernel/coding 中立类型；持久化读取必须先得到严格 native 聚合。不得借转换层恢复旧 engine 命令、第二 runtime owner、core session 磁盘模型或静默 fallback。

## 架构方向

- 目标是单一状态所有权、清晰依赖方向和可验证兼容性；已经删除的 core 不得以 facade、兼容 crate 或复制状态所有者的方式回流。
- driver 负责输入、展示、传输和明确的本地操作；coding runtime 负责业务生命周期；kernel 只负责中立 agent 循环；capabilities 提供可复用能力实现。
- 不需要运行中 conversation/provider/session 的本地查询或副作用，应留在 driver/local service。需要操作运行中状态的行为必须通过 runtime 定义清楚的命令、事件和终态。
- 新能力优先放入职责正确的现有层。不得把业务语义下沉到 kernel，也不得为了去 core 新建一个无边界的“杂物 crate”。
- 不预设必须先创建 `atomcode-protocol`。只有出现稳定跨进程 schema、非 Rust codegen 或独立版本契约的真实需求时，才拆纯协议叶子；否则复用现有 kernel/coding 中立类型。
- 不预设创建大而全的 `atomcode-foundation`。config、auth、plugin、session、transport、process utilities 应按内聚职责复用现有 crate 或独立拆分，避免产生新的 core。
- 版本号、发布配置和版本策略不属于默认架构收口范围；除非任务明确要求，不得顺带修改。
- 问题修复必须检查同一状态所有权、协议边界和受影响 driver，优先修复共同根因；不得只修点名入口而让其他入口继续使用错误路径。

## Runtime 生命周期不变量

涉及 submit、steer、cancel、approval、request、compact、provider/model reload、session/resume、fresh、undo、cd、goal、loop 或 shutdown 时，必须检查：

- live `AgentHandle`、config、parts、provider、session binding、generation、pending request、snapshot broker 和 controller 是否仍由单一 runtime owner 管理；
- session id、working directory、snapshot、provider 选择、审批 grant、gateway affinity 和持久化目标在重建前后是否保持；
- build/prepare/assemble/restore 任一步失败时，是否显式失败或回滚，而不是静默 fresh、空 snapshot、noop handle 或假成功；
- pending approval/request 在 cancel、reload、session switch 和 shutdown 时是否 fail-closed；
- 旧 generation 的迟到事件是否会污染 replacement runtime；
- 每个 accepted operation 是否都有 success、error、cancel、replace、shutdown 对应终态；
- goal/loop 的互斥、evaluation、held turn、delay/wakeup、cancel 和 turn terminal 是否属于同一生命周期；
- snapshot 的运行时权威来源是否明确，历史 core 数据是否仅作为 importer/兼容输入。

当任务涉及 turn completion 或 compaction 时，先复核现有 `LifecycleHooks::turn_complete`、kernel 终止路径和 `atomcode-capabilities` compaction 实现。没有证明现有 seam 缺失前，不得新增重叠 hook、第二压缩状态机或猜测式回合末补丁。

## 历史兼容面维护

`atomcode-core` 已完成退役。后续兼容工作只允许围绕仍保留的单向 importer 和明确的
wire DTO 展开：

1. 明确当前数据或状态的唯一 owner；
2. 找全持久化格式和兼容入口；
3. 历史格式只能作为边界清晰、可测试的单向 importer；
4. importer 消费者归零后删除对应 DTO、转换和测试；
5. 禁止恢复 legacy writer、双向转换、运行时 fallback 或新的 core facade。

兼容收口以“减少一个 importer、数据模型、转换链或 fallback”为度量。不得以移动文件、
增加 facade、创建新 crate 或净删除行数冒充架构进度。

## 兼容面迁移与退役判定

四态判定只适用于正在删除的旧协议、旧格式、旧 API 或 fallback，不要求普通功能开发套用：

1. **逻辑已实现**：新 owner 已有能力；
2. **消费者已切换**：目标 driver/服务已使用新路径；
3. **legacy fallback 仍保留**：旧入口、旧格式写入、旧 handler 或回退仍可达；
4. **legacy 接口面已退役**：旧调用点、类型、handler、依赖和 fallback 已删除，并通过相关验证。

只有第 4 种状态可以称为“已退役”。兼容格式仍可读取但已成为独立单向 importer 时，必须明确报告 importer 仍保留，不得称为格式已经删除。

退役任务必须基于当前代码检查并报告：

- 所有生产发送点、处理方、事件消费者和持久化读写方；
- CLI、TUI、daemon、headless、background、ACP、clix 中实际受影响的入口；
- 旧类型、handler、feature flag、fallback 和依赖是否仍可达；
- 被删除、迁移或仍保留的测试；
- 新旧格式或协议的失败、取消、恢复和降级语义。

## 修改前检查

普通局部修改按风险执行最小检查。涉及 runtime 生命周期、公共协议、持久化、安全边界、跨 crate 依赖或兼容面退役时，开始修改前必须：

1. 记录当前 branch、commit SHA 和 worktree 状态；
2. 搜索目标符号的生产方、消费者、持久化点和转换边界；
3. 查看相关文件近期 Git 历史，确认任务没有已经实现或改变方向；
4. 写明状态 owner、目标边界和失败语义；
5. 若为退役任务，写明预计删除的旧 surface，而不只是新增内容。

发现 dirty worktree 时保留用户改动；不擅自重置、覆盖、删除或借架构任务重构无关代码。

## 验证与交付

- 修改过程中运行最小相关测试；一个逻辑单元完成后运行受影响 crate 的测试。
- 仅当变更跨 crate、公共协议、持久化格式、workspace 依赖或构建配置时，运行相关 workspace 检查。
- `cargo test` 已完成相同编译验证时，不紧接着重复运行 `cargo check`；代码和环境未变化时不盲目重跑失败命令。
- 纯文档、注释和格式修改可以不运行测试，但必须检查 diff 和文档内部一致性。
- 交付前运行 `cargo fmt --all`；`cargo fmt --all -- --check` 必须退出 0。这是 `check.yml` 的 lint job 里唯一的**阻塞**门（clippy 仍是 report-only）。全仓已经 fmt-clean，所以现在跑它只会动你自己改过的文件——这正是它能当门的前提，别再让它积债。
- 纯格式化改动单独成一个 commit，不要混进功能 commit：全仓 fmt 会碰上百个文件，混在一起的 diff 没法评审，也会让 rebase 冲突面无谓地扩大。
- 若 `--check` 在你没动过的文件上报错，多半是 rustfmt 在 match arm 的长字符串字面量上不收敛（`cargo fmt` 不改、`--check` 照报）。按块形式重写该 arm、字面量用 `\` 续行；不要给这道门加回 `continue-on-error`。
- runtime/协议迁移按实际影响覆盖 CLI、TUI、daemon、headless、background、session resume、approval、cancel、provider reload 和持久化兼容；不受影响的入口无需机械重复验证。
- 退役任务的最终说明必须包含：验证基线、达到的四态、实际删除项、仍保留的 importer/legacy surface、测试结果和唯一下一步。
- 普通功能或修复只需报告行为变化、风险、验证结果和已知未验证范围，不强制套用迁移报告模板。
- 未删除旧类型、handler、依赖或 fallback 时，必须明确“尚未退役”，不得以功能可用替代完成声明。
