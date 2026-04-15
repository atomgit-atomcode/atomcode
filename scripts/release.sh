#!/bin/bash
set -e

# Always run from project root
cd "$(dirname "$0")/.."

# Prefer rustup toolchain over Homebrew rustc — Homebrew's rust ships only the
# host target, so cross-compiling to x86_64-apple-darwin fails with "can't find
# crate for `core`". rustup supports `rustup target add` for extra targets.
if [ -x "$HOME/.cargo/bin/rustc" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

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
echo "[1/4] Building ${TARGET_ARM}..."
rustup target add "$TARGET_ARM" 2>/dev/null || true
cargo build --release --target "$TARGET_ARM"
cp "target/${TARGET_ARM}/release/atomcode" "${DIST}/atomcode-${VERSION}-darwin-arm64"
cp "target/${TARGET_ARM}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-darwin-arm64"
echo "  -> ${DIST}/atomcode-${VERSION}-darwin-arm64"
echo "  -> ${DIST}/atomcode-daemon-${VERSION}-darwin-arm64"

# --- macOS Intel ---
TARGET_X86="x86_64-apple-darwin"
echo "[2/4] Building ${TARGET_X86}..."
rustup target add "$TARGET_X86" 2>/dev/null || true
cargo build --release --target "$TARGET_X86"
cp "target/${TARGET_X86}/release/atomcode" "${DIST}/atomcode-${VERSION}-darwin-x64"
cp "target/${TARGET_X86}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-darwin-x64"
echo "  -> ${DIST}/atomcode-${VERSION}-darwin-x64"
echo "  -> ${DIST}/atomcode-daemon-${VERSION}-darwin-x64"

# --- Linux x64 (cross-compile with musl) ---
TARGET_LINUX="x86_64-unknown-linux-musl"
echo "[3/4] Building ${TARGET_LINUX}..."
rustup target add "$TARGET_LINUX" 2>/dev/null || true
if command -v x86_64-linux-musl-gcc &>/dev/null; then
    export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
    export CFLAGS_x86_64_unknown_linux_musl="-fPIC"
    cargo build --release --target "$TARGET_LINUX"
    cp "target/${TARGET_LINUX}/release/atomcode" "${DIST}/atomcode-${VERSION}-linux-x64"
    cp "target/${TARGET_LINUX}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-linux-x64"
    echo "  -> ${DIST}/atomcode-${VERSION}-linux-x64"
    echo "  -> ${DIST}/atomcode-daemon-${VERSION}-linux-x64"
else
    echo "  !! Skipped: musl-cross not installed (brew install FiloSottile/musl-cross/musl-cross)"
fi

# --- Windows (cross-compile) ---
TARGET_WIN="x86_64-pc-windows-gnu"
echo "[4/4] Building ${TARGET_WIN}..."
rustup target add "$TARGET_WIN" 2>/dev/null || true
if command -v x86_64-w64-mingw32-gcc &>/dev/null; then
    cargo build --release --target "$TARGET_WIN"
    cp "target/${TARGET_WIN}/release/atomcode.exe" "${DIST}/atomcode-${VERSION}-windows-x64.exe"
    cp "target/${TARGET_WIN}/release/atomcode-daemon.exe" "${DIST}/atomcode-daemon-${VERSION}-windows-x64.exe"
    echo "  -> ${DIST}/atomcode-${VERSION}-windows-x64.exe"
    echo "  -> ${DIST}/atomcode-daemon-${VERSION}-windows-x64.exe"
else
    echo "  !! Skipped: mingw-w64 not installed (brew install mingw-w64)"
fi

# --- Sign macOS atomcode binaries (skip with ATOMCODE_SKIP_SIGN=1) ---
if [ "${ATOMCODE_SKIP_SIGN:-0}" != "1" ]; then
    echo ""
    echo "=== Signing macOS atomcode binaries ==="
    "$(dirname "$0")/sign-macos.sh" "$DIST"
else
    echo ""
    echo "=== Skipping macOS signing (ATOMCODE_SKIP_SIGN=1) ==="
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
