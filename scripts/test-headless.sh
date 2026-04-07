#!/usr/bin/env bash
# AtomCode --headless mode smoke tests
#
# Usage: ./scripts/test-headless.sh
#
# Optional environment:
#   ATOMCODE_TEST_PROVIDER   Provider name to use for live (network) tests
#                            (e.g. "openai", "kimi"). When unset, only the
#                            offline argument-validation test runs and the
#                            network-dependent tests are skipped.
#
# Exit codes:
#   0  all executed tests passed
#   1  one or more assertions failed
#   2  setup error (binary missing, etc.)

set -euo pipefail

# Always run from project root
cd "$(dirname "$0")/.."

BIN="./target/debug/atomcode"
if [ ! -x "$BIN" ]; then
    echo "fatal: $BIN not found. Run: cargo build" >&2
    exit 2
fi

# Pick a timeout binary (macOS users typically have gtimeout via coreutils)
if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_BIN="timeout"
elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN="gtimeout"
else
    echo "fatal: neither 'timeout' nor 'gtimeout' is on PATH (brew install coreutils)" >&2
    exit 2
fi

TMPDIR_T=$(mktemp -d /tmp/atomcode-headless-XXXXXX)
trap 'rm -rf "$TMPDIR_T"' EXIT INT TERM

PASSED=0
FAILED=0
SKIPPED=0
FAIL_NAMES=()

pass() { PASSED=$((PASSED + 1)); echo "  [PASS] $1"; }
fail() { FAILED=$((FAILED + 1)); FAIL_NAMES+=("$1"); echo "  [FAIL] $1"; }
skip() { SKIPPED=$((SKIPPED + 1)); echo "  [SKIP] $1"; }

# run_atom <secs> <stdout_file> <stderr_file> <args...>
# Always closes stdin (</dev/null) so a hang on stdin will be caught by the
# timeout. Returns the timeout/exit code via $? — caller must use `|| rc=$?`.
run_atom() {
    local secs="$1"; shift
    local out="$1"; shift
    local err="$1"; shift
    "$TIMEOUT_BIN" "$secs" "$BIN" "$@" </dev/null >"$out" 2>"$err"
}

echo "=== AtomCode Headless Smoke Tests ==="
echo "  Binary  : $BIN"
echo "  Timeout : $TIMEOUT_BIN"
echo "  TmpDir  : $TMPDIR_T"
echo "  Provider: ${ATOMCODE_TEST_PROVIDER:-<unset — network tests will be skipped>}"
echo ""

###############################################################################
# T5 (offline): missing --prompt → non-zero exit, stderr explains why
#               This is the only test that runs without a configured provider.
###############################################################################
T5="T5: --headless without --prompt errors with helpful message"
echo "[T5] Running: --headless (no -p)"
out="$TMPDIR_T/t5.out"; err="$TMPDIR_T/t5.err"; rc=0
run_atom 5 "$out" "$err" --headless || rc=$?

if [ "$rc" -eq 0 ]; then
    fail "$T5 — expected non-zero exit, got 0"
elif [ "$rc" -eq 124 ]; then
    fail "$T5 — process hung (timed out)"
elif ! grep -qi "prompt is required" "$err"; then
    fail "$T5 — stderr missing 'prompt is required' (got: $(head -2 "$err" | tr '\n' ' '))"
else
    pass "$T5 (exit=$rc)"
fi
echo ""

###############################################################################
# Network-gated tests — require a configured provider.
###############################################################################
if [ -z "${ATOMCODE_TEST_PROVIDER:-}" ]; then
    skip "T1: --headless -p emits stdout                  (needs ATOMCODE_TEST_PROVIDER)"
    skip "T2: --headless stdout has no decoration markers (needs ATOMCODE_TEST_PROVIDER)"
    skip "T3: --headless stderr has log/diagnostic output (needs ATOMCODE_TEST_PROVIDER)"
    skip "T4: --headless does not block on stdin          (needs ATOMCODE_TEST_PROVIDER)"
