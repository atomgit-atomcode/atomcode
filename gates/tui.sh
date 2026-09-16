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

# 每一步都包一层硬超时，超时按失败处理 —— 不是跳过，也不是「慢」。
#
# 起因（真实）：把 El::Row 改成支持多行子元素时，我让每个子元素先「量」一次
# 再「画」一次 —— 每层工作量翻倍，200 层嵌套就是 2^200。测试没有失败，它挂住了，
# 而现场表现是「构建怎么这么久」。挂死是最坏的失败模式，因为它报的是慢不是错，
# 于是人会去优化构建、换机器，唯独不会怀疑那个测试。
#
# macOS 没有 GNU timeout，所以自己来：后台跑、轮询、到点杀掉。
TIMEOUT=${TUI_GATE_TIMEOUT:-300}
run_with_timeout() {           # run_with_timeout <secs> <cmd...>
  local secs="$1"; shift
  "$@" >/tmp/tui-gate.$$ 2>&1 &
  local pid=$!
  local waited=0
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$waited" -ge "$secs" ]; then
      kill -9 "$pid" 2>/dev/null
      wait "$pid" 2>/dev/null
      echo "（超时 ${secs}s —— 按失败处理，不是按慢处理）" >> /tmp/tui-gate.$$
      return 124
    fi
    sleep 1
    waited=$((waited + 1))
  done
  wait "$pid"
}

step() {                       # step <name> <cmd...>
  local name="$1"; shift
  printf '\033[2m→ %s\033[0m\n' "$name"
  if run_with_timeout "$TIMEOUT" "$@"; then
    printf '\033[32m  ok\033[0m   %s\n' "$name"
  else
    printf '\033[31m  FAIL\033[0m %s\n' "$name"
    sed -n '1,40p' /tmp/tui-gate.$$ | sed 's/^/       /'
    fail=1
  fi
  rm -f /tmp/tui-gate.$$
}

[ "$BLESS" = 1 ] && export TUI_BLESS=1

# 测试用 nextest 跑，不用 `cargo test`：后者一个 test binary 跑完才跑下一个，
# 而每步还包着 TUI_GATE_TIMEOUT（默认 300s）的硬超时 —— 串行跑法会把这道门
# 推向超时，而超时在这里按失败处理。判据口径不变（2026-09-13 两边都是 397）。
# 细节见 AGENTS.md「测试与构建命令」。
if ! cargo nextest --version >/dev/null 2>&1; then
  printf '\033[31m  FAIL\033[0m %s\n' "缺 cargo-nextest（见 https://nexte.st 安装）"
  exit 1
fi

step "值类型与几何（单元）"        cargo nextest run -p atomcode-tui --lib
[ "$FAST" = 1 ] || step "集成与端到端"  cargo nextest run -p atomcode-tui --tests
[ "$FAST" = 1 ] || step "宿主 harness 未被弄坏" cargo nextest run -p atomcode-harness
[ "$FAST" = 1 ] || step "启动器能构建"  cargo build -q -p atomcode --bin atomcode
[ "$FAST" = 1 ] || step "启动器能审计（无 tty）" ./target/debug/atomcode --tui --audit
[ "$FAST" = 1 ] || step "屏幕与产品会话互通" cargo nextest run -p atomcode --test tui_front
[ "$FAST" = 1 ] || step "判据只能增不能减"     gates/tui-test-count.sh
step "分层：OS 差异不得漏出屏蔽层" gates/tui-layers.sh
step "分层闸门自身会判红"          gates/tui-layers.spec.sh
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
