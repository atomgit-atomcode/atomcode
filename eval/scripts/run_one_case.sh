#!/usr/bin/env bash
# Run a single eval case. Invoked by run.sh via xargs -P.
#
# Required env:
#   EVAL_BIN              path to atomcode binary
#   EVAL_RUN_DIR          runs/<ts>/ directory
#   EVAL_CASE_JSON        single-line JSON output of parse_case.py
#   EVAL_PROVIDER_OVERRIDE   optional, overrides case provider (may be empty)
#   EVAL_CONFIG_PATH      optional, path to atomcode config.toml (passed as
#                         --config so atomcode finds real providers without
#                         requiring a copy in each case's isolated home dir).
#                         May be empty — atomcode falls back to dummy provider.
#
# This script always exits 0 (unless the environment is fundamentally
# broken). Per-case failures are reflected in meta.json status.

# Source portable.sh for TIMEOUT_BIN.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/lib/portable.sh"

# Strict mode AFTER sourcing portable.sh (which already enables set -u).
set -e

# --- guard: required env vars ---
if [ -z "${EVAL_BIN:-}" ]; then
    echo "fatal: EVAL_BIN is not set" >&2
    exit 1
fi
if [ ! -x "${EVAL_BIN}" ]; then
    echo "fatal: EVAL_BIN '${EVAL_BIN}' is not executable" >&2
    exit 1
fi
if [ -z "${EVAL_RUN_DIR:-}" ]; then
    echo "fatal: EVAL_RUN_DIR is not set" >&2
    exit 1
fi
if [ -z "${EVAL_CASE_JSON:-}" ]; then
    echo "fatal: EVAL_CASE_JSON is not set" >&2
    exit 1
fi

# --- helper: read a top-level field from EVAL_CASE_JSON ---
# Wraps python3 -c so we can pull scalar fields without jq dependency.
read_field() {
    local field="$1"
    python3 -c '
import json, sys
d = json.loads(sys.argv[1])
key = sys.argv[2]
v = d.get(key)
if v is None:
    print("")
elif isinstance(v, (list, dict)):
    print(json.dumps(v))
else:
    print(v)
' "$EVAL_CASE_JSON" "$field"
}

CASE_ID=$(read_field id)
FORM=$(read_field form)
PROVIDER=$(read_field provider)
TIMEOUT_SECS=$(read_field timeout_secs)
CASE_PATH=$(read_field case_path)
SEED_DIR=$(read_field seed_dir)

