#!/usr/bin/env bash
# 交叉编译 FreeType（静态库），给 text2png 插件用。
#
# 关掉 zlib/png/bzip2/harfbuzz/brotli：这些只影响「字体文件里嵌了压缩或彩色位图」的情形，
# 系统里的 Noto CJK 是普通轮廓字体，用不上；关掉后产物只有 1 MB，也不需要额外依赖。
#
# 输出：$FT_PREFIX/lib/libfreetype.a + include/freetype2
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BUILD=${BUILD:-$HOME/android-git-build}
NDK=${NDK:-$BUILD/ndk/android-ndk-r26b}
API=${API:-26}
FT_PREFIX=${FT_PREFIX:-$HOME/freetype-prefix}
TARBALL=${1:-$REPO/freetype-2.13.3.tar.gz}

[ -f "$TARBALL" ] || { echo "error: missing $TARBALL" >&2; exit 1; }

CC=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android${API}-clang
AR=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar
RANLIB=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ranlib

WORK=$BUILD/freetype
echo "→ 交叉编译 FreeType → $FT_PREFIX"
rm -rf "$WORK"
mkdir -p "$WORK"
tar -C "$WORK" -xf "$TARBALL"
SRC=$(ls -d "$WORK"/freetype-*/ | head -1)
[ -n "$SRC" ] || { echo "error: tarball has no freetype-* directory" >&2; exit 1; }

cd "$SRC"
./configure --host=aarch64-linux-android --prefix="$FT_PREFIX" \
  --disable-shared --enable-static \
  --without-harfbuzz --without-png --without-zlib --without-bzip2 --with-brotli=no \
  CC="$CC" AR="$AR" RANLIB="$RANLIB" CFLAGS="-fPIC -O2" > "$WORK/configure.log" 2>&1 || {
    echo "error: configure 失败，见 $WORK/configure.log" >&2
    tail -5 "$WORK/configure.log" >&2
    exit 1
  }
make -j4 > "$WORK/make.log" 2>&1 || {
  echo "error: make 失败，见 $WORK/make.log" >&2
  tail -5 "$WORK/make.log" >&2
  exit 1
}
make install > /dev/null 2>&1

LIB=$FT_PREFIX/lib/libfreetype.a
[ -f "$LIB" ] || { echo "error: 没有生成 $LIB" >&2; exit 1; }
printf '  libfreetype.a %.1f MB\n' "$(echo "scale=1; $(stat -c%s "$LIB") / 1048576" | bc)"
