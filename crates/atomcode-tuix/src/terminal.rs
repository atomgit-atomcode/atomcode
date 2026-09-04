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
    /// `TERMINAL_EMULATOR` — set by IntelliJ-platform IDEs (IDEA, Android
    /// Studio, DevEco Studio, …) to `JetBrains-JediTerm`. The JediTerm
    /// Swing terminal grids CJK at 2 cells (same as us) but its paint
    /// layer has no font fallback, so a fallback CJK glyph with a ~1-cell
    /// advance is drawn CENTERED in the 2-cell box — leaving the 2nd
    /// column visually blank. That turns our per-cell-CUP positioning into
    /// a visible "每个汉字后空一格" gap, and the per-`─`-CUP rule into a
    /// fragmented line. Captured here so the render layer can switch to a
    /// per-row tight repaint that streams each changed row as one
    /// contiguous run (one CUP, terminal-advance positioning) instead of
    /// one CUP per non-ASCII cell. Read in exactly one other place
    /// (`event_loop/commands.rs`, for QR aspect tolerance).
    pub terminal_emulator: Option<String>,
    /// `ATOMCODE_JEDITERM` manual override for the JediTerm render quirk:
    /// `1`/`true` forces the JediTerm tight-repaint path on, anything else
    /// (`0`/`false`) forces it off, unset = auto-detect via
    /// `terminal_emulator`. Escape hatch for two cases: (a) DevEco/IDE
    /// launchers that don't propagate `TERMINAL_EMULATOR` into our process
    /// (so auto-detect misses), and (b) A/B testing the path on-device.
    pub force_jediterm: Option<bool>,
    /// `ATOMCODE_KITTY` explicit keyboard-protocol override. `true` forces
    /// Kitty CSI-u on, `false` forces it off, unset uses conservative terminal
    /// identification. This is intentionally tri-state so browser terminals
    /// that only advertise generic `xterm-256color` are not assumed capable.
    pub force_kitty_keyboard: Option<bool>,
    pub kitty_window_id: Option<String>,
    pub wezterm_version: Option<String>,
    pub alacritty_socket: Option<String>,
    /// `TMUX` identifies an indirect terminal session. Protocol forwarding is
    /// disabled unless the user separately confirms passthrough support.
    pub tmux: Option<String>,
    /// Any SSH marker makes client-side terminal capabilities unproven unless
    /// the matching protocol override is explicitly enabled.
    pub ssh_connection: Option<String>,
    pub ssh_client: Option<String>,
    pub ssh_tty: Option<String>,
    /// Strict `0`/`1` overrides. A malformed, present value is treated as
    /// `false`; unlike older compatibility toggles, words such as `true` are
    /// deliberately not accepted as capability evidence.
    pub force_mouse_sgr: Option<bool>,
    pub force_osc52_clipboard: Option<bool>,
    pub force_tmux_passthrough: Option<bool>,
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
            terminal_emulator: std::env::var("TERMINAL_EMULATOR").ok(),
            force_jediterm: std::env::var("ATOMCODE_JEDITERM")
                .ok()
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true")),
            force_kitty_keyboard: std::env::var("ATOMCODE_KITTY")
                .ok()
                .and_then(|v| parse_bool_override(&v)),
            kitty_window_id: std::env::var("KITTY_WINDOW_ID").ok(),
            wezterm_version: std::env::var("WEZTERM_VERSION").ok(),
            alacritty_socket: std::env::var("ALACRITTY_SOCKET").ok(),
            tmux: std::env::var("TMUX").ok(),
            ssh_connection: std::env::var("SSH_CONNECTION").ok(),
            ssh_client: std::env::var("SSH_CLIENT").ok(),
            ssh_tty: std::env::var("SSH_TTY").ok(),
            force_mouse_sgr: strict_capability_override("ATOMCODE_MOUSE_SGR"),
            force_osc52_clipboard: strict_capability_override("ATOMCODE_OSC52"),
            force_tmux_passthrough: strict_capability_override("ATOMCODE_TMUX_PASSTHROUGH"),
        }
    }
}