# prompt_body needs special handling because it can have newlines
PROMPT_BODY=$(python3 -c "
import json, sys
print(json.loads(sys.argv[1]).get('prompt_body', ''), end='')
" "$EVAL_CASE_JSON")

# seed_files is a JSON object — pass through as JSON for the populate step
SEED_FILES_JSON=$(python3 -c "
import json, sys
print(json.dumps(json.loads(sys.argv[1]).get('seed_files', {})))
" "$EVAL_CASE_JSON")

# --- provider override ---
if [ -n "${EVAL_PROVIDER_OVERRIDE:-}" ]; then
    PROVIDER="$EVAL_PROVIDER_OVERRIDE"
fi

# --- create case directory ---
CASE_DIR="$EVAL_RUN_DIR/$CASE_ID"
mkdir -p "$CASE_DIR/cwd" "$CASE_DIR/home"

# --- copy prompt.md (the original case file) ---
# Tolerate missing/unreadable CASE_PATH so we always write meta.json. If
# the case file vanished between parse and run, we still want a valid
# case dir for render_index.py to display.
if ! cp "$CASE_PATH" "$CASE_DIR/prompt.md" 2>/dev/null; then
    echo "warning: could not copy CASE_PATH=$CASE_PATH; writing empty prompt.md" >&2
    : > "$CASE_DIR/prompt.md"
fi

# --- populate cwd ---
if [ "$FORM" = "B" ] && [ -n "$SEED_DIR" ]; then
    # Form B: copy seed/ contents into cwd/. The trailing /. is critical:
    # it copies the *contents* of seed/, not the seed dir itself.
    cp -R "$SEED_DIR/." "$CASE_DIR/cwd/"
elif [ "$FORM" = "A" ]; then
    # Form A: write inline seed_files to cwd/<key> using a python helper
    python3 - "$CASE_DIR/cwd" "$SEED_FILES_JSON" <<'PYEOF'
import json, os, sys
target = sys.argv[1]
files = json.loads(sys.argv[2])
for path, content in files.items():
    full = os.path.join(target, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w", encoding="utf-8") as f:
        f.write(content)
PYEOF
fi

# --- write prompt body to a tmp file (avoid argv escaping nightmares) ---
PROMPT_TMP=$(mktemp /tmp/atomcode-eval-prompt-XXXXXX)
trap 'rm -f "$PROMPT_TMP"' EXIT
printf '%s' "$PROMPT_BODY" > "$PROMPT_TMP"

# --- turn off set -e for the atomcode run so we can capture exit code ---
set +e

# --- run atomcode ---
START_MS=$(python3 -c 'import time; print(int(time.time()*1000))')
START_ISO=$(python3 -c '
from datetime import datetime, timezone
print(datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"))
')

(
    cd "$CASE_DIR/cwd" || exit 1
    export ATOMCODE_HOME="$CASE_DIR/home"
    # Build atomcode args. --provider and --config are both conditional:
    # - --provider: only when PROVIDER is non-empty (otherwise atomcode
    #   uses config.toml's default_provider)
    # - --config: only when EVAL_CONFIG_PATH is non-empty (otherwise
    #   atomcode uses its fallback)
    atomcode_args=(-v -p "$(cat "$PROMPT_TMP")")
    if [ -n "$PROVIDER" ]; then
        atomcode_args=(--provider "$PROVIDER" "${atomcode_args[@]}")
    fi
    if [ -n "${EVAL_CONFIG_PATH:-}" ]; then
        atomcode_args=(--config "$EVAL_CONFIG_PATH" "${atomcode_args[@]}")
    fi
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" "$EVAL_BIN" \
        "${atomcode_args[@]}" \
        </dev/null \
        > "$CASE_DIR/stdout.txt" 2> "$CASE_DIR/stderr.txt"
)
EXIT_CODE=$?

END_MS=$(python3 -c 'import time; print(int(time.time()*1000))')
END_ISO=$(python3 -c '
from datetime import datetime, timezone
print(datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"))
')
WALL_MS=$((END_MS - START_MS))

# --- count [approval-denied] lines in stderr ---
# grep -c exits 1 when no lines match (count=0) — use a subshell + always-true to capture cleanly.
if [ -f "$CASE_DIR/stderr.txt" ]; then
    DENIAL_COUNT=$(grep -c '\[approval-denied\]' "$CASE_DIR/stderr.txt" || true)
else
    DENIAL_COUNT=0
fi
# Strip any surrounding whitespace/newlines from grep -c output.
DENIAL_COUNT=$(printf '%s' "$DENIAL_COUNT" | tr -d '[:space:]')
# Ensure it's a valid integer; fall back to 0 if empty.
case "$DENIAL_COUNT" in
    ''|*[!0-9]*) DENIAL_COUNT=0 ;;
esac
if [ "$DENIAL_COUNT" -gt 0 ]; then
    HAD_DENIAL="true"
else
    HAD_DENIAL="false"
fi

# --- derive status ---
case "$EXIT_CODE" in
    0)
        if [ "$HAD_DENIAL" = "true" ]; then
            STATUS="denied"
        else
            STATUS="pass"
        fi
        ;;
    1)   STATUS="fail" ;;
    2)   STATUS="denied" ;;
    124) STATUS="timeout" ;;
    130) STATUS="cancelled" ;;
    *)   STATUS="error" ;;
esac

RUN_ID="$(basename "$EVAL_RUN_DIR")"

# --- write meta.json (use sys.argv to avoid Python source injection) ---
python3 - "$CASE_DIR/meta.json" \
    "$CASE_ID" "$FORM" "$PROVIDER" \
    "$EXIT_CODE" "$WALL_MS" \
    "$HAD_DENIAL" "$DENIAL_COUNT" \
    "$START_ISO" "$END_ISO" "$RUN_ID" "$STATUS" "$EXIT_CODE" <<'PYEOF'
import json
import sys

(_, out_path, cid, form, prov, ec, wms, denial_str, dc, start, end, run, status, ec_again) = sys.argv

meta = {
    "id": cid,
    "form": form,
    "provider": prov,
    "exit_code": int(ec),
    "wall_ms": int(wms),
    "timed_out": int(ec_again) == 124,
    "had_denial": denial_str == "true",
    "denial_count": int(dc),
    "started_at": start,
    "ended_at": end,
    "run_id": run,
    "status": status,
}
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(meta, f, indent=2)
PYEOF

exit 0
