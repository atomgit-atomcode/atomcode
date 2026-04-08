#!/usr/bin/env bash
# SWE-bench predict phase entry point.
#
# Loads dataset from eval/swebench/cache/dataset.json, filters per
# manifest.toml and CLI flags, dispatches run_one_instance.sh per
# selected instance via xargs -P, watches for catastrophic failure
# patterns, and writes run-level summary.json.
#
# Usage:
#   ./eval/swebench/run.sh                        # full run with resume
#   ./eval/swebench/run.sh --limit 20             # pilot
#   ./eval/swebench/run.sh --instance-id <id>     # single instance
#   ./eval/swebench/run.sh --dry-run              # preview, no side effects
#   ./eval/swebench/run.sh --warm-cache           # pre-download dataset + bare repos
#   ./eval/swebench/run.sh --fresh                # ignore previous runs' predictions

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EVAL_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$EVAL_DIR/scripts/lib/portable.sh"

# --- defaults ---
MANIFEST="$SCRIPT_DIR/manifest.toml"
CACHE_DIR="$SCRIPT_DIR/cache"
DATASET_CACHE="$CACHE_DIR/dataset.json"
REPO_CACHE="${HOME}/.cache/atomcode-eval/swebench/repos"
RUNS_ROOT="$EVAL_DIR/runs"
BIN=""                      # will default to ./target/release/atomcode
CONFIG_PATH="${HOME}/.atomcode/config.toml"

LIMIT=0
INSTANCE_ID=""
DRY_RUN=0
WARM_CACHE=0
FRESH=0
RETRY_FAILED=0
CONCURRENCY=""              # blank = read from manifest
PROVIDER=""                 # blank = use atomcode config default
PROMPT_TEMPLATE=""          # blank = use manifest
RUNS_DIR_OVERRIDE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --limit)          LIMIT="$2"; shift 2 ;;
        --instance-id)    INSTANCE_ID="$2"; shift 2 ;;
        --dry-run)        DRY_RUN=1; shift ;;
        --warm-cache)     WARM_CACHE=1; shift ;;
        --fresh)          FRESH=1; shift ;;
        --retry-failed)   RETRY_FAILED=1; shift ;;
        --concurrency)    CONCURRENCY="$2"; shift 2 ;;
        --provider)       PROVIDER="$2"; shift 2 ;;
        --prompt)         PROMPT_TEMPLATE="$2"; shift 2 ;;
        --bin)            BIN="$2"; shift 2 ;;
        --config)         CONFIG_PATH="$2"; shift 2 ;;
        --runs-dir)       RUNS_DIR_OVERRIDE="$2"; shift 2 ;;
        --help|-h)
            sed -n '2,18p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "unknown flag: $1" >&2
            exit 2
            ;;
    esac
done

# Default binary: release build
if [ -z "$BIN" ]; then
    BIN="$EVAL_DIR/../target/release/atomcode"
fi

# ---------------------------------------------------------------------------
# Self-check
# ---------------------------------------------------------------------------
self_check() {
    local failed=0
    if ! python3 -c 'import datasets' 2>/dev/null; then
        echo "warning: python \`datasets\` not installed (needed for --warm-cache and fresh fetches)" >&2
    fi
    if [ ! -x "$BIN" ]; then
        echo "error: atomcode binary not found or not executable: $BIN" >&2
        echo "       hint: cargo build --release, or pass --bin /path/to/atomcode" >&2
        failed=1
    fi
    if [ ! -f "$CONFIG_PATH" ]; then
        echo "warning: config file not found: $CONFIG_PATH" >&2
        echo "         runs will use atomcode's fallback provider" >&2
    fi
    if [ ! -d "$RUNS_ROOT" ]; then
        mkdir -p "$RUNS_ROOT"
    fi
    # Disk check: need at least 50GB free in the runs root
    if command -v df >/dev/null; then
        local avail_kb
        avail_kb=$(df -k "$RUNS_ROOT" | awk 'NR==2 {print $4}')
        if [ -n "$avail_kb" ] && [ "$avail_kb" -lt 52428800 ]; then
            echo "warning: less than 50GB free in $RUNS_ROOT (have ${avail_kb}KB)" >&2
        fi
    fi
    return $failed
}

