#!/usr/bin/env bash
# 把 lupa 打成「Python 载荷」插件。
#
# 为什么打包而不是 `pip install`：和 cffi 一样——设备上装它要么联网、要么本地有 wheel；
# 做成包之后可浏览、可回滚、可精确卸载。lupa 的载荷是纯扩展 + 少量 .py，没有额外
# 运行时依赖（Lua 已经静态进 `lua54.so`），所以不需要像 cffi 那样带上 pycparser。
#
# 载荷通过 `python_packages` 暴露：安装时 daemon 把 `lib/python/*` 软链进
# `<product>/python-packages`，运行时的 `PYTHONPATH` 已经包含那里。
#
# 用法：scripts/build-android-lupa-plugin.sh [lupa.whl]
# 默认取 dist/ 下最新的 lupa-*-cp312-cp312-linux_aarch64.whl
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=$REPO/dist

LUPA_WHEEL=${1:-$(ls -1t "$OUT"/lupa-*-cp312-cp312-linux_aarch64.whl 2>/dev/null | head -1 || true)}
[ -f "${LUPA_WHEEL:-}" ] || {
  echo "usage: $0 <lupa.whl>" >&2
  echo "  lupa wheel 由 scripts/build-android-lupa-wheel.sh 产出" >&2
  exit 2
}

VERSION=$(basename "$LUPA_WHEEL" | sed -E 's/^lupa-([0-9.]+)-.*$/\1/')

WORK=$OUT/lupa-plugin
PKG=$WORK/lupa
echo "→ 组装 lupa $VERSION"
rm -rf "$WORK"
mkdir -p "$PKG/lib/python"

# wheel 就是 zip；用 python 的 zipfile，构建机不必有 unzip。
python3 -m zipfile -e "$LUPA_WHEEL" "$PKG/lib/python"

# 扩展必须真的在：少了它 lupa 只剩 __init__.py，一 import 就报
# `cannot import any version of the lupa Lua wrapper`。
ls "$PKG/lib/python/lupa"/lua54*.so >/dev/null 2>&1 || {
  echo "error: wheel 里没有 lua54 扩展（是不是拿错了 manylinux 包？）" >&2
  exit 1
}

# 和 cffi 同样的理由：扩展是早些时候编的，DT_NEEDED 里若写着长的 libpython 名，
# 运行时按短名 SONAME 匹配不上。打包时等长改写。
python3 "$REPO/scripts/plugin_payload_fixup.py" "$PKG/lib/python"

cat > "$PKG/package.json" <<JSON
{
  "name": "lupa",
  "version": "$VERSION",
  "description": "lupa $VERSION —— Python 里调用 Lua（Lua 5.4 已静态编进扩展）",
  "python_packages": "lib/python"
}
JSON

cat > "$PKG/README.md" <<'MD'
# lupa

在 Python 里调用 Lua。Lua 5.4 已经静态编进 `lua54` 扩展，不需要另外装 Lua 解释器。

```python
import lupa.lua54 as lua
rt = lua.LuaRuntime()
print(rt.eval('1 + 2'))            # 3
print(rt.execute('return "hi"'))   # hi
```

两个 Lua 与 Python 互通的要点：

- **Python 对象进 Lua**：`rt.globals().pyobj = [1, 2, 3]`，Lua 侧按表用；
- **Lua 函数回 Python**：`rt.eval('function(x) return x * 2 end')` 得到可直接调用的对象。

C 写的 Lua 模块（`require 'cjson'` 这种）需要 `lupa.allow_lua_module_loading()`
包住 import，且模块本身得是给设备编的 aarch64 版本。
MD

ARCHIVE="$OUT/lupa-$VERSION-arm64.tar.gz"
rm -f "$ARCHIVE"
tar -C "$PKG" -czf "$ARCHIVE" .
printf '  %-22s %s\n' "lupa" "$(du -h "$ARCHIVE" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256 /'