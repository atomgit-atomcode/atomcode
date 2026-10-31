#!/usr/bin/env bash
# Build the clang/lld provider APK and the sysroot data package it reads.
#
# Two artifacts, because Android draws a line between the two:
#
#   com.atomcode.tools.clang    APK with clang + lld + their runtime libraries in
#                               `nativeLibraryDir` (the only executable location
#                               on the device; a data-dir binary returns 126).
#   clang-sysroot-<ver>         data package: headers, the clang resource
#                               directory, and the NDK's aarch64 link-time
#                               stubs/crt objects. Read, never executed.
#
# Where the toolchain comes from: Termux publishes maintained bionic LLVM builds
# (`clang` 21.x, `lld`, `libllvm`). The NDK cannot be used for this — its clang is
# a *host* binary and does not run on the phone. Fetching is done by
# scripts/termux/fetch.sh, splitting by scripts/termux/split.sh.
#
# Usage:
#   bash scripts/build-android-clang-provider.sh
#
#   MIRROR=...            Termux mirror (default: TUNA; the upstream host drops
#                         large transfers on this network)
#   ANDROID_HOME=...      SDK (also used for the NDK sysroot copy)
#   ANDROID_NDK_HOME=...  NDK; defaults to the newest under $ANDROID_HOME/ndk or
#                         .android-sdk/ndk
#   GRADLE=...            gradle executable (default: `gradle` on PATH)
#   SKIP_APK=1            stop before the Gradle build (jniLibs + data only)
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$PWD

MIRROR=${MIRROR:-https://mirrors.tuna.tsinghua.edu.cn/termux/apt/termux-main}
CACHE=${CACHE:-$REPO/.termux-cache}
PROVIDER=$REPO/android-clang
JNI=$PROVIDER/app/src/main/jniLibs/arm64-v8a
DIST=$REPO/dist

sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$REPO/.android-sdk}}
ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}
if [ -z "$ndk" ] && [ -d "$sdk/ndk" ]; then
  ndk="$sdk/ndk/$(ls "$sdk/ndk" | sort -V | tail -1)"
fi
[ -n "$ndk" ] && [ -d "$ndk" ] || { echo "error: no NDK (set ANDROID_NDK_HOME)" >&2; exit 1; }
host_tag=linux-x86_64
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) host_tag=windows-x86_64 ;; Darwin*) host_tag=darwin-x86_64 ;; esac
sysroot="$ndk/toolchains/llvm/prebuilt/$host_tag/sysroot"

echo "→ packages"
MIRROR=$MIRROR bash scripts/termux/fetch.sh "$CACHE/clang" clang

echo "→ split (executables → jniLibs, read-only → data)"
rm -rf "$CACHE/clang-split"
ANDROID_NDK_HOME=$ndk bash scripts/termux/split.sh \
  "$CACHE/clang/root" "$CACHE/clang-split/jniLibs/arm64-v8a" "$CACHE/clang-split/data" \
  scripts/termux/manifests/clang.txt

clang_dir=$(ls "$CACHE/clang/root/lib/clang" | sort -V | tail -1)
# The deb's control file is the authority for the version (the index lists it
# too, but the index also lists every other package).
version=${ATOMCODE_CLANG_VERSION:-$(ar p "$CACHE/clang/debs/clang.deb" control.tar.xz 2>/dev/null |
  tar -xJO ./control 2>/dev/null | awk -F': *' '/^Version:/{print $2}' | head -1 || true)}
version=${version:-$clang_dir}
echo "→ clang $version (resource dir lib/clang/$clang_dir)"

# One data root that satisfies both flags the wrapper passes:
#   --sysroot=<root>        → <root>/usr/include, <root>/usr/lib/aarch64-linux-android
#   -resource-dir=<root>/lib/clang/<ver>
data=$CACHE/clang-data
rm -rf "$data"
mkdir -p "$data/usr/lib" "$data/lib/clang"
cp -r "$CACHE/clang/root/include" "$data/usr/include"
cp -r "$CACHE/clang/root/lib/clang/$clang_dir" "$data/lib/clang/$clang_dir"
cp -r "$sysroot/usr/lib/aarch64-linux-android" "$data/usr/lib/aarch64-linux-android"

echo "→ APK jniLibs"
rm -rf "$JNI"
mkdir -p "$JNI"
cp "$CACHE"/clang-split/jniLibs/arm64-v8a/* "$JNI"/

# `clang` is the launcher, not the driver: it appends `-resource-dir` and
# `--sysroot` for the data package and execs `libclang-real.so` next to it. It has
# to be a binary because a script in the data directory cannot be executed either.
cc="$ndk/toolchains/llvm/prebuilt/$host_tag/bin/aarch64-linux-android${API:-26}-clang"
[ -x "$cc" ] || cc="$cc.cmd"
[ -e "$cc" ] || { echo "error: no NDK clang at $cc" >&2; exit 1; }
echo "→ launcher (clang $clang_dir)"
"$cc" -O2 -DCLANG_RESOURCE="\"lib/clang/$clang_dir\"" \
  scripts/termux/launcher/clang.c -o "$JNI/libclang.so"
ls -lh "$JNI/libclang.so" "$JNI/libclang-real.so"

mkdir -p "$DIST"
# The installer links `<product>/<link>` at this package's directory. `clang` is
# the name `$ATOMCODE_CLANG_DATA` points at, and the package root is exactly the
# root the launcher expects (`usr/include`, `usr/lib`, `lib/clang/<ver>`), so the
# link target is the package root — an empty relative path.
cat > "$data/manifest.json" <<EOF
{
  "schema": 1,
  "name": "clang-sysroot",
  "version": "$version",
  "entry": "",
  "links": { "clang": "" }
}
EOF
bundle="$DIST/clang-sysroot-$version-arm64.tar.gz"
echo "→ data package $bundle"
tar -czf "$bundle" -C "$data" manifest.json usr lib
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
cp "$apk" "$DIST/clang-provider-$version-arm64.apk"
echo "→ $DIST/clang-provider-$version-arm64.apk ($(du -h "$apk" | cut -f1))"
echo "  sha256 $(sha256sum "$apk" | cut -d' ' -f1)"