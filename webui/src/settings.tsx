// Settings store: theme (light/dark/system) + language (zh/en), persisted to
// localStorage and exposed via a Preact context. `t()` does message lookup +
// {placeholder} interpolation against the i18n catalog.

import { createContext, ComponentChildren } from 'preact';
import { useContext, useEffect, useState } from 'preact/hooks';
import { messages, Lang, MsgKey } from './i18n';

export type Theme = 'light' | 'dark' | 'system';

/** Which settings dialog to open from the sidebar settings menu. */
export type SettingsSection = 'theme' | 'language' | 'model' | 'remote';

type TParams = Record<string, string | number>;

interface SettingsCtx {
  theme: Theme;
  setTheme: (t: Theme) => void;
  lang: Lang;
  setLang: (l: Lang) => void;
  t: (key: MsgKey, params?: TParams) => string;
}

const Ctx = createContext<SettingsCtx | null>(null);

const THEME_KEY = 'atomcode.theme';
const LANG_KEY = 'atomcode.lang';

function readTheme(): Theme {
  try {
    const v = localStorage.getItem(THEME_KEY);
    if (v === 'light' || v === 'dark' || v === 'system') return v;
  } catch {
    /* ignore */
  }
  // Default to the warm-ivory light theme (claude.ai look) when the user
  // hasn't picked one; they can still switch to dark/system in settings.
  return 'light';
}

function readLang(): Lang {
  try {
    const v = localStorage.getItem(LANG_KEY);
    if (v === 'zh' || v === 'en') return v;
  } catch {
    /* ignore */
  }
  return 'zh';
}

export function SettingsProvider({ children }: { children: ComponentChildren }) {
  const [theme, setThemeState] = useState<Theme>(readTheme);
  const [lang, setLangState] = useState<Lang>(readLang);

  // Apply theme to <html data-theme>; theme.css keys light/dark off this.
  useEffect(() => {
    document.documentElement.setAttribute('data-theme', theme);
    try {
      localStorage.setItem(THEME_KEY, theme);
    } catch {
      /* ignore */
    }
  }, [theme]);

  useEffect(() => {
    document.documentElement.setAttribute('lang', lang === 'zh' ? 'zh-CN' : 'en');
    try {
      localStorage.setItem(LANG_KEY, lang);
    } catch {
      /* ignore */
    }
  }, [lang]);

  function t(key: MsgKey, params?: TParams): string {
    const table = messages[lang] ?? messages.zh;
    let s = table[key] ?? messages.zh[key] ?? key;
    if (params) {
      for (const k of Object.keys(params)) {
        s = s.split(`{${k}}`).join(String(params[k]));
      }
    }
    return s;
  }

  return (
    <Ctx.Provider
      value={{ theme, setTheme: setThemeState, lang, setLang: setLangState, t }}
    >
      {children}
    </Ctx.Provider>
  );
}

export function useSettings(): SettingsCtx {
  const c = useContext(Ctx);
  if (!c) throw new Error('useSettings must be used within <SettingsProvider>');
  return c;
}

/** Convenience hook when a component only needs the translator. */
export function useT(): SettingsCtx['t'] {
  return useSettings().t;
}