if ! self_check; then
    exit 3
fi

# ---------------------------------------------------------------------------
# Warm-cache mode
# ---------------------------------------------------------------------------
if [ "$WARM_CACHE" = "1" ]; then
    echo "=== warm-cache: fetch dataset ==="
    python3 "$SCRIPT_DIR/fetch_dataset.py" || exit $?

    echo "=== warm-cache: pre-clone all bare repos ==="
    mkdir -p "$REPO_CACHE"
    python3 -c '
import json, sys, subprocess, os
cache = json.load(open(sys.argv[1]))
repos = sorted({i["repo"] for i in cache["instances"]})
print(f"Pre-cloning {len(repos)} repos to {sys.argv[2]}")
for repo in repos:
    safe = repo.replace("/", "__")
    dest = os.path.join(sys.argv[2], f"{safe}.git")
    if os.path.exists(dest):
        print(f"  {repo}: already cached")
        continue
    print(f"  {repo}: cloning...")
    result = subprocess.run(["git", "clone", "--bare", "--quiet", f"https://github.com/{repo}.git", dest])
    if result.returncode != 0:
        print(f"  {repo}: FAILED", file=sys.stderr)
        sys.exit(1)
    subprocess.run(["git", "-C", dest, "config", "gc.auto", "0"])
' "$DATASET_CACHE" "$REPO_CACHE"

    echo "=== warm-cache: pull docker grader images (placeholder — done by grade.sh on first use) ==="
    echo "Run ./grade.sh --warm-cache on a run dir to pre-pull docker images."

    echo ""
    echo "warm-cache complete"
    exit 0
fi

# ---------------------------------------------------------------------------
# Load dataset from cache
# ---------------------------------------------------------------------------
if [ ! -f "$DATASET_CACHE" ]; then
    echo "error: dataset cache not found: $DATASET_CACHE" >&2
    echo "       run: ./eval/swebench/run.sh --warm-cache" >&2
    exit 4
fi

DATASET_REVISION=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["revision"])' "$DATASET_CACHE")

# Read manifest defaults
read_manifest_val() {
    local key="$1"  # dotted like "predict.concurrency"
    python3 -c '
import tomllib, sys
m = tomllib.load(open(sys.argv[1], "rb"))
keys = sys.argv[2].split(".")
v = m
for k in keys:
    v = v.get(k) if isinstance(v, dict) else None
    if v is None:
        print("")
        sys.exit(0)
print(v)
' "$MANIFEST" "$key"
}

[ -z "$CONCURRENCY" ]     && CONCURRENCY=$(read_manifest_val "predict.concurrency")
[ -z "$CONCURRENCY" ]     && CONCURRENCY=4
[ -z "$PROMPT_TEMPLATE" ] && PROMPT_TEMPLATE=$(read_manifest_val "predict.prompt_template")
[ -z "$PROMPT_TEMPLATE" ] && PROMPT_TEMPLATE="default"
INCLUDE_HINTS=$(read_manifest_val "predict.include_hints")
[ -z "$INCLUDE_HINTS" ]   && INCLUDE_HINTS="false"
TIMEOUT_SECS=$(read_manifest_val "predict.timeout_secs")
[ -z "$TIMEOUT_SECS" ]    && TIMEOUT_SECS=600
MAX_TURNS=$(read_manifest_val "predict.max_turns")
[ -z "$MAX_TURNS" ]       && MAX_TURNS=30
ABORT_N_FAIL=$(read_manifest_val "predict.abort_on_consecutive_failures")
[ -z "$ABORT_N_FAIL" ]    && ABORT_N_FAIL=10
ABORT_N_DENY=$(read_manifest_val "predict.abort_on_consecutive_denials")
[ -z "$ABORT_N_DENY" ]    && ABORT_N_DENY=5

