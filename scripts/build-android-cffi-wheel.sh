#!/usr/bin/env bash
# Build a cffi wheel that installs on this device.
#
# Why this exists: `cffi` is not a pure-Python package. PyPI has no
# `linux_aarch64_android` wheel for it — the manylinux ones link against glibc,
# which the device does not have — and building it on the phone is impossible
# (no system C headers). So it is cross-compiled here against the same CPython
# and libffi the runtime packages were built with.
#
# Output: dist/cffi-<version>-cp312-cp312-linux_aarch64.whl
#
# Install on device (the platform flag is required: pip cannot infer Android):
#   python3 -m pip install --platform linux_aarch64 --only-binary=:all: \
#     --no-deps --target $HOME/python-packages /path/to/cffi-….whl
#
# Needs: NDK clang, the CPython prefix (headers + libpython3.12) and a static
# libffi, i.e. the same prefixes scripts/build-android.sh produces.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BUILD=${BUILD:-$HOME/android-git-build}
NDK_BIN=$BUILD/ndk/android-ndk-r26b/toolchains/llvm/prebuilt/linux-x86_64/bin
PREFIX=${PREFIX:-$BUILD/prefix}
PYTHON_PREFIX=${PYTHON_PREFIX:-$BUILD/pyprefix}
CC=${CC:-$NDK_BIN/aarch64-linux-android26-clang}

SOURCE=${1:-}
WORK=$BUILD/cffi-build
OUT=$REPO/dist

if [ -z "$SOURCE" ]; then
  # Prefer a local tarball; the build host cannot always reach PyPI.
  SOURCE=$(ls -1 "$REPO"/cffi-src.tar.gz 2>/dev/null | head -1 || true)
fi
if [ -z "$SOURCE" ] || [ ! -f "$SOURCE" ]; then
  echo "usage: $0 <cffi-sdist.tar.gz>" >&2
  echo "  (download it on a machine that reaches a mirror, e.g. with the device:" \
       "bin/python3 -c 'urllib.request…')" >&2
  exit 2
fi

rm -rf "$WORK"
mkdir -p "$WORK"
tar -C "$WORK" -xzf "$SOURCE"
SRC=$(ls -d "$WORK"/cffi-*/ | head -1)
cd "$SRC"

VERSION=$(python3 -c 'import re,sys;print(re.search(r"version\s*=\s*\"([^\"]+)\"",open("pyproject.toml").read()).group(1))')
echo "→ cffi $VERSION"

# One translation unit plus libffi; no Python build system involved.
echo "→ compiling _cffi_backend"
"$CC" -shared -fPIC -O2 \
  -I"$PYTHON_PREFIX/include/python3.12" -I"$PREFIX/include" \
  -o _cffi_backend.cpython-312.so src/c/_cffi_backend.c \
  -L"$PREFIX/lib" -lffi -L"$PYTHON_PREFIX/lib" -lpython3.12 \
  -Wl,-soname,_cffi_backend.cpython-312.so

echo "→ assembling the wheel"
rm -rf wheel
mkdir -p wheel
cp -R src/cffi wheel/cffi
cp _cffi_backend.cpython-312.so wheel/
DIST="cffi-$VERSION.dist-info"
mkdir -p "wheel/$DIST"
printf 'Metadata-Version: 2.1\nName: cffi\nVersion: %s\nSummary: Foreign Function Interface for Python calling C code.\nRequires-Python: >=3.9\nRequires-Dist: pycparser\n' \
  "$VERSION" > "wheel/$DIST/METADATA"
# The tag is what makes pip accept it: cp312 matches the runtime's ABI, and
# linux_aarch64 is the platform the installer is told to expect.
printf 'Wheel-Version: 1.0\nGenerator: atomcode-android\nRoot-Is-Purelib: false\nTag: cp312-cp312-linux_aarch64\n' \
  > "wheel/$DIST/WHEEL"
python3 - "$VERSION" <<'PY'
import base64, hashlib, os, sys, zipfile
version = sys.argv[1]
root = "wheel"
dist = f"cffi-{version}.dist-info"
names = []
for base, _dirs, files in os.walk(root):
    for name in files:
        names.append(os.path.relpath(os.path.join(base, name), root))
records = []
for rel in names:
    data = open(os.path.join(root, rel), "rb").read()
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
    records.append(f"{rel},sha256={digest},{len(data)}")
records.append(f"{dist}/RECORD,,")
open(os.path.join(root, dist, "RECORD"), "w").write("\n".join(records) + "\n")
out = f"cffi-{version}-cp312-cp312-linux_aarch64.whl"
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
    for rel in names + [f"{dist}/RECORD"]:
        zf.write(os.path.join(root, rel), rel)
print("   ", out, os.path.getsize(out), "bytes")
PY

mkdir -p "$OUT"
cp "$(ls cffi-*.whl | head -1)" "$OUT/"
echo "→ $OUT/$(ls cffi-*.whl | head -1)"