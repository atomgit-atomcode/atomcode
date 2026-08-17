// notifications.ts 单测：node --test（无 jsdom），浏览器全局手动 stub。
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  loadPrefs,
  savePrefs,
  applyConfigDefaults,
  shouldNotify,
  notificationsSupported,
  maybeNotifyTurnFinished,
  disposeNotifications,
  type NotificationPrefs,
  type TurnFinishedInfo,
} from './notifications.ts';

const DEFAULTS: NotificationPrefs = {
  enabled: true,
  minDurationSecs: 8,
  backgroundOnly: true,
};

function makePrefs(over: Partial<NotificationPrefs> = {}): NotificationPrefs {
  return { ...DEFAULTS, ...over };
}

function makeInfo(over: Partial<TurnFinishedInfo> = {}): TurnFinishedInfo {
  const info: TurnFinishedInfo = {
    stopReason: 'natural',
    sessionId: 'session-1',
    durationMs: 60_000,
    ...over,
  };
  info.dedupeKey ??= `${info.sessionId ?? 'unknown'}:turn-1`;
  return info;
}

// ── 浏览器全局 stub 工具 ──────────────────────────────────────────────

class FakeStore {
  private map = new Map<string, string>();
  getItem(key: string): string | null {
    return this.map.has(key) ? this.map.get(key)! : null;
  }
  setItem(key: string, value: string): void {
    this.map.set(key, value);
  }
  removeItem(key: string): void {
    this.map.delete(key);
  }
  clear(): void {
    this.map.clear();
  }
}

interface FakeNotificationInstance {
  title: string;
  options: Record<string, unknown>;
  onclick: ((ev: Event) => void) | null;
  close: () => void;
}

let fakeNotificationCtor: { new (title: string, options?: Record<string, unknown>): FakeNotificationInstance; permission: NotificationPermission } | null = null;

function installNotification(permission: NotificationPermission = 'granted'): {
  instances: FakeNotificationInstance[];
} {
  const instances: FakeNotificationInstance[] = [];
  fakeNotificationCtor = class {
    static permission = permission;
    title: string;
    options: Record<string, unknown>;
    onclick: ((ev: Event) => void) | null = null;
    constructor(title: string, options?: Record<string, unknown>) {
      this.title = title;
      this.options = options ?? {};
      instances.push(this);
    }
    close() {
      /* no-op */
    }
  } as unknown as typeof fakeNotificationCtor;
  // 立即挂到全局：maybeNotifyTurnFinished 在调用时读取 Notification，
  // 必须让后续 new Notification(...) 落入本批 instances。
  (globalThis as Record<string, unknown>).Notification = fakeNotificationCtor;
  return { instances };
}

/** 临时安装浏览器全局；test 结束自动恢复。 */
function withBrowserGlobals(
  fn: () => void,
  opts: { permission?: NotificationPermission } = {},
): void {
  const origNotification = (globalThis as Record<string, unknown>).Notification;
  const origDocument = (globalThis as Record<string, unknown>).document;
  const origWindow = (globalThis as Record<string, unknown>).window;
  const origLocalStorage = (globalThis as Record<string, unknown>).localStorage;
  const origBroadcastChannel = (globalThis as Record<string, unknown>).BroadcastChannel;
  try {
    installNotification(opts.permission);
    (globalThis as Record<string, unknown>).Notification = fakeNotificationCtor;
    (globalThis as Record<string, unknown>).document = {
      visibilityState: 'hidden',
    };
    (globalThis as Record<string, unknown>).window = { isSecureContext: true };
    (globalThis as Record<string, unknown>).localStorage = new FakeStore();
    (globalThis as Record<string, unknown>).BroadcastChannel = FakeBroadcastChannel;
    // 清模块级 BroadcastChannel 缓存与静态实例数组：ensureChannel() 复用旧实例
    // 会导致新测试断言不到新创建的 channel，故每个测试都从干净状态开始。
    disposeNotifications();
    resetBroadcastChannels();
    fn();
  } finally {
    if (origNotification === undefined) {
      delete (globalThis as Record<string, unknown>).Notification;
    } else {
      (globalThis as Record<string, unknown>).Notification = origNotification;
    }
    if (origDocument === undefined) {
      delete (globalThis as Record<string, unknown>).document;
    } else {
      (globalThis as Record<string, unknown>).document = origDocument;
    }
    if (origWindow === undefined) {
      delete (globalThis as Record<string, unknown>).window;
    } else {
      (globalThis as Record<string, unknown>).window = origWindow;
    }
    if (origLocalStorage === undefined) {
      delete (globalThis as Record<string, unknown>).localStorage;
    } else {
      (globalThis as Record<string, unknown>).localStorage = origLocalStorage;
    }
    if (origBroadcastChannel === undefined) {
      delete (globalThis as Record<string, unknown>).BroadcastChannel;
    } else {
      (globalThis as Record<string, unknown>).BroadcastChannel = origBroadcastChannel;
    }
    fakeNotificationCtor = null;
  }
}

