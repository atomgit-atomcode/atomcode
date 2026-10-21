# First frame startup timing

Baseline: main at 5185ad3d9; worktree contains only unrelated untracked files.

Goal: measure the first successful frame flush, rather than treating `tui_enter` as the first visible frame. Record `total_ms` from the native Rust main entry and `ui_ms` from entering the CLI screen runner. The native-entry clock excludes shell/npm wrappers, the OS loader and any previous executable before re-exec.

Design: pass a monotonic Instant through the existing launcher functions. Keep existing startup stages but align their total clock with native entry. Before running the screen, arm the terminal surface with the process/UI clocks. The terminal owns the write/flush seam and consumes these clocks only after the first nonempty frame patch is successfully written and flushed. Headless/custom surfaces default to no timing. No new runtime owner, session facts, prompt content, config settings or user-visible messages.

Implementation:
1. Add a default no-op startup timing method to Surface and a terminal-local optional clock pair. Factor the existing patch write/flush into a helper returning the first elapsed sample. Log `stage=first_frame`, `total_ms`, `ui_ms` at INFO once after success; failed/empty writes do not report.
2. Capture native entry before helper/launch setup, thread the Instant through main/async_main/run and the existing CLI TUI runner. Keep fast exits and startup behavior intact.
3. Test empty patch, write error, flush error, retry, single-sample behavior and an unarmed surface. Run TUI/CLI crate tests sequentially, formatting and compile gate. Logging remains metadata-only; successful flush means bytes submitted to the terminal, not proof of physical display.

Validation: TUI and CLI suites and all four compile-gate phases passed. First-frame timing is separate from history catch-up: a flushed initial frame does not mean all historical facts have been replayed. An isolated PTY attempt did not yield a captured timing event; no real-terminal or Windows timing measurement is claimed.
