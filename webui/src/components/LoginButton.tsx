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
  zh: { signIn: '登录', signingIn: '登录中…', signOut: '退出登录', hint: '已在浏览器打开登录页…' },
  en: { signIn: 'Sign in', signingIn: 'Signing in…', signOut: 'Sign out', hint: 'Opened sign-in in your browser…' },
};

// Login/logout state + actions, consumed by the Sidebar settings menu.
export function useAuth() {
  const { lang } = useSettings();
  const labels = L[lang === 'en' ? 'en' : 'zh'];

  const [loggedIn, setLoggedIn] = useState(false);
  const [user, setUser] = useState<UserInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const pollTimer = useRef<number | null>(null);

  async function refresh() {
    try {
      const r = await fetch('/auth/status', { headers: authHeaders() });
      const s = await r.json();
      setLoggedIn(!!s.logged_in);
      setUser(s.user ?? null);
    } catch {
      /* ignore */
    }
  }

  useEffect(() => {
    refresh();
    return () => {
      if (pollTimer.current) clearInterval(pollTimer.current);
    };
  }, []);

  async function startLogin() {
    if (busy) return;
    setBusy(true);
    try {
      const r = await fetch('/auth/login/start', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', ...authHeaders() },
        body: JSON.stringify({ open_browser: false }),
      });
      const start = await r.json();
      if (start?.url) window.open(start.url, '_blank', 'noopener');
      const id = start?.login_id;
      if (!id) {
        setBusy(false);
        return;
      }
      // Poll until the login completes (or ~5 min timeout).
      let ticks = 0;
      pollTimer.current = window.setInterval(async () => {
        ticks += 1;
        try {
          await fetch(`/auth/login/${encodeURIComponent(id)}/poll`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', ...authHeaders() },
          });
          const sr = await fetch('/auth/status', { headers: authHeaders() });
          const s = await sr.json();
          if (s.logged_in) {
            setLoggedIn(true);
            setUser(s.user ?? null);
            stopPolling();
          }
        } catch {
          /* keep polling */
        }
        if (ticks > 150) stopPolling(); // ~5 min at 2s
      }, 2000);
    } catch {
      setBusy(false);
    }
  }

  function stopPolling() {
    if (pollTimer.current) {
      clearInterval(pollTimer.current);
      pollTimer.current = null;
    }
    setBusy(false);
  }

  async function doLogout() {
    try {
      await fetch('/auth/logout', { method: 'POST', headers: authHeaders() });
    } catch {
      /* ignore */
    }
    setLoggedIn(false);
    setUser(null);
  }

  return { loggedIn, user, busy, labels, startLogin, doLogout };
}