# ---------------------------------------------------------------------------
# Filter instances per flags
# ---------------------------------------------------------------------------
FILTERED_INSTANCES=$(python3 - "$DATASET_CACHE" "$MANIFEST" "$INSTANCE_ID" "$LIMIT" <<'PYEOF'
import json, sys, tomllib
cache = json.load(open(sys.argv[1]))
manifest = tomllib.load(open(sys.argv[2], "rb"))
inst_id_filter = sys.argv[3]
limit = int(sys.argv[4])

instances = cache["instances"]
f = manifest.get("filter", {})

if inst_id_filter:
    instances = [i for i in instances if i["instance_id"] == inst_id_filter]
else:
    repos = f.get("repos") or []
    if repos:
        instances = [i for i in instances if i["repo"] in repos]
    ids = f.get("instance_ids") or []
    if ids:
        instances = [i for i in instances if i["instance_id"] in ids]
    excl = f.get("exclude_instance_ids") or []
    if excl:
        instances = [i for i in instances if i["instance_id"] not in excl]
    manifest_limit = f.get("limit", 0)
    if manifest_limit > 0 and limit == 0:
        instances = instances[:manifest_limit]

if limit > 0:
    instances = instances[:limit]

for inst in instances:
    print(inst["instance_id"])
PYEOF
)

TOTAL_INSTANCES=$(printf '%s\n' "$FILTERED_INSTANCES" | grep -c '^' || echo 0)

if [ "$TOTAL_INSTANCES" = "0" ]; then
    echo "no instances match the filter" >&2
    exit 5
fi

# ---------------------------------------------------------------------------
# Determine run dir + resume
# ---------------------------------------------------------------------------
if [ -n "$RUNS_DIR_OVERRIDE" ]; then
    EVAL_RUN_DIR="$RUNS_DIR_OVERRIDE"
elif [ "$FRESH" = "1" ] || [ ! -d "$RUNS_ROOT" ] || [ -z "$(ls -A "$RUNS_ROOT" 2>/dev/null)" ]; then
    EVAL_RUN_DIR="$RUNS_ROOT/$(date +%Y-%m-%d_%H-%M-%S)"
else
    # Resume: find the most recent run dir
    LATEST=$(ls -1t "$RUNS_ROOT" | head -1)
    if [ -n "$LATEST" ] && [ -d "$RUNS_ROOT/$LATEST" ]; then
        EVAL_RUN_DIR="$RUNS_ROOT/$LATEST"
        echo "[resume] continuing run at $EVAL_RUN_DIR" >&2
    else
        EVAL_RUN_DIR="$RUNS_ROOT/$(date +%Y-%m-%d_%H-%M-%S)"
    fi
fi
mkdir -p "$EVAL_RUN_DIR"

# Compute the "skip set" from existing per-case prediction.json files.
# (We adopted per-case JSON in T7.5 to avoid concurrent append races.
# The aggregated predictions.jsonl is generated at end of dispatch.)
SKIP_SET=""
if [ "$FRESH" != "1" ] && [ -d "$EVAL_RUN_DIR" ]; then
    SKIP_SET=$(python3 - "$EVAL_RUN_DIR" <<'PYEOF'
import json, os, sys
run_dir = sys.argv[1]
seen = set()
for name in os.listdir(run_dir):
    p = os.path.join(run_dir, name, "prediction.json")
    if os.path.isfile(p):
        try:
            with open(p) as f:
                seen.add(json.load(f)["instance_id"])
        except Exception:
            pass
print("\n".join(sorted(seen)))
PYEOF
)
fi

