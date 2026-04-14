#!/bin/bash
# Codesign + notarize macOS `atomcode` binaries for distribution.
#
# Only signs the `atomcode` CLI. `atomcode-daemon` is NOT signed (per project
# decision — daemon runs inside CI/user-controlled environments, user-facing
# Gatekeeper only inspects the `atomcode` launcher).
#
# Notarization: requires a keychain profile pre-created with
#   xcrun notarytool store-credentials atomcode-notary \
#     --apple-id "you@example.com" --team-id T949H383MF \
#     --password <app-specific-password>
#
# Override via env:
#   ATOMCODE_SIGN_IDENTITY     (default: the project's team Developer ID)
#   ATOMCODE_NOTARY_PROFILE    (default: atomcode-notary)
#   ATOMCODE_SKIP_NOTARIZE=1   (codesign only, skip notarization)
set -euo pipefail

IDENTITY="${ATOMCODE_SIGN_IDENTITY:-Developer ID Application: Chongqing Kaiyuan Gongchuang Technology Co., Ltd. (T949H383MF)}"
NOTARY_PROFILE="${ATOMCODE_NOTARY_PROFILE:-atomcode-notary}"
SKIP_NOTARIZE="${ATOMCODE_SKIP_NOTARIZE:-0}"

usage() {
    cat <<EOF
Usage: $0 <binary-path | dist-directory>

Examples:
    $0 target/release/atomcode
    $0 dist/v2.5.0

Env:
    ATOMCODE_SIGN_IDENTITY   override signing identity
    ATOMCODE_NOTARY_PROFILE  override notarytool keychain profile (default: atomcode-notary)
    ATOMCODE_SKIP_NOTARIZE=1 codesign only, skip notarization
EOF
    exit 1
}

[ $# -eq 1 ] || usage

TARGET="$1"

# --- Preflight: identity must exist in keychain ---
if ! security find-identity -v -p codesigning | grep -q "$IDENTITY"; then
    echo "Error: signing identity not found in keychain:"
    echo "  $IDENTITY"
    echo ""
    echo "Available identities:"
    security find-identity -v -p codesigning
    exit 1
fi

# --- Preflight: notarytool profile exists (if we'll notarize) ---
if [ "$SKIP_NOTARIZE" != "1" ]; then
    if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
        cat <<EOF
Error: notarytool keychain profile "$NOTARY_PROFILE" not found or invalid.

Create it once with:
    xcrun notarytool store-credentials $NOTARY_PROFILE \\
      --apple-id "YOUR_APPLE_ID" \\
      --team-id T949H383MF \\
      --password "APP_SPECIFIC_PASSWORD"

Or skip notarization: ATOMCODE_SKIP_NOTARIZE=1 $0 $1
EOF
        exit 1
    fi
fi

sign_one() {
    local bin="$1"
    echo "[sign] $(basename "$bin")"
    # --force: replace Rust's adhoc linker-signed signature
    # --timestamp: RFC 3161 timestamp (required for notarization)
    # --options=runtime: hardened runtime (required for notarization)
    codesign \
        --force \
        --timestamp \
        --options=runtime \
        --sign "$IDENTITY" \
        "$bin"
    codesign --verify --strict "$bin"
}

notarize_one() {
    local bin="$1"
    local tmpzip
    tmpzip=$(mktemp -d)/"$(basename "$bin").zip"
    # ditto produces a zip that notarytool accepts for bare binaries.
    /usr/bin/ditto -c -k --keepParent "$bin" "$tmpzip"

    echo "[notarize] submitting $(basename "$bin")..."
    # --wait blocks until Apple finishes processing (usually < 2 min for CLI bin).
    xcrun notarytool submit "$tmpzip" \
        --keychain-profile "$NOTARY_PROFILE" \
        --wait

    rm -f "$tmpzip"
    # Note: tar.gz / bare Mach-O cannot carry a stapled ticket. Gatekeeper
    # queries Apple online on first launch. For offline-valid tickets you'd
    # need a .dmg or .pkg container + `xcrun stapler staple`.
}

is_macho_atomcode_bin() {
    local path="$1"
    local base
    base=$(basename "$path")
    # Must be a regular file named `atomcode` or `atomcode-<version>-darwin-<arch>`,
    # and must NOT be the daemon.
    case "$base" in
        atomcode-daemon*) return 1 ;;
        atomcode|atomcode-*-darwin-*) ;;
        *) return 1 ;;
    esac
    [ -f "$path" ] || return 1
    # Skip packaged artifacts (the tar.gz would match the glob otherwise).
    case "$base" in
        *.tar.gz|*.zip|*.sig|*.txt) return 1 ;;
    esac
    file "$path" 2>/dev/null | grep -q "Mach-O"
}

process_one() {
    local bin="$1"
    sign_one "$bin"
    if [ "$SKIP_NOTARIZE" != "1" ]; then
        notarize_one "$bin"
    fi
}

if [ -f "$TARGET" ]; then
    if is_macho_atomcode_bin "$TARGET"; then
        process_one "$TARGET"
    else
        echo "Error: $TARGET is not a macOS atomcode binary."
        exit 1
    fi
elif [ -d "$TARGET" ]; then
    found=0
    for bin in "$TARGET"/atomcode*; do
        [ -e "$bin" ] || continue
        if is_macho_atomcode_bin "$bin"; then
            process_one "$bin"
            found=$((found + 1))
        fi
    done
    if [ $found -eq 0 ]; then
        echo "No signable atomcode binaries found in $TARGET"
        exit 1
    fi
    echo ""
    echo "Signed $found binary(ies)."
else
    echo "Error: $TARGET not found"
    exit 1
fi
