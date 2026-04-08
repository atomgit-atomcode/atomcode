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
