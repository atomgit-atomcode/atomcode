#!/usr/bin/env bash
# One command, one exit code: is the new TUI sound?
#
# Everything here runs with no tty, no network, no model and no human — that is
# the point. A gate that needs a person to look at it will stop an agent that is
# following the process correctly.
#
#   gates/tui.sh            run everything
#   gates/tui.sh --fast     skip the slow layers (for an inner loop)
#   gates/tui.sh --bless    regenerate golden frames, then verify
set -uo pipefail
cd "$(dirname "$0")/.."

FAST=0; BLESS=0
for a in "$@"; do
  case "$a" in
    --fast)  FAST=1 ;;
    --bless) BLESS=1 ;;
    *) echo "unknown flag: $a" >&2; exit 2 ;;
  esac
done

fail=0
step() {                       # step <name> <cmd...>
  local name="$1"; shift
  printf '\033[2m→ %s\033[0m\n' "$name"
  if "$@" >/tmp/tui-gate.$$ 2>&1; then
    printf '\033[32m  ok\033[0m   %s\n' "$name"
  else
    printf '\033[31m  FAIL\033[0m %s\n' "$name"
    sed -n '1,40p' /tmp/tui-gate.$$ | sed 's/^/       /'
    fail=1
  fi
  rm -f /tmp/tui-gate.$$
}

[ "$BLESS" = 1 ] && export TUI_BLESS=1

step "值类型与几何（单元）"        cargo test -q -p atomcode-tui --lib
[ "$FAST" = 1 ] || step "集成与端到端"  cargo test -q -p atomcode-tui --tests
[ "$FAST" = 1 ] || step "宿主 harness 未被弄坏" cargo test -q -p atomcode-harness
[ "$FAST" = 1 ] || step "启动器能构建"  cargo build -q -p atomcode-tui
[ "$FAST" = 1 ] || step "启动器能审计（无 tty）" ./target/debug/atui --offline --audit
step "阴性对照：坏东西必须判红"    gates/tui-negative.sh
step "格式"                        cargo fmt -p atomcode-tui -- --check
# `--no-deps` is load-bearing, not tidiness: without it `-D warnings` promotes
# every pre-existing warning in the dependency tree, the gate is red on its
# first run, and a gate that is red on its first run gets turned off.
step "lint"                        cargo clippy -q -p atomcode-tui --all-targets --no-deps -- -D warnings

if [ "$fail" = 0 ]; then
  printf '\033[32m\nTUI gate: 通过\033[0m\n'
else
  printf '\033[31m\nTUI gate: 失败\033[0m\n'
fi
exit $fail
