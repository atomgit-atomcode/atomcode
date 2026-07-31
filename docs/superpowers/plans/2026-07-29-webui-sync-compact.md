# webui sync 模式支持 /compact 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 webui 在 sync 模式下的 `/compact` 针对共享实时运行时执行压缩，替换掉「同步模式下暂不支持该命令」的拒绝。

**Architecture:** 新增一个薄的 `POST /live/compact` 端点，把 `DriverCommand::Compact(None)` 派发到 live hub 绑定的共享运行时；压缩结果通过既有的 `NativeLiveWireProjector`（`CompactionFinished → Warning`）回流到 webui。前端在 sync 时改调该端点。不新增任何事件管道。

**Tech Stack:** Rust（axum daemon：`atomcode-daemon`）、TypeScript/Preact（`webui`）。

## Global Constraints

- 代码注释/提交信息中**不要提及 opencode**（借鉴思路可以，落地用中性描述）。
- 提交信息结尾附：`Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`。
- 当前分支 `release/v5.0.3`，在其上提交（勿切 main）。
- webui `dist/` 被 gitignore：只提交 `webui/src/`；TypeScript 改动需本地 `tsc` 校验。
- i18n 中英两表 key 必须同步（`webui/src/i18n.ts` 的 zh 段 ~307、en 段 ~626）。
- 后端单测放在 `crates/atomcode-daemon/src/live_hub.rs` 的 `#[cfg(test)]` 里，用 fresh `LiveViewHub::new()`，**不要**触碰进程级全局 `native_live::hub()`（会与其它测试串扰）。
- webui 行为改动无自动化真机测试，靠手动 sync 模式验证（本项目惯例）。

---

### Task 1: 后端 `/live/compact` 端点 + 路由 + hub 派发契约测试

**Files:**
- Modify: `crates/atomcode-daemon/src/live_api.rs`（在 `live_cancel` 后新增 `live_compact`，约 1806-1809 之后）
- Modify: `crates/atomcode-daemon/src/lib.rs`（`/live/cancel` 路由旁，约 5177-5178）
- Test: `crates/atomcode-daemon/src/live_hub.rs`（`#[cfg(test)] mod tests`，约 1273 附近，仿 `driver_commands_and_local_inputs_share_the_bound_runtime`）

**Interfaces:**
- Consumes:
  - `atomcode_coding::DriverCommand::Compact(Option<String>)`（`crates/atomcode-coding/src/runtime.rs:395`，`atomcode_coding::DriverCommand` 已从 `atomcode-coding/src/lib.rs:87` 导出）。
  - `crate::native_live::dispatch(command: DriverCommand) -> Result<(), HubError>`（`crates/atomcode-daemon/src/native_live.rs:192`，`pub`）。
  - `LiveViewHub::{new, bind, dispatch, join}` 与测试内的 `FakeControl`/`control()`/`snapshot()`（`live_hub.rs` tests 已有）。
- Produces:
  - HTTP：`POST /live/compact` → `200 {"accepted": bool}`（`accepted:false` 表示无绑定运行时）。前端 Task 2 依赖此形状。
  - 函数：`pub(crate) async fn live_compact(State<AppState>) -> impl IntoResponse`。

- [ ] **Step 1: 写失败测试（hub 把 Compact 路由到绑定运行时）**

在 `crates/atomcode-daemon/src/live_hub.rs` 的 `mod tests` 内新增：

```rust
    #[test]
    fn dispatch_routes_manual_compact_to_bound_runtime() {
        let hub = LiveViewHub::new();
        let (control, commands) = control();
        hub.bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();

        hub.dispatch(DriverCommand::Compact(None)).unwrap();

        assert!(matches!(
            commands.lock().unwrap().as_slice(),
            [DriverCommand::Compact(None)]
        ));
    }

    #[test]
    fn dispatch_compact_without_runtime_is_unbound() {
        let hub = LiveViewHub::new();
        let error = hub.dispatch(DriverCommand::Compact(None)).unwrap_err();
        assert_eq!(error, HubError::Unbound);
    }
```

- [ ] **Step 2: 跑测试确认能编译并通过（契约成立）**

Run: `cargo test -p atomcode-daemon --lib dispatch_routes_manual_compact_to_bound_runtime dispatch_compact_without_runtime_is_unbound`

Expected: 两测试 PASS。（hub 的 `dispatch_locked` 是泛型转发，Compact 天然可路由；这两测试锁定端点所依赖的契约——绑定则转发、未绑定则 `Unbound`。若 `DriverCommand::Compact` 的元数形状与断言不符，此步会**编译失败**，即为红。）

> 若 Step 2 直接绿：这是契约特征测试，green 可接受，继续。若编译失败，按报错修正断言的 pattern（如 `Compact(None)` 的确切形状）后再跑。

