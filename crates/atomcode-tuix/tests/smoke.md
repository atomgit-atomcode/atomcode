<!-- crates/atomcode-tuix/tests/smoke.md -->
# TUIx Acceptance Smoke Checklist

Manual acceptance after each build. All items must pass before considering the rebuild complete.

## Build

- [ ] `cargo build --release 2>&1 | tail -3` → Finished, 0 errors
- [ ] `cargo test -p atomcode-tuix 2>&1 | tail -5` → all 79 pass
- [ ] `cargo clippy -p atomcode-tuix --all-targets 2>&1 | tail -10` → no warnings

## TTY happy path

- [ ] `./target/release/atomcode --tuix` shows welcome (logo + model + dir + tips)
- [ ] `❯ ` prompt appears on its own line
- [ ] Type `hello` + Enter → "❯ hello" enters scrollback, assistant reply begins streaming
- [ ] Spinner animates (10 frames cycling) during streaming
- [ ] TextDelta clears spinner, prints text with `  │ ` bar prefix
- [ ] `▸ tool(detail)` tool line renders
- [ ] `✓ summary` / `✗ summary` tool result coloured correctly
- [ ] TurnComplete → `❯ ` returns on fresh line

## CJK (MANDATORY — prior impl panicked here)

- [ ] Input `你好，帮我看看这个文件` + Enter → no panic, cursor column correct
- [ ] Assistant output in Chinese → no width misalignment

## Injection defence

- [ ] Paste `\x1b[2J\x1b[H` into input → screen does NOT clear
- [ ] Tool output containing `\x1b]0;pwned\x07` → terminal title unchanged
- [ ] Bash tool output `\x1b[31mred\x1b[0m` → either displayed as literal text or stripped; does not affect subsequent rendering

## Ctrl+C

- [ ] Ctrl+C during streaming → sends Cancel, renderer shows "(cancelled)"
- [ ] Ctrl+C on empty idle buffer → clean exit
- [ ] Ctrl+C on non-empty idle buffer → clears buffer only; does not exit

## Ctrl+Z / fg

- [ ] Ctrl+Z during streaming → returns to shell with sane terminal (not raw mode)
- [ ] `fg` → spinner resumes, interaction resumes normally

## Slash commands

- [ ] `/help` → lists all commands, auto-generated from registry
- [ ] `/cd /tmp` → working dir changes, agent notified
- [ ] `/clear` → screen cleared, welcome re-rendered
- [ ] `/status` → shows model / dir / config / tokens
- [ ] `/quit` → clean exit

## Tab completion

- [ ] `/h<Tab>` → completes to `/help `
- [ ] `/z<Tab>` → no completion (no matches)

## History

- [ ] After exit + re-enter, `↑` recalls prior messages
- [ ] `~/.atomcode/history` file exists and is readable
- [ ] Consecutive duplicate messages are collapsed

## Pipe / plain mode

- [ ] `echo "say hi" | ./target/release/atomcode --tuix` → no ANSI bytes in output
- [ ] `./target/release/atomcode --tuix > out.txt` → `out.txt` is pure text
- [ ] `NO_COLOR=1 ./target/release/atomcode --tuix` → no colour; spinner retained on TTY
- [ ] `TERM=dumb ./target/release/atomcode --tuix` → no colour, no spinner

## Approval flow

- [ ] Trigger a tool that requires approval → prompt appears inline
- [ ] Y → approves; agent proceeds
- [ ] N → denies; agent receives rejection
- [ ] A → approves + remembers for session

## Resize

- [ ] Resize terminal during streaming → prior output stays intact, new output uses new width
- [ ] Resize during idle prompt → input redraws on next keypress without corruption
