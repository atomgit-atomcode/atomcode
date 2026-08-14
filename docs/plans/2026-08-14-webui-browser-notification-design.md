# WebUI 任务完成浏览器提醒设计

## 目标

任务或 session 回合完成后，webui 浏览器后台标签页弹出系统通知；TUI 现有通知通道
（`atomcode-capabilities::notify`）原样保留，互不干扰。

## 方案定位

- 浏览器侧：纯前端 Web Notification API（`new Notification()`），由 webui SPA 在
  回合终态事件处触发，不依赖任何新事件或新端点；
- TUI 侧：保留 `notify.rs`（终端转义序列 → OS 原生 → BEL）及 `tuix/event_loop` 调用点；
- 后端（daemon/coding/kernel）：除 `/config` 只读暴露 notifications 段外零改动；
- 降级：非 secure context（LAN HTTP 访问）下 Notification API 不可用，静默降级。

## 实现基点（代码事实）

| 项 | 位置 | 事实 |
|---|---|---|
| `/chat` 终态事件 | `webui/src/components/Chat.tsx:1912`（`case 'done'`）、`:1937`（`case 'stopped'`） | `done` 含 `stop_reason`/`session_id`/`message`；均调用 `setBusy(false)` |
| `/live` 终态 | `Chat.tsx:1316-1351`（`case 'state'`） | `state` 携带 `running`/`stop_reason`/`message`，经 `reduceLiveLifecycle` 得 `lifecycle.terminal`，为真即回合结束（`:1350` `onLiveTurnDone`）。`snapshot`（`:1165`）只确立会话并恢复消息，**不是**终态事件 |
| busy 状态 | `Chat.tsx:431-442` | `busy` state + `busyRef` 同步镜像（SSE 回调异步安全） |
| 前端设置面板 | `webui/src/settings.tsx` | `SettingsSection = 'theme'\|'language'\|'model'\|'remote'`；持久化走 localStorage |
| `/config` 返回 | `crates/atomcode-daemon/src/api_config.rs:38` | `ConfigResponse { path, default_provider, default_workdir, providers }` —— 当前不含 notifications 段 |
| 通知配置 | `crates/atomcode-config/src/config/mod.rs:1181` | `NotificationConfig { enabled, min_duration_secs(默认8), terminal, system, bell, background_only }`；`skip_serializing`，由 `render_notifications_section` 手动写盘 |
| 设置注册 | `crates/atomcode-config/src/settings.rs:155-170` | `notifications.enabled`、`notifications.bell` 已注册，`ApplyPolicy::NextTurn` |
| TUI 触发 | `tuix/event_loop/mod.rs:21867`、`:22086` | `notify_turn_finished(TurnNotification { duration, turn_count, tool_call_count, stop_reason, ... })` |
| 标题文案映射 | `notify.rs:238-252` | Natural→"AtomCode done"，Cancelled→"AtomCode cancelled"，Error→"AtomCode failed"，TurnLimit/StepLimit→"AtomCode stopped" |
| 会话恢复 URL | `webui/src/app.tsx:17-27` | 已支持 `?session=<短id>` 刷新恢复，通知点击可复用 |

## 架构

```
浏览器（webui SPA）
  Chat.tsx 两条 SSE 终态路径
    ├─ /chat: case 'done' / 'stopped' / 'error'
    └─ /live: case 'snapshot' 且 terminal
         │
         ▼
  lib/notifications.ts
    ├─ 读取偏好（localStorage + /config 降级 + 默认值）
    ├─ 权限管理（requestPermission / 状态）
    ├─ 守卫链（secure context / enabled / hidden / min_duration）
    ├─ 去重（BroadcastChannel 跨标签 + tag 同会话去重）
    └─ new Notification(title, { body, tag, icon })
         │
         ▼（可选只读配置）
daemon /config (api_config.rs) → atomcode-config NotificationConfig
TUI 通道（notify.rs）完全独立、原样保留
```

## 前端实现

### `webui/src/lib/notifications.ts`（新增）

