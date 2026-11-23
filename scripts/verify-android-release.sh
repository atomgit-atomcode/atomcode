#!/usr/bin/env bash
# Check a release artifact BEFORE it is uploaded, without a device.
#
#   bash scripts/verify-android-release.sh                       # release apk
#   bash scripts/verify-android-release.sh path/to/app.aab       # store bundle
#
# What it is for: `gradle assembleRelease` succeeds in states that are useless —
# an unsigned apk, an apk built from stale `jniLibs` (the P11 note), a version
# that disagrees with the CLI, or one of the two `.so` files missing from the
# package. Each is reported as one sentence naming what is wrong, because "the
# upload was rejected" is what it looks like later.
#
# Exit 0 only when every check passed. For an AAB the signature is produced at
# upload (Play re-signs with the app signing key), so only its contents and
# version are checked here.
set -euo pipefail
cd "$(dirname "$0")/.."

artifact="${1:-android/app/build/outputs/apk/release/app-release.apk}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}
ok() { echo "  ok  $*"; }

# --- the tools -----------------------------------------------------------------
sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
if [ -z "$sdk" ] && [ -f android/local.properties ]; then
  sdk="$(sed -n 's/^sdk\.dir=//p' android/local.properties | head -1)"
fi
[ -n "$sdk" ] && [ -d "$sdk" ] || fail "Android SDK not found (ANDROID_HOME or android/local.properties sdk.dir)"
build_tools="$(ls -d "$sdk"/build-tools/*/ 2>/dev/null | sort -V | tail -1)"
[ -n "$build_tools" ] || fail "no build-tools under $sdk/build-tools"

find_tool() {
  local name="$1" candidate
  for candidate in "$build_tools/$name" "$build_tools/$name.exe" "$build_tools/$name.bat"; do
    if [ -f "$candidate" ]; then
      printf '%s' "$candidate"
      return 0
    fi
  done
  return 1
}

# --- what the product says it is -----------------------------------------------
product_version="$(awk '
  /^\[workspace\.package\]/ { in_pkg = 1; next }
  /^\[/ { in_pkg = 0 }
  in_pkg && /^version[ \t]*=/ { gsub(/.*=[ \t]*"|"[ \t]*$/, ""); print; exit }
' Cargo.toml)"
[ -n "$product_version" ] || fail "cannot read [workspace.package] version from Cargo.toml"
digits="${product_version%%-*}"
expected_code="$(awk -F. '{ printf "%d", $1 * 10000 + $2 * 100 + $3 }' <<<"$digits")"
[ -n "$expected_code" ] || fail "version \"$product_version\" is not major.minor.patch"

[ -f "$artifact" ] || fail "artifact not found: $artifact (build it: bash scripts/build-android.sh --release)"
size="$(du -h "$artifact" | cut -f1)"
echo "artifact: $artifact ($size)"
echo "workspace version: $product_version (versionCode $expected_code)"

case "$artifact" in
  *.aab)
    listing="$(unzip -l "$artifact")" || fail "cannot read the bundle: $artifact"
    grep -q 'base/lib/arm64-v8a/libatomcode_android.so' <<<"$listing" \
      || fail "the bundle has no base/lib/arm64-v8a/libatomcode_android.so (stale or missing jniLibs)"
    ok "base/lib/arm64-v8a/libatomcode_android.so is in the bundle"
    grep -q 'base/lib/arm64-v8a/libpluginhost.so' <<<"$listing" \
      || fail "the bundle has no base/lib/arm64-v8a/libpluginhost.so (runtime packages cannot start)"
    ok "base/lib/arm64-v8a/libpluginhost.so is in the bundle"
    grep -q 'base/manifest/AndroidManifest.xml' <<<"$listing" \
      || fail "the bundle has no base/manifest/AndroidManifest.xml"
    ok "base/manifest/AndroidManifest.xml is in the bundle"
    echo "note: an AAB is signed when it is uploaded (Play app signing), so the"
    echo "      signature and versionCode are not checked here — check the apk."
    exit 0
    ;;
  *.apk) ;;
  *) fail "expected an .apk or .aab, got: $artifact" ;;
esac

# --- version, package, ABI, debuggable -----------------------------------------
aapt2="$(find_tool aapt2)" || fail "aapt2 not found in $build_tools"
badging="$("$aapt2" dump badging "$artifact" 2>&1)" || fail "aapt2 cannot read $artifact:\n$badging"

package_line="$(grep -m1 "^package:" <<<"$badging")"
[ -n "$package_line" ] || fail "no package line in aapt2 output"
package_name="$(sed -n "s/^package: name='\([^']*\)'.*/\1/p" <<<"$package_line")"
apk_code="$(sed -n "s/.* versionCode='\([0-9]*\)'.*/\1/p" <<<"$package_line")"
apk_name="$(sed -n "s/.* versionName='\([^']*\)'.*/\1/p" <<<"$package_line")"

[ "$package_name" = "com.atomcode.app" ] || fail "package is '$package_name', expected com.atomcode.app"
ok "package com.atomcode.app"
[ "$apk_name" = "$product_version" ] \
  || fail "versionName is '$apk_name' but the workspace is '$product_version' (retype it in neither place — rebuild)"
ok "versionName $apk_name matches the workspace"
[ "$apk_code" = "$expected_code" ] \
  || fail "versionCode is '$apk_code' but $product_version implies $expected_code (a store rejects a non-increasing code)"
ok "versionCode $apk_code matches major*10000+minor*100+patch"

native="$(grep -m1 "^native-code:" <<<"$badging" || true)"
[ "$native" = "native-code: 'arm64-v8a'" ] \
  || fail "native-code is '${native:-none}', expected exactly 'arm64-v8a' (one ABI on purpose; $native would quadruple the apk)"
ok "one ABI, arm64-v8a"

if grep -q "^application-debuggable" <<<"$badging"; then
  fail "the artifact is debuggable — that is a debug build, not a release one"
fi
ok "not debuggable"

# --- the two libraries the app cannot start without ----------------------------
listing="$(unzip -l "$artifact")" || fail "cannot read $artifact as a zip"
grep -q 'lib/arm64-v8a/libatomcode_android.so' <<<"$listing" \
  || fail "no lib/arm64-v8a/libatomcode_android.so in the apk (stale jniLibs: gradle packages whatever is there)"
ok "libatomcode_android.so is in the apk"
grep -q 'lib/arm64-v8a/libpluginhost.so' <<<"$listing" \
  || fail "no lib/arm64-v8a/libpluginhost.so — CPython/Lua/SQLite/quickjs plugins and MCP servers cannot start"
ok "libpluginhost.so is in the apk"

# --- signature -----------------------------------------------------------------
apksigner="$(find_tool apksigner)" || fail "apksigner not found in $build_tools"
certs="$("$apksigner" verify --print-certs "$artifact" 2>&1)" \
  || fail "apksigner refuses the apk (unsigned, or truncated):\n$certs"
grep -qi "Android Debug" <<<"$certs" \
  && fail "the apk is signed with the debug certificate — it can never be updated by the store key"
signer="$(sed -n 's/^Signer #1 certificate DN: //p' <<<"$certs" | head -1)"
digest="$(sed -n 's/^Signer #1 certificate SHA-256 digest: //p' <<<"$certs" | head -1)"
ok "signed: ${signer:-<no DN>}"
[ -n "$digest" ] && echo "       SHA-256 $digest"
echo "       compare that digest with the one the previous release was signed with"

echo "PASS: $artifact is uploadable"
