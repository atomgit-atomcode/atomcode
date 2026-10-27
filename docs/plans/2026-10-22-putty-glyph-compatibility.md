# Automatic SSH glyph fallback

Baseline: release/v5.2.5 at 3e4e2559f99fc4b7cda50a56c965874fc3fb8aa3.

PuTTY on Windows renders Chinese but lacks some decorative glyphs. The user
verified ASCII and native mouse selection independently on released v5.2.4.

Reuse `basic_glyphs_for`, the existing console-safe rewrite, ASCII spinner and
mascot gates. Explicit PuTTY and anonymous SSH clients use basic decorations
regardless of server OS. Known modern client identities retain their previous
rendering; local Unix behavior is unchanged. Preserve existing explicit
ATOMCODE_ASCII / ATOMCODE_UNICODE overrides. Do not add a configuration setting,
command, dependency or another renderer.

Tradeoff: an SSH client that identifies only as xterm cannot declare font
coverage, so its decorations become conservative even if its font is capable.
Chinese, colors and basic box drawing remain. No missing-glyph feedback exists
for the server to detect after rendering; this is a conservative startup policy,
not a promise that arbitrary user/model Unicode can always render.

Mouse behavior is separate from this glyph fallback. SSH/PuTTY now defaults to
terminal-owned selection and paste, prioritizing the classic PuTTY workflow.
Ctrl+G can take application mouse control; ATOMCODE_MOUSE=1 opts in at startup.
A startup reminder is part of the welcome foot below cwd/model, alongside the
existing HarmonyOS unsupported-mouse reminder path. It names native copy/paste
and Ctrl+T tool expansion, without advertising Ctrl+G in the startup text.

Tests cover PuTTY and anonymous SSH, known modern clients, local Unix, explicit
overrides and Chinese preservation. Windows PuTTY acceptance remains manual.

Validation: all 1,830 TUI tests passed, including startup placement and mouse
policy checks. The full compile gate, formatting and diff checks passed.
