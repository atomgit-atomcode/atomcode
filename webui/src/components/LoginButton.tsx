// Account / login button for the sidebar bottom bar (left of settings).
// Self-contained: reads the webui token from the URL, calls /auth/* directly,
// and picks zh/en labels from settings — to stay decoupled from api.ts/i18n.ts.

import { useEffect, useRef, useState } from 'preact/hooks';
import { useSettings } from '../settings';

const TOKEN = new URLSearchParams(location.search).get('token') ?? '';
function authHeaders(): Record<string, string> {
  return TOKEN ? { Authorization: 'Bearer ' + TOKEN } : {};
}

interface UserInfo {
  username: string;
  name?: string | null;
  email?: string | null;
  avatar_url?: string | null;
}

const L = {
  zh: { signIn: '登录', signingIn: '登录中…', signOut: '退出登录', hint: '已在浏览器打开登录页…' },
  en: { signIn: 'Sign in', signingIn: 'Signing in…', signOut: 'Sign out', hint: 'Opened sign-in in your browser…' },
};

export function LoginButton() {
  const { lang } = useSettings();
  const tt = L[lang === 'en' ? 'en' : 'zh'];

  const [loggedIn, setLoggedIn] = useState(false);
  const [user, setUser] = useState<UserInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const pollTimer = useRef<number | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);

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

  // Close the logout menu on outside click.
  useEffect(() => {
    if (!menuOpen) return;
    const h = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setMenuOpen(false);
    };
    document.addEventListener('mousedown', h);
    return () => document.removeEventListener('mousedown', h);
  }, [menuOpen]);

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
    setMenuOpen(false);
    try {
      await fetch('/auth/logout', { method: 'POST', headers: authHeaders() });
    } catch {
      /* ignore */
    }
    setLoggedIn(false);
    setUser(null);
  }

  if (loggedIn) {
    const label = user?.name || user?.username || 'account';
    const initial = label.slice(0, 1).toUpperCase();
    const avatar = user?.avatar_url;
    return (
      <div class="sidebar-login" ref={menuRef}>
        <button
          class="sidebar-login-btn is-account"
          onClick={() => setMenuOpen((o) => !o)}
          title={label}
        >
          <span class="login-avatar">
            {avatar ? (
              <img
                class="login-avatar-img"
                src={avatar}
                alt={label}
                referrerpolicy="no-referrer"
                onError={(e) => {
                  // Fall back to the initial if the image fails to load.
                  (e.currentTarget as HTMLImageElement).style.display = 'none';
                }}
              />
            ) : (
              initial
            )}
          </span>
          <span class="login-name">{label}</span>
        </button>
        {menuOpen && (
          <div class="item-menu login-menu">
            <button class="item-menu-row" onClick={doLogout}>
              {tt.signOut}
            </button>
          </div>
        )}
      </div>
    );
  }

  return (
    <button
      class="sidebar-login-btn"
      onClick={startLogin}
      disabled={busy}
      title={busy ? tt.hint : tt.signIn}
    >
      <span class="login-avatar empty">👤</span>
      <span class="login-name">{busy ? tt.signingIn : tt.signIn}</span>
    </button>
  );
}
