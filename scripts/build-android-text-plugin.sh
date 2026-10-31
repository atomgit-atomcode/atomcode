#!/usr/bin/env bash
# 构建 text2png 插件：把文字（含中文）渲染成 PNG。
#
# 补 svg 插件的缺口：nanosvg 不支持 `<text>`，而「画一张带中文标注的图」是常见需求。
# 字体用设备上的系统字体，不随包携带；包里只有 ~200 KB 的 .so。
#
# 依赖：先跑 scripts/build-android-freetype.sh 得到 $FT_PREFIX/lib/libfreetype.a。
# 输出：dist/text-<version>-arm64.tar.gz
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BUILD=${BUILD:-$HOME/android-git-build}
NDK=${NDK:-$BUILD/ndk/android-ndk-r26b}
API=${API:-26}
VERSION=${VERSION:-1.0.0}
OUT=$REPO/dist
FT_PREFIX=${FT_PREFIX:-$HOME/freetype-prefix}

CC=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android${API}-clang
STB=$REPO/scripts/svg-plugin/vendor

[ -f "$FT_PREFIX/lib/libfreetype.a" ] || {
  echo "error: 缺 $FT_PREFIX/lib/libfreetype.a" >&2
  echo "  先跑: scripts/build-android-freetype.sh" >&2
  exit 1
}
[ -f "$STB/stb_image_write.h" ] || { echo "error: 缺 $STB/stb_image_write.h" >&2; exit 1; }

WORK=$BUILD/text-plugin
PKG=$WORK/text
echo "→ 组装 text2png"
rm -rf "$WORK"
mkdir -p "$PKG/lib"

echo "→ 编译 plugin.so"
"$CC" -shared -fPIC -O2 \
  -I"$FT_PREFIX/include/freetype2" -I"$STB" \
  -o "$PKG/lib/plugin.so" \
  "$REPO/scripts/text-plugin/plugin.c" \
  "$FT_PREFIX/lib/libfreetype.a" -lm

echo "   size $(stat -c%s "$PKG/lib/plugin.so" 2>/dev/null || stat -f%z "$PKG/lib/plugin.so") bytes"

cat > "$PKG/package.json" <<JSON
{
  "name": "text",
  "version": "$VERSION",
  "description": "文字渲染：把一段文字（含中文）渲染成 PNG，补 SVG 不支持 <text> 的缺口",
  "entry": "lib/plugin.so",
  "symbol": "plugin_main",
  "aliases": ["text2png"]
}
JSON

cat > "$PKG/README.md" <<'MD'
# text（text2png）

把文字渲染成 PNG。字体取设备系统字体，不随包携带。

```sh
text2png out.png "Hello 世界" --size 40 --fg 1b2b4b --bg f8f2d8
text2png out.png "一段会自动换行的长文字…" --width 480 --size 28
text2png out.png "透明背景" --bg none
```

选项：`--font PATH`（默认 `/system/fonts/NotoSansCJK-Regular.ttc`）、`--size PX`（默认 32）、
`--fg RRGGBB`、`--bg RRGGBB|none`、`--width PX`（默认 720）、`--padding PX`、`--align left|center|right`。

中文能显示是因为用系统中文字体；若换成没有 CJK 字形的字体，中文会渲染成空白或方框——
这不是 bug，是字体缺字形。
MD

ARCHIVE="$OUT/text-$VERSION-arm64.tar.gz"
rm -f "$ARCHIVE"
tar -C "$PKG" -czf "$ARCHIVE" .
printf '  %-22s %s\n' "text" "$(du -h "$ARCHIVE" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256 /'
