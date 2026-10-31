#!/usr/bin/env bash
# Package nodejs-mobile's `libnode.so` as a plugin.
#
# A program cannot be executed from this app's data directory, so `node` cannot
# be shipped as a binary. nodejs-mobile's build is a *shared library* that exports
# `node::Start(argc, argv)` for embedders, which is exactly the shape our plugin
# mechanism already uses (same as Lua and CPython): `pluginhost` dlopens the
# library and calls `plugin_main`.
#
# Usage: scripts/build-android-node-plugin.sh <nodejs-mobile-android.zip>
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BUILD=${BUILD:-$HOME/android-git-build}
NDK=${NDK:-$BUILD/ndk/android-ndk-r26b}
API=${API:-26}
OUT=$REPO/dist

ZIP=${1:-}
if [ -z "$ZIP" ] || [ ! -f "$ZIP" ]; then
  echo "usage: $0 <nodejs-mobile-v*-android.zip>" >&2
  exit 2
fi

BIN=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin
CXX=$BIN/aarch64-linux-android${API}-clang++
READELF=$BIN/llvm-readelf

WORK=$BUILD/node-plugin
rm -rf "$WORK"
mkdir -p "$WORK/src"
python3 -m zipfile -e "$ZIP" "$WORK/src"

# The archive carries one directory per ABI; arm64 is the device's.
ABI_DIR=$WORK/src/bin/arm64-v8a
HEADERS=$WORK/src/include
[ -f "$ABI_DIR/libnode.so" ] || { echo "error: no arm64 libnode.so in $ZIP" >&2; exit 1; }
[ -d "$HEADERS/node" ] || { echo "error: no node headers in $ZIP" >&2; exit 1; }

VERSION=$(basename "$ZIP" | sed -E 's/^nodejs-mobile-v([0-9.]+)-android\.zip$/\1/')

PKG=$WORK/pkg
mkdir -p "$PKG/lib"
cp "$ABI_DIR/libnode.so" "$PKG/lib/libnode.so"

echo "→ libnode.so dependencies:"
$READELF -d "$PKG/lib/libnode.so" 2>/dev/null | grep -E "NEEDED" | sed 's/^/  /'

echo "→ compiling the embedder shim"
"$CXX" -shared -fPIC -O2 -std=c++17 \
  -I"$HEADERS/node" \
  -o "$PKG/lib/plugin.so" \
  "$REPO/scripts/node-plugin/plugin.cc" \
  -L"$PKG/lib" -lnode \
  -Wl,-rpath,'$ORIGIN'

# `libnode.so` links libc++ by plain soname; bionic only resolves a DT_NEEDED
# against a library that is already loaded. The runtime package ships
# libc++_shared.so for numpy, so reuse it here rather than duplicating 1.4 MB.
VENDOR="$REPO/dist/vendor-libs"
# Only libc++: node links it by plain soname. openblas belongs to the runtime
# package (it is numpy's dependency), and copying it here would both bloat this
# bundle and hide a missing runtime library behind a neighbouring package.
for need in libc++_shared.so; do
  if [ -f "$VENDOR/$need" ]; then
    cp "$VENDOR/$need" "$PKG/lib/"
    echo "  + $need"
  fi
done

preload_entries="\"lib/libnode.so\", \"lib/libc++_shared.so\""
cat > "$PKG/package.json" <<JSON
{
  "name": "node",
  "version": "$VERSION",
  "description": "Node.js $VERSION embedded from nodejs-mobile (libnode.so + embedder shim)",
  "entry": "lib/plugin.so",
  "symbol": "plugin_main",
  "aliases": ["node"],
  "preload": [$preload_entries]
}
JSON

cat > "$PKG/README.md" <<'MD'
# node (embedded libnode)

nodejs-mobile's `libnode.so` plus a two-line embedder shim, packaged as a plugin.
`pluginhost` dlopens it; `node::Start` runs the interpreter in-process.

```
node -e 'console.log(process.version, 1 + 1)'
```

Known limits on this device, all consequences of "the interpreter is a library,
not a process":

- `process.execPath` is `pluginhost`, so `child_process.fork`/`spawn` cannot
  re-execute node itself.
- Prebuilt native addons (`.node`) link glibc and will not load; ones built for
  Android need libnode's symbols and only resolve because libnode is preloaded.
- A segmentation fault inside JS takes the whole agent down with it.
MD

ARCHIVE="$OUT/node-$VERSION-arm64.tar.gz"
rm -f "$ARCHIVE"
tar -C "$PKG" -czf "$ARCHIVE" .
printf '  %-22s %s\n' "node" "$(du -h "$ARCHIVE" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256 /'
