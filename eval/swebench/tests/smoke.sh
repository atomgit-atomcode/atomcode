#!/usr/bin/env bash
# End-to-end smoke for the SWE-bench subsystem.
# Exercises run_one_instance.sh against fake-atomcode-swebench.sh
# using a tiny local git repo as the "target" of SWE-bench.
#
# Run: ./eval/swebench/tests/smoke.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SWEBENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
FAKE="$SCRIPT_DIR/fake-atomcode-swebench.sh"

TMPROOT=$(mktemp -d /tmp/swebench-smoke-XXXXXX)
TMPRUNS="$TMPROOT/runs"
TMPCACHE="$TMPROOT/cache"
TMPREPO="$TMPROOT/fake-repo"
trap 'rm -rf "$TMPROOT"' EXIT

mkdir -p "$TMPRUNS"
mkdir -p "$TMPCACHE"

# --- create a fake "upstream" repo at a specific commit ---
mkdir -p "$TMPREPO"
git -C "$TMPREPO" init --quiet
git -C "$TMPREPO" config user.email smoke@test.local
git -C "$TMPREPO" config user.name smoke
echo "# Fake repo for smoke" > "$TMPREPO/README.md"
echo "print('hello')" > "$TMPREPO/main.py"
git -C "$TMPREPO" add .
git -C "$TMPREPO" commit --quiet -m "initial"
BASE_COMMIT=$(git -C "$TMPREPO" rev-parse HEAD)

# Pre-populate the bare cache so run_one_instance.sh doesn't try to clone from github.
mkdir -p "$TMPCACHE"
git clone --bare --quiet "$TMPREPO" "$TMPCACHE/fake-owner__fake-repo.git"
git -C "$TMPCACHE/fake-owner__fake-repo.git" config gc.auto 0

PASSED=0
FAILED=0
ok()  { PASSED=$((PASSED + 1)); echo "  [PASS] $1"; }
bad() { FAILED=$((FAILED + 1)); echo "  [FAIL] $1"; }

echo "=== Scenario 1: successful prediction ==="

INSTANCE_JSON='{
  "instance_id": "fake-owner__fake-repo-1",
  "repo": "fake-owner/fake-repo",
  "base_commit": "'"$BASE_COMMIT"'",
  "problem_statement": "FAKE_NATURAL please fix the README",
  "hints_text": ""
}'
INSTANCE_B64=$(printf '%s' "$INSTANCE_JSON" | base64 | tr -d '\n')

RUN_DIR="$TMPRUNS/2026-04-08_12-00-00"
mkdir -p "$RUN_DIR"

EVAL_RUN_DIR="$RUN_DIR" \
EVAL_INSTANCE_JSON_B64="$INSTANCE_B64" \
EVAL_BIN="$FAKE" \
EVAL_CONFIG_PATH="" \
EVAL_REPO_CACHE="$TMPCACHE" \
EVAL_PROMPT_TEMPLATE="default" \
EVAL_INCLUDE_HINTS="false" \
EVAL_TIMEOUT_SECS="60" \
EVAL_MAX_TURNS="10" \
EVAL_PROVIDER="siliconflow" \
EVAL_DATASET_REVISION="fake-rev-abcdef" \
"$SWEBENCH_DIR/run_one_instance.sh"

CASE_DIR="$RUN_DIR/fake-owner__fake-repo-1"

[ -f "$CASE_DIR/meta.json" ]           && ok "meta.json created"           || bad "meta.json missing"
[ -f "$CASE_DIR/patch.diff" ]          && ok "patch.diff created"          || bad "patch.diff missing"
[ -f "$CASE_DIR/prompt.rendered.txt" ] && ok "prompt.rendered.txt created" || bad "prompt.rendered.txt missing"
[ -f "$CASE_DIR/stdout.txt" ]          && ok "stdout.txt created"          || bad "stdout.txt missing"
[ -f "$CASE_DIR/stderr.txt" ]          && ok "stderr.txt created"          || bad "stderr.txt missing"
[ -f "$CASE_DIR/prediction.json" ]     && ok "prediction.json created"     || bad "prediction.json missing"

# meta.json status should be "predicted"
STATUS=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status"))' "$CASE_DIR/meta.json")
[ "$STATUS" = "predicted" ] && ok "status=predicted" || bad "status wrong: got $STATUS"

# form should be "swebench"
FORM=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("form"))' "$CASE_DIR/meta.json")
[ "$FORM" = "swebench" ] && ok "form=swebench" || bad "form wrong: got $FORM"

