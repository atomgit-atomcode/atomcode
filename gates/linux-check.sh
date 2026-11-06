#!/usr/bin/env bash
# Cross-check the Linux target from a Windows/macOS host.
#
# Why this exists: `cfg(unix)` code — whole test bodies included — is never
# compiled by a Windows-only gate. Two real defects slipped through that way
# (a `CancellationToken` import and a `BTreeMap` import used only under
# `#[cfg(unix)]`), and a reviewer found them before this script did.
#
# The blocker was `ring`: rustls' crypto backend has a build script that
# compiles C and asm for the TARGET, so a cross `cargo check` needs a C
# compiler for `x86_64-unknown-linux-gnu`. This host has none, and zig ships
# one. zig's `cc` rejects the `--target=x86_64-unknown-linux-gnu` spelling that
# cc-rs passes when cross-compiling (it wants `x86_64-linux-gnu`), so the
# compiler is reached through a small wrapper that rewrites that flag.
#
# Usage:
#   ZIG=/path/to/zig gates/linux-check.sh              # crates: default set
#   ZIG=/path/to/zig gates/linux-check.sh -p atomcode-tui
#
# Zig itself is NOT vendored: download it from https://ziglang.org/download/.
set -euo pipefail

crates=(-p atomcode-capabilities -p atomcode-schedule -p atomcode-daemon -p atomcode-coding)
if [ "$#" -gt 0 ]; then
  crates=("$@")
fi

zig="${ZIG:-}"
if [ -z "$zig" ]; then
  # The local convention this repository's Android work already uses.
  zig="$(ls -d target/zigdl/zig-*/zig.exe target/zigdl/zig-*/zig 2>/dev/null | head -1 || true)"
fi
if [ -z "$zig" ] || [ ! -x "$zig" ]; then
  echo "linux-check: no zig found. Download it from https://ziglang.org/download/" >&2
  echo "             and pass ZIG=/path/to/zig, or place it under target/zigdl/." >&2
  exit 2
fi

# Wrappers must live on a path Windows python can open: cc-rs spawns the
# compiler as an ordinary Windows process, so a POSIX `/tmp/...` script path is
# read as `<drive>:\tmp\...` and python exits 2 without a message. Keep them
# beside zig, inside the repository's ignored `target/`, and clean up after.
# cc-rs spawns the compiler as an ordinary Windows process, which cannot open a
# POSIX `/e/...` spelling, so the wrapper directory is converted to a Windows
# path. `cygpath` is what Git Bash/MSYS2 ship for exactly this.
to_win() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}
work="$(to_win "$(cd "$(dirname "$zig")" && pwd)")/.wrappers"
# Both the wrapper and the interpreter reach cc-rs as Windows processes, so
# every path they see has to be a Windows one — a POSIX spelling fails with a
# bare "failed to find tool" and no hint about why.
zig="$(to_win "$(cd "$(dirname "$zig")" && pwd)")/$(basename "$zig")"
python="$(to_win "$(command -v python || command -v python3)")"
mkdir -p "$work"
trap 'rm -rf "$work"' EXIT

write_wrapper() {
  local out="$1" mode="$2"
  cat >"$out" <<EOF
import subprocess, sys
ZIG = r"$zig"
args = sys.argv[1:]
if "$mode" == "cc":
    kept, i = [], 0
    while i < len(args):
        a = args[i]
        if a in ("-target", "--target"):
            i += 2
            continue
        if a.startswith("--target=") or a.startswith("-target="):
            i += 1
            continue
        kept.append(a)
        i += 1
    args = kept
    argv = [ZIG, "cc", "-target", "x86_64-linux-gnu", *args]
else:
    argv = [ZIG, "ar", *args]
sys.exit(subprocess.run(argv, check=False).returncode)
EOF
  echo "$out"
}

cc_wrapper="$(write_wrapper "$work/zigcc.py" cc)"
ar_wrapper="$(write_wrapper "$work/zigar.py" ar)"

echo "linux-check: zig = $zig"
CC_x86_64_unknown_linux_gnu="$python $cc_wrapper" \
  AR_x86_64_unknown_linux_gnu="$python $ar_wrapper" \
  cargo check "${crates[@]}" --all-targets --target x86_64-unknown-linux-gnu