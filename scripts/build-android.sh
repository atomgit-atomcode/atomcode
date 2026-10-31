#!/usr/bin/env bash
# Build the Rust core for Android and drop the .so where Gradle expects it.
#
#   bash scripts/build-android.sh          # cargo build only, no APK
#   bash scripts/build-android.sh --apk    # also run gradle assembleDebug
#
# Requires:
#   - rustup target add aarch64-linux-android
#   - an Android NDK (r26+). Resolution order:
#       $ANDROID_NDK_HOME  ->  $ANDROID_NDK_ROOT  ->  $ANDROID_HOME/ndk/<newest>
#   - the Android SDK + a JDK 17 for --apk
#
# The linker is NOT in .cargo/config.toml on purpose: its path depends on the
# NDK install location, which is a property of the machine, not of the repo.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${ATOMCODE_ANDROID_PROFILE:-android}"
TARGET="aarch64-linux-android"
ABI="arm64-v8a"
# minSdk is 26 in app/build.gradle.kts; the NDK wrapper name must match.
API="${ATOMCODE_ANDROID_API:-26}"

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) HOST_TAG="windows-x86_64" ;;
  Darwin*)              HOST_TAG="darwin-x86_64" ;;
  *)                    HOST_TAG="linux-x86_64" ;;
esac

ndk="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [ -z "$ndk" ] && [ -n "${ANDROID_HOME:-}" ] && [ -d "$ANDROID_HOME/ndk" ]; then
  ndk="$ANDROID_HOME/ndk/$(ls "$ANDROID_HOME/ndk" | sort -V | tail -1)"
fi
if [ -z "$ndk" ] || [ ! -d "$ndk" ]; then
  echo "error: Android NDK not found." >&2
  echo "  set ANDROID_NDK_HOME, or install the NDK under \$ANDROID_HOME/ndk" >&2
  exit 1
fi

toolchain="$ndk/toolchains/llvm/prebuilt/$HOST_TAG/bin"
if [ ! -d "$toolchain" ]; then
  echo "error: NDK toolchain not found at $toolchain" >&2
  exit 1
fi

# The NDK ships two spellings of the same driver. The `.cmd` wrapper only runs
# through cmd.exe, and Cargo hands the linker path straight to CreateProcess —
# so on Windows prefer clang.exe and pass the target explicitly, which is what
# the wrapper does anyway.
if [ "$HOST_TAG" = "windows-x86_64" ]; then
  linker="$toolchain/clang.exe"
  export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=--target=$TARGET$API"
else
  linker="$toolchain/${TARGET}${API}-clang"
fi
if [ ! -e "$linker" ]; then
  echo "error: linker not found: $linker" >&2
  exit 1
fi
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$linker"

# ring / zstd-sys / tree-sitter build C with the NDK clang too.
export CC_aarch64_linux_android="$linker"
export CXX_aarch64_linux_android="$toolchain/clang++.exe"
[ -e "$CXX_aarch64_linux_android" ] || export CXX_aarch64_linux_android="$toolchain/${TARGET}${API}-clang++"
export AR_aarch64_linux_android="$toolchain/llvm-ar.exe"
[ -e "$AR_aarch64_linux_android" ] || export AR_aarch64_linux_android="$toolchain/llvm-ar"
export RANLIB_aarch64_linux_android="$toolchain/llvm-ranlib.exe"
[ -e "$RANLIB_aarch64_linux_android" ] || export RANLIB_aarch64_linux_android="$toolchain/llvm-ranlib"

echo "→ target $TARGET (API $API, profile $PROFILE)"
echo "  NDK:    $ndk"
echo "  linker: $linker"