# efficiency.turns should be 2 (fake-atomcode emitted turns=2)
TURNS=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["efficiency"]["turns"])' "$CASE_DIR/meta.json")
[ "$TURNS" = "2" ] && ok "efficiency.turns=2" || bad "efficiency.turns wrong: got $TURNS"

# efficiency.stop_reason should be "natural"
STOP_REASON=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["efficiency"]["stop_reason"])' "$CASE_DIR/meta.json")
[ "$STOP_REASON" = "natural" ] && ok "efficiency.stop_reason=natural" || bad "stop_reason wrong: got $STOP_REASON"

# patch.diff should be non-empty
PATCH_SIZE=$(wc -c < "$CASE_DIR/patch.diff" | tr -d ' ')
[ "$PATCH_SIZE" -gt 0 ] && ok "patch.diff non-empty ($PATCH_SIZE bytes)" || bad "patch.diff empty"

# prediction.json should be one JSON line with matching instance_id
PRED_ID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["instance_id"])' "$CASE_DIR/prediction.json")
[ "$PRED_ID" = "fake-owner__fake-repo-1" ] && ok "prediction.json instance_id correct" || bad "prediction.json instance_id wrong: $PRED_ID"

echo ""
echo "=== Scenario 2: turn_limit truncation ==="

INSTANCE_JSON2='{
  "instance_id": "fake-owner__fake-repo-2",
  "repo": "fake-owner/fake-repo",
  "base_commit": "'"$BASE_COMMIT"'",
  "problem_statement": "FAKE_TURN_LIMIT please",
  "hints_text": ""
}'
INSTANCE_B64_2=$(printf '%s' "$INSTANCE_JSON2" | base64 | tr -d '\n')

EVAL_RUN_DIR="$RUN_DIR" \
EVAL_INSTANCE_JSON_B64="$INSTANCE_B64_2" \
EVAL_BIN="$FAKE" \
EVAL_CONFIG_PATH="" \
EVAL_REPO_CACHE="$TMPCACHE" \
EVAL_PROMPT_TEMPLATE="default" \
EVAL_INCLUDE_HINTS="false" \
EVAL_TIMEOUT_SECS="60" \
EVAL_MAX_TURNS="2" \
EVAL_PROVIDER="siliconflow" \
EVAL_DATASET_REVISION="fake-rev-abcdef" \
"$SWEBENCH_DIR/run_one_instance.sh"

CASE_DIR2="$RUN_DIR/fake-owner__fake-repo-2"
STOP_REASON2=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["efficiency"]["stop_reason"])' "$CASE_DIR2/meta.json")
[ "$STOP_REASON2" = "turn_limit" ] && ok "scenario 2: stop_reason=turn_limit" || bad "scenario 2 stop_reason wrong: $STOP_REASON2"

echo ""
echo "=== Scenario 3: failure ==="

INSTANCE_JSON3='{
  "instance_id": "fake-owner__fake-repo-3",
  "repo": "fake-owner/fake-repo",
  "base_commit": "'"$BASE_COMMIT"'",
  "problem_statement": "FAKE_FAIL please",
  "hints_text": ""
}'
INSTANCE_B64_3=$(printf '%s' "$INSTANCE_JSON3" | base64 | tr -d '\n')

EVAL_RUN_DIR="$RUN_DIR" \
EVAL_INSTANCE_JSON_B64="$INSTANCE_B64_3" \
EVAL_BIN="$FAKE" \
EVAL_CONFIG_PATH="" \
EVAL_REPO_CACHE="$TMPCACHE" \
EVAL_PROMPT_TEMPLATE="default" \
EVAL_INCLUDE_HINTS="false" \
EVAL_TIMEOUT_SECS="60" \
EVAL_MAX_TURNS="10" \
EVAL_PROVIDER="siliconflow" \
EVAL_DATASET_REVISION="fake-rev-abcdef" \
"$SWEBENCH_DIR/run_one_instance.sh"

CASE_DIR3="$RUN_DIR/fake-owner__fake-repo-3"
STATUS3=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status"))' "$CASE_DIR3/meta.json")
[ "$STATUS3" = "fail" ] && ok "scenario 3: status=fail" || bad "scenario 3 status wrong: $STATUS3"

echo ""
echo "=== Scenario 4: run.sh dispatch with 3 fake instances ==="

