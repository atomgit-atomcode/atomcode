#!/usr/bin/env bash
# Fake atomcode for SWE-bench smoke tests. Unlike the Form A/B fake,
# this one:
#   - Accepts --max-turns / --config arguments
#   - Accepts --prompt-file (reads prompt from file — post-hardening)
#   - Emits stderr in the real atomcode -v format: [tokens], [tool→],
#     [tool←], [done] with optional `stopped=` suffix
#   - Writes a small file modification so `git diff` is non-empty
#   - Keys behavior off prompt content for test scenarios:
#       * prompt contains "FAKE_NATURAL"     → normal success (turns=2)
#       * prompt contains "FAKE_TURN_LIMIT"  → emit stopped=turn_limit
#       * prompt contains "FAKE_FAIL"        → exit 1
#       * prompt contains "FAKE_DENY"        → emit [approval-denied]
#       * prompt contains "FAKE_TIMEOUT"     → sleep 600

set -u

if [ "${1:-}" = "--version" ]; then
    echo "atomcode 0.0.0-fake-swebench (fake5678)"
    exit 0
fi

prompt=""
prompt_file=""
provider=""
max_turns=""
config=""
verbose=0
while [ $# -gt 0 ]; do
    case "$1" in
        --provider)    provider="$2"; shift 2 ;;
        --config)      config="$2"; shift 2 ;;
        --max-turns)   max_turns="$2"; shift 2 ;;
        --prompt-file) prompt_file="$2"; shift 2 ;;
        -v)            verbose=1; shift ;;
        -p)            prompt="$2"; shift 2 ;;
        *)             shift ;;
    esac
done

# If --prompt-file given and -p not, load prompt from file
if [ -z "$prompt" ] && [ -n "$prompt_file" ] && [ -f "$prompt_file" ]; then
    prompt=$(cat "$prompt_file")
fi

emit_tokens()  { [ "$verbose" = "1" ] && echo "[tokens] prompt=$1 completion=$2" >&2; }
emit_tool_start() { [ "$verbose" = "1" ] && echo "[tool→ $1 args={\"fake\":true}]" >&2; }
emit_tool_end()   { [ "$verbose" = "1" ] && echo "[tool← $1 OK 5ms]" >&2; }
emit_done() {
    [ "$verbose" = "1" ] || return 0
    local turns="$1" tool_calls="$2" tokens="$3" stopped="$4"
    if [ -n "$stopped" ]; then
        echo "[done] 2.5s tokens=$tokens turns=$turns tool_calls=$tool_calls stopped=$stopped" >&2
    else
        echo "[done] 2.5s tokens=$tokens turns=$turns tool_calls=$tool_calls" >&2
    fi
}

case "$prompt" in
    *FAKE_TIMEOUT*)
        sleep 600
        ;;
    *FAKE_FAIL*)
        emit_tokens 1000 50
        echo "fake failure mode" >&2
        exit 1
        ;;
    *FAKE_DENY*)
        emit_tokens 1000 50
        echo "[approval-denied] tool=bash reason=rm -rf detected" >&2
        exit 2
        ;;
    *FAKE_TURN_LIMIT*)
        # Simulate hitting --max-turns
        emit_tokens 1500 200
        emit_tool_start "read_file"
        emit_tool_end "read_file"
        emit_tokens 2000 150
        emit_tool_start "edit_file"
        emit_tool_end "edit_file"
        # Actually modify a file so git diff is non-empty
        if [ -f README.md ]; then
            echo "" >> README.md
            echo "# FAKE_TURN_LIMIT modification" >> README.md
        elif [ -f README.rst ]; then
            echo "" >> README.rst
            echo ".. FAKE_TURN_LIMIT modification" >> README.rst
        else
            echo "fake modification" > fake_modified_by_swebench_test.txt
        fi
        echo "I made some progress but ran out of turns."
        emit_done 2 2 3850 "turn_limit"
        exit 0
        ;;
    *FAKE_NATURAL*|*)
        # Default: normal success path
        emit_tokens 1200 180
        emit_tool_start "list_directory"
        emit_tool_end "list_directory"
        emit_tokens 1800 100
        emit_tool_start "read_file"
        emit_tool_end "read_file"
        emit_tokens 2100 250
        emit_tool_start "edit_file"
        emit_tool_end "edit_file"
        # Actually modify a file so git diff produces a real patch
        if [ -f README.md ]; then
            sed -i.bak 's/^# /# FAKE_SWEBENCH_EDIT: /' README.md 2>/dev/null || \
                printf '# FAKE_SWEBENCH_EDIT\n' > README.md
            rm -f README.md.bak
        else
            echo "fake modification" > fake_modified.txt
        fi
        echo "Fixed the issue by editing the README."
        emit_done 2 3 5430 ""
        exit 0
        ;;
esac
