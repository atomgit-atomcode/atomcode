#!/usr/bin/env bash
# Build a provider APK for one Android-native toolchain piece.
#
# Why a provider APK at all: this app may not execute anything from its own data
# directory (measured on device: exit 126), and every toolchain program — ninja,
# cmake, the Go compiler — has to run. The one directory Android does execute from
# is an APK's `nativeLibraryDir`, so the tool rides here as `lib<stem>.so` and the
# host links `<product>/bin/<name>` at it. Same carrier as the zig, clang and node
# providers; this script is the generic driver those three spell out one by one.
#
# Usage:
#   bash scripts/build-android-tool-provider.sh <name> <termux-package>...
#
#   <name>            provider name; the project must exist at android-<name>/, the
#                     split manifest at scripts/termux/manifests/<name>.txt
#   MIRROR=...        Termux mirror (default TUNA)
#   ANDROID_HOME=...  SDK (for the NDK's llvm-readelf used by split.sh)
#   GRADLE=...        gradle executable (default: `gradle` on PATH)
#   SKIP_APK=1        stop after staging jniLibs and the data tree
#
# Output:
#   android-<name>/app/src/main/jniLibs/arm64-v8a/   the APK's executables
#   .termux-cache/<name>-data/                       the read-only data tree
#   dist/<name>-provider-arm64.apk                   installable APK
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$PWD

name=${1:?usage: build-android-tool-provider.sh <name> <termux-package>...}
shift
primary=$1
[ "$#" -gt 0 ] || { echo "error: no Termux packages named" >&2; exit 2; }

MIRROR=${MIRROR:-https://mirrors.tuna.tsinghua.edu.cn/termux/apt/termux-main}
CACHE=${CACHE:-$REPO/.termux-cache}
PROVIDER=$REPO/android-$name
MANIFEST=$REPO/scripts/termux/manifests/$name.txt
JNI=$PROVIDER/app/src/main/jniLibs/arm64-v8a
DIST=$REPO/dist
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$REPO/.android-sdk}}
ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}
if [ -z "$ndk" ] && [ -d "$sdk/ndk" ]; then
  ndk="$sdk/ndk/$(ls "$sdk/ndk" | sort -V | tail -1)"
fi

[ -f "$MANIFEST" ] || { echo "error: no manifest at $MANIFEST" >&2; exit 1; }
[ -f "$PROVIDER/app/build.gradle.kts" ] || {
  echo "error: no provider project at $PROVIDER (copy android-ninja/ and rename)" >&2
  exit 1
}

echo "→ packages: $*"
MIRROR=$MIRROR bash scripts/termux/fetch.sh "$CACHE/$name" "$@"

echo "→ split (executables → jniLibs, read-only → data)"
rm -rf "$CACHE/$name-split"
ANDROID_NDK_HOME=${ndk:-} bash scripts/termux/split.sh \
  "$CACHE/$name/root" "$CACHE/$name-split/jniLibs/arm64-v8a" "$CACHE/$name-split/data" \
  "$MANIFEST"

echo "→ stage jniLibs"
rm -rf "$JNI"
mkdir -p "$JNI"
cp -r "$CACHE/$name-split/jniLibs/arm64-v8a"/. "$JNI"/

# Termux bakes its own $PREFIX into some binaries, and a path that does not exist
# here breaks them in ways that look like a broken toolchain (cmake's default
# `CMAKE_SH` is the one that bit us: every generated Makefile named
# `/data/data/com.termux/files/usr/bin/sh`). The patch is an equal-length rewrite,
# so the ELF is untouched apart from those bytes.
if [ -f scripts/termux/patch-terminux-sh.py ]; then
  echo "→ patch Termux paths"
  python3 scripts/termux/patch-terminux-sh.py "$JNI"/*.so 2>/dev/null ||
    python scripts/termux/patch-terminux-sh.py "$JNI"/*.so 2>/dev/null || true
fi

data=$CACHE/$name-data
rm -rf "$data"
mkdir -p "$data"
if [ -d "$CACHE/$name-split/data" ]; then
  cp -r "$CACHE/$name-split/data"/. "$data"/
fi
if [ -d "$data" ] && [ -n "$(ls -A "$data" 2>/dev/null)" ]; then
  echo "→ data: $(du -sh "$data" | cut -f1)"
  # The data half is a market package of its own — the versioned `.so` files Gradle
  # refuses to package and the read-only trees (GOROOT, cmake modules) live here.
  # Its version comes from the deb that carries the primary package.
  deb=$(ls "$CACHE/$name/debs/$primary"*.deb 2>/dev/null | head -1 || true)
  version=$(ar p "$deb" control.tar.xz 2>/dev/null |
    tar -xJO ./control 2>/dev/null | awk -F': *' '/^Version:/{print $2}' | head -1 || true)
  version=${version:-0.0.0}
  bash scripts/build-android-data-package.sh "$name-data" "$version" "$data" "$name"
fi

if [ "${SKIP_APK:-0}" = "1" ]; then
  echo "→ SKIP_APK=1, stopping before Gradle"
  exit 0
fi

echo "→ apk"
"${GRADLE:-gradle}" -p "$PROVIDER" assembleDebug --offline -q
mkdir -p "$DIST"
cp "$PROVIDER/app/build/outputs/apk/debug/app-debug.apk" "$DIST/$name-provider-arm64.apk"
echo "→ dist/$name-provider-arm64.apk ($(du -h "$DIST/$name-provider-arm64.apk" | cut -f1))"
