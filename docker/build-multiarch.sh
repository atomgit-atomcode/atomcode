#!/bin/bash
set -e

# Build and push the AtomCode Daemon image for multiple architectures
# (linux/amd64 + linux/arm64), so NAS / home-server users can pull one
# multi-arch image on both x86 and ARM hardware.
#
# Usage:
#   docker/build-multiarch.sh                 # build + push (requires docker login)
#   docker/build-multiarch.sh <image:tag>     # custom image name
#   BUILD_ONLY=1 docker/build-multiarch.sh    # build + load the current host architecture
#
# Prerequisites:
#   - musl cross toolchain (for scripts/release.sh):
#       brew install FiloSottile/musl-cross/musl-cross
#   - docker buildx (bundled with Docker Desktop / OrbStack)

cd "$(dirname "$0")/.."

# --- Resolve image name ---
IMAGE="${1:-}"
if [ -z "$IMAGE" ]; then
    VERSION=$(awk -F'"' '
        /^\[workspace\.package\]/ { in_section = 1; next }
        /^\[/ { in_section = 0 }
        in_section && /^version *=/ { print $2; exit }
    ' Cargo.toml)
    IMAGE="atomcode-daemon:v${VERSION}"
fi
echo "==> Image: ${IMAGE}"

# --- 1. Build daemon binaries for both Linux arches ---
# release.sh cross-compiles atomcode-daemon for x64 and arm64 into dist/v*/
echo "==> Building Linux x64 + arm64 daemon binaries..."
ATOMCODE_INCLUDE_DAEMON=1 ./scripts/release.sh

# --- 2. Sanity-check both artifacts exist ---
X64_BIN=$(ls dist/v*/atomcode-daemon-*-linux-x64 2>/dev/null | head -1 || true)
ARM_BIN=$(ls dist/v*/atomcode-daemon-*-linux-arm64 2>/dev/null | head -1 || true)
if [ -z "$X64_BIN" ] || [ -z "$ARM_BIN" ]; then
    echo "ERROR: missing daemon artifacts (x64: ${X64_BIN:-none}, arm64: ${ARM_BIN:-none})." >&2
    echo "       Install musl cross toolchain and re-run: brew install FiloSottile/musl-cross/musl-cross" >&2
    exit 1
fi
echo "    x64:   ${X64_BIN}"
echo "    arm64: ${ARM_BIN}"

# --- 3. Build multi-arch image ---
PLATFORMS="linux/amd64,linux/arm64"
if [ "${BUILD_ONLY:-0}" = "1" ]; then
    case "$(uname -m)" in
        arm64|aarch64) LOCAL_PLATFORM="linux/arm64" ;;
        *) LOCAL_PLATFORM="linux/amd64" ;;
    esac
    echo "==> Building + loading locally for ${LOCAL_PLATFORM}..."
    docker buildx build \
        --platform "$LOCAL_PLATFORM" \
        --provenance=false \
        -t "$IMAGE" \
        --load \
        -f docker/Dockerfile-Daemon .
else
    echo "==> Building + pushing ${PLATFORMS}..."
    docker buildx build \
        --platform "$PLATFORMS" \
        --provenance=false \
        -t "$IMAGE" \
        --push \
        -f docker/Dockerfile-Daemon .
fi

echo "==> Done: ${IMAGE}"
