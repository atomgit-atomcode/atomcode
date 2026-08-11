#!/bin/bash
set -e

# Always run from project root
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

# Prefer rustup toolchain over Homebrew rustc — Homebrew's rust ships only the
# host target, so cross-compiling to x86_64-apple-darwin fails with "can't find
# crate for `core`". rustup supports `rustup target add` for extra targets.
if [ -x "$HOME/.cargo/bin/rustc" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

# Source of truth: [workspace.package].version in Cargo.toml — the binary
# embeds this via env!("CARGO_PKG_VERSION"), so keying releases off it
# guarantees the package filename matches `atomcode --version`. Git tags
# can drift (Cargo bumped but tag not yet pushed, or vice versa).
# ATOMCODE_VERSION env override is preserved for CI / one-off rebuilds.
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
    echo "or override with ATOMCODE_VERSION=v1.0.0."
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
echo "Using VERSION=${VERSION}"

DIST="dist/${VERSION}"
mkdir -p "$DIST"

echo "=== AtomCode Release ${VERSION} ==="
echo ""

# Build the embedded webui frontend so the binary embeds the latest UI.
#
# Fatal, not a warning: webui/dist is gitignored, so there is no committed
# copy to fall back on. Skipping this step produces a binary whose /webui
# serves 404 — a defect nobody sees until the release is installed.
if [ ! -d webui ]; then
  echo "error: webui/ is missing; cannot build the embedded UI" >&2
  exit 1
fi
if ! command -v npm >/dev/null 2>&1; then
  echo "error: npm not found, and webui/dist is gitignored — there is nothing" >&2
  echo "       to fall back on. A release built now would serve 404 for /webui." >&2
  exit 1
fi
echo "Building webui frontend..."
(cd webui && npm ci && npm run build)
# `rust-embed` accepts a missing/empty folder silently (allow_missing), so a
# half-failed frontend build would otherwise ship as an empty UI.
if [ ! -f webui/dist/index.html ]; then
  echo "error: webui build produced no dist/index.html" >&2
  exit 1
fi
echo ""

# Default to CLI-only builds. Daemon is internal/CI-facing (see sign-macos.sh
# header) and shipping it in releases bloats artifacts + signing surface.
# Set ATOMCODE_INCLUDE_DAEMON=1 to also build and package atomcode-daemon.
INCLUDE_DAEMON="${ATOMCODE_INCLUDE_DAEMON:-0}"
CARGO_PKG_ARGS=(-p atomcode)
if [ "$INCLUDE_DAEMON" = "1" ]; then
    CARGO_PKG_ARGS+=(-p atomcode-daemon)
fi

# Copies the daemon binary if INCLUDE_DAEMON=1; no-op otherwise.
# $1 = source path (without .exe extension), $2 = dest path (without .exe extension), $3 = ".exe" or ""
copy_daemon() {
    [ "$INCLUDE_DAEMON" = "1" ] || return 0
    local src="$1$3" dst="$2$3"
    cp "$src" "$dst"
    echo "  -> $dst"
}

# --- macOS ARM (Apple Silicon) ---
TARGET_ARM="aarch64-apple-darwin"
echo "[1/6] Building ${TARGET_ARM}..."
rustup target add "$TARGET_ARM" 2>/dev/null || true
cargo build --release --target "$TARGET_ARM" "${CARGO_PKG_ARGS[@]}"
cp "target/${TARGET_ARM}/release/atomcode" "${DIST}/atomcode-${VERSION}-darwin-arm64"
echo "  -> ${DIST}/atomcode-${VERSION}-darwin-arm64"
copy_daemon "target/${TARGET_ARM}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-darwin-arm64" ""

# --- macOS Intel ---
TARGET_X86="x86_64-apple-darwin"
echo "[2/6] Building ${TARGET_X86}..."
rustup target add "$TARGET_X86" 2>/dev/null || true
cargo build --release --target "$TARGET_X86" "${CARGO_PKG_ARGS[@]}"
cp "target/${TARGET_X86}/release/atomcode" "${DIST}/atomcode-${VERSION}-darwin-x64"
echo "  -> ${DIST}/atomcode-${VERSION}-darwin-x64"
copy_daemon "target/${TARGET_X86}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-darwin-x64" ""

# --- Linux x64 (cross-compile with musl) ---
TARGET_LINUX="x86_64-unknown-linux-musl"
echo "[3/6] Building ${TARGET_LINUX}..."
rustup target add "$TARGET_LINUX" 2>/dev/null || true
if command -v x86_64-linux-musl-gcc &>/dev/null; then
    export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
    export CFLAGS_x86_64_unknown_linux_musl="-fPIC"
    cargo build --release --target "$TARGET_LINUX" "${CARGO_PKG_ARGS[@]}"
    cp "target/${TARGET_LINUX}/release/atomcode" "${DIST}/atomcode-${VERSION}-linux-x64"
    echo "  -> ${DIST}/atomcode-${VERSION}-linux-x64"
    copy_daemon "target/${TARGET_LINUX}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-linux-x64" ""
else
    echo "  !! Skipped: musl-cross not installed (brew install FiloSottile/musl-cross/musl-cross)"
fi

# --- Linux ARM64 (cross-compile with musl) ---
TARGET_LINUX_ARM="aarch64-unknown-linux-musl"
echo "[4/6] Building ${TARGET_LINUX_ARM}..."
rustup target add "$TARGET_LINUX_ARM" 2>/dev/null || true
if command -v aarch64-linux-musl-gcc &>/dev/null; then
    export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
    export CFLAGS_aarch64_unknown_linux_musl="-fPIC"
    cargo build --release --target "$TARGET_LINUX_ARM" "${CARGO_PKG_ARGS[@]}"
    cp "target/${TARGET_LINUX_ARM}/release/atomcode" "${DIST}/atomcode-${VERSION}-linux-arm64"
    echo "  -> ${DIST}/atomcode-${VERSION}-linux-arm64"
    copy_daemon "target/${TARGET_LINUX_ARM}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-linux-arm64" ""
else
    echo "  !! Skipped: aarch64 musl-cross not installed (brew reinstall FiloSottile/musl-cross/musl-cross — aarch64 ships by default; do NOT pass --with-aarch64, it gets fuzzy-matched to --without-aarch64)"
fi

# --- Windows x64 (cross-compile) ---
TARGET_WIN="x86_64-pc-windows-gnu"
echo "[5/6] Building ${TARGET_WIN}..."
rustup target add "$TARGET_WIN" 2>/dev/null || true
if command -v x86_64-w64-mingw32-gcc &>/dev/null; then
    cargo build --release --target "$TARGET_WIN" "${CARGO_PKG_ARGS[@]}"
    cp "target/${TARGET_WIN}/release/atomcode.exe" "${DIST}/atomcode-${VERSION}-windows-x64.exe"
    echo "  -> ${DIST}/atomcode-${VERSION}-windows-x64.exe"
    copy_daemon "target/${TARGET_WIN}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-windows-x64" ".exe"
else
    echo "  !! Skipped: mingw-w64 not installed (brew install mingw-w64)"
fi

# --- Windows ARM64 (cross-compile) ---
TARGET_WIN_ARM="aarch64-pc-windows-gnullvm"
echo "[6/6] Building ${TARGET_WIN_ARM}..."
rustup target add "$TARGET_WIN_ARM" 2>/dev/null || true
if command -v aarch64-w64-mingw32-gcc &>/dev/null; then
    cargo build --release --target "$TARGET_WIN_ARM" "${CARGO_PKG_ARGS[@]}"
    cp "target/${TARGET_WIN_ARM}/release/atomcode.exe" "${DIST}/atomcode-${VERSION}-windows-arm64.exe"
    echo "  -> ${DIST}/atomcode-${VERSION}-windows-arm64.exe"
    copy_daemon "target/${TARGET_WIN_ARM}/release/atomcode-daemon" "${DIST}/atomcode-daemon-${VERSION}-windows-arm64" ".exe"
else
    echo "  !! Skipped: llvm-mingw not installed (brew install llvm-mingw or see https://github.com/mstorsjo/llvm-mingw)"
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

# --- SHA256 ---
echo ""
echo "=== SHA256 ==="
cd "$DIST"
shasum -a 256 atomcode-* 2>/dev/null | tee checksums.txt

# --- latest.json (manifest for /upgrade self-update) ---
#
# Emits the manifest the TUI's `/upgrade` command reads from
# latest.json in the docs repo root. Shape:
#   { version, released_at, binaries: { "<target>": { sha256, size } } }
# Only entries for binaries that actually built are included — if a
# cross-compile was skipped (musl / mingw missing), that target simply
# doesn't appear and /upgrade will report "no binary for target …"
# rather than pointing at a 404.
echo ""
echo "=== Generating latest.json ==="
MANIFEST="$ROOT/latest.json"
RELEASED_AT=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

emit_entry() {
    # $1 = target tag (darwin-arm64 / linux-x64 / windows-x64.exe etc.)
    # $2 = filename in DIST
    local target="$1"
    local file="$2"
    if [ ! -f "$file" ]; then
        return 1
    fi
    local sha
    if command -v sha256sum >/dev/null 2>&1; then
        sha=$(sha256sum "$file" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        sha=$(shasum -a 256 "$file" | awk '{print $1}')
    else
        echo "ERROR: sha256sum or shasum not found" >&2
        return 1
    fi
    local size
    if stat -f%z "$file" >/dev/null 2>&1; then
        size=$(stat -f%z "$file")        # macOS / BSD
    else
        size=$(stat -c%s "$file")        # GNU coreutils
    fi
    printf '    "%s": { "sha256": "%s", "size": %s }' "$target" "$sha" "$size"
}

{
    printf '{\n'
    printf '  "version": "%s",\n' "$VERSION"
    printf '  "released_at": "%s",\n' "$RELEASED_AT"
    printf '  "binaries": {\n'
    first=1
    for pair in \
        "darwin-arm64:atomcode-${VERSION}-darwin-arm64" \
        "darwin-x64:atomcode-${VERSION}-darwin-x64" \
        "linux-x64:atomcode-${VERSION}-linux-x64" \
        "linux-arm64:atomcode-${VERSION}-linux-arm64" \
        "ohos-arm64:atomcode-${VERSION}-ohos-arm64" \
        "windows-x64:atomcode-${VERSION}-windows-x64.exe" \
        "windows-arm64:atomcode-${VERSION}-windows-arm64.exe"
    do
        target="${pair%%:*}"
        file="${pair#*:}"
        if [ -f "$file" ]; then
            if [ "$first" -eq 0 ]; then printf ',\n'; fi
            emit_entry "$target" "$file"
            first=0
        fi
    done
    printf '\n  }\n'
    printf '}\n'
} > "$MANIFEST"

echo "  -> ${MANIFEST}"
cat "$MANIFEST"

echo ""
echo "Done. Release artifacts in ${DIST}/"
echo "  * Copy ${MANIFEST} to the docs repo root (next to latest.txt) so /upgrade can find it."
ls -lh atomcode-* 2>/dev/null
