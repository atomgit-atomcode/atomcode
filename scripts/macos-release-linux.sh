#!/usr/bin/env bash
# Linux cross-build script for AtomCode daemon.
#
# Produces Linux release artifacts for atomcode-daemon, cross-compiled from macOS.
#
# Requirements on macOS:
#   1. Rust + rustup
#   2. Linux x64 musl target + linker:
#        rustup target add x86_64-unknown-linux-musl
#        brew install FiloSottile/musl-cross/musl-cross
#
# Optional Linux ARM64 support:
#   1. Rust target:
#        rustup target add aarch64-unknown-linux-musl
#   2. aarch64 musl-cross linker is included by default in the above brew formula.
#
# If Homebrew downloads are slow in China, use mirrors for the install command:
#   HOMEBREW_NO_AUTO_UPDATE=1 \
#   HOMEBREW_API_DOMAIN=https://mirrors.ustc.edu.cn/homebrew-bottles/api \
#   HOMEBREW_BOTTLE_DOMAIN=https://mirrors.ustc.edu.cn/homebrew-bottles \
#   brew install FiloSottile/musl-cross/musl-cross
#
# Environment:
#   ATOMCODE_VERSION=vX.Y.Z       Override version. Defaults to Cargo.toml.
#   ATOMCODE_LINUX_TARGETS=x64    Comma-separated: x64,arm64,all. Defaults to x64.
#   ATOMCODE_INCLUDE_CLI=1        Also build atomcode CLI binary.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

if [ -x "$HOME/.cargo/bin/rustc" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

VERSION="${ATOMCODE_VERSION:-}"
if [ -z "$VERSION" ]; then
    CARGO_VERSION=$(awk -F'"' '
        /^\[workspace\.package\]/ { in_section = 1; next }
        /^\[/ { in_section = 0 }
        in_section && /^version *=/ { print $2; exit }
    ' Cargo.toml)
    if [ -n "$CARGO_VERSION" ]; then
        VERSION="v${CARGO_VERSION}"
    fi
fi

if [ -z "$VERSION" ]; then
    echo "Could not determine version. Set ATOMCODE_VERSION=v1.2.3."
    exit 1
fi

case "$VERSION" in
    v[0-9]*) ;;
    *)
        echo "Refusing to release with non-vX.Y.Z version: '$VERSION'"
        echo "Set ATOMCODE_VERSION=v1.2.3 if you really mean to."
        exit 1
        ;;
esac

DIST="${ROOT}/dist/${VERSION}"
mkdir -p "$DIST"

# Default: build daemon only. Set ATOMCODE_INCLUDE_CLI=1 to also build CLI.
INCLUDE_CLI="${ATOMCODE_INCLUDE_CLI:-0}"
CARGO_PKG_ARGS=(-p atomcode-daemon)
if [ "$INCLUDE_CLI" = "1" ]; then
    CARGO_PKG_ARGS+=(-p atomcode)
fi

want_target() {
    local name="$1"
    local requested="${ATOMCODE_LINUX_TARGETS:-x64}"
    [ "$requested" = "all" ] && return 0
    case ",${requested}," in
        *",${name},"*) return 0 ;;
        *) return 1 ;;
    esac
}

copy_cli() {
    [ "$INCLUDE_CLI" = "1" ] || return 0
    local target="$1"
    local suffix="$2"
    local src="target/${target}/release/atomcode"
    local dst="${DIST}/atomcode-${VERSION}-${suffix}"
    cp "$src" "$dst"
    echo "  -> $dst"
}

build_linux_x64() {
    local target="x86_64-unknown-linux-musl"
    local suffix="linux-x64"

    echo "[x64] Checking linker..."
    if ! command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
        echo "Missing x86_64-linux-musl-gcc."
        echo "Install it with: brew install FiloSottile/musl-cross/musl-cross"
        exit 1
    fi

    echo "[x64] Building ${target}..."
    rustup target add "$target" >/dev/null
    export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
    export CFLAGS_x86_64_unknown_linux_musl="-fPIC"
    cargo build --release --target "$target" "${CARGO_PKG_ARGS[@]}"

    local out="${DIST}/atomcode-daemon-${VERSION}-${suffix}"
    cp "target/${target}/release/atomcode-daemon" "$out"
    echo "  -> $out"
    copy_cli "$target" "$suffix"
}

build_linux_arm64() {
    local target="aarch64-unknown-linux-musl"
    local suffix="linux-arm64"

    echo "[arm64] Checking linker..."
    if ! command -v aarch64-linux-musl-gcc >/dev/null 2>&1; then
        echo "Missing aarch64-linux-musl-gcc."
        echo "Install it with: brew install FiloSottile/musl-cross/musl-cross"
        echo "Note: aarch64 linker ships by default; do NOT pass --with-aarch64."
        exit 1
    fi

    echo "[arm64] Building ${target}..."
    rustup target add "$target" >/dev/null
    export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
    export CFLAGS_aarch64_unknown_linux_musl="-fPIC"
    cargo build --release --target "$target" "${CARGO_PKG_ARGS[@]}"

    local out="${DIST}/atomcode-daemon-${VERSION}-${suffix}"
    cp "target/${target}/release/atomcode-daemon" "$out"
    echo "  -> $out"
    copy_cli "$target" "$suffix"
}

echo "=== AtomCode Linux Release ${VERSION} (cross-compile from macOS) ==="
echo "Artifacts: ${DIST}"
echo "Targets: ${ATOMCODE_LINUX_TARGETS:-x64}"
echo ""

if want_target x64; then
    build_linux_x64
fi

if want_target arm64; then
    build_linux_arm64
fi

echo ""
echo "=== SHA256 ==="
cd "$DIST"
shasum -a 256 atomcode-*linux-* 2>/dev/null | tee checksums-linux.txt

echo ""
echo "Done. Linux artifacts:"
ls -lh atomcode-*linux-* checksums-linux.txt 2>/dev/null
