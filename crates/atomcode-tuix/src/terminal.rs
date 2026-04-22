// crates/atomcode-tuix/src/terminal.rs
use std::io::IsTerminal;

/// All environment signals we care about for rendering decisions.
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
    pub lang: Option<String>,
    pub lc_all: Option<String>,
}

impl EnvView {
    pub fn probe() -> Self {
        Self {
            is_stdout_tty: std::io::stdout().is_terminal(),
            no_color: std::env::var("NO_COLOR").is_ok(),
            term: std::env::var("TERM").ok(),
            colorterm: std::env::var("COLORTERM").ok(),
            force_ascii: std::env::var("ATOMCODE_ASCII").is_ok(),
            lang: std::env::var("LANG").ok(),
            lc_all: std::env::var("LC_ALL").ok(),
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
        let unicode_symbols = !env.force_ascii && !is_dumb && !ascii_locale;

        Self {
            tty,
            colors: tty && !env.no_color && !is_dumb,
            spinner: tty && !is_dumb,
            bracketed_paste: tty && !is_dumb,
            raw_mode: tty && !is_dumb,
            scroll_region: tty && !is_dumb,
            unicode_symbols,
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
        if self.unicode_symbols { "\u{276f} " } else { "> " }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_env_disables_colors() {
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: true,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
            force_ascii: false,
            lang: Some("en_US.UTF-8".to_string()),
            lc_all: None,
        });
        assert!(!caps.colors);
        assert!(caps.tty);
        assert!(caps.spinner); // 非 dumb + 是 tty 仍保留 spinner
    }

    #[test]
    fn non_tty_forces_plain_mode() {
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: false,
            no_color: false,
            term: Some("xterm".to_string()),
            colorterm: None,
            force_ascii: false,
            lang: Some("en_US.UTF-8".to_string()),
            lc_all: None,
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
            is_stdout_tty: true,
            no_color: false,
            term: Some("dumb".to_string()),
            colorterm: None,
            force_ascii: false,
            lang: Some("en_US.UTF-8".to_string()),
            lc_all: None,
        });
        assert!(caps.tty);
        assert!(!caps.colors);
        assert!(!caps.spinner);
        assert!(!caps.unicode_symbols, "dumb TERM forces ASCII fallback");
    }

    #[test]
    fn atomcode_ascii_env_forces_ascii() {
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: false,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
            force_ascii: true,
            lang: Some("en_US.UTF-8".to_string()),
            lc_all: None,
        });
        assert!(!caps.unicode_symbols);
        assert_eq!(caps.prompt_chevron(), "> ");
    }

    #[test]
    fn c_locale_forces_ascii() {
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: false,
            term: Some("xterm-256color".to_string()),
            colorterm: None,
            force_ascii: false,
            lang: Some("C".to_string()),
            lc_all: None,
        });
        assert!(!caps.unicode_symbols, "LANG=C → ASCII fallback");
    }

    #[test]
    fn lc_all_wins_over_lang() {
        // POSIX: LC_ALL overrides LANG.
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: false,
            term: Some("xterm-256color".to_string()),
            colorterm: None,
            force_ascii: false,
            lang: Some("en_US.UTF-8".to_string()),
            lc_all: Some("C".to_string()),
        });
        assert!(!caps.unicode_symbols);
    }

    #[test]
    fn utf8_locale_keeps_unicode() {
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: false,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
            force_ascii: false,
            lang: Some("zh_CN.UTF-8".to_string()),
            lc_all: None,
        });
        assert!(caps.unicode_symbols);
        assert_eq!(caps.prompt_chevron(), "\u{276f} ");
    }

    #[test]
    fn tty_xterm_gets_everything() {
        let caps = TerminalCaps::from_env(EnvView {
            is_stdout_tty: true,
            no_color: false,
            term: Some("xterm-256color".to_string()),
            colorterm: Some("truecolor".to_string()),
            force_ascii: false,
            lang: Some("en_US.UTF-8".to_string()),
            lc_all: None,
        });
        assert!(caps.tty);
        assert!(caps.colors);
        assert!(caps.spinner);
        assert!(caps.bracketed_paste);
        assert!(caps.raw_mode);
        assert!(caps.unicode_symbols);
    }
}
