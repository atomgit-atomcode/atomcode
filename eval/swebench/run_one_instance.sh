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

# Preflight: flock is required for bare-cache serialization.
# On macOS: brew install flock
command -v flock >/dev/null 2>&1 || { echo "fatal: flock required (on macOS: brew install flock)" >&2; exit 1; }

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

# Validate existing bare repo: must have a resolvable HEAD. A half-cloned
# stub (warm-cache killed mid-fetch) passes `[ -d ]` but `git rev-parse HEAD`
# fails — delete it so we re-clone cleanly.
if [ -d "$BARE_REPO" ] && ! git -C "$BARE_REPO" rev-parse --quiet --verify HEAD >/dev/null 2>&1; then
    echo "[instance $INSTANCE_ID] stale/corrupt bare cache, removing: $BARE_REPO" >&2
    rm -rf "$BARE_REPO"
fi

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
                rm -rf "$BARE_REPO"  # don't leave a corrupt stub for the next run
                exit 1
            fi
        fi
    ) 9>"$EVAL_REPO_CACHE/.lock"
    if [ $? -ne 0 ]; then
        write_error_meta "bare_clone_failed" "first-time bare clone failed"
        exit 0
    fi
fi
# Assert gc.auto 0 on every worker run (idempotent; protects operator-seeded caches too).
git -C "$BARE_REPO" config gc.auto 0 2>/dev/null || true

# ---------------------------------------------------------------------------
# Clone working tree from bare cache + checkout base_commit
# ---------------------------------------------------------------------------
# --local --shared: hard-links objects from bare cache via .git/objects/info/alternates.
# Each per-instance cwd is ~50MB (working tree + tiny index), not 500MB.
# Wipe any stale cwd from a previous failed attempt on the same instance
# (resume path). git clone refuses non-empty targets.
rm -rf "$CWD_DIR"
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

# ---------------------------------------------------------------------------
# Render prompt
# ---------------------------------------------------------------------------
PROMPT_TEMPLATE="${EVAL_PROMPT_TEMPLATE:-default}"
INCLUDE_HINTS_FLAG="--no-include-hints"
if [ "${EVAL_INCLUDE_HINTS:-false}" = "true" ]; then
    INCLUDE_HINTS_FLAG="--include-hints"
fi

PROMPT_FILE="$CASE_DIR/prompt.rendered.txt"
if ! printf '%s' "$INSTANCE_JSON" | python3 "$SCRIPT_DIR/render_prompt.py" \
     --template "$PROMPT_TEMPLATE" \
     "$INCLUDE_HINTS_FLAG" \
     > "$PROMPT_FILE" 2> "$CASE_DIR/render.err"; then
    write_error_meta "prompt_render_failed" "$(cat "$CASE_DIR/render.err")"
    exit 0
fi

# Save the raw "case source" (instance JSON) too — analogous to Form A/B's prompt.md
printf '%s\n' "$INSTANCE_JSON" > "$CASE_DIR/prompt.md"

# ---------------------------------------------------------------------------
# Invoke atomcode
# ---------------------------------------------------------------------------
START_MS=$(python3 -c 'import time; print(int(time.time()*1000))')
START_ISO=$(python3 -c 'from datetime import datetime, timezone; print(datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00","Z"))')

# Build args: -v for trace, --max-turns for turn cap, --config for real provider,
# --provider if EVAL_PROVIDER is set.
atomcode_args=(
    -v
    --max-turns "${EVAL_MAX_TURNS:-30}"
    --prompt-file "$PROMPT_FILE"
)
if [ -n "${EVAL_CONFIG_PATH:-}" ]; then
    atomcode_args=(--config "$EVAL_CONFIG_PATH" "${atomcode_args[@]}")
fi
if [ -n "${EVAL_PROVIDER:-}" ]; then
    atomcode_args=(--provider "$EVAL_PROVIDER" "${atomcode_args[@]}")
fi

set +e
(
    cd "$CWD_DIR" || exit 1
    export ATOMCODE_HOME="$HOME_DIR"
    "$TIMEOUT_BIN" "${EVAL_TIMEOUT_SECS:-600}" "$EVAL_BIN" \
        "${atomcode_args[@]}" \
        </dev/null \
        > "$CASE_DIR/stdout.txt" 2> "$CASE_DIR/stderr.txt"
)
EXIT_CODE=$?
set -e

