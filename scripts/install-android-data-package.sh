#!/usr/bin/env bash
# Install a data package onto a connected device, the way the app's own
# `/packages/install` would on the phone.
#
# Why the tar detour: `adb` cannot write into an app's data directory, and the
# app's uid under `run-as` cannot read `/sdcard` (that is the `runas_app` domain).
# `/data/local/tmp` is readable by both, so the tarball goes through there and is
# unpacked as the app's uid.
#
# Usage:
#   bash scripts/install-android-data-package.sh <tarball> <link>
#
#   <link>      the directory under `<product>/` to unpack into. It must match the
#               package's `links` name (`go`, `cmake`, `node`, `clang`, …).
#
# Environment: APP_ID (default com.atomcode.app), ADB (default `adb` on PATH).
set -euo pipefail
cd "$(dirname "$0")/.."

bundle=${1:?usage: install-android-data-package.sh <tarball> <link>}
link=${2:?missing link name (go, cmake, node, clang …)}
app_id=${APP_ID:-com.atomcode.app}

if [ -n "${ADB:-}" ]; then
  adb=$ADB
elif [ -x .android-sdk/platform-tools/adb.exe ]; then
  adb=.android-sdk/platform-tools/adb.exe
elif [ -x .android-sdk/platform-tools/adb ]; then
  adb=.android-sdk/platform-tools/adb
else
  adb=adb
fi

[ -f "$bundle" ] || { echo "error: no such bundle: $bundle" >&2; exit 1; }
"$adb" shell "pm path $app_id >/dev/null 2>&1" ||
  { echo "error: $app_id is not installed on the device" >&2; exit 1; }

echo "→ push $(basename "$bundle")"
MSYS_NO_PATHCONV=1 "$adb" push "$bundle" /data/local/tmp/atomcode-data.tar.gz >/dev/null

echo "→ unpack as $app_id into <product>/$link"
MSYS_NO_PATHCONV=1 "$adb" shell "run-as $app_id sh -c 'cat /data/local/tmp/atomcode-data.tar.gz > files/data.tar.gz && rm -rf files/atomcode/$link && mkdir -p files/atomcode/$link && tar xzf files/data.tar.gz -C files/atomcode/$link && rm -f files/data.tar.gz && ls files/atomcode/$link'"

MSYS_NO_PATHCONV=1 "$adb" shell "rm -f /data/local/tmp/atomcode-data.tar.gz" >/dev/null

echo "→ installed; restart the app so the host picks the paths up"
