#!/usr/bin/env bash
# Host-side self test for the text plugin's buffer growth.
#
# The plugin renders text to a PNG with FreeType and is dlopened into the host
# process on the device. FreeType and a font file are not available on a
# build machine, and they are not what needs testing — the buffer is pure C,
# and it is where the memory-safety bug lived: `text_push` wrote an unchecked
# `realloc` result back into the struct and then indexed it, so a failed
# allocation became a NULL dereference inside the agent process.
#
#   bash scripts/text-plugin/selftest.sh          # uses cc/gcc on PATH
#   CC=/usr/bin/clang bash scripts/text-plugin/selftest.sh
#
# It compiles the same `text_buffer.h` the plugin uses — the real growth path,
# not a copy of it — and injects a failing allocator to reach the failure
# branches deterministically.
set -euo pipefail
cd "$(dirname "$0")/../.."

CC="${CC:-cc}"
command -v "$CC" >/dev/null 2>&1 || CC=gcc
command -v "$CC" >/dev/null 2>&1 || { echo "error: no C compiler (set CC)" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "== build =="
"$CC" -std=c99 -O1 -Wall -Wextra -Werror \
  -o "$work/text-selftest" \
  scripts/text-plugin/selftest.c -Iscripts/text-plugin

echo "== the buffer survives a failed growth =="
"$work/text-selftest"

# The plugin itself is C, and no Rust gate in this repo can see a C file: if
# `plugin.c` stopped compiling, nothing else would notice until someone built
# the Android package on a machine that has the NDK and the FreeType prefix.
# The stub headers let the host compiler check the real source. `-fsyntax-only`
# means nothing is linked, so the stub needs no implementation.
echo "== the plugin still compiles =="
"$CC" -std=c99 -fsyntax-only -Wall -Wextra \
  -Iscripts/text-plugin/stub -Iscripts/text-plugin/stub/freetype2 \
  -Iscripts/svg-plugin/vendor \
  scripts/text-plugin/plugin.c
echo "  ok   plugin.c compiles against the FreeType stub"
