#!/usr/bin/env bash
# Build small C plugins for AtomCode on Android: Lua and SQLite.
#
# A package is a shared library that `pluginhost` dlopens — Android forbids
# executing a downloaded binary (see docs/android-port.md §P5.7/§P5.8). Both
# programs here are plain C with an `int main(...)`, so the build renames that
# entry point (`-Dmain=<symbol>`) and `scripts/plugins/plugin_main.c` forwards
# `plugin_main` to it. No fork, no exec, no second process.
#
#   LUA_TGZ=./lua.tgz SQLITE_ZIP=./sqlite.zip \
#   ANDROID_NDK_LINUX_HOME=~/android-git-build/ndk/android-ndk-r26b \
#     bash scripts/build-android-plugins.sh
#
# Sources (not vendored):
#   lua.tgz     https://www.lua.org/ftp/lua-5.4.7.tar.gz
#   sqlite.zip  https://www.sqlite.org/2025/sqlite-amalgamation-3500400.zip
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
NDK="${ANDROID_NDK_LINUX_HOME:-$HOME/android-git-build/ndk/android-ndk-r26b}"
API="${ATOMCODE_ANDROID_API:-26}"
TARGET=aarch64-linux-android
BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
CC="$BIN/${TARGET}${API}-clang"
SRC="${PLUGIN_SRC:-/tmp/atomcode-plugin-src}"
OUT="${OUT:-$REPO/dist}"
SHIM="$REPO/scripts/plugins/plugin_main.c"
LUA_SHIM="${LUA_SHIM:-$REPO/scripts/lua-plugin/plugin.c}"
# Prebuilt C modules to ship inside the package (`lib/lua/*.so`). The lua shim
# puts that directory on LUA_CPATH, so a bundle can carry the modules it needs
# without writing anywhere outside itself.
LUA_MODULES="${LUA_MODULES:-$OUT/lua-modules}"

LUA_VERSION="${LUA_VERSION:-5.4.7}"
LUA_TGZ="${LUA_TGZ:-$REPO/lua.tgz}"
SQLITE_VERSION="${SQLITE_VERSION:-3.50.4}"
SQLITE_ZIP="${SQLITE_ZIP:-$REPO/sqlite.zip}"
SQLITE_DIRNAME="${SQLITE_DIRNAME:-sqlite-amalgamation-3500400}"

[ -x "$CC" ] || { echo "error: missing $CC" >&2; exit 1; }
[ -f "$SHIM" ] || { echo "error: missing $SHIM" >&2; exit 1; }
mkdir -p "$SRC" "$OUT"

package() { # <dir> <name> <version> <description> [extra-json]
  local dir="$1" name="$2" version="$3" description="$4" extra="${5:-}"
  {
    printf '{
'
    printf '  "name": "%s",
' "$name"
    printf '  "version": "%s",
' "$version"
    printf '  "description": "%s",
' "$description"
    printf '  "entry": "lib/plugin.so",
'
    printf '  "symbol": "plugin_main"'
    if [ -n "$extra" ]; then printf ',
%s' "$extra"; fi
    printf '
}
'
  } > "$dir/package.json"
  local archive="$OUT/$name-$version-arm64.tar.gz"
  rm -f "$archive"
  tar -C "$dir" -czf "$archive" .
  printf '  %-22s %s  (unpacked %s)\n' \
    "$name" "$(du -h "$archive" | cut -f1)" "$(du -sh "$dir" | cut -f1)"
  sha256sum "$archive" | sed 's/^/    sha256 /'
}

