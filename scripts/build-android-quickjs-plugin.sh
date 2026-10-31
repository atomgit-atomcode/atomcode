#!/usr/bin/env bash
# 构建 quickjs 插件：把 qjs CLI 编成一个可由 pluginhost 加载的 .so。
#
# 为什么用插件而不是塞进 APK：QuickJS 是纯计算型解释器，没有 exec 需求，放进 APK 会让
# 每个用户都背上它；做成包则是「用的人装、不用的人没有」。同理也适用于以后的语言运行时。
#
# 输入：quickjs 源码 tarball（默认 ./quickjs.tar.xz，或 $1）
# 输出：dist/quickjs-<version>-arm64.tar.gz
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BUILD=${BUILD:-$HOME/android-git-build}
NDK=${NDK:-$BUILD/ndk/android-ndk-r26b}
API=${API:-26}
OUT=$REPO/dist

TARBALL=${1:-$REPO/quickjs.tar.xz}
[ -f "$TARBALL" ] || { echo "error: missing $TARBALL" >&2; exit 1; }

CC=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android${API}-clang
WORK=$BUILD/quickjs-plugin
echo "→ 解包 $TARBALL"
rm -rf "$WORK"
mkdir -p "$WORK/src"
tar -C "$WORK/src" -xf "$TARBALL"
SRC=$(ls -d "$WORK"/src/quickjs-*/ | head -1)
[ -n "$SRC" ] || { echo "error: tarball has no quickjs-* directory" >&2; exit 1; }
VERSION=$(cat "$SRC/VERSION" 2>/dev/null || basename "$SRC" | sed 's/quickjs-//;s#/##')

PKG=$WORK/quickjs
mkdir -p "$PKG/lib"
echo "→ 编译 qjs（version $VERSION）"

# qjs.c 引用预编译的 REPL 字节码（`qjsc_repl` / `qjsc_repl_size`），而 tarball 只带
# `repl.js`。这段字节码是构建期产物：先用宿主 cc 编一个 qjsc，再用它把 repl.js 生成 C。
# 不生成的话链接仍然“成功”，但运行时 dlopen 会报
# `cannot locate symbol "qjsc_repl_size"` —— 装得上、跑不起来，最难查的那种。
echo "→ 用宿主工具生成 REPL 字节码"
HOSTCC=${HOSTCC:-cc}
"$HOSTCC" -O1 -D_GNU_SOURCE -DCONFIG_VERSION="\"$VERSION\"" \
  -o "$WORK/qjsc" \
  "$SRC/qjsc.c" "$SRC/quickjs.c" "$SRC/libregexp.c" "$SRC/libunicode.c" \
  "$SRC/cutils.c" "$SRC/dtoa.c" "$SRC/quickjs-libc.c" -lm -ldl
"$WORK/qjsc" -c -o "$WORK/qjsc_repl.c" "$SRC/repl.js"
grep -q "qjsc_repl_size" "$WORK/qjsc_repl.c" || {
  echo "error: qjsc 没有产出 REPL 符号" >&2
  exit 1
}

# -Dmain=quickjs_main：把 CLI 的入口改名，plugin_main 只做转发。
# dtoa.c 是 QuickJS 新版的数值格式化实现；qjsc/run-test262 是构建期工具，不编。
"$CC" -shared -fPIC -O2 \
  -D_GNU_SOURCE -DCONFIG_VERSION="\"$VERSION\"" -Dmain=quickjs_main \
  -o "$PKG/lib/plugin.so" \
  "$SRC/quickjs.c" "$SRC/libregexp.c" "$SRC/libunicode.c" "$SRC/cutils.c" \
  "$SRC/dtoa.c" "$SRC/quickjs-libc.c" "$SRC/qjs.c" "$WORK/qjsc_repl.c" \
  "$REPO/scripts/quickjs-plugin/plugin.c" \
  -lm -ldl

echo "   size $(stat -c%s "$PKG/lib/plugin.so" 2>/dev/null || stat -f%z "$PKG/lib/plugin.so") bytes"

cat > "$PKG/package.json" <<JSON
{
  "name": "quickjs",
  "version": "$VERSION",
  "description": "QuickJS $VERSION —— 小巧的 ES2023 引擎（qjs CLI），适合脚本与沙箱里的小工具",
  "entry": "lib/plugin.so",
  "symbol": "plugin_main",
  "aliases": ["qjs"]
}
JSON

cat > "$PKG/README.md" <<'MD'
# quickjs（qjs）

小巧的 ES2023 引擎。相比 node：启动快、内存小（无 V8/JIT），适合一次性脚本；
不支持 npm 生态与原生扩展。

```sh
qjs -e 'console.log(1 + 2)'
qjs script.js arg1 arg2
```

不带参数时会进入非交互路径（REPL 依赖 readline，Android 上没有）。
需要完整 Node 语义（`child_process`、npm 包）时用 `node` 包，不要用这个。
MD

ARCHIVE="$OUT/quickjs-$VERSION-arm64.tar.gz"
rm -f "$ARCHIVE"
tar -C "$PKG" -czf "$ARCHIVE" .
printf '  %-22s %s\n' "quickjs" "$(du -h "$ARCHIVE" | cut -f1)"
sha256sum "$ARCHIVE" | sed 's/^/  sha256 /'