```ts
export interface NotificationPrefs {
  enabled: boolean;        // 总开关，默认 true
  minDurationSecs: number; // 最短回合时长，默认 8（与 NotificationConfig 一致）
  backgroundOnly: boolean; // 仅后台标签页弹，默认 true
}

export interface TurnFinishedInfo {
  stopReason?: string;     // 'natural' | 'cancelled' | 'error' | 'turn_limit' | 'step_limit' | undefined
  sessionId?: string;      // tag 去重 + 点击恢复
  message?: string;        // 可选摘要
  durationMs?: number;     // 本 tab 观测到的回合时长；undefined = late join，跳过时长过滤
}

export function loadPrefs(): NotificationPrefs;                 // localStorage → /config → 默认
export async function requestNotificationPermission(): Promise<boolean>;
export function notificationsSupported(): boolean;              // 'Notification' in window && isSecureContext
export function initNotificationPermissionPrompt(): void;       // 首次用户交互时自动请求权限（App 挂载时调用一次）
export function maybeNotifyTurnFinished(info: TurnFinishedInfo): void;
export function disposeNotifications(): void;                   // 关闭 BroadcastChannel
```

守卫链（`maybeNotifyTurnFinished` 内部，任一失败即 return）：

1. `notificationsSupported()` —— LAN HTTP 下 false，静默降级；
2. `Notification.permission === 'granted'`；
3. `prefs.enabled`；
4. `prefs.backgroundOnly ? document.visibilityState === 'hidden' : true`（visible 不弹，
   对齐 TUI `background_only` 语义；注意浏览器语义是「标签页隐藏」而非「应用失焦」）；
5. `durationMs !== undefined` 时 `durationMs >= minDurationSecs * 1000`。`durationMs` 取
   **本 tab 主动 submit 时刻**到终态的时间（`turnStartedAtRef` 只在用户 `deliver`
   路径设置）；live 恢复的 `running=true`（snapshot/state 重放）**不得**重置
   `turnStartedAtRef`，否则 late join 页面的时长被污染成「页面打开→结束」而失真。
   late join（本 tab 从未 submit）→ `durationMs` 传 undefined → 跳过时长过滤直接按
   stop_reason 弹，避免漏报；
6. 跨标签去重：**localStorage 时间戳是权威**（弹前读写 `atomcode.webui_last_notify`，
   同 sessionId 且距上次 < 5s 则跳过），BroadcastChannel('atomcode-webui-notify')
   只作即时信号通知其他 tab 刷新检查——先广播后检查存在竞态窗口，必须靠
   localStorage 兜底；
7. 同 tab 去重：`tag=sessionId` 覆盖旧通知 + 上述 5s 窗口。

已知边界（实现时注意）：

- late join 安全性：`live_hub` 在 `TurnFinished` 时 `state.replay.clear()` 且
  `replay=false`（`live_hub.rs:881,886`），join 重放只含进行中回合的观测，**不会**
  重放历史终态 → 打开页面不会对已结束回合误报；
- 反向缺口：SSE 断流重连（15s keepalive 看门狗，`Chat.tsx:553-558`）期间回合恰好
  结束的，重连后 snapshot 已含终态消息但不再有 `state{running:false}` 事件 → 通知
  丢失。可接受（TUI 通道兜底），实现时无需补救；
- 通知点击恢复：`location.href = '/?session='` 整页跳转会丢失当前标签页输入框内容，
  仅当目标 session 与当前查看会话不同时才导航，否则只 `window.focus()`；
- 权限引导：开关默认 `enabled=true` 但权限未授予时永不弹，用户易误以为功能失效。缓解
  为**首次用户交互自动请求**：`initNotificationPermissionPrompt()` 在 App 挂载时注册
  `pointerdown`/`keydown` 一次性监听，若 `permission === 'default'` 且开关开启，首次
  点击/按键即在用户手势事件栈内调 `requestPermission()`（浏览器允许），把「开但永远
  不弹」变成「首次交互即引导授权」；被拒绝后不再重复打扰（权限变 denied）。设置面板
  同时保留手动入口与站点设置引导。

