#!/bin/bash
set -e

# Always run from project root
cd "$(dirname "$0")/.."

VERSION=$(git describe --tags --abbrev=0 2>/dev/null)
if [ -z "$VERSION" ]; then
    echo "No git tag found. Create one first: git tag -a v1.0.0 -m 'v1.0.0'"
    exit 1
fi

DIST="dist/${VERSION}"
mkdir -p "$DIST"

echo "=== AtomCode Release ${VERSION} ==="
echo ""

# --- macOS ARM (Apple Silicon) ---
TARGET_ARM="aarch64-apple-darwin"
echo "[1/3] Building ${TARGET_ARM}..."
rustup target add "$TARGET_ARM" 2>/dev/null || true
cargo build --release --target "$TARGET_ARM"
cp "target/${TARGET_ARM}/release/atomcode" "${DIST}/atomcode-${VERSION}-darwin-arm64"
echo "  -> ${DIST}/atomcode-${VERSION}-darwin-arm64"

# --- macOS Intel ---
TARGET_X86="x86_64-apple-darwin"
echo "[2/3] Building ${TARGET_X86}..."
rustup target add "$TARGET_X86" 2>/dev/null || true
cargo build --release --target "$TARGET_X86"
cp "target/${TARGET_X86}/release/atomcode" "${DIST}/atomcode-${VERSION}-darwin-x64"
echo "  -> ${DIST}/atomcode-${VERSION}-darwin-x64"

# --- Windows (cross-compile) ---
TARGET_WIN="x86_64-pc-windows-gnu"
echo "[3/3] Building ${TARGET_WIN}..."
rustup target add "$TARGET_WIN" 2>/dev/null || true
if command -v x86_64-w64-mingw32-gcc &>/dev/null; then
    cargo build --release --target "$TARGET_WIN"
    cp "target/${TARGET_WIN}/release/atomcode.exe" "${DIST}/atomcode-${VERSION}-windows-x64.exe"
    echo "  -> ${DIST}/atomcode-${VERSION}-windows-x64.exe"
else
    echo "  !! Skipped: mingw-w64 not installed (brew install mingw-w64)"
fi

# --- Package ---
echo ""
echo "=== Packaging ==="
cd "$DIST"
rm -f *.tar.gz *.zip checksums.txt 2>/dev/null
for f in atomcode-*; do
    [ -f "$f" ] || continue
    [[ "$f" == *.tar.gz ]] && continue
    [[ "$f" == *.zip ]] && continue
    if [[ "$f" == *.exe ]]; then
        zip "${f%.exe}.zip" "$f"
        echo "  -> ${f%.exe}.zip"
    else
        chmod +x "$f"
        tar czf "${f}.tar.gz" "$f"
        echo "  -> ${f}.tar.gz"
    fi
done

# --- SHA256 ---
echo ""
echo "=== SHA256 ==="
shasum -a 256 *.tar.gz *.zip 2>/dev/null | tee checksums.txt
echo ""
echo "Done. Release artifacts in ${DIST}/"
ls -lh *.tar.gz *.zip 2>/dev/null