# If retry-failed, also skip instances whose meta.json already exists with a
# non-failure status. Instances with status in {fail, denied, error, timeout}
# will be re-run.
FINAL_INSTANCES=$(python3 - "$EVAL_RUN_DIR" "$RETRY_FAILED" <<PYEOF
import json, os, sys
run_dir = sys.argv[1]
retry_failed = sys.argv[2] == "1"
all_filtered = """$FILTERED_INSTANCES""".strip().splitlines()
skip_set = set("""$SKIP_SET""".strip().splitlines()) if """$SKIP_SET""".strip() else set()

# Re-queue failed instances if --retry-failed
RETRY_STATUSES = {"fail", "denied", "error", "timeout"}
if retry_failed:
    to_unskip = set()
    for iid in skip_set:
        meta = os.path.join(run_dir, iid, "meta.json")
        if os.path.exists(meta):
            try:
                with open(meta) as f:
                    st = json.load(f).get("status", "")
                if st in RETRY_STATUSES:
                    to_unskip.add(iid)
            except Exception:
                pass
    skip_set -= to_unskip

for iid in all_filtered:
    if iid and iid not in skip_set:
        print(iid)
PYEOF
)
REMAINING=$(printf '%s\n' "$FINAL_INSTANCES" | grep -c '^' || echo 0)

echo ""
echo "=== predict run ==="
echo "  dataset:     $DATASET_REVISION"
echo "  run dir:     $EVAL_RUN_DIR"
echo "  total:       $TOTAL_INSTANCES instances after filter"
echo "  remaining:   $REMAINING (after resume skip)"
echo "  concurrency: $CONCURRENCY"
echo "  provider:    ${PROVIDER:-<config default>}"
echo "  prompt:      $PROMPT_TEMPLATE"
echo "  timeout:     ${TIMEOUT_SECS}s per instance"
echo "  max turns:   $MAX_TURNS"
echo ""

if [ "$DRY_RUN" = "1" ]; then
    echo "[dry-run] would process $REMAINING instances; estimated cost: $(python3 -c "print(f'~\${$REMAINING * 0.15:.2f}')")" # rough ~50k tok * $3/M
    exit 0
fi

# ---------------------------------------------------------------------------
# Early-abort monitor: kills the dispatcher if too many consecutive failures
# ---------------------------------------------------------------------------
ABORT_FLAG="$EVAL_RUN_DIR/.abort"
rm -f "$ABORT_FLAG"

