// WebUI 任务完成浏览器通知模块（纯前端 Web Notification API）。
//
// 设计要点：
// - 决策逻辑（shouldNotify）是纯函数，不触碰浏览器全局，可在 node --test 中直接单测；
// - maybeNotifyTurnFinished 负责收集浏览器全局（Notification/BroadcastChannel/
//   document.visibilityState/localStorage）并执行副作用；
// - 去重：localStorage 时间戳是权威（跨标签共享），BroadcastChannel 只作即时信号；
// - 标题/正文文案镜像 atomcode-capabilities/src/notify.rs，保证跨端一致。

export interface NotificationPrefs {
  /** 总开关，默认 true。 */
  enabled: boolean;
  /** 最短回合时长（秒），默认 8，与 NotificationConfig.min_duration_secs 一致。 */
  minDurationSecs: number;
  /** 仅后台标签页提醒（document.hidden 才弹），默认 true。 */
  backgroundOnly: boolean;
}

export interface TurnFinishedInfo {
  /** 'natural' | 'cancelled' | 'error' | 'turn_limit' | 'step_limit' | undefined */
  stopReason?: string;
  /** 用于 tag 去重 + 点击恢复会话。 */
  sessionId?: string;
  /** 可选摘要（截断 120 字符）。 */
  message?: string;
  /** 本 tab 观测到的回合时长；undefined = late join，跳过时长过滤。 */
  durationMs?: number;
}

const PREFS_KEY = 'atomcode.webui_notify';
const LAST_NOTIFY_KEY = 'atomcode.webui_last_notify';
const CHANNEL_NAME = 'atomcode-webui-notify';
const DEDUP_WINDOW_MS = 5000;
const MESSAGE_SNIPPET_MAX = 120;

const DEFAULT_PREFS: NotificationPrefs = {
  enabled: true,
  minDurationSecs: 8,
  backgroundOnly: true,
};

interface LastNotifyRecord {
  sessionId: string;
  ts: number;
}

function safeParsePrefs(raw: string | null): NotificationPrefs | null {
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as Partial<NotificationPrefs>;
    if (typeof parsed !== 'object' || parsed === null) return null;
    return {
      enabled: typeof parsed.enabled === 'boolean' ? parsed.enabled : DEFAULT_PREFS.enabled,
      minDurationSecs:
        typeof parsed.minDurationSecs === 'number'
          ? parsed.minDurationSecs
          : DEFAULT_PREFS.minDurationSecs,
      backgroundOnly:
        typeof parsed.backgroundOnly === 'boolean'
          ? parsed.backgroundOnly
          : DEFAULT_PREFS.backgroundOnly,
    };
  } catch {
    return null;
  }
}

/** 读取偏好：localStorage 显式设置 > /config 种入的默认值 > 硬编码默认值。 */
export function loadPrefs(): NotificationPrefs {
  if (typeof localStorage === 'undefined') return { ...DEFAULT_PREFS };
  const saved = safeParsePrefs(localStorage.getItem(PREFS_KEY));
  return saved ?? { ...DEFAULT_PREFS };
}

/** 持久化偏好（设置面板调用）。 */
export function savePrefs(prefs: NotificationPrefs): void {
  if (typeof localStorage === 'undefined') return;
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  } catch {
    /* ignore */
  }
}

/**
 * 从 `/config` 种入 daemon 默认值：仅当用户**未显式设置**过偏好时生效
 * （localStorage 无 `atomcode.webui_notify` 键）。让与 TUI 共享的
 * `notifications.enabled` / `min_duration_secs` 对 webui 生效，同时
 * 用户一旦在设置面板改过则不再被后端默认值覆盖。
 */
export function applyConfigDefaults(
  cfg: { enabled: boolean; min_duration_secs: number } | undefined,
): void {
  if (!cfg || typeof localStorage === 'undefined') return;
  if (localStorage.getItem(PREFS_KEY) !== null) return; // 用户显式设置过，不覆盖
  const prefs: NotificationPrefs = {
    enabled: cfg.enabled,
    minDurationSecs: cfg.min_duration_secs,
    backgroundOnly: DEFAULT_PREFS.backgroundOnly,
  };
  savePrefs(prefs);
}