- [ ] **Step 3: 新增端点 `live_compact`**

在 `crates/atomcode-daemon/src/live_api.rs` 的 `live_cancel`（约 1806-1809）之后追加：

```rust
/// POST /live/compact —— webui/手机端在 sync 模式请求对共享实时运行时执行一次
/// 手动压缩。派发 `DriverCommand::Compact(None)` 到 live hub；压缩结果经既有的
/// `NativeLiveWireProjector`（CompactionFinished → Warning）回流到各视图。
/// 返回 `{"accepted": bool}`：false 表示当前没有绑定的实时运行时（无可压缩对象）。
pub(crate) async fn live_compact(State(_state): State<AppState>) -> impl IntoResponse {
    let accepted =
        crate::native_live::dispatch(atomcode_coding::DriverCommand::Compact(None)).is_ok();
    Json(serde_json::json!({ "accepted": accepted }))
}
```

- [ ] **Step 4: 注册路由**

在 `crates/atomcode-daemon/src/lib.rs` 的 `.route("/live/cancel", post(live_api::live_cancel))`（约 5177）之后新增一行：

```rust
        .route("/live/compact", post(live_api::live_compact))
```

- [ ] **Step 5: 编译并跑相关测试**

Run: `cargo build -p atomcode-daemon && cargo test -p atomcode-daemon --lib dispatch_routes_manual_compact_to_bound_runtime dispatch_compact_without_runtime_is_unbound`

Expected: 编译通过；两测试 PASS。

- [ ] **Step 6: 提交**

```bash
git add crates/atomcode-daemon/src/live_api.rs crates/atomcode-daemon/src/lib.rs crates/atomcode-daemon/src/live_hub.rs
git commit -m "feat(daemon): POST /live/compact dispatches manual compaction to live runtime

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: webui `postLiveCompact` + i18n + sync 分支改写

**Files:**
- Modify: `webui/src/api.ts`（在 `postLiveStop` 后，约 777 之后新增 `postLiveCompact`）
- Modify: `webui/src/i18n.ts`（zh 段约 311、en 段约 630：新增 `cmd.compact.syncNoRuntime`）
- Modify: `webui/src/components/Chat.tsx`（`execServerCommand`，约 1427-1435 的 sync 拒绝分支）

**Interfaces:**
- Consumes:
  - Task 1 的 `POST /live/compact` → `{ accepted: boolean }`。
  - 既有 `authHeaders()`（`api.ts`）、`pushCommandNotice`、`busyRef`、`sync`、`t(...)`（`Chat.tsx`）。
- Produces:
  - `export async function postLiveCompact(): Promise<{ accepted: boolean }>`。
  - i18n key `cmd.compact.syncNoRuntime`（中/英）。

- [ ] **Step 1: 新增 `postLiveCompact`**

在 `webui/src/api.ts` 的 `postLiveStop`（约 777）之后新增（仿 `postLiveStop` 形状，但不抛错——`accepted:false` 是正常「无运行时」信号，交给调用方处理）：

```typescript
/** Sync-mode manual compaction: dispatch a compaction against the shared live
 *  runtime. `accepted:false` means no live runtime is bound (nothing to compact). */
export async function postLiveCompact(): Promise<{ accepted: boolean }> {
  const resp = await fetch('/live/compact', {
    method: 'POST',
    headers: authHeaders(),
  });
  if (!resp.ok) throw new Error(`live compact failed: ${resp.status}`);
  const body = await resp.json() as { accepted?: boolean };
  return { accepted: body.accepted === true };
}
```

- [ ] **Step 2: 新增 i18n key（中英同步）**

在 `webui/src/i18n.ts` 的 zh 段 `'cmd.session.busy'`（约 311）之后新增：

```typescript
  'cmd.compact.syncNoRuntime': '当前没有可压缩的实时会话',
```

在 en 段 `'cmd.session.busy'`（约 630）之后新增：

```typescript
  'cmd.compact.syncNoRuntime': 'No live session to compact',