END_MS=$(python3 -c 'import time; print(int(time.time()*1000))')
END_ISO=$(python3 -c 'from datetime import datetime, timezone; print(datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00","Z"))')
WALL_MS=$((END_MS - START_MS))

# ---------------------------------------------------------------------------
# Capture patch
# ---------------------------------------------------------------------------
git -C "$CWD_DIR" diff > "$CASE_DIR/patch.diff" 2>/dev/null || true
PATCH_SIZE=$(wc -c < "$CASE_DIR/patch.diff" | tr -d ' ')

# ---------------------------------------------------------------------------
# Parse stderr for efficiency metrics
# ---------------------------------------------------------------------------
# We use a Python helper to read stderr.txt and extract:
#   - turns (from [done] line)
#   - prompt_tokens / completion_tokens (sum of all [tokens] lines)
#   - tool_calls (count of [tool→] lines)
#   - tool_breakdown (count per tool name)
#   - stop_reason (from [done] line's optional `stopped=<tag>` suffix; default "natural")
#   - denial_count (count of [approval-denied] lines)

METRICS_JSON=$(python3 - "$CASE_DIR/stderr.txt" <<'PYEOF'
import json, re, sys
from collections import Counter

path = sys.argv[1]
try:
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        lines = f.readlines()
except FileNotFoundError:
    lines = []

prompt_tokens = 0
completion_tokens = 0
tool_counts = Counter()
turns = 0
stop_reason = "natural"
denial_count = 0

# [tokens] prompt=N completion=M
re_tokens = re.compile(r"\[tokens\] prompt=(\d+) completion=(\d+)")
# [tool→ name args=...]
re_tool = re.compile(r"\[tool→ (\w+)")
# [done] <d>s tokens=N turns=M tool_calls=K [stopped=<tag>]
re_done = re.compile(r"\[done\].*turns=(\d+).*tool_calls=(\d+)(?:.*stopped=(\w+))?")
# [approval-denied]
re_denied = re.compile(r"\[approval-denied\]")

for line in lines:
    m = re_tokens.search(line)
    if m:
        prompt_tokens += int(m.group(1))
        completion_tokens += int(m.group(2))
        continue
    m = re_tool.search(line)
    if m:
        tool_counts[m.group(1)] += 1
        continue
    m = re_done.search(line)
    if m:
        turns = int(m.group(1))
        if m.group(3):
            stop_reason = m.group(3)
        continue
    if re_denied.search(line):
        denial_count += 1

json.dump({
    "turns": turns,
    "prompt_tokens": prompt_tokens,
    "completion_tokens": completion_tokens,
    "tool_calls": sum(tool_counts.values()),
    "tool_breakdown": dict(tool_counts),
    "stop_reason": stop_reason,
    "denial_count": denial_count,
}, sys.stdout)
PYEOF
)

# Extract denial_count for meta.json.had_denial (Form A/B compat)
DENIAL_COUNT=$(printf '%s' "$METRICS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["denial_count"])')
HAD_DENIAL="false"
if [ "$DENIAL_COUNT" -gt 0 ]; then
    HAD_DENIAL="true"
fi

# ---------------------------------------------------------------------------
# Cost estimation
# ---------------------------------------------------------------------------
PROMPT_TOK=$(printf '%s' "$METRICS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["prompt_tokens"])')
COMPLETION_TOK=$(printf '%s' "$METRICS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["completion_tokens"])')
ESTIMATED_COST=$(python3 "$SCRIPT_DIR/pricing.py" "${EVAL_PROVIDER:-siliconflow}" "$PROMPT_TOK" "$COMPLETION_TOK" 2>/dev/null || echo "0.000000")

# ---------------------------------------------------------------------------
# Derive status
# ---------------------------------------------------------------------------
case "$EXIT_CODE" in
    0)
        if [ "$HAD_DENIAL" = "true" ]; then
            STATUS="denied"
        else
            STATUS="predicted"
        fi
        ;;
    1)   STATUS="fail" ;;
    2)   STATUS="denied" ;;
    124) STATUS="timeout" ;;
    130) STATUS="cancelled" ;;
    *)   STATUS="error" ;;
esac