monitor_abort() {
    local dir="$1"
    local run_id
    run_id=$(basename "$dir")
    while true; do
        sleep 5
        [ -f "$ABORT_FLAG" ] && return 0

        # Check most recent N instance results for consecutive failures
        # mac-only stat syntax; TODO portable
        recent=$(find "$dir" -name "meta.json" -type f 2>/dev/null | \
            xargs -I{} stat -f "%m {}" 2>/dev/null | \
            sort -rn | head -$ABORT_N_FAIL | awk '{print $2}')
        [ -z "$recent" ] && continue

        fail_streak=$(python3 -c '
import json, sys
files = sys.stdin.read().strip().splitlines()
if not files:
    sys.exit(0)
streak = 0
for f in files:
    try:
        with open(f) as fp:
            st = json.load(fp).get("status", "")
        if st not in ("predicted",):
            streak += 1
        else:
            break
    except Exception:
        pass
print(streak)
' <<< "$recent" || echo 0)

        if [ "$fail_streak" -ge "$ABORT_N_FAIL" ] 2>/dev/null; then
            echo "" >&2
            echo "=== EARLY ABORT: $fail_streak consecutive non-predicted instances ===" >&2
            echo "Check $dir/*/stderr.txt for root cause." >&2
            touch "$ABORT_FLAG"
            return 0
        fi
    done
}

monitor_abort "$EVAL_RUN_DIR" &
MONITOR_PID=$!
trap 'kill $MONITOR_PID 2>/dev/null || true' EXIT

# ---------------------------------------------------------------------------
# Dispatch via xargs -P
# ---------------------------------------------------------------------------
export EVAL_RUN_DIR
export EVAL_BIN="$BIN"
export EVAL_CONFIG_PATH="$CONFIG_PATH"
export EVAL_REPO_CACHE="$REPO_CACHE"
export EVAL_PROMPT_TEMPLATE="$PROMPT_TEMPLATE"
export EVAL_INCLUDE_HINTS="$INCLUDE_HINTS"
export EVAL_TIMEOUT_SECS="$TIMEOUT_SECS"
export EVAL_MAX_TURNS="$MAX_TURNS"
export EVAL_PROVIDER="$PROVIDER"
export EVAL_DATASET_REVISION="$DATASET_REVISION"
export CONCURRENCY

# Build per-instance JSON (base64) then pipe to xargs. Each line is a
# base64 blob of that instance's full dict.
INSTANCE_B64_LIST=$(python3 - "$DATASET_CACHE" <<PYEOF
import json, sys, base64
cache = json.load(open(sys.argv[1]))
want = set("""$FINAL_INSTANCES""".strip().splitlines())
for inst in cache["instances"]:
    if inst["instance_id"] in want:
        b64 = base64.b64encode(json.dumps(inst).encode()).decode()
        print(b64)
PYEOF
)

printf '%s\n' "$INSTANCE_B64_LIST" | grep -v '^$' | xargs -n1 -P "$CONCURRENCY" -I{} \
    bash -c '
        [ -f "$EVAL_RUN_DIR/.abort" ] && exit 0
        EVAL_INSTANCE_JSON_B64="$1" "$0" 2>&1 | sed "s|^|  |"
    ' "$SCRIPT_DIR/run_one_instance.sh" {}

# ---------------------------------------------------------------------------
# Aggregate per-case prediction.json → predictions.jsonl
# (T7.5 hardening: workers write per-case to avoid POSIX append races,
# then we concat here post-dispatch. Single-writer, no concurrency.)
# ---------------------------------------------------------------------------
AGG="$EVAL_RUN_DIR/predictions.jsonl"
: > "$AGG"
for d in "$EVAL_RUN_DIR"/*/; do
    p="${d}prediction.json"
    if [ -f "$p" ]; then
        cat "$p" >> "$AGG"
    fi
done
PRED_COUNT=$(wc -l < "$AGG" | tr -d ' ')
echo "[aggregate] wrote $PRED_COUNT predictions to $AGG" >&2

# Stop monitor
touch "$ABORT_FLAG" 2>/dev/null || true
kill $MONITOR_PID 2>/dev/null || true
wait $MONITOR_PID 2>/dev/null || true

# ---------------------------------------------------------------------------
# Post-run: generate summary.json and print summary
# ---------------------------------------------------------------------------
generate_summary() {
    python3 - "$EVAL_RUN_DIR" "$BIN" "$DATASET_REVISION" "$PROMPT_TEMPLATE" "$INCLUDE_HINTS" "$PROVIDER" <<'PYEOF'
import json
import os
import sys
from datetime import datetime, timezone
from collections import defaultdict

run_dir = sys.argv[1]
bin_path = sys.argv[2]
dataset_rev = sys.argv[3]
prompt_tpl = sys.argv[4]
include_hints = sys.argv[5] == "true"
provider = sys.argv[6]

# Collect all per-instance meta.json files
metas = []
for name in sorted(os.listdir(run_dir)):
    mpath = os.path.join(run_dir, name, "meta.json")
    if os.path.exists(mpath):
        try:
            with open(mpath) as f:
                metas.append(json.load(f))
        except Exception:
            pass

# Status counts
totals = defaultdict(int)
for m in metas:
    totals[m.get("status", "unknown")] += 1

# Efficiency aggregates
n = len(metas)
def avg(key_path):
    vals = []
    for m in metas:
        v = m
        for k in key_path.split("."):
            v = v.get(k) if isinstance(v, dict) else None
            if v is None:
                break
        if isinstance(v, (int, float)):
            vals.append(v)
    return sum(vals) / len(vals) if vals else 0

total_cost = sum(
    m.get("efficiency", {}).get("estimated_cost_usd", 0) for m in metas
)
resolved = sum(1 for m in metas if m.get("swebench_resolved") is True)
cost_per_resolved = (total_cost / resolved) if resolved > 0 else None

# By-outcome comparison
def group_stats(filter_fn):
    group = [m for m in metas if filter_fn(m)]
    if not group:
        return {"n": 0}
    return {
        "n": len(group),
        "avg_turns": sum(m.get("efficiency", {}).get("turns", 0) for m in group) / len(group),
        "avg_prompt_tokens": sum(m.get("efficiency", {}).get("prompt_tokens", 0) for m in group) / len(group),
        "avg_completion_tokens": sum(m.get("efficiency", {}).get("completion_tokens", 0) for m in group) / len(group),
        "avg_wall_seconds": sum(m.get("wall_ms", 0) for m in group) / 1000.0 / len(group),
    }

summary = {
    "started_at": metas[0]["started_at"] if metas else "",
    "ended_at": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00","Z"),
    "status": "done",
    "runner_version": f"eval/swebench/run.sh",
    "case_count": len(metas),
    "concurrency": int(os.environ.get("CONCURRENCY", "4") or 4),
    "totals": dict(totals),
    "swebench": {
        "dataset": "princeton-nlp/SWE-bench_Verified",
        "dataset_revision": dataset_rev,
        "prompt_template": prompt_tpl,
        "include_hints": include_hints,
        "provider": provider,
        "model": None,
        "primary_score": None,  # populated by grade phase
        "secondary_metrics": {
            "avg_turns": avg("efficiency.turns"),
            "avg_prompt_tokens": avg("efficiency.prompt_tokens"),
            "avg_completion_tokens": avg("efficiency.completion_tokens"),
            "avg_tool_calls": avg("efficiency.tool_calls"),
            "avg_wall_seconds": avg("wall_ms") / 1000.0 if n else 0,
            "total_estimated_cost_usd": round(total_cost, 4),
            "cost_per_resolved_usd": round(cost_per_resolved, 4) if cost_per_resolved else None,
            "by_outcome": {
                "predicted": group_stats(lambda m: m.get("status") == "predicted"),
                "failed":    group_stats(lambda m: m.get("status") in ("fail","error","timeout","denied")),
            },
        },
    },
}

out_path = os.path.join(run_dir, "summary.json")
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(summary, f, indent=2)
print(f"summary.json written to {out_path}")
PYEOF
}

generate_summary

# ---------------------------------------------------------------------------
# Print human-readable summary to stdout
# ---------------------------------------------------------------------------
python3 - "$EVAL_RUN_DIR/summary.json" <<'PYEOF'
import json, sys
s = json.load(open(sys.argv[1]))
sw = s["swebench"]
sm = sw["secondary_metrics"]
print()
print("=" * 50)
print("PREDICT PHASE COMPLETE")
print("=" * 50)
print(f"Run dir:     {sys.argv[1].rsplit('/',1)[0]}")
print(f"Dataset:     {sw['dataset_revision']}")
print(f"Prompt:      {sw['prompt_template']}")
print(f"Provider:    {sw['provider'] or '<config default>'}")
print()
print("Totals:")
for status, count in sorted(s["totals"].items()):
    print(f"  {status:10s}: {count}")
print()
print("Secondary (efficiency) metrics:")
print(f"  avg turns:             {sm['avg_turns']:.1f}")
print(f"  avg prompt tokens:     {sm['avg_prompt_tokens']:.0f}")
print(f"  avg completion tokens: {sm['avg_completion_tokens']:.0f}")
print(f"  avg tool calls:        {sm['avg_tool_calls']:.1f}")
print(f"  avg wall time:         {sm['avg_wall_seconds']:.1f}s")
print(f"  estimated cost:        ${sm['total_estimated_cost_usd']:.2f}")
print()
print(f"Next: ./eval/swebench/grade.sh {sys.argv[1].rsplit('/',1)[0]}")
PYEOF