class FakeBroadcastChannel {
  static instances: FakeBroadcastChannel[] = [];
  name: string;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  sent: unknown[] = [];
  closed = false;
  constructor(name: string) {
    this.name = name;
    FakeBroadcastChannel.instances.push(this);
  }
  postMessage(data: unknown): void {
    this.sent.push(data);
  }
  close(): void {
    this.closed = true;
  }
}

function resetBroadcastChannels(): void {
  FakeBroadcastChannel.instances = [];
}

// ── shouldNotify 守卫链 ───────────────────────────────────────────────

test('shouldNotify: all conditions satisfied → true', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs(),
      hidden: true,
      info: makeInfo(),
      lastNotify: null,
      now: 1000,
    }),
    true,
  );
});

test('shouldNotify: unsupported (LAN HTTP) → false', () => {
  assert.equal(
    shouldNotify({
      supported: false,
      permission: 'granted',
      prefs: makePrefs(),
      hidden: true,
      info: makeInfo(),
      lastNotify: null,
      now: 1000,
    }),
    false,
  );
});

test('shouldNotify: permission not granted → false', () => {
  for (const permission of ['default', 'denied'] as const) {
    assert.equal(
      shouldNotify({
        supported: true,
        permission,
        prefs: makePrefs(),
        hidden: true,
        info: makeInfo(),
        lastNotify: null,
        now: 1000,
      }),
      false,
      `permission=${permission}`,
    );
  }
});

test('shouldNotify: disabled → false', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs({ enabled: false }),
      hidden: true,
      info: makeInfo(),
      lastNotify: null,
      now: 1000,
    }),
    false,
  );
});

test('shouldNotify: backgroundOnly + visible tab → false', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs({ backgroundOnly: true }),
      hidden: false,
      info: makeInfo(),
      lastNotify: null,
      now: 1000,
    }),
    false,
  );
});

test('shouldNotify: backgroundOnly=false + visible tab → true', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs({ backgroundOnly: false }),
      hidden: false,
      info: makeInfo(),
      lastNotify: null,
      now: 1000,
    }),
    true,
  );
});

test('shouldNotify: turn shorter than min_duration → false', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs({ minDurationSecs: 8 }),
      hidden: true,
      info: makeInfo({ durationMs: 5000 }),
      lastNotify: null,
      now: 1000,
    }),
    false,
  );
});

test('shouldNotify: turn exactly at min_duration → true', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs({ minDurationSecs: 8 }),
      hidden: true,
      info: makeInfo({ durationMs: 8000 }),
      lastNotify: null,
      now: 1000,
    }),
    true,
  );
});

test('shouldNotify: missing duration fails closed when a minimum is configured', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs({ minDurationSecs: 8 }),
      hidden: true,
      info: makeInfo({ durationMs: undefined }),
      lastNotify: null,
      now: 1000,
    }),
    false,
  );
});

test('shouldNotify: same session within 5s dedup window → false', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs(),
      hidden: true,
      info: makeInfo({ sessionId: 's1' }),
      lastNotify: { sessionId: 's1', dedupeKey: 's1:turn-1', ts: 900 },
      now: 1000,
    }),
    false,
  );
});

