#!/bin/bash
# AtomCode 全量测试 — 每次改动前必须通过
# 用法: ./scripts/test-all.sh
# 输出: 测试报告到 stdout + test-report.md

set -e
# EXIT trap 会在正常退出和信号终止时清理后台 job。清理命令的结果
# 不会覆盖脚本原本的退出状态；避免共用 INT/TERM trap 将中断误报为 0。
trap 'jobs -p | xargs kill 2>/dev/null || true' EXIT

REPORT="test-report.md"

echo "# AtomCode Test Report" > $REPORT
echo "**Date:** $(date '+%Y-%m-%d %H:%M:%S')" >> $REPORT
echo "**Build:** $(git rev-parse --short HEAD)" >> $REPORT
echo "**Branch:** $(git branch --show-current)" >> $REPORT
echo "**Scope:** All workspace crates (cargo test --workspace)" >> $REPORT
echo "" >> $REPORT

# Run all tests in one cargo invocation (compile + run)
# 保留 cargo 退出码以区分编译失败、测试失败与全部成功，避免假阳性。
# 注意：在 `set -e` 下，`output=$(cargo test ...)` 若 cargo 返回非 0 会立即
# 终止脚本，导致 cargo_status 永远拿不到。这里临时关闭 errexit 来捕获退出码。
echo "=== AtomCode Full Test Suite ==="
echo ""
echo -n "Compiling & running all tests... "
set +e
output=$(cargo test --workspace 2>&1)
cargo_status=$?
set -e

# Extract compile warnings from the combined output. Rustc diagnostics usually
# start with `warning:`; some tools emit `warning[lint_name]:`.
warnings=$(echo "$output" | grep -cE "^warning(:|\\[)" || true)
echo "done (${warnings} warnings)"

if [ "$warnings" -gt 0 ]; then
    echo "## Compile Warnings ($warnings)" >> $REPORT
    echo '```' >> $REPORT
    echo "$output" | grep -E "^warning(:|\\[)" >> $REPORT
    echo '```' >> $REPORT
else
    echo "## Compile Check (0 warnings)" >> $REPORT
fi
echo "" >> $REPORT

# Parse results per test binary from the combined output
PASSED=0
FAILED=0
ERRORS=""

# Extract per-suite results: "running N tests" blocks separated by binary names
# Each "test result:" line has the summary
while IFS= read -r line; do
    if [[ "$line" =~ "test result:" ]]; then
        p=$(echo "$line" | grep -oP '\d+ passed' | grep -oP '\d+' || echo 0)
        f=$(echo "$line" | grep -oP '\d+ failed' | grep -oP '\d+' || echo 0)
        PASSED=$((PASSED + p))
        FAILED=$((FAILED + f))
    fi
done <<< "$output"

# Fallback: count individual test lines if no "test result:" found
if [ "$PASSED" -eq 0 ] && [ "$FAILED" -eq 0 ]; then
    PASSED=$(echo "$output" | grep -c "... ok" || true)
    FAILED=$(echo "$output" | grep -c "FAILED" || true)
fi

echo "done"
echo ""

# Report failed tests
if [ "$FAILED" -gt 0 ]; then
    echo "## Failed Tests" >> $REPORT
    echo '```' >> $REPORT
    echo "$output" | grep -E "FAILED|panicked|assertion" | head -20 >> $REPORT
    echo '```' >> $REPORT
    echo "" >> $REPORT
fi

# Summary — 以 cargo 退出码为权威，区分编译失败、测试失败与全部成功。
echo "=== Summary ==="
echo "  Passed: $PASSED"
echo "  Failed: $FAILED"
echo "  Cargo exit: $cargo_status"

echo "---" >> $REPORT
echo "## Summary" >> $REPORT
echo "- **Passed:** $PASSED" >> $REPORT
echo "- **Failed:** $FAILED" >> $REPORT
echo "- **Cargo exit:** $cargo_status" >> $REPORT

# 编译失败：cargo 在编译阶段返回非 0，但输出中通常没有 "test result:" 行，
# 旧脚本会因 PASSED=0/FAILED=0 误报 ALL TESTS PASSED，这里直接拦截。
if [ "$cargo_status" -ne 0 ] && [ "$FAILED" -eq 0 ]; then
    echo "## Build/Run Failure (cargo exit $cargo_status)" >> $REPORT
    echo '```' >> $REPORT
    echo "$output" | grep -E "error\[|error:|panicked|FAILED" | head -20 >> $REPORT
    echo '```' >> $REPORT
    echo "" >> $REPORT
    echo "- **Status: FAILED (build/run error, no tests counted)**" >> $REPORT
    echo ""
    echo "BUILD/RUN FAILED — no test results parsed"
    exit 1
fi

if [ "$FAILED" -gt 0 ] || [ "$cargo_status" -ne 0 ]; then
    echo ""
    echo "$output" | grep -E "FAILED|panicked|error\[" | head -10
    echo "- **Status: FAILED**" >> $REPORT
    echo ""
    exit 1
else
    echo "- **Status: ALL PASSED**" >> $REPORT
    echo ""
    echo "ALL TESTS PASSED"
    exit 0
fi
