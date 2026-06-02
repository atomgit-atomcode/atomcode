#!/usr/bin/env bash
# Windows cross-build script for AtomCode.
#
# Produces Windows .exe release artifacts that can be installed by install.ps1.
#
# Requirements on macOS:
#   1. Rust + rustup
#   2. Windows x64 Rust target:
#        rustup target add x86_64-pc-windows-gnu
#   3. Windows x64 linker:
#        brew install mingw-w64
#
# Optional Windows ARM64 support:
#   1. Rust target:
#        rustup target add aarch64-pc-windows-gnullvm
#   2. Linker providing aarch64-w64-mingw32-gcc:
#        brew install llvm-mingw
#
# If Homebrew downloads are slow in China, use mirrors for the install command:
#   HOMEBREW_NO_AUTO_UPDATE=1 \
#   HOMEBREW_API_DOMAIN=https://mirrors.ustc.edu.cn/homebrew-bottles/api \
#   HOMEBREW_BOTTLE_DOMAIN=https://mirrors.ustc.edu.cn/homebrew-bottles \
#   brew install mingw-w64
#
# Environment:
#   ATOMCODE_VERSION=vX.Y.Z       Override version. Defaults to Cargo.toml.
#   ATOMCODE_WINDOWS_TARGETS=x64  Comma-separated: x64,arm64,all. Defaults to x64.
#   ATOMCODE_INCLUDE_DAEMON=1     Also build atomcode-daemon.exe.
#   ATOMCODE_USE_MIRROR=1         Use rsproxy.cn (ByteDance) mirror for faster
#                                 crates.io downloads in China.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

if [ -x "$HOME/.cargo/bin/rustc" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

# --- China mirror (rsproxy.cn by ByteDance) ---
# When ATOMCODE_USE_MIRROR=1, set rustup + cargo to use the fast domestic mirror.
# This dramatically speeds up crate downloads and rustup target installs.
if [ "${ATOMCODE_USE_MIRROR:-0}" = "1" ]; then
    export RUSTUP_DIST_SERVER="https://rsproxy.cn"
    export RUSTUP_UPDATE_ROOT="https://rsproxy.cn/rustup"

    CARGO_CONFIG="$HOME/.cargo/config.toml"
    MIRROR_MARKER="# atomcode-mirror-rsproxy"
    if [ ! -f "$CARGO_CONFIG" ] || ! grep -q "$MIRROR_MARKER" "$CARGO_CONFIG" 2>/dev/null; then
        echo "[mirror] Configuring rsproxy.cn (ByteDance) for crates.io ..."
        mkdir -p "$(dirname "$CARGO_CONFIG")"
        cat >> "$CARGO_CONFIG" <<'MIRROR_EOF'
# atomcode-mirror-rsproxy
[source.crates-io]
replace-with = 'rsproxy-sparse'

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
MIRROR_EOF
        echo "[mirror] Done. Appended rsproxy-sparse config to $CARGO_CONFIG"
    else
        echo "[mirror] rsproxy already configured in $CARGO_CONFIG, skipping."
    fi
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

INCLUDE_DAEMON="${ATOMCODE_INCLUDE_DAEMON:-0}"
CARGO_PKG_ARGS=(-p atomcode)
if [ "$INCLUDE_DAEMON" = "1" ]; then
    CARGO_PKG_ARGS+=(-p atomcode-daemon)
fi

want_target() {
    local name="$1"
    local requested="${ATOMCODE_WINDOWS_TARGETS:-x64}"
    [ "$requested" = "all" ] && return 0
    case ",${requested}," in
        *",${name},"*) return 0 ;;
        *) return 1 ;;
    esac
}

copy_daemon() {
    [ "$INCLUDE_DAEMON" = "1" ] || return 0
    local target="$1"
    local suffix="$2"
    local src="target/${target}/release/atomcode-daemon.exe"
    local dst="${DIST}/atomcode-daemon-${VERSION}-${suffix}.exe"
    cp "$src" "$dst"
    echo "  -> $dst"
}

build_windows_x64() {
    local target="x86_64-pc-windows-gnu"
    local suffix="windows-x64"

    echo "[x64] Checking linker..."
    if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
        echo "Missing x86_64-w64-mingw32-gcc."
        echo "Install it with: brew install mingw-w64"
        exit 1
    fi

    echo "[x64] Building ${target}..."
    rustup target add "$target" >/dev/null
    cargo build --release --target "$target" "${CARGO_PKG_ARGS[@]}"

    local out="${DIST}/atomcode-${VERSION}-${suffix}.exe"
    cp "target/${target}/release/atomcode.exe" "$out"
    echo "  -> $out"
    copy_daemon "$target" "$suffix"
}

build_windows_arm64() {
    local target="aarch64-pc-windows-gnullvm"
    local suffix="windows-arm64"

    echo "[arm64] Checking linker..."
    if ! command -v aarch64-w64-mingw32-gcc >/dev/null 2>&1; then
        echo "Missing aarch64-w64-mingw32-gcc."
        echo "Install it with: brew install llvm-mingw"
        exit 1
    fi

    echo "[arm64] Building ${target}..."
    rustup target add "$target" >/dev/null
    cargo build --release --target "$target" "${CARGO_PKG_ARGS[@]}"

    local out="${DIST}/atomcode-${VERSION}-${suffix}.exe"
    cp "target/${target}/release/atomcode.exe" "$out"
    echo "  -> $out"
    copy_daemon "$target" "$suffix"
}

echo "=== AtomCode Windows Release ${VERSION} ==="
echo "Artifacts: ${DIST}"
echo "Targets: ${ATOMCODE_WINDOWS_TARGETS:-x64}"
echo ""

# Build the embedded webui frontend so the binary embeds the latest UI.
if [ -d webui ] && command -v npm >/dev/null 2>&1; then
  echo "Building webui frontend..."
  (cd webui && npm ci && npm run build)
else
  echo "warning: skipping webui build (npm not found or webui/ missing); using committed webui/dist" >&2
fi
echo ""

if want_target x64; then
    build_windows_x64
fi

if want_target arm64; then
    build_windows_arm64
fi

echo ""
echo "=== SHA256 ==="
cd "$DIST"
shasum -a 256 atomcode-*windows-*.exe | tee checksums-windows.txt

echo ""
echo "Done. Windows artifacts:"
ls -lh atomcode-*windows-*.exe checksums-windows.txt