# Build a fake dataset cache with 3 instances
FAKE_CACHE_DIR="$TMPROOT/fake-dataset-cache"
mkdir -p "$FAKE_CACHE_DIR"
cat > "$FAKE_CACHE_DIR/dataset.json" <<EOF
{
  "revision": "fake-rev-abcdef",
  "instances": [
    {
      "instance_id": "fake-owner__fake-repo-10",
      "repo": "fake-owner/fake-repo",
      "base_commit": "$BASE_COMMIT",
      "problem_statement": "FAKE_NATURAL: issue 10",
      "hints_text": ""
    },
    {
      "instance_id": "fake-owner__fake-repo-11",
      "repo": "fake-owner/fake-repo",
      "base_commit": "$BASE_COMMIT",
      "problem_statement": "FAKE_NATURAL: issue 11",
      "hints_text": ""
    },
    {
      "instance_id": "fake-owner__fake-repo-12",
      "repo": "fake-owner/fake-repo",
      "base_commit": "$BASE_COMMIT",
      "problem_statement": "FAKE_NATURAL: issue 12",
      "hints_text": ""
    }
  ]
}
EOF

# Preserve any real user cache by moving it aside; restored on trap.
# Without this, running the smoke on a dev machine wipes the user's
# real dataset.json (300MB+) as a side effect of the symlink swap.
CACHE_BACKUP=""
if [ -e "$SWEBENCH_DIR/cache" ] && [ ! -L "$SWEBENCH_DIR/cache" ]; then
    CACHE_BACKUP="$TMPROOT/cache-backup"
    mv "$SWEBENCH_DIR/cache" "$CACHE_BACKUP"
fi
ln -s "$FAKE_CACHE_DIR" "$SWEBENCH_DIR/cache"

# Symlink the fake bare repo into HOME cache so run.sh finds it (option c fix)
mkdir -p "$HOME/.cache/atomcode-eval/swebench/repos"
ln -sf "$TMPCACHE/fake-owner__fake-repo.git" "$HOME/.cache/atomcode-eval/swebench/repos/fake-owner__fake-repo.git"

# Updated trap: restore the user's real cache if we backed one up, otherwise
# leave a minimal .gitkeep stub. Clean up HOME cache symlink + TMPROOT.
trap '
    rm -f "$SWEBENCH_DIR/cache"
    if [ -n "$CACHE_BACKUP" ] && [ -e "$CACHE_BACKUP" ]; then
        mv "$CACHE_BACKUP" "$SWEBENCH_DIR/cache"
    else
        mkdir -p "$SWEBENCH_DIR/cache"
        touch "$SWEBENCH_DIR/cache/.gitkeep"
    fi
    rm -f "$HOME/.cache/atomcode-eval/swebench/repos/fake-owner__fake-repo.git"
    rm -rf "$TMPROOT"
' EXIT

RUN4_DIR="$TMPROOT/runs/run4"
"$SWEBENCH_DIR/run.sh" \
    --bin "$FAKE" \
    --config "" \
    --runs-dir "$RUN4_DIR/ts" \
    --concurrency 2 \
    --provider siliconflow 2>&1 | tail -30

[ -f "$RUN4_DIR/ts/predictions.jsonl" ] && ok "M4: predictions.jsonl created" || bad "M4: predictions.jsonl missing"
PRED_COUNT=$(wc -l < "$RUN4_DIR/ts/predictions.jsonl" | tr -d ' ')
[ "$PRED_COUNT" = "3" ] && ok "M4: 3 predictions" || bad "M4: expected 3 predictions, got $PRED_COUNT"

[ -f "$RUN4_DIR/ts/summary.json" ] && ok "M4: summary.json created" || bad "M4: summary.json missing"
PREDICTED_COUNT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["totals"].get("predicted",0))' "$RUN4_DIR/ts/summary.json")
[ "$PREDICTED_COUNT" = "3" ] && ok "M4: 3 predicted" || bad "M4: expected 3 predicted, got $PREDICTED_COUNT"

echo ""
echo "=== Scenario 5: resume skips completed instances ==="

# Re-run the same dispatch; should skip all 3 (they're already in predictions.jsonl)
"$SWEBENCH_DIR/run.sh" \
    --bin "$FAKE" \
    --config "" \
    --runs-dir "$RUN4_DIR/ts" \
    --concurrency 2 \
    --provider siliconflow 2>&1 | tail -10

# predictions.jsonl line count should still be 3 (no dupes)
PRED_COUNT2=$(wc -l < "$RUN4_DIR/ts/predictions.jsonl" | tr -d ' ')
[ "$PRED_COUNT2" = "3" ] && ok "M4: resume skipped completed (still 3 predictions)" || bad "M4: resume broke — got $PRED_COUNT2 predictions"

echo ""
echo "=== SWE-bench smoke summary: $PASSED pass, $FAILED fail ==="
[ "$FAILED" = "0" ] && exit 0 || exit 1
