#!/usr/bin/env bash
# Package a prebuilt Android wheel as a plugin.
#
# Why a package instead of `pip install`: numpy is ~30 MB of compiled extensions,
# and on this device it cannot be built (no working cross-compile path, see
# docs/android-port.md). Installing it as a *package* gives what pip cannot:
# a recorded owner, one-click rollback, a size the UI can show, and an uninstall
# that removes exactly the names it added.
#
# The payload is exposed by symlink: the daemon links `lib/python/*` into
# `<product>/python-packages`, which the CPython runtime already has on
# `PYTHONPATH`. One copy on disk, and removal takes the links away.
#
# Usage: scripts/build-android-numpy-plugin.sh [wheel]
#        (defaults to the newest dist/numpy-*-android_21_arm64_v8a.whl)
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=$REPO/dist

WHEEL=${1:-}
if [ -z "$WHEEL" ]; then
  WHEEL=$(ls -1t "$OUT"/numpy-*-android_21_arm64_v8a.whl 2>/dev/null | head -1 || true)
fi
if [ -z "$WHEEL" ] || [ ! -f "$WHEEL" ]; then
  echo "usage: $0 <numpy-*-android_21_arm64_v8a.whl>" >&2
  echo "  Fetch it with pip download --platform android_21_arm64_v8a (see docs)." >&2
  exit 2
fi

NAME=$(basename "$WHEEL" .whl)
# numpy-1.26.2-0-cp312-cp312-android_21_arm64_v8a → version 1.26.2
VERSION=$(printf '%s' "$NAME" | cut -d- -f2)
BUILD_DIR=$OUT/numpy-plugin
PKG=$BUILD_DIR/numpy

echo "→ packaging numpy $VERSION from $(basename "$WHEEL")"
rm -rf "$BUILD_DIR"
mkdir -p "$PKG/lib/python"

# `python3 -m zipfile` rather than `unzip`: not every build host has unzip, and
# the wheel is a zip.
python3 -m zipfile -e "$WHEEL" "$PKG/lib/python"

# The payload carries the shared libraries it links (the `.libs` layout wheels
# use): the dependency belongs to this package, not to the runtime, so a runtime
# upgrade cannot silently break numpy and the runtime does not ship a BLAS it
# never uses. The runtime's shim preloads everything in `python-packages/.libs`
# with RTLD_GLOBAL, which is what makes these resolvable by soname.
VENDOR="$REPO/dist/vendor-libs"
mkdir -p "$PKG/lib/python/.libs"
for lib in libopenblas.so libc++_shared.so; do
  if [ -f "$VENDOR/$lib" ]; then
    cp "$VENDOR/$lib" "$PKG/lib/python/.libs/"
    echo "  + .libs/$lib"
  fi
done

cat > "$PKG/package.json" <<JSON
{
  "name": "numpy",
  "version": "$VERSION",
  "description": "NumPy $VERSION for Android arm64 (prebuilt wheel; exposed on PYTHONPATH)",
  "python_packages": "lib/python"
}
JSON

cat > "$PKG/README.md" <<'MD'
# numpy (Android arm64)

Prebuilt by Chaquopy for `cp312` / `arm64_v8a`, packaged as a plugin rather than
installed with pip so that uninstall is exact and the size is visible.

`import numpy` works because the daemon symlinks `lib/python/*` into
`<product>/python-packages`, which the runtime puts on `PYTHONPATH`.

This wheel links `libopenblas` and `libc++_shared` by plain soname, and an
app-private directory is not on the loader's search path. Both ship inside the
payload under `lib/python/.libs/`, which the daemon also links into
`python-packages`; the CPython shim preloads every `.so` found there with
`RTLD_GLOBAL`, so the sonames resolve before `numpy`'s extensions load.

Numbers are only as good as the BLAS that ships: this build links `libopenblas`,
so `dot`/`matmul`/`linalg` are the threaded paths, not the naive fallbacks.
MD

ARCHIVE="$OUT/numpy-$VERSION-arm64.tar.gz"
rm -f "$ARCHIVE"
tar -C "$PKG" -czf "$ARCHIVE" .
printf '  %-22s %s\n' "numpy" "$(du -h "$ARCHIVE" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256 /'