RUN_ID="$(basename "$EVAL_RUN_DIR")"

# ---------------------------------------------------------------------------
# Write meta.json
# ---------------------------------------------------------------------------
python3 - \
    "$CASE_DIR/meta.json" \
    "$INSTANCE_ID" "$STATUS" "$EXIT_CODE" "$WALL_MS" \
    "$HAD_DENIAL" "$DENIAL_COUNT" "$START_ISO" "$END_ISO" "$RUN_ID" \
    "$REPO" "$BASE_COMMIT" "$PROMPT_TEMPLATE" "${EVAL_INCLUDE_HINTS:-false}" \
    "${EVAL_DATASET_REVISION:-}" "$PATCH_SIZE" "${EVAL_PROVIDER:-}" \
    "$ESTIMATED_COST" "$METRICS_JSON" <<'PYEOF'
import json, sys

(_, out, iid, status, ec, wms, denial_str, dc, start, end, run,
 repo, sha, template, include_hints_str, dataset_rev, patch_size, provider,
 cost_str, metrics_json) = sys.argv

metrics = json.loads(metrics_json)

meta = {
    "id": iid,
    "form": "swebench",
    "provider": provider,
    "exit_code": int(ec),
    "wall_ms": int(wms),
    "timed_out": int(ec) == 124,
    "had_denial": denial_str == "true",
    "denial_count": int(dc),
    "started_at": start,
    "ended_at": end,
    "run_id": run,
    "status": status,
    "swebench": {
        "repo": repo,
        "base_commit": sha,
        "prompt_template": template,
        "include_hints": include_hints_str == "true",
        "dataset_revision": dataset_rev,
        "patch_size_bytes": int(patch_size),
    },
    "efficiency": {
        "turns": metrics["turns"],
        "prompt_tokens": metrics["prompt_tokens"],
        "completion_tokens": metrics["completion_tokens"],
        "tool_calls": metrics["tool_calls"],
        "tool_breakdown": metrics["tool_breakdown"],
        "stop_reason": metrics["stop_reason"],
        "estimated_cost_usd": float(cost_str),
    },
}
with open(out, "w", encoding="utf-8") as f:
    json.dump(meta, f, indent=2)
PYEOF

# ---------------------------------------------------------------------------
# Per-case prediction (run.sh concatenates these at end of predict phase)
# ---------------------------------------------------------------------------
# Quote EVAL_BIN to handle paths with spaces.
ATOMCODE_VERSION=$("$EVAL_BIN" --version 2>/dev/null | head -1 | awk '{print $2}')
MODEL_NAME="atomcode-${ATOMCODE_VERSION:-unknown}-${EVAL_PROVIDER:-default}-${PROMPT_TEMPLATE}"

# Pass patch via file path so we don't hit ARG_MAX on big patches (>256KB on macOS).
python3 - "$CASE_DIR/prediction.json" "$INSTANCE_ID" "$MODEL_NAME" "$CASE_DIR/patch.diff" <<'PYEOF'
import json, sys
(_, out, iid, model, patch_path) = sys.argv
with open(patch_path, "r", encoding="utf-8", errors="replace") as f:
    patch = f.read()
pred = {
    "instance_id": iid,
    "model_name_or_path": model,
    "model_patch": patch,
}
with open(out, "w", encoding="utf-8") as f:
    f.write(json.dumps(pred) + "\n")
PYEOF

# ---------------------------------------------------------------------------
# Cwd compression: remove .git/objects alternates so cwd is self-contained
# for post-mortem inspection, then strip .git/objects and .git/logs to save disk.
# See spec §15.9.
# ---------------------------------------------------------------------------
if [ "$STATUS" != "error" ]; then
    # Preserve HEAD and refs/heads/main (if it exists) for human debugging.
    # The actual git history isn't needed — we have patch.diff.
    rm -rf "$CWD_DIR/.git/objects" "$CWD_DIR/.git/logs" 2>/dev/null || true
    # Remove alternates file so the bare cache can eventually be gc'd if needed.
    rm -f "$CWD_DIR/.git/objects/info/alternates" 2>/dev/null || true
fi

echo "[instance $INSTANCE_ID] done status=$STATUS wall=${WALL_MS}ms turns=$(printf '%s' "$METRICS_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["turns"])')" >&2
exit 0
