#!/usr/bin/env bash
# project-local: 判据数量只能升不能降。
#
# 起因（真实）：把 region.rs 改写成再导出时，连同它的 #[cfg(test)] 一起删掉了
# 5 条判据。全套测试仍然全绿——因为剩下的确实都过。丢判据不会判红，它只会让
# 判据变少，而这正是最难发现的一种退化：覆盖在缩小，仪表盘却更绿了。
#
# 数字本身没什么意义，趋势有：它只能升。真要删判据，就明确改基线并在提交里
# 说清为什么——那时它是一个决定，而不是一次意外。
set -uo pipefail
cd "$(dirname "$0")/.."
BASE=${TUI_TEST_BASELINE:-gates/tui-test-count.baseline}
CMD=${TUI_TEST_CMD:-"cargo test -q -p atomcode-tui"}

out=$(eval "$CMD" 2>&1)
if echo "$out" | grep -qE "^error|FAILED"; then
  echo "  ✗ 测试没跑通，数量无从谈起"
  echo "$out" | tail -5
  exit 1
fi
n=$(echo "$out" | grep -oE '^test result: ok\. [0-9]+' | grep -oE '[0-9]+' | paste -sd+ - | bc)
n=${n:-0}

base=""
[ -f "$BASE" ] && base=$(cat "$BASE" 2>/dev/null | tr -d ' \n')
if [ -z "$base" ]; then
  echo "$n" > "$BASE"
  echo "  ⊙ 判据数：建立基线 ${n}"
  exit 0
fi
if [ "$n" -lt "$base" ]; then
  echo "  ✗ 判据数 ${n} 少于基线 ${base} —— 有判据被删掉了，而全绿说明不了任何事"
  exit 1
fi
if [ "$n" -gt "$base" ]; then
  echo "$n" > "$BASE"
  echo "  ✓ 判据数 ${n}（基线从 ${base} 抬到 ${n}）"
else
  echo "  ✓ 判据数 ${n}（持平）"
fi
