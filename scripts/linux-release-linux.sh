#!/bin/bash
# Linux Native Build Script
# 在 Linux 服务器上编译 Linux 原生版本
set -e

# Always run from project root
cd "$(dirname "$0")/.."

VERSION=$(git describe --tags --abbrev=0 2>/dev/null)
if [ -z "$VERSION" ]; then
    echo "No git tag found. Create one first: git tag -a v1.0.0 -m 'v1.0.0'"
    exit 1
fi

# Detect architecture
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64)
        TARGET="x86_64-unknown-linux-gnu"
        SUFFIX="linux-x64"
        ;;
    aarch64)
        TARGET="aarch64-unknown-linux-gnu"
        SUFFIX="linux-arm64"
        ;;
    *)
        echo "Unsupported architecture: ${ARCH}"
        exit 1
esac

DIST="dist/${VERSION}"
mkdir -p "$DIST"

echo "=== AtomCode Linux Release ${VERSION} ==="
echo "Target: ${TARGET}"
echo "Architecture: ${ARCH}"
echo ""

# Build the embedded webui frontend so the binary embeds the latest UI.
if [ -d webui ] && command -v npm >/dev/null 2>&1; then
  echo "Building webui frontend..."
  (cd webui && npm ci && npm run build)
else
  echo "warning: skipping webui build (npm not found or webui/ missing); using committed webui/dist" >&2
fi
echo ""

# Build
echo "[1/2] Building ${TARGET} (native)..."
cargo build --release --target "$TARGET"

# Copy binaries
echo "[2/2] Copying artifacts..."
cp "target/${TARGET}/release/atomcode" "${DIST}/atomcode-${VERSION}-${SUFFIX}"
cp "target/${TARGET}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-${SUFFIX}"
echo "  -> ${DIST}/atomcode-${VERSION}-${SUFFIX}"
echo "  -> ${DIST}/atomcode-daemon-${VERSION}-${SUFFIX}"

# Package
echo ""
echo "=== Packaging ==="
cd "$DIST"
rm -f *${SUFFIX}*.tar.gz 2>/dev/null
for f in atomcode-*${SUFFIX} atomcode-daemon-*${SUFFIX}; do
    [ -f "$f" ] || continue
    chmod +x "$f"
    tar czf "${f}.tar.gz" "$f"
    echo "  -> ${f}.tar.gz"
done

# SHA256
echo ""
echo "=== SHA256 ==="
sha256sum *${SUFFIX}*.tar.gz | tee -a checksums.txt
echo ""
echo "Done. Release artifacts:"
ls -lh *${SUFFIX}*.tar.gz