```

- [ ] **Step 3: 改写 `execServerCommand` 的 sync 分支**

在 `webui/src/components/Chat.tsx` 中，把当前（约 1427-1435）：

```typescript
      execServerCommand: async (command, arg) => {
        const SESSION_MUTATING = new Set(['undo', 'compact']);
        if (SESSION_MUTATING.has(command)) {
          if (busyRef.current) { pushCommandNotice(t('cmd.session.busy')); return; }
          if (sync) { pushCommandNotice(t('cmd.session.syncUnsupported')); return; }
        }
        if (command === 'compact') pushCommandNotice(t('cmd.compact.pending'));
```

改为（busy 守卫对两命令都保留；sync 下 compact 走实时端点，undo 仍拒绝）：

```typescript
      execServerCommand: async (command, arg) => {
        const SESSION_MUTATING = new Set(['undo', 'compact']);
        if (SESSION_MUTATING.has(command)) {
          if (busyRef.current) { pushCommandNotice(t('cmd.session.busy')); return; }
          if (sync) {
            // sync 模式：compact 派发到共享实时运行时（结果经 /live 的 Warning 事件
            // 渲染压缩标记）；undo 暂无实时路径，维持拒绝。
            if (command === 'compact') {
              pushCommandNotice(t('cmd.compact.pending'));
              try {
                const { accepted } = await postLiveCompact();
                if (!accepted) pushCommandNotice(t('cmd.compact.syncNoRuntime'));
              } catch (e) {
                pushCommandNotice(t('chat.connError', { msg: e instanceof Error ? e.message : String(e) }));
              }
              return;
            }
            pushCommandNotice(t('cmd.session.syncUnsupported'));
            return;
          }
        }
        if (command === 'compact') pushCommandNotice(t('cmd.compact.pending'));
```

确认 `postLiveCompact` 已在 `Chat.tsx` 顶部从 `../api`（或现有 api 导入处）导入。检查现有 import 行（搜索 `postLiveProvider` / `postCommand` 的 import）并把 `postLiveCompact` 加入同一 `import { ... } from '...'`。

- [ ] **Step 4: TypeScript 校验**

Run: `cd webui && npx tsc --noEmit`
Expected: 无类型错误（尤其确认 `postLiveCompact` 已导入、`chat.connError` key 存在——它在 `execServerCommand` 别处已被使用）。

- [ ] **Step 5: 提交**

```bash
git add webui/src/api.ts webui/src/i18n.ts webui/src/components/Chat.tsx
git commit -m "feat(webui): run /compact via /live/compact in sync mode

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: 手动 sync 模式真机验证

**Files:** 无（纯验证）。

- [ ] **Step 1: 构建并起服务**

Run: `cargo build -p atomcode-daemon && (cd webui && npm run build)`（按项目既有构建方式；若有 `run` skill 覆盖启动方式则以其为准）。

- [ ] **Step 2: 场景 A —— sync 下正常压缩**

打开 webui，URL 带 `?sync=1`（或用底栏同步开关开启 sync），进行多轮对话直到有可压缩历史。输入 `/compact` 回车。
Expected: **不再**出现「同步模式下暂不支持该命令」；出现 `cmd.compact.pending`（正在压缩）；随后出现压缩标记 Warning（移除 N 条消息、tokens before→after）。若同一进程有 TUI 附着，TUI 侧也应显示相同压缩标记。

- [ ] **Step 3: 场景 B —— 回合进行中拒绝**

在一个回合流式生成过程中输入 `/compact`。
Expected: 显示 `cmd.session.busy`（请先停止当前回合），不派发压缩。

- [ ] **Step 4: 场景 C —— 无可压缩/无运行时**

在全新会话（无实时运行时绑定，或空历史刚连上）输入 `/compact`。
Expected: 若无绑定运行时，显示 `cmd.compact.syncNoRuntime`（当前没有可压缩的实时会话）；不报「暂不支持」。

- [ ] **Step 5: 回归 —— 非 sync 路径不变**

关闭 sync，输入 `/compact`。
Expected: 走原 `/command` 磁盘路径，行为与改动前一致（`cmd.compact.done` 或 `cmd.compact.none`）。

---

## Self-Review

**Spec 覆盖：**
- 后端新增 `/live/compact` 端点 → Task 1 Step 3。✅
- `lib.rs` 注册路由 → Task 1 Step 4。✅
- `api.ts` `postLiveCompact` → Task 2 Step 1。✅
- `Chat.tsx` sync 分支改调端点 + 保留 busy 守卫 + undo 维持拒绝 → Task 2 Step 3。✅
- `accepted:false` 提示 key → Task 2 Step 2（`cmd.compact.syncNoRuntime`）。✅
- 压缩结果由既有 SSE Warning 渲染（不双提示）→ Task 2 Step 3 注释 + Task 3 场景 A。✅
- 后端单测（未绑定 accepted:false / 绑定则派发 Compact）→ Task 1 Step 1。✅
- 前端手动 sync 验证 → Task 3。✅
- 范围外（undo sync、hub 快照陈旧、消息列表就地替换）→ 计划未触碰，符合 spec。✅

**占位符扫描：** 无 TBD/TODO；所有代码步骤均含具体代码。✅

**类型一致性：** `postLiveCompact(): Promise<{ accepted: boolean }>` 在 Task 2 Step 1 定义、Step 3 消费一致；`DriverCommand::Compact(None)` 在 Task 1 测试、端点两处一致；`live_compact` 函数名在 Step 3 定义、Step 4 路由引用一致。✅