/** 浏览器通知能力：secure context（localhost 可用，LAN HTTP 不可用）。 */
export function notificationsSupported(): boolean {
  return (
    typeof Notification !== 'undefined' &&
    typeof window !== 'undefined' &&
    window.isSecureContext
  );
}

/** 请求通知权限。只在用户手势内调用（设置面板开关）。 */
export async function requestNotificationPermission(): Promise<boolean> {
  if (!notificationsSupported()) {
    console.warn('[webui-notify] requestPermission 被跳过：notificationsSupported() === false');
    return false;
  }
  try {
    console.warn('[webui-notify] 请求通知权限（Notification.requestPermission）…');
    const result = await Notification.requestPermission();
    console.warn(`[webui-notify] requestPermission 结果: ${result}`);
    return result === 'granted';
  } catch (err) {
    console.warn('[webui-notify] requestPermission 抛出异常:', err);
    return false;
  }
}

// 权限自动提示：只在 App 挂载时注册一次；测试用钩子重置。
let promptListenerAttached = false;

/**
 * 首次用户交互时自动请求通知权限（浏览器要求 requestPermission 必须在用户手势
 * 事件栈内调用）。App 挂载时调用一次。
 *
 * 背景：开关默认 enabled=true，若权限一直停在 'default' 且用户从未点过设置面板
 * 开关，通知会静默失效（守卫链在 permission 处拦截，用户无感知）。本函数让
 * 页面首次交互（点击/按键）即弹出浏览器授权框，把「开但永远不弹」变成
 * 「首次交互即引导授权」。
 */
export function initNotificationPermissionPrompt(): void {
  if (promptListenerAttached) return;
  if (typeof window === 'undefined' || typeof document === 'undefined') return;
  if (!notificationsSupported()) return;
  if (typeof Notification === 'undefined' || Notification.permission !== 'default') return;
  if (!loadPrefs().enabled) return;

  let requested = false;
  const request = () => {
    if (requested) return;
    requested = true;
    window.removeEventListener('pointerdown', request);
    window.removeEventListener('keydown', request);
    // 仍在用户手势事件栈内：浏览器允许弹出授权框。
    void requestNotificationPermission();
  };
  window.addEventListener('pointerdown', request);
  window.addEventListener('keydown', request);
  promptListenerAttached = true;
}

/** 测试专用：重置权限提示的已注册状态，让每个测试从干净状态开始。 */
export function __resetNotificationPermissionPromptForTest(): void {
  promptListenerAttached = false;
}

/**
 * 纯决策函数：是否应弹出通知。任一条件不满足返回 false。
 *
 * 条件：supported / permission granted / enabled / backgroundOnly?(hidden) /
 * 时长过滤 / 5s 去重窗口。`onReject` 为可选调试回调：被拦截时回传原因。
 */
