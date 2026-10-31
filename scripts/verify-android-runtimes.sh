#!/usr/bin/env bash
# Verify the clang and node providers on a real device.
#
# This is the step that cannot be faked on the build host: the APK's libraries are
# Android/bionic ELF files, and whether they run depends on the platform's exec
# rules, the loader path, and clang finding its sysroot — none of which a Windows
# build machine can answer.
#
# What it does:
#   1. installs both provider APKs,
#   2. streams each data package into the app's private directory (adb cannot write
#      there directly; `run-as` + `cat` can) and unpacks it,
#   3. links `<product>/bin/{clang,clang++,lld,node}` at the providers' lib dirs —
#      the same links the host creates at startup, made here so no restart is needed,
#   4. compiles, links and runs a real program with clang, and runs node and npm.
#
# Usage: bash scripts/verify-android-runtimes.sh
#   APP_ID=com.atomcode.app        (default)
#   DIST=dist
set -euo pipefail
cd "$(dirname "$0")/.."
APP_ID=${APP_ID:-com.atomcode.app}
DIST=${DIST:-dist}

say() { printf '\n=== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

[ -n "$(adb devices | sed -n '2p')" ] || fail "no device on adb"

apk_clang=$(ls "$DIST"/clang-provider-*-arm64.apk 2>/dev/null | sort -V | tail -1)
apk_node=$(ls "$DIST"/node-provider-*-arm64.apk 2>/dev/null | sort -V | tail -1)
data_clang=$(ls "$DIST"/clang-sysroot-*-arm64.tar.gz 2>/dev/null | sort -V | tail -1)
data_node=$(ls "$DIST"/node-lib-*-arm64.tar.gz 2>/dev/null | sort -V | tail -1)
[ -n "$apk_clang" ] && [ -n "$apk_node" ] || fail "provider APKs missing under $DIST"
[ -n "$data_clang" ] && [ -n "$data_node" ] || fail "data packages missing under $DIST"

say "install providers"
adb install -r "$apk_clang" | tail -1
adb install -r "$apk_node" | tail -1

lib_dir() { # lib_dir <package> → nativeLibraryDir (the one executable place)
  adb shell "dumpsys package $1" 2>/dev/null |
    sed -n 's/.*nativeLibraryDir=\(.*\)/\1/p' | head -1
}
clang_lib=$(lib_dir com.atomcode.tools.clang)
node_lib=$(lib_dir com.atomcode.tools.node)
[ -n "$clang_lib" ] || fail "clang provider not installed"
[ -n "$node_lib" ] || fail "node provider not installed"

install_data() { # install_data <tar> <product-subdir>
  local tar=$1 sub=$2 name
  name=$(basename "$tar")
  say "push $name → files/atomcode/$sub"
  # The app's files directory is private; `run-as` can write it and reads its own
  # stdin, so the tarball is streamed rather than staged on /sdcard.
  adb exec-out "run-as $APP_ID sh -c 'cat > files/$name.tar'" < "$tar"
  adb shell "run-as $APP_ID sh -c 'cd files && rm -rf atomcode/$sub && mkdir -p atomcode/$sub && tar -xf $name.tar -C atomcode/$sub && rm -f $name.tar'"
}

install_data "$data_clang" clang
install_data "$data_node" node

say "link tools into <product>/bin"
adb shell "run-as $APP_ID sh -c '
  mkdir -p files/atomcode/bin
  cd files/atomcode/bin
  ln -sf $clang_lib/libclang.so    clang
  ln -sf $clang_lib/libclang.so    clang++
  ln -sf $clang_lib/liblld.so      lld
  ln -sf $node_lib/libnode.so      node
  ls -l clang clang++ lld node'"

# The environment the host sets at startup (shims.rs), repeated here because this
# script drives the binaries directly rather than through a fresh app start. The
# data root is computed on the device: `run-as` starts in a different cwd.
run_in_product() { # run_in_product <script>
  adb shell "run-as $APP_ID sh -c '
    cd files || exit 1
    export LD_LIBRARY_PATH=$clang_lib:$node_lib
    export ATOMCODE_CLANG_DATA=\$(pwd)/atomcode/clang
    $1'"
}

say "clang --version"
run_in_product './atomcode/bin/clang --version' | head -3

say "compile + link + run (clang, target aarch64-linux-android26)"
run_in_product '
  cat > atomcode/hello.c <<EOF
#include <stdio.h>
int main(void) { printf("hello clang %d\n", 40 + 2); return 0; }
EOF
  ./atomcode/bin/clang --target=aarch64-linux-android26 -O1 atomcode/hello.c -o atomcode/hello || exit 1
  ./atomcode/hello'

say "lld --version"
run_in_product './atomcode/bin/lld --version' | head -2

say "node -v / node -e / npm"
run_in_product '
  ./atomcode/bin/node -v
  ./atomcode/bin/node -e "console.log(6 * 7)"
  ./atomcode/bin/node atomcode/node/lib/node_modules/npm/bin/npm-cli.js --version'

say "all provider checks passed"