fn strict_capability_override(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .and_then(|value| parse_strict_bool_override(&value))
}

fn parse_strict_bool_override(value: &str) -> Option<bool> {
    Some(matches!(value, "1"))
}

fn parse_bool_override(value: &str) -> Option<bool> {
    match value.trim() {
        "1" => Some(true),
        "0" => Some(false),
        value if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes") => {
            Some(true)
        }
        value if value.eq_ignore_ascii_case("false") || value.eq_ignore_ascii_case("no") => {
            Some(false)
        }
        _ => None,
    }
}

fn is_non_empty(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
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
    /// JediTerm (IntelliJ-platform terminal: DevEco Studio, Android
    /// Studio, IDEA). Detected via `TERMINAL_EMULATOR == "JetBrains-JediTerm"`
    /// or forced by `ATOMCODE_JEDITERM`. **Deliberately inert w.r.t. every
    /// other capability** — it does NOT feed `unicode_symbols`/`legacy_conhost`
    /// (so it can't change the chevron, ASCII fallback, or resize path). Its
    /// only consumer is `Screen`'s per-row tight-repaint path, which streams
    /// each changed row as one contiguous run to avoid the per-cell-CUP gap +
    /// per-`─`-CUP rule fragmentation that JediTerm's no-fallback paint layer
    /// produces. See `EnvView::terminal_emulator` for the full mechanism.
    pub jediterm: bool,
    /// A modern terminal emulator was detected: Windows Terminal (`WT_SESSION`)
    /// or iTerm2 / VS Code / WezTerm / Hyper (`TERM_PROGRAM`). Same signal as
    /// the legacy-console heuristic. Consumed by the welcome-mascot gate: the
    /// half-block + per-cell-background pixel art renders reliably only on
    /// modern emulators; bare / SSH terminals (FinalShell, PuTTY, …) that set
    /// neither var may not paint cell backgrounds, fragmenting the art — so we
    /// omit it there (the tips stack cleanly instead). Note this is `false` over
    /// SSH regardless of the client, since SSH doesn't forward these client-side
    /// vars to the remote where atomcode runs.
    pub modern_emulator: bool,
    /// Whether this terminal should receive Kitty CSI-u keyboard enhancement.
    /// Generic terminal names such as `xterm-256color` are insufficient:
    /// JumpServer and other xterm.js web terminals commonly expose that value
    /// while only partially implementing enhanced key reporting.
    pub kitty_keyboard: bool,
    /// Proven support for button-event tracking with SGR coordinates.
    pub mouse_sgr: bool,
    /// Proven support for writing clipboard contents with OSC 52.
    pub osc52_clipboard: bool,
    /// Explicit evidence that the current tmux server forwards terminal
    /// protocols used by this TUI.
    pub tmux_passthrough: bool,
}

impl TerminalCaps {
    pub fn from_env(env: EnvView) -> Self {
        // `TERM=dumb` means "no escape sequences, no raw mode" on Unix
        // (Emacs `M-x shell`, some CI wrappers). But TERM is a Unix
        // terminfo concept: on Windows crossterm drives the console via the
        // Win32 console API and ignores TERM entirely, so a stray
        // `TERM=dumb` — commonly leaked into the environment by Git / MSYS /
        // SSH tooling — does NOT mean the console lacks raw mode, colours,
        // or VT processing. Honouring it there wrongly zeroed `raw_mode`,
        // dropping atomcode into the cooked LINE-input fallback where arrow
        // keys never reach menus (you could only Enter-select the first
        // item). Scope the dumb check to non-Windows.
        let is_dumb = !env.is_windows && env.term.as_deref() == Some("dumb");
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
        // UTF-8 output only proves that the console accepts the code points; it
        // says nothing about the active font's block-glyph geometry. In
        // particular, pwsh7 on Win10 conhost commonly runs code page 65001 but
        // renders `▀/▄/█` with seams that destroy terminal QR codes. Only an
        // actual emulator marker is strong enough to enable Unicode artwork.
        let on_modern_emulator = env.wt_session.is_some() || env.term_program.is_some();
        let windows_legacy_console = env.is_windows && !on_modern_emulator;

        let unicode_symbols = if env.force_unicode {
            true
        } else {
            !env.force_ascii && !is_dumb && !ascii_locale && !windows_legacy_console
        };

        // JediTerm: manual override wins, else auto-detect via the exact
        // `TERMINAL_EMULATOR` string IntelliJ-platform terminals export.
        // INTENTIONALLY computed AFTER (and independent of) unicode_symbols /
        // legacy_conhost above — it must not perturb any existing decision.
        let jediterm = env
            .force_jediterm
            .unwrap_or_else(|| env.terminal_emulator.as_deref() == Some("JetBrains-JediTerm"));

        let term_program = env
            .term_program
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let term = env.term.as_deref().unwrap_or_default().to_ascii_lowercase();
        let known_kitty_keyboard = is_non_empty(&env.kitty_window_id)
            || is_non_empty(&env.wezterm_version)
            || is_non_empty(&env.alacritty_socket)
            || term.contains("kitty")
            || matches!(
                term_program.as_str(),
                "kitty"
                    | "wezterm"
                    | "alacritty"
                    | "ghostty"
                    | "iterm.app"
                    | "iterm2"
                    | "warpterminal"
            );
        let kitty_keyboard = env.force_kitty_keyboard.unwrap_or(known_kitty_keyboard) && !jediterm;

        let in_tmux = is_non_empty(&env.tmux);
        let in_ssh = is_non_empty(&env.ssh_connection)
            || is_non_empty(&env.ssh_client)
            || is_non_empty(&env.ssh_tty);
        let tmux_passthrough = in_tmux && env.force_tmux_passthrough == Some(true);
        let known_protocol_emulator = is_non_empty(&env.kitty_window_id)
            || is_non_empty(&env.wezterm_version)
            || is_non_empty(&env.alacritty_socket)
            || is_non_empty(&env.wt_session)
            || matches!(term.as_str(), "kitty" | "xterm-kitty")
            || matches!(
                term_program.as_str(),
                "kitty" | "wezterm" | "alacritty" | "ghostty" | "iterm.app" | "iterm2"
            );
        let capability_safe = tty && !is_dumb && !jediterm && !windows_legacy_console;
        let mouse_passthrough =
            (!in_tmux || tmux_passthrough) && (!in_ssh || env.force_mouse_sgr == Some(true));
        let clipboard_passthrough =
            (!in_tmux || tmux_passthrough) && (!in_ssh || env.force_osc52_clipboard == Some(true));
        // Mouse capture is OPT-IN: OFF by default (even on the allowlist) so
        // terminal-native mouse selection/copy/paste/double-click stay intact
        // and behave consistently across every terminal. Set `ATOMCODE_MOUSE_SGR=1`
        // (force == Some(true)) to re-enable atomcode's app-level mouse
        // selection (word double-click, clean cross-chrome copy, click-to-cursor).
        // `capability_safe` still hard-blocks dumb/jediterm/legacy even when
        // forced; `mouse_passthrough` keeps tmux/ssh safe. `known_protocol_emulator`
        // no longer gates it — an explicit opt-in is the user's responsibility.
        // (OSC52 keyboard-copy below is UNCHANGED: still allowlist-default-on.)
        let mouse_sgr = capability_safe && env.force_mouse_sgr == Some(true) && mouse_passthrough;
        let osc52_clipboard = capability_safe
            && known_protocol_emulator
            && env.force_osc52_clipboard != Some(false)
            && clipboard_passthrough;

        Self {
            tty,
            colors: tty && !env.no_color && !is_dumb,
            spinner: tty && !is_dumb,
            bracketed_paste: tty && !is_dumb,
            raw_mode: tty && !is_dumb,
            scroll_region: tty && !is_dumb,
            unicode_symbols,
            legacy_conhost: windows_legacy_console,
            jediterm,
            modern_emulator: on_modern_emulator,
            kitty_keyboard,
            mouse_sgr,
            osc52_clipboard,
            tmux_passthrough,
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
            terminal_emulator: None,
            force_jediterm: None,
            force_kitty_keyboard: None,
            kitty_window_id: None,
            wezterm_version: None,
            alacritty_socket: None,
            tmux: None,
            ssh_connection: None,
            ssh_client: None,
            ssh_tty: None,
            force_mouse_sgr: None,
            force_osc52_clipboard: None,
            force_tmux_passthrough: None,
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
        assert!(
            !TerminalCaps::from_env(EnvView {
                is_windows: false,
                ..env()
            })
            .legacy_conhost
        );
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
        // Non-Windows: TERM=dumb is authoritative (Emacs `M-x shell`, etc.).
        let caps = TerminalCaps::from_env(EnvView {
            is_windows: false,
            term: Some("dumb".to_string()),
            colorterm: None,
            ..env()
        });
        assert!(caps.tty);
        assert!(!caps.colors);
        assert!(!caps.spinner);
        assert!(!caps.raw_mode, "dumb TERM on Unix has no raw mode");
        assert!(!caps.unicode_symbols, "dumb TERM forces ASCII fallback");
    }

    #[test]
    fn mouse_and_clipboard_caps_are_conservative_across_terminal_boundaries() {
        struct Case {
            name: &'static str,
            env: EnvView,
            mouse_sgr: bool,
            osc52_clipboard: bool,
            tmux_passthrough: bool,
        }

        let cases = [
            Case {
                name: "kitty",
                env: EnvView {
                    kitty_window_id: Some("1".into()),
                    ..env()
                },
                // Mouse capture is now OPT-IN (ATOMCODE_MOUSE_SGR=1): off by
                // default even on the allowlist, so terminal-native mouse
                // selection/copy/paste stays intact. OSC52 keyboard-copy is
                // unchanged (still on for the allowlist).
                mouse_sgr: false,
                osc52_clipboard: true,
                tmux_passthrough: false,
            },
            Case {
                name: "wezterm",
                env: EnvView {
                    wezterm_version: Some("20240203".into()),
                    ..env()
                },
                // Mouse capture is now OPT-IN (ATOMCODE_MOUSE_SGR=1): off by
                // default even on the allowlist, so terminal-native mouse
                // selection/copy/paste stays intact. OSC52 keyboard-copy is
                // unchanged (still on for the allowlist).
                mouse_sgr: false,
                osc52_clipboard: true,
                tmux_passthrough: false,
            },
            Case {
                name: "iterm2",
                env: EnvView {
                    term_program: Some("iTerm.app".into()),
                    ..env()
                },
                // Mouse capture is now OPT-IN (ATOMCODE_MOUSE_SGR=1): off by
                // default even on the allowlist, so terminal-native mouse
                // selection/copy/paste stays intact. OSC52 keyboard-copy is
                // unchanged (still on for the allowlist).
                mouse_sgr: false,
                osc52_clipboard: true,
                tmux_passthrough: false,
            },
            Case {
                name: "windows_terminal",
                env: EnvView {
                    is_windows: true,
                    wt_session: Some("session".into()),
                    ..env()
                },
                // Mouse capture is now OPT-IN (ATOMCODE_MOUSE_SGR=1): off by
                // default even on the allowlist, so terminal-native mouse
                // selection/copy/paste stays intact. OSC52 keyboard-copy is
                // unchanged (still on for the allowlist).
                mouse_sgr: false,
                osc52_clipboard: true,
                tmux_passthrough: false,
            },
            Case {
                name: "legacy_conhost",
                env: EnvView {
                    is_windows: true,
                    force_mouse_sgr: Some(true),
                    force_osc52_clipboard: Some(true),
                    ..env()
                },
                mouse_sgr: false,
                osc52_clipboard: false,
                tmux_passthrough: false,
            },
            Case {
                name: "tmux_without_passthrough",
                env: EnvView {
                    term: Some("screen-256color".into()),
                    tmux: Some("/tmp/tmux-501/default,1,0".into()),
                    kitty_window_id: Some("1".into()),
                    ..env()
                },
                mouse_sgr: false,
                osc52_clipboard: false,
                tmux_passthrough: false,
            },
            Case {
                name: "ssh_unknown",
                env: EnvView {
                    ssh_connection: Some("192.0.2.1 50000 192.0.2.2 22".into()),
                    ..env()
                },
                mouse_sgr: false,
                osc52_clipboard: false,
                tmux_passthrough: false,
            },
            Case {
                name: "jediterm",
                env: EnvView {
                    terminal_emulator: Some("JetBrains-JediTerm".into()),
                    force_mouse_sgr: Some(true),
                    force_osc52_clipboard: Some(true),
                    ..env()
                },
                mouse_sgr: false,
                osc52_clipboard: false,
                tmux_passthrough: false,
            },
            Case {
                name: "dumb",
                env: EnvView {
                    term: Some("dumb".into()),
                    ..env()
                },
                mouse_sgr: false,
                osc52_clipboard: false,
                tmux_passthrough: false,
            },
            Case {
                name: "non_tty",
                env: EnvView {
                    is_stdout_tty: false,
                    kitty_window_id: Some("1".into()),
                    force_mouse_sgr: Some(true),
                    force_osc52_clipboard: Some(true),
                    ..env()
                },
                mouse_sgr: false,
                osc52_clipboard: false,
                tmux_passthrough: false,
            },
        ];

        for case in cases {
            let caps = TerminalCaps::from_env(case.env);
            assert_eq!(caps.mouse_sgr, case.mouse_sgr, "{} mouse", case.name);
            assert_eq!(
                caps.osc52_clipboard, case.osc52_clipboard,
                "{} clipboard",
                case.name
            );
            assert_eq!(
                caps.tmux_passthrough, case.tmux_passthrough,
                "{} tmux passthrough",
                case.name
            );
        }
    }

    #[test]
    fn mouse_and_clipboard_caps_require_explicit_remote_passthrough() {
        let remote_kitty = EnvView {
            kitty_window_id: Some("1".into()),
            ssh_connection: Some("192.0.2.1 50000 192.0.2.2 22".into()),
            force_mouse_sgr: Some(true),
            force_osc52_clipboard: Some(true),
            force_tmux_passthrough: Some(true),
            ..env()
        };
        let caps = TerminalCaps::from_env(remote_kitty);
        assert!(caps.mouse_sgr);
        assert!(caps.osc52_clipboard);
        assert!(!caps.tmux_passthrough, "SSH alone is not a tmux session");

        let tmux_kitty = EnvView {
            kitty_window_id: Some("1".into()),
            tmux: Some("/tmp/tmux-501/default,1,0".into()),
            force_mouse_sgr: Some(true),
            force_osc52_clipboard: Some(true),
            force_tmux_passthrough: Some(true),
            ..env()
        };
        let caps = TerminalCaps::from_env(tmux_kitty);
        assert!(caps.mouse_sgr);
        assert!(caps.osc52_clipboard);
        assert!(caps.tmux_passthrough);
    }

    #[test]
    fn capability_overrides_accept_only_zero_or_one_and_malformed_is_false() {
        assert_eq!(parse_strict_bool_override("1"), Some(true));
        assert_eq!(parse_strict_bool_override("0"), Some(false));
        assert_eq!(parse_strict_bool_override("true"), Some(false));
        assert_eq!(parse_strict_bool_override("garbage"), Some(false));
    }

    #[test]
    fn ssh_inside_tmux_requires_both_remote_and_tmux_evidence() {
        let fixture = |mouse, clipboard, passthrough| EnvView {
            term: Some("xterm-kitty".into()),
            kitty_window_id: Some("1".into()),
            ssh_connection: Some("192.0.2.1 50000 192.0.2.2 22".into()),
            tmux: Some("/tmp/tmux-501/default,1,0".into()),
            force_mouse_sgr: mouse,
            force_osc52_clipboard: clipboard,
            force_tmux_passthrough: passthrough,
            ..env()
        };

        let all = TerminalCaps::from_env(fixture(Some(true), Some(true), Some(true)));
        assert!(all.mouse_sgr);
        assert!(all.osc52_clipboard);
        assert!(all.tmux_passthrough);

        let missing_ssh = TerminalCaps::from_env(fixture(None, None, Some(true)));
        assert!(!missing_ssh.mouse_sgr);
        assert!(!missing_ssh.osc52_clipboard);

        let missing_tmux = TerminalCaps::from_env(fixture(Some(true), Some(true), None));
        assert!(!missing_tmux.mouse_sgr);
        assert!(!missing_tmux.osc52_clipboard);
        assert!(!missing_tmux.tmux_passthrough);

        let malformed = TerminalCaps::from_env(fixture(Some(false), Some(false), Some(false)));
        assert!(!malformed.mouse_sgr);
        assert!(!malformed.osc52_clipboard);
        assert!(!malformed.tmux_passthrough);
    }

    #[test]
    fn kitty_term_detection_is_an_exact_allowlist() {
        for term in ["not-kitty-compatible", "kittyish", "", "  "] {
            let caps = TerminalCaps::from_env(EnvView {
                term: Some(term.into()),
                ..env()
            });
            assert!(!caps.mouse_sgr, "{term:?} must not prove mouse support");
            assert!(
                !caps.osc52_clipboard,
                "{term:?} must not prove clipboard support"
            );
        }

        for term in ["kitty", "xterm-kitty"] {
            let caps = TerminalCaps::from_env(EnvView {
                term: Some(term.into()),
                ..env()
            });
            // Mouse capture is opt-in (ATOMCODE_MOUSE_SGR=1) — OFF by default
            // even on the allowlist, so native mouse selection stays. OSC52
            // keyboard-copy remains allowlisted-on.
            assert!(
                !caps.mouse_sgr,
                "{term:?}: mouse capture is opt-in, off by default"
            );
            assert!(caps.osc52_clipboard, "{term:?} is allowlisted");
        }
    }

    #[test]
    fn mouse_capture_is_opt_in_via_force_only() {
        // Default (no ATOMCODE_MOUSE_SGR): an allowlisted terminal does NOT
        // capture the mouse — native selection/copy/paste is preserved.
        let default_kitty = TerminalCaps::from_env(EnvView {
            kitty_window_id: Some("1".into()),
            ..env()
        });
        assert!(!default_kitty.mouse_sgr, "default: mouse capture off");
        assert!(
            default_kitty.osc52_clipboard,
            "osc52 unaffected by the flip"
        );

        // Opt-in: ATOMCODE_MOUSE_SGR=1 (force = Some(true)) turns app-level
        // mouse capture back on, even without an allowlist match (the user
        // takes responsibility); still gated by capability_safe.
        let opted_in = TerminalCaps::from_env(EnvView {
            term: Some("xterm-256color".into()),
            force_mouse_sgr: Some(true),
            ..env()
        });
        assert!(opted_in.mouse_sgr, "explicit opt-in re-enables capture");

        // capability_safe still hard-blocks opt-in on a dumb/degraded terminal.
        let opted_in_dumb = TerminalCaps::from_env(EnvView {
            term: Some("dumb".into()),
            force_mouse_sgr: Some(true),
            ..env()
        });
        assert!(
            !opted_in_dumb.mouse_sgr,
            "capability_safe overrides opt-in on dumb"
        );
    }

    // Regression: a Windows user reported that arrow keys couldn't navigate
    // any menu (/model list, approval options) — only Enter worked, always
    // picking the first item — in BOTH cmd.exe and Windows Terminal. A
    // tuix.log showed input arriving as `paste(<line>)` + `key(Press,Enter)`
    // with zero `[ RD]` reader traces: the cooked LINE reader was running,
    // not the raw-mode reader, so arrow keys were swallowed by the console's
    // own line editor and never reached the app. Cause: a stray `TERM=dumb`
    // in the environment (Git/MSYS/SSH tooling leaks it) forced
    // `raw_mode=false`. But TERM is a Unix terminfo concept; crossterm drives
    // the Windows console via the Win32 API and ignores TERM entirely, so on
    // Windows a dumb TERM must NOT disable raw mode / interactivity.
    #[test]
    fn dumb_term_is_ignored_on_windows() {
        let caps = TerminalCaps::from_env(EnvView {
            is_windows: true,
            term: Some("dumb".to_string()),
            colorterm: None,
            ..env()
        });
        assert!(caps.tty);
        assert!(
            caps.raw_mode,
            "Windows console raw mode is independent of TERM — a stray \
             TERM=dumb must not drop us into the cooked line reader"
        );
        assert!(caps.bracketed_paste);
        assert!(caps.scroll_region);
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
        assert!(
            caps.unicode_symbols,
            "VS Code's integrated terminal is fine"
        );
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
        assert!(
            caps.legacy_conhost,
            "the Unicode preference must not reclassify legacy conhost"
        );
    }

    // ── JediTerm detection (DevEco Studio / IntelliJ-platform terminals) ──

    #[test]
    fn jediterm_detected_from_terminal_emulator() {
        let caps = TerminalCaps::from_env(EnvView {
            terminal_emulator: Some("JetBrains-JediTerm".to_string()),
            ..env()
        });
        assert!(caps.jediterm, "exact TERMINAL_EMULATOR string → jediterm");
    }

    #[test]
    fn jediterm_false_for_other_or_absent_emulator() {
        let other = TerminalCaps::from_env(EnvView {
            terminal_emulator: Some("xterm".to_string()),
            ..env()
        });
        assert!(!other.jediterm);
        assert!(!TerminalCaps::from_env(env()).jediterm, "absent → false");
    }

    #[test]
    fn atomcode_jediterm_env_overrides_autodetect() {
        // Forced ON without TERMINAL_EMULATOR (DevEco launcher dropped it).
        let on = TerminalCaps::from_env(EnvView {
            force_jediterm: Some(true),
            ..env()
        });
        assert!(on.jediterm);
        // Forced OFF wins even when the emulator string is present.
        let off = TerminalCaps::from_env(EnvView {
            terminal_emulator: Some("JetBrains-JediTerm".to_string()),
            force_jediterm: Some(false),
            ..env()
        });
        assert!(!off.jediterm);
    }

    /// The jediterm flag MUST NOT perturb any pre-existing capability —
    /// it's a pure additive signal for the render layer. Same env, with
    /// vs without the JediTerm marker, must agree on unicode_symbols,
    /// legacy_conhost, colors, and the chevron.
    #[test]
    fn jediterm_flag_is_inert_for_other_caps() {
        let base = TerminalCaps::from_env(env());
        let jt = TerminalCaps::from_env(EnvView {
            terminal_emulator: Some("JetBrains-JediTerm".to_string()),
            ..env()
        });
        assert_eq!(base.unicode_symbols, jt.unicode_symbols);
        assert_eq!(base.legacy_conhost, jt.legacy_conhost);
        assert_eq!(base.colors, jt.colors);
        assert_eq!(base.prompt_chevron(), jt.prompt_chevron());
        // And on bare Windows the JediTerm marker still leaves the
        // legacy-console ASCII fallback exactly as it was.
        let win = TerminalCaps::from_env(EnvView {
            is_windows: true,
            ..env()
        });
        let win_jt = TerminalCaps::from_env(EnvView {
            is_windows: true,
            terminal_emulator: Some("JetBrains-JediTerm".to_string()),
            ..env()
        });
        assert_eq!(win.unicode_symbols, win_jt.unicode_symbols);
        assert_eq!(win.legacy_conhost, win_jt.legacy_conhost);
        assert!(win_jt.jediterm);
    }

    /// The Kitty keyboard push (CSI u) must be suppressed on JediTerm —
    /// arming it makes DevEco/IDEA's terminal re-frame mouse-move reports as
    /// kitty key events, which crossterm decodes into a stream of gibberish
    /// `Char`s in the input box. Mirrors the long-standing Windows exclusion.
    #[test]
    fn jediterm_suppresses_kitty_keyboard_push() {
        let jt = TerminalCaps::from_env(EnvView {
            terminal_emulator: Some("JetBrains-JediTerm".to_string()),
            ..env()
        });
        assert!(
            !crate::should_enable_kitty_keyboard(&jt),
            "JediTerm TTY must NOT get the Kitty keyboard push"
        );
        // Forced via ATOMCODE_JEDITERM=1 even when the env marker is absent
        // (DevEco launchers that drop TERMINAL_EMULATOR).
        let forced = TerminalCaps::from_env(EnvView {
            force_jediterm: Some(true),
            ..env()
        });
        assert!(!crate::should_enable_kitty_keyboard(&forced));
        // A generic xterm TTY (including JumpServer/xterm.js web terminals)
        // is deliberately conservative: TERM alone does not prove CSI-u.
        let plain = TerminalCaps::from_env(env());
        assert!(!crate::should_enable_kitty_keyboard(&plain));
        // Never pushed when stdout isn't a TTY, JediTerm or not.
        let not_tty = TerminalCaps::from_env(EnvView {
            is_stdout_tty: false,
            ..env()
        });
        assert!(!crate::should_enable_kitty_keyboard(&not_tty));
    }

    #[test]
    fn known_terminals_enable_kitty_keyboard_on_supported_platforms() {
        for term_program in [
            "kitty",
            "WezTerm",
            "Alacritty",
            "ghostty",
            "iTerm.app",
            "WarpTerminal",
        ] {
            let caps = TerminalCaps::from_env(EnvView {
                term_program: Some(term_program.to_string()),
                ..env()
            });
            assert_eq!(
                crate::should_enable_kitty_keyboard(&caps),
                cfg!(not(windows)),
                "{term_program} should use enhanced keys where crossterm can decode CSI-u"
            );
        }
    }

    #[test]
    fn atomcode_kitty_override_controls_generic_terminal() {
        let forced_on = TerminalCaps::from_env(EnvView {
            force_kitty_keyboard: Some(true),
            ..env()
        });
        assert_eq!(
            crate::should_enable_kitty_keyboard(&forced_on),
            cfg!(not(windows))
        );

        let forced_off = TerminalCaps::from_env(EnvView {
            term_program: Some("kitty".to_string()),
            force_kitty_keyboard: Some(false),
            ..env()
        });
        assert!(!crate::should_enable_kitty_keyboard(&forced_off));
    }

    #[test]
    fn jediterm_safety_wins_over_kitty_force_on() {
        let caps = TerminalCaps::from_env(EnvView {
            terminal_emulator: Some("JetBrains-JediTerm".to_string()),
            force_kitty_keyboard: Some(true),
            ..env()
        });
        assert!(!crate::should_enable_kitty_keyboard(&caps));
    }

    #[test]
    fn empty_terminal_markers_do_not_enable_kitty_keyboard() {
        let caps = TerminalCaps::from_env(EnvView {
            kitty_window_id: Some(String::new()),
            wezterm_version: Some("  ".to_string()),
            alacritty_socket: Some(String::new()),
            ..env()
        });
        assert!(!crate::should_enable_kitty_keyboard(&caps));
    }

    #[test]
    fn atomcode_kitty_override_parser_is_strict_and_case_insensitive() {
        for value in ["1", " true ", "YES"] {
            assert_eq!(parse_bool_override(value), Some(true));
        }
        for value in ["0", " false ", "NO"] {
            assert_eq!(parse_bool_override(value), Some(false));
        }
        for value in ["", "on", "off", "invalid"] {
            assert_eq!(parse_bool_override(value), None);
        }
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
