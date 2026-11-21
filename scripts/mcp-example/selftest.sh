#!/usr/bin/env bash
# Host-side smoke test for the example MCP server.
#
# The Android package is a `pluginhost` plugin, and `pluginhost` only exists in
# the APK — but the protocol logic is portable C, so the same source compiles
# against the host libc and speaks the same newline-delimited JSON-RPC over
# stdio. That makes the buffer arithmetic testable without a device, which is
# how the heap overflow in `repeat` was found: it wrote `text_len * times +
# (times - 1)` bytes into a buffer sized for `text_len * times`.
#
#   bash scripts/mcp-example/selftest.sh          # uses cc/gcc on PATH
#   CC=/usr/bin/clang bash scripts/mcp-example/selftest.sh
#
# Built with AddressSanitizer when the compiler supports it, so an overflow is
# a failure rather than a silent corruption.
set -euo pipefail
cd "$(dirname "$0")/../.."

CC="${CC:-cc}"
command -v "$CC" >/dev/null 2>&1 || CC=gcc
command -v "$CC" >/dev/null 2>&1 || { echo "error: no C compiler (set CC)" >&2; exit 1; }

SRC="crates/atomcode-capabilities/assets/setup-seeds/mcp/example_server.c"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# ASan when available: it turns the overflow into a hard failure.
san=""
if "$CC" -fsanitize=address -x c -o "$work/probe" - </dev/null >/dev/null 2>&1; then
  san="-fsanitize=address -fno-omit-frame-pointer"
fi
echo "== build (sanitizer: ${san:-off}) =="
# The same flags as `scripts/build-android-mcp-plugin.sh`, deliberately without
# `-Werror`: gcc's format-truncation warning on the response buffer fires here
# but not under the NDK clang, and a warning must not decide whether the self
# test runs. A real error still fails the build.
# shellcheck disable=SC2086
"$CC" -std=c99 -O1 -Wall -Wextra $san \
  -o "$work/mcp-example" "$SRC" scripts/mcp-example/host_main.c

fail=0
check() { # check <name> <haystack> <needle>
  if printf '%s' "$2" | grep -qF -- "$3"; then
    echo "  ok   $1"
  else
    echo "  FAIL $1 (missing: $3)" >&2
    printf '       got: %s\n' "$2" >&2
    fail=1
  fi
}

# The server reads one JSON-RPC message per line from stdin and answers on
# stdout. `repeat` is the tool that had the overflow: `times` copies of `text`
# joined by newlines.
call_repeat() { # call_repeat <text> <times>
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"repeat","arguments":{"text":"'"$1"'","times":'"$2"'"}}}' \
    | "$work/mcp-example" 2>/dev/null | tail -1
}

echo "== repeat: the separator count is part of the length =="
# `times = 2` of a 5-byte text is 11 bytes ("hello\nhello"), not 10.
out="$(call_repeat hello 2)"
check "two copies are joined by one newline" "$out" 'hello\nhello'
# `times = 20` is the maximum the tool accepts, and the case that overflowed by
# 19 bytes before the fix.
out="$(call_repeat hello 20)"
check "twenty copies survive" "$out" "hello"
count="$(printf '%s' "$out" | grep -o 'hello' | wc -l | tr -d ' ')"
if [ "$count" = "20" ]; then
  echo "  ok   twenty copies are present"
else
  echo "  FAIL expected 20 copies, got $count" >&2
  fail=1
fi

echo "== the length guard still rejects oversized results =="
out="$(call_repeat hello 20)"  # 20 * 5 + 19 = 119 bytes, under the cap
check "short text is accepted" "$out" '"isError":false'

echo "== an out-of-range count is refused =="
out="$(call_repeat hello 21)"
check "times above the maximum is an error" "$out" '"isError":true'

if [ "$fail" = "0" ]; then
  echo "ok"
else
  echo "FAILED" >&2
  exit 1
fi