#!/usr/bin/env bash
# 把 cffi 打成「Python 载荷」插件。
#
# 为什么打包而不是 `pip install`：cffi 带一个编译好的 `_cffi_backend` 扩展，设备上装它要
# 么联网、要么本地有 wheel；做成包之后它和 numpy 一样可浏览、可回滚、可精确卸载，而且
# 依赖（pycparser）与本体一起走，不会出现「装上了 cffi 却缺依赖」。
#
# 载荷通过 `python_packages` 暴露：安装时 daemon 把 `lib/python/*` 软链进
# `<product>/python-packages`，运行时的 `PYTHONPATH` 已经包含那里。
#
# 用法：
#   scripts/build-android-cffi-plugin.sh [cffi.whl] [pycparser.whl]
# 默认取 dist/ 下最新的两个 wheel。
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=$REPO/dist

CFFI_WHEEL=${1:-$(ls -1t "$OUT"/cffi-*-cp312-cp312-linux_aarch64.whl 2>/dev/null | head -1 || true)}
PYCPARSER_WHEEL=${2:-$(ls -1t "$OUT"/pycparser-*-py3-none-any.whl 2>/dev/null | head -1 || true)}

[ -f "${CFFI_WHEEL:-}" ] || {
  echo "usage: $0 <cffi.whl> <pycparser.whl>" >&2
  echo "  cffi wheel 由 scripts/build-android-cffi-wheel.sh 产出" >&2
  exit 2
}
[ -f "${PYCPARSER_WHEEL:-}" ] || {
  echo "error: 缺 pycparser wheel（cffi 的运行时依赖，必须随包）" >&2
  exit 2
}

VERSION=$(basename "$CFFI_WHEEL" | sed -E 's/^cffi-([0-9.]+)-.*$/\1/')
PYCPARSER_VERSION=$(basename "$PYCPARSER_WHEEL" | sed -E 's/^pycparser-([0-9.]+)-.*$/\1/')

WORK=$OUT/cffi-plugin
PKG=$WORK/cffi
echo "→ 组装 cffi $VERSION（含 pycparser $PYCPARSER_VERSION）"
rm -rf "$WORK"
mkdir -p "$PKG/lib/python"

# wheel 就是 zip；用 python 的 zipfile，构建机不必有 unzip。
python3 -m zipfile -e "$CFFI_WHEEL" "$PKG/lib/python"
python3 -m zipfile -e "$PYCPARSER_WHEEL" "$PKG/lib/python"

# cffi 的扩展是早前编的，DT_NEEDED 还写着长名 `libpython3.12.so.1.0`；运行时已经把
# SONAME 统一成短名，长名既匹配不上预加载那一份、文件系统里也找不到。打包时等长改写。
python3 "$REPO/scripts/plugin_payload_fixup.py" "$PKG/lib/python"

# 编译好的扩展必须真的在：少了它 cffi 只是纯 Python 壳，一 import 就报
# `No module named '_cffi_backend'`。
ls "$PKG/lib/python"/_cffi_backend*.so >/dev/null 2>&1 || {
  echo "error: cffi wheel 里没有 _cffi_backend 扩展（是不是拿错了 manylinux 包？）" >&2
  exit 1
}

cat > "$PKG/package.json" <<JSON
{
  "name": "cffi",
  "version": "$VERSION",
  "description": "cffi $VERSION + pycparser $PYCPARSER_VERSION —— 在 Python 里调用 C 库（自带编译好的 _cffi_backend）",
  "python_packages": "lib/python"
}
JSON

cat > "$PKG/README.md" <<'MD'
# cffi

在 Python 里调用 C 库。包含编译好的 `_cffi_backend`（aarch64-android）与运行时依赖
`pycparser`，装完即可用：

```python
import cffi
ffi = cffi.FFI()
ffi.cdef("double cos(double);")
libm = ffi.dlopen("libm.so")
print(libm.cos(0.0))     # 1.0
```

ABI 模式（`dlopen` + `cdef`）开箱可用。API 模式（`set_source` + `compile`）需要设备上的
C 编译器，本机没有可用编译器，这条路走不通——要用 API 模式请先在构建机上编好扩展。
MD

ARCHIVE="$OUT/cffi-$VERSION-arm64.tar.gz"
rm -f "$ARCHIVE"
tar -C "$PKG" -czf "$ARCHIVE" .
printf '  %-22s %s\n' "cffi" "$(du -h "$ARCHIVE" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256 /'