test('shouldNotify: same session beyond 5s window → true', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs(),
      hidden: true,
      info: makeInfo({ sessionId: 's1' }),
      lastNotify: { sessionId: 's1', dedupeKey: 's1:turn-1', ts: 1000 - 6000 },
      now: 1000,
    }),
    true,
  );
});

test('shouldNotify: different session ignores last notify → true', () => {
  assert.equal(
    shouldNotify({
      supported: true,
      permission: 'granted',
      prefs: makePrefs(),
      hidden: true,
      info: makeInfo({ sessionId: 's2' }),
      lastNotify: { sessionId: 's1', dedupeKey: 'another-turn', ts: 900 },
      now: 1000,
    }),
    true,
  );
});

// ── 偏好读写 ──────────────────────────────────────────────────────────

test('loadPrefs: no localStorage → defaults', () => {
  withBrowserGlobals(() => {
    const prefs = loadPrefs();
    assert.deepEqual(prefs, DEFAULTS);
  });
});

test('loadPrefs: empty storage → defaults', () => {
  withBrowserGlobals(() => {
    const ls = (globalThis as Record<string, unknown>).localStorage as FakeStore;
    assert.equal(ls.getItem('atomcode.webui_notify'), null);
    assert.deepEqual(loadPrefs(), DEFAULTS);
  });
});

test('loadPrefs: corrupt JSON → defaults', () => {
  withBrowserGlobals(() => {
    const ls = (globalThis as Record<string, unknown>).localStorage as FakeStore;
    ls.setItem('atomcode.webui_notify', '{not-json');
    assert.deepEqual(loadPrefs(), DEFAULTS);
  });
});

test('loadPrefs: partial entry merges defaults', () => {
  withBrowserGlobals(() => {
    const ls = (globalThis as Record<string, unknown>).localStorage as FakeStore;
    ls.setItem('atomcode.webui_notify', JSON.stringify({ enabled: false }));
    const prefs = loadPrefs();
    assert.equal(prefs.enabled, false);
    assert.equal(prefs.minDurationSecs, 8);
    assert.equal(prefs.backgroundOnly, true);
  });
});

test('savePrefs round-trips through loadPrefs', () => {
  withBrowserGlobals(() => {
    savePrefs(makePrefs({ enabled: false, minDurationSecs: 15, backgroundOnly: false }));
    assert.deepEqual(loadPrefs(), { enabled: false, minDurationSecs: 15, backgroundOnly: false });
  });
});

test('applyConfigDefaults seeds daemon defaults when user never set prefs', () => {
  withBrowserGlobals(() => {
    const ls = (globalThis as Record<string, unknown>).localStorage as FakeStore;
    assert.equal(ls.getItem('atomcode.webui_notify'), null);
    applyConfigDefaults({ enabled: false, min_duration_secs: 30 });
    assert.deepEqual(loadPrefs(), { enabled: false, minDurationSecs: 30, backgroundOnly: true });
  });
});

test('applyConfigDefaults does not overwrite an explicit user setting', () => {
  withBrowserGlobals(() => {
    savePrefs(makePrefs({ enabled: true, minDurationSecs: 5 }));
    applyConfigDefaults({ enabled: false, min_duration_secs: 30 });
    assert.deepEqual(loadPrefs(), { enabled: true, minDurationSecs: 5, backgroundOnly: true });
  });
});

test('applyConfigDefaults ignores undefined config', () => {
  withBrowserGlobals(() => {
    const ls = (globalThis as Record<string, unknown>).localStorage as FakeStore;
    applyConfigDefaults(undefined);
    assert.equal(ls.getItem('atomcode.webui_notify'), null);
  });
});

// ── 能力探测 ──────────────────────────────────────────────────────────

test('notificationsSupported: false without Notification global', () => {
  withBrowserGlobals(() => {
    delete (globalThis as Record<string, unknown>).Notification;
    assert.equal(notificationsSupported(), false);
  });
});

