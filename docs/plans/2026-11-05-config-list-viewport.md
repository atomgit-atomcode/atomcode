# Config list viewport

Baseline: release/v5.2.6, a8832b025.
The Config layout used fit/truncate while arrow keys moved a cursor outside
that prefix. Keep the existing filter and selection owner. Window the setting
rows around the selected index once decoration no longer fits, with a bounded
above/below indicator. Render and mouse hit testing share layout. No new runtime
state or configuration persistence is required.

Wheel events inside Config move its selection (and clear armed reset), so the
same viewport follows keyboard, page keys and wheel. Editing and shortcut
capture consume the wheel without moving the target. Off-panel wheel routing
remains with the conversation. Extremely small panels retain the existing
input-priority fallback.

Validate a 60-setting list at heights 6/10/24/50, both navigation directions,
filtered index mapping and shared click geometry; run TUI tests and compile/fmt
gates. No native terminal manual run is claimed.

Validation: TUI 1916/1916 passed; the final keyboard/page and geometry regression
and wheel routing tests passed. Compile gate, cargo fmt check and git diff check
passed. Cargo.lock and latest.json remain unchanged.
