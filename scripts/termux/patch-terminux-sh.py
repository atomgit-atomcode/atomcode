#!/usr/bin/env python3
"""Retarget a Termux-built binary at Android's own shell.

Termux builds every package for its own $PREFIX, and some of those paths are
compiled in, not looked up. `cmake` carries
`/data/data/com.termux/files/usr/bin/sh` as its default `CMAKE_SH`, which it
writes into every generated Makefile — on a non-Termux app that path does not
exist, so `make` dies with `.../usr/bin/sh: No such file or directory` (exit
127) even though the compiler works and `/bin/sh` is right there. `-DCMAKE_SH=`
does not override it (measured: the baked default wins).

The fix is an equal-length byte replacement: the string is rewritten to
`/bin/sh` and the rest of the original bytes become NUL, so every offset in the
ELF stays where it was and nothing has to be relocated.

Usage: patch-terminux-sh.py <file>...
"""

import sys

OLD = b"/data/data/com.termux/files/usr/bin/sh"
NEW = b"/bin/sh"


def patch(path: str) -> bool:
    with open(path, "rb") as handle:
        data = handle.read()
    if OLD not in data:
        return False
    # Same length ⇒ same offsets: the trailing bytes of the old string become NUL,
    # which terminates the C string right after `/bin/sh`.
    patched = data.replace(OLD, NEW + b"\x00" * (len(OLD) - len(NEW)))
    with open(path, "wb") as handle:
        handle.write(patched)
    return True


def main() -> int:
    changed = 0
    for path in sys.argv[1:]:
        try:
            if patch(path):
                print(f"  patched {path}")
                changed += 1
        except OSError as error:
            print(f"  skipped {path}: {error}", file=sys.stderr)
    if changed == 0:
        print("  no Termux shell path found (nothing to do)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
