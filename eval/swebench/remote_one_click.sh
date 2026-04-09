#!/usr/bin/env bash
# One-shot helpers for SWE-bench against root@10.205.224.222 (override via env).
#
# Usage:
#   1) Local predict → rsync run dir → remote grade (typical)
#        ./eval/swebench/remote_one_click.sh sync-grade eval/runs/<ts> [--regrade] ...
#
#   2) Full predict + grade on the remote (atomcode + venv already on server)
#        ./eval/swebench/remote_one_click.sh remote-run-grade [--limit 20] [... run.sh flags]
#
# Environment (optional):
#   SWEBENCH_SSH=root@10.205.224.222
#   SWEBENCH_REMOTE_REPO=/root/atomcode      # atomcode checkout on server
#   SWEBENCH_REMOTE_VENV=/opt/swebench-eval-venv
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

SWEBENCH_SSH="${SWEBENCH_SSH:-root@10.205.224.222}"
SWEBENCH_REMOTE_REPO="${SWEBENCH_REMOTE_REPO:-/root/atomcode}"
SWEBENCH_REMOTE_VENV="${SWEBENCH_REMOTE_VENV:-/opt/swebench-eval-venv}"

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \?//'
}

cmd_sync_grade() {
    local RUN_DIR="${1:?run directory required}"
    shift

    if [[ ! -d "$RUN_DIR" ]] && [[ -d "$REPO_ROOT/$RUN_DIR" ]]; then
        RUN_DIR="$REPO_ROOT/$RUN_DIR"
    fi
    if [[ ! -d "$RUN_DIR" ]]; then
        echo "error: run dir not found: $RUN_DIR" >&2
        exit 2
    fi
    RUN_DIR="$(cd "$RUN_DIR" && pwd)"
    local RUN_ID
    RUN_ID="$(basename "$RUN_DIR")"

    if [[ ! -f "$RUN_DIR/predictions.jsonl" ]]; then
        echo "error: missing predictions.jsonl under $RUN_DIR (finish predict first)" >&2
        exit 3
    fi

    local REL="eval/runs/$RUN_ID"
    echo "[sync-grade] $SWEBENCH_SSH:$SWEBENCH_REMOTE_REPO/$REL"
    ssh -o BatchMode=yes "$SWEBENCH_SSH" "mkdir -p $(printf '%q' "$SWEBENCH_REMOTE_REPO/eval/runs/$RUN_ID")"
    rsync -avz -e ssh "${RUN_DIR}/" "${SWEBENCH_SSH}:$(printf '%q' "$SWEBENCH_REMOTE_REPO")/${REL}/"

    echo "[sync-grade] grading on remote..."
    ssh -o BatchMode=yes "$SWEBENCH_SSH" bash -s -- \
        "$SWEBENCH_REMOTE_REPO" "$SWEBENCH_REMOTE_VENV" "$REL" "$@" <<'REMOTE'
set -euo pipefail
REPO="$1"
VENV="$2"
REL="$3"
shift 3
cd "$REPO"
# shellcheck disable=SC1090
source "$VENV/bin/activate"
exec ./eval/swebench/grade.sh "$@" "$REL"
REMOTE
}

cmd_remote_run_grade() {
    echo "[remote-run-grade] $SWEBENCH_SSH  (predict then grade latest run dir)"
    ssh -o BatchMode=yes "$SWEBENCH_SSH" bash -s -- \
        "$SWEBENCH_REMOTE_REPO" "$SWEBENCH_REMOTE_VENV" "$@" <<'REMOTE'
set -euo pipefail
REPO="$1"
VENV="$2"
shift 2
cd "$REPO"
# shellcheck disable=SC1090
source "$VENV/bin/activate"
./eval/swebench/run.sh "$@"
RUN=$(ls -1t eval/runs | head -1)
if [[ -z "${RUN:-}" ]] || [[ ! -d "eval/runs/$RUN" ]]; then
    echo "error: no eval/runs directory after run.sh" >&2
    exit 4
fi
echo "[remote-run-grade] grading eval/runs/$RUN"
exec ./eval/swebench/grade.sh "eval/runs/$RUN"
REMOTE
}

case "${1:-}" in
    sync-grade)
        shift
        cmd_sync_grade "$@"
        ;;
    remote-run-grade)
        shift
        cmd_remote_run_grade "$@"
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        echo "error: unknown subcommand: ${1:-}" >&2
        usage
        exit 2
        ;;
esac