else
    PROV="$ATOMCODE_TEST_PROVIDER"

    ###########################################################################
    # T1: --headless -p succeeds and emits non-empty stdout
    ###########################################################################
    T1="T1: --headless -p exits 0 with non-empty stdout"
    echo "[T1] Running: --headless -p (provider=$PROV)"
    out="$TMPDIR_T/t1.out"; err="$TMPDIR_T/t1.err"; rc=0
    run_atom 30 "$out" "$err" \
        --provider "$PROV" --headless -p "Reply with the single word: ok" || rc=$?
    if [ "$rc" -eq 124 ]; then
        fail "$T1 — process hung"
    elif [ "$rc" -ne 0 ]; then
        fail "$T1 — exit=$rc; stderr tail: $(tail -3 "$err" | tr '\n' ' ')"
    elif [ ! -s "$out" ]; then
        fail "$T1 — stdout empty"
    else
        pass "$T1 ($(wc -c <"$out" | tr -d ' ') bytes on stdout)"
    fi
    echo ""

    ###########################################################################
    # T2: --headless stdout has only LLM text — no [Tool: ...] / [Done ...] etc.
    ###########################################################################
    T2="T2: --headless stdout has no decoration markers"
    echo "[T2] Running: --headless -p"
    out="$TMPDIR_T/t2.out"; err="$TMPDIR_T/t2.err"; rc=0
    run_atom 30 "$out" "$err" \
        --provider "$PROV" --headless -p "Reply with the single word: ok" || rc=$?
    if [ "$rc" -eq 124 ]; then
        fail "$T2 — process hung"
    elif [ "$rc" -ne 0 ]; then
        fail "$T2 — exit=$rc; stderr tail: $(tail -3 "$err" | tr '\n' ' ')"
    elif grep -E '^\[(Tool|Done|Approval|Error|Tokens|Thinking|Executing|Cancelled|Working)' "$out" >/dev/null; then
        offender=$(grep -m1 -E '^\[' "$out")
        fail "$T2 — stdout contains decoration: $offender"
    else
        pass "$T2"
    fi
    echo ""

    ###########################################################################
    # T3: stderr has at least one diagnostic line during a tool-using turn
    ###########################################################################
    T3="T3: --headless stderr has at least one log line"
    echo "[T3] Running: --headless -p (tool-using prompt)"
    out="$TMPDIR_T/t3.out"; err="$TMPDIR_T/t3.err"; rc=0
    run_atom 60 "$out" "$err" \
        --provider "$PROV" --headless \
        -p "List the files in the current directory then reply DONE" || rc=$?
    if [ "$rc" -eq 124 ]; then
        fail "$T3 — process hung"
    elif [ "$rc" -ne 0 ]; then
        fail "$T3 — exit=$rc; stderr tail: $(tail -3 "$err" | tr '\n' ' ')"
    elif [ ! -s "$err" ]; then
        fail "$T3 — stderr empty (expected at least one diagnostic line)"
    else
        pass "$T3 ($(wc -l <"$err" | tr -d ' ') lines on stderr)"
    fi
    echo ""

    ###########################################################################
    # T4: --headless with stdin closed must not block. The </dev/null in
    #     run_atom guarantees stdin is closed; if approval logic still tries
    #     to read it, the timeout will fire and rc will be 124.
    ###########################################################################
    T4="T4: --headless does not block on stdin (stdin closed)"
    echo "[T4] Running: --headless -p </dev/null"
    out="$TMPDIR_T/t4.out"; err="$TMPDIR_T/t4.err"; rc=0
    run_atom 30 "$out" "$err" \
        --provider "$PROV" --headless -p "Reply with the single word: ok" || rc=$?
    if [ "$rc" -eq 124 ]; then
        fail "$T4 — process hung (likely blocked on stdin)"
    elif [ "$rc" -ne 0 ]; then
        fail "$T4 — exit=$rc; stderr tail: $(tail -3 "$err" | tr '\n' ' ')"
    else
        pass "$T4"
    fi
    echo ""
fi

###############################################################################
# Summary
###############################################################################
echo "=== Summary ==="
echo "  Passed : $PASSED"
echo "  Failed : $FAILED"
echo "  Skipped: $SKIPPED"
echo ""

if [ "$FAILED" -gt 0 ]; then
    echo "Failed tests:"
    for name in "${FAIL_NAMES[@]}"; do
        echo "  - $name"
    done
    exit 1
fi

echo "ALL HEADLESS SMOKE TESTS PASSED"
exit 0
