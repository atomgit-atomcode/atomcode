#!/usr/bin/env bash
# 把 Zig 的标准库打成**数据包**，让 `zig` 装完即可用。
#
# 为什么需要它：zig 的二进制在 helper APK 的 nativeLibraryDir 里（Android 只允许从那里
# 执行），而它启动时要找自己的标准库——二进制旁边没有 `lib/`，所以只认 `$ZIG_LIB_DIR`。
# 插件清单因此声明一条产品目录链接 `{"zig": "lib"}`：安装后 `<product>/zig` 指向包内的
# 标准库目录，daemon 在安装时与启动时都会把 `ZIG_LIB_DIR` 设成它。
#
# 注意是 `<product>/zig` 本身：zig 在 `$ZIG_LIB_DIR` **底下**找 `std/`，所以那个目录就是
# 标准库根，不能在底下再拼一层 `lib`——那层目录并不存在，变量会压根没被设上。
#
# 输入：dist/zig-lib.tar.gz（`tar czf dist/zig-lib.tar.gz -C <toolchain> lib`）
# 输出：dist/zig-lib-<version>-arm64.tar.gz
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=$REPO/dist
VERSION=${VERSION:-0.13.0}
SOURCE=${1:-$OUT/zig-lib.tar.gz}

[ -f "$SOURCE" ] || {
  echo "error: missing $SOURCE" >&2
  echo "  先生成：tar czf $OUT/zig-lib.tar.gz -C <zig-toolchain> lib" >&2
  exit 1
}

WORK=$OUT/zig-lib-plugin
PKG=$WORK/zig-lib
echo "→ 组装 zig-lib $VERSION"
rm -rf "$WORK"
mkdir -p "$PKG"
tar -C "$PKG" -xzf "$SOURCE"

[ -d "$PKG/lib" ] || { echo "error: $SOURCE 里没有 lib/" >&2; exit 1; }

cat > "$PKG/package.json" <<'JSON'
{
  "name": "zig-lib",
  "version": "__VERSION__",
  "description": "Zig 标准库：配合 helper APK 里的 zig 二进制使用（install 后 <product>/zig 可用）",
  "entry": "",
  "links": { "zig": "lib" }
}
JSON
# 版本要真的写进去（heredoc 里不能展开，这里补一刀）。
python3 - "$PKG/package.json" "$VERSION" <<'PY'
import io, sys
path, version = sys.argv[1], sys.argv[2]
text = io.open(path, encoding="utf-8").read().replace("__VERSION__", version)
io.open(path, "w", encoding="utf-8", newline="\n").write(text)
PY

cat > "$PKG/README.md" <<'MD'
# zig-lib

Zig 的标准库。**不是**一个能直接跑的命令——它只把 `<product>/zig` 链接到自己的 `lib/`，
供 helper APK 里的 `zig` 使用。

装完后 `$ZIG_LIB_DIR` 指向 `<product>/zig`（链接本身就是标准库目录，`std/` 就在它底下），
daemon 在安装时就会设好；如果当时 `zig` 还没出现（helper 未安装），重启一次应用即可。

```sh
zig version            # 0.13.0
zig cc -shared -fPIC -o lib.so lib.c
```

不需要 zig 时直接卸载，`<product>/zig` 会一并摘掉。
MD

ARCHIVE="$OUT/zig-lib-$VERSION-arm64.tar.gz"
rm -f "$ARCHIVE"
tar -C "$PKG" -czf "$ARCHIVE" .
printf '  %-22s %s\n' "zig-lib" "$(du -h "$ARCHIVE" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256 /'
