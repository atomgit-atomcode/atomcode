// Account / login state for the settings menu (the gear's "Account" entry).
// Self-contained: reads the webui token from the URL, calls /auth/* directly,
// and picks zh/en labels from settings — to stay decoupled from api.ts/i18n.ts.

import { useEffect, useRef, useState } from 'preact/hooks';
import { useSettings } from '../settings';

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
  },
  en: {
    signIn: 'Sign in',
    signingIn: 'Signing in…',
    signOut: 'Sign out',
    hint: 'Opened sign-in in your browser…',
    expired: 'Session expired — click to sign in again',
  },
};

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
  const busyRef = useRef(false);
  const pollTimer = useRef<number | null>(null);
  const loginGeneration = useRef(0);

  async function refresh(shouldApply: () => boolean = () => true) {
    try {
      const r = await fetch('/auth/status', { headers: authHeaders() });
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
    try {
      const r = await fetch('/auth/login/start', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', ...authHeaders() },
        body: JSON.stringify({ open_browser: false }),
      });
      if (!r.ok) throw new Error(`Login start failed: ${r.status}`);
      const start = await r.json();
      if (start?.url) window.open(start.url, '_blank', 'noopener');
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
          stopPolling();
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
              stopPolling();
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
          stopPolling();
        } catch {
          if (Date.now() < deadline) schedule(2000); else stopPolling();
        }
      };
      await poll();
    } catch {
      busyRef.current = false;
      setBusy(false);
    }
  }

  function stopPolling() {
    busyRef.current = false;
    loginGeneration.current += 1;
    if (pollTimer.current !== null) {
      clearTimeout(pollTimer.current);
      pollTimer.current = null;
    }
    setBusy(false);
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

  return { loggedIn, expired, user, busy, labels, startLogin, doLogout };
}