# Gateway signing is ON by default: the overlay in this tree is the real
# implementation (HKDF/HMAC `sign_v1`, not the public stub), and without it every
# gateway request is refused — the app looks logged in but no turn can run. That
# is a silent, hard-to-diagnose failure, so the default is the working one and
# turning it OFF is the explicit act: ATOMCODE_ANDROID_CRYPTO=0.
FEATURES=()
# The CLI is a different package, so it needs its own feature name: the host
# crate exposes `codingplan-crypto`, the CLI exposes `codingplan-crypto` of its
# own. Passing `atomcode-android/...` to `-p atomcode` fails outright, which is
# how this broke the first time crypto was switched on.
CLI_FEATURES=()
if [ "${ATOMCODE_ANDROID_CRYPTO:-1}" = "1" ]; then
  FEATURES+=(--features atomcode-android/codingplan-crypto)
  CLI_FEATURES+=(--features codingplan-crypto)
  echo "  crypto:  codingplan-crypto (gateway signing enabled)"
else
  echo "  crypto:  DISABLED (gateway requests will be refused; unset ATOMCODE_ANDROID_CRYPTO)"
fi

cargo build --profile "$PROFILE" --target "$TARGET" -p atomcode-android "${FEATURES[@]}"
# The runtime-package launcher. Android only lets `nativeLibraryDir` execute, so
# this is staged beside the host library and symlinked into `<data>/bin` by
# `shims`. It is a separate process on purpose: a crashing interpreter must not
# take the daemon down with it.
cargo build --profile "$PROFILE" --target "$TARGET" -p atomcode-android --bin pluginhost "${FEATURES[@]}"

# The CLI, for the same reason the launcher is built: Android refuses to
# execute anything outside `nativeLibraryDir`, so a standalone `atomcode daemon`
# can only run from there. It goes into the **helper** APK, not the main one:
# the helper exists precisely to carry binaries this app may execute, and the
# CLI is only useful to someone who asked for it.
if [ "${ATOMCODE_ANDROID_CLI:-1}" = "1" ]; then
  cargo build --profile "$PROFILE" --target "$TARGET" -p atomcode --bin atomcode "${CLI_FEATURES[@]}"
  cli="target/$TARGET/$PROFILE/atomcode"
  if [ ! -f "$cli" ]; then
    echo "error: expected $cli after the build" >&2
    exit 1
  fi
  cli_dest="android-tools/app/src/main/jniLibs/$ABI"
  mkdir -p "$cli_dest"
  cp "$cli" "$cli_dest/libatomcode.so"
  # Strip it: the debug build carries ~200 MB of symbols, and the helper APK
  # is already 51 MB because of zig.
  "$toolchain/llvm-strip" --strip-all "$cli_dest/libatomcode.so" 2>/dev/null || true
  echo "→ helper: libatomcode.so ($(du -h "$cli_dest/libatomcode.so" | cut -f1))"
fi

so="target/$TARGET/$PROFILE/libatomcode_android.so"
if [ ! -f "$so" ]; then
  echo "error: expected $so after the build" >&2
  exit 1
fi
launcher="target/$TARGET/$PROFILE/pluginhost"
if [ ! -f "$launcher" ]; then
  echo "error: expected $launcher after the build" >&2
  exit 1
fi
dest="android/app/src/main/jniLibs/$ABI"
mkdir -p "$dest"
cp "$so" "$dest/libatomcode_android.so"
# Only `lib*.so` names are packaged, so the launcher wears that name too; the
# symlink `<data>/bin/pluginhost` is what a caller actually runs.
cp "$launcher" "$dest/libpluginhost.so"
echo "→ copied $(du -h "$dest/libatomcode_android.so" | cut -f1) to $dest/ (+ libpluginhost.so)"

if [ "${1:-}" = "--apk" ]; then
  if [ ! -f android/local.properties ] && [ -n "${ANDROID_HOME:-}" ]; then
    printf 'sdk.dir=%s\n' "${ANDROID_HOME//\\/\\\\}" > android/local.properties
  fi
  ( cd android && ./gradlew assembleDebug )
  echo "→ APK: android/app/build/outputs/apk/debug/app-debug.apk"
fi