export function shouldNotify(opts: {
  supported: boolean;
  permission: NotificationPermission;
  prefs: NotificationPrefs;
  /** document.visibilityState === 'hidden' */
  hidden: boolean;
  info: TurnFinishedInfo;
  /** 上次同会话通知记录；null = 无。 */
  lastNotify: LastNotifyRecord | null;
  /** 当前时间戳（注入以便测试）。 */
  now: number;
  /** 调试：被拦截时回传原因（不改变返回结果）。 */
  onReject?: (reason: string) => void;
}): boolean {
  const { supported, permission, prefs, hidden, info, lastNotify, now, onReject } = opts;
  if (!supported) {
    onReject?.('notificationsSupported() === false（非 secure context 或浏览器无 Notification）');
    return false;
  }
  if (permission !== 'granted') {
    onReject?.(`Notification.permission === '${permission}'（需用户在浏览器授予通知权限）`);
    return false;
  }
  if (!prefs.enabled) {
    onReject?.('prefs.enabled === false（设置面板「完成通知」开关未打开）');
    return false;
  }
  if (prefs.backgroundOnly && !hidden) {
    onReject?.('backgroundOnly 且页面可见（document.visibilityState !== hidden，需切到后台标签）');
    return false;
  }
  // late join（durationMs === undefined）跳过时长过滤，避免漏报。
  if (info.durationMs !== undefined && info.durationMs < prefs.minDurationSecs * 1000) {
    onReject?.(
      `回合时长 ${info.durationMs}ms < minDurationSecs ${prefs.minDurationSecs}s，被时长过滤`,
    );
    return false;
  }
  // 同会话 5s 去重窗口。
  if (info.sessionId && lastNotify && lastNotify.sessionId === info.sessionId) {
    if (now - lastNotify.ts < DEDUP_WINDOW_MS) {
      onReject?.(
        `同会话 ${info.sessionId} 在 5s 去重窗口内已弹过（${now - lastNotify.ts}ms 前）`,
      );
      return false;
    }
  }
  return true;
}

function fmtDuration(ms: number): string {
  // 镜像 notify.rs::fmt_duration（<1s 显示 ms，否则一位小数秒）。
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function titleForStopReason(stopReason?: string): string {
  switch (stopReason) {
    case 'natural':
      return 'AtomCode done';
    case 'cancelled':
      return 'AtomCode cancelled';
    case 'error':
      return 'AtomCode failed';
    case 'turn_limit':
    case 'step_limit':
      return 'AtomCode stopped';
    default:
      return 'AtomCode finished';
  }
}

function statusForStopReason(stopReason?: string): string {
  switch (stopReason) {
    case 'natural':
      return 'Done';
    case 'cancelled':
      return 'Cancelled';
    case 'error':
      return 'Failed';
    case 'turn_limit':
    case 'step_limit':
      return 'Stopped';
    default:
      return 'Finished';
  }
}

function buildBody(info: TurnFinishedInfo): string {
  let body = statusForStopReason(info.stopReason);
  if (info.durationMs !== undefined) {
    body += ` · ${fmtDuration(info.durationMs)}`;
  }
  if (info.message && info.message.trim()) {
    const snippet = info.message.trim().slice(0, MESSAGE_SNIPPET_MAX);
    body += ` · ${snippet}`;
  }
  return body;
}

function readLastNotify(): LastNotifyRecord | null {
  if (typeof localStorage === 'undefined') return null;
  const raw = localStorage.getItem(LAST_NOTIFY_KEY);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as LastNotifyRecord;
    if (
      parsed &&
      typeof parsed.sessionId === 'string' &&
      typeof parsed.ts === 'number'
    ) {
      return parsed;
    }
    return null;
  } catch {
    return null;
  }
}

function writeLastNotify(record: LastNotifyRecord): void {
  if (typeof localStorage === 'undefined') return;
  try {
    localStorage.setItem(LAST_NOTIFY_KEY, JSON.stringify(record));
  } catch {
    // 写失败不阻塞弹窗（隐私模式/配额满时静默降级为无跨标签权威去重）。
  }
}

// BroadcastChannel 引用（同进程多个 Chat 实例/多次挂载共用同一通道；卸载时 dispose）。
let channel: BroadcastChannel | null = null;
// 其他标签页广播的最近一次通知（sessionId + 时间戳）。跨标签去重必须**按会话**
// 匹配：只抑制同一会话 5s 内的重复，不同会话的通知不能被误抑制。
let peerNotified: { sessionId: string; ts: number } | null = null;

function ensureChannel(): BroadcastChannel | null {
  if (typeof BroadcastChannel === 'undefined') return null;
  if (channel) return channel;
  try {
    channel = new BroadcastChannel(CHANNEL_NAME);
    channel.onmessage = (event: MessageEvent) => {
      const data = event.data as { sessionId?: string; ts?: number } | null;
      if (data && typeof data.ts === 'number' && data.sessionId) {
        peerNotified = { sessionId: data.sessionId, ts: data.ts };
      }
    };
  } catch {
    channel = null;
  }
  return channel;
}

