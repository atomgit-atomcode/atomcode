#!/usr/bin/env bash
# Per-instance SWE-bench worker. Invoked by run.sh via xargs -P.
#
# Required env:
#   EVAL_RUN_DIR              eval/runs/<ts>/  (run root)
#   EVAL_INSTANCE_JSON_B64    base64-encoded single-instance JSON blob
#                             (base64 avoids shell-escaping hell for
#                              problem_statement with quotes/newlines)
#   EVAL_BIN                  path to atomcode binary
#   EVAL_CONFIG_PATH          --config value (atomcode provider config)
#   EVAL_REPO_CACHE           ~/.cache/atomcode-eval/swebench/repos
#   EVAL_PROMPT_TEMPLATE      prompts/<name>.md name (default: "default")
#   EVAL_INCLUDE_HINTS        "true" or "false"
#   EVAL_TIMEOUT_SECS         int, atomcode timeout
#   EVAL_MAX_TURNS            int, atomcode --max-turns value
#   EVAL_PROVIDER             provider name (for pricing lookup)
#
# Exit code: always 0 unless the runner itself is fundamentally broken.
# Per-instance outcomes are reflected in meta.json.status.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Source the Form A/B runner's portable shim for TIMEOUT_BIN.
# This is the ONE cross-subsystem reference allowed — documented in spec §5.
. "$SCRIPT_DIR/../scripts/lib/portable.sh"

# --- guard: required env vars ---
for var in EVAL_RUN_DIR EVAL_INSTANCE_JSON_B64 EVAL_BIN EVAL_REPO_CACHE; do
    if [ -z "${!var:-}" ]; then
        echo "fatal: $var is not set" >&2
        exit 1
    fi
done

# ---------------------------------------------------------------------------
# Helper: write a minimal error meta.json and exit 0 (the error is carried
# by meta.json.status and the runner keeps going for other instances).
# ---------------------------------------------------------------------------
write_error_meta() {
    local status="$1"
    local message="$2"
    local now
    now=$(python3 -c 'from datetime import datetime, timezone; print(datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00","Z"))')
    python3 - "$CASE_DIR/meta.json" "$INSTANCE_ID" "$status" "$message" "$now" "$REPO" "$BASE_COMMIT" <<'PYEOF'
import json, sys
(_, out, iid, status, msg, ts, repo, sha) = sys.argv
meta = {
    "id": iid,
    "form": "swebench",
    "provider": "",
    "exit_code": -1,
    "wall_ms": 0,
    "timed_out": False,
    "had_denial": False,
    "denial_count": 0,
    "started_at": ts,
    "ended_at": ts,
    "run_id": "",
    "status": status,
    "errors": [msg],
    "swebench": {
        "repo": repo,
        "base_commit": sha,
        "prompt_template": "",
        "include_hints": False,
        "dataset_revision": "",
        "patch_size_bytes": 0,
    },
}
with open(out, "w", encoding="utf-8") as f:
    json.dump(meta, f, indent=2)
PYEOF
    echo "[instance $INSTANCE_ID] failed: $status — $message" >&2
}

# --- decode instance JSON ---
INSTANCE_JSON=$(printf '%s' "$EVAL_INSTANCE_JSON_B64" | base64 -d)
if [ -z "$INSTANCE_JSON" ]; then
    echo "fatal: EVAL_INSTANCE_JSON_B64 decoded to empty" >&2
    exit 1
fi

# --- helper: read a scalar field from INSTANCE_JSON ---
read_field() {
    local field="$1"
    python3 -c '
import json, sys
d = json.loads(sys.argv[1])
v = d.get(sys.argv[2])
if v is None:
    print("")
else:
    print(v, end="")
' "$INSTANCE_JSON" "$field"
}

INSTANCE_ID=$(read_field instance_id)
REPO=$(read_field repo)
BASE_COMMIT=$(read_field base_commit)

if [ -z "$INSTANCE_ID" ] || [ -z "$REPO" ] || [ -z "$BASE_COMMIT" ]; then
    echo "fatal: instance JSON missing required fields" >&2
    exit 1
fi

# Normalize repo name to a safe dir name: "django/django" → "django__django"
REPO_DIR=$(printf '%s' "$REPO" | tr '/' '_' | tr '_' '_' | sed 's|__|__|g')
# Actually simpler: use the SWE-bench convention of {owner}__{name}
REPO_DIR="${REPO//\//__}"

CASE_DIR="$EVAL_RUN_DIR/$INSTANCE_ID"
CWD_DIR="$CASE_DIR/cwd"
HOME_DIR="$CASE_DIR/home"

mkdir -p "$CASE_DIR" "$CWD_DIR" "$HOME_DIR"

echo "[instance $INSTANCE_ID] repo=$REPO base=${BASE_COMMIT:0:8}" >&2

# ---------------------------------------------------------------------------
# Bare cache population with flock serialization
# ---------------------------------------------------------------------------
mkdir -p "$EVAL_REPO_CACHE"
BARE_REPO="$EVAL_REPO_CACHE/$REPO_DIR.git"

if [ ! -d "$BARE_REPO" ]; then
    # Serialize concurrent populators: only one run_one_instance.sh at a
    # time may populate the same bare repo. Other workers block on the
    # flock and then find the repo already present on second check.
    (
        flock -x 9
        if [ ! -d "$BARE_REPO" ]; then
            echo "[instance $INSTANCE_ID] first-time bare clone: $REPO" >&2
            if ! git clone --bare "https://github.com/$REPO.git" "$BARE_REPO" 2>>"$CASE_DIR/clone.log"; then
                echo "[instance $INSTANCE_ID] bare clone failed, see $CASE_DIR/clone.log" >&2
                exit 1
            fi
            # Disable auto-gc permanently — see spec §12, GC breaks alternate refs.
            git -C "$BARE_REPO" config gc.auto 0
        fi
    ) 9>"$EVAL_REPO_CACHE/.lock"
    if [ $? -ne 0 ]; then
        write_error_meta "bare_clone_failed" "first-time bare clone failed"
        exit 0
    fi
fi

# ---------------------------------------------------------------------------
# Clone working tree from bare cache + checkout base_commit
# ---------------------------------------------------------------------------
# --local --shared: hard-links objects from bare cache via .git/objects/info/alternates.
# Each per-instance cwd is ~50MB (working tree + tiny index), not 500MB.
if ! git clone --local --shared --quiet "$BARE_REPO" "$CWD_DIR" 2>>"$CASE_DIR/clone.log"; then
    write_error_meta "clone_from_cache_failed" "git clone --local --shared failed"
    exit 0
fi

if ! git -C "$CWD_DIR" checkout --quiet "$BASE_COMMIT" 2>>"$CASE_DIR/clone.log"; then
    write_error_meta "checkout_failed" "could not checkout $BASE_COMMIT"
    exit 0
fi

if ! git -C "$CWD_DIR" clean -fdx --quiet 2>>"$CASE_DIR/clone.log"; then
    write_error_meta "clean_failed" "git clean -fdx failed"
    exit 0
fi
