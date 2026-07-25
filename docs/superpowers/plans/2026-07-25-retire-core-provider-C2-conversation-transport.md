# 退役 core::provider 子项目C2 — daemon 传输层脱 core::conversation 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [ ]`).
> **⚠️ 高风险**：这是 daemon 活传输层（webui `/chat` + `/live`）的重构，牵涉持久化/取消/图片恢复/cold_summaries/轮次语义，回归即用户可见。每个可编译步必须是某条路径的**完整**切换（core Conversation 与 kernel 缓冲不能在同一路径混用）。

**Goal:** 把 daemon `/chat`（`process_chat_request`）+ `/live`（`run_chat_turn_v2`）的 core `Conversation` 缓冲换成 kernel-native，消除 `snapshot_to_core ↔ snapshot_to_kernel` 无谓往返，使 `core::conversation` 外部消费者归零（C3 才删模块）。

**Architecture:** daemon 现在把 kernel `SessionSnapshot` 转成 core `Conversation` 当临时缓冲，跑 turn 前又转回 kernel，turn 后再转回 core——一次无谓往返。core `Conversation` 从不发给 provider。C2 用 kernel `Vec<Message>` + 一个薄 daemon 缓冲（重建 `add_user_message`/`cancel_current_turn`/cold_summaries 语义）取代它。

**Tech Stack:** Rust workspace；crate `atomcode-daemon`（+ 可能薄助手在 daemon 内）。core 类型：`Conversation`/`ConversationSnapshot`/`TurnTracker`。kernel：`Message`/`SessionSnapshot`/`Role`/cold-summary-as-synthetic-message 编码（`LEGACY_COLD_SUMMARY_*`）。

## Global Constraints

- 每任务后 `cargo build --workspace` + `cargo test --workspace --no-run`；daemon 套件绿（webui embedded-asset 两测试是既有环境性失败，无关）。
- 每任务一提交；`docs/superpowers` 用 `git add -f`。
- **parity 硬约束**：持久化、turn 取消、图片恢复、cold_summaries、轮次语义逐条不变。
- **不半迁**：每个可编译步是某条路径的完整切换。
- worktree `retire-core-conversation`，push release/v5.0.3。

## 前置调查（Task 0，执行者必做，不写代码）

在动手前，执行者须精确回答并记录（写进本计划的"调查记录"节）：

1. **core `Conversation` 完整消费面**：`grep -nE "conv\.|Conversation|ConversationSnapshot|turn_tracker|cold_summaries|cancel_current_turn|add_user_message|from_snapshot|\.snapshot\(\)"` 于 lib.rs + live_api.rs，逐点列出（行号 + 该点语义）。
2. **`TurnTracker` 的 daemon 依赖**：`cancel_current_turn`（lib.rs:3844）与 `turn_tracker.on_user_message`（lib.rs:3733）在 kernel 侧的等价物是什么？原生 runtime 是否已自管轮次/取消，使 daemon 侧的 turn_tracker **冗余**？（关键决策：若原生 runtime 的 cancel 已权威，则 daemon 的 `cancel_current_turn` 可删而非重建。）
3. **cold_summaries 表示差异**：core 用独立 `cold_summaries: Vec<String>` 字段；kernel 用前缀标记的合成 message（`LEGACY_COLD_SUMMARY_ORIGIN/PREFIX`）。列出 daemon 读 `c.cold_summaries` 的每一点（如 live_api.rs:378 prefix、live_api.rs:290 压缩），确定改用 kernel 编码后如何取值（kernel 侧是否有 `cold_summaries_from_messages` 助手？message.rs:9 注释提到）。
4. **`install_authoritative_terminal_snapshot`**（live_api.rs:267/545）+ `AuthoritativeTerminal.snapshot: ConversationSnapshot`（:72）：终结回填在做什么？能否直接用 kernel 终结 snapshot（原生 runtime 已产出 kernel snapshot）跳过 `snapshot_to_core`？
5. **图片恢复**（lib.rs:3855 `conv.messages` restore images）：语义 + kernel Message 上的等价实现。
6. **持久化**：`persist_pre_runtime_terminal(&Path,&str,&ConversationSnapshot)`（legacy_convert）+ `conv.snapshot()`（lib.rs:3759）——原生持久化是否已有 kernel snapshot 落盘路径可复用（`SessionManager` 存 `SessionSnapshot`，manager.rs:453）？

**Task 0 产出**：把上述答案回填本节，据此可能需要**修订下面的 Task 划分**（尤其 turn_tracker/cancel 若冗余则删而非重建，能大幅缩小 C2）。

## Task 1: 引入 daemon kernel 缓冲 + 纯转换助手（不切换路径）

**Files:** Create `crates/atomcode-daemon/src/live_buffer.rs`（或加进 live_api）；Modify daemon lib.rs 注册。

**目标**：新增一个薄的 kernel-native 缓冲（据 Task 0 决策，或是 `struct LiveBuffer { messages: Vec<kernel::Message> }` + 方法 `from_kernel_snapshot`/`to_kernel_snapshot`/`push_user_text`/`push_user_with_images`/`cancel_current_turn`(若非冗余)/`cold_summaries`），逐条镜像 core `Conversation` 被 daemon 用到的方法，但全 kernel 类型。纯逻辑，可 TDD 单测（建 user message、取消截断、cold_summary 取值）。

