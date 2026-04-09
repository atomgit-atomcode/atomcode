#!/usr/bin/env bash
# SWE-bench grade phase entry point.
#
# Wraps upstream `python -m swebench.harness.run_evaluation` and writes
# the result back into per-instance meta.json and summary.json.
#
# Usage:
#   ./eval/swebench/grade.sh eval/runs/<ts>              # grade all predicted (local docker)
#   ./eval/swebench/grade.sh --modal eval/runs/<ts>      # grade on Modal (no local docker)
#   ./eval/swebench/grade.sh --regrade eval/runs/<ts>    # re-grade already-graded
#   ./eval/swebench/grade.sh --instance-id <id> <dir>    # grade single instance

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

REGRADE=0
INSTANCE_ID=""
MAX_WORKERS=""
RUN_DIR=""
USE_MODAL=0

while [ $# -gt 0 ]; do
    case "$1" in
        --regrade)     REGRADE=1; shift ;;
        --instance-id) INSTANCE_ID="$2"; shift 2 ;;
        --max-workers) MAX_WORKERS="$2"; shift 2 ;;
        --modal)       USE_MODAL=1; shift ;;
        --help|-h)     sed -n '2,14p' "$0" | sed 's/^# \?//'; exit 0 ;;
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
# Precheck: docker (skipped for --modal) + swebench package (+ modal for --modal)
# ---------------------------------------------------------------------------
if [ "$USE_MODAL" = "0" ]; then
    if ! docker info >/dev/null 2>&1; then
        echo "error: docker daemon is not running" >&2
        echo "       fix: start Docker Desktop, or add --modal to run on Modal.com instead" >&2
        exit 4
    fi
fi

if ! python3 -c 'import swebench' 2>/dev/null; then
    echo "error: python package 'swebench' not installed" >&2
    echo "       fix: pip install swebench" >&2
    exit 4
fi

if [ "$USE_MODAL" = "1" ]; then
    if ! python3 -c 'import modal' 2>/dev/null; then
        echo "error: --modal requires the 'modal' package" >&2
        echo "       fix: pip install modal && modal token new" >&2
        exit 4
    fi
    if [ ! -f "$HOME/.modal.toml" ]; then
        echo "error: modal token not configured" >&2
        echo "       fix: run 'modal token new' to authenticate" >&2
        exit 4
    fi
fi

# ---------------------------------------------------------------------------
# Filter: any instance with a non-empty patch that has not yet been graded
# (unless --regrade). The status field is unreliable — a case can have
# status=timeout (wall-killed during post-edit verification) yet still hold
# a complete patch.diff. Always trust patch presence over status.
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
    if not regrade and meta.get("swebench_resolved") is not None:
        # Already graded
        continue
    # Trust patch presence, not status. A case is gradeable iff it has
    # a non-empty patch in either patch.diff or prediction.json.
    patch_size = 0
    patch_path = os.path.join(case_dir, "patch.diff")
    if os.path.exists(patch_path):
        patch_size = os.path.getsize(patch_path)
    if patch_size == 0:
        pred_path = os.path.join(case_dir, "prediction.json")
        if os.path.exists(pred_path):
            try:
                with open(pred_path) as f:
                    patch_size = len(json.load(f).get("model_patch", ""))
            except Exception:
                pass
    if patch_size == 0:
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

# ---------------------------------------------------------------------------
# Build a filtered predictions subset (only instances we're grading)
# ---------------------------------------------------------------------------
FILTERED_PREDS="$RUN_DIR/grading/predictions.filtered.jsonl"
mkdir -p "$RUN_DIR/grading"

python3 - "$PREDICTIONS_FILE" "$FILTERED_PREDS" <<PYEOF
import json, sys
want = set("""$FILTER_INSTANCES""".strip().splitlines())
with open(sys.argv[1]) as inp, open(sys.argv[2], "w") as out:
    for line in inp:
        try:
            rec = json.loads(line)
        except Exception:
            continue
        if rec.get("instance_id") in want:
            out.write(line)
