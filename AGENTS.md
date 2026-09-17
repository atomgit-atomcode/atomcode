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
- native `SessionManager` 是唯一 session 存储（`SessionMeta` 索引、`PresentationFile` 等旁挂）；会话内容是 kernel `session` 词汇的事件日志，`SessionSnapshot` 只是它的投影读法（见下一条与 0024）；core session 模块与持久化 API 已退役。历史 core JSON 只允许由 daemon 私有 DTO 单向导入，禁止恢复 legacy writer、core 磁盘投影或双向持久化转换。
- `atomcode-core` crate 已从 workspace 删除；生产代码不得重新依赖或重建同名兼容层。历史 core JSON 只由 daemon 私有 DTO 单向导入。
- coding 的装配只有一套：`parts::prepare` 建能力图，`runtime::mount` 挂成 plexus 行清单（`on_harness` + `host_rows`）。手写的 kernel 中间件/钩子链(`parts::assemble` / `build_coding_agent`)与 `ATOMCODE_ENGINE` 开关已删除，不得重建第二套装配或引擎选择开关。链式当年的事件流录在 `crates/atomcode-coding/tests/golden/differential/`，差分台对着它回归；那些 golden 不可重录（录它的引擎已经没了），要改就改文件本身并在 commit 里说明理由。
- coding runtime 里**会话的唯一权威是 harness 事件日志**（`docs/adr/0024-the-session-log-is-the-authority.md`）：`SessionManager` 存事件格式会话——`<id>.events`（头一行 + 每条事实 `{seq, at, event}`，流式片段不落盘）与 `<id>.index`（元数据，提交点），旁挂 `.ui` / `.rewind` / `.rewind.txn` / `.todos`；文件名一律避开已发布版本扫描的后缀，回滚不会读到半个会话。日志由 coding 的 `session-store` 行（`coding/src/session_store.rs`，host 把它 swap 进 `session-persistence-jsonl` 的位置，排在建 agent 的行之前）在 runtime 持有的租约下逐条同步追加，第一次写失败即停（上报 uncertain commit，runtime fail-close），之后一条都不再写；`session-native` 的 resume 就是重放这份日志，不得回退到快照或任何别的来源。撤销 / rewind / 恢复是追加事实（`Rewound`，目标不是已有前缀时整段撤回再提交目标对话，`SessionManager::append_conversation_change`），不重写日志；唯一的例外是重建失败时回滚自己刚追加、尚无 agent 读过的那段（`truncate_events`）。已发布版本存成快照的会话在打开时读时转换一次（`open_for_resume` / `open_as_events`），旧文件加 `.migrated` 挪开、不删。`SnapshotHook` / `TranscriptHook` 对事件会话只写索引与旁挂，不写快照与 transcript。harness 自己的 JSONL 行在本产品里关闭；`sessions/harness/` 下的旧 journal 不读。
  - 读者：recall / `/worklog` 用 `events::turn_records` 从日志折出每回合记录（撤回的回合标 `undone`），网页历史读日志里的回合时间，Claude Code hooks 的 `transcript_path` 指向 `<id>.events`；`TranscriptHook` 已删。daemon 的无状态撤销与压缩也是追加事实（`plan_conversation_change`），每回合统计按 `visible_turns` 取舍。人取消时半截回复是 `PartialReply` 事实。索引记 `format_version`，比本构建新的会话在目录里标 `needs_newer_version`、拒绝 resume；读到不认识的事件种类同样按「更新的版本」拒读。
  - 团队成员与 `task` 子 agent 的会话同样落盘：各是一个事件会话，存成 `storage_id`（`<lead>/<name>` 的 `/` 换成 `~`），索引记 `parent`，所以目录 / picker / `--continue` / recall 都不列、单独打开被拒、删 lead 时连带删；`session-store` 在 agent 创建时给它取一份自己的租约、移除时放掉，取不到就不让它运行。成员的创建参数在日志头 `member` 里，`stop` 等成员空闲后提交 `Stopped` 再移除；resume lead 时 team 行按 `children(lead)` 把没有 `Stopped` 的成员带回（角色按当前定义重算）。判据：`harness/tests/team.rs` 的 `a_resumed_lead_brings_back_the_members_it_did_not_stop` 等、`coding/tests/runtime_criteria.rs::a_team_is_kept_and_comes_back_with_its_lead`、`capabilities` 的 `a_delegated_session_is_kept_under_its_parent`。
