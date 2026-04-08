#!/usr/bin/env bash
# Cross-platform tool detection. Source this from any eval script:
#
#     # Robust source pattern (use $BASH_SOURCE not $0 — $0 is wrong when
#     # the *caller* is itself sourced, e.g. from a test harness):
#     . "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/portable.sh"
#
# Exports: TIMEOUT_BIN, PY_VER. Defines exported function: sha256_of.
# Exits with 2 if any required tool is missing.
#
# NOTE: this library enables `set -u` in the caller's shell as a side effect.
# If a caller doesn't want nounset, it should `set +u` after sourcing.

# --- idempotent guard ---
# Prevent double-execution if multiple scripts source this in the same shell.
# The :- default is required because set -u is enabled below.
if [ -n "${_ATOMCODE_PORTABLE_SH_SOURCED:-}" ]; then
    return 0 2>/dev/null || exit 0
fi
_ATOMCODE_PORTABLE_SH_SOURCED=1

set -u

# --- timeout / gtimeout ---
if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_BIN="timeout"
elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN="gtimeout"
else
    echo "fatal: neither 'timeout' nor 'gtimeout' is on PATH" >&2
    echo "       on macOS: brew install coreutils" >&2
    exit 2
fi
export TIMEOUT_BIN

# --- sha256 ---
# We expose a function 'sha256_of <file>' that prints the hex digest
# (no filename suffix), portable across linux sha256sum and macOS shasum.
if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() {
        sha256sum "$1" | awk '{print $1}'
    }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() {
        shasum -a 256 "$1" | awk '{print $1}'
    }
else
    echo "fatal: neither 'sha256sum' nor 'shasum' is on PATH" >&2
    exit 2
fi
export -f sha256_of

# --- python 3.11+ check (parse_case.py / render_index.py 用 tomllib) ---
if ! command -v python3 >/dev/null 2>&1; then
    echo "fatal: python3 not on PATH" >&2
    exit 2
fi
# Single python3 invocation to get both version string and OK flag.
# Output is two whitespace-separated tokens: "<major.minor> <0|1>".
read -r PY_VER PY_OK <<<"$(python3 -c '
import sys
v = sys.version_info
print(f"{v.major}.{v.minor}", 1 if v >= (3, 11) else 0)
')"
if [ "${PY_OK:-0}" != "1" ]; then
    echo "fatal: python3 >= 3.11 required (found ${PY_VER:-?}) — needed for tomllib stdlib" >&2
    exit 2
fi
export PY_VER