test('notificationsSupported: false on insecure window', () => {
  withBrowserGlobals(() => {
    (globalThis as Record<string, unknown>).window = { isSecureContext: false };
    assert.equal(notificationsSupported(), false);
  });
});

test('notificationsSupported: true on localhost-like secure context', () => {
  withBrowserGlobals(() => {
    assert.equal(notificationsSupported(), true);
  });
});

// ── maybeNotifyTurnFinished 端到端（stub 全局） ───────────────────────

test('maybeNotifyTurnFinished: creates a notification with mirrored title/body', () => {
  withBrowserGlobals(() => {
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo({ stopReason: 'natural', durationMs: 60_000 }));
    assert.equal(instances.length, 1);
    assert.equal(instances[0].title, 'AtomCode done');
    assert.match(instances[0].options.body as string, /^Done · 60\.0s/);
    assert.equal(instances[0].options.tag, 'session-1');
    assert.equal(instances[0].options.icon, '/favicon.png');
  });
});

test('maybeNotifyTurnFinished: title mapping for every stop reason', () => {
  withBrowserGlobals(() => {
    const cases: [string | undefined, string, string][] = [
      ['natural', 'AtomCode done', /^Done/],
      ['cancelled', 'AtomCode cancelled', /^Cancelled/],
      ['error', 'AtomCode failed', /^Failed/],
      ['turn_limit', 'AtomCode stopped', /^Stopped/],
      ['step_limit', 'AtomCode stopped', /^Stopped/],
      [undefined, 'AtomCode finished', /^Finished/],
    ];
    // 每个 case 用不同 sessionId：避免上一个 case 写入的 5s 去重记录拦截本 case。
    let i = 0;
    for (const [reason, expectedTitle, bodyRe] of cases) {
      i += 1;
      const { instances } = installNotification('granted');
      maybeNotifyTurnFinished(makeInfo({ stopReason: reason, sessionId: `map-${i}` }));
      assert.equal(instances.length, 1, `stopReason=${String(reason)}`);
      assert.equal(instances[0].title, expectedTitle, `stopReason=${String(reason)}`);
      assert.match(instances[0].options.body as string, bodyRe);
    }
  });
});

test('maybeNotifyTurnFinished: respects min_duration', () => {
  withBrowserGlobals(() => {
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo({ durationMs: 2000, stopReason: 'natural' }));
    assert.equal(instances.length, 0);
  });
});

test('maybeNotifyTurnFinished: respects enabled=false', () => {
  withBrowserGlobals(() => {
    const ls = (globalThis as Record<string, unknown>).localStorage as FakeStore;
    ls.setItem('atomcode.webui_notify', JSON.stringify({ enabled: false }));
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo());
    assert.equal(instances.length, 0);
  });
});

test('maybeNotifyTurnFinished: no-op when permission denied', () => {
  withBrowserGlobals(
    () => {
      const { instances } = installNotification('denied');
      maybeNotifyTurnFinished(makeInfo());
      assert.equal(instances.length, 0);
    },
    { permission: 'denied' },
  );
});

test('maybeNotifyTurnFinished: no-op on visible tab when backgroundOnly', () => {
  withBrowserGlobals(() => {
    (globalThis as Record<string, unknown>).document = { visibilityState: 'visible' };
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo());
    assert.equal(instances.length, 0);
  });
});

test('maybeNotifyTurnFinished: writes dedup record and broadcasts to peers', () => {
  withBrowserGlobals(() => {
    resetBroadcastChannels();
    const ls = (globalThis as Record<string, unknown>).localStorage as FakeStore;
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1' }));
    const record = JSON.parse(ls.getItem('atomcode.webui_last_notify')!);
    assert.equal(record.sessionId, 's1');
    assert.equal(record.dedupeKey, 's1:turn-1');
    assert.equal(typeof record.ts, 'number');
    const channels = FakeBroadcastChannel.instances;
    assert.ok(channels.length > 0);
    assert.ok(channels.some((c) => c.sent.length > 0 && (c.sent[0] as { sessionId: string }).sessionId === 's1'));
  });
});