- `describe_self` 告诉模型的每一句都由**实现那件事的行**（或读 patch/flag 的启动器）经 `self_knowledge::describes` / `describes_live` 登记，`self_knowledge` 自己不假定挂了哪些行、不替任何领域开专用注册表。替换掉一行（`swap`/`disable`）就要接过它该说的话：`session-native` 描述原生会话、`skills-host`/`mcp-host` 描述 SKILLS/MCP、`model` 行描述 `/model` 都是这么来的。工具自己的 `description()` 每次请求都会发给模型，不要再为工具往 `describe_self` 里写第二份；`describe_self` 只补工具描述覆盖不到的：会话、人如何添加/配置能力、前端命令、配置文件（身份已在系统提示里由 persona 行说，不重复）。config.toml 的说明写在读它的代码旁边（`coding/src/config.rs::describe_config_file`，由 `config-file` 行登记；可安全编辑的设置目录由 `atomcode_config::settings::describe_catalog` 渲染）。判据在 `harness/tests/self_knowledge.rs` 与 `coding/tests/runtime_criteria.rs` 的 `the_agent_is_told_where_its_session_really_is`、`a_capability_the_runtime_mounts_itself_still_describes_itself`、`a_runtime_configured_from_a_file_describes_the_file`。
- **凭据（API key、token）绝不进入行配置**：配置树是可打印的数据（`dump_runtime` 原样渲染每行 config），也绝不进入 `describe_self` 的任何回答。需要凭据的行由宿主注册一个持有凭据的插件实例并 `swap` 过去（`tool-web-keyed` 是范例）。判据 `coding/tests/mount_wiring.rs::a_configured_credential_never_enters_the_config_tree`。
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

### 提交前必须核 `Cargo.lock`（本机会反复污染它）

**本机每跑一次 `cargo build`，`Cargo.lock` 都会被写进一批公开仓库不该有的私有依赖。**

成因：`crates/atomcode-codingplan-crypto/` 在公开仓库里是**只有签名没有实现的 stub**，而本机放的是**实现版**——它的 `Cargo.toml` 声明了 `hmac`/`sha2`/`hkdf`/`zeroize`/`subtle`。根 `Cargo.toml` 的 `workspace.members = ["crates/*"]` 是 glob，把这个目录包了进来，于是 cargo 认为这些依赖属于本 workspace，解析后写进 lock。

证据链（2026-09-15 实测）：`6504f4ca` 删掉那 41 行 → `8fb2eaf9` 又加回来 42 行（同一批 + 1 行合法的 `atomcode-coding`）→ 手工剔干净后跑一次 `cargo build -p atomcode-tui --lib`，`Cargo.lock` 的 md5 立刻变回含私有依赖的版本。

**所以「剔掉那几行再提交」是一次性的，不是修好了。** push 前必须核：

```bash
grep -c 'name = "hkdf"\|name = "hmac"\|name = "zeroize_derive"' Cargo.lock   # 必须是 0
```

非 0 就用干净基线重来（`git show <干净 commit>:Cargo.lock > Cargo.lock`，再把该提交之后**合法的**依赖变化补回，例如 `atomcode-tui` 新增的 `atomcode-coding`），确认 `git diff Cargo.lock` 只剩你要删的那些行。**不要**用 `cargo update` 或 `cargo generate-lockfile` 去「修」它——它们只会把私有依赖写回来。

这是个已知的反复成本，不是可以一次解决的 bug：`[workspace.exclude]` 看着对症，但 `atomcode-auth` 用 path 依赖引它，排除会打断公开侧的 feature 门。真要根治得先想清楚私有 overlay 该以什么身份进入 workspace。

### 测试与构建命令（2026-09-13 实测定标，勿凭直觉推翻）

- 跑测试用 `cargo nextest run -p <crate>`，不用 `cargo test`。`cargo test` 一个 test binary 跑完才跑下一个，nextest 跨 binary 进程级并行。配置在 `.config/nextest.toml`。
- **但 nextest 不是无条件更快，收益与该 crate 的 test binary 数量成正比**（2026-09-13 实测，同一套测试）：

  | crate | test binary 数 | `cargo test` | nextest | + 虚拟时钟 |
  | --- | --- | --- | --- | --- |
  | kernel（305 个测试） | 28 | 570s | 31.4s | **0.64s** |
  | harness（280 个测试） | 22 | — | 14.8s | **4.04s** |
  | tui（397 个测试） | 1（几乎全是 lib 单测） | 1.63s | 2.12s | — |

  tui 反而略慢：只有一个 binary，`cargo test` 的串行短板压根不发作，显出来的是 nextest 的进程启动开销。所以判断依据是 binary 数量，不是"nextest 更快"这句话本身。改门或改脚本前先数一下 `ls crates/<c>/tests/*.rs | wc -l`。
- nextest 不跑 doctest。全仓 24 个 doc 代码块，带 `text`/`json`/`toml` 语言标记和 `ignore` 的都不跑——数 ``` 的个数会高估可跑数量，`atomcode-tui` 的 2 个块实测就是 0 passed / 2 ignored。真要确认某个 crate 丢没丢覆盖，跑 `cargo test --doc -p <crate>` 看 `test result` 那行，别靠 grep 估。
- 新增 async 测试，只要被测路径上有超时、退避或重试（provider retry、rate limit、stream timeout、conformance check timeout），一律写 `#[tokio::test(flavor = "current_thread", start_paused = true)]`，让 `tokio::time::sleep` 走虚拟时钟。裸 `#[tokio::test]` 会真等墙钟时间——sleep 往往不在测试里而在被测的生产代码里（如 `agent.rs` 的重试退避），grep 测试文件是看不出来的。kernel 曾有 11 个这样的测试，最慢单个 27s，合计占整套 305 个测试时间的 97%。范式见 `crates/atomcode-kernel/tests/tool_batch.rs`。
- 虚拟时钟不会让测试变空过：断言打在可观测结果上（重试是否耗尽、approval 问了几次），逻辑没跑到就会红。但 `start_paused` 要求 current_thread，改之前确认该测试不依赖真并行（无 `std::thread`、`spawn_blocking`、`block_on`）。
- **转之前先看该测试怎么断言时间，这里有两个陷阱**（2026-09-13 在 `harness/tests/recovery.rs` 踩到）：
  - 上界断言 `started.elapsed() < Duration::from_secs(20)`（"必须有界"）：若 `Instant` 来自 `std::time`，虚拟钟下真实时间几乎不走，断言变成恒真，判据当场作废。修法是把 `Instant` 换成 `tokio::time::Instant` —— 未暂停时等价 std，暂停时跟随虚拟钟，于是"超时被误写成 30s"这类退化仍然抓得住。
  - 下界断言 `run.elapsed >= Duration::from_millis(900)`（"必须真等过"）：**这种测试不要转**，虚拟钟会让它失去意义甚至判红。`recovery.rs` 的 `a_real_retry_after_is_honoured_over_any_guess` 和 `concurrency.rs` 就是这一类，它们至今仍真等墙钟时间，是刻意的。
  - 一句话：只转慢榜上的测试，且先 grep 一遍 `elapsed`。
- 按 crate 跑 `-p <crate>`，不要随手 `--workspace`。9 个 consumer 各开不同的 `atomcode-capabilities` feature 子集，resolver v2 刻意不跨 crate 统一 feature，于是这个 95k 行的库会被编 19 次（单份 28–138MB）；全量构建 91 个 test binary 一次吃掉 8.5GB 磁盘。2026-09-13 一次全量 nextest 把磁盘顶到 99%、swap 耗尽，test binary 被 SIGKILL，整个 66GB `target/` 随后消失。
- 不要给 dev profile 加 `[profile.dev.build-override] opt-level = 3`。已做过 A/B，结论为负：冷 check capabilities 从 21.7s 变 42.0s（多烧 144s CPU 把 syn/serde_derive 编成 -O3），稳态 CPU 无差异（1.95s vs 2.00s）。全仓 derive 密度约每 280 行一个，这笔一次性成本摊不平。
- 调试信息分两档：依赖 `[profile.dev.package."*"] debug = 0`（连行号也不留，panic 仍有符号名），workspace 内自己的 crate `[profile.dev] debug = "line-tables-only"`（**2026-09-18 用户决定改的**，此前是默认的 `debug = 2`）。原来那条规矩写的是"不要改成对整个 dev profile 生效，那会连自己代码的单步调试一起砍掉"——代价确实是这个：backtrace 的函数名与行号还在，失去的是调试器里看局部变量和单步。要单步某个 crate，临时加 `[profile.dev.package.atomcode-xxx] debug = 2` 只编那一个包。**收益尚未 A/B 量过**：改它的动机是 target 体积（量到过一个 worktree 56GB，其中 deps 34GB、incremental 21GB），但 macOS 上 debuginfo 留在 `.o` 里、不进最终二进制（见下一条 strip 的实测），所以省的是 `deps/` 与链接时间，不是二进制大小。谁先做出 A/B，把数字补在这里。
- 已排除的加速手段，别重复试：换链接器（macOS 已是 Apple 新 ld，`rust-lld` 不支持 mach-o）、sccache（不同 feature 组合是不同缓存键，救不了 19 个变体，还要额外磁盘）、砍 test binary 的 debuginfo（`profile.test` 的 `line-tables-only` 早已生效，strip 一个 98MB 的 binary 只掉 10MB）。
- `cargo check` 从来不是瓶颈：冷编译 capabilities 21.7s，稳态重编 2.5s。遇到"编译慢"先分清是 check 慢还是 test 慢，再分清是 CPU 饱和还是 I/O 等待（对比 `/usr/bin/time -p` 的 real 与 user+sys）。
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