标题/正文映射（镜像 `notify.rs:238`）：

| stop_reason | title | body |
|---|---|---|
| `natural` | AtomCode done | Done · {时长} |
| `cancelled` | AtomCode cancelled | Cancelled · {时长} |
| `error` | AtomCode failed | Failed · {时长} |
| `turn_limit`/`step_limit` | AtomCode stopped | Stopped · {时长} |
| undefined | AtomCode finished | Finished |

正文可追加 `· {message 前若干字符}`（截断 120 字符）。`new Notification` 调用：

```ts
const n = new Notification(title, {
  body,
  tag: sessionId ?? `turn-${Date.now()}`,
  icon: '/favicon.png',     // webui/public/favicon.png 已存在
});
n.onclick = () => {
  window.focus();
  if (sessionId) location.href = `/?session=${sessionId.slice(0, 8)}`;
  n.close();
};
```

权限管理：`requestNotificationPermission()` 只在用户手势内调用（设置面板开关），结果
缓存到 `localStorage['atomcode.webui_notify_permission']`；拒绝时设置面板显示引导文案。
偏好存储：`localStorage['atomcode.webui_notify']` = `{ enabled, minDurationSecs, backgroundOnly }`；
加载优先级 localStorage 显式设置 > `/config` notifications 段 > 默认值。

### `webui/src/components/Chat.tsx`（修改）

- 新增 `turnStartedAtRef`：`setBusy(true)` 且为 null 时记录 `Date.now()`；`setBusy(false)`
  时计算 durationMs 供终态事件使用；
- `/chat` 路径：`case 'done'`（`:1930` 后）、`case 'stopped'`（`:1939` 后）、`case 'error'`
  （`:1945` 后）追加 `maybeNotifyTurnFinished(...)`；注意 `done` 携带 `stop_reason`，
  而 `stopped`/`error` 事件**没有**该字段（`api.ts:32-33`），需分别硬编码
  `'cancelled'`/`'error'`；且仅当 `event.session_id === 当前查看会话` 时才弹（防切换
  会话后旧流 `done` 到达误报）；
- `/live` 路径：`case 'state'`（`:1316`）的 `terminal` 分支（`:1326` 真、`:1350` 附近）
  追加 `maybeNotifyTurnFinished(...)`；`state` 不直接携带 session_id，靠
  `:1281-1287` 的当前会话门控保证只处理本会话；
- 会话切换/卸载时 `disposeNotifications()`、重置 `turnStartedAtRef`（`:700` 处一并清）。

### 设置面板

- `webui/src/settings.tsx`：`SettingsSection` 增加 `'notifications'`；
- `webui/src/components/SettingsDialogs.tsx`：新增 `NotificationsDialog`（仿 `ThemeDialog`）：
  - 开关「浏览器完成通知」→ 用户手势内 `requestNotificationPermission()`；
  - 开关「仅后台标签页提醒」（`backgroundOnly`）；
  - 数字输入「最短回合时长(秒)」（默认 8）；
  - 权限未授予/不支持时显示引导文案；
- `webui/src/i18n.ts`：新增 `notifications.*` 中英文案；
- `webui/src/api.ts`：`ConfigInfo` 增加可选段 `notifications?: NotificationConfigInfo`
  （enabled/min_duration_secs/bell），未暴露时用默认值，向后兼容。

### 单测 `webui/src/lib/notifications.test.ts`（新增）

守卫链各分支、标题映射全表、跨标签去重、localStorage 优先级。webui 测试基础设施为
`node --test`（无 jsdom），`notifications.ts` 依赖的浏览器全局
（`Notification`/`BroadcastChannel`/`document.visibilityState`/`localStorage`）须在
测试内手动 stub（参考现有 `chatTerminal.test.ts` 直接 import `.ts` 的模式）。

## 可选后端项

### 5.1 `/config` 暴露 notifications 段（推荐一期做）

