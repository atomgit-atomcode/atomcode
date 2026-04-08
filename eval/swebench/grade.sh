#!/usr/bin/env bash
# SWE-bench grade phase entry point.
#
# Wraps upstream `python -m swebench.harness.run_evaluation` and writes
# the result back into per-instance meta.json and summary.json.
#
# Usage:
#   ./eval/swebench/grade.sh eval/runs/<ts>              # grade all predicted instances
#   ./eval/swebench/grade.sh --regrade eval/runs/<ts>    # re-grade already-graded
#   ./eval/swebench/grade.sh --instance-id <id> <dir>    # grade single instance

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

REGRADE=0
INSTANCE_ID=""
MAX_WORKERS=""
RUN_DIR=""

while [ $# -gt 0 ]; do
    case "$1" in
        --regrade)     REGRADE=1; shift ;;
        --instance-id) INSTANCE_ID="$2"; shift 2 ;;
        --max-workers) MAX_WORKERS="$2"; shift 2 ;;
        --help|-h)     sed -n '2,12p' "$0" | sed 's/^# \?//'; exit 0 ;;
        -*)            echo "unknown flag: $1" >&2; exit 2 ;;
        *)             RUN_DIR="$1"; shift ;;
    esac
done

if [ -z "$RUN_DIR" ]; then
    echo "usage: grade.sh [--regrade] [--instance-id ID] [--max-workers N] <run-dir>" >&2
    exit 2
fi

if [ ! -d "$RUN_DIR" ]; then
    echo "error: run dir not found: $RUN_DIR" >&2
    exit 3
fi

PREDICTIONS_FILE="$RUN_DIR/predictions.jsonl"
if [ ! -f "$PREDICTIONS_FILE" ]; then
    echo "error: predictions.jsonl not found in $RUN_DIR" >&2
    exit 3
fi

# ---------------------------------------------------------------------------
# Precheck: docker + swebench package
# ---------------------------------------------------------------------------
if ! docker info >/dev/null 2>&1; then
    echo "error: docker daemon is not running" >&2
    echo "       fix: start Docker Desktop (or systemctl start docker)" >&2
    exit 4
fi

if ! python3 -c 'import swebench' 2>/dev/null; then
    echo "error: python package 'swebench' not installed" >&2
    echo "       fix: pip install swebench" >&2
    exit 4
fi

# ---------------------------------------------------------------------------
# Filter: only instances that are status=predicted and not yet graded
# (unless --regrade).
# ---------------------------------------------------------------------------
FILTER_INSTANCES=$(python3 - "$RUN_DIR" "$REGRADE" "$INSTANCE_ID" <<'PYEOF'
import json, os, sys
run_dir = sys.argv[1]
regrade = sys.argv[2] == "1"
single = sys.argv[3]

selected = []
for name in sorted(os.listdir(run_dir)):
    case_dir = os.path.join(run_dir, name)
    meta_path = os.path.join(case_dir, "meta.json")
    if not os.path.exists(meta_path):
        continue
    with open(meta_path) as f:
        meta = json.load(f)
    if meta.get("form") != "swebench":
        continue
    if single and meta.get("id") != single:
        continue
    if meta.get("status") != "predicted" and meta.get("swebench_resolved") is None:
        # Non-predicted instances (fail/error/timeout) are not graded
        continue
    if not regrade and meta.get("swebench_resolved") is not None:
        # Already graded
        continue
    selected.append(meta["id"])

for iid in selected:
    print(iid)
PYEOF
)
GRADE_COUNT=$(printf '%s\n' "$FILTER_INSTANCES" | grep -c '^' || echo 0)

if [ "$GRADE_COUNT" = "0" ]; then
    echo "no instances need grading (use --regrade to re-grade all)"
    exit 0
fi

echo "=== grade phase ==="
echo "  run dir:     $RUN_DIR"
echo "  to grade:    $GRADE_COUNT instances"
echo ""

# placeholder for actual grader invocation — task 16 will fill this in
echo "[stub] would invoke swebench.harness.run_evaluation here"
exit 0
