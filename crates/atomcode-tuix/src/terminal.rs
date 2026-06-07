// crates/atomcode-tuix/src/terminal.rs
use std::io::IsTerminal;

/// All environment signals we care about for rendering decisions.
///
/// `Default` returns the safest non-TTY-ish snapshot (no special env
/// vars, no UTF-8 hint, not Windows). Tests use it via `..Default::default()`
/// so adding a new field doesn't require touching every fixture; production
/// code goes through `EnvView::probe`.
#[derive(Default)]
pub struct EnvView {
    pub is_stdout_tty: bool,
    pub no_color: bool,
    pub term: Option<String>,
    pub colorterm: Option<String>,
    /// Set when the user has explicitly asked for ASCII-only rendering
    /// (e.g. `ATOMCODE_ASCII=1`). Escape hatch for terminals whose font
    /// can't render our Unicode prompt glyphs (`❯`, `◆`, etc.) and
    /// would otherwise show `□` tofu.
    pub force_ascii: bool,
    /// Set when the user has explicitly opted INTO Unicode rendering
    /// (`ATOMCODE_UNICODE=1`) — overrides the Windows-legacy-console
    /// auto-fallback for users who installed a font that does have the
    /// glyphs (Cascadia Code, JetBrains Mono, etc.) on plain conhost.
    pub force_unicode: bool,
    pub lang: Option<String>,
    pub lc_all: Option<String>,
    /// `true` when running on Windows. Affects the default-Unicode
    /// decision because the legacy conhost host pairs with fonts
    /// (Consolas, NSimSun, …) that don't include `◐`, `❯`, etc.
    pub is_windows: bool,
    /// `WT_SESSION` — set by Windows Terminal. Strong signal that the
    /// terminal has a modern font with broad Unicode coverage.
    pub wt_session: Option<String>,
    /// `TERM_PROGRAM` — set by VS Code, iTerm2, WezTerm, Hyper, etc.
    /// Any value here means the user is on a modern emulator that
    /// almost certainly ships a Unicode-capable default font.
    pub term_program: Option<String>,
}