/** 关闭 BroadcastChannel（会话切换/卸载时调用）。 */
export function disposeNotifications(): void {
  if (channel) {
    channel.close();
    channel = null;
  }
  peerNotified = null;
}

/** 从 URL 读取当前查看的会话短 id（用于通知点击恢复）。 */
function currentSessionFromUrl(): string | null {
  try {
    return new URLSearchParams(window.location.search).get('session');
  } catch {
    return null;
  }
}

/** 通知点击：聚焦窗口；仅当目标会话与当前查看会话不同时才导航（避免丢输入）。 */
function onNotificationClick(notification: Notification, sessionId?: string): void {
  notification.onclick = () => {
    try {
      window.focus();
      if (sessionId) {
        const short = sessionId.slice(0, 8);
        if (currentSessionFromUrl() !== short) {
          window.location.href = `/?session=${short}`;
        }
      }
    } finally {
      notification.close();
    }
  };
}

/**
 * 回合终态时调用：守卫链通过则弹系统通知。
 *
 * - 不在 SSE 回调里请求权限（浏览器要求用户手势）；
 * - 弹前检查 localStorage 权威去重记录，弹后写入并广播；
 * - 通知 tag=sessionId，同会话新通知覆盖旧通知。
 */
export function maybeNotifyTurnFinished(info: TurnFinishedInfo): void {
  const supported = notificationsSupported();
  const prefs = loadPrefs();
  const hidden = typeof document !== 'undefined' && document.visibilityState === 'hidden';
  const permission: NotificationPermission =
    typeof Notification !== 'undefined'
      ? Notification.permission
      : 'denied';
  const lastNotify = readLastNotify();
  const now = Date.now();

  console.warn('[webui-notify] maybeNotifyTurnFinished 调用', {
    info,
    supported,
    prefs,
    hidden,
    permission,
    lastNotify,
  });

  // 跨标签即时信号（peerNotified 由 BroadcastChannel 更新）并入去重判定：
  // 其他 tab 在 5s 内弹过**同一会话** → 本 tab 不再弹。按 sessionId 匹配，
  // 不同会话的通知不受抑制。
  const peerRecent =
    info.sessionId &&
    peerNotified !== null &&
    peerNotified.sessionId === info.sessionId &&
    now - peerNotified.ts < DEDUP_WINDOW_MS;

  if (peerRecent) {
    console.warn(`[webui-notify] 被拦截：其他标签页 5s 内已弹过同一会话 ${info.sessionId}`);
    return;
  }

  const decision = shouldNotify({
    supported,
    permission,
    prefs,
    hidden,
    info,
    lastNotify,
    now,
    onReject: (reason) => console.warn(`[webui-notify] 守卫链拦截: ${reason}`),
  });

  if (!decision) {
    return;
  }

  const sessionId = info.sessionId;
  const title = titleForStopReason(info.stopReason);
  const body = buildBody(info);
  const record: LastNotifyRecord = { sessionId: sessionId ?? `turn-${now}`, ts: now };

  console.warn(`[webui-notify] 弹出通知: "${title}" / "${body}" / tag=${record.sessionId}`);

  let notification: Notification;
  try {
    notification = new Notification(title, {
      body,
      tag: sessionId ?? `turn-${now}`,
      icon: '/favicon.png',
    });
  } catch (err) {
    console.warn('[webui-notify] new Notification() 抛出异常:', err);
    return;
  }

  // 先写权威去重记录再广播（写失败不阻塞弹窗）。
  writeLastNotify(record);
  const ch = ensureChannel();
  if (ch) {
    try {
      ch.postMessage(record);
    } catch {
      /* ignore */
    }
  }

  onNotificationClick(notification, sessionId);
}
