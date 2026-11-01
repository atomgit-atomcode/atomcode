#!/usr/bin/env bash
# 为设备构建 lupa（Python ↔ Lua 桥接）的 wheel。
#
# 为什么不能直接 `pip install lupa`：PyPI 上只有 manylinux 的 aarch64 轮子，那些链的是
# glibc，设备上没有；Chaquopy 的 Android 轮子仓库里有 numpy 但没有 lupa。设备上自己编也
# 不行——没有系统 C 头文件。所以只能对着同一套 CPython 交叉编译。
#
# lupa 的 sdist 自带 Lua 源码（`third-party/lua54/`）和已生成好的 Cython C 文件
# （`lupa/lua54.c`），所以既不需要联网取 Lua，也不需要 Cython：一个翻译单元 + Lua 核心
# 一起编成 `lupa/lua54.so` 即可。这正是它比 numpy 好办的地方。
#
# 输出：dist/lupa-<version>-cp312-cp312-linux_aarch64.whl
#
# 装到设备（平台标签必须显式给，pip 推不出 Android）：
#   python3 -m pip install --platform linux_aarch64 --only-binary=:all: \
#     --no-deps --target $HOME/python-packages /path/to/lupa-….whl
#
# 需要：NDK clang，以及 `dist/cpython-<ver>-arm64/include/python3.12`（仓库里已有的
# cpython 产物），即 scripts/build-android.sh 产出的那套前缀。
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=$REPO/dist
NDK=${ANDROID_NDK_HOME:-$REPO/.android-sdk/ndk/26.1.10909125}
HOST_TAG=windows-x86_64
case "$(uname -s)" in
  Linux*) HOST_TAG=linux-x86_64 ;;
  Darwin*) HOST_TAG=darwin-x86_64 ;;
esac
CC=${CC:-$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin/aarch64-linux-android26-clang}
PYINC=${PYINC:-$(ls -d "$OUT"/cpython-*-arm64/include/python3.12 2>/dev/null | head -1 || true)}

SOURCE=${1:-}
if [ -z "$SOURCE" ]; then
  SOURCE=$(ls -1 "$REPO"/lupa-*.tar.gz 2>/dev/null | head -1 || true)
fi
if [ -z "$SOURCE" ] || [ ! -f "$SOURCE" ]; then
  echo "usage: $0 <lupa-sdist.tar.gz>" >&2
  echo "  (sdist from PyPI; the build host may not reach it, so it is not downloaded here)" >&2
  exit 2
fi
if [ -z "${PYINC:-}" ] || [ ! -f "$PYINC/Python.h" ]; then
  echo "error: 找不到 CPython 头文件（PYINC=$PYINC）" >&2
  echo "  先产出 dist/cpython-*-arm64（scripts/build-android.sh）" >&2
  exit 2
fi
for tool in "$CC"; do
  [ -x "$tool" ] || { echo "error: 找不到编译器 $tool" >&2; exit 2; }
done

WORK=$OUT/lupa-build
rm -rf "$WORK"
mkdir -p "$WORK"
tar -C "$WORK" -xzf "$SOURCE"
SRC=$(ls -d "$WORK"/lupa-*/ | head -1)
cd "$SRC"

VERSION=$(python3 -c 'import re;print(re.search(r"__version__\s*=\s*[\"\x27]([^\"\x27]+)",open("lupa/version.py").read()).group(1))')
echo "→ lupa $VERSION"

LUA=third-party/lua54
[ -f "$LUA/lapi.c" ] || { echo "error: sdist 里没有 $LUA（Lua 源码）" >&2; exit 2; }

# Lua 核心的对象清单直接取它自己的 Makefile，避免手抄漏文件。
LUA_SRCS=$(python3 - "$LUA/Makefile" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
names, keep = [], False
for line in text.splitlines():
    line = line.split('#', 1)[0].rstrip()
    if not line.strip():
        continue
    m = re.match(r'(CORE|AUX|LIB)_O\s*=(.*)', line)
    if m:
        names += m.group(2).replace('\\', '').split()
        keep = line.endswith('\\')
    elif keep and line.startswith((' ', '\t')):
        names += line.replace('\\', '').split()
        keep = line.endswith('\\')
    else:
        keep = False
print(' '.join(n[:-2] + '.c' for n in names if n.endswith('.o')))
PY
)
COUNT=$(printf '%s\n' $LUA_SRCS | wc -l)
echo "→ 编译 lupa/lua54 + $COUNT 个 Lua 源文件"

# LUA_USE_POSIX 让 os.time/os.date 走 POSIX 实现；不定义 LUA_USE_LINUX，它会连带
# 要求 readline（设备上没有）。
#
# `-lm` 不能省：Lua 的 `lmathlib.c` 用 `pow`，它只住在 libm 里。构建机上宿主把
# libm 并进了 libc，所以漏掉这一行照样编得过；到了 bionic 上就是
# `dlopen failed: cannot locate symbol "pow"`（真机上踩到过一次）。
"$CC" -shared -fPIC -O2 -DLUA_USE_POSIX \
  -I"$PYINC" -I"$LUA" \
  -o lua54.cpython-312.so \
  lupa/lua54.c $(for f in $LUA_SRCS; do printf '%s/%s ' "$LUA" "$f"; done) \
  -lm \
  -Wl,-soname,lua54.cpython-312.so

echo "→ 组装 wheel"
rm -rf wheel
mkdir -p wheel/lupa
# 只要运行时要的东西：模块初始化脚本、版本、以及编好的扩展。.pyx/.pxi 是构建输入。
cp lupa/__init__.py lupa/version.py wheel/lupa/
cp lua54.cpython-312.so wheel/lupa/
DIST="lupa-$VERSION.dist-info"
mkdir -p "wheel/$DIST"
printf 'Metadata-Version: 2.1\nName: lupa\nVersion: %s\nSummary: Python wrapper around Lua and LuaJIT.\nRequires-Python: >=3.8\n' \
  "$VERSION" > "wheel/$DIST/METADATA"
# 标签决定 pip 是否接受：cp312 对上运行时 ABI，linux_aarch64 是安装时要求的平台。
printf 'Wheel-Version: 1.0\nGenerator: atomcode-android\nRoot-Is-Purelib: false\nTag: cp312-cp312-linux_aarch64\n' \
  > "wheel/$DIST/WHEEL"
python3 - "$VERSION" <<'PY'
import base64, hashlib, os, sys, zipfile
version = sys.argv[1]
root, dist = "wheel", f"lupa-{version}.dist-info"
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
out = f"lupa-{version}-cp312-cp312-linux_aarch64.whl"
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
    for rel in names + [f"{dist}/RECORD"]:
        zf.write(os.path.join(root, rel), rel)
print("   ", out, os.path.getsize(out), "bytes")
PY

mkdir -p "$OUT"
cp "$(ls lupa-*.whl | head -1)" "$OUT/"
echo "→ $OUT/$(ls lupa-*.whl | head -1)"