# 退役 core::provider — 子项目 C：daemon 脱 core::conversation + 删除 conversation/provider/ctx

> 状态：设计待确认。Option 1 的最后一个子项目。A（/compact provider）、B（vision）已完成落地。
> 目标：把 daemon 的 `/chat` + `/live` 传输层从 `core::conversation::Conversation`（一个纯往返 shim）迁到 kernel-native，删掉 `/chat` preflight 的 `core::provider`，最终删除 `core::conversation`、`core::provider`、`core::ctx` 三个模块。这一步完成后 `atomcode-core` 的会话/provider 核心退役。

## 1. 背景（调查确认）

A+B 后，`core::conversation` / `core::provider` 的**唯一外部代码消费者只剩 daemon** 三文件：`legacy_convert.rs`、`lib.rs`（`process_chat_request`）、`live_api.rs`（`run_chat_turn_v2` + 快照转换）。`core::ctx` **零外部消费者**（仅 core 内部 test 引用）。capabilities/config 里的 `core::provider` 命中全是**注释**（`Mirrors core::provider::…`），非代码依赖。

**关键事实**：daemon 里的 core `Conversation` 是一个**临时消息缓冲 + 转换 shim，从不发给 provider**。数据流是一次无谓往返：
```
native SessionSnapshot(kernel) → snapshot_to_core → Conversation(core 缓冲)
  → 取 prefix → snapshot_to_kernel → 原生 kernel runtime 跑 turn(全 kernel)
  → 终结 snapshot(kernel) → snapshot_to_core → 回填 Conversation
```
原生 runtime 全程 kernel 类型；core `Conversation` 只是被转来转去的缓冲。所以 C 的核心是**去掉这层往返**，用 kernel `Vec<Message>` + 元数据直接做缓冲。

**`/chat` preflight core provider**：`active_provider = provider::create_provider(...)`（lib.rs:3636）B 之后只剩 `set_session_id`（lib.rs:3676）一处用途，而 VL 现在自建 provider 并在 build 时绑 session（B Task3），故此 preflight provider **已完全冗余**——删掉即摘除最后一个 `core::provider` 外部消费者。

## 2. 分解（三刀，逐个可回滚）

**C1 — 删 /chat preflight core provider（小、安全、先做）**
- 移除 `active_provider` 的 `core::provider::create_provider` 构造（lib.rs:3636-3641）与其 `set_session_id`（3676）。preflight 校验语义用**原生 factory build**保留（`coding_provider_factory().build(&coding_cfg, Some(&session_id))` 一次，Err→干净错误），或直接依赖 runtime 自身构造报错（择一，倾向保留一次原生校验以维持"坏 provider 早报错"体验）。
- 删 lib.rs:86 `use atomcode_core::provider;`。
- 结果：`core::provider` 外部消费者归零（但**尚不能删模块**——core::conversation 内部仍用它，随 C3）。

**C2 — daemon 传输层脱 core::conversation（大、真重构）**
- 把 `/chat`（`process_chat_request`）与 `/live`（`run_chat_turn_v2`）的 core `Conversation` 缓冲换成 kernel-native 缓冲：
  - `Conversation::from_snapshot(core_snap)` ← 现在先 `snapshot_to_core(kernel_snap)` 再 from_snapshot → 改为**直接持有 kernel `SessionSnapshot` / `Vec<Message>`**，删掉 `snapshot_to_core` 入口往返。
  - `conv.add_user_message(text)` / `conv.messages.push(MultiPart{...})` → 直接建 `kernel::Message::user(text)` / `Message::user_with_images(...)` push 进 kernel Vec。
  - `conv.turn_tracker.on_user_message()` → 原生 runtime 已自管轮次计数；确认可去（若 daemon 侧需要轮数，用 kernel snapshot 的等价物）。
  - `conv.snapshot()`（persist pre-runtime cancel，lib.rs:3759）→ 直接持有 kernel snapshot，`persist_pre_runtime_terminal` 改收 kernel `SessionSnapshot`（或复用原生持久化）。
  - `conv.cancel_current_turn()`（lib.rs:3844）→ 在 kernel Vec 上截断未完成轮（等价逻辑）。
  - 终结回填 `snapshot_to_core` + `install_authoritative_terminal_snapshot`（live_api.rs:495/545）→ 直接用 kernel 终结 snapshot，删回填往返。
- `AuthoritativeTerminal.snapshot: ConversationSnapshot` → `SessionSnapshot`（live_api.rs:72）。
- `legacy_convert.rs`：`snapshot_to_core` / `message_to_core` / `snapshot_to_kernel` / `persist_pre_runtime_terminal` 逐个消除消费者后删除或瘦身。保留 `message_to_kernel`（若 legacy importer 仍需，见 [[project 会话迁移]]）。
- **图片恢复**（lib.rs:3855 `conv.messages` restore images）、**cold_summaries**（live_api.rs:378 prefix）等语义逐一在 kernel Vec 上重建，parity 保持。

**C3 — 删除 core::conversation + core::provider + core::ctx**
- 确认三模块外部消费者全零后，删模块本体 + lib.rs 声明 + orphan 测试。
- daemon `Cargo.toml` 视情去掉 `atomcode-core` 依赖（若 legacy_convert 完全消除；否则保留 importer 所需最小面）。
- 更新过期 doc 注释。

> **⚠️ C3 执行发现（2026-07-25）：物理删除被更深的 core::tool/ctx 纠缠阻塞。** C1+C2 后三模块**外部代码消费者已全零**（provider 仅余注释）。但它们物理上删不掉，因为 core 内部 `conversation↔provider↔ctx↔tool` 互相引用，且 **`core::tool` 仍有 9 个外部消费者**（daemon 的 `PermissionDecision`/`parse_permission_decision`、config 的 `real_home_dir`），`core::tool/mod.rs` 又内部用 `crate::ctx::file_store::FileStore`。故删 conversation/provider/ctx 需**先退役 core::tool + ctx::file_store**（外部消费者迁到 capabilities——capabilities 已有 `real_home_dir`/`strip_verbatim_prefix` 的 port）——这是一个**独立的后续子项目 D**，不属 C。
>
> **C 的实际完成度**：会话迁移**功能上已完成**——tuix/cli/daemon 传输全部脱离 core::conversation，含测试零引用（KEY CHECK grep 空）。legacy importer 用 frozen 本地 DTO 读旧盘，不依赖 core::conversation。剩余仅为 core 内部纠缠 + 物理删除，待子项目 D（退役 core::tool）解锁。

## 3. 关键设计决策

- **C 顺序**：C1（trivial，摘 provider 消费者）→ C2（传输重构，最大）→ C3（删除）。C1 独立可先落地；C2 是主体；C3 是收口。
- **不新建抽象**：daemon 缓冲直接用 kernel `Vec<Message>` + `SessionSnapshot`（原生 runtime 的类型），不造新 wrapper。
- **parity 是硬约束**：`/chat` 与 `/live` 的持久化、取消、图片恢复、cold_summaries、轮次语义逐条保持——这是 webui 的活路径，回归即用户可见。
- **turn_tracker / persistence**：优先复用原生 runtime 既有的 kernel 持久化路径（`/live` 已是最接近的原生路径），让 `/chat` 向 `/live` 的原生形态收敛，而非并行维护两套。

## 4. 测试

- **单测**：legacy_convert 现有往返测试随消费者消除而删/改；C2 的缓冲逻辑（建 user message、取消截断、图片恢复）抽纯函数单测。
- **构建**：每刀 `cargo build --workspace` + `cargo test --workspace --no-run`；daemon 套件绿（webui embedded-asset 两测试是既有环境性失败，无关）。
- **真机**：webui `/chat` 与 `/live` 各跑：新会话贴图（VL caption）、续聊历史、turn 取消（Esc）、压缩后续聊、刷新重连——确认无回归。这是 C 的验收核心（daemon 传输是 webui 命脉）。

## 5. 风险 / 回滚

- **C2 高风险**：daemon 活传输层，~100 处 core::conversation 命中横跨 3 文件，牵涉持久化/取消/图片/压缩语义。必须切成更小的可编译步（先 live_api，再 lib.rs，或先只读快照后写路径），每步绿。C2 可能需要自己的实施计划再分任务。
- 每刀独立 commit；C1/C3 小可快回滚；C2 若中途受阻，已落地的 C1 + A + B 不受影响。
- **不能半迁**：core `Conversation` 与 kernel 缓冲不能在同一路径混用（类型会打架）——C2 每个可编译步必须是某条路径的完整切换。

## 6. 非目标（YAGNI）

- 不改 tuix（已 100% 脱 core）、不改 cli（B 后视觉也脱了）。
- 不动 legacy importer 的**读取**能力（历史会话导入仍需 `message_to_kernel` 等 core DTO 读取，若其依赖 core 结构则保留最小读取面——以 C2/C3 实际消费面为准）。
- 不重构原生 runtime 本身——只让 daemon 传输向它的 kernel 形态收敛。