test('maybeNotifyTurnFinished: second call for same session within 5s → single notification', () => {
  withBrowserGlobals(() => {
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1' }));
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1' }));
    assert.equal(instances.length, 1);
  });
});

test('maybeNotifyTurnFinished: distinct turns in the same session are not suppressed', () => {
  withBrowserGlobals(() => {
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1', dedupeKey: 's1:turn-1' }));
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1', dedupeKey: 's1:turn-2' }));
    assert.equal(instances.length, 2);
  });
});

test('maybeNotifyTurnFinished: different session after a notify → notifies again', () => {
  withBrowserGlobals(() => {
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1' }));
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's2' }));
    assert.equal(instances.length, 2);
  });
});

// 跨标签去重按权威终态键匹配；同一终态在 5s 内被抑制，不同 session/turn 不受影响。
test('maybeNotifyTurnFinished: peer broadcast suppresses same session only', () => {
  withBrowserGlobals(() => {
    resetBroadcastChannels();
    const { instances } = installNotification('granted');
    // 第一次弹通知并广播（产生 channel）。
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1' }));
    assert.equal(instances.length, 1);
    const ch = FakeBroadcastChannel.instances.find((c) => !c.closed);
    assert.ok(ch, 'channel should exist');

    // 模拟其他标签页广播 s1 的通知。
    const peerTs = Date.now();
    ch!.onmessage?.({
      data: { sessionId: 's1', dedupeKey: 's1:turn-1', ts: peerTs },
    });

    // 同一会话 s1 在 5s 内被抑制。
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1' }));
    assert.equal(instances.length, 1, 'same session should be suppressed');

    // 不同会话 s2 不受抑制。
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's2' }));
    assert.equal(instances.length, 2, 'different session must not be suppressed');
  });
});

test('maybeNotifyTurnFinished: peer broadcast outside 5s window allows same session', () => {
  withBrowserGlobals(() => {
    resetBroadcastChannels();
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1' }));
    const ch = FakeBroadcastChannel.instances.find((c) => !c.closed);
    assert.ok(ch, 'channel should exist');
    // 隔离变量：把同 tab 权威去重记录也改成 6s 前（否则首次弹窗写入的新记录
    // 会先被 5s 去重窗口拦截，测不到 peer 广播路径）。
    const ls = (globalThis as Record<string, unknown>).localStorage as FakeStore;
    ls.setItem('atomcode.webui_last_notify', JSON.stringify({
      sessionId: 's1',
      dedupeKey: 's1:turn-1',
      ts: Date.now() - 6000,
    }));
    // 广播时间戳在 6s 前（超过 5s 去重窗口）。
    ch!.onmessage?.({
      data: {
        sessionId: 's1',
        dedupeKey: 's1:turn-1',
        ts: Date.now() - 6000,
      },
    });
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's1' }));
    assert.equal(instances.length, 2, 'stale peer notify should not suppress');
  });
});

test('maybeNotifyTurnFinished: body truncates long message', () => {
  withBrowserGlobals(() => {
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo({ message: 'x'.repeat(500) }));
    assert.equal(instances.length, 1);
    const body = instances[0].options.body as string;
    assert.ok(body.length < 140, `body too long: ${body.length}`);
    assert.match(body, /x{120}$/);
  });
});

test('disposeNotifications closes the channel it created', () => {
  withBrowserGlobals(() => {
    resetBroadcastChannels();
    maybeNotifyTurnFinished(makeInfo());
    const created = FakeBroadcastChannel.instances;
    assert.ok(created.length > 0, 'should have created a channel');
    assert.ok(created.every((c) => !c.closed), 'channel should start open');
    disposeNotifications();
    assert.ok(created.every((c) => c.closed), 'dispose should close every channel');
    // 再次通知应复用新通道（旧的已关闭，不能再发）。
    const { instances } = installNotification('granted');
    maybeNotifyTurnFinished(makeInfo({ sessionId: 's2' }));
    const reopened = FakeBroadcastChannel.instances.filter((c) => !c.closed);
    assert.ok(reopened.length >= 1, 'dispose 后再次通知应新建可用通道');
    assert.equal(instances.length, 1);
  });
});
