#!/bin/bash
# AtomCode dev environment setup
# Usage: bash scripts/setup.sh [--release] [--cross]
#   --release  also do a release build to verify everything compiles
#   --cross    also add cross-compilation targets (darwin-arm64/x64, windows-gnu)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DO_RELEASE=false
DO_CROSS=false
for arg in "$@"; do
    case "$arg" in
        --release) DO_RELEASE=true ;;
        --cross)   DO_CROSS=true ;;
    esac
done

# ── Color helpers ────────────────────────────────────────────────────────────
C_RESET='\033[0m'
C_BOLD='\033[1m'
C_GREEN='\033[0;32m'
C_YELLOW='\033[0;33m'
C_CYAN='\033[0;36m'
C_RED='\033[0;31m'

info()    { echo -e "${C_CYAN}[setup]${C_RESET} $*"; }
success() { echo -e "${C_GREEN}[ok]${C_RESET}    $*"; }
warn()    { echo -e "${C_YELLOW}[warn]${C_RESET}  $*"; }
error()   { echo -e "${C_RED}[error]${C_RESET} $*" >&2; exit 1; }
step()    { echo -e "\n${C_BOLD}==> $*${C_RESET}"; }

# ── OS detection ─────────────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin) PLATFORM="macos" ;;
    Linux)  PLATFORM="linux" ;;
    *)      error "Unsupported OS: $OS. Only macOS and Linux are supported." ;;
esac

info "Platform: $PLATFORM / $ARCH"
info "Project:  $PROJECT_ROOT"

# ── 1. System dependencies ───────────────────────────────────────────────────
step "System dependencies"

install_macos_deps() {
    if ! command -v brew &>/dev/null; then
        warn "Homebrew not found. Installing..."
        /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    fi
    success "Homebrew $(brew --version | head -1)"

    BREW_PKGS=()
    command -v git     &>/dev/null || BREW_PKGS+=(git)
    command -v curl    &>/dev/null || BREW_PKGS+=(curl)
    # cmake is needed by some tree-sitter grammars
    command -v cmake   &>/dev/null || BREW_PKGS+=(cmake)
    # pkg-config helps cargo find system libs (openssl etc.)
    command -v pkg-config &>/dev/null || BREW_PKGS+=(pkg-config)

    if [ ${#BREW_PKGS[@]} -gt 0 ]; then
        info "Installing: ${BREW_PKGS[*]}"
        brew install "${BREW_PKGS[@]}"
    fi

    if "$DO_CROSS"; then
        command -v x86_64-w64-mingw32-gcc &>/dev/null || {
            info "Installing mingw-w64 for Windows cross-compilation..."
            brew install mingw-w64
        }
    fi
}

install_linux_deps() {
    if command -v apt-get &>/dev/null; then
        PKG_MANAGER="apt-get"
        PKGS="build-essential curl git cmake pkg-config libssl-dev"
        if "$DO_CROSS"; then
            PKGS="$PKGS gcc-mingw-w64-x86-64"
        fi
        info "Updating apt cache..."
        sudo apt-get update -qq
        # shellcheck disable=SC2086
        sudo apt-get install -y $PKGS
    elif command -v dnf &>/dev/null; then
        PKGS="gcc gcc-c++ curl git cmake pkgconf-pkg-config openssl-devel"
        if "$DO_CROSS"; then
            PKGS="$PKGS mingw64-gcc"
        fi
        # shellcheck disable=SC2086
        sudo dnf install -y $PKGS
    elif command -v pacman &>/dev/null; then
        PKGS="base-devel curl git cmake pkgconf openssl"
        if "$DO_CROSS"; then
            PKGS="$PKGS mingw-w64-gcc"
        fi
        # shellcheck disable=SC2086
        sudo pacman -Sy --noconfirm $PKGS
    else
        warn "Unknown package manager. Please manually install: gcc, curl, git, cmake, pkg-config, openssl-dev"
    fi
}

case "$PLATFORM" in
    macos) install_macos_deps ;;
    linux) install_linux_deps ;;
esac

success "System dependencies OK"

# ── 2. Rust toolchain ────────────────────────────────────────────────────────
step "Rust toolchain"

if ! command -v rustup &>/dev/null; then
    info "Installing rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    # Make rustup/cargo available in this shell
    source "$HOME/.cargo/env"
fi

# Ensure PATH includes cargo
export PATH="$HOME/.cargo/bin:$PATH"

command -v cargo &>/dev/null || error "cargo not found after rustup install. Please restart your shell and re-run this script."

RUST_VERSION="$(rustup show active-toolchain 2>/dev/null || rustc --version)"
success "Rust: $RUST_VERSION"

# Ensure stable is up-to-date
info "Updating stable toolchain..."
rustup update stable --no-self-update

# Required components
info "Ensuring rustfmt + clippy are installed..."
rustup component add rustfmt clippy

if "$DO_CROSS"; then
    step "Cross-compilation targets"
    TARGETS=(
        "aarch64-apple-darwin"
        "x86_64-apple-darwin"
        "x86_64-pc-windows-gnu"
    )
    for t in "${TARGETS[@]}"; do
        info "Adding target: $t"
        rustup target add "$t"
    done
    success "Cross targets added"
fi

# ── 3. Verify project builds ─────────────────────────────────────────────────
step "Building atomcode (debug)"
cd "$PROJECT_ROOT"
cargo build 2>&1 | tail -5
success "Debug build OK"

if "$DO_RELEASE"; then
    step "Building atomcode (release)"
    cargo build --release 2>&1 | tail -5
    success "Release build OK: target/release/atomcode"
fi

# ── 4. Optional: site (Node.js / Tailwind) ───────────────────────────────────
if [ -f "$PROJECT_ROOT/site/package.json" ] && command -v node &>/dev/null; then
    step "Site dependencies (npm)"
    cd "$PROJECT_ROOT/site"
    npm install --silent
    success "Site npm deps installed"
elif [ -f "$PROJECT_ROOT/site/package.json" ]; then
    warn "Node.js not found — skipping site setup. Install Node 18+ if you need to work on the site."
fi

# ── Done ─────────────────────────────────────────────────────────────────────
cd "$PROJECT_ROOT"
echo ""
echo -e "${C_GREEN}${C_BOLD}AtomCode dev environment ready.${C_RESET}"
echo ""
echo "  Quick start:"
echo "    cargo run                   # debug build + run"
echo "    cargo test                  # run tests"
echo "    cargo clippy                # lint"
echo "    bash scripts/release.sh     # build release artifacts"
echo ""
if ! echo "$PATH" | grep -q "$HOME/.cargo/bin"; then
    warn "Add Cargo to your PATH permanently:"
    echo '    echo '"'"'export PATH="$HOME/.cargo/bin:$PATH"'"'"' >> ~/.zshrc  # or ~/.bashrc'
fi
