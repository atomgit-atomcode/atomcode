#!/usr/bin/env bash
# Build the example MCP server as a pluginhost library, and pack it as a bundle.
#
#   bash scripts/build-android-mcp-plugin.sh                 # → dist/mcp-example-<ver>.tar.gz
#   ANDROID_HOME=/path/to/sdk bash scripts/build-android-mcp-plugin.sh
#
# Why a library: on Android this app may not `execve` anything from its data
# directory (measured: exit 126 from the real app process), so a market data package
# cannot ship a runnable binary. `dlopen` from that directory IS allowed, and
# `pluginhost` — which lives in the APK's nativeLibraryDir, the one place Android
# executes from — is the only process that needs to be executable. So the MCP server
# is a `.so` with `int plugin_main(int argc, char **argv)` and speaks the protocol on
# the stdio it inherits (newline-delimited JSON-RPC 2.0).
#
# Needs: an Android NDK (r26+) and a shell. No Rust, no CMake, no other dependency —
# the source is plain C against libc.
set -euo pipefail
cd "$(dirname "$0")/.."

API="${ATOMCODE_ANDROID_API:-26}"
HOST_TAG=""
ndk="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [ -z "$ndk" ] && [ -n "${ANDROID_HOME:-}" ] && [ -d "$ANDROID_HOME/ndk" ]; then
  ndk="$ANDROID_HOME/ndk/$(ls "$ANDROID_HOME/ndk" | sort -V | tail -1)"
fi
if [ -z "$ndk" ] || [ ! -d "$ndk" ]; then
  echo "error: Android NDK not found (set ANDROID_NDK_HOME, or install under \$ANDROID_HOME/ndk)" >&2
  exit 1
fi
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) HOST_TAG="windows-x86_64" ;;
  Darwin*)              HOST_TAG="darwin-x86_64" ;;
  *)                    HOST_TAG="linux-x86_64" ;;
esac
BIN="$ndk/toolchains/llvm/prebuilt/$HOST_TAG/bin"
if [ "$HOST_TAG" = "windows-x86_64" ]; then
  CC="$BIN/clang.exe"
else
  CC="$BIN/clang"
fi
[ -x "$CC" ] || { echo "error: missing $CC" >&2; exit 1; }

VERSION="${ATOMCODE_MCP_EXAMPLE_VERSION:-1.0.0}"
SRC="crates/atomcode-capabilities/assets/setup-seeds/mcp/example_server.c"
OUT="dist/mcp-example"
rm -rf "$OUT"
mkdir -p "$OUT/lib"

echo "→ compiling $SRC for aarch64-linux-android$API"
"$CC" --target="aarch64-linux-android$API" -shared -fPIC -O2 -Wall -Wextra \
  -o "$OUT/lib/libmcp_example.so" "$SRC"

cat > "$OUT/package.json" <<JSON
{
  "name": "mcp-example",
  "version": "$VERSION",
  "description": "Minimal MCP server built as a pluginhost library (tools: now, repeat).",
  "entry": "lib/libmcp_example.so",
  "symbol": "plugin_main",
  "aliases": ["mcp-example"]
}
JSON

BUNDLE="dist/mcp-example-$VERSION.tar.gz"
tar -czf "$BUNDLE" -C "$OUT" package.json lib
echo "→ $BUNDLE"
ls -l "$BUNDLE" | awk '{print "  size", $5, "bytes"}'
echo "  sha256 $(sha256sum "$BUNDLE" | cut -d' ' -f1)"
cat <<'NOTE'
→ install on a device (the app's own API, no adb root needed):
    adb push dist/mcp-example-<ver>.tar.gz /sdcard/Download/
    POST /packages/install {"path":"/sdcard/Download/mcp-example-<ver>.tar.gz"}
  then point mcp.json at the alias it creates:
    {"mcpServers":{"example-bin":{"command":"<product>/bin/mcp-example","args":[]}}}
NOTE
