#!/usr/bin/env bash
# 构建 svg2png 插件：把 SVG 光栅化成 PNG，让 agent 能「画完自己看」。
#
# 依赖只有 vendor/ 下的三个头文件（nanosvg / nanosvgrast / stb_image_write），
# 所以构建完全离线。
#
# 输出：dist/svg-<version>-arm64.tar.gz
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BUILD=${BUILD:-$HOME/android-git-build}
NDK=${NDK:-$BUILD/ndk/android-ndk-r26b}
API=${API:-26}
VERSION=${VERSION:-1.0.0}
OUT=$REPO/dist

CC=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android${API}-clang
VENDOR=$REPO/scripts/svg-plugin/vendor

for header in nanosvg.h nanosvgrast.h stb_image_write.h; do
  [ -f "$VENDOR/$header" ] || { echo "error: missing vendor/$header" >&2; exit 1; }
done

WORK=$BUILD/svg-plugin
PKG=$WORK/svg
echo "→ 组装 $PKG"
rm -rf "$WORK"
mkdir -p "$PKG/lib"

echo "→ 编译 plugin.so"
"$CC" -shared -fPIC -O2 -I"$VENDOR" \
  -o "$PKG/lib/plugin.so" \
  "$REPO/scripts/svg-plugin/plugin.c" -lm

# nanosvg 的解析器要读文件，符号表里留个印记以便核对链接是否真的带上了
echo "   size $(stat -c%s "$PKG/lib/plugin.so" 2>/dev/null || stat -f%z "$PKG/lib/plugin.so") bytes"

cat > "$PKG/package.json" <<JSON
{
  "name": "svg",
  "version": "$VERSION",
  "description": "SVG 渲染：把 .svg 光栅化成 .png（nanosvg + stb_image_write），让 agent 能看见自己画的东西",
  "entry": "lib/plugin.so",
  "symbol": "plugin_main",
  "aliases": ["svg2png"]
}
JSON

cat > "$PKG/README.md" <<'MD'
# svg（svg2png）

把 SVG 光栅化为 PNG。用途是闭合「画 → 看 → 改」：agent 写出的 SVG 渲染成图片后，
`read_file` 就能显示，不需要浏览器或 GUI 工具链。

```sh
svg2png in.svg out.png          # 按 SVG 自身尺寸（96 dpi）
svg2png in.svg out.png 720      # 指定输出宽度，等比缩放
```

体积小（~100 KB），无外部依赖：nanosvg 与 stb_image_write 都是随包编译的头文件实现。

支持范围是 nanosvg 的能力：路径、基本图形、渐变、变换、`<text>` **不支持**（文字需要
字体引擎，见后续的 text 渲染插件）。需要文字时先用路径轮廓或位图代替。
MD

ARCHIVE="$OUT/svg-$VERSION-arm64.tar.gz"
rm -f "$ARCHIVE"
tar -C "$PKG" -czf "$ARCHIVE" .
printf '  %-22s %s\n' "svg" "$(du -h "$ARCHIVE" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256 /'
