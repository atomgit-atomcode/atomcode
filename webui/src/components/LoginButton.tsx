// Account / login state for the settings menu (the gear's "Account" entry).
// Self-contained: reads the webui token from the URL, calls /auth/* directly,
// and picks zh/en labels from settings — to stay decoupled from api.ts/i18n.ts.

import { useEffect, useRef, useState } from 'preact/hooks';
import { useSettings } from '../settings';
import { fill, openSignIn } from '../lib/signIn';

const TOKEN = new URLSearchParams(location.search).get('token') ?? '';
function authHeaders(): Record<string, string> {
  return TOKEN ? { Authorization: 'Bearer ' + TOKEN } : {};
}

export interface UserInfo {
  username: string;
  name?: string | null;
  email?: string | null;
  avatar_url?: string | null;
}

const L = {
  zh: {
    signIn: '登录',
    signingIn: '登录中…',
    signOut: '退出登录',
    hint: '已在浏览器打开登录页…',
    expired: '登录已过期，点击重新登录',
    waiting: '授权页已在浏览器新标签页打开，请在那里完成登录。',
    blocked: '浏览器拦截了授权页的弹出。',
    openAgain: '点此打开授权页',
    notSeen: '没看到？点此打开授权页',
    cancel: '取消',
    stale: '页面与 webui 的连接已失效（webui 可能已重启）。请在终端运行 /webui，用它打印的新链接打开。',
    startFailed: '登录没能开始：{error}',
    notCompleted: '登录没有完成（{status}），请重试。',
    timedOut: '已超时',
  },
  en: {
    signIn: 'Sign in',
    signingIn: 'Signing in…',
    signOut: 'Sign out',
    hint: 'Opened sign-in in your browser…',
    expired: 'Session expired — click to sign in again',
    waiting: 'The sign-in page opened in a new browser tab. Finish signing in there.',
    blocked: 'The browser blocked the sign-in page from opening.',
    openAgain: 'Open the sign-in page',
    notSeen: "Don't see it? Open the sign-in page",
    cancel: 'Cancel',
    stale: 'This page has lost its connection to the webui (it may have restarted). Run /webui in the terminal and open the new link it prints.',
    startFailed: 'Could not start signing in: {error}',
    notCompleted: 'Signing in did not finish ({status}). Try again.',
    timedOut: 'timed out',
  },
};

/** Fired when the daemon stops recognising this page (it restarted with a new
 *  token): `main.tsx` swaps the app for the page that says where to get the
 *  new link, since every request from here on would be refused. */
export const UNAUTHORIZED_EVENT = 'atomcode:unauthorized';

/** A sign-in in progress: the page to finish it on, and whether the browser
 *  let us open it. */
export interface PendingLogin {
  url: string;
  blocked: boolean;
}