PYEOF

# ---------------------------------------------------------------------------
# Invoke upstream grader
# ---------------------------------------------------------------------------
RUN_ID="$(basename "$RUN_DIR")"
MAX_WORKERS_ARG=""
if [ -n "$MAX_WORKERS" ]; then
    MAX_WORKERS_ARG="--max_workers $MAX_WORKERS"
fi

MODAL_ARG=""
if [ "$USE_MODAL" = "1" ]; then
    MODAL_ARG="--modal True"
    echo "[grade] running on Modal.com (no local docker)"
else
    echo "[grade] running on local docker"
fi

echo "[grade] invoking swebench.harness.run_evaluation..."
# Capture harness output to a log so we can salvage partial results when the
# upstream grader hangs/crashes/SIGTERMs (e.g. modal sandbox retry storms).
HARNESS_LOG="$RUN_DIR/grading/harness.log"
mkdir -p "$RUN_DIR/grading"

python3 -m swebench.harness.run_evaluation \
    --dataset_name "princeton-nlp/SWE-bench_Verified" \
    --predictions_path "$FILTERED_PREDS" \
    --run_id "$RUN_ID" \
    $MAX_WORKERS_ARG \
    $MODAL_ARG 2>&1 | tee "$HARNESS_LOG"
HARNESS_RC="${PIPESTATUS[0]}"

if [ "$HARNESS_RC" -ne 0 ]; then
    echo "warning: swebench.harness.run_evaluation exited $HARNESS_RC — attempting partial salvage from $HARNESS_LOG" >&2
    # Last-resort: parse "Result for <id>: resolved: True/False" lines from
    # the harness stdout (these are emitted per case as they finish, even if
    # the harness itself later crashes or is killed). Build a synthetic
    # report so the rest of the pipeline can still produce a grading.json.
    python3 - "$HARNESS_LOG" "$RUN_DIR/grading/salvaged_report.json" "$RUN_ID" <<'PYEOF'
