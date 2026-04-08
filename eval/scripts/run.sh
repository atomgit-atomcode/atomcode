#!/usr/bin/env bash
# AtomCode batch eval harness — V1.
#
# Usage: ./eval/scripts/run.sh [options]
# See ./eval/scripts/run.sh --help for the full option list.

# Strict mode: portable.sh enables set -u; we add -e and pipefail here.
set -eo pipefail

# Helper: assert the next arg exists for a value-taking option.
_need_arg() {
    if [ $# -lt 2 ]; then
        echo "fatal: option '$1' requires an argument" >&2
        exit 2
    fi
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

. "$SCRIPT_DIR/lib/portable.sh"
# After sourcing portable.sh, set -u is enabled. Combine with our -e + pipefail.

# --- defaults ---
BIN="$PROJECT_ROOT/target/release/atomcode"
CASES_DIR="$PROJECT_ROOT/eval/cases"
RUNS_DIR="$PROJECT_ROOT/eval/runs"
CONCURRENCY=""
PROVIDER_OVERRIDE=""
ONLY_IDS=()
ONLY_PATTERN=""
CONFIG_PATH=""                    # passed to atomcode via --config; defaults to ~/.atomcode/config.toml if that file exists
CONFIG_USER_PROVIDED=0            # 1 if --config was explicitly passed (makes the file-exists check fatal)

# --- arg parsing ---
usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

Options:
  --bin <path>           Atomcode binary to test (default: ./target/release/atomcode)
  --cases-dir <path>     Where to scan for case files (default: ./eval/cases)
  --runs-dir <path>      Where to write run artifacts (default: ./eval/runs)
  --concurrency <N>      Parallel cases (default: min(4, nproc))
  --only <id>            Run only the case with this exact id (can repeat)
  --only-pattern <re>    Run only cases whose id matches this regex
  --provider <name>      Override provider for ALL cases
  --config <path>        Atomcode config.toml to use for provider lookup
                         (default: ~/.atomcode/config.toml if it exists;
                         otherwise each case falls back to atomcode's
                         built-in dummy provider and will fail LLM calls)
  -h, --help             Show this help

Examples:
  $(basename "$0")
  $(basename "$0") --only 001-fizzbuzz
  $(basename "$0") --provider kimi
  $(basename "$0") --config /tmp/custom-config.toml
  $(basename "$0") --bin /tmp/atomcode-2.4.0 --runs-dir runs/v2.4
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --bin)           _need_arg "$@"; BIN="$2"; shift 2 ;;
        --cases-dir)     _need_arg "$@"; CASES_DIR="$2"; shift 2 ;;
        --runs-dir)      _need_arg "$@"; RUNS_DIR="$2"; shift 2 ;;
        --concurrency)   _need_arg "$@"; CONCURRENCY="$2"; shift 2 ;;
        --only)          _need_arg "$@"; ONLY_IDS+=("$2"); shift 2 ;;
        --only-pattern)  _need_arg "$@"; ONLY_PATTERN="$2"; shift 2 ;;
        --provider)      _need_arg "$@"; PROVIDER_OVERRIDE="$2"; shift 2 ;;
        --config)        _need_arg "$@"; CONFIG_PATH="$2"; CONFIG_USER_PROVIDED=1; shift 2 ;;
        -h|--help)       usage; exit 0 ;;
        *) echo "fatal: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

# --- default concurrency ---
if [ -z "$CONCURRENCY" ]; then
    if command -v nproc >/dev/null 2>&1; then
        NPROC=$(nproc)
    else
        NPROC=$(sysctl -n hw.ncpu 2>/dev/null || echo 4)
    fi
    CONCURRENCY=$(( NPROC < 4 ? NPROC : 4 ))
fi

# Validate concurrency is a positive integer.
case "$CONCURRENCY" in
    ''|*[!0-9]*)
        echo "fatal: --concurrency must be a positive integer (got: '$CONCURRENCY')" >&2
        exit 2
        ;;
esac
if [ "$CONCURRENCY" -lt 1 ]; then
    echo "fatal: --concurrency must be >= 1" >&2
    exit 2
fi

# --- resolve config.toml path ---
# Without this, each case's ATOMCODE_HOME starts empty, atomcode can't find
# any providers, and falls back to a dummy "http://localhost:1" provider
# that guarantees all cases fail. We pass --config to atomcode so it loads
# the real provider config (API keys etc.) from outside the isolated home.
if [ "$CONFIG_USER_PROVIDED" = "0" ]; then
    DEFAULT_CONFIG="$HOME/.atomcode/config.toml"
    if [ -f "$DEFAULT_CONFIG" ]; then
        CONFIG_PATH="$DEFAULT_CONFIG"
    fi
    # If default doesn't exist, CONFIG_PATH stays empty → atomcode uses its
    # own fallback (dummy provider, LLM calls fail). User will see this in
    # the banner below.
