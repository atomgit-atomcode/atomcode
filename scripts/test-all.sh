#!/bin/bash
# AtomCode 全量测试 — 每次改动前必须通过
# 用法: ./scripts/test-all.sh
# 输出: 测试报告到 stdout + test-report.md

set -e

REPORT="test-report.md"
PASSED=0
FAILED=0
SKIPPED=0
ERRORS=""

echo "# AtomCode Test Report" > $REPORT
echo "**Date:** $(date '+%Y-%m-%d %H:%M:%S')" >> $REPORT
echo "**Build:** $(git rev-parse --short HEAD)" >> $REPORT
echo "**Branch:** $(git branch --show-current)" >> $REPORT
echo "" >> $REPORT

run_test_suite() {
    local name="$1"
    local cmd="$2"
    echo -n "Running $name... "

    local output
    local exit_code
    output=$(eval "$cmd" 2>&1) || true
    exit_code=$?

    local passed=$(echo "$output" | grep -c "... ok" || true)
    local failed=$(echo "$output" | grep -c "FAILED" || true)
    local total=$((passed + failed))

    PASSED=$((PASSED + passed))
    FAILED=$((FAILED + failed))

    if [ "$failed" -gt 0 ]; then
        echo "FAILED ($passed/$total passed)"
        echo "## ❌ $name ($passed/$total)" >> $REPORT
        echo '```' >> $REPORT
        echo "$output" | grep -E "FAILED|panicked|assertion" | head -10 >> $REPORT
        echo '```' >> $REPORT
        ERRORS="$ERRORS\n  - $name: $failed failed"
    else
        echo "OK ($passed passed)"
        echo "## ✅ $name ($passed passed)" >> $REPORT
    fi
    echo "" >> $REPORT
}

echo "=== AtomCode Full Test Suite ==="
echo ""

# 1. Core unit tests (agent, conversation, tool, turn, etc.)
run_test_suite "Core Unit Tests" \
    "cargo test -p atomcode-core --lib 2>&1"

# 2. Phase 2 integration tests
run_test_suite "Phase 2 Tests" \
    "cargo test -p atomcode-core --test phase2_test 2>&1"

# 3. Full restart tests
run_test_suite "Full Restart Tests" \
    "cargo test -p atomcode-core --test full_restart_test 2>&1"

# 4. Grep integration tests
run_test_suite "Grep Tests" \
    "cargo test -p atomcode-core --test grep_test 2>&1"

# 5. Grep raw tests
run_test_suite "Grep Raw Tests" \
    "cargo test -p atomcode-core --test grep_raw_test 2>&1"

# 6. Compile check (no warnings)
echo -n "Running Compile Check... "
compile_output=$(cargo check 2>&1)
warnings=$(echo "$compile_output" | grep -c "warning\[" || true)
if [ "$warnings" -gt 0 ]; then
    echo "WARNINGS ($warnings)"
    echo "## ⚠️ Compile Warnings ($warnings)" >> $REPORT
    echo '```' >> $REPORT
    echo "$compile_output" | grep "warning\[" >> $REPORT
    echo '```' >> $REPORT
else
    echo "OK (0 warnings)"
    echo "## ✅ Compile Check (0 warnings)" >> $REPORT
fi
echo "" >> $REPORT

# Summary
echo ""
echo "=== Summary ==="
echo "  Passed: $PASSED"
echo "  Failed: $FAILED"

echo "---" >> $REPORT
echo "## Summary" >> $REPORT
echo "- **Passed:** $PASSED" >> $REPORT
echo "- **Failed:** $FAILED" >> $REPORT

if [ "$FAILED" -gt 0 ]; then
    echo ""
    echo "  FAILURES:$ERRORS"
    echo "- **Status: FAILED**" >> $REPORT
    echo ""
    echo "❌ TESTS FAILED — do not commit"
    exit 1
else
    echo "- **Status: ALL PASSED ✅**" >> $REPORT
    echo ""
    echo "✅ ALL TESTS PASSED"
    exit 0
fi
