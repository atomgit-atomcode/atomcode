#!/usr/bin/env bash
# Build the payload of the helper APK (`com.atomcode.tools`).
#
# Why a second APK: `make` runs a compiler and a compiler runs an assembler —
# they must `execve` children. Android forbids that for anything in OUR data
# directory, but it DOES allow executing another package's `nativeLibraryDir`
# (measured on device, see docs/android-port.md §P5.9). So this APK is a plain
# file holder: no service, no Binder, no IPC. The main app links these binaries
# into `<data>/bin` and calls them like any command.
#
#   MAKE_TGZ=./make.tgz TCC_TGZ=./tcc.src \
#   ANDROID_NDK_LINUX_HOME=~/android-git-build/ndk/android-ndk-r26b \
#     bash scripts/build-android-tools.sh
#
# Sources (not vendored):
#   make.tgz  https://ftp.gnu.org/gnu/make/make-4.4.1.tar.gz
#   tcc.src   https://github.com/TinyCC/tinycc/archive/refs/heads/mob.tar.gz
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
NDK="${ANDROID_NDK_LINUX_HOME:-$HOME/android-git-build/ndk/android-ndk-r26b}"
API="${ATOMCODE_ANDROID_API:-26}"
TARGET=aarch64-linux-android
BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
CC="$BIN/${TARGET}${API}-clang"
SRC="${TOOLS_SRC:-/tmp/atomcode-tools}"
DEST="$REPO/android-tools/app/src/main/jniLibs/arm64-v8a"

MAKE_TGZ="${MAKE_TGZ:-$REPO/make.tgz}"
TCC_TGZ="${TCC_TGZ:-$REPO/tcc.src}"

[ -x "$CC" ] || { echo "error: missing $CC" >&2; exit 1; }
mkdir -p "$SRC" "$DEST"

# Zig moved to its own market package (`docs/android-next-steps.md` §三): one binary of
# 141 MB unpacked was why this APK weighed 82 MB. Prune a stale copy left by an older
# tree so it cannot silently end up back in the payload.
if [ -f "$DEST/libzig.so" ]; then
  echo "→ dropping stale libzig.so (zig is a market package now)"
  rm -f "$DEST/libzig.so"
fi

# --- GNU make ---------------------------------------------------------------
if [ ! -x "$SRC/make-bin" ]; then
  echo "→ building GNU make"
  rm -rf "$SRC/make" && mkdir -p "$SRC/make"
  tar -C "$SRC/make" -xzf "$MAKE_TGZ"
  local_make="$SRC/make/$(ls "$SRC/make" | head -1)"
  ( cd "$local_make"
    ./configure --host="$TARGET" CC="$CC" AR="$BIN/llvm-ar" RANLIB="$BIN/llvm-ranlib" \
      --disable-nls --without-guile
    make -j4 )
  cp "$local_make/make" "$SRC/make-bin"
fi
cp "$SRC/make-bin" "$DEST/libmake.so"

# --- Tiny C Compiler --------------------------------------------------------
# Two cross-compile traps, both handled here:
#
#  1. `c2str.exe` is a BUILD-HOST tool (built from conftest.c). With CC pointing
#     at the Android clang it comes out ARM64 and cannot run on the build host,
#     so build it natively and freeze its output first.
#  2. bionic folds pthread into libc — there is no `-lpthread` to link against,
#     so `LIBS` has to be overridden.
#
# `libtcc1.a` is deliberately NOT built: tcc generates it by RUNNING tcc, which
# cannot happen on the host. `tcc -c` does not need it.
if [ ! -x "$SRC/tcc-bin" ]; then
  echo "→ building tcc"
  rm -rf "$SRC/tcc" && mkdir -p "$SRC/tcc"
  tar -C "$SRC/tcc" -xzf "$TCC_TGZ"
  tcc_dir="$SRC/tcc/$(ls "$SRC/tcc" | head -1)"
  ( cd "$tcc_dir"
    ./configure --cc="$CC" --cpu=arm64 --targetos=Linux --extra-cflags="-fPIC -O2"
    cc -DC2STR conftest.c -o c2str.exe
    ./c2str.exe include/tccdefs.h tccdefs_.h
    touch tccdefs_.h
    make tcc LIBS="-lm -ldl" )
  cp "$tcc_dir/tcc" "$SRC/tcc-bin"
fi
cp "$SRC/tcc-bin" "$DEST/libtcc.so"

echo "→ staged into $DEST"
for f in libmake.so libtcc.so; do
  printf '  %-14s %s\n' "$f" "$(du -h "$DEST/$f" | cut -f1)"
done
echo "→ build the helper APK: ( cd android-tools && gradle assembleDebug )"
