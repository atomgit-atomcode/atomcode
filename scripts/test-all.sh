#!/bin/bash
# AtomCode 全量测试 — 每次改动前必须通过
# 用法: ./scripts/test-all.sh
# 输出: 测试报告到 stdout + test-report.md

set -e
trap 'jobs -p | xargs kill 2>/dev/null' EXIT INT TERM

REPORT="test-report.md"

echo "# AtomCode Test Report" > $REPORT
echo "**Date:** $(date '+%Y-%m-%d %H:%M:%S')" >> $REPORT
echo "**Build:** $(git rev-parse --short HEAD)" >> $REPORT
echo "**Branch:** $(git branch --show-current)" >> $REPORT
echo "" >> $REPORT

# Run all tests in one cargo invocation (compile + run)
echo "=== AtomCode Full Test Suite ==="
echo ""
echo -n "Compiling & running all tests... "
output=$(cargo test -p atomcode-core 2>&1) || true

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

# Summary
echo "=== Summary ==="
echo "  Passed: $PASSED"
echo "  Failed: $FAILED"

echo "---" >> $REPORT
echo "## Summary" >> $REPORT
echo "- **Passed:** $PASSED" >> $REPORT
echo "- **Failed:** $FAILED" >> $REPORT

if [ "$FAILED" -gt 0 ]; then
    echo ""
    echo "$output" | grep "FAILED" | head -10
    echo "- **Status: FAILED**" >> $REPORT
    echo ""
    exit 1
else
    echo "- **Status: ALL PASSED**" >> $REPORT
    echo ""
    echo "ALL TESTS PASSED"
    exit 0
fi