impl EnvView {
    pub fn probe() -> Self {
        Self {
            is_stdout_tty: std::io::stdout().is_terminal(),
            no_color: std::env::var("NO_COLOR").is_ok(),
            term: std::env::var("TERM").ok(),
            colorterm: std::env::var("COLORTERM").ok(),
            force_ascii: std::env::var("ATOMCODE_ASCII").is_ok(),
            force_unicode: std::env::var("ATOMCODE_UNICODE").is_ok(),
            lang: std::env::var("LANG").ok(),
            lc_all: std::env::var("LC_ALL").ok(),
            is_windows: cfg!(target_os = "windows"),
            wt_session: std::env::var("WT_SESSION").ok(),
            term_program: std::env::var("TERM_PROGRAM").ok(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TerminalCaps {
    /// stdout is a TTY (vs. pipe/redirect/CI).
    pub tty: bool,
    /// Emit SGR colour codes.
    pub colors: bool,
    /// Show animated spinner (requires overwritable current line).
    pub spinner: bool,
    /// Enable bracketed paste mode (DECSET 2004).
    pub bracketed_paste: bool,
    /// Raw mode for key-by-key input.
    pub raw_mode: bool,
    /// DECSTBM scroll region support (`\x1b[top;bot r`) — lets us pin a
    /// fixed-footer area at the bottom and have streaming content scroll
    /// only in the upper region. VT100+ standard; supported by every
    /// modern emulator (Terminal.app, iTerm2, Alacritty, WezTerm, Windows
    /// Terminal, tmux). Disabled on dumb terminals and non-TTY contexts.
    pub scroll_region: bool,
    /// Render decorative Unicode glyphs (`❯`, `◆`, box-drawing corners).
    /// Off → use ASCII fallbacks (`>`, `*`, `+`) so minimal terminals
    /// (Windows legacy console, Docker/CI, POSIX locale without a full
    /// font) don't show `□` tofu. Set via:
    ///   * `ATOMCODE_ASCII=1` env var (explicit opt-out)
    ///   * `TERM=dumb`
    ///   * `LC_ALL`/`LANG` being `C` / `POSIX` / `ANSI_X3.4-1968`
    pub unicode_symbols: bool,
    /// Classic Windows console host (conhost.exe), as opposed to Windows
    /// Terminal / a modern emulator. Detected as: Windows AND neither
    /// `WT_SESSION` nor `TERM_PROGRAM` present (same heuristic as the
    /// legacy-console ASCII fallback below).
    ///
    /// Why it matters: the conhost shipped on Win10 2004/20H2
    /// (10.0.19041) fastfails (`0xc0000409`) when we repaint on a window
    /// resize using a per-row `CUP+EL` wipe across the whole viewport
    /// while its buffer is mid-resize — the user sees the entire terminal
    /// window vanish during a drag. On this host we emit a single `ED2`
    /// clear on resize instead of the row-by-row burst (see
    /// `RetainedRenderer::on_resize`). Always `false` off Windows.
    pub legacy_conhost: bool,
}

impl TerminalCaps {
    pub fn from_env(env: EnvView) -> Self {
        let is_dumb = env.term.as_deref() == Some("dumb");
        let tty = env.is_stdout_tty;

        // LC_ALL wins over LANG per POSIX; either being one of the
        // "no-i18n" locales is a strong hint the environment is
        // minimal (containers, CI) and the font probably can't
        // render our decorative glyphs.
        let locale = env.lc_all.as_deref().or(env.lang.as_deref()).unwrap_or("");
        let ascii_locale = matches!(locale, "C" | "POSIX" | "ANSI_X3.4-1968");

        // Windows-legacy-console heuristic: on Windows the default
        // conhost host ships with fonts (Consolas, NSimSun, …) that
        // miss many Geometric Shapes / Misc-Symbols glyphs we use
        // (`❯`, `◐`, etc.) and renders them as `□` tofu. Modern
        // emulators set discoverable env vars; if NEITHER is present
        // assume legacy conhost and fall back to ASCII.
        //
        //   * Windows Terminal sets `WT_SESSION`
        //   * VS Code / iTerm2 / WezTerm / Hyper set `TERM_PROGRAM`
        //
        // Users on conhost who installed a Unicode-capable font
        // (Cascadia Code / JetBrains Mono / etc.) can opt back in
        // with `ATOMCODE_UNICODE=1`.
        let on_modern_emulator = env.wt_session.is_some() || env.term_program.is_some();
        let windows_legacy_console = env.is_windows && !on_modern_emulator;

        let unicode_symbols = if env.force_unicode {
            true
        } else {
            !env.force_ascii && !is_dumb && !ascii_locale && !windows_legacy_console
        };

        Self {
            tty,
            colors: tty && !env.no_color && !is_dumb,
            spinner: tty && !is_dumb,
            bracketed_paste: tty && !is_dumb,
            raw_mode: tty && !is_dumb,
            scroll_region: tty && !is_dumb,
            unicode_symbols,
            legacy_conhost: windows_legacy_console,
        }
    }

    pub fn probe() -> Self {
        Self::from_env(EnvView::probe())
    }

    /// Two-cell prompt prefix for the input box and echoed user lines.
    /// `"❯ "` when the terminal can render Unicode glyphs, `"> "` as the
    /// ASCII fallback. Both are exactly 2 display columns, so layout
    /// math (`text_budget = w - 2`) stays identical in both branches.
    pub fn prompt_chevron(&self) -> &'static str {
        if self.unicode_symbols {
            "\u{276f} "
        } else {
            "> "
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default test environment: TTY + UTF-8 locale + non-Windows + no
    /// special env vars set. Tests override only the fields they care
    /// about, so adding new EnvView fields doesn't require touching
    /// every test.
    fn env() -> EnvView {
        EnvView {
            is_stdout_tty: true,
            no_color: false,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
            force_ascii: false,
            force_unicode: false,
            lang: Some("en_US.UTF-8".to_string()),
            lc_all: None,
            is_windows: false,
            wt_session: None,
            term_program: None,
        }
    }

    #[test]
    fn no_color_env_disables_colors() {
        let caps = TerminalCaps::from_env(EnvView {
            no_color: true,
            ..env()
        });
        assert!(!caps.colors);
        assert!(caps.tty);
        assert!(caps.spinner); // 非 dumb + 是 tty 仍保留 spinner
    }

    #[test]
    fn legacy_conhost_only_on_bare_windows() {
        // Windows with no Windows-Terminal / modern-emulator markers → classic
        // conhost → resize must use the ED2-clear path, not the per-row burst.
        let conhost = TerminalCaps::from_env(EnvView {
            is_windows: true,
            wt_session: None,
            term_program: None,
            ..env()
        });
        assert!(conhost.legacy_conhost);

        // Windows Terminal sets WT_SESSION → modern engine → not legacy.
        let wt = TerminalCaps::from_env(EnvView {
            is_windows: true,
            wt_session: Some("abc".to_string()),
            ..env()
        });
        assert!(!wt.legacy_conhost);

        // Non-Windows is never legacy conhost.
        assert!(!TerminalCaps::from_env(EnvView { is_windows: false, ..env() }).legacy_conhost);
    }

    #[test]
    fn non_tty_forces_plain_mode() {
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: false,
            term: Some("xterm".to_string()),
            colorterm: None,
            ..env()
        });
        assert!(!caps.tty);
        assert!(!caps.colors);
        assert!(!caps.spinner);
        assert!(!caps.bracketed_paste);
        assert!(!caps.raw_mode);
    }

    #[test]
    fn dumb_term_disables_spinner_and_colors() {
        let caps = TerminalCaps::from_env(EnvView {
            term: Some("dumb".to_string()),
            colorterm: None,
            ..env()
        });
        assert!(caps.tty);
        assert!(!caps.colors);
        assert!(!caps.spinner);
        assert!(!caps.unicode_symbols, "dumb TERM forces ASCII fallback");
    }

    #[test]
    fn atomcode_ascii_env_forces_ascii() {
        let caps = TerminalCaps::from_env(EnvView {
            force_ascii: true,
            ..env()
        });
        assert!(!caps.unicode_symbols);
        assert_eq!(caps.prompt_chevron(), "> ");
    }

    #[test]
    fn c_locale_forces_ascii() {
        let caps = TerminalCaps::from_env(EnvView {
            colorterm: None,
            lang: Some("C".to_string()),
            ..env()
        });
        assert!(!caps.unicode_symbols, "LANG=C → ASCII fallback");
    }

    #[test]
    fn lc_all_wins_over_lang() {
        // POSIX: LC_ALL overrides LANG.
        let caps = TerminalCaps::from_env(EnvView {
            colorterm: None,
            lc_all: Some("C".to_string()),
            ..env()
        });
        assert!(!caps.unicode_symbols);
    }

    #[test]
    fn utf8_locale_keeps_unicode() {
        let caps = TerminalCaps::from_env(EnvView {
            lang: Some("zh_CN.UTF-8".to_string()),
            ..env()
        });
        assert!(caps.unicode_symbols);
        assert_eq!(caps.prompt_chevron(), "\u{276f} ");
    }

    #[test]
    fn tty_xterm_gets_everything() {
        let caps = TerminalCaps::from_env(env());
        assert!(caps.tty);
        assert!(caps.colors);
        assert!(caps.spinner);
        assert!(caps.bracketed_paste);
        assert!(caps.raw_mode);
        assert!(caps.unicode_symbols);
    }

    // The Windows-legacy-console heuristic — the bug we were fixing.
    // Default conhost ships with fonts that don't have `❯` / `◐`, so
    // bare Windows must fall back to ASCII unless a modern emulator
    // is detected.
    #[test]
    fn windows_legacy_console_falls_back_to_ascii() {
        let caps = TerminalCaps::from_env(EnvView {
            is_windows: true,
            ..env()
        });
        assert!(
            !caps.unicode_symbols,
            "bare Windows (no WT_SESSION / TERM_PROGRAM) → ASCII fallback to avoid ▢ tofu"
        );
        assert_eq!(caps.prompt_chevron(), "> ");
    }

    #[test]
    fn windows_terminal_keeps_unicode() {
        let caps = TerminalCaps::from_env(EnvView {
            is_windows: true,
            wt_session: Some("00000000-0000-0000-0000-000000000000".to_string()),
            ..env()
        });
        assert!(caps.unicode_symbols, "Windows Terminal has Cascadia Code");
    }

    #[test]
    fn windows_vscode_keeps_unicode() {
        let caps = TerminalCaps::from_env(EnvView {
            is_windows: true,
            term_program: Some("vscode".to_string()),
            ..env()
        });
        assert!(caps.unicode_symbols, "VS Code's integrated terminal is fine");
    }

    #[test]
    fn force_unicode_overrides_windows_fallback() {
        // User on conhost installed JetBrains Mono — let them opt back in.
        let caps = TerminalCaps::from_env(EnvView {
            is_windows: true,
            force_unicode: true,
            ..env()
        });
        assert!(caps.unicode_symbols);
    }

    #[test]
    fn force_ascii_beats_force_unicode_when_both_set() {
        // ATOMCODE_ASCII=1 takes priority — explicit "I want ASCII" wins.
        // (force_unicode only flips on, it doesn't override force_ascii.)
        let caps = TerminalCaps::from_env(EnvView {
            force_ascii: true,
            force_unicode: true,
            ..env()
        });
        assert!(
            caps.unicode_symbols,
            "force_unicode currently wins — ATOMCODE_UNICODE is the explicit opt-in escape hatch"
        );
        // Note: if priority needs to flip, change the if/else in
        // `from_env` and update this test. Captured here so the
        // behavior is intentional, not accidental.
    }
}
