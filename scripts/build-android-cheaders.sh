#!/usr/bin/env bash
# Build the `cheaders` data package: everything needed to compile C on the phone.
#
# Why a package and not part of the runtime: the headers are useful to `tcc`
# (from the helper APK), to Python extension authors and to Lua module authors
# alike, and are useless to a device that only runs scripts. They are shipped as
# a **data-only** package — no entry, no command name, nothing to run.
#
# Output: dist/cheaders-<version>-arm64.tar.gz
#
# Install: POST /packages/install (or the plugin page). Nothing is executable.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BUILD=${BUILD:-$HOME/android-git-build}
NDK=${NDK:-$BUILD/ndk/android-ndk-r26b}
PYTHON_PREFIX=${PYTHON_PREFIX:-$BUILD/pyprefix}
LUA_SRC=${LUA_SRC:-$HOME/luasrc/lua-5.4.7/src}
VERSION=${VERSION:-1.0.0}
API=${API:-26}

NDK_BIN=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin
CC=$NDK_BIN/aarch64-linux-android${API}-clang
SYSROOT=$NDK/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr
INCLUDE=$SYSROOT/include
ARCH_INCLUDE=$INCLUDE/aarch64-linux-android
LIBDIR=$SYSROOT/lib/aarch64-linux-android/$API
CLANG_INCLUDE=$(ls -d "$NDK"/toolchains/llvm/prebuilt/linux-x86_64/lib/clang/*/include | head -1)

WORK=${WORK:-$BUILD/cheaders-pkg}
OUT=$REPO/dist

for need in "$CC" "$INCLUDE/stdio.h" "$LIBDIR/libc.so"; do
  [ -e "$need" ] || { echo "error: missing $need" >&2; exit 1; }
done

echo "→ assembling $WORK"
rm -rf "$WORK"
mkdir -p "$WORK/include" "$WORK/lib" "$WORK/builtin"

# bionic, minus the graphics/media trees that no C script needs. `.` merge of the
# arch directory afterwards: aarch64's `asm/` headers live there, not in the
# common include dir.
(
  cd "$INCLUDE"
  for item in *; do
    case "$item" in
      EGL|GLES|GLES2|GLES3|KHR|OMXAL|SLES|vulkan|android|media|OMX|camera2|drm) continue ;;
    esac
    cp -R "$item" "$WORK/include/" 2>/dev/null || true
  done
)
cp -R "$ARCH_INCLUDE/." "$WORK/include/" 2>/dev/null || true
# `sys/cdefs.h` needs android/versioning.h, so put that one subtree back.
cp -R "$INCLUDE/android" "$WORK/include/" 2>/dev/null || true

# Compiler-provided (freestanding) headers: stddef.h, stdarg.h, … tcc looks for
# these under its install prefix, which does not exist on Android.
for header in stddef.h stdarg.h stdbool.h float.h stdalign.h stdnoreturn.h; do
  cp "$CLANG_INCLUDE/$header" "$WORK/builtin/" 2>/dev/null || true
done

# Link-time pieces. tcc wants crti.o/crtn.o; Android ships neither, so empty
# objects stand in (they only carry .init/.fini prologues a shared object does
# not need). libc/libm/libdl/liblog are the linker stubs, not the runtime libs.
cp "$LIBDIR/crtbegin_so.o" "$LIBDIR/crtend_so.o" "$WORK/lib/" 2>/dev/null || true
cp "$LIBDIR/libc.so" "$LIBDIR/libm.so" "$LIBDIR/libdl.so" "$LIBDIR/liblog.so" "$WORK/lib/" 2>/dev/null || true
: > "$WORK/empty.c"
"$CC" -c -o "$WORK/lib/crti.o" "$WORK/empty.c"
"$CC" -c -o "$WORK/lib/crtn.o" "$WORK/empty.c"
rm -f "$WORK/empty.c"

# Language headers, so one pack covers C, Python extensions and Lua modules.
cp -R "$PYTHON_PREFIX/include/python3.12" "$WORK/include/python3.12" 2>/dev/null || true
mkdir -p "$WORK/include/lua5.4"
for header in lua.h luaconf.h lualib.h lauxlib.h lua.hpp; do
  cp "$LUA_SRC/$header" "$WORK/include/lua5.4/" 2>/dev/null || true
done

cat > "$WORK/package.json" <<JSON
{
  "name": "cheaders",
  "version": "$VERSION",
  "description": "C headers for compiling on the device (bionic + compiler builtins + Python + Lua, with CRTs and libc stubs)"
}
JSON

cat > "$WORK/README.md" <<'MD'
# cheaders — compile C on the phone with tcc

Nothing here is executable; this package is data. `tcc` (from the helper APK)
has no system include path on Android, so point it at this package explicitly.

```sh
P=$HOME/packages/cheaders
tcc -shared -fPIC -nostdlib -DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD \
    -I$P/include -I$P/builtin -I$P/include/python3.12 -I$P/include/lua5.4 \
    -L$P/lib -lc -o mylib.so mylib.c
```

| Path | What |
|---|---|
| `include/` | bionic (libc) headers |
| `builtin/` | compiler-provided headers (`stddef.h`, `stdarg.h`, …) |
| `lib/` | `crti.o`/`crtn.o` + `libc`/`libm`/`libdl`/`liblog` linker stubs |
| `include/python3.12/` | CPython headers (extension modules) |
| `include/lua5.4/` | Lua headers (C modules for `require`) |

Two flags are not optional:

- `-nostdlib` — tcc looks for CRT files under `/usr/local/lib/tcc`, which does
  not exist on Android.
- `-DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD` — bionic declares `ioctl` a second
  time behind `__overloadable`, which tcc does not implement and reads as a
  conflicting redefinition.

The output is a **shared object**, loadable through `ctypes` or Lua `require`.
Executables are not usable: shared storage and the app data directory are both
mounted `noexec`.
MD

echo "→ packing"
mkdir -p "$OUT"
ARCHIVE="$OUT/cheaders-$VERSION-arm64.tar.gz"
rm -f "$ARCHIVE"
tar -C "$WORK" -czf "$ARCHIVE" .
printf '  %-22s %s\n' "cheaders" "$(du -h "$ARCHIVE" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256 /'
