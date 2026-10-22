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

Mouse policy is unchanged: the current release branch already gives SSH native
selection/paste at startup. v5.2.2 and v5.2.3 both captured mouse input; v5.2.3
changed clipboard handling and notices. Treat the reported copy/paste regression
separately from missing decorative glyphs.

Tests cover PuTTY and anonymous SSH, known modern clients, local Unix, explicit
overrides and Chinese preservation. Windows PuTTY acceptance remains manual.

Validation: all 1,826 TUI tests passed; the full compile gate, cargo fmt check,
diff check and Cargo.lock checks passed.
