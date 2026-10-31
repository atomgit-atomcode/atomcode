#!/usr/bin/env bash
# Build the CPython runtime package for AtomCode on Android.
#
# The package is what `pluginhost` runs. Android never lets the app exec a file
# from its data directory, so the interpreter ships as a shared library plus a
# `plugin_main` shim; `pluginhost` dlopens the shim and CPython takes over.
#
# Layout inside the bundle:
#
#   package.json              manifest read by pluginhost
#   lib/plugin.so             the shim (exports plugin_main, links libpython)
#   lib/libpython3.12.so.1.0  the interpreter (its SONAME, so the loader finds it)
#   lib/python3.12/…          the standard library
#
# Requires a CPython built with `--enable-shared --host=aarch64-linux-android`
# (see docs/android-port.md §P5.7 for the configure line) plus the Linux NDK.
#
#   PYTHON_PREFIX=~/android-git-build/pyprefix \
#   ANDROID_NDK_LINUX_HOME=~/android-git-build/ndk/android-ndk-r26b \
#     bash scripts/build-android-cpython-plugin.sh
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
PYTHON_PREFIX="${PYTHON_PREFIX:-$HOME/android-git-build/pyprefix}"
NDK="${ANDROID_NDK_LINUX_HOME:-$HOME/android-git-build/ndk/android-ndk-r26b}"
API="${ATOMCODE_ANDROID_API:-26}"
TARGET=aarch64-linux-android
VERSION="${CPYTHON_VERSION:-3.12.10}"
OUT="${OUT:-$REPO/dist/cpython-$VERSION-arm64}"

BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
CC="$BIN/${TARGET}${API}-clang"
for need in "$CC" "$PYTHON_PREFIX/include/python3.12/Python.h" \
            "$PYTHON_PREFIX/lib/libpython3.12.so.1.0"; do
  [ -e "$need" ] || { echo "error: missing $need" >&2; exit 1; }
done

echo "→ assembling $OUT"
rm -rf "$OUT"
mkdir -p "$OUT/lib"

# The interpreter, under its SONAME: the shim's DT_NEEDED names this exact file,
# and `-rpath $ORIGIN` makes the loader look beside the shim.
cp "$PYTHON_PREFIX/lib/libpython3.12.so.1.0" "$OUT/lib/libpython3.12.so.1.0"
# Two names for one file. Our own DT_NEEDED entries were rewritten to the short
# SONAME (below), and a plain `dlopen` — the health probe, or any third-party tool
# — resolves a DT_NEEDED by *file name* on the search path. Shipping the short
# name as a link keeps both lookups working against a single loaded instance.
ln -sf libpython3.12.so.1.0 "$OUT/lib/libpython3.12.so"

# Wheels built elsewhere (Chaquopy, Termux) name their dependency
# `libpython3.12.so`, while our build's SONAME carries the `.1.0` suffix. The
# loader matches an already-loaded library by SONAME, so the short name never
# resolves and any such extension fails with "library libpython3.12.so not
# found". Rewriting the SONAME string in place (same byte length, NUL padded)
# makes one instance satisfy both names: the file keeps its `.so.1.0` filename
# for our own DT_NEEDED, and registers the short name for third-party modules.
PYLIB="$OUT/lib/libpython3.12.so.1.0"
python3 - "$PYLIB" <<'PATCH'
import sys
path = sys.argv[1]
with open(path, "rb") as handle:
    data = bytearray(handle.read())
old = b"libpython3.12.so.1.0"
short = b"libpython3.12.so"
new = short + bytes(len(old) - len(short))
assert len(new) == len(old)
count = data.count(old)
assert count == 1, f"expected one SONAME occurrence, found {count}"
begin = data.index(old)
data[begin:begin + len(old)] = new
with open(path, "wb") as handle:
    handle.write(data)
print("   libpython SONAME -> libpython3.12.so")
PATCH

echo "→ compiling the plugin_main shim"
"$CC" -shared -fPIC -O2 \
  -I"$PYTHON_PREFIX/include/python3.12" \
  -o "$OUT/lib/plugin.so" \
  "$REPO/scripts/cpython-plugin/plugin.c" \
  -L"$OUT/lib" -l:libpython3.12.so.1.0 \
  -Wl,-rpath,'$ORIGIN'

echo "→ pruning the standard library"
# `test` alone is 133 MB of CPython's own test suite; the rest is dead weight on
# a phone (no Tk, no IDLE). `ensurepip` is deliberately KEPT: it carries the pip
# wheel, which is only useful now that `_ssl` is built in (a pip that cannot
# reach an index is a trap, not a feature).
STDLIB="$OUT/lib/python3.12"
mkdir -p "$STDLIB"
tar -C "$PYTHON_PREFIX/lib/python3.12" \
  --exclude='__pycache__' \
  --exclude='test' --exclude='tests' \
  --exclude='idlelib' --exclude='tkinter' --exclude='turtledemo' \
  --exclude='lib2to3' --exclude='pydoc_data' \
  --exclude='config-3.12' --exclude='site-packages' \
  -cf - . | tar -C "$STDLIB" -xf -

