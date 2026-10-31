#!/usr/bin/env bash
# Host-side smoke test for the filesystem MCP server.
#
# The Android package is a plugin loaded by `pluginhost`, which only exists in
# the APK. The protocol logic is portable C, so this compiles the same source
# against the host libc and drives it over stdio exactly as a client would:
# initialize → tools/list → tools/call, plus the two failure paths that matter
# (a path escaping the root, and a binary file).
#
#   bash scripts/mcp-filesystem/selftest.sh          # uses cc/gcc on PATH
#   CC=/usr/bin/clang bash scripts/mcp-filesystem/selftest.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

CC="${CC:-cc}"
command -v "$CC" >/dev/null 2>&1 || CC=gcc
command -v "$CC" >/dev/null 2>&1 || { echo "error: no C compiler (set CC)" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root="$work/root"
mkdir -p "$root/sub"
printf 'hello from the server\n' > "$root/notes.txt"
printf 'nested\n' > "$root/sub/inner.txt"
printf 'secret\n' > "$work/outside.txt"
printf 'bin\000ary\n' > "$root/blob.bin"

"$CC" -std=c99 -O1 -Wall -Wextra -Werror \
  -o "$work/mcp-filesystem" \
  scripts/mcp-filesystem/plugin.c scripts/mcp-filesystem/host_main.c

run() { ATOMCODE_MCP_FS_ROOT="$root" "$work/mcp-filesystem"; }

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

out=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | run)
check "initialize names the server" "$out" '"name":"mcp-filesystem"'
check "tools/list advertises list_dir" "$out" '"name":"list_dir"'
check "tools/list advertises read_file" "$out" '"name":"read_file"'
check "tools/list marks tools read-only" "$out" '"readOnlyHint":true'

out=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_dir","arguments":{"path":"."}}}' | run)
check "list_dir shows a file" "$out" 'notes.txt'
check "list_dir marks a directory" "$out" 'sub/'

out=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"notes.txt"}}}' | run)
check "read_file returns the content" "$out" 'hello from the server'

out=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"stat","arguments":{"path":"sub"}}}' | run)
check "stat reports a directory" "$out" 'directory'

out=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"../outside.txt"}}}' | run)
check "a path escaping the root is refused" "$out" '"isError":true'
if printf '%s' "$out" | grep -qF -- 'secret'; then
  echo "  FAIL the refused path must not leak the outside file" >&2
  fail=1
else
  echo "  ok   the outside text never appears"
fi

out=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"blob.bin"}}}' | run)
check "a binary file is refused, not echoed" "$out" 'binary file'

out=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"nope","arguments":{}}}' | run)
check "an unknown tool is an error" "$out" 'unknown tool: nope'

if [ "$fail" != 0 ]; then
  echo "filesystem MCP selftest: FAILED" >&2
  exit 1
fi
echo "filesystem MCP selftest: passed"