import json, re, sys
log_path, out_path, run_id = sys.argv[1:]
results = {}
try:
    with open(log_path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            m = re.match(r"^Result for (\S+): resolved: (True|False)", line)
            if m:
                results[m.group(1)] = m.group(2) == "True"
except FileNotFoundError:
    pass
if not results:
    print("salvage: no per-case results found in harness log", file=sys.stderr)
    sys.exit(0)
report = {
    "salvaged": True,
    "salvaged_from": log_path,
    "resolved_ids":   [i for i, ok in results.items() if ok],
    "unresolved_ids": [i for i, ok in results.items() if not ok],
    "error_ids": [],
}
with open(out_path, "w") as f:
    json.dump(report, f, indent=2)
print(f"salvage: wrote {len(results)} results ({sum(results.values())} resolved) to {out_path}", file=sys.stderr)
PYEOF
    if [ ! -s "$RUN_DIR/grading/salvaged_report.json" ]; then
        echo "error: salvage failed; no usable grading output" >&2
        exit 5
    fi
fi

# The harness writes its report to the current working directory as
# `<model_name>.<run_id>.json` (e.g. atomcode-2.5.0-default-default.2026-04-08_19-28-44.json).
# Match by RUN_ID anywhere in the filename, pick the most recently modified.
# Portable: BSD stat uses -f "%m %N"; GNU uses -c '%Y %n'. Python avoids the split.
GRADER_OUT=$(RUN_ID="$RUN_ID" python3 - <<'PYEOF'
import glob, os
run_id = os.environ["RUN_ID"]
paths = []
for pattern in (f"./*{run_id}*.json", f"./*/*{run_id}*.json"):
    paths.extend(p for p in glob.glob(pattern) if os.path.isfile(p))
if not paths:
    print("")
else:
    print(max(paths, key=lambda p: os.path.getmtime(p)))
PYEOF
)
if [ -z "$GRADER_OUT" ]; then
    # Fall back to the salvaged report (only present when harness crashed/was killed).
    if [ -s "$RUN_DIR/grading/salvaged_report.json" ]; then
        GRADER_OUT="$RUN_DIR/grading/salvaged_report.json"
        echo "[grade] using salvaged report (upstream harness produced no JSON)" >&2
    else
        echo "error: could not locate grader output JSON (expected *${RUN_ID}*.json in cwd)" >&2
        exit 6
    fi
fi
echo "[grade] grader report: $GRADER_OUT"

# Normalize into our grading.json shape
python3 - "$GRADER_OUT" "$RUN_DIR/grading/grading.json" "$RUN_ID" <<'PYEOF'
import json, sys
from datetime import datetime, timezone

src = json.load(open(sys.argv[1]))
out_path = sys.argv[2]
run_id = sys.argv[3]

# Upstream report shape varies; handle the common forms.
resolved_ids = src.get("resolved_ids") or []
unresolved_ids = src.get("unresolved_ids") or []
error_ids = src.get("error_ids") or []
per_instance = {}
for iid in resolved_ids:
    per_instance[iid] = {"resolved": True, "failure_mode": None}
for iid in unresolved_ids:
    per_instance[iid] = {"resolved": False, "failure_mode": src.get("failure_modes", {}).get(iid, "applied_but_failed")}
for iid in error_ids:
    per_instance[iid] = {"resolved": False, "failure_mode": "grader_error"}

totals = {
    "resolved": len(resolved_ids),
    "unresolved": len(unresolved_ids),
    "error": len(error_ids),
    "total": len(resolved_ids) + len(unresolved_ids) + len(error_ids),
}

out = {
    "run_id": run_id,
    "graded_at": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00","Z"),
    "grader_version": "swebench-harness",  # could read from package version
    "totals": totals,
    "report": per_instance,
}

with open(out_path, "w") as f:
    json.dump(out, f, indent=2)
PYEOF

# ---------------------------------------------------------------------------
# Ingest: write results back to per-instance meta.json
# ---------------------------------------------------------------------------
python3 "$SCRIPT_DIR/ingest_grading.py" \
    --run-dir "$RUN_DIR" \
    --grader-report "$RUN_DIR/grading/grading.json"

# ---------------------------------------------------------------------------
# Update summary.json with primary_score
# ---------------------------------------------------------------------------
python3 - "$RUN_DIR/summary.json" "$RUN_DIR/grading/grading.json" <<'PYEOF'
import json, sys
summary_path, grading_path = sys.argv[1], sys.argv[2]
with open(summary_path) as f:
    s = json.load(f)
with open(grading_path) as f:
    g = json.load(f)

totals = g.get("totals", {})
resolved = totals.get("resolved", 0)
unresolved = totals.get("unresolved", 0)
total = totals.get("total", 0)
rate = (resolved / total) if total else 0.0

s["swebench"]["primary_score"] = {
    "resolved": resolved,
    "unresolved": unresolved,
    "total": total,
    "resolved_rate": round(rate, 4),
}

with open(summary_path, "w") as f:
    json.dump(s, f, indent=2)
print(f"primary_score updated: {resolved}/{total} = {rate*100:.1f}%")
PYEOF

# ---------------------------------------------------------------------------
# Refresh render_index.py HTML
# ---------------------------------------------------------------------------
python3 "$SCRIPT_DIR/../scripts/render_index.py" "$RUN_DIR" 2>/dev/null || true

echo ""
echo "=== grade complete ==="
python3 - "$RUN_DIR/grading/grading.json" <<'PYEOF'
import json, sys
t = json.load(open(sys.argv[1]))["totals"]
print(f"  resolved:   {t.get('resolved', 0)}")
print(f"  unresolved: {t.get('unresolved', 0)}")
print(f"  error:      {t.get('error', 0)}")
print(f"  total:      {t.get('total', 0)}")
PYEOF
exit 0