echo "→ installing pip into site-packages"
# `ensurepip` cannot run here: it bootstraps pip by spawning the interpreter,
# and an embedded CPython reports an empty `sys.executable`, so the spawn dies
# with `Permission denied: ''`. Unpacking the wheel `ensurepip` already ships
# gives the same result without a subprocess, and it is reproducible.
PIP_WHL=$(ls "$STDLIB"/ensurepip/_bundled/pip-*.whl 2>/dev/null | head -1)
if [ -n "$PIP_WHL" ]; then
  mkdir -p "$STDLIB/site-packages"
  # `unzip` is not guaranteed on a build host; python's zipfile is.
  python3 -m zipfile -e "$PIP_WHL" "$STDLIB/site-packages"
  echo "   pip from $(basename "$PIP_WHL")"
else
  echo "   no bundled pip wheel found; skipping"
fi

echo "→ shipping sitecustomize"
# An embedded CPython has an empty `sys.executable`; the shim exports the real
# launcher path and this module assigns it. Without it anything that re-executes
# the interpreter fails with `Permission denied: ''`.
if [ -f "$REPO/scripts/cpython-plugin/sitecustomize.py" ]; then
  mkdir -p "$STDLIB/site-packages"
  cp "$REPO/scripts/cpython-plugin/sitecustomize.py" "$STDLIB/site-packages/"
  echo "   sitecustomize.py → site-packages"
fi

echo "→ building _posixshmem"
# bionic has no `shm_open`/`shm_unlink`, so configure turns `_posixshmem` off and
# `multiprocessing`'s resource tracker — which imports it unconditionally — takes
# the `spawn` start method and `shared_memory` down with it. Build our own,
# backed by files under $TMPDIR.
DYNALOAD="$STDLIB/lib-dynload"
if [ -d "$DYNALOAD" ] && [ -f "$REPO/scripts/cpython-plugin/posixshmem.c" ]; then
  "$CC" -shared -fPIC -O2 \
    -I"$PYTHON_PREFIX/include/python3.12" \
    -o "$DYNALOAD/_posixshmem.cpython-312.so" \
    "$REPO/scripts/cpython-plugin/posixshmem.c" \
    -L"$PYTHON_PREFIX/lib" -lpython3.12
  echo "   _posixshmem.cpython-312.so"
fi

echo "→ packing headers (Python.h)"
# Without these the interpreter can only be *used*, never built against — and
# `sysconfig.get_config_var("INCLUDEPY")` points at a path that does not exist.
if [ -d "$PYTHON_PREFIX/include/python3.12" ]; then
  mkdir -p "$OUT/include"
  cp -R "$PYTHON_PREFIX/include/python3.12" "$OUT/include/"
fi

cat > "$OUT/package.json" <<JSON
{
  "name": "cpython",
  "version": "$VERSION",
  "description": "CPython $VERSION interpreter (shared library, loaded by pluginhost)",
  "entry": "lib/plugin.so",
  "symbol": "plugin_main",
  "aliases": ["python3", "python"],
  "preload": ["lib/libpython3.12.so.1.0"],
  "data_dirs": ["python-packages", ".local"]
}
JSON

# Every extension in this package was linked against `libpython3.12.so.1.0`,
# and bionic resolves a DT_NEEDED only against a loaded library whose SONAME
# matches exactly. Now that the SONAME is the short name, the old references
# must be rewritten too — otherwise our own `_posixshmem` fails with
# "library libpython3.12.so.1.0 not found" while a third-party wheel works.
python3 - "$OUT" <<'NEEDED'
import os
import sys

root = sys.argv[1]
old = b"libpython3.12.so.1.0"
short = b"libpython3.12.so"
new = short + bytes(len(old) - len(short))
patched = 0
for base, _dirs, files in os.walk(root):
    for name in files:
        if not name.endswith(".so"):
            continue
        path = os.path.join(base, name)
        with open(path, "rb") as handle:
            data = handle.read()
        if old not in data:
            continue
        with open(path, "wb") as handle:
            handle.write(data.replace(old, new))
        patched += 1
print(f"   rewrote libpython soname reference in {patched} shared objects")
NEEDED

ARCHIVE="$OUT.tar.gz"
echo "→ packing $ARCHIVE"
tar -C "$OUT" -czf "$ARCHIVE" .
printf '  bundle  %s\n' "$(du -h "$ARCHIVE" | cut -f1)"
printf '  unpacked %s\n' "$(du -sh "$OUT" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256  /'
echo "→ install on device: POST /packages/install {\"path\": \"…/$VERSION.tar.gz\"}"
