#!/bin/sh
# AtomCode installer — curl | sh
#
#   curl -fsSL https://raw.atomgit.com/atomgit_atomcode/atomcode/raw/main/scripts/install.sh | sh
#
# Env overrides:
#   ATOMCODE_VERSION   release tag to install (default: latest release, auto-detected
#                        from the AtomGit API)
#   ATOMCODE_PREFIX    install dir (absolute path; default: /usr/local/bin if writable,
#                        else ~/.local/bin). On HarmonyOS as non-root, default is ~/.local/bin.
# IMPORTANT: when changing install paths, the PATH-rc edit format, or filenames here,
# also update scripts/uninstall.sh AND
# crates/atomcode-cli/src/uninstall/paths.rs. The CI parity test guards
# the manifest, but binary path / rc-edit format are not checked.
set -eu

# Fallback version used only when ATOMCODE_VERSION is unset and the API lookup fails.
DEFAULT_VERSION="v5.0.3"
REPO_BASE="https://atomgit.com/atomgit_atomcode/atomcode/releases/download"
REPO_LATEST_API="https://api.atomgit.com/api/v5/repos/atomgit_atomcode/atomcode/releases/latest"

# --- detect platform ---
uname_s=$(uname -s)
uname_m=$(uname -m)
ext=""  # binary filename suffix; ".exe" on Windows shells (set below)

case "$uname_s" in
    Darwin) os="darwin" ;;
    Linux)  os="linux"  ;;
    HarmonyOS) os="ohos" ;;
    # MSYS2 / MinGW / Git-Bash / Cygwin: a Unix shell running ON Windows. `uname -s` looks
    # like MSYS_NT-10.0-26100 / MINGW64_NT-... / CYGWIN_NT-.... Install the native Windows
    # `.exe` into this shell's environment (windows-specific PREFIX + suffix handled below).
    MSYS*|MINGW*|CYGWIN*) os="windows"; ext=".exe" ;;
    *) echo "Unsupported OS: $uname_s (Windows users: download the zip from the release page)"; exit 1 ;;
esac

case "$uname_m" in
    arm64|aarch64) arch="arm64" ;;
    x86_64|amd64)  arch="x64"   ;;
    *) echo "Unsupported arch: $uname_m"; exit 1 ;;
esac

# --- pick install dir ---
if [ -n "${ATOMCODE_PREFIX:-}" ]; then
    PREFIX="$ATOMCODE_PREFIX"
elif [ "$os" = "ohos" ] || [ "$os" = "windows" ]; then
    # Windows shells (MSYS/Git-Bash/Cygwin) have no sudo and a system /usr/local/bin under
    # the MSYS root; install into the user's home instead (always writable, no elevation).
    PREFIX="$HOME/.local/bin"
elif [ -w /usr/local/bin ] 2>/dev/null; then
    PREFIX="/usr/local/bin"
elif [ "$(id -u)" -eq 0 ]; then
    PREFIX="/usr/local/bin"
else
    PREFIX="$HOME/.local/bin"
fi
mkdir -p "$PREFIX"

# --- referral invite code handling ---
# Priority: env var > --invite= arg
INVITE="${ATOMCODE_INVITE:-}"

# Parse --invite=ABC12345 or --invite ABC12345 from command-line arguments.
# Use while+shift instead of for+shift — for iterates a snapshot of $@.
while [ $# -gt 0 ]; do
  case "$1" in
    --invite=*) INVITE="${1#*=}"; shift ;;
    --invite)   shift; INVITE="$1"; shift ;;
    *)          shift ;;
  esac
done

if [ -n "$INVITE" ]; then
  ATOMCODE_DIR="${ATOMCODE_HOME:-$HOME/.atomcode}"
  mkdir -p "$ATOMCODE_DIR"

  # Generate install_uuid (prefer uuidgen, fallback to /proc or /dev/urandom)
  INSTALL_UUID=""
  if command -v uuidgen >/dev/null 2>&1; then
    INSTALL_UUID="$(uuidgen)"
  elif [ -f /proc/sys/kernel/random/uuid ]; then
    INSTALL_UUID="$(cat /proc/sys/kernel/random/uuid)"
  else
    # Fallback: read raw bytes and format as UUID without relying on od word order.
    INSTALL_HEX="$(od -An -N16 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')"
    INSTALL_UUID="$(printf '%s' "$INSTALL_HEX" | sed -E 's/^(.{8})(.{4})(.{4})(.{4})(.{12}).*/\1-\2-\3-\4-\5/')"
  fi

  # Validate invite code: 8 alphanumeric chars
  if echo "$INVITE" | grep -qE '^[A-Za-z0-9]{8}$'; then
    cat > "$ATOMCODE_DIR/pending_invite" <<EOF
invite_code=${INVITE}
install_uuid=${INSTALL_UUID}
attempted_at=$(date +%s)
EOF
  else
    echo "Warning: invalid invite code format, skipping referral" >&2
  fi
fi
# --- end referral handling ---

# --- download ---
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
DEST="$TMP/atomcode${ext}"

# Pick download tool: $_fetch streams a URL to stdout (for the API lookup),
# $_down saves a URL to a file (for the binary).
if command -v curl >/dev/null 2>&1; then
    _fetch="curl -sL --connect-timeout 5 --max-time 10"
    _down="curl -fL --progress-bar -o"
elif command -v wget >/dev/null 2>&1; then
    _fetch="wget -qO- --timeout=10 --tries=1"
    _down="wget --show-progress -O"
else
    echo "Error: need curl or wget." >&2
    exit 1
fi

# --- resolve version ---
# Honor ATOMCODE_VERSION if set; otherwise auto-detect the latest release tag
# from the API, falling back to DEFAULT_VERSION if the lookup yields nothing.
if [ -n "${ATOMCODE_VERSION:-}" ]; then
    VERSION="$ATOMCODE_VERSION"
else
    echo "==> Detecting latest version"
    VERSION=$($_fetch "$REPO_LATEST_API" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    [ -n "$VERSION" ] || VERSION="$DEFAULT_VERSION"
fi

BIN_NAME="atomcode-${VERSION}-${os}-${arch}${ext}"
URL="${REPO_BASE}/${VERSION}/${BIN_NAME}"

echo "==> Downloading $BIN_NAME"
echo "    from $URL"
$_down "$DEST" "$URL"

# Sanity check: must be a real binary, not an HTML 404 page
if head -c 4 "$DEST" | grep -q "<" 2>/dev/null; then
    echo "Error: download looks like an HTML page, not a binary."
    echo "       The release may not exist for your platform, or the URL is wrong."
    echo "       URL: $URL"
    exit 1
fi

chmod +x "$DEST"

# --- install ---
TARGET="$PREFIX/atomcode${ext}"
if [ "$os" = "windows" ]; then
    # No sudo on MSYS/Git-Bash, and PREFIX is the user's own dir. A running atomcode.exe locks
    # the file on NTFS, so `mv` can fail with a lock error — surface a clear hint instead of a
    # raw `set -e` abort (mirrors install.ps1's "close any running atomcode.exe" guidance).
    echo "==> Installing to $TARGET"
    if ! mv "$DEST" "$TARGET"; then
        echo "Error: could not write $TARGET." >&2
        echo "       If atomcode is already running, close it and re-run this installer." >&2
        exit 1
    fi
elif [ -e "$TARGET" ] && [ ! -w "$TARGET" ]; then
    echo "==> Installing to $TARGET (sudo required)"
    sudo mv "$DEST" "$TARGET"
elif [ ! -w "$PREFIX" ]; then
    echo "==> Installing to $TARGET (sudo required)"
    sudo mv "$DEST" "$TARGET"
else
    echo "==> Installing to $TARGET"
    mv "$DEST" "$TARGET"
fi

# --- done ---
echo ""
echo "Installed: $TARGET"
"$TARGET" --version 2>/dev/null || true

if [ "$os" = "windows" ]; then
    echo ""
    echo "Note: installed for this Unix shell (MSYS/MinGW/Git-Bash/Cygwin)."
    echo "      For a system-wide Windows install (cmd / PowerShell PATH), use instead:"
    echo "      powershell -c \"irm https://raw.atomgit.com/atomgit_atomcode/atomcode/raw/main/scripts/install.ps1 | iex\""
fi

case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *)
        # Auto-append PATH export to shell rc file
        LINE="export PATH=\"$PREFIX:\$PATH\""
        RC=""
        if [ -n "${ZSH_VERSION:-}" ] || [ "$(basename "${SHELL:-}")" = "zsh" ]; then
            RC="$HOME/.zshrc"
        elif [ -n "${BASH_VERSION:-}" ] || [ "$(basename "${SHELL:-}")" = "bash" ]; then
            RC="$HOME/.bashrc"
        fi

        if [ -n "$RC" ]; then
            # Match the COMPLETE export line we manage (grep -x = whole line, -F =
            # fixed string), not a bare substring of "$PREFIX". A `grep -qF "$PREFIX"`
            # false-positives when the rc merely MENTIONS the prefix as a substring
            # (e.g. an "AtomCodeBackup" path), silently skipping the real PATH add.
            if [ -f "$RC" ] && grep -qxF "$LINE" "$RC" 2>/dev/null; then
                # Already present, skip
                :
            else
                echo "" >> "$RC"
                echo "# Added by AtomCode installer" >> "$RC"
                echo "$LINE" >> "$RC"
                echo ""
                echo "Added $PREFIX to PATH in $RC"
            fi
            echo ""
            echo "To start using atomcode right now, run:"
            echo ""
            echo "    source $RC"
            echo ""
        else
            echo ""
            echo "Note: $PREFIX is not in your PATH. Add this line to your shell rc:"
            echo "    $LINE"
        fi
        ;;
esac
