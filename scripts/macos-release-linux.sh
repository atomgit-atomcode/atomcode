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

# ============== Interactive Menu ==============

echo "=== AtomCode Linux Release ${VERSION} (cross-compile from macOS) ==="
echo ""

# Build the embedded webui frontend so the binary embeds the latest UI.
if [ -d "$ROOT/webui" ] && command -v npm >/dev/null 2>&1; then
  echo "Building webui frontend..."
  (cd "$ROOT/webui" && npm ci && npm run build)
else
  echo "warning: skipping webui build (npm not found or webui/ missing); using committed webui/dist" >&2
fi
echo ""

# Step 1: Select target architecture
echo "请选择目标架构："
echo "  1) Linux x64 (x86_64-unknown-linux-musl)"
echo "  2) Linux ARM64 (aarch64-unknown-linux-musl)"
echo "  3) 全部 (x64 + ARM64)"
echo ""
read -rp "请输入 [1/2/3] (默认: 1): " arch_choice
arch_choice="${arch_choice:-1}"

case "$arch_choice" in
    1) BUILD_X64=1; BUILD_ARM64=0 ;;
    2) BUILD_X64=0; BUILD_ARM64=1 ;;
    3) BUILD_X64=1; BUILD_ARM64=1 ;;
    *)
        echo "无效选择: ${arch_choice}，默认构建 x64"
        BUILD_X64=1; BUILD_ARM64=0
        ;;
esac

# Step 2: Select build scope
echo ""
echo "请选择构建范围："
echo "  1) 仅 atomcode-daemon"
echo "  2) atomcode-daemon + atomcode CLI"
echo ""
read -rp "请输入 [1/2] (默认: 1): " scope_choice
scope_choice="${scope_choice:-1}"

case "$scope_choice" in
    1) INCLUDE_CLI=0 ;;
    2) INCLUDE_CLI=1 ;;
    *)
        echo "无效选择: ${scope_choice}，默认仅构建 daemon"
        INCLUDE_CLI=0
        ;;
esac

CARGO_PKG_ARGS=(-p atomcode-daemon)
if [ "$INCLUDE_CLI" = "1" ]; then
    CARGO_PKG_ARGS+=(-p atomcode)
fi

# Confirm
echo ""
echo "--- 构建配置 ---"
echo "版本:     ${VERSION}"
echo "架构:     $([ "$BUILD_X64" = "1" ] && echo -n "x64 "; [ "$BUILD_ARM64" = "1" ] && echo -n "arm64")"
echo "产物:     atomcode-daemon$([ "$INCLUDE_CLI" = "1" ] && echo " + atomcode CLI")"
echo "输出目录: ${DIST}"
echo ""
read -rp "确认开始构建? [Y/n] " confirm
confirm="${confirm:-Y}"
if [[ ! "$confirm" =~ ^[Yy] ]]; then
    echo "已取消。"
    exit 0
fi

# ============== Build Functions ==============

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

    echo ""
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

    echo ""
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

# ============== Execute Build ==============

echo ""
echo "=== 开始构建 ==="

if [ "$BUILD_X64" = "1" ]; then
    build_linux_x64
fi

if [ "$BUILD_ARM64" = "1" ]; then
    build_linux_arm64
fi

echo ""
echo "=== SHA256 ==="
cd "$DIST"
shasum -a 256 atomcode-*linux-* 2>/dev/null | tee checksums-linux.txt

echo ""
echo "Done. Linux artifacts:"
ls -lh atomcode-*linux-* checksums-linux.txt 2>/dev/null