fi
if [ -n "$CONFIG_PATH" ] && [ ! -f "$CONFIG_PATH" ]; then
    # Only fatal when the user explicitly provided the path — otherwise
    # we silently skip the default above.
    echo "fatal: --config file not found: $CONFIG_PATH" >&2
    exit 2
fi

# --- locate binary ---
if [ ! -f "$BIN" ] || [ ! -x "$BIN" ]; then
    echo "fatal: atomcode binary not found or not executable: $BIN" >&2
    echo "       hint: cargo build --release" >&2
    exit 2
fi
BIN_ABS=$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")

# --- fingerprint binary ---
VERSION_STRING=$("$TIMEOUT_BIN" 5 "$BIN" --version 2>/dev/null || echo "unknown")
BINARY_SHA256=$(sha256_of "$BIN_ABS")
BINARY_MTIME=$(python3 -c "
import os, sys
from datetime import datetime, timezone
t = os.path.getmtime(sys.argv[1])
print(datetime.fromtimestamp(t, timezone.utc).isoformat(timespec='seconds').replace('+00:00', 'Z'))
" "$BIN_ABS")

# --- freshness check (warn-only) ---
NEWEST_SRC=$(find "$PROJECT_ROOT/crates" -name '*.rs' -type f -newer "$BIN_ABS" 2>/dev/null | head -1)
if [ -n "$NEWEST_SRC" ]; then
    echo "warning: binary mtime ($BINARY_MTIME) is older than $NEWEST_SRC — did you forget to rebuild?" >&2
fi

# --- ATOMCODE_HOME capability probe ---
# We invoke the binary with a deliberately nonexistent provider, expecting it
# to walk past CLI parsing into run() and fail at config loading. Stderr must
# mention "Provider not found" (or similar). If the binary doesn't honor
# ATOMCODE_HOME (i.e. Task 1's change isn't present), it would fail at a
# different layer or use ~/.atomcode/ instead of our tmp dir, and the message
# wouldn't match.
#
# To force the provider-lookup code path (rather than the "no providers → dummy"
# fallback), we write a minimal config.toml with one real (but unreachable)
# provider into the probe home dir. This ensures Config::config_dir() reads
# PROBE_HOME/config.toml (not ~/.atomcode/config.toml), and the nonexistent
# probe provider fails with "Provider not found in config".
PROBE_HOME=$(mktemp -d /tmp/atomcode-eval-probe-XXXXXX)
PROBE_OUT=$(mktemp /tmp/atomcode-eval-probe-out-XXXXXX)
PROBE_ERR=$(mktemp /tmp/atomcode-eval-probe-err-XXXXXX)

# Trap any unexpected exit between here and the explicit cleanups below.
_probe_cleanup() { rm -rf "$PROBE_HOME" "$PROBE_OUT" "$PROBE_ERR"; }
trap _probe_cleanup EXIT

# Write a minimal valid config so the binary enters the provider-lookup branch.
cat > "$PROBE_HOME/config.toml" <<'TOML'
default_provider = "__eval_sentinel__"

[providers.__eval_sentinel__]
type = "openai"
model = "gpt-4"
api_key = "sk-probe-sentinel"
base_url = "http://localhost:1"
TOML

set +e
"$TIMEOUT_BIN" 5 env ATOMCODE_HOME="$PROBE_HOME" \
    "$BIN" --provider __nonexistent_eval_probe__ -p x \
    </dev/null > "$PROBE_OUT" 2> "$PROBE_ERR"
PROBE_RC=$?
set -e

if [ "$PROBE_RC" -eq 0 ]; then
    echo "fatal: ATOMCODE_HOME probe unexpectedly exited 0" >&2
    echo "       (expected non-zero from __nonexistent_eval_probe__ provider)" >&2
    cat "$PROBE_ERR" >&2
    rm -rf "$PROBE_HOME" "$PROBE_OUT" "$PROBE_ERR"
    exit 2
fi
if [ "$PROBE_RC" -eq 124 ]; then
    echo "fatal: ATOMCODE_HOME probe timed out after 5 seconds" >&2
    echo "       binary may be hung at startup; check for stale lock files" >&2
    rm -rf "$PROBE_HOME" "$PROBE_OUT" "$PROBE_ERR"
    exit 2
fi
if ! grep -qiE "provider.*(not found|missing|unknown)" "$PROBE_ERR"; then
    echo "fatal: ATOMCODE_HOME probe — binary doesn't seem to support ATOMCODE_HOME" >&2
    echo "       expected stderr to mention 'Provider not found' style error." >&2
    echo "       got: $(head -3 "$PROBE_ERR")" >&2
    rm -rf "$PROBE_HOME" "$PROBE_OUT" "$PROBE_ERR"
    exit 2
fi
rm -rf "$PROBE_HOME" "$PROBE_OUT" "$PROBE_ERR"
trap - EXIT

# --- create run dir ---
RUN_TS=$(python3 -c "
from datetime import datetime, timezone
print(datetime.now(timezone.utc).strftime('%Y-%m-%d_%H-%M-%S'))
")
RUN_DIR="$RUNS_DIR/$RUN_TS"
mkdir -p "$RUN_DIR"

# --- runner version (best effort) ---
RUNNER_GIT_HASH=$(cd "$PROJECT_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo "unknown")
RUNNER_DIRTY=""
if [ -n "$(cd "$PROJECT_ROOT" && git status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    RUNNER_DIRTY="+dirty"
fi
RUNNER_VERSION="eval/scripts/run.sh @ ${RUNNER_GIT_HASH}${RUNNER_DIRTY}"

STARTED_AT=$(python3 -c "
from datetime import datetime, timezone
print(datetime.now(timezone.utc).isoformat(timespec='seconds').replace('+00:00', 'Z'))
")

# --- write skeleton summary.json (will be updated at the end) ---
# Use sys.argv to pass values into the python heredoc — keeps the heredoc
# quoted and immune to shell injection via VERSION_STRING / BINARY_PATH.
python3 - "$RUN_DIR/summary.json" \
    "$STARTED_AT" "$VERSION_STRING" "$BIN_ABS" "$BINARY_SHA256" "$BINARY_MTIME" \
    "$RUNNER_VERSION" "$CONCURRENCY" <<'PYEOF'
import json
import sys

(_, out_path, started_at, version_string, bin_abs, sha256, bmtime,
 runner_version, concurrency) = sys.argv

out = {
    "started_at": started_at,
    "ended_at": None,
    "status": "in-progress",
    "atomcode": {
        "version_string": version_string,
        "binary_path": bin_abs,
        "binary_sha256": sha256,
        "binary_mtime": bmtime,
    },
    "runner_version": runner_version,
    "case_count": 0,
    "concurrency": int(concurrency),
    "totals": {
        "pass": 0, "fail": 0, "denied": 0, "timeout": 0,
        "cancelled": 0, "error": 0, "invalid": 0, "aborted": 0,
    },
}
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(out, f, indent=2)
PYEOF

echo "=== AtomCode Eval ==="
echo "  Binary      : $BIN_ABS"
echo "  Version     : $VERSION_STRING"
echo "  SHA256      : $BINARY_SHA256"
echo "  Run dir     : $RUN_DIR"
echo "  Concurrency : $CONCURRENCY"
[ -n "${PROVIDER_OVERRIDE:-}" ] && echo "  Provider    : $PROVIDER_OVERRIDE (override)"
if [ -n "$CONFIG_PATH" ]; then
    echo "  Config      : $CONFIG_PATH"
else
    echo "  Config      : (none — atomcode will fall back to its dummy provider; LLM calls will fail)"
fi
echo ""

# --- case discovery ---
echo "Discovering cases..."
CASE_JSONS=()  # array of single-line JSON outputs from parse_case.py
ALL_IDS=()     # plain ids for filtering and id-uniqueness check

discover_one() {
    local case_path="$1"
    local fallback_id="$2"
    local json
    json=$(python3 "$SCRIPT_DIR/parse_case.py" "$case_path")
    CASE_JSONS+=("$json")
    local id
    id=$(python3 -c "
import json, sys
d = json.loads(sys.argv[1])
fallback = sys.argv[2]
print(d.get('id') or fallback)
" "$json" "$fallback_id")
    ALL_IDS+=("$id")
}

if [ ! -d "$CASES_DIR" ]; then
    echo "fatal: cases dir not found: $CASES_DIR" >&2
    exit 2
fi

# Iterate cases dir in stable sorted order via python.
while IFS= read -r entry; do
    if [ -f "$entry" ] && case "$entry" in *.md) true;; *) false;; esac; then
        discover_one "$entry" "$(basename "$entry" .md)"
    elif [ -d "$entry" ] && [ -f "$entry/case.md" ]; then
        discover_one "$entry/case.md" "$(basename "$entry")"
    fi
done < <(python3 -c "
import os, sys
for n in sorted(os.listdir(sys.argv[1])):
    print(os.path.join(sys.argv[1], n))
" "$CASES_DIR")

# --- duplicate id check ---
if [ ${#ALL_IDS[@]} -gt 0 ]; then
    DUP=$(printf '%s\n' "${ALL_IDS[@]}" | sort | uniq -d | head -1 || true)
    if [ -n "$DUP" ]; then
        echo "fatal: duplicate case id: $DUP" >&2
        exit 2
    fi
fi

# --- apply --only / --only-pattern filters ---
SELECTED_JSONS=()
if [ ${#CASE_JSONS[@]} -gt 0 ]; then
    for json in "${CASE_JSONS[@]}"; do
        # Use same fallback logic as discover_one: id or path-derived name.
        id=$(python3 -c "
import json, os, sys
d = json.loads(sys.argv[1])
raw_id = d.get('id') or ''
if not raw_id:
    # fall back to the filename stem from case_path if present
    raw_id = os.path.splitext(os.path.basename(d.get('case_path', '')))[0]
print(raw_id)
" "$json")
        [ -z "$id" ] && continue

        keep=1
        if [ ${#ONLY_IDS[@]} -gt 0 ]; then
            keep=0
            for o in "${ONLY_IDS[@]}"; do
                if [ "$id" = "$o" ]; then
                    keep=1
                    break
                fi
            done
        fi
        if [ "$keep" = "1" ] && [ -n "$ONLY_PATTERN" ]; then
            if ! echo "$id" | grep -qE "$ONLY_PATTERN"; then
                keep=0
            fi
        fi
        [ "$keep" = "1" ] && SELECTED_JSONS+=("$json")
    done
fi

CASE_COUNT=${#SELECTED_JSONS[@]}
echo "  Discovered  : ${#ALL_IDS[@]}"
echo "  Selected    : $CASE_COUNT"
echo ""

if [ "$CASE_COUNT" = "0" ]; then
    echo "no cases selected — nothing to do"
fi

# --- write stub meta.json for invalid cases (so render_index sees them) ---
if [ ${#SELECTED_JSONS[@]} -gt 0 ]; then
    for json in "${SELECTED_JSONS[@]}"; do
        ok=$(python3 -c "
import json, sys
d = json.loads(sys.argv[1])
print('1' if d.get('ok') else '0')
" "$json")
        if [ "$ok" = "0" ]; then
            id=$(python3 -c "
import json, os, sys
d = json.loads(sys.argv[1])
print(d.get('id') or os.path.splitext(os.path.basename(d.get('case_path', 'unknown')))[0])
" "$json")
            mkdir -p "$RUN_DIR/$id"
            python3 - "$RUN_DIR/$id/meta.json" "$json" "$RUN_TS" "$id" <<'PYEOF'
import json, sys
out_path, case_json, run_id, fallback_id = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
case = json.loads(case_json)
meta = {
    "id": case.get("id") or fallback_id,
    "form": None,
    "provider": None,
    "exit_code": None,
    "wall_ms": None,
    "timed_out": False,
    "had_denial": False,
    "denial_count": 0,
    "started_at": None,
    "ended_at": None,
    "run_id": run_id,
    "status": "invalid",
    "errors": case.get("errors", []),
}
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(meta, f, indent=2)
PYEOF
        fi
    done
fi

# --- dispatch valid cases via xargs -P ---
DISPATCH_TMP=$(mktemp /tmp/atomcode-eval-dispatch-XXXXXX)
# Add to existing EXIT trap (Task 8 polish set up _probe_cleanup; we set
# our own here for the dispatch tmp file). EXIT also covers normal exit,
# while the INT trap below adds child-process cleanup on Ctrl-C.
trap 'rm -f "$DISPATCH_TMP"' EXIT TERM

# SIGINT handler: kill xargs (which propagates SIGTERM to its bash workers,
# which then die mid-case). The 5-second grace period gives workers time to
# write any partial meta.json before we SIGKILL them. Bash defers signal
# delivery during a trap handler, so we use 'sleep & wait' to keep the grace
# interruptible by a second Ctrl-C.
sigint_handler() {
    echo "" >&2
    echo "interrupted, killing xargs (which propagates to workers)..." >&2
    pkill -TERM -P $$ 2>/dev/null || true
    # Re-enable INT so a 2nd Ctrl-C during the grace period works immediately.
    trap - INT
    sleep 5 &
    wait $! 2>/dev/null || true
    pkill -KILL -P $$ 2>/dev/null || true
    # DISPATCH_TMP cleanup is handled by the EXIT trap.
    exit 130
}
trap sigint_handler INT

# Pack each valid case json into a base64 line so xargs can pass it cleanly.
if [ ${#SELECTED_JSONS[@]} -gt 0 ]; then
    for json in "${SELECTED_JSONS[@]}"; do
        ok=$(python3 -c "
import json, sys
d = json.loads(sys.argv[1])
print('1' if d.get('ok') else '0')
" "$json")
        if [ "$ok" = "1" ]; then
            # base64 the JSON and strip all newlines (portable: macOS + linux).
            printf '%s' "$json" | base64 | tr -d '\n' >> "$DISPATCH_TMP"
            printf '\n' >> "$DISPATCH_TMP"
        fi
    done
fi

VALID_COUNT=$(wc -l < "$DISPATCH_TMP" 2>/dev/null | tr -d ' ' || echo 0)
echo "Running $VALID_COUNT valid cases (concurrency=$CONCURRENCY)..."
echo ""

if [ "$VALID_COUNT" -gt 0 ]; then
    # Export all dispatch params as env vars so xargs bash -c stays short (avoids ARG_MAX).
    export _EVAL_BIN="$BIN_ABS"
    export _EVAL_RUN_DIR="$RUN_DIR"
    export _EVAL_PROVIDER_OVERRIDE="$PROVIDER_OVERRIDE"
    export _EVAL_CONFIG_PATH="$CONFIG_PATH"
    export _EVAL_RUN_ONE="$SCRIPT_DIR/run_one_case.sh"
    < "$DISPATCH_TMP" xargs -n1 -P "$CONCURRENCY" bash -c '
        set -u
        line="$1"
        json=$(printf "%s" "$line" | base64 --decode)
        export EVAL_BIN="$_EVAL_BIN"
        export EVAL_RUN_DIR="$_EVAL_RUN_DIR"
        export EVAL_CASE_JSON="$json"
        export EVAL_PROVIDER_OVERRIDE="$_EVAL_PROVIDER_OVERRIDE"
        export EVAL_CONFIG_PATH="$_EVAL_CONFIG_PATH"
        "$_EVAL_RUN_ONE"
    ' _
fi

# --- collect totals from per-case meta.json ---
TOTALS_JSON=$(python3 - "$RUN_DIR" <<'PYEOF'
import json, os, sys
run_dir = sys.argv[1]
# 'aborted' is reserved for future use — currently no code path produces it,
# but kept in the schema so render_index.py shows it in the totals row.
totals = {"pass": 0, "fail": 0, "denied": 0, "timeout": 0,
          "cancelled": 0, "error": 0, "invalid": 0, "aborted": 0}
for entry in sorted(os.listdir(run_dir)):
    p = os.path.join(run_dir, entry, "meta.json")
    if not os.path.isfile(p):
        continue
    try:
        meta = json.load(open(p))
    except Exception:
        continue
    s = meta.get("status", "error")
    if s in totals:
        totals[s] += 1
    else:
        totals["error"] += 1
print(json.dumps(totals))
PYEOF
)

# --- update summary.json (status: done) ---
ENDED_AT=$(python3 -c "
from datetime import datetime, timezone
print(datetime.now(timezone.utc).isoformat(timespec='seconds').replace('+00:00', 'Z'))
")
python3 - "$RUN_DIR/summary.json" "$ENDED_AT" "$CASE_COUNT" "$TOTALS_JSON" <<'PYEOF'
import json, sys
path, ended, count, totals_json = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
summary = json.load(open(path))
summary["ended_at"] = ended
summary["status"] = "done"
summary["case_count"] = count
totals = json.loads(totals_json)
for k in ("pass", "fail", "denied", "timeout", "cancelled", "error", "invalid", "aborted"):
    summary["totals"][k] = totals.get(k, 0)
with open(path, "w", encoding="utf-8") as f:
    json.dump(summary, f, indent=2)
PYEOF

# --- render index.html ---
python3 "$SCRIPT_DIR/render_index.py" "$RUN_DIR"

# --- final summary line ---
# Single python invocation reads all 5 totals from $TOTALS_JSON (already in scope).
read -r PASS FAIL DENIED TIMEOUT INVALID <<<"$(python3 -c "
import json, sys
t = json.loads(sys.argv[1])
print(t.get('pass', 0), t.get('fail', 0), t.get('denied', 0), t.get('timeout', 0), t.get('invalid', 0))
" "$TOTALS_JSON")"

echo ""
echo "eval done: $CASE_COUNT cases, $PASS pass, $FAIL fail, $DENIED denied, $TIMEOUT timeout, $INVALID invalid"
echo "-> $RUN_DIR/index.html"

# Runner itself always exits 0; CI tooling should jq summary.json for
# pass-rate-based exit codes.
exit 0