- [ ] Step 1-N：按 Task 0 清单，为每个被消费的 Conversation 方法写失败测试 → 实现 → 绿。（cancel/turn 边界逻辑若保留则从 core `TurnTracker` port 纯逻辑；若 Task 0 判定冗余则不建。）
- [ ] Commit：`feat(daemon): kernel-native live buffer（镜像 Conversation daemon 消费面，纯 kernel）`

## Task 2: 切换 `/live` 的 `run_chat_turn_v2` 到 kernel 缓冲

**Files:** Modify `crates/atomcode-daemon/src/live_api.rs`（`run_chat_turn_v2` :357-550、`AuthoritativeTerminal` :72、`install_authoritative_terminal_snapshot` :267、`committed_compaction_snapshot` :276）。

**目标**：`conv: Arc<Mutex<Conversation>>` → `Arc<Mutex<LiveBuffer>>`（或直接 `Arc<Mutex<Vec<kernel::Message>>>`+侧带 cold_summaries）。删 `snapshot_to_kernel(&prefix)`（:387，prefix 已是 kernel）+ 终结 `snapshot_to_core`（:495，直接用 kernel 终结 snapshot）。`extract_user_input` 改吃 kernel Message。这是**一条路径的完整切换**——run_chat_turn_v2 及其 conv 类型、所有读写点同一 commit 内改完。

- [ ] Step 1：改 `run_chat_turn_v2` 签名 + prefix 提取（kernel 直取，去 snapshot_to_kernel）。
- [ ] Step 2：终结回填改 kernel（去 snapshot_to_core，AuthoritativeTerminal→SessionSnapshot）。
- [ ] Step 3：`extract_user_input` kernel 版；cold_summaries 取值改 kernel 编码。
- [ ] Step 4：`cargo build -p atomcode-daemon`（此时 `/chat` 调用点 conv 类型仍 core → 会红；若 run_chat_turn_v2 被 `/chat` 与 `/live` 共用，Task 2/3 可能**必须合并为一个 commit**——以编译边界为准，宁可一个较大 commit 也不留半迁）。
- [ ] Commit（可能与 Task 3 合并）。

## Task 3: 切换 `/chat` 的 `process_chat_request` 到 kernel 缓冲

**Files:** Modify `crates/atomcode-daemon/src/lib.rs`（:3665 load、:3687 from_snapshot、:3717 add_user_message、:3720 messages.push、:3733 turn_tracker、:3759 snapshot+persist、:3844 cancel、:3855 图片恢复）。

**目标**：删 `snapshot_to_core`（:3665，直接持 kernel snapshot）；`Conversation::from_snapshot` → `LiveBuffer::from_kernel_snapshot`；`add_user_message`/`messages.push(MultiPart)` → kernel `Message::user`/`user_with_images`；`turn_tracker`/`cancel_current_turn` 按 Task 0 决策（删或用 LiveBuffer）；`conv.snapshot()`+`persist_pre_runtime_terminal` → kernel snapshot 落盘（复用原生持久化）；图片恢复在 kernel Vec 上重建。

- [ ] Step 1-N：逐点切换（同一 commit，完整路径）。
- [ ] Build + daemon 测试绿。
- [ ] Commit：`refactor(daemon): /chat + /live 传输层脱 core::conversation（去 snapshot 往返，缓冲改 kernel）`

## Task 4: 消除 legacy_convert 的 core↔kernel 往返函数

**Files:** Modify `crates/atomcode-daemon/src/legacy_convert.rs` + 其测试。

**目标**：`snapshot_to_core` / `message_to_core` / `snapshot_to_kernel` / `persist_pre_runtime_terminal` 消费者归零后删除；`message_to_kernel`（若 legacy importer 历史读取仍需则保留最小面——以实际消费为准）。删对应往返测试。

- [ ] Step 1：`grep` 确认各函数零消费 → 删除。
- [ ] Step 2：`snapshot_to_core` 若还被 KernelSummaryProvider 遗留引用？（A 已删该 adapter，应已零）——确认。
- [ ] Build + test 绿。
- [ ] Commit：`chore(daemon): 删 legacy_convert 的 core↔kernel 往返（传输已全 kernel）`

## Task 5: 真机验收（仅用户可做）

webui 各跑并确认无回归：`/chat` 与 `/live` 新会话贴图（VL caption）、续聊历史、turn 取消（Esc/cancel）、压缩后续聊、页面刷新重连、并发两 tab。

---

## 后续（C3，另计划或本计划尾）

C2 落地 + 真机绿后：确认 `core::conversation`/`core::provider`/`core::ctx` 外部消费者全零 → 删三模块本体 + lib.rs 声明 + orphan 测试；daemon Cargo.toml 视情去 `atomcode-core` 依赖。

## 调查记录

（Task 0 执行后回填。特别是 turn_tracker/cancel 是否冗余的决策——它决定 Task 1/3 规模。）

## Self-Review 记录

- **Spec 覆盖**：spec C §2 的 C2 = 本计划 Task 1-4；C3 = 尾节。spec §5 高风险"不半迁"约束落在 Task 2/3 的"完整路径切换 + 可能合并 commit"。
- **已知不确定性**：本计划**刻意含 Task 0 调查**——turn_tracker/cancel 在原生 runtime 下是否冗余、cold_summaries 的 kernel 取值助手是否存在，会实质改变 Task 1/3 规模。这不是 placeholder，而是高风险重构必须先测的真实分叉点（比盲写实现代码更安全）。
- **风险顺序**：Task 1（纯加法缓冲+单测，零切换）→ Task 2/3（路径切换，可能合并）→ Task 4（删往返）→ C3（删模块）。Task 1 独立安全；切换步是主体风险；每步编译边界为准。
