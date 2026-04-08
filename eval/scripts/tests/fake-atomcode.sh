#!/usr/bin/env bash
# Fake atomcode binary for runner acceptance testing.
# Supports: --version, ATOMCODE_HOME probe, basic --provider X -v -p Y
#
# Behaviors keyed off the prompt body:
#   - if prompt contains "fail-please" → exit 1
#   - if prompt contains "deny-please" → emit [approval-denied] line, exit 2
#   - otherwise → write a marker file in cwd, echo a reply, exit 0

set -u

if [ "${1:-}" = "--version" ]; then
    echo "atomcode 0.0.0-fake (fake1234)"
    exit 0
fi

provider=""
prompt=""
verbose=0
while [ $# -gt 0 ]; do
    case "$1" in
        --provider) provider="$2"; shift 2 ;;
        -v)         verbose=1; shift ;;
        -p)         prompt="$2"; shift 2 ;;
        *)          shift ;;
    esac
done

# ATOMCODE_HOME probe contract: when invoked with the magic provider name,
# emit a "Provider not found" stderr message so run.sh's pre-flight probe
# can verify the binary respects ATOMCODE_HOME and walks into config loading.
if [ "$provider" = "__nonexistent_eval_probe__" ]; then
    echo "Error: Provider '$provider' not found in config" >&2
    exit 1
fi

# Make sure ATOMCODE_HOME is honored — write a marker into $home/logs/
# so the smoke test can verify isolation worked.
mkdir -p "${ATOMCODE_HOME:-/tmp}/logs"
echo "{\"fake\": true, \"provider\": \"$provider\"}" > "${ATOMCODE_HOME}/logs/fake-request.json"

# Behavior switches based on prompt content
if echo "$prompt" | grep -q 'fail-please'; then
    echo "fake fail" >&2
    exit 1
fi
if echo "$prompt" | grep -q 'deny-please'; then
    echo "[approval-denied] tool=bash reason=fake" >&2
    echo "fake reply (denied)"
    exit 2
fi

# Default: success — write a marker file in cwd, echo a reply
echo "fake reply for: $prompt"
echo "marker" > marker.txt
exit 0