`crates/atomcode-daemon/src/api_config.rs`：`ConfigResponse` 增加
`notifications: NotificationConfigInfo`（enabled/min_duration_secs/bell）。收益：前端
`loadPrefs()` 可读后端配置，`min_duration_secs` 与 TUI 一致。

### 5.2 前端开关回写后端（二期）

`POST /config/notifications`（走 `update_config` + `ConfigStore`）；`NotificationConfig`
增加 `browser: bool`（`default_true`，`render_notifications_section` 补渲染）；
`settings.rs` 注册 `notifications.browser`。收益：与 TUI 共用 `enabled` 总开关。风险：
配置格式+写盘+settings 改动面扩大，需按配置/持久化改动流程走。

## 不改动的部分

- `atomcode-capabilities/src/notify.rs` 原样保留；
- `tuix/event_loop/mod.rs` 的 `notify_turn_finished` 调用点原样保留；
- `NotificationConfig` 现有字段语义不变；
- `webui.rs`、`live_hub.rs`、`live_api.rs`、kernel/coding 零改动；
- 双端同时触发：允许重复提醒（TUI OS 通知 + 浏览器通知各自独立）；跨进程
  "任一通道已弹则另一静默"需 daemon 广播标记，属二期，本方案不引入。

## 风险与边界

| 风险 | 等级 | 对策 |
|---|---|---|
| LAN HTTP（0.0.0.0 绑定）无 Notification API | 高 | 设置面板提示 + 守卫静默降级；文档注明仅 localhost 可用 |
| 权限需用户手势 | 中 | 首次用户交互自动请求（initNotificationPermissionPrompt）+ 设置面板开关；绝不在 SSE 回调里 requestPermission |
| 多标签页重复弹 | 中 | BroadcastChannel 去重（同 session 5s 窗口） |
| 同一会话连续多回合重复弹 | 低 | `tag=sessionId` 覆盖 + 5s 窗口 |
| 页面可见时弹（打扰） | 低 | `backgroundOnly` 默认 true，`document.hidden` 才弹 |
| 用户拒绝权限后无入口恢复 | 低 | 设置面板显示浏览器站点设置引导 |
| 与 TUI 通知语义不一致 | 低 | 文案全表镜像 `notify.rs`；时长默认值同取 8s |

## 交付物

| 文件 | 类型 |
|---|---|
| `webui/src/lib/notifications.ts` | 新增 |
| `webui/src/lib/notifications.test.ts` | 新增 |
| `webui/src/components/Chat.tsx` | 修改 |
| `webui/src/components/SettingsDialogs.tsx` | 修改 |
| `webui/src/settings.tsx` | 修改 |
| `webui/src/i18n.ts` | 修改 |
| `webui/src/api.ts` | 修改 |
| `crates/atomcode-daemon/src/api_config.rs` | 修改（5.1） |
| `crates/atomcode-daemon/src/lib.rs`（ConfigResponse 定义） | 修改（5.1） |

## 验证

1. `cd webui && npm test`（新增单测全绿——纯逻辑，无需浏览器/atomcode）；
2. `npm run build`（前端产物可编译）；
3. 最小化前端验证（可选，无需完整 atomcode）：写一个 mock SSE 后端（Node 小脚本，
   提供 `/live` 的 `snapshot` + `state{running:false, stop_reason}`、`/config`、`/project`
   等最小端点），浏览器打开真实 webui SPA 连它，验证"终态事件 → 弹通知"全链路；
4. 浏览器侧能力探针（可选）：任意 localhost 静态页调 `Notification.requestPermission()`
   弹一条通知，验证 localhost secure context 与权限流（与 atomcode 无关）；
5. 手动验收：TUI `/webui` 打开 → 后台标签 → 长任务（>8s）→ 弹通知；<8s 不弹；
   前台不弹；第二标签同会话不重复弹；点击通知跳回 `?session=` 恢复；
   `--host 0.0.0.0` 下设置面板提示不可用；
4. 回归：TUI 通知不受影响（跑长任务验证 OS 通知仍触发）；
5. 若做 5.1：`cargo test -p atomcode-daemon` 的 api_config 相关测试。
