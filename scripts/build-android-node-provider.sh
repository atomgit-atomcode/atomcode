#!/usr/bin/env bash
# Build the node provider APK and the npm/header data package it reads.
#
# Why a provider APK and not the existing plugin: the `node` market package embeds
# nodejs-mobile's libnode, whose last release is 18.20.4. Upgrading means shipping
# node itself, and a program cannot be executed from this app's data directory
# (return 126) — only from some APK's `nativeLibraryDir`. So node rides in an APK
# as `libnode.so` and the app links `<data>/bin/node` at it, like the zig helper.
#
# The interpreter comes from Termux (`nodejs-lts`, an Android/bionic build); the
# `npm` package is a JS tree and travels with the data, because nothing in it is
# executed by the kernel.
#
# Usage:
#   bash scripts/build-android-node-provider.sh
#
#   MIRROR=...            Termux mirror (default TUNA)
#   ANDROID_HOME=...      SDK
#   GRADLE=...            gradle executable (default: `gradle` on PATH)
#   SKIP_APK=1            stop before the Gradle build
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$PWD

MIRROR=${MIRROR:-https://mirrors.tuna.tsinghua.edu.cn/termux/apt/termux-main}
CACHE=${CACHE:-$REPO/.termux-cache}
PROVIDER=$REPO/android-node
JNI=$PROVIDER/app/src/main/jniLibs/arm64-v8a
DIST=$REPO/dist
sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$REPO/.android-sdk}}
ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}
if [ -z "$ndk" ] && [ -d "$sdk/ndk" ]; then
  ndk="$sdk/ndk/$(ls "$sdk/ndk" | sort -V | tail -1)"
fi

echo "→ packages"
MIRROR=$MIRROR bash scripts/termux/fetch.sh "$CACHE/node" nodejs-lts npm

echo "→ split (node → jniLibs, npm tree + headers → data)"
rm -rf "$CACHE/node-split"
ANDROID_NDK_HOME=${ndk:-} bash scripts/termux/split.sh \
  "$CACHE/node/root" "$CACHE/node-split/jniLibs/arm64-v8a" "$CACHE/node-split/data" \
  scripts/termux/manifests/node.txt

version=${ATOMCODE_NODE_VERSION:-$(ar p "$CACHE/node/debs/nodejs-lts.deb" control.tar.xz 2>/dev/null |
  tar -xJO ./control 2>/dev/null | awk -F': *' '/^Version:/{print $2}' | head -1 || true)}
version=${version:-unknown}
echo "→ node $version"

# The data root is what the app will expose as its node prefix: `include/node`
# for addon builds and `lib/node_modules/npm` for the package manager.
data=$CACHE/node-data
rm -rf "$data"
mkdir -p "$data"
cp -r "$CACHE/node-split/data"/. "$data"/
# npm is an `all` package: its tree lands under lib/node_modules, but the deb also
# ships bin/npm as a symlink. That link is useless on device (a script in the data
# directory cannot be executed), so it is dropped rather than shipped broken.
rm -f "$data/bin/npm" "$data/bin/npx"

echo "→ APK jniLibs"
rm -rf "$JNI"
mkdir -p "$JNI"
cp "$CACHE"/node-split/jniLibs/arm64-v8a/* "$JNI"/

mkdir -p "$DIST"
bundle="$DIST/node-lib-$version-arm64.tar.gz"
echo "→ data package $bundle"
tar -czf "$bundle" -C "$data" .
echo "  size   $(du -h "$bundle" | cut -f1)"
echo "  sha256 $(sha256sum "$bundle" | cut -d' ' -f1)"

if [ "${SKIP_APK:-0}" = "1" ]; then
  echo "→ SKIP_APK=1, stopping before Gradle"
  exit 0
fi

printf 'sdk.dir=%s\n' "$(cd "$sdk" && pwd)" > "$PROVIDER/local.properties"
gradle=${GRADLE:-gradle}
command -v "$gradle" >/dev/null 2>&1 || { echo "error: gradle not found (set GRADLE=...)" >&2; exit 1; }
echo "→ gradle assembleDebug"
( cd "$PROVIDER" && ANDROID_HOME=$sdk "$gradle" assembleDebug )
apk=$PROVIDER/app/build/outputs/apk/debug/app-debug.apk
cp "$apk" "$DIST/node-provider-$version-arm64.apk"
echo "→ $DIST/node-provider-$version-arm64.apk ($(du -h "$apk" | cut -f1))"
echo "  sha256 $(sha256sum "$apk" | cut -d' ' -f1)"