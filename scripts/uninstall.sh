#!/bin/sh
# AtomCode uninstaller — curl | sh
#
#   curl -fsSL https://atomgit.com/atomgit_atomcode/atomcode/raw/main/uninstall.sh | sh
#
# Flags:
#   --yes          skip prompts (use defaults: G1=yes, G2=no, G3=yes)
#   --purge        delete everything including ~/.atomcode/
#   --keep-data    only delete binary + PATH edit
#   --dry-run      print plan, do nothing
#   --print-manifest  emit the path manifest used for parity tests, exit
#
# This is the fallback uninstaller. Prefer `atomcode uninstall` if the
# binary is still working.
set -eu

# ---- manifest (must mirror crates/atomcode-core/src/uninstall/paths.rs) ----
ATOMCODE_GROUP2_FILES="auth.toml mcp.json config.toml ATOMCODE.md"
ATOMCODE_GROUP3_FILES="history input_history.txt recent_dirs.txt codingplan_sync.json device_id"
ATOMCODE_GROUP3_DIRS="staged telemetry plugins commands skills"
ATOMCODE_GROUP3_PREFIXES="notice."

# ---- emit-manifest mode (used by parity test) ----
if [ "${1:-}" = "--print-manifest" ]; then
    printf 'group2_files=%s\n' "$ATOMCODE_GROUP2_FILES"
    printf 'group3_files=%s\n' "$ATOMCODE_GROUP3_FILES"
    printf 'group3_dirs=%s\n' "$ATOMCODE_GROUP3_DIRS"
    printf 'group3_prefixes=%s\n' "$ATOMCODE_GROUP3_PREFIXES"
    exit 0
fi

# ---- parse flags ----
DO_YES=0; DO_PURGE=0; DO_KEEP=0; DO_DRY=0
for arg in "$@"; do
    case "$arg" in
        --yes) DO_YES=1 ;;
        --purge) DO_PURGE=1 ;;
        --keep-data) DO_KEEP=1 ;;
        --dry-run) DO_DRY=1 ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

if [ "$DO_PURGE" = 1 ] && [ "$DO_KEEP" = 1 ]; then
    echo "--purge and --keep-data conflict" >&2; exit 2
fi

# ---- detect binary ----
BIN=$(command -v atomcode 2>/dev/null || true)
if [ -z "$BIN" ]; then
    for c in /usr/local/bin/atomcode "$HOME/.local/bin/atomcode"; do
        [ -e "$c" ] && BIN=$c && break
    done
fi
if [ -z "$BIN" ] && [ -n "${ATOMCODE_BIN:-}" ]; then
    BIN=$ATOMCODE_BIN
fi
if [ -z "$BIN" ]; then
    echo "atomcode binary not found in PATH or default locations." >&2
    echo "If installed elsewhere, pass ATOMCODE_BIN=/path/to/atomcode" >&2
fi
BIN_DIR=$(dirname "$BIN" 2>/dev/null || echo "")

DATA="${ATOMCODE_HOME:-$HOME/.atomcode}"

# ---- plan ----
echo "Will remove (Group 1):"
[ -n "$BIN" ] && echo "  $BIN"
if [ -n "$BIN_DIR" ]; then
    for f in atomcode.bak .atomcode.rolling .atomcode.download .atomcode.writable-probe; do
        [ -e "$BIN_DIR/$f" ] && echo "  $BIN_DIR/$f"
    done
fi
for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
    if [ -e "$rc" ] && grep -q "Added by AtomCode installer" "$rc" 2>/dev/null; then
        echo "  $rc (PATH line)"
    fi
done

echo
if [ "$DO_KEEP" = 0 ]; then
    echo "Will consider (Group 2 — credentials):"
    for f in $ATOMCODE_GROUP2_FILES; do [ -e "$DATA/$f" ] && echo "  $DATA/$f"; done
    echo
    echo "Will remove (Group 3 — state):"
    for f in $ATOMCODE_GROUP3_FILES; do [ -e "$DATA/$f" ] && echo "  $DATA/$f"; done
    for d in $ATOMCODE_GROUP3_DIRS;  do [ -e "$DATA/$d" ] && echo "  $DATA/$d"; done
    for p in $ATOMCODE_GROUP3_PREFIXES; do
        for entry in "$DATA/$p"*; do [ -e "$entry" ] && echo "  $entry"; done
    done
fi

[ "$DO_DRY" = 1 ] && exit 0

# ---- prompt ----
DO_G2=0; DO_G3=1
if [ "$DO_PURGE" = 1 ]; then
    DO_G2=1; DO_G3=1
elif [ "$DO_KEEP" = 1 ]; then
    DO_G2=0; DO_G3=0
elif [ "$DO_YES" = 0 ]; then
    if [ ! -t 0 ]; then
        echo "refusing to run interactively without a TTY; pass --yes / --purge / --keep-data / --dry-run" >&2
        exit 2
    fi
    printf "[Group 1] Remove binary and PATH edit? [Y/n]: "; read ans
    case "${ans:-Y}" in [Nn]*) echo "aborted"; exit 1 ;; esac
    printf "[Group 2] Remove credentials and global config? [y/N]: "; read ans
    case "${ans:-N}" in [Yy]*) DO_G2=1 ;; esac
    printf "[Group 3] Remove local state and extensions? [Y/n]: "; read ans
    case "${ans:-Y}" in [Nn]*) DO_G3=0 ;; esac
    printf "Continue? [y/N]: "; read ans
    case "${ans:-N}" in [Yy]*) ;; *) echo "aborted"; exit 1 ;; esac
fi

# ---- execute (state → credentials → rc → binary) ----
if [ "$DO_G3" = 1 ]; then
    for f in $ATOMCODE_GROUP3_FILES; do rm -f "$DATA/$f"; done
    for d in $ATOMCODE_GROUP3_DIRS;  do rm -rf "$DATA/$d"; done
    for p in $ATOMCODE_GROUP3_PREFIXES; do rm -f "$DATA/$p"*; done
fi
if [ "$DO_G2" = 1 ]; then
    for f in $ATOMCODE_GROUP2_FILES; do rm -f "$DATA/$f"; done
fi
rmdir "$DATA" 2>/dev/null || true

# rc cleanup
for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
    [ -e "$rc" ] || continue
    if grep -q "Added by AtomCode installer" "$rc"; then
        cp "$rc" "$rc.atomcode-uninstall.bak"
        # Delete the comment line + the next non-blank line if it's an export PATH line.
        awk '
            /^# Added by AtomCode installer$/ { skip=1; next }
            skip == 1 && /^export PATH=/ { skip=0; next }
            skip == 1 && /^[[:space:]]*$/ { next }
            { skip=0; print }
        ' "$rc.atomcode-uninstall.bak" > "$rc"
        echo "edited $rc (backup: $rc.atomcode-uninstall.bak)"
    fi
done

# binary
if [ -n "$BIN" ] && [ -e "$BIN" ]; then
    BIN_DIR=$(dirname "$BIN")
    if [ -w "$BIN" ] && [ -w "$BIN_DIR" ]; then
        rm -f "$BIN" "$BIN_DIR/atomcode.bak" \
              "$BIN_DIR/.atomcode.rolling" \
              "$BIN_DIR/.atomcode.download" \
              "$BIN_DIR/.atomcode.writable-probe"
    else
        sudo rm -f "$BIN" "$BIN_DIR/atomcode.bak" \
              "$BIN_DIR/.atomcode.rolling" \
              "$BIN_DIR/.atomcode.download" \
              "$BIN_DIR/.atomcode.writable-probe"
    fi
fi

echo "uninstall complete."
