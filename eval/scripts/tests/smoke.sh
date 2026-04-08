#!/usr/bin/env bash
# End-to-end smoke for the eval runner using a fake atomcode binary.
# Run: ./eval/scripts/tests/smoke.sh
# Exits 0 on full pass, non-zero on any failure.
#
# This is the V1 acceptance gate per the plan's §13. It exercises every
# code path in the runner (form A pass, form A fail, form B pass, invalid)
# without burning real LLM tokens.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EVAL_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

FAKE="$SCRIPT_DIR/fake-atomcode.sh"
TMPCASES=$(mktemp -d /tmp/eval-smoke-cases-XXXXXX)
TMPRUNS=$(mktemp -d /tmp/eval-smoke-runs-XXXXXX)
trap 'rm -rf "$TMPCASES" "$TMPRUNS"' EXIT

PASSED=0
FAILED=0
ok()  { PASSED=$((PASSED + 1)); echo "  [PASS] $1"; }
bad() { FAILED=$((FAILED + 1)); echo "  [FAIL] $1"; }

# --- create 4 cases: form A pass, form A fail, form B pass, invalid ---
cat > "$TMPCASES/001-pass.md" <<'EOF'
+++
id = "001-pass"
provider = "fake"
+++
just say hi
EOF

cat > "$TMPCASES/002-fail.md" <<'EOF'
+++
id = "002-fail"
provider = "fake"
+++
fail-please now
EOF

mkdir -p "$TMPCASES/010-form-b/seed"
echo "seed file" > "$TMPCASES/010-form-b/seed/data.txt"
cat > "$TMPCASES/010-form-b/case.md" <<'EOF'
+++
id = "010-form-b"
provider = "fake"
+++
read data.txt please
EOF

cat > "$TMPCASES/999-invalid.md" <<'EOF'
+++
id = "999-invalid"
not valid TOML syntax >>>>
+++
malformed frontmatter — should be invalid
EOF

# --- run the eval ---
echo "=== Running runner against fake binary ==="
"$EVAL_DIR/run.sh" --bin "$FAKE" --cases-dir "$TMPCASES" --runs-dir "$TMPRUNS" --concurrency 2

# Find the run dir (only one expected)
RUN_DIR=$(ls -d "$TMPRUNS"/*/ 2>/dev/null | head -1 | sed 's:/$::')
if [ -z "$RUN_DIR" ]; then
    bad "no run dir was created"
    exit 1
fi

echo ""
echo "=== Assertions ==="

# 1. summary.json exists and has the right shape
[ -f "$RUN_DIR/summary.json" ] && ok "summary.json exists" || bad "summary.json missing"

# 2. index.html exists
[ -f "$RUN_DIR/index.html" ] && ok "index.html exists" || bad "index.html missing"

# 3. each of the 4 cases has its own dir
for id in 001-pass 002-fail 010-form-b 999-invalid; do
    [ -d "$RUN_DIR/$id" ] && ok "case dir exists: $id" || bad "case dir missing: $id"
done

# 4. valid cases have meta.json + stdout.txt + stderr.txt + cwd/ + home/
for id in 001-pass 002-fail 010-form-b; do
    for f in meta.json stdout.txt stderr.txt; do
        [ -f "$RUN_DIR/$id/$f" ] && ok "$id/$f exists" || bad "$id/$f missing"
    done
    [ -d "$RUN_DIR/$id/cwd" ] && ok "$id/cwd/ exists" || bad "$id/cwd/ missing"
    [ -d "$RUN_DIR/$id/home" ] && ok "$id/home/ exists" || bad "$id/home/ missing"
done

# 5. status values
status_of() {
    python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('status'))" "$1"
}
[ "$(status_of "$RUN_DIR/001-pass/meta.json")" = "pass" ] && ok "001-pass status=pass" || bad "001-pass status wrong"
[ "$(status_of "$RUN_DIR/002-fail/meta.json")" = "fail" ] && ok "002-fail status=fail" || bad "002-fail status wrong"
[ "$(status_of "$RUN_DIR/010-form-b/meta.json")" = "pass" ] && ok "010-form-b status=pass" || bad "010-form-b status wrong"
[ "$(status_of "$RUN_DIR/999-invalid/meta.json")" = "invalid" ] && ok "999-invalid status=invalid" || bad "999-invalid status wrong"

# 6. form B seed copied into cwd
[ -f "$RUN_DIR/010-form-b/cwd/data.txt" ] && ok "form B seed file copied to cwd" || bad "form B seed file missing in cwd"

# 7. summary totals
TOTALS_PASS=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['totals']['pass'])" "$RUN_DIR/summary.json")
TOTALS_FAIL=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['totals']['fail'])" "$RUN_DIR/summary.json")
TOTALS_INVALID=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['totals']['invalid'])" "$RUN_DIR/summary.json")
[ "$TOTALS_PASS" = "2" ]    && ok "totals.pass = 2"    || bad "totals.pass = $TOTALS_PASS (expected 2)"
[ "$TOTALS_FAIL" = "1" ]    && ok "totals.fail = 1"    || bad "totals.fail = $TOTALS_FAIL (expected 1)"
[ "$TOTALS_INVALID" = "1" ] && ok "totals.invalid = 1" || bad "totals.invalid = $TOTALS_INVALID (expected 1)"

# 8. index.html mentions all 4 case ids
for id in 001-pass 002-fail 010-form-b 999-invalid; do
    grep -q "$id" "$RUN_DIR/index.html" && ok "index.html mentions $id" || bad "index.html missing $id"
done

# 9. ATOMCODE_HOME isolation: marker file should be in case home,
# NOT in real ~/.atomcode
[ -f "$RUN_DIR/001-pass/home/logs/fake-request.json" ] && ok "ATOMCODE_HOME isolation works" || bad "fake-request.json missing in case home"

# 10. cwd marker (form A pass case writes marker.txt)
[ -f "$RUN_DIR/001-pass/cwd/marker.txt" ] && ok "cwd write captured" || bad "marker.txt missing"

echo ""
echo "=== Smoke summary: $PASSED pass, $FAILED fail ==="
[ "$FAILED" = "0" ] && exit 0 || exit 1