# --- Lua -------------------------------------------------------------------
# The standalone interpreter is `lua.c`; every other `src/*.c` is a library or
# runtime file. `luac.c` is the *compiler's* main and must stay out.
build_lua() {
  echo "→ building Lua $LUA_VERSION"
  local work="$SRC/lua"
  rm -rf "$work" && mkdir -p "$work"
  tar -C "$work" -xzf "$LUA_TGZ"
  local dir="$work/lua-$LUA_VERSION"
  local out="$OUT/lua-$LUA_VERSION-arm64"
  rm -rf "$out" && mkdir -p "$out/lib"

  local sources=()
  for file in "$dir"/src/*.c; do
    case "$(basename "$file")" in
      luac.c) continue ;;
      lua.c) continue ;;
      *) sources+=("$file") ;;
    esac
  done

  # `lua.c` carries the entry point; rename it so the shim can call it.
  "$CC" -c -fPIC -O2 -DLUA_USE_DLOPEN \
    -Dmain=lua_standalone_main -o "$work/lua_main.o" "$dir/src/lua.c"
  # Lua gets its own shim: upstream's LUA_ROOT is /usr/local, which does not
  # exist on Android, so `package.cpath` had nowhere to look for a C module.
  "$CC" -c -fPIC -O2 -DLUA_USE_DLOPEN -o "$work/lua_shim.o" "$LUA_SHIM" \
    -DPLUGIN_ENTRY=lua_standalone_main
  # `LUA_USE_DLOPEN` must reach *every* translation unit: the sources below are
  # compiled right here on the link line, and `loadlib.c` is the one that decides
  # whether `require` may dlopen a C module. Missing it here produced
  # "dynamic libraries not enabled; check your Lua installation".
  "$CC" -shared -fPIC -O2 -DLUA_USE_DLOPEN -o "$out/lib/plugin.so" \
    "$work/lua_main.o" "$work/lua_shim.o" "${sources[@]}" -lm -ldl

  if [ -d "$LUA_MODULES" ] && ls "$LUA_MODULES"/*.so >/dev/null 2>&1; then
    mkdir -p "$out/lib/lua"
    cp "$LUA_MODULES"/*.so "$out/lib/lua/"
    echo "  + lua modules: $(cd "$LUA_MODULES" && ls *.so | tr '\n' ' ')"
  fi

  # `preload` is what makes `require("somelib")` work at all: on Android bionic
  # does not promote a dlopen'd library's symbols to the global namespace, so a
  # C module loading afterwards cannot resolve `luaL_checkversion_` (measured:
  # `cannot locate symbol "luaL_checkversion_"`). Naming the plugin itself as a
  # preload makes `pluginhost` re-exec with LD_PRELOAD, the one mechanism bionic
  # honours — the same fix the CPython runtime uses.
  package "$out" lua "$LUA_VERSION" "Lua $LUA_VERSION interpreter (embedded, no fork)"     '  "preload": ["lib/plugin.so"]'
}

# --- SQLite ----------------------------------------------------------------
# Two translation units: the engine (`sqlite3.c`) and the CLI (`shell.c`).
build_sqlite() {
  echo "→ building SQLite $SQLITE_VERSION"
  local work="$SRC/sqlite"
  rm -rf "$work" && mkdir -p "$work"
  # `unzip` is not on every build host; Python always is (the CPython plugin
  # build needs it too), so use its zipfile module instead of assuming.
  ( cd "$work" && python3 -m zipfile -e "$SQLITE_ZIP" . )
  local dir="$work/$SQLITE_DIRNAME"
  local out="$OUT/sqlite3-$SQLITE_VERSION-arm64"
  rm -rf "$out" && mkdir -p "$out/lib"

  # Full-text search, spatial indexing and the math functions are built in.
  # They cost a few hundred KB and turn "no such module: fts5" into a working
  # feature. `SQLITE_OMIT_LOAD_EXTENSION` is deliberately NOT set so `.load` can
  # still reach an extension later; keep this list in step with the
  # `libsqlite3.a` build, because the Python module and this shell must agree.
  local FEATURES="-DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS3 -DSQLITE_ENABLE_FTS4 -DSQLITE_ENABLE_FTS5 -DSQLITE_ENABLE_RTREE -DSQLITE_ENABLE_MATH_FUNCTIONS -DSQLITE_ENABLE_COLUMN_METADATA"
  "$CC" -c -fPIC -O2 $FEATURES \
    -o "$work/sqlite3.o" "$dir/sqlite3.c"
  # The shell's own main becomes the plugin entry, via the shim.
  "$CC" -c -fPIC -O2 $FEATURES \
    -Dmain=sqlite3_shell_main -o "$work/shell.o" "$dir/shell.c"
  "$CC" -c -fPIC -O2 -o "$work/sqlite_shim.o" "$SHIM" \
    -DPLUGIN_ENTRY=sqlite3_shell_main
  "$CC" -shared -fPIC -O2 -o "$out/lib/plugin.so" \
    "$work/sqlite3.o" "$work/shell.o" "$work/sqlite_shim.o" -lm

  package "$out" sqlite3 "$SQLITE_VERSION" "SQLite $SQLITE_VERSION shell (embedded, no fork)"
}

# 只重建指定的包：`scripts/build-android-plugins.sh lua`。默认两个都建。
# 之所以要能挑：给 lua 加一个 C 模块时不该顺手把 sqlite3 也重打一遍（会换掉它的 sha）。
TARGETS=${1:-"lua sqlite3"}
for target in $TARGETS; do
  case "$target" in
    lua) build_lua ;;
    sqlite3) build_sqlite ;;
    *) echo "error: 未知目标 $target（可选：lua、sqlite3）" >&2; exit 2 ;;
  esac
done
echo "→ bundles in $OUT"
