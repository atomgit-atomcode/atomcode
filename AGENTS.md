# 项目全局开发约束

## 适用范围与使用方式

本文件记录当前架构边界和长期开发约束，不再承担历史迁移进度报告的职责。历史方案、旧基线和已完成的 bridge 退役过程应留在 `docs/`，不得继续作为当前实现前提。

涉及以下范围时，设计或修改前必须先核对当前代码、调用方和近期 Git 历史：

- `crates/atomcode-kernel/`；
- `crates/atomcode-capabilities/`；
- `crates/atomcode-coding/` 的 runtime、provider、session、controller；
- CLI、TUI、daemon、ACP 的 runtime、session、command/event 接入；
- daemon 中保留的历史 core JSON 单向 importer 与兼容 DTO；
- 公共协议、持久化格式、审批、安全边界或跨 crate 依赖方向。

本文件描述的是约束，不是永远正确的现状快照。若约束中的事实与当前代码冲突，以当前代码为准；先说明差异，再修正文档或实现，不得按旧路径盲目补代码。

## 当前架构事实

当前 coding agent 的目标调用链是：

```text
CLI / TUI / daemon / background / ACP code
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
- native `SessionManager` 是唯一 session 存储（`SessionMeta` 索引、`PresentationFile` 等旁挂）；会话内容是 kernel `session` 词汇的事件日志，`SessionSnapshot` 只是它的投影读法（见下一条与 0024）；core session 模块与持久化 API 已退役。历史 core JSON 只允许由 daemon 私有 DTO 单向导入，禁止恢复 legacy writer、core 磁盘投影或双向持久化转换。
- `atomcode-core` crate 已从 workspace 删除；生产代码不得重新依赖或重建同名兼容层。历史 core JSON 只由 daemon 私有 DTO 单向导入。
- coding 的装配只有一套：`parts::prepare` 建能力图，`runtime::mount` 挂成 plexus 行清单（`on_harness` + `host_rows`）。手写的 kernel 中间件/钩子链(`parts::assemble` / `build_coding_agent`)与 `ATOMCODE_ENGINE` 开关已删除，不得重建第二套装配或引擎选择开关。链式当年的事件流录在 `crates/atomcode-coding/tests/golden/differential/`，差分台对着它回归；那些 golden 不可重录（录它的引擎已经没了），要改就改文件本身并在 commit 里说明理由。
- coding runtime 里**会话的唯一权威是 harness 事件日志**（`docs/adr/0024-the-session-log-is-the-authority.md`）：`SessionManager` 存事件格式会话——`<id>.events`（头一行 + 每条事实 `{seq, at, event}`，流式片段不落盘）与 `<id>.index`（元数据，提交点），旁挂 `.ui` / `.rewind` / `.rewind.txn` / `.todos`；文件名一律避开已发布版本扫描的后缀，回滚不会读到半个会话。日志由 coding 的 `session-store` 行（`coding/src/session_store.rs`，host 把它 swap 进 `session-persistence-jsonl` 的位置，排在建 agent 的行之前）在 runtime 持有的租约下逐条同步追加，第一次写失败即停（上报 uncertain commit，runtime fail-close），之后一条都不再写；`session-native` 的 resume 就是重放这份日志，不得回退到快照或任何别的来源。撤销 / rewind / 恢复是追加事实（`Rewound`，目标不是已有前缀时整段撤回再提交目标对话，`SessionManager::append_conversation_change`），不重写日志；唯一的例外是重建失败时回滚自己刚追加、尚无 agent 读过的那段（`truncate_events`）。已发布版本存成快照的会话在打开时读时转换一次（`open_for_resume` / `open_as_events`），旧文件加 `.migrated` 挪开、不删。`SnapshotHook` / `TranscriptHook` 对事件会话只写索引与旁挂，不写快照与 transcript。harness 自己的 JSONL 行在本产品里关闭；`sessions/harness/` 下的旧 journal 不读。
  - 读者：recall / `/worklog` 用 `events::turn_records` 从日志折出每回合记录（撤回的回合标 `undone`），网页历史读日志里的回合时间，Claude Code hooks 的 `transcript_path` 指向 `<id>.events`；`TranscriptHook` 已删。daemon 的无状态撤销与压缩也是追加事实（`plan_conversation_change`），每回合统计按 `visible_turns` 取舍。人取消时半截回复是 `PartialReply` 事实。索引记 `format_version`，比本构建新的会话在目录里标 `needs_newer_version`、拒绝 resume；读到不认识的事件种类同样按「更新的版本」拒读。
  - 团队成员与 `task` 子 agent 的会话同样落盘：各是一个事件会话，存成 `storage_id`（`<lead>/<name>` 的 `/` 换成 `~`），索引记 `parent`，所以目录 / picker / `--continue` / recall 都不列、单独打开被拒、删 lead 时连带删；`session-store` 在 agent 创建时给它取一份自己的租约、移除时放掉，取不到就不让它运行。成员的创建参数在日志头 `member` 里，`stop` 等成员空闲后提交 `Stopped` 再移除；resume lead 时 team 行按 `children(lead)` 把没有 `Stopped` 的成员带回（角色按当前定义重算）。判据：`harness/tests/team.rs` 的 `a_resumed_lead_brings_back_the_members_it_did_not_stop` 等、`coding/tests/runtime_criteria.rs::a_team_is_kept_and_comes_back_with_its_lead`、`capabilities` 的 `a_delegated_session_is_kept_under_its_parent`。
- `describe_self` 告诉模型的每一句都由**实现那件事的行**（或读 patch/flag 的启动器）经 `self_knowledge::describes` / `describes_live` 登记，`self_knowledge` 自己不假定挂了哪些行、不替任何领域开专用注册表。替换掉一行（`swap`/`disable`）就要接过它该说的话：`session-native` 描述原生会话、`skills-host`/`mcp-host` 描述 SKILLS/MCP、`model` 行描述 `/model` 都是这么来的。工具自己的 `description()` 每次请求都会发给模型，不要再为工具往 `describe_self` 里写第二份；`describe_self` 只补工具描述覆盖不到的：会话、人如何添加/配置能力、前端命令、配置文件（身份已在系统提示里由 persona 行说，不重复）。config.toml 的说明写在读它的代码旁边（`coding/src/config.rs::describe_config_file`，由 `config-file` 行登记；可安全编辑的设置目录由 `atomcode_config::settings::describe_catalog` 渲染）。判据在 `harness/tests/self_knowledge.rs` 与 `coding/tests/runtime_criteria.rs` 的 `the_agent_is_told_where_its_session_really_is`、`a_capability_the_runtime_mounts_itself_still_describes_itself`、`a_runtime_configured_from_a_file_describes_the_file`。
- **凭据（API key、token）绝不进入行配置**：配置树是可打印的数据（`dump_runtime` 原样渲染每行 config），也绝不进入 `describe_self` 的任何回答。需要凭据的行由宿主注册一个持有凭据的插件实例并 `swap` 过去（`tool-web-keyed` 是范例）。判据 `coding/tests/mount_wiring.rs::a_configured_credential_never_enters_the_config_tree`。
- **产品目录是宿主交进来的数据**：库（kernel/capabilities/coding/harness 行/review/auth/codingplan/tui）不写 `.atomcode` 字面量、不读 `ATOMCODE_HOME`，只收 `atomcode_capabilities::ProductDirs`（或从它取出的目录）；harness 行经 `product-dirs` 缝取（`atomcode_harness::product_dirs(ctx)`）。解析环境只在宿主：`atomcode_coding::config::product_dirs_from_env()`。名字只在 `atomcode-config/src/distribution.rs` 写一次，`gates/product-dir.sh` 基线为 0。判据 `coding/tests/runtime_criteria.rs::a_runtime_under_another_name_keeps_to_its_own_dirs`。
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
- CLI、TUI、daemon、headless、background、ACP 中实际受影响的入口；
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

本节只保留当前执行约束。修改编译门、Cargo.lock 处理、测试/构建加速或 profile 前，必须先读 [开发验证的历史依据与实验记录](docs/development-verification-notes.md) 对应小节，核对已验证结论，避免重试已排除方案。历史数字和事故过程不再逐次追加到本文件；有效规则变化时原地更新，证据记入链接文档。

### 编译、测试与格式门

- 修改过程中运行最小相关测试；一个逻辑单元完成后运行受影响 crate 的测试。
- 合之前必须跑 `bash gates/compile.sh`：它覆盖 `cargo check --workspace --all-targets` 及 feature 门控测试的编译。默认 feature 的单 crate 测试看不到所有 consumer 路径；不得把这道编译门改成测试 `run`。
- 跑测试用 `cargo nextest run -p <crate>`，不用 `cargo test`；不要随手 `--workspace`，避免不同 feature 变体和测试链接耗尽磁盘。改门或脚本前按 test binary 数量判断并行收益，不能假定 nextest 无条件更快；实测依据见链接文档。
- nextest 不跑 doctest。需要确认 doc 覆盖时，用 `cargo test --doc -p <crate>` 看实际结果，不能只数代码块。
- 仅当变更跨 crate、公共协议、持久化格式、workspace 依赖或构建配置时，运行相关 workspace 检查。已完成相同编译验证后不紧接着重复 `cargo check`；代码和环境未变时不盲目重跑失败命令。
- 纯文档、注释和格式修改可不跑测试，但必须检查 diff 和文档内部一致性。
- 交付前运行 `cargo fmt --all`，并要求 `cargo fmt --all -- --check` 退出 0；不得绕过阻塞的格式门。纯格式改动单独成 commit，不混进功能提交。
- 若未修改文件在 fmt 后仍不收敛，检查 match arm 长字符串：改用块和 `\` 续行，不得给门加回 `continue-on-error`。

### Cargo.lock 与私有 overlay

- 提交、push 前检查 `Cargo.lock`：`hkdf`、`hmac`、`zeroize_derive` 三个 package 名必须均为 0；合法的 `subtle` / `zeroize` 不在判据内。公开 crypto stub 本身不会污染 lock，私有实现 overlay 在场时才需止血。
- 不确定 index 标志时用 `git ls-files -v Cargo.lock` 核对；`skip-worktree` 只对本 clone 生效，且会阻止目标 lock 不同的 checkout/switch/pull。切换前清除 `--skip-worktree`，需要时再设回；不要因切换报错就擅自还原文件。`assume-unchanged` 不是可靠替代。
- 发现污染时先读链接文档的操作说明，用干净 commit 的 lock 加回其后的合法依赖变化，并逐项审查 diff；不得用 `cargo update` / `cargo generate-lockfile` 修复。`workspace.exclude` 挡不住 optional path 依赖进入 lock，不要重复试；根因需在私有 overlay 的依赖接入方式解决。
- 不得把 `Cargo.lock` 加进 `.gitignore` 或从仓库删除；可复现发布依赖它。不要在没有实现版 overlay 时无谓设置 index 标志。

### 异步时间测试与构建性能

- 新增 async 测试，若被测路径有超时、重试或退避，使用 `#[tokio::test(flavor = "current_thread", start_paused = true)]`，断言仍打在实际可观测结果上。
- 转换既有测试前先查 `elapsed` 和真实并行依赖（`std::thread` / `spawn_blocking` / `block_on`）；只转换慢榜中适用的测试。上界时间断言使用 `tokio::time::Instant`，避免虚拟钟下恒真；要求真实等待的下界测试不得转换。具体陷阱与保留案例见链接文档。
- 不加 `[profile.dev.build-override] opt-level = 3`。不重复尝试已排除的 macOS 链接器替换、sccache 和仅剥测试 binary 调试信息的优化；依据见链接文档。
- 保持依赖 `debug = 0`、workspace dev `debug = "line-tables-only"` 的两档设置；需局部变量和单步调试时临时对单个 crate 设 `debug = 2`。不把未量过的 target 体积/链接收益说成已证实，也不误称可减少最终二进制大小。
- 遇到构建慢先区分 check / test、CPU / I/O，用 real 与 user+sys 实测，不凭直觉改 profile。

### 交付说明

- runtime/协议迁移按实际影响覆盖 CLI、TUI、daemon、headless、background、session resume、approval、cancel、provider reload 和持久化兼容；不受影响入口不机械重复验证。
- 退役任务报告验证基线、达到的四态、实际删除项、仍保留的 importer/legacy surface、测试结果和唯一下一步；未删旧类型、handler、依赖或 fallback 必须说明“尚未退役”。
- 普通功能或修复报告行为变化、风险、验证结果和已知未验证范围，不强套迁移报告模板。
