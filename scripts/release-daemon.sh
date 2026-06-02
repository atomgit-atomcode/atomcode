#!/bin/bash
set -euo pipefail

# Build atomcode-daemon artifacts used by the VS Code extension package.
# Unlike scripts/release.sh, this script is daemon-only and fails fast when a
# required cross compiler is missing, so missing VSIX binaries are obvious.

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
    echo "Could not determine version. Set [workspace.package].version in Cargo.toml,"
    echo "or override with ATOMCODE_VERSION=v1.2.3."
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

DIST="dist/${VERSION}"
mkdir -p "$DIST"

require_cmd() {
    local cmd="$1"
    local hint="$2"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd"
        echo "Install hint: $hint"
        exit 1
    fi
}

build_daemon() {
    local target="$1"
    local suffix="$2"
    local exe="${3:-atomcode-daemon}"
    local env_prefix="${4:-}"

    echo "Building ${target}..."
    rustup target add "$target" >/dev/null 2>&1 || true
    if [ -n "$env_prefix" ]; then
        eval "$env_prefix cargo build --release --target \"$target\" -p atomcode-daemon"
    else
        cargo build --release --target "$target" -p atomcode-daemon
    fi

    local src="target/${target}/release/${exe}"
    local dst="${DIST}/atomcode-daemon-${VERSION}-${suffix}"
    if [[ "$exe" == *.exe ]]; then
        dst="${dst}.exe"
    fi

    cp "$src" "$dst"
    if [[ "$dst" != *.exe ]]; then
        chmod +x "$dst"
    fi
    echo "  -> ${dst}"
}

echo "=== AtomCode Daemon Release ${VERSION} ==="
echo ""

# Build the embedded webui frontend so the binary embeds the latest UI.
if [ -d webui ] && command -v npm >/dev/null 2>&1; then
  echo "Building webui frontend..."
  (cd webui && npm ci && npm run build)
else
  echo "warning: skipping webui build (npm not found or webui/ missing); using committed webui/dist" >&2
fi
echo ""

build_daemon "aarch64-apple-darwin" "darwin-arm64"
build_daemon "x86_64-apple-darwin" "darwin-x64"

require_cmd "x86_64-linux-musl-gcc" "brew install FiloSottile/musl-cross/musl-cross"
build_daemon \
    "x86_64-unknown-linux-musl" \
    "linux-x64" \
    "atomcode-daemon" \
    "CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc CFLAGS_x86_64_unknown_linux_musl=-fPIC"

require_cmd "aarch64-linux-musl-gcc" "brew install FiloSottile/musl-cross/musl-cross"
build_daemon \
    "aarch64-unknown-linux-musl" \
    "linux-arm64" \
    "atomcode-daemon" \
    "CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc CFLAGS_aarch64_unknown_linux_musl=-fPIC"

require_cmd "x86_64-w64-mingw32-gcc" "brew install mingw-w64"
build_daemon "x86_64-pc-windows-gnu" "windows-x64" "atomcode-daemon.exe"

echo ""
echo "=== SHA256 ==="
(
    cd "$DIST"
    shasum -a 256 atomcode-daemon-* | tee daemon-checksums.txt
)

echo ""
echo "=== VS Code packaging env ==="
cat <<EOF
ATOMCODE_DAEMON_DARWIN_ARM64="${ROOT}/${DIST}/atomcode-daemon-${VERSION}-darwin-arm64" \\
ATOMCODE_DAEMON_DARWIN_X64="${ROOT}/${DIST}/atomcode-daemon-${VERSION}-darwin-x64" \\
ATOMCODE_DAEMON_LINUX_X64="${ROOT}/${DIST}/atomcode-daemon-${VERSION}-linux-x64" \\
ATOMCODE_DAEMON_LINUX_ARM64="${ROOT}/${DIST}/atomcode-daemon-${VERSION}-linux-arm64" \\
ATOMCODE_DAEMON_WIN32_X64="${ROOT}/${DIST}/atomcode-daemon-${VERSION}-windows-x64.exe" \\
npm --prefix "${ROOT}/extensions/vscode" run package
EOF

echo ""
echo "Done. Daemon artifacts in ${DIST}/"
ls -lh "${DIST}"/atomcode-daemon-*