// Login/logout state + actions, consumed by the Sidebar settings menu.
export function useAuth() {
  const { lang } = useSettings();
  const labels = L[lang === 'en' ? 'en' : 'zh'];

  const [loggedIn, setLoggedIn] = useState(false);
  // Credentials exist but the token is dead (expired + unrefreshable). The
  // server now probes real usability, so the sidebar can stop claiming
  // "logged in" when chat would actually reject the token.
  const [expired, setExpired] = useState(false);
  const [user, setUser] = useState<UserInfo | null>(null);
  const [busy, setBusy] = useState(false);
  // What a sign-in in progress is waiting on, shown under the button — the
  // tooltip that used to say it was never seen.
  const [pending, setPending] = useState<PendingLogin | null>(null);
  // Why the last attempt did not finish. Every failure used to be silent.
  const [problem, setProblem] = useState<string | null>(null);
  const busyRef = useRef(false);
  const pollTimer = useRef<number | null>(null);
  const loginGeneration = useRef(0);

  async function refresh(shouldApply: () => boolean = () => true) {
    try {
      const r = await fetch('/auth/status', { headers: authHeaders() });
      if (r.status === 401) {
        window.dispatchEvent(new Event(UNAUTHORIZED_EVENT));
        return;
      }
      const s = await r.json();
      if (!shouldApply()) return;
      setLoggedIn(!!s.logged_in);
      setExpired(!!s.expired);
      setUser(s.user ?? null);
    } catch {
      /* ignore */
    }
  }

  useEffect(() => {
    let active = true;
    const refreshWhileMounted = () => {
      if (active) void refresh(() => active);
    };
    refreshWhileMounted();
    const interval = window.setInterval(refreshWhileMounted, 2_000);
    const onVisibility = () => {
      if (document.visibilityState === 'visible') refreshWhileMounted();
    };
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      active = false;
      window.clearInterval(interval);
      document.removeEventListener('visibilitychange', onVisibility);
      loginGeneration.current += 1;
      if (pollTimer.current !== null) clearTimeout(pollTimer.current);
    };
  }, []);

  async function startLogin() {
    if (busyRef.current) return;
    busyRef.current = true;
    const generation = ++loginGeneration.current;
    setBusy(true);
    setProblem(null);
    try {
      const r = await fetch('/auth/login/start', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', ...authHeaders() },
        body: JSON.stringify({ open_browser: false }),
      });
      if (r.status === 401) {
        // The daemon no longer knows this page: signing in cannot even start.
        busyRef.current = false;
        setBusy(false);
        setProblem(labels.stale);
        window.dispatchEvent(new Event(UNAUTHORIZED_EVENT));
        return;
      }
      if (!r.ok) {
        const body = (await r.json().catch(() => ({}))) as { error?: string };
        throw new Error(body.error || `HTTP ${r.status}`);
      }
      const start = await r.json();
      if (start?.url) setPending({ url: start.url, blocked: !openSignIn(start.url) });
      const id = start?.login_id;
      if (!id) {
        busyRef.current = false;
        setBusy(false);
        return;
      }
      const deadline = Date.now() + Math.max(1, start.expires_in_seconds ?? 600) * 1000;
      const schedule = (delayMs: number) => {
        if (loginGeneration.current !== generation) return;
        pollTimer.current = window.setTimeout(() => void poll(), Math.max(100, delayMs));
      };
      const poll = async () => {
        if (loginGeneration.current !== generation) return;
        if (Date.now() >= deadline) {
          await fetch(`/auth/login/${encodeURIComponent(id)}`, {
            method: 'DELETE',
            headers: authHeaders(),
          }).catch(() => undefined);
          stopPolling(fill(labels.notCompleted, { status: labels.timedOut }));
          return;
        }
        try {
          const response = await fetch(`/auth/login/${encodeURIComponent(id)}/poll`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', ...authHeaders() },
          });
          const result = await response.json().catch(() => ({}));
          if (!response.ok) {
            if (result.retryable === true && Date.now() < deadline) {
              schedule(2000);
            } else {
              stopPolling(
                fill(labels.notCompleted, { status: result.error ?? `HTTP ${response.status}` }),
              );
            }
            return;
          }
          if (result.status === 'pending') {
            schedule(result.retry_after_ms ?? 2000);
            return;
          }
          if (result.status === 'authorized') {
            await refresh();
            stopPolling();
            return;
          }
          // expired / cancelled / failed are terminal login states.
          stopPolling(fill(labels.notCompleted, { status: String(result.status ?? '?') }));
        } catch {
          if (Date.now() < deadline) {
            schedule(2000);
          } else {
            stopPolling(fill(labels.notCompleted, { status: labels.timedOut }));
          }
        }
      };
      await poll();
    } catch (error) {
      busyRef.current = false;
      setBusy(false);
      setPending(null);
      setProblem(
        fill(labels.startFailed, { error: error instanceof Error ? error.message : String(error) }),
      );
    }
  }

  /** Stop waiting. `why` says what went wrong, when something did. */
  function stopPolling(why?: string) {
    busyRef.current = false;
    loginGeneration.current += 1;
    if (pollTimer.current !== null) {
      clearTimeout(pollTimer.current);
      pollTimer.current = null;
    }
    setBusy(false);
    setPending(null);
    setProblem(why ?? null);
  }

  /** Give up on the sign-in in progress, so another can be started. */
  function cancelLogin() {
    stopPolling();
  }

  /** Open the sign-in page again — it was blocked, or closed, or never seen. */
  function reopenLogin() {
    if (!pending) return;
    setPending({ url: pending.url, blocked: !openSignIn(pending.url) });
  }

  async function doLogout() {
    stopPolling();
    try {
      await fetch('/auth/logout', { method: 'POST', headers: authHeaders() });
    } catch {
      /* ignore */
    }
    setLoggedIn(false);
    setExpired(false);
    setUser(null);
  }

  return {
    loggedIn,
    expired,
    user,
    busy,
    labels,
    pending,
    problem,
    startLogin,
    cancelLogin,
    reopenLogin,
    dismissProblem: () => setProblem(null),
    doLogout,
  };
}
