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
# 用 nextest 而不是 cargo test：`cargo test` 一个 test binary 跑完才跑下一个，
# 在这个仓库里代价很大（见 AGENTS.md「测试与构建命令」）。计数口径已核对过：
# 2026-09-13 两边都是 397（cargo test 的 365+0+32+0 == nextest 的 397 tests run）。
# nextest 不跑 doctest，而 atomcode-tui 的 2 个 ``` 块都是 ignore/text，可跑 doctest
# 为 0 —— 所以换 runner 不会让这个棘轮掉一个数。
CMD=${TUI_TEST_CMD:-"cargo nextest run -p atomcode-tui"}

if ! cargo nextest --version >/dev/null 2>&1; then
  echo "  ✗ 需要 cargo-nextest（见 https://nexte.st 安装）"
  exit 1
fi

# 去掉颜色码：环境里设了 FORCE_COLOR 时 nextest 即使不接终端也上色，
# 汇总行成了 `516\e[0m tests run`，数字就解析不出来。
out=$(eval "$CMD" 2>&1 | sed $'s/\x1b\[[0-9;]*m//g')
# nextest 的失败面：编译期 `error...`、单测 `FAIL [`、汇总行 `N failed`。
if echo "$out" | grep -qE "^error|FAIL \[|[1-9][0-9]* failed"; then
  echo "  ✗ 测试没跑通，数量无从谈起"
  echo "$out" | tail -5
  exit 1
fi
n=$(echo "$out" | grep -oE '[0-9]+ tests run' | grep -oE '^[0-9]+' | head -1)